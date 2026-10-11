//! Packaged sample sets in the archival account's `sets` container
//! (docs/operations.md, "Sample sets"), built once and never replaced:
//!
//! - `{name}/raw.tar`: the set's LoC batch archives as retained (`raw/`),
//!   each with its manifest, in a plain tar (the archives are bzip2
//!   already), so a container can fetch one blob and curate offline.
//! - `{name}/docs.ndjson.zst`: the sample's documents exactly as the release
//!   builds them ([`super::sample`]), one zstd stream (the sample's parts,
//!   whose frames concatenate), so a container can load any Quickwit
//!   without curating.
//! - `{name}/manifest.json`, written last: what the set is, how it was
//!   chosen, and every file's size and sha256.
//!
//! Everything streams: nothing is held whole.

use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usnm_store::ObjectStore;

use super::sample;
use crate::raw;
use crate::source::hex;

/// One batch of a set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetBatch {
    /// With its version: `az_fireant_ver01`.
    pub batch: String,
    /// Its path in `raw.tar` (and in the `raw` container).
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub source_url: String,
}

/// One file of a set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// A set's `manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    /// The `-vN` of the name.
    pub version: Option<u32>,
    pub created_at: DateTime<Utc>,
    /// How the batches were chosen (e.g. every 100th batch of LoC's listing
    /// by name from offset 39).
    pub selection: String,
    /// The published version of the environment the set was built from.
    pub source_version: String,
    pub batches: Vec<SetBatch>,
    /// Pages in the batches (every page of every curated part), and the
    /// documents a release builds from them.
    pub pages: u64,
    pub docs: u64,
    /// Whether the documents have American Stories' text (`text_as`,
    /// `text_as_cg`), and at which version.
    pub american_stories: bool,
    pub american_stories_version: Option<u32>,
    /// The common-word pairs' version (`text_cg`).
    pub common_grams: u32,
    /// sha256 of `infra/quickwit/pages-index.yaml` the documents fit.
    pub index_config_sha256: String,
    /// The corpus bounds searches clamp to (`current.json`).
    pub bounds: (String, String),
    /// The builder: its commit (`USNM_GIT_SHA`) and package version.
    pub git_sha: Option<String>,
    pub builder: String,
    pub files: Vec<SetFile>,
}

/// The `N` of a `-vN` name.
pub fn version_of(name: &str) -> Option<u32> {
    name.rsplit_once("-v").and_then(|(_, v)| v.parse().ok())
}

/// A set's manifest.
pub async fn manifest(sets: &dyn ObjectStore, name: &str) -> anyhow::Result<Manifest> {
    let path = format!("{name}/manifest.json");
    let bytes = sets
        .get(&path)
        .await?
        .with_context(|| format!("no set `{name}` (`{path}` is missing)"))?;
    serde_json::from_slice(&bytes).context(path)
}

/// A tar header for one file (ustar).
fn tar_header(path: &str, size: u64, mtime: DateTime<Utc>) -> anyhow::Result<Vec<u8>> {
    let mut h = tar::Header::new_ustar();
    h.set_path(path)?;
    h.set_size(size);
    h.set_mode(0o644);
    h.set_mtime(u64::try_from(mtime.timestamp()).unwrap_or(0));
    h.set_entry_type(tar::EntryType::Regular);
    h.set_cksum();
    Ok(h.as_bytes().to_vec())
}

/// Zeros that pad `size` bytes to the tar's 512-byte records.
fn tar_padding(size: u64) -> Vec<u8> {
    vec![0u8; ((512 - size % 512) % 512) as usize]
}

/// Sends chunks to an upload, counting and hashing them.
struct Sink {
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
    hash: Sha256,
    bytes: u64,
}

impl Sink {
    async fn send(&mut self, chunk: bytes::Bytes) -> anyhow::Result<()> {
        self.hash.update(&chunk);
        self.bytes += chunk.len() as u64;
        self.tx
            .send(chunk)
            .await
            .map_err(|_| anyhow::anyhow!("the upload stopped taking data"))
    }
}

/// Stream `path` of `sets` from what `fill` sends, then commit it in the
/// Cold tier if `fill` succeeded. Its size and sha256.
async fn upload<F, Fut>(sets: Arc<dyn ObjectStore>, path: &str, fill: F) -> anyhow::Result<SetFile>
where
    F: FnOnce(Sink) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Sink>>,
{
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let (commit, committed) = tokio::sync::oneshot::channel();
    let p = path.to_owned();
    let task = tokio::spawn(async move {
        sets.put_stream(
            &p,
            "application/octet-stream",
            Some(raw::TIER),
            rx,
            committed,
        )
        .await
    });
    let sink = Sink {
        tx,
        hash: Sha256::new(),
        bytes: 0,
    };
    match fill(sink).await {
        Ok(sink) => {
            let Sink { tx, hash, bytes } = sink;
            drop(tx);
            let _ = commit.send(true);
            let n = task.await??;
            if n != bytes {
                bail!("`{path}`: uploaded {n} bytes, sent {bytes}");
            }
            Ok(SetFile {
                path: path.to_owned(),
                bytes,
                sha256: hex(&hash.finalize()),
            })
        }
        Err(e) => {
            let _ = commit.send(false);
            Err(e)
        }
    }
}

/// What a set is built from.
pub struct Sources<'a> {
    /// The archival `raw` container.
    pub raw: Arc<dyn ObjectStore>,
    /// The sample's store and prefix ([`super::sample`]).
    pub sample: &'a dyn ObjectStore,
    pub sample_prefix: &'a str,
    /// The published version's batches (`{batch}_verNN`).
    pub batches: Vec<String>,
}

/// Build set `name` in `sets` (refusing one that exists).
pub async fn bundle(
    sets: Arc<dyn ObjectStore>,
    name: &str,
    selection: &str,
    src: Sources<'_>,
) -> anyhow::Result<Manifest> {
    if !usnm_store::is_safe_segment(name) {
        bail!("`{name}` can't name a set");
    }
    // One build per set, and a set is never replaced.
    super::claim(sets.as_ref(), name).await?;
    let sm = sample::manifest(src.sample, src.sample_prefix).await?;
    if sm.pct < 100.0 {
        tracing::warn!(
            pct = sm.pct,
            "the sample is a share of its version's pages, but raw.tar holds whole batches"
        );
    }
    let created_at = Utc::now();
    // Every batch's retained archive, before anything is written.
    let mut batches = Vec::new();
    for b in &src.batches {
        let m = raw::find(src.raw.as_ref(), b, None)
            .await?
            .with_context(|| format!("batch `{b}` has no retained archive in `raw`"))?;
        batches.push(m);
    }
    tracing::info!(
        set = name,
        batches = batches.len(),
        docs = sm.docs,
        "building a set"
    );

    let (raw_store, listed) = (src.raw.clone(), batches.clone());
    let tar = upload(sets.clone(), &format!("{name}/raw.tar"), |mut sink| async move {
        for m in &listed {
            sink.send(tar_header(&m.path, m.bytes, m.fetched_at)?.into())
                .await?;
            let mut stream = raw_store
                .get_stream(&m.path)
                .await?
                .with_context(|| format!("`{}` is missing", m.path))?;
            let (mut hash, mut n) = (Sha256::new(), 0u64);
            while let Some(c) = stream.next().await {
                let c = c?;
                hash.update(&c);
                n += c.len() as u64;
                sink.send(c).await?;
            }
            let sha = hex(&hash.finalize());
            if n != m.bytes || !sha.eq_ignore_ascii_case(&m.sha256) {
                bail!(
                    "`{}` reads as {n} bytes, sha256 {sha}; its manifest says {} bytes, {}",
                    m.path,
                    m.bytes,
                    m.sha256
                );
            }
            sink.send(tar_padding(n).into()).await?;
            let manifest = serde_json::to_vec_pretty(m)?;
            let mpath = format!("{}/manifest.json", m.batch);
            sink.send(tar_header(&mpath, manifest.len() as u64, m.fetched_at)?.into())
                .await?;
            let len = manifest.len() as u64;
            sink.send(manifest.into()).await?;
            sink.send(tar_padding(len).into()).await?;
            tracing::info!(batch = %m.batch, mb = n / (1024 * 1024), "archive added to the set");
        }
        // The end of the archive: two zero records.
        sink.send(vec![0u8; 1024].into()).await?;
        Ok(sink)
    })
    .await?;

    let (parts, sample_store) = (sm.parts.clone(), src.sample);
    let docs = upload(
        sets.clone(),
        &format!("{name}/docs.ndjson.zst"),
        |mut sink| async move {
            // Streamed: a part can be any size.
            for p in &parts {
                let mut stream = sample_store
                    .get_stream(&p.path)
                    .await?
                    .with_context(|| format!("sample part `{}` is missing", p.path))?;
                while let Some(c) = stream.next().await {
                    sink.send(c?).await?;
                }
            }
            Ok(sink)
        },
    )
    .await?;

    let m = Manifest {
        name: name.to_owned(),
        version: version_of(name),
        created_at,
        selection: selection.to_owned(),
        source_version: sm.version.clone(),
        batches: batches
            .iter()
            .map(|m| SetBatch {
                batch: m.batch.clone(),
                path: m.path.clone(),
                bytes: m.bytes,
                sha256: m.sha256.clone(),
                source_url: m.source_url.clone(),
            })
            .collect(),
        pages: sm.pages_read,
        docs: sm.docs,
        american_stories: sm.american_stories,
        american_stories_version: sm
            .american_stories
            .then_some(usnm_core::american_stories::VERSION),
        common_grams: sm.common_grams,
        index_config_sha256: hex(&Sha256::digest(crate::sink::INDEX_TEMPLATE.as_bytes())),
        bounds: sm.bounds.clone(),
        git_sha: std::env::var("USNM_GIT_SHA").ok().filter(|s| !s.is_empty()),
        builder: concat!("usnm-qwcluster bundle ", env!("CARGO_PKG_VERSION")).to_owned(),
        files: vec![tar, docs],
    };
    // Last, so a set with a manifest is a complete one.
    if !sets
        .put_new(
            &format!("{name}/manifest.json"),
            serde_json::to_vec_pretty(&m)?,
            "application/json",
        )
        .await?
    {
        bail!("set `{name}` was created meanwhile; build another version");
    }
    Ok(m)
}

/// Send set `name`'s documents to `tx` as request bodies of at most
/// `limit` bytes (whole lines), streaming, checking the file's sha256 at
/// the end.
pub async fn feed_docs(
    sets: &dyn ObjectStore,
    m: &Manifest,
    limit: usize,
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> anyhow::Result<()> {
    let file = m
        .files
        .iter()
        .find(|f| f.path.ends_with("/docs.ndjson.zst"))
        .context("the set has no docs.ndjson.zst")?;
    sample::feed(
        sets,
        &file.path,
        Some(file.bytes),
        Some(&file.sha256),
        limit,
        tx,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_padding() {
        assert_eq!(version_of("loc-1pct-v1"), Some(1));
        assert_eq!(version_of("loc-1pct-v12"), Some(12));
        assert_eq!(version_of("loc-1pct"), None);
        assert_eq!(tar_padding(512).len(), 0);
        assert_eq!(tar_padding(1).len(), 511);
        assert_eq!(tar_header("a/b.tar.bz2", 5, Utc::now()).unwrap().len(), 512);
    }
}
