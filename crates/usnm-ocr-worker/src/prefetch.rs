//! `usnm-ocr-worker prefetch` (#368): fetch the scans a manifest lists
//! (`usnm_ocr_contract::prefetch`) onto a node's disk ahead of the workers,
//! so every engine reads the same local scan and LoC serves each image
//! once.
//!
//! **Fetching** goes through [`LocClient`], the worker's own client: the
//! same User-Agent, 3.5 s between loc.gov API requests and 1 s between
//! image requests (shared by every page in flight in this process),
//! `Retry-After` honoured on 429 and 503 and a 403 held the same way, at
//! most an hour. Nothing works around a block: after
//! [`Options::max_failures_in_a_row`] pages fail in a row the run stops.
//! A lock file in the output directory keeps it to one fetching process.
//!
//! **Two sets** (the maintainer's split, #368): the *keep* set, every
//! non-English page and the worst English batches' pages, is kept for good;
//! the *buffer* set, the rest of English, is a rolling buffer that the
//! workers empty as they read it (ADR-0015 decision 9). Keep pages are
//! fetched first, in [`priority`] order, then buffer pages in issue order,
//! and the fetcher waits whenever the buffer's scans on disk reach
//! [`Options::buffer_cap`], looking again every minute.
//!
//! **Layout**: `<out>/<set>/<batch>/<lccn>/<date>/<edition>/<seq>.<ext>`
//! (`<set>` is `keep` or `buffer`, so nothing that empties the buffer can
//! reach a keep scan), where
//! `<ext>` is `jpg` for a full scan and `pct<n>.jpg` for one scaled to n%,
//! with a sidecar `<file>.json` ([`Sidecar`]) holding the LoC URL, the
//! fetch time, the size and the sha256. The scan is written to a `.part`
//! file, synced and renamed, then the sidecar the same way, so a scan
//! counts only once its sidecar is there. Each fetch is also appended to
//! `<out>/index-<tag>.jsonl`, and each failure to `<out>/failures-<tag>.jsonl`
//! (`<tag>` is `full` or `pct<n>`).
//!
//! **Resuming**: a page whose scan and sidecar are there, with the size and
//! sha256 the sidecar records, is skipped; anything else is fetched again.
//!
//! **Status**: `<out>/status-<tag>.json` ([`Status`]), rewritten every
//! [`Options::progress_every`] pages and every 30 s, with a log line
//! (JSON on stdout, so journald under systemd) at each one. Logs name
//! pages by doc_id.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Semaphore};
use tokio::time::Instant;
use usnm_ocr_contract::prefetch::{self, ManifestRow};
// The sets and the fetch order are the contract's, so the seeder (12 Step
// 11) can queue pages in the order they are fetched.
pub use usnm_ocr_contract::prefetch::{priority, set_of, Set, WORST_ENGLISH};

use crate::loc::{image_url, FetchError, ImageSize, LocClient, USER_AGENT};

/// How often a fetcher waiting at the buffer cap counts the buffer again.
pub const BUFFER_RECHECK: Duration = Duration::from_secs(60);

/// What to fetch.
#[derive(Debug, Clone)]
pub struct Options {
    /// The manifest (`manifest.parquet`).
    pub manifest: PathBuf,
    /// The root the scans go under.
    pub out: PathBuf,
    /// The keep set's resolution.
    pub size: ImageSize,
    /// The buffer set's resolution.
    pub buffer_size: ImageSize,
    /// Which sets to fetch.
    pub sets: Sets,
    /// Wait while the buffer set's scans on disk reach this many bytes.
    pub buffer_cap: u64,
    /// Only the first this many pages (after `sample`).
    pub limit: Option<usize>,
    /// A sample of about this many pages, stratified by language and decade.
    pub sample: Option<usize>,
    /// Pages in flight at once (they share the pacers).
    pub concurrency: usize,
    /// A status write and log line every this many pages.
    pub progress_every: u64,
    /// Stop after this many pages fail in a row.
    pub max_failures_in_a_row: u32,
    /// The order pages are fetched in.
    pub order: Order,
    /// Stop starting pages once the scans of this run's pages that are
    /// on disk (already there or fetched) add up to this many bytes.
    pub max_total_bytes: Option<u64>,
    /// The lock that keeps it to one fetching process; default
    /// `<out>/.prefetch.lock`. Runs into different directories that must
    /// not overlap (a sample beside the bulk fetch) share one.
    pub lock_file: Option<PathBuf>,
}

/// Which sets a run fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sets {
    Keep,
    Buffer,
    Both,
}

impl Sets {
    fn has(self, s: Set) -> bool {
        matches!(
            (self, s),
            (Self::Both, _) | (Self::Keep, Set::Keep) | (Self::Buffer, Set::Buffer)
        )
    }
}

/// The order pages are fetched in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// The manifest's (issue) order.
    Issue,
    /// By [`priority`] group, then issue order within each.
    Priority,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            manifest: PathBuf::new(),
            out: PathBuf::new(),
            size: ImageSize::Full,
            buffer_size: ImageSize::Full,
            sets: Sets::Both,
            buffer_cap: 1_000_000_000_000,
            limit: None,
            sample: None,
            concurrency: 4,
            progress_every: 100,
            max_failures_in_a_row: 20,
            order: Order::Priority,
            max_total_bytes: None,
            lock_file: None,
        }
    }
}

/// The tag of a size in file names: `full` or `pct50`.
pub fn tag(size: ImageSize) -> String {
    match size {
        ImageSize::Full => "full".into(),
        ImageSize::Pct(p) => format!("pct{p}"),
    }
}

/// Where a page's scan goes: under its set's directory.
pub fn scan_path(out: &Path, row: &ManifestRow, size: ImageSize) -> PathBuf {
    page_path(
        &out.join(set_of(row).dir()),
        &row.batch,
        &row.lccn,
        row.date,
        row.edition,
        row.seq,
        size,
    )
}

/// Where the scan of page `seq` of an issue goes, under a set's directory.
pub fn page_path(
    set_dir: &Path,
    batch: &str,
    lccn: &str,
    date: chrono::NaiveDate,
    edition: u16,
    seq: u16,
    size: ImageSize,
) -> PathBuf {
    let ext = match size {
        ImageSize::Full => "jpg".to_owned(),
        ImageSize::Pct(p) => format!("pct{p}.jpg"),
    };
    set_dir
        .join(batch)
        .join(lccn)
        .join(date.format("%Y-%m-%d").to_string())
        .join(edition.to_string())
        .join(format!("{seq}.{ext}"))
}

/// The bytes of scans (`*.jpg`) under `dir`, 0 if it isn't there. Any
/// other error is returned, so a cap is never checked against a count that
/// missed files.
pub fn dir_scan_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && d == dir => return Ok(0),
            // Deleted while counting.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for e in entries {
            let e = e?;
            let t = e.file_type()?;
            if t.is_dir() {
                stack.push(e.path());
            } else if t.is_file() && e.file_name().to_string_lossy().ends_with(".jpg") {
                match e.metadata() {
                    Ok(m) => total += m.len(),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }
    Ok(total)
}

/// The marker a consumed buffer scan leaves (`<scan>.done`, its sidecar
/// renamed by the scan server's delete), so a later run doesn't fetch it
/// again.
pub fn consumed_path(scan: &Path) -> PathBuf {
    let mut p = scan.as_os_str().to_owned();
    p.push(".done");
    PathBuf::from(p)
}

/// The sidecar beside a scan.
pub fn sidecar_path(scan: &Path) -> PathBuf {
    let mut p = scan.as_os_str().to_owned();
    p.push(".json");
    PathBuf::from(p)
}

/// What is recorded beside each scan, and in the index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    pub doc_id: String,
    pub batch: String,
    pub batch_version: u16,
    /// The image URL fetched, on tile.loc.gov.
    pub loc_url: String,
    pub iiif_base: String,
    /// `full` or `scaled:<n>`.
    pub resolution: String,
    pub fetched_at: DateTime<Utc>,
    pub bytes: u64,
    pub sha256: String,
    /// The image request's time, sent to last byte.
    pub download_ms: u64,
    pub user_agent: String,
}

/// `status-<tag>.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// `running`, `waiting` (the buffer is at its cap), `done`, `budget`
    /// (`--max-total-bytes` reached), `stopped` (asked to) or `failed`.
    pub state: String,
    pub version: String,
    pub pid: u32,
    pub started_at: String,
    pub updated_at: String,
    pub manifest: String,
    pub manifest_sha256: String,
    pub resolution: String,
    pub out: String,
    /// Pages this run works through (after `--sample` and `--limit`).
    pub pages: u64,
    /// Pages finished: already there, fetched or failed.
    pub finished: u64,
    pub already_there: u64,
    pub fetched: u64,
    pub failed_permanent: u64,
    pub failed_transient: u64,
    pub bytes_fetched: u64,
    /// This run's pages on disk: already there plus fetched.
    pub bytes_on_disk: u64,
    pub max_total_bytes: Option<u64>,
    /// The buffer set's scans on disk, as last counted, and its cap.
    pub buffer_bytes: u64,
    pub buffer_cap: u64,
    pub buffer_resolution: String,
    /// Pages of each set in this run.
    pub keep_pages: u64,
    pub buffer_pages: u64,
    pub elapsed_secs: u64,
    pub fetched_per_hour: f64,
    pub eta_secs: Option<u64>,
    /// Seconds loc.gov still holds our requests for, if it asked us to wait.
    pub loc_hold_secs: Option<u64>,
    pub last_doc_id: String,
    pub error: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A sha256 as lowercase hex.
pub fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A stable 64-bit order for a doc_id (the first 8 bytes of its sha256).
fn rank(doc_id: &str) -> u64 {
    let d = Sha256::digest(doc_id.as_bytes());
    u64::from_be_bytes(d[..8].try_into().expect("8 bytes"))
}

/// About `n` rows stratified by (language, decade): one from each stratum
/// (the largest strata first, when `n` is smaller than their number), the
/// rest shared in proportion to the strata's sizes (largest remainders),
/// never more than a stratum holds. Within a stratum the rows with the
/// lowest [`rank`] are taken, so the same manifest gives the same sample.
/// Returned in issue order.
pub fn sample(rows: &[ManifestRow], n: usize) -> Vec<ManifestRow> {
    let mut strata: BTreeMap<(String, i32), Vec<&ManifestRow>> = BTreeMap::new();
    for r in rows {
        strata
            .entry((r.lang_or_und().to_owned(), r.decade()))
            .or_default()
            .push(r);
    }
    let mut keys: Vec<(String, i32)> = strata.keys().cloned().collect();
    // Largest first; ties in key order.
    keys.sort_by(|a, b| strata[b].len().cmp(&strata[a].len()).then(a.cmp(b)));
    let mut take: BTreeMap<(String, i32), usize> = BTreeMap::new();
    if n <= keys.len() {
        for k in keys.iter().take(n) {
            take.insert(k.clone(), 1);
        }
    } else {
        for k in &keys {
            take.insert(k.clone(), 1);
        }
        let total = rows.len() as f64;
        let rest = (n - keys.len()) as f64;
        let mut shares: Vec<(f64, &(String, i32))> = Vec::new();
        let mut given = 0usize;
        for k in &keys {
            let want = rest * strata[k].len() as f64 / total;
            let whole = (want.floor() as usize).min(strata[k].len() - 1);
            *take.get_mut(k).expect("set above") += whole;
            given += whole;
            shares.push((want - want.floor(), k));
        }
        shares.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        let mut left = (n - keys.len()).saturating_sub(given);
        // Round after round in remainder order, until the sample is full or
        // every stratum is.
        while left > 0 {
            let mut gave = false;
            for (_, k) in &shares {
                if left == 0 {
                    break;
                }
                let t = take.get_mut(*k).expect("set above");
                if *t < strata[*k].len() {
                    *t += 1;
                    left -= 1;
                    gave = true;
                }
            }
            if !gave {
                break;
            }
        }
    }
    let mut out: Vec<ManifestRow> = Vec::new();
    for (k, rows) in &mut strata {
        rows.sort_by_key(|r| (rank(&r.doc_id), r.doc_id.clone()));
        let t = take.get(k).copied().unwrap_or(0);
        out.extend(rows.iter().take(t).map(|r| (*r).clone()));
    }
    prefetch::sort(&mut out);
    out
}

/// The size of the scan at `scan`, if it and its sidecar are there and
/// agree (size and sha256).
async fn verified(scan: &Path, row: &ManifestRow, size: ImageSize) -> Option<u64> {
    let side = tokio::fs::read(sidecar_path(scan)).await.ok()?;
    let side = serde_json::from_slice::<Sidecar>(&side).ok()?;
    if side.doc_id != row.doc_id || side.resolution != size.to_string() {
        return None;
    }
    let b = tokio::fs::read(scan).await.ok()?;
    (b.len() as u64 == side.bytes && sha256(&b) == side.sha256).then_some(side.bytes)
}

/// Write `body` to `path` through a synced `.part` file and a rename.
async fn write_atomic(path: &Path, body: Vec<u8>) -> anyhow::Result<()> {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let dir = path.parent().context("no parent directory")?;
        std::fs::create_dir_all(dir)?;
        let mut part = path.as_os_str().to_owned();
        part.push(".part");
        let part = PathBuf::from(part);
        let mut f = std::fs::File::create(&part)?;
        f.write_all(&body)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&part, &path)?;
        Ok(())
    })
    .await?
}

/// An append-only JSON Lines file, shared by the tasks.
struct Lines(Mutex<std::fs::File>);

impl Lines {
    fn open(path: &Path) -> anyhow::Result<Self> {
        Ok(Self(Mutex::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?,
        )))
    }

    fn push(&self, v: &impl Serialize) -> anyhow::Result<()> {
        let mut line = serde_json::to_vec(v)?;
        line.push(b'\n');
        let mut f = self.0.lock().expect("lines lock");
        f.write_all(&line)?;
        Ok(())
    }
}

/// A failure line.
#[derive(Debug, Serialize, Deserialize)]
pub struct Failure {
    pub doc_id: String,
    /// `permanent` (LoC said no, or lists no image) or `transient`.
    pub kind: String,
    pub error: String,
    pub at: String,
}

#[derive(Default)]
struct Counts {
    finished: AtomicU64,
    already_there: AtomicU64,
    fetched: AtomicU64,
    failed_permanent: AtomicU64,
    failed_transient: AtomicU64,
    bytes_fetched: AtomicU64,
    /// The keep set's scans of this run on disk: found there or fetched.
    keep_bytes: AtomicU64,
    /// The buffer set's scans on disk, all of them: the last count plus
    /// fetches since. Workers delete buffer scans, so it is counted again
    /// while waiting. It is what the cap is checked against.
    buffer_bytes: AtomicU64,
    /// This run's buffer pages found on disk or fetched (for the status and
    /// `--max-total-bytes`; consumed scans aren't taken off).
    run_buffer_bytes: AtomicU64,
    in_a_row: AtomicU32,
}

struct Run {
    opts: Options,
    loc: Arc<LocClient>,
    counts: Counts,
    status: Mutex<Status>,
    status_path: PathBuf,
    status_write: Mutex<()>,
    /// The current state (`running` or `waiting`), which every status write
    /// reports until the run ends.
    state: Mutex<String>,
    /// Index and failure lines by resolution tag, so each file holds one
    /// resolution.
    index: BTreeMap<String, Lines>,
    failures: BTreeMap<String, Lines>,
    stop_rx: watch::Receiver<bool>,
    started: Instant,
    /// Set when the run must stop taking pages: (state, error).
    stop: Mutex<Option<(String, Option<String>)>>,
}

impl Run {
    fn snapshot(&self, state: &str) -> Status {
        let c = &self.counts;
        let mut s = self.status.lock().expect("status lock").clone();
        s.state = state.to_owned();
        s.updated_at = now_rfc3339();
        s.finished = c.finished.load(Ordering::SeqCst);
        s.already_there = c.already_there.load(Ordering::SeqCst);
        s.fetched = c.fetched.load(Ordering::SeqCst);
        s.failed_permanent = c.failed_permanent.load(Ordering::SeqCst);
        s.failed_transient = c.failed_transient.load(Ordering::SeqCst);
        s.bytes_fetched = c.bytes_fetched.load(Ordering::SeqCst);
        s.buffer_bytes = c.buffer_bytes.load(Ordering::SeqCst);
        s.bytes_on_disk = self.on_disk();
        let secs = self.started.elapsed().as_secs_f64();
        s.elapsed_secs = secs as u64;
        let worked = s.finished.saturating_sub(s.already_there);
        s.fetched_per_hour = if secs >= 1.0 {
            (s.fetched as f64 * 3600.0 / secs * 10.0).round() / 10.0
        } else {
            0.0
        };
        s.eta_secs = (worked > 0 && secs >= 1.0)
            .then(|| (s.pages.saturating_sub(s.finished) as f64 * secs / worked as f64) as u64);
        s.loc_hold_secs = self
            .loc
            .held_until()
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs());
        s
    }

    /// This run's scans: its keep and buffer pages found on disk or fetched.
    fn on_disk(&self) -> u64 {
        let c = &self.counts;
        c.keep_bytes.load(Ordering::SeqCst) + c.run_buffer_bytes.load(Ordering::SeqCst)
    }

    fn set_state(&self, state: &str) {
        *self.state.lock().expect("state lock") = state.to_owned();
    }

    /// Write the status in the current state.
    fn write_status(&self) {
        let state = self.state.lock().expect("state lock").clone();
        self.write_status_as(&state);
    }

    fn write_status_as(&self, state: &str) {
        // One write at a time: the ticker and the page tasks share the
        // `.tmp` file.
        let _one = self.status_write.lock().expect("status write lock");
        let s = self.snapshot(state);
        let write = || -> anyhow::Result<()> {
            let mut tmp = self.status_path.as_os_str().to_owned();
            tmp.push(".tmp");
            let tmp = PathBuf::from(tmp);
            std::fs::write(&tmp, serde_json::to_vec_pretty(&s)?)?;
            std::fs::rename(&tmp, &self.status_path)?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!(error = %e, "can't write the status file");
        }
        tracing::info!(
            state,
            finished = s.finished,
            pages = s.pages,
            already_there = s.already_there,
            fetched = s.fetched,
            failed_permanent = s.failed_permanent,
            failed_transient = s.failed_transient,
            bytes_fetched = s.bytes_fetched,
            bytes_on_disk = s.bytes_on_disk,
            buffer_bytes = s.buffer_bytes,
            fetched_per_hour = s.fetched_per_hour,
            eta_secs = s.eta_secs,
            loc_hold_secs = s.loc_hold_secs,
            "prefetch progress"
        );
    }

    fn stop_with(&self, state: &str, error: Option<String>) {
        let mut stop = self.stop.lock().expect("stop lock");
        if stop.is_none() {
            *stop = Some((state.to_owned(), error));
        }
    }

    fn stopping(&self) -> bool {
        self.stop.lock().expect("stop lock").is_some()
    }

    /// Count the buffer again. If it can't be counted, it is taken as full,
    /// so the fetcher waits rather than overfill the pool.
    async fn count_buffer(&self) {
        let dir = self.opts.out.join(Set::Buffer.dir());
        let n = match tokio::task::spawn_blocking(move || dir_scan_bytes(&dir)).await {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "can't count the buffer; treating it as full");
                u64::MAX
            }
            Err(e) => {
                tracing::warn!(error = %e, "can't count the buffer; treating it as full");
                u64::MAX
            }
        };
        self.counts.buffer_bytes.store(n, Ordering::SeqCst);
    }

    /// Wait while the buffer is at its cap, counting it again every
    /// minute. False if the run was stopped meanwhile.
    async fn wait_for_buffer(&self) -> bool {
        let mut stop = self.stop_rx.clone();
        while self.counts.buffer_bytes.load(Ordering::SeqCst) >= self.opts.buffer_cap {
            self.set_state("waiting");
            self.write_status();
            tokio::select! {
                _ = tokio::time::sleep(BUFFER_RECHECK) => {}
                _ = stop.wait_for(|s| *s) => {
                    self.stop_with("stopped", None);
                }
            }
            if self.stopping() {
                return false;
            }
            self.count_buffer().await;
        }
        self.set_state("running");
        true
    }

    /// One page. False if it was left for a later run (stopped while
    /// waiting for the buffer).
    async fn page(&self, row: ManifestRow) -> anyhow::Result<bool> {
        let set = set_of(&row);
        let size = match set {
            Set::Keep => self.opts.size,
            Set::Buffer => self.opts.buffer_size,
        };
        let path = scan_path(&self.opts.out, &row, size);
        let c = &self.counts;
        if let Some(n) = verified(&path, &row, size).await {
            c.already_there.fetch_add(1, Ordering::SeqCst);
            match set {
                Set::Keep => c.keep_bytes.fetch_add(n, Ordering::SeqCst),
                Set::Buffer => c.run_buffer_bytes.fetch_add(n, Ordering::SeqCst),
            };
            // A page that is done breaks a run of failures too.
            c.in_a_row.store(0, Ordering::SeqCst);
            return Ok(true);
        }
        // A buffer page a worker has read and the scan server deleted.
        if set == Set::Buffer
            && tokio::fs::try_exists(consumed_path(&path))
                .await
                .unwrap_or(false)
        {
            c.already_there.fetch_add(1, Ordering::SeqCst);
            c.in_a_row.store(0, Ordering::SeqCst);
            return Ok(true);
        }
        // Only a page that needs fetching waits for room in the buffer.
        if set == Set::Buffer && !self.wait_for_buffer().await {
            return Ok(false);
        }
        let t = tag(size);
        let key = row.key()?;
        match self.loc.scan_at(&key, size).await {
            Ok(scan) => {
                let side = Sidecar {
                    doc_id: row.doc_id.clone(),
                    batch: row.batch.clone(),
                    batch_version: row.batch_version,
                    loc_url: image_url(&scan.iiif_base, size),
                    iiif_base: scan.iiif_base.clone(),
                    resolution: size.to_string(),
                    fetched_at: Utc::now(),
                    bytes: scan.jpeg.len() as u64,
                    sha256: sha256(&scan.jpeg),
                    download_ms: u64::try_from(scan.download.as_millis()).unwrap_or(u64::MAX),
                    user_agent: USER_AGENT.to_owned(),
                };
                write_atomic(&path, scan.jpeg.to_vec()).await?;
                write_atomic(&sidecar_path(&path), serde_json::to_vec_pretty(&side)?).await?;
                self.index[&t].push(&side)?;
                c.fetched.fetch_add(1, Ordering::SeqCst);
                c.bytes_fetched.fetch_add(side.bytes, Ordering::SeqCst);
                match set {
                    Set::Keep => c.keep_bytes.fetch_add(side.bytes, Ordering::SeqCst),
                    Set::Buffer => {
                        c.run_buffer_bytes.fetch_add(side.bytes, Ordering::SeqCst);
                        c.buffer_bytes.fetch_add(side.bytes, Ordering::SeqCst)
                    }
                };
                c.in_a_row.store(0, Ordering::SeqCst);
            }
            Err(e) => {
                let (kind, msg) = match &e {
                    FetchError::Permanent(m) => ("permanent", m.clone()),
                    FetchError::Transient(m) => ("transient", m.clone()),
                };
                tracing::warn!(doc_id = row.doc_id, kind, error = msg, "prefetch failed");
                self.failures[&t].push(&Failure {
                    doc_id: row.doc_id.clone(),
                    kind: kind.into(),
                    error: msg.chars().take(300).collect(),
                    at: now_rfc3339(),
                })?;
                if kind == "permanent" {
                    c.failed_permanent.fetch_add(1, Ordering::SeqCst);
                } else {
                    c.failed_transient.fetch_add(1, Ordering::SeqCst);
                }
                let n = c.in_a_row.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= self.opts.max_failures_in_a_row {
                    self.stop_with(
                        "failed",
                        Some(format!(
                            "{n} pages failed in a row (the last: {msg}); stopping rather than pressing on"
                        )),
                    );
                }
            }
        }
        Ok(true)
    }
}

/// Hold the lock file for the life of the process, so only one prefetch
/// fetches at a time.
fn lock(path: &Path) -> anyhow::Result<std::fs::File> {
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(|_| {
        anyhow::anyhow!(
            "another prefetch holds {}: one fetching process at a time",
            path.display()
        )
    })?;
    Ok(f)
}

/// The rows a run works through: the manifest, sampled, ordered, then
/// limited. Keep pages come first. With [`Order::Priority`] they go by
/// issue, each issue in its best keep page's group, so an issue's pages stay
/// together and its item JSON is asked for once (even when they came from
/// different batches); buffer pages stay in issue order either way.
pub fn select(rows: Vec<ManifestRow>, opts: &Options) -> Vec<ManifestRow> {
    let rows: Vec<ManifestRow> = rows
        .into_iter()
        .filter(|r| opts.sets.has(set_of(r)))
        .collect();
    let mut rows = match opts.sample {
        Some(n) => sample(&rows, n),
        None => rows,
    };
    type Issue = (String, chrono::NaiveDate, u16);
    let mut best: std::collections::HashMap<Issue, u8> = std::collections::HashMap::new();
    if opts.order == Order::Priority {
        for r in rows.iter().filter(|r| set_of(r) == Set::Keep) {
            let e = best
                .entry((r.lccn.clone(), r.date, r.edition))
                .or_insert(u8::MAX);
            *e = (*e).min(priority(r));
        }
    }
    rows.sort_by_cached_key(|r| {
        let set = set_of(r);
        let issue = (r.lccn.clone(), r.date, r.edition);
        match (set, opts.order) {
            (Set::Keep, Order::Priority) => (set, best[&issue], Some((issue, r.seq))),
            // Otherwise the manifest's order, which the stable sort keeps.
            _ => (set, 0, None),
        }
    });
    if let Some(l) = opts.limit {
        rows.truncate(l);
    }
    rows
}

/// Fetch the manifest's scans until done, stopped by `stop`, or too many
/// pages fail in a row. Returns the final status.
pub async fn run(
    opts: Options,
    loc: Arc<LocClient>,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<Status> {
    let mut stop_main = stop.clone();
    std::fs::create_dir_all(&opts.out)
        .with_context(|| format!("creating {}", opts.out.display()))?;
    let lock_path = opts
        .lock_file
        .clone()
        .unwrap_or_else(|| opts.out.join(".prefetch.lock"));
    let _lock = lock(&lock_path)?;
    let bytes = tokio::fs::read(&opts.manifest)
        .await
        .with_context(|| format!("reading {}", opts.manifest.display()))?;
    let manifest_sha256 = sha256(&bytes);
    // Only the rows of the sets asked for are held (about 250 bytes a row).
    let mut manifest_pages = 0usize;
    let sets = opts.sets;
    let rows = prefetch::decode_where(Bytes::from(bytes), |r| {
        manifest_pages += 1;
        sets.has(set_of(r))
    })
    .context("reading the manifest")?;
    let rows = select(rows, &opts);
    // The status is named for the resolution of the set the run starts with.
    let status_tag = tag(if opts.sets == Sets::Buffer {
        opts.buffer_size
    } else {
        opts.size
    });
    let mut index = BTreeMap::new();
    let mut failures = BTreeMap::new();
    for size in [opts.size, opts.buffer_size] {
        let t = tag(size);
        if !index.contains_key(&t) {
            index.insert(
                t.clone(),
                Lines::open(&opts.out.join(format!("index-{t}.jsonl")))?,
            );
            failures.insert(
                t.clone(),
                Lines::open(&opts.out.join(format!("failures-{t}.jsonl")))?,
            );
        }
    }
    let run = Arc::new(Run {
        loc,
        counts: Counts::default(),
        status: Mutex::new(Status {
            version: env!("CARGO_PKG_VERSION").into(),
            pid: std::process::id(),
            started_at: now_rfc3339(),
            manifest: opts.manifest.display().to_string(),
            manifest_sha256,
            resolution: opts.size.to_string(),
            out: opts.out.display().to_string(),
            pages: rows.len() as u64,
            max_total_bytes: opts.max_total_bytes,
            buffer_cap: opts.buffer_cap,
            buffer_resolution: opts.buffer_size.to_string(),
            keep_pages: rows.iter().filter(|r| set_of(r) == Set::Keep).count() as u64,
            buffer_pages: rows.iter().filter(|r| set_of(r) == Set::Buffer).count() as u64,
            ..Status::default()
        }),
        status_path: opts.out.join(format!("status-{status_tag}.json")),
        status_write: Mutex::new(()),
        state: Mutex::new("running".into()),
        index,
        failures,
        stop_rx: stop,
        started: Instant::now(),
        stop: Mutex::new(None),
        opts,
    });
    tracing::info!(
        manifest_pages,
        pages = rows.len(),
        resolution = %run.opts.size,
        buffer_resolution = %run.opts.buffer_size,
        buffer_cap = run.opts.buffer_cap,
        concurrency = run.opts.concurrency,
        out = %run.opts.out.display(),
        "prefetch starting"
    );
    run.count_buffer().await;
    run.write_status();
    // A status write every 30 s, so a long hold still shows.
    let ticker = {
        let run = Arc::clone(&run);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(30));
            every.tick().await;
            loop {
                every.tick().await;
                run.write_status();
            }
        })
    };
    let slots = Arc::new(Semaphore::new(run.opts.concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    let every = run.opts.progress_every.max(1);
    for row in rows {
        if run.stopping() {
            break;
        }
        let permit = tokio::select! {
            p = Arc::clone(&slots).acquire_owned() => p.expect("semaphore open"),
            _ = stop_main.wait_for(|s| *s) => {
                run.stop_with("stopped", None);
                break;
            }
        };
        if run.stopping() {
            break;
        }
        if let Some(max) = run.opts.max_total_bytes {
            if run.on_disk() >= max {
                run.stop_with("budget", None);
                break;
            }
        }
        let run = Arc::clone(&run);
        tasks.spawn(async move {
            let doc_id = row.doc_id.clone();
            match run.page(row).await {
                Ok(true) => {}
                Ok(false) => return,
                Err(e) => run.stop_with("failed", Some(format!("{doc_id}: {e:#}"))),
            }
            let n = run.counts.finished.fetch_add(1, Ordering::SeqCst) + 1;
            run.status.lock().expect("status lock").last_doc_id = doc_id;
            if n.is_multiple_of(every) {
                run.write_status();
            }
            drop(permit);
        });
        // Collect what's finished as we go, so the set stays small.
        while let Some(r) = tasks.try_join_next() {
            r?;
        }
    }
    while let Some(r) = tasks.join_next().await {
        r?;
    }
    // Stopped before the last write, so it can't put `running` back.
    ticker.abort();
    let _ = ticker.await;
    let (state, error) = run
        .stop
        .lock()
        .expect("stop lock")
        .clone()
        .unwrap_or_else(|| ("done".into(), None));
    run.status.lock().expect("status lock").error = error.clone();
    run.set_state(&state);
    run.write_status();
    let status = run.snapshot(&state);
    if state == "failed" {
        bail!(error.unwrap_or_else(|| "prefetch failed".into()));
    }
    Ok(status)
}

/// What a sample says about the whole manifest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub resolution: String,
    pub manifest_pages: u64,
    /// Issues in the manifest: one loc.gov API request each.
    pub manifest_issues: u64,
    pub sampled_pages: u64,
    pub sample_bytes_mean: f64,
    pub sample_bytes_median: f64,
    pub sample_bytes_p90: f64,
    pub sample_download_ms_median: f64,
    /// Σ over (language, decade) strata of the stratum's pages times its
    /// sampled mean; strata with no sampled page use the sample's mean.
    pub projected_bytes: f64,
    /// Strata with no sampled page, and their pages.
    pub unsampled_strata: u64,
    pub unsampled_pages: u64,
    /// The API requests alone at their gap.
    pub api_hours: f64,
    /// The image requests at max(gap, mean download time / concurrency),
    /// for concurrency 1 and 2.
    pub image_hours_c1: f64,
    pub image_hours_c2: f64,
    /// By stratum: (pages in the manifest, sampled, sampled mean bytes).
    pub strata: BTreeMap<String, (u64, u64, f64)>,
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[i]
}

/// Project the whole manifest's size and fetch time from a sample's index
/// lines. `api_gap` and `tile_gap` are the pacing the full run would use.
pub fn estimate(
    manifest: &[ManifestRow],
    index: &[Sidecar],
    api_gap: Duration,
    tile_gap: Duration,
) -> anyhow::Result<Estimate> {
    let by_id: BTreeMap<&str, &ManifestRow> =
        manifest.iter().map(|r| (r.doc_id.as_str(), r)).collect();
    let stratum = |r: &ManifestRow| format!("{}/{}s", r.lang_or_und(), r.decade());
    let mut sampled: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut sizes = Vec::new();
    let mut downloads = Vec::new();
    let mut resolution = String::new();
    // The latest line per page, if a page was fetched twice.
    let mut latest: BTreeMap<&str, &Sidecar> = BTreeMap::new();
    for s in index {
        latest.insert(&s.doc_id, s);
    }
    for s in latest.values() {
        let Some(r) = by_id.get(s.doc_id.as_str()) else {
            continue;
        };
        if resolution.is_empty() {
            resolution = s.resolution.clone();
        } else if resolution != s.resolution {
            bail!("the index mixes {resolution} and {}", s.resolution);
        }
        sampled.entry(stratum(r)).or_default().push(s.bytes as f64);
        sizes.push(s.bytes as f64);
        downloads.push(s.download_ms as f64);
    }
    if sizes.is_empty() {
        bail!("no index line names a page of the manifest");
    }
    sizes.sort_by(f64::total_cmp);
    downloads.sort_by(f64::total_cmp);
    let mean = sizes.iter().sum::<f64>() / sizes.len() as f64;
    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut issues = std::collections::BTreeSet::new();
    for r in manifest {
        *counts.entry(stratum(r)).or_default() += 1;
        issues.insert((r.lccn.as_str(), r.date, r.edition));
    }
    let mut e = Estimate {
        resolution,
        manifest_pages: manifest.len() as u64,
        manifest_issues: issues.len() as u64,
        sampled_pages: sizes.len() as u64,
        sample_bytes_mean: mean,
        sample_bytes_median: quantile(&sizes, 0.5),
        sample_bytes_p90: quantile(&sizes, 0.9),
        sample_download_ms_median: quantile(&downloads, 0.5),
        ..Estimate::default()
    };
    for (k, n) in &counts {
        let (m, got) = match sampled.get(k) {
            Some(v) => (v.iter().sum::<f64>() / v.len() as f64, v.len() as u64),
            None => {
                e.unsampled_strata += 1;
                e.unsampled_pages += n;
                (mean, 0)
            }
        };
        e.projected_bytes += *n as f64 * m;
        e.strata.insert(k.clone(), (*n, got, m));
    }
    let dl = downloads.iter().sum::<f64>() / downloads.len() as f64 / 1000.0;
    let per = |c: f64| tile_gap.as_secs_f64().max(dl / c);
    e.api_hours = e.manifest_issues as f64 * api_gap.as_secs_f64() / 3600.0;
    e.image_hours_c1 = e.manifest_pages as f64 * per(1.0) / 3600.0;
    e.image_hours_c2 = e.manifest_pages as f64 * per(2.0) / 3600.0;
    Ok(e)
}

/// Read index lines (JSON Lines of [`Sidecar`]).
pub fn read_index(text: &str) -> anyhow::Result<Vec<Sidecar>> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("index line {}", i + 1)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loc::LocConfig;
    use chrono::NaiveDate;
    use std::sync::atomic::AtomicUsize;
    use usnm_core::ids::PageKey;

    fn row(lccn: &str, y: i32, seq: u16, lang: &str) -> ManifestRow {
        let key = PageKey::new(lccn, NaiveDate::from_ymd_opt(y, 5, 3).unwrap(), 1, seq).unwrap();
        ManifestRow::new(&key, "batch_x", 1, Some(lang.into()), "detected", 0.5)
    }

    #[test]
    fn samples_every_stratum_and_the_rest_in_proportion() {
        let mut rows = Vec::new();
        for i in 1..=80 {
            rows.push(row("sn1", 1885, i, "deu"));
        }
        for i in 1..=15 {
            rows.push(row("sn2", 1912, i, "eng"));
        }
        for i in 1..=5 {
            rows.push(row("sn3", 1907, i, "srp"));
        }
        let s = sample(&rows, 20);
        assert_eq!(s.len(), 20);
        let n = |l: &str| s.iter().filter(|r| r.lang_or_und() == l).count();
        // One each, then 17 shared 80:15:5 (13.6, 2.55, 0.85; the two
        // largest remainders get the last two).
        assert_eq!((n("deu"), n("eng"), n("srp")), (15, 3, 2));
        assert_eq!(sample(&rows, 20), s, "the same sample every time");
        // Fewer than the strata: the largest ones.
        let s = sample(&rows, 2);
        assert_eq!((n2(&s, "deu"), n2(&s, "eng"), n2(&s, "srp")), (1, 1, 0));
        // More than there are: everything.
        assert_eq!(sample(&rows, 500).len(), 100);
    }

    #[test]
    fn fills_the_sample_when_small_strata_are_full() {
        // One 100-row stratum and 100 one-row strata: 150 asked, 150 given.
        let mut rows: Vec<ManifestRow> = (1..=100).map(|i| row("sn1", 1885, i, "deu")).collect();
        for y in 0..100 {
            rows.push(row("sn2", 1700 + y * 10, 1, "eng"));
        }
        assert_eq!(sample(&rows, 150).len(), 150);
    }

    #[test]
    fn priority_reads_marc_and_iso_codes() {
        let mut r = row("sn1", 1890, 1, "ger");
        assert_eq!(priority(&r), 0);
        r.lang = Some("deu".into());
        assert_eq!(priority(&r), 0);
        r.date = NaiveDate::from_ymd_opt(1925, 1, 1).unwrap();
        assert_eq!(priority(&r), 5);
        for (code, want) in [("cze", 2), ("ces", 2), ("srp", 3), ("rus", 4), ("eng", 5)] {
            assert_eq!(priority(&row("sn1", 1890, 1, code)), want, "{code}");
        }
        let mut r = row("sn1", 1890, 1, "eng");
        r.batch = "nn_kant".into();
        assert_eq!(priority(&r), 1);
    }

    #[test]
    fn priority_order_keeps_an_issue_together() {
        let mut rows = vec![
            row("sn1", 1890, 1, "pol"),
            row("sn1", 1890, 2, "deu"),
            row("sn2", 1890, 1, "deu"),
            row("sn3", 1890, 1, "ces"),
        ];
        // The German page came from another batch, which sorts first.
        rows[1].batch = "aaa_other".into();
        prefetch::sort(&mut rows);
        let picked = select(rows, &Options::default());
        let ids: Vec<String> = picked.iter().map(|r| r.doc_id.clone()).collect();
        // sn1's Polish page goes with its German one, in the German group.
        assert_eq!(
            ids,
            [
                "sn1_1890-05-03_ed-1_seq-1",
                "sn1_1890-05-03_ed-1_seq-2",
                "sn2_1890-05-03_ed-1_seq-1",
                "sn3_1890-05-03_ed-1_seq-1"
            ]
        );
    }

    fn n2(s: &[ManifestRow], l: &str) -> usize {
        s.iter().filter(|r| r.lang_or_und() == l).count()
    }

    #[test]
    fn lays_scans_out_by_set_batch_title_issue_and_page() {
        let r = row("sn83045462", 1890, 7, "ger");
        let p = scan_path(Path::new("/tank/ocr/scans"), &r, ImageSize::Full);
        assert_eq!(
            p,
            Path::new("/tank/ocr/scans/keep/batch_x/sn83045462/1890-05-03/1/7.jpg")
        );
        let e = row("sn83045462", 1890, 7, "eng");
        let p = scan_path(Path::new("/t"), &e, ImageSize::Pct(50));
        assert_eq!(
            p,
            Path::new("/t/buffer/batch_x/sn83045462/1890-05-03/1/7.pct50.jpg")
        );
        assert_eq!(
            sidecar_path(&p),
            Path::new("/t/buffer/batch_x/sn83045462/1890-05-03/1/7.pct50.jpg.json")
        );
    }

    #[test]
    fn keeps_non_english_and_the_worst_english_batches() {
        assert_eq!(set_of(&row("sn1", 1890, 1, "ger")), Set::Keep);
        assert_eq!(set_of(&row("sn1", 1890, 1, "yid")), Set::Keep);
        assert_eq!(set_of(&row("sn1", 1890, 1, "eng")), Set::Buffer);
        let mut r = row("sn1", 1890, 1, "eng");
        r.batch = "txdn_japan".into();
        assert_eq!(set_of(&r), Set::Keep);
        // Keep pages first, whatever the order; one set alone on request.
        let rows = vec![row("sn1", 1890, 1, "eng"), row("sn2", 1890, 1, "pol")];
        let ids = |o: &Options| -> Vec<String> {
            select(rows.clone(), o)
                .into_iter()
                .map(|r| r.lccn)
                .collect()
        };
        let issue = Options {
            order: Order::Issue,
            ..Options::default()
        };
        assert_eq!(ids(&issue), ["sn2", "sn1"]);
        let only = Options {
            sets: Sets::Buffer,
            ..Options::default()
        };
        assert_eq!(ids(&only), ["sn1"]);
    }

    #[tokio::test]
    async fn a_buffer_at_its_cap_doesnt_block_pages_already_there() {
        let dir = tempfile::tempdir().unwrap();
        let r = row("sn1", 1890, 1, "eng");
        let manifest = dir.path().join("m.parquet");
        std::fs::write(
            &manifest,
            prefetch::encode(std::slice::from_ref(&r)).unwrap(),
        )
        .unwrap();
        let out = dir.path().join("scans");
        let path = scan_path(&out, &r, ImageSize::Full);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"jpeg").unwrap();
        let side = Sidecar {
            doc_id: r.doc_id.clone(),
            batch: r.batch.clone(),
            batch_version: 1,
            loc_url: String::new(),
            iiif_base: String::new(),
            resolution: "full".into(),
            fetched_at: Utc::now(),
            bytes: 4,
            sha256: sha256(b"jpeg"),
            download_ms: 0,
            user_agent: String::new(),
        };
        std::fs::write(sidecar_path(&path), serde_json::to_vec(&side).unwrap()).unwrap();
        let opts = Options {
            manifest,
            out,
            buffer_cap: 1,
            ..Options::default()
        };
        let (_tx, rx) = watch::channel(false);
        let s = run(
            opts,
            client("http://127.0.0.1:9", Duration::from_millis(1)),
            rx,
        )
        .await
        .unwrap();
        assert_eq!((s.state.as_str(), s.already_there), ("done", 1));
    }

    #[tokio::test]
    async fn a_consumed_buffer_page_isnt_fetched_again() {
        let dir = tempfile::tempdir().unwrap();
        let r = row("sn1", 1890, 1, "eng");
        let manifest = dir.path().join("m.parquet");
        std::fs::write(
            &manifest,
            prefetch::encode(std::slice::from_ref(&r)).unwrap(),
        )
        .unwrap();
        let out = dir.path().join("scans");
        let path = scan_path(&out, &r, ImageSize::Full);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(consumed_path(&path), b"{}").unwrap();
        let opts = Options {
            manifest,
            out,
            ..Options::default()
        };
        let (_tx, rx) = watch::channel(false);
        // Nothing listens: a fetch would fail.
        let s = run(
            opts,
            client("http://127.0.0.1:9", Duration::from_millis(1)),
            rx,
        )
        .await
        .unwrap();
        assert_eq!((s.already_there, s.fetched, s.failed_transient), (1, 0, 0));
    }

    #[test]
    fn counts_only_scans_and_a_missing_buffer_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(dir_scan_bytes(&dir.path().join("none")).unwrap(), 0);
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/1.jpg"), b"1234").unwrap();
        std::fs::write(dir.path().join("a/b/1.jpg.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("a/2.jpg.done"), b"{}").unwrap();
        assert_eq!(dir_scan_bytes(dir.path()).unwrap(), 4);
    }

    #[tokio::test]
    async fn waits_at_the_buffer_cap_until_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let rows: Vec<ManifestRow> = (1..=2).map(|i| row("sn1", 1890, i, "eng")).collect();
        let manifest = dir.path().join("m.parquet");
        std::fs::write(&manifest, prefetch::encode(&rows).unwrap()).unwrap();
        let out = dir.path().join("scans");
        // Something in the buffer already, over a cap of 1 byte.
        std::fs::create_dir_all(out.join("buffer/x")).unwrap();
        std::fs::write(out.join("buffer/x/1.jpg"), b"jpeg").unwrap();
        let opts = Options {
            manifest,
            out: out.clone(),
            buffer_cap: 1,
            ..Options::default()
        };
        let (tx, rx) = watch::channel(false);
        let run = tokio::spawn(run(
            opts,
            client("http://127.0.0.1:9", Duration::from_millis(1)),
            rx,
        ));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status: Status =
            serde_json::from_slice(&std::fs::read(out.join("status-full.json")).unwrap()).unwrap();
        assert_eq!(status.state, "waiting");
        assert_eq!(status.buffer_bytes, 4);
        tx.send_replace(true);
        let s = run.await.unwrap().unwrap();
        assert_eq!((s.state.as_str(), s.fetched), ("stopped", 0));
    }

    #[test]
    fn projects_by_stratum() {
        let manifest: Vec<ManifestRow> = (1..=10)
            .map(|i| row("sn1", 1885, i, "deu"))
            .chain((1..=10).map(|i| row("sn2", 1912, i, "eng")))
            .chain((1..=4).map(|i| row("sn3", 1907, i, "srp")))
            .collect();
        let line = |r: &ManifestRow, bytes: u64| Sidecar {
            doc_id: r.doc_id.clone(),
            batch: r.batch.clone(),
            batch_version: 1,
            loc_url: String::new(),
            iiif_base: String::new(),
            resolution: "full".into(),
            fetched_at: Utc::now(),
            bytes,
            sha256: String::new(),
            download_ms: 3000,
            user_agent: USER_AGENT.into(),
        };
        let index = vec![
            line(&manifest[0], 4_000_000),
            line(&manifest[1], 2_000_000),
            line(&manifest[10], 1_000_000),
        ];
        let e = estimate(
            &manifest,
            &index,
            Duration::from_millis(3500),
            Duration::from_secs(1),
        )
        .unwrap();
        // deu: 10 × 3 MB; eng: 10 × 1 MB; srp unsampled: 4 × the mean (7/3 MB).
        let want = 30e6 + 10e6 + 4.0 * 7e6 / 3.0;
        assert!(
            (e.projected_bytes - want).abs() < 1.0,
            "{}",
            e.projected_bytes
        );
        assert_eq!((e.unsampled_strata, e.unsampled_pages), (1, 4));
        assert_eq!(e.manifest_issues, 3);
        // 3 s downloads: 3 s a page alone, 1.5 s with two in flight.
        assert!((e.image_hours_c1 - 24.0 * 3.0 / 3600.0).abs() < 1e-9);
        assert!((e.image_hours_c2 - 24.0 * 1.5 / 3600.0).abs() < 1e-9);
        assert!((e.api_hours - 3.0 * 3.5 / 3600.0).abs() < 1e-9);
    }

    /// A stand-in loc.gov with one issue of three pages. The image server
    /// answers its first request 429 with `Retry-After: 2`, serves page 3's
    /// image as missing (404), and records when each image request came.
    async fn fake_loc(
        times: Arc<Mutex<Vec<std::time::Instant>>>,
        hits: Arc<AtomicUsize>,
    ) -> String {
        use axum::http::StatusCode as S;
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let jp2 = |n: &str| {
            format!("https://tile.loc.gov/storage-services/service/ndnp/x/batch_x/data/sn1/1/1890050301/{n}.jp2")
        };
        let item = serde_json::json!({"resources": [{"files": [
            [{"mimetype": "image/jp2", "url": jp2("0001")}],
            [{"mimetype": "image/jp2", "url": jp2("0002")}],
            [{"mimetype": "image/jp2", "url": jp2("0003")}],
        ]}]});
        let app = axum::Router::new()
            .route(
                "/item/sn1/1890-05-03/ed-1/",
                axum::routing::get(move || {
                    let item = item.clone();
                    async move { axum::Json(item) }
                }),
            )
            .route(
                "/iiif/{service}/full/{size}/0/default.jpg",
                axum::routing::get(
                    move |axum::extract::Path((service, size)): axum::extract::Path<(
                        String,
                        String,
                    )>,
                          headers: axum::http::HeaderMap| {
                        let times = times.clone();
                        let hits = hits.clone();
                        async move {
                            assert!(headers["user-agent"]
                                .to_str()
                                .unwrap()
                                .starts_with("usnewsmap-ocr-worker/"));
                            times.lock().unwrap().push(std::time::Instant::now());
                            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                                return (S::TOO_MANY_REQUESTS, [("retry-after", "2")])
                                    .into_response();
                            }
                            if service.ends_with("0003") {
                                return S::NOT_FOUND.into_response();
                            }
                            format!("jpeg {service} {size}")
                                .into_bytes()
                                .into_response()
                        }
                    },
                ),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        root
    }

    fn client(root: &str, tile_gap: Duration) -> Arc<LocClient> {
        Arc::new(
            LocClient::new(LocConfig {
                api_gap: Duration::from_millis(10),
                tile_gap,
                attempts: 3,
                backoff: Duration::from_millis(10),
                api_root: root.to_owned(),
                iiif_root: format!("{root}/iiif/"),
            })
            .unwrap(),
        )
    }

    async fn manifest(dir: &Path) -> PathBuf {
        let rows: Vec<ManifestRow> = (1..=3).map(|i| row("sn1", 1890, i, "deu")).collect();
        let p = dir.join("manifest.parquet");
        std::fs::write(&p, prefetch::encode(&rows).unwrap()).unwrap();
        p
    }

    #[tokio::test]
    async fn fetches_paced_honours_retry_after_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let times = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let root = fake_loc(times.clone(), hits.clone()).await;
        let out = dir.path().join("scans");
        let opts = Options {
            manifest: manifest(dir.path()).await,
            out: out.clone(),
            size: ImageSize::Pct(50),
            concurrency: 2,
            progress_every: 1,
            ..Options::default()
        };
        let (_tx, rx) = watch::channel(false);
        let t0 = std::time::Instant::now();
        let s = run(
            opts.clone(),
            client(&root, Duration::from_millis(300)),
            rx.clone(),
        )
        .await
        .unwrap();
        assert!(
            t0.elapsed() >= Duration::from_secs(2),
            "waited out Retry-After"
        );
        assert_eq!(s.state, "done");
        assert_eq!((s.fetched, s.failed_permanent, s.already_there), (2, 1, 0));
        // Every image request after the first waited for the hold, and they
        // were at least the gap apart.
        let t = times.lock().unwrap().clone();
        assert_eq!(t.len(), 4);
        assert!(t[1] - t[0] >= Duration::from_secs(2));
        for w in t[1..].windows(2) {
            assert!(
                w[1] - w[0] >= Duration::from_millis(290),
                "{:?}",
                w[1] - w[0]
            );
        }
        let r = row("sn1", 1890, 1, "deu");
        let scan = scan_path(&out, &r, ImageSize::Pct(50));
        let body = std::fs::read(&scan).unwrap();
        assert!(String::from_utf8_lossy(&body).ends_with("0001 pct:50"));
        let side: Sidecar =
            serde_json::from_slice(&std::fs::read(sidecar_path(&scan)).unwrap()).unwrap();
        assert_eq!(side.sha256, sha256(&body));
        assert_eq!(side.bytes, body.len() as u64);
        assert!(side.loc_url.ends_with(":0001/full/pct:50/0/default.jpg"));
        assert_eq!(side.resolution, "scaled:50");
        let index =
            read_index(&std::fs::read_to_string(out.join("index-pct50.jsonl")).unwrap()).unwrap();
        assert_eq!(index.len(), 2);
        let failures = std::fs::read_to_string(out.join("failures-pct50.jsonl")).unwrap();
        assert!(failures.contains("sn1_1890-05-03_ed-1_seq-3") && failures.contains("permanent"));
        let status: Status =
            serde_json::from_slice(&std::fs::read(out.join("status-pct50.json")).unwrap()).unwrap();
        assert_eq!(status.state, "done");

        // Again: the two scans are there and verified, so only page 3 is asked for.
        let before = hits.load(Ordering::SeqCst);
        let s = run(
            opts.clone(),
            client(&root, Duration::from_millis(10)),
            rx.clone(),
        )
        .await
        .unwrap();
        assert_eq!((s.already_there, s.fetched, s.failed_permanent), (2, 0, 1));
        assert_eq!(hits.load(Ordering::SeqCst), before + 1);

        // A scan that no longer matches its sidecar is fetched again.
        std::fs::write(&scan, b"changed").unwrap();
        let s = run(opts, client(&root, Duration::from_millis(10)), rx)
            .await
            .unwrap();
        assert_eq!((s.already_there, s.fetched), (1, 1));
    }

    #[tokio::test]
    async fn one_process_at_a_time_and_a_run_of_failures_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("scans");
        std::fs::create_dir_all(&out).unwrap();
        let path = out.join(".prefetch.lock");
        let held = lock(&path).unwrap();
        // flock is per open file description, so a second open file is refused.
        assert!(lock(&path).is_err());
        drop(held);
        // Nothing listens here: every page fails, transiently.
        let opts = Options {
            manifest: manifest(dir.path()).await,
            out: out.clone(),
            concurrency: 1,
            max_failures_in_a_row: 2,
            ..Options::default()
        };
        let (_tx, rx) = watch::channel(false);
        let err = run(
            opts,
            client("http://127.0.0.1:9", Duration::from_millis(1)),
            rx,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("failed in a row"), "{err}");
        let status: Status =
            serde_json::from_slice(&std::fs::read(out.join("status-full.json")).unwrap()).unwrap();
        assert_eq!(status.state, "failed");
        assert_eq!(status.failed_transient, 2);
    }
}
