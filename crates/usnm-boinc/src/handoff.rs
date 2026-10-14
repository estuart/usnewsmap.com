//! The remote hand-off (#469): one blob container (`boinc-handoff` on the OCR exchange
//! account) between three places that share no filesystem.
//!
//! - The scan fetcher (CT 211) pushes scans it already fetched, with their sidecars, under
//!   `scans/`, and the prefetch manifest under `manifests/`
//!   (`scripts/ocr-node/push-boinc-scan.sh`).
//! - The hand-off job (in the main Container Apps environment, which reaches Cosmos and the
//!   lake) copies those down (`get`), runs `usnm-boinc export`, and keeps its dispatch root
//!   under `dispatch/` (`put`). Later it brings the spool and `trust.json` down and runs
//!   `collect --trust`.
//! - The BOINC bridge job copies one exported job into its own hand-off root (`pull`),
//!   submits it, and after assimilation puts the job's spool files and the gold
//!   `trust.json` under `spool/<job>/` and `gold/` (`put_spool`).
//!
//! Page bytes reach the BOINC server only from this container, never from LoC or the
//! scan cache. `pull` checks them against the request's size and sha256, and `submit`
//! checks again before it stages them as a BOINC download.
use crate::{dir, load, read, Lock, Stage};
use anyhow::{ensure, Context};
use std::path::{Path, PathBuf};
use usnm_ocr_contract::boinc;
use usnm_store::ObjectStore;

/// Where the dispatch root lives in the container.
pub const DISPATCH: &str = "dispatch";
/// The largest object `get` copies (a scan at its limit).
const MAX_OBJECT: usize = boinc::MAX_SCAN;
const MAX_MARKER: usize = 64 * 1024;
const MAX_TRUST: usize = 16 << 20;

fn join(prefix: &str, rest: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        rest.to_string()
    } else {
        format!("{prefix}/{rest}")
    }
}

/// Copies every object under `prefix` into `to`, keeping the path below `prefix`. Returns
/// how many it copied.
pub async fn get(store: &dyn ObjectStore, prefix: &str, to: &Path) -> anyhow::Result<usize> {
    let base = prefix.trim_matches('/');
    let prefix = format!("{base}/");
    let mut n = 0;
    for path in store.list(base).await? {
        // A listing matches names, so `scans` also lists `scans2/...`.
        let Some(rel) = path.strip_prefix(&prefix) else {
            continue;
        };
        usnm_store::validate_path(rel)?;
        let bytes = store
            .get(&path)
            .await?
            .with_context(|| format!("{path} went away"))?;
        ensure!(bytes.len() <= MAX_OBJECT, "{path} is over the size limit");
        crate::atomic(&to.join(rel), &bytes)?;
        n += 1;
    }
    Ok(n)
}

/// Every regular file under `dir`, as `/`-separated paths below it, without the lock and
/// unfinished temporary files.
fn files(dir: &Path) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        for e in std::fs::read_dir(dir.join(&rel))? {
            let e = e?;
            let name = e
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 file name under {}", dir.display()))?;
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push(rel.join(name));
            } else if t.is_file() && name != ".lock" && !name.starts_with(".tmp") {
                let p = rel.join(name);
                let parts: Vec<&str> = p
                    .components()
                    .map(|c| c.as_os_str().to_str().context("non-UTF-8 path"))
                    .collect::<anyhow::Result<_>>()?;
                out.push(parts.join("/"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Writes every file under `from` to `prefix/` in the store, replacing what is there (the
/// journal changes stage). Returns how many it wrote.
pub async fn put(store: &dyn ObjectStore, from: &Path, prefix: &str) -> anyhow::Result<usize> {
    let names = files(from)?;
    for rel in &names {
        usnm_store::validate_path(rel)?;
        let bytes = read(&from.join(rel), MAX_OBJECT)?;
        store
            .put(&join(prefix, rel), bytes, "application/octet-stream")
            .await?;
    }
    Ok(names.len())
}

/// The three files an exported job needs on the server, with their size limits.
const JOB_FILES: [(&str, usize); 3] = [
    ("page", boinc::MAX_SCAN),
    ("request.json", boinc::MAX_JSON),
    ("record.json", boinc::MAX_JSON * 2),
];

/// What `pull` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pulled {
    /// The job is in the server's hand-off root and reserved: submit it.
    Reserved,
    /// The server already had this job, past reservation: leave it.
    Already(Stage),
}

/// Server side: copies job `id` from the container's dispatch root into the server's
/// hand-off root `root`. A job the server already has must be the same job; one past
/// reservation is left as it is, so a repeated run submits nothing.
pub async fn pull(store: &dyn ObjectStore, root: &Path, id: &str) -> anyhow::Result<Pulled> {
    let target = dir(root, id)?;
    let _lock = Lock::take(root)?;
    let src = join(DISPATCH, &format!("jobs/{id}"));
    let mut got = Vec::new();
    for (name, max) in JOB_FILES {
        let bytes = store
            .get(&format!("{src}/{name}"))
            .await?
            .with_context(|| format!("{src}/{name} is not in the hand-off container"))?;
        ensure!(bytes.len() <= max, "{src}/{name} is over its size limit");
        got.push((name, bytes));
    }
    let incoming: crate::Record = serde_json::from_slice(&got[2].1)?;
    incoming.job.validate()?;
    ensure!(
        incoming.job.id == id && incoming.job.identity()? == id,
        "the container's record is not job {id}"
    );
    let request: boinc::Job = serde_json::from_slice(&got[1].1)?;
    ensure!(
        request == incoming.job,
        "the container's request and record differ"
    );
    let page = &got[0].1;
    ensure!(
        page.len() as u64 == request.scan.bytes && boinc::sha256(page) == request.scan.sha256,
        "the container's page is not the one exported (size or sha256 differ)"
    );
    if target.join("record.json").exists() {
        let old = load(root, id)?;
        ensure!(
            old.job == incoming.job && old.plan == incoming.plan && old.stamp == incoming.stamp,
            "the server's journal holds a different entry for this job"
        );
        if old.stage != Stage::Reserved {
            return Ok(Pulled::Already(old.stage));
        }
    }
    // Only a reservation that export completed can be submitted: a prepared entry's ledger
    // reservation may have been refused.
    ensure!(
        incoming.stage == Stage::Reserved,
        "the container's record is {:?}, not reserved",
        incoming.stage
    );
    let mut record = incoming;
    record.error = None;
    crate::atomic(&target.join("page"), page)?;
    crate::atomic(&target.join("request.json"), &got[1].1)?;
    crate::save(root, &record)?;
    Ok(Pulled::Reserved)
}

/// The spool names one job can have, with their size limits.
pub fn spool_names(workunit: &str) -> [(String, usize); 5] {
    [
        (format!("{workunit}__file_result.json"), boinc::MAX_JSON),
        (format!("{workunit}__file_text.txt"), boinc::MAX_TEXT),
        (format!("{workunit}__file_layout.xml"), boinc::MAX_ALTO),
        (format!("{workunit}_error"), MAX_MARKER),
        (format!("{workunit}_no_output_files"), MAX_MARKER),
    ]
}

/// Server side: writes job `id`'s spool files from `spool` to `spool/<id>/`, the canonical
/// result's host id (when known) to `spool/<id>/host_id`, and `trust` (when given) to
/// `gold/trust.json`. Returns how many spool files it wrote.
pub async fn put_spool(
    store: &dyn ObjectStore,
    spool: &Path,
    id: &str,
    host_id: Option<u64>,
    trust: Option<&Path>,
) -> anyhow::Result<usize> {
    ensure!(boinc::is_sha256(id), "invalid job id");
    let mut n = 0;
    for (name, max) in spool_names(&format!("usnm_{id}")) {
        let path = spool.join(&name);
        if path.exists() {
            let bytes = read(&path, max).with_context(|| path.display().to_string())?;
            store
                .put(
                    &format!("spool/{id}/{name}"),
                    bytes,
                    "application/octet-stream",
                )
                .await?;
            n += 1;
        }
    }
    ensure!(
        n > 0,
        "no spool files for usnm_{id} under {} (not assimilated yet?)",
        spool.display()
    );
    if let Some(h) = host_id {
        store
            .put(
                &format!("spool/{id}/host_id"),
                h.to_string().into_bytes(),
                "text/plain",
            )
            .await?;
    }
    if let Some(t) = trust {
        crate::trust::Trust::load(t)?;
        store
            .put("gold/trust.json", read(t, MAX_TRUST)?, "application/json")
            .await?;
    }
    Ok(n)
}

/// Hand-off job side: copies job `id`'s spool into `spool` (only the job's own spool names)
/// and returns the host id the server recorded, if any.
pub async fn get_spool(
    store: &dyn ObjectStore,
    id: &str,
    spool: &Path,
) -> anyhow::Result<Option<u64>> {
    ensure!(boinc::is_sha256(id), "invalid job id");
    let mut n = 0;
    for (name, max) in spool_names(&format!("usnm_{id}")) {
        if let Some(bytes) = store.get(&format!("spool/{id}/{name}")).await? {
            ensure!(bytes.len() <= max, "{name} is over its size limit");
            crate::atomic(&spool.join(&name), &bytes)?;
            n += 1;
        }
    }
    ensure!(n > 0, "no spool for job {id} in the hand-off container");
    match store.get(&format!("spool/{id}/host_id")).await? {
        Some(b) => Ok(Some(
            std::str::from_utf8(&b)?
                .trim()
                .parse()
                .context("bad host_id")?,
        )),
        None => Ok(None),
    }
}

/// Hand-off job side: copies `gold/trust.json` to `out`, after checking it is one. Returns
/// false when the container has none (a terminal failure needs none).
pub async fn get_trust(store: &dyn ObjectStore, out: &Path) -> anyhow::Result<bool> {
    let Some(bytes) = store.get("gold/trust.json").await? else {
        return Ok(false);
    };
    ensure!(bytes.len() <= MAX_TRUST, "trust.json is too large");
    let _: crate::trust::Trust = serde_json::from_slice(&bytes)?;
    crate::atomic(out, &bytes)?;
    Ok(true)
}
