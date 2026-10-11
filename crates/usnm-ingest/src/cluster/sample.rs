//! A fixed sample of the published version's pages, as the documents a full
//! release builds for them (05 §5.5, `release::page_doc`): LoC's text and its
//! common-word pairs, and with `american_stories` American Stories' text and
//! its pairs, every other field as the release writes it.
//!
//! **Which pages.** A page is in the sample when the first 8 bytes of
//! SHA-256 of its `doc_id`, read as a big-endian integer, modulo 10,000, are
//! below `pct × 100`: the OCR audit's rule (ja-ocr/quality.py `sampled`)
//! with SHA-256 for its BLAKE2b, which the Rust side has no crate for. The
//! same pages come out every run, and a 1% sample is a 1% sample of every
//! title, year and batch.
//!
//! **What it reads.** The published version (`current.json`): its batch list,
//! titles and places from its snapshot, so the sample needs no pipeline
//! state. The copies of a page that ships in more than one batch are
//! settled as a full release does (`dedup::plan`), so each page is in the
//! sample once. Every curated part of every batch is read in full (the
//! hash needs the id, and a part's rows are mixed), so the sample costs one
//! pass over the curated lake, plus American Stories' parts when asked.
//!
//! **What it writes.** `{prefix}/docs-{n}.ndjson.zst` (zstd NDJSON, about
//! 64 MiB before compression each, in batch order) and `{prefix}/manifest.json`
//! (what was sampled, counts, timings); [`super::load`] reads them back.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use usnm_core::text::TextStatus;
use usnm_core::time::day_number;
use usnm_store::ObjectStore;

use crate::american_stories;
use crate::catalog::Catalog;
use crate::curated::{read_part, CuratedRow};
use crate::dedup::{self, Plan};
use crate::release::page_doc;
use crate::state::{RunBatch, DUPLICATES_FILE, RUN_BATCHES_FILE};
use crate::worker::Counts;

/// Uncompressed bytes per sample part.
pub const PART_BYTES: usize = 64 * 1024 * 1024;

/// Whether a page is in a sample of `cut` in 10,000.
pub fn sampled(doc_id: &str, cut: u32) -> bool {
    let h = Sha256::digest(doc_id.as_bytes());
    let mut first = [0u8; 8];
    first.copy_from_slice(&h[..8]);
    u64::from_be_bytes(first) % 10_000 < u64::from(cut)
}

/// `pct` as a cut in 10,000: 1 → 100, 0.5 → 50. Between 0.01 and 100.
pub fn cut_of(pct: f64) -> anyhow::Result<u32> {
    if !(0.01..=100.0).contains(&pct) {
        bail!("the sample share must be between 0.01 and 100 percent, not {pct}");
    }
    Ok((pct * 100.0).round() as u32)
}

/// The published version a sample is taken from.
pub struct Published {
    pub version: String,
    pub batches: Vec<RunBatch>,
    pub catalog: Catalog,
    /// `current.json`'s `bounds` (`from`, `to`), which searches clamp to.
    pub bounds: (String, String),
    /// `current.json`'s `american_stories`: the version's indexes have it.
    pub american_stories: Option<u64>,
    /// The copies the version hides at search time (its `duplicates.json`).
    pub hidden: usize,
    /// The analyzer its main indexes were built with (`common_grams`, #168):
    /// version 1 where absent, the latest for a version this code doesn't know.
    pub analyzer: usnm_core::text::Analyzer,
}

/// Read the published version from the reference store.
pub async fn published(reference: &dyn ObjectStore) -> anyhow::Result<Published> {
    let json = |path: String| async move {
        let bytes = reference
            .get(&path)
            .await?
            .with_context(|| format!("`{path}` is missing"))?;
        serde_json::from_slice::<Value>(&bytes).with_context(|| path.clone())
    };
    let current = json("current.json".into()).await?;
    let version = current["index_version"]
        .as_str()
        .context("current.json has no index_version")?
        .to_owned();
    if !usnm_store::is_safe_segment(&version) {
        bail!("current.json names `{version}`, which isn't a version id");
    }
    let batches: Vec<RunBatch> =
        serde_json::from_value(json(format!("{version}/{RUN_BATCHES_FILE}")).await?)
            .context(RUN_BATCHES_FILE)?;
    let catalog = Catalog::new(
        serde_json::from_value(json(format!("{version}/titles.json")).await?)
            .context("titles.json")?,
        serde_json::from_value(json(format!("{version}/places.json")).await?)
            .context("places.json")?,
    )?;
    let hidden = match reference
        .get(&format!("{version}/{DUPLICATES_FILE}"))
        .await?
    {
        Some(b) => serde_json::from_slice::<Vec<Value>>(&b)
            .map(|v| v.len())
            .unwrap_or(0),
        None => 0,
    };
    let bound = |k: &str| current["bounds"][k].as_str().unwrap_or_default().to_owned();
    Ok(Published {
        version,
        batches,
        catalog,
        bounds: (bound("from"), bound("to")),
        american_stories: current["american_stories"].as_u64(),
        hidden,
        analyzer: match current["common_grams"].as_u64() {
            None => usnm_core::text::Analyzer::V1,
            Some(v) => u32::try_from(v)
                .ok()
                .and_then(usnm_core::common_grams::analyzer)
                .unwrap_or(usnm_core::text::Analyzer::LATEST),
        },
    })
}

/// The batch list of `version` (its `batches.json`), e.g. the version a
/// sample was taken from, whatever is published now.
pub async fn run_batches(
    reference: &dyn ObjectStore,
    version: &str,
) -> anyhow::Result<Vec<RunBatch>> {
    if !usnm_store::is_safe_segment(version) {
        bail!("`{version}` isn't a version id");
    }
    let path = format!("{version}/{RUN_BATCHES_FILE}");
    let bytes = reference
        .get(&path)
        .await?
        .with_context(|| format!("`{path}` is missing"))?;
    serde_json::from_slice(&bytes).context(path)
}

/// How to take a sample.
#[derive(Debug, Clone)]
pub struct Spec {
    /// Pages in 10,000 ([`cut_of`]).
    pub cut: u32,
    pub american_stories: bool,
    /// Batches read at once.
    pub concurrency: usize,
    pub part_bytes: usize,
    /// Only the first `n` batches of the version (a trial run).
    pub max_batches: Option<usize>,
}

/// One sample part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartInfo {
    pub path: String,
    pub docs: u64,
    /// Before compression.
    pub bytes: u64,
    pub compressed_bytes: u64,
}

/// What a sample holds, as `manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub pct: f64,
    pub cut: u32,
    pub rule: String,
    pub american_stories: bool,
    pub common_grams: u32,
    pub bounds: (String, String),
    pub batches: u64,
    /// Rows of the curated parts read (every page of every batch).
    pub pages_read: u64,
    pub docs: u64,
    /// Documents with American Stories' text, and those with only it.
    pub docs_with_as: u64,
    pub docs_only_as: u64,
    pub bytes: u64,
    pub compressed_bytes: u64,
    pub parts: Vec<PartInfo>,
    pub started_at: DateTime<Utc>,
    pub secs: f64,
}

/// The documents of one batch's sampled pages.
#[derive(Default)]
struct BatchSample {
    lines: Vec<u8>,
    docs: u64,
    pages_read: u64,
    with_as: u64,
    only_as: u64,
}

struct Shared {
    curated: Arc<dyn ObjectStore>,
    catalog: Catalog,
    plan: Plan,
    stories: Option<american_stories::Source>,
    cut: u32,
    analyzer: usnm_core::text::Analyzer,
}

async fn sample_batch(s: Arc<Shared>, b: RunBatch) -> anyhow::Result<BatchSample> {
    let mut out = BatchSample::default();
    let mut rows: Vec<CuratedRow> = Vec::new();
    for path in &b.curated.parts {
        let bytes = s
            .curated
            .get(path)
            .await?
            .with_context(|| format!("curated part `{path}` is missing"))?;
        let (sh, batch) = (s.clone(), b.batch.clone());
        let (kept, read) = tokio::task::spawn_blocking(move || {
            let mut kept = Vec::new();
            let mut read = 0u64;
            read_part(bytes.into(), true, |row| {
                read += 1;
                if sampled(&row.key.doc_id(), sh.cut) && sh.plan.keeps(&row.key, &batch) {
                    kept.push(row);
                }
                Ok(())
            })
            .map(|()| (kept, read))
        })
        .await?
        .with_context(|| path.clone())?;
        out.pages_read += read;
        rows.extend(kept);
    }
    // American Stories' text for the sampled pages only: their title-days.
    let texts = match &s.stories {
        Some(source) if !rows.is_empty() => {
            let mut counts = Counts::new();
            for r in &rows {
                *counts
                    .entry(r.key.lccn.clone())
                    .or_default()
                    .entry(day_number(r.key.date))
                    .or_default() += 1;
            }
            source.load(s.curated.as_ref(), &counts).await?.texts
        }
        _ => Default::default(),
    };
    for row in &rows {
        let text_as = texts.get(&row.key.doc_id()).map(String::as_str);
        let ok = row.status == TextStatus::Ok;
        if !ok && text_as.is_none() {
            continue;
        }
        let title = s
            .catalog
            .title(&row.key.lccn)
            .with_context(|| format!("title `{}` is missing from the catalog", row.key.lccn))?;
        let place = s.catalog.place(&title.place_id).context("place")?;
        // No `decade` field: the cluster's indexes take the template as
        // it is (`load::index_config`, `sink::INDEX_TEMPLATE`), as do sets.
        let doc = page_doc(
            row,
            title,
            place,
            text_as,
            crate::release::DocFormat {
                decade: false,
                analyzer: s.analyzer,
                // Separate fields, as the template the cluster loads has.
                text: usnm_core::text_layout::TextLayout::Separate,
            },
        );
        serde_json::to_writer(&mut out.lines, &doc)?;
        out.lines.push(b'\n');
        out.docs += 1;
        out.with_as += u64::from(text_as.is_some());
        out.only_as += u64::from(!ok);
    }
    Ok(out)
}

/// Take the sample of `published` into `out` under `prefix`.
pub async fn build(
    curated: Arc<dyn ObjectStore>,
    published: Published,
    out: &dyn ObjectStore,
    prefix: &str,
    spec: &Spec,
) -> anyhow::Result<Manifest> {
    usnm_store::validate_path(prefix)?;
    // One build per prefix, and a finished sample is never replaced: a load
    // may be reading it.
    super::claim(out, prefix).await?;
    let started_at = Utc::now();
    let start = tokio::time::Instant::now();
    let mut batches = published.batches;
    if let Some(n) = spec.max_batches {
        batches.truncate(n);
    }
    if spec.american_stories && published.american_stories.is_none() {
        tracing::warn!(
            version = %published.version,
            "the published version has no American Stories' text; sampling it anyway"
        );
    }
    let plan = dedup::plan(
        curated.as_ref(),
        &batches,
        &BTreeSet::new(),
        &Some(BTreeSet::new()),
        spec.american_stories,
    )
    .await?;
    let stories = if spec.american_stories {
        american_stories::check_written(curated.as_ref()).await?;
        Some(american_stories::Source::list(curated.as_ref()).await?)
    } else {
        None
    };
    tracing::info!(
        version = %published.version,
        batches = batches.len(),
        pct = f64::from(spec.cut) / 100.0,
        duplicate_pages = plan.duplicate_pages,
        american_stories = spec.american_stories,
        "sampling"
    );
    let shared = Arc::new(Shared {
        curated,
        catalog: published.catalog,
        plan,
        stories,
        cut: spec.cut,
        analyzer: published.analyzer,
    });
    let total = batches.len();
    let mut results = stream::iter(batches)
        .map(|b| tokio::spawn(sample_batch(shared.clone(), b)))
        .buffered(spec.concurrency.max(1));

    let mut m = Manifest {
        version: published.version,
        pct: f64::from(spec.cut) / 100.0,
        cut: spec.cut,
        rule: "sha256(doc_id)[0..8] big-endian mod 10000 < cut".into(),
        american_stories: spec.american_stories,
        // The pairs' version the documents were built with: the published
        // version's, which bench parses queries for.
        common_grams: usnm_core::common_grams::version(published.analyzer),
        bounds: published.bounds,
        batches: 0,
        pages_read: 0,
        docs: 0,
        docs_with_as: 0,
        docs_only_as: 0,
        bytes: 0,
        compressed_bytes: 0,
        parts: Vec::new(),
        started_at,
        secs: 0.0,
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut last_log = tokio::time::Instant::now();
    while let Some(joined) = results.next().await {
        let b = joined.context("a batch's task failed")??;
        m.batches += 1;
        m.pages_read += b.pages_read;
        m.docs += b.docs;
        m.docs_with_as += b.with_as;
        m.docs_only_as += b.only_as;
        buf.extend_from_slice(&b.lines);
        // A batch can be larger than several parts: cut at line boundaries.
        while buf.len() >= spec.part_bytes {
            let rest = buf.split_off(cut_point(&buf, spec.part_bytes));
            flush(out, prefix, &mut buf, &mut m).await?;
            buf = rest;
        }
        if last_log.elapsed() >= Duration::from_secs(30) || m.batches as usize == total {
            last_log = tokio::time::Instant::now();
            let secs = start.elapsed().as_secs_f64().max(0.001);
            tracing::info!(
                batches = m.batches,
                of = total,
                pages_read = m.pages_read,
                pages_per_sec = (m.pages_read as f64 / secs).round(),
                docs = m.docs,
                mb_written = m.compressed_bytes / (1024 * 1024),
                "sample progress"
            );
        }
    }
    flush(out, prefix, &mut buf, &mut m).await?;
    m.secs = start.elapsed().as_secs_f64();
    out.put(
        &format!("{prefix}/manifest.json"),
        serde_json::to_vec_pretty(&m)?,
        "application/json",
    )
    .await?;
    Ok(m)
}

/// Where to end a part of `buf` (whole lines, `buf.len() >= limit`): after
/// the last line that ends within `limit` bytes, or after the first line if
/// that one alone is longer.
fn cut_point(buf: &[u8], limit: usize) -> usize {
    let within = &buf[..limit.min(buf.len())];
    match within.iter().rposition(|&b| b == b'\n') {
        Some(i) => i + 1,
        None => buf
            .iter()
            .position(|&b| b == b'\n')
            .map_or(buf.len(), |i| i + 1),
    }
}

async fn flush(
    out: &dyn ObjectStore,
    prefix: &str,
    buf: &mut Vec<u8>,
    m: &mut Manifest,
) -> anyhow::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let raw = std::mem::take(buf);
    let bytes = raw.len() as u64;
    let docs = raw.iter().filter(|&&b| b == b'\n').count() as u64;
    let compressed = tokio::task::spawn_blocking(move || zstd::encode_all(&raw[..], 3)).await??;
    let path = format!("{prefix}/docs-{:05}.ndjson.zst", m.parts.len());
    let compressed_bytes = compressed.len() as u64;
    out.put(&path, compressed, "application/zstd").await?;
    m.parts.push(PartInfo {
        path,
        docs,
        bytes,
        compressed_bytes,
    });
    m.bytes += bytes;
    m.compressed_bytes += compressed_bytes;
    Ok(())
}

/// A sample's manifest.
pub async fn manifest(store: &dyn ObjectStore, prefix: &str) -> anyhow::Result<Manifest> {
    let path = format!("{prefix}/manifest.json");
    let bytes = store
        .get(&path)
        .await?
        .with_context(|| format!("no sample at `{prefix}` (`{path}` is missing)"))?;
    serde_json::from_slice(&bytes).context(path)
}

/// One sample part's NDJSON, decompressed, read as a stream (no store
/// size limit applies). For tests and small parts; loads use [`feed`].
pub async fn part(store: &dyn ObjectStore, path: &str) -> anyhow::Result<Vec<u8>> {
    use futures::StreamExt;
    let mut stream = store
        .get_stream(path)
        .await?
        .with_context(|| format!("sample part `{path}` is missing"))?;
    let mut compressed = Vec::new();
    while let Some(c) = stream.next().await {
        compressed.extend_from_slice(&c?);
    }
    Ok(tokio::task::spawn_blocking(move || zstd::decode_all(&compressed[..])).await??)
}

/// Stream zstd NDJSON at `path` (one or more frames) to `tx` as request
/// bodies of at most `limit` bytes, whole lines, decoding as it reads: no
/// object is held whole. Checks the file's sha256 when `sha256` is given.
/// `Ok(false)` when the receiver went away first.
pub async fn feed(
    store: &dyn ObjectStore,
    path: &str,
    size: Option<u64>,
    sha256: Option<&str>,
    limit: usize,
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
) -> anyhow::Result<bool> {
    use std::io::BufRead;
    let stream = store
        .get_stream(path)
        .await?
        .with_context(|| format!("`{path}` is missing"))?;
    let crate::source::Download {
        reader,
        digest,
        task,
        ..
    } = crate::source::from_store(stream, path, size);
    let sent = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let decoder = zstd::stream::read::Decoder::new(reader)?;
        let mut lines = std::io::BufReader::new(decoder);
        let (mut cur, mut line) = (Vec::new(), Vec::new());
        loop {
            line.clear();
            if lines.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if !line.ends_with(b"\n") {
                line.push(b'\n');
            }
            if line.len() > limit {
                bail!("a document is {} bytes, over the request limit", line.len());
            }
            if cur.len() + line.len() > limit
                && tx.blocking_send(std::mem::take(&mut cur).into()).is_err()
            {
                return Ok(false);
            }
            cur.extend_from_slice(&line);
        }
        let alive = cur.is_empty() || tx.blocking_send(cur.into()).is_ok();
        // Keep the read going until here.
        drop(task);
        Ok(alive)
    })
    .await?;
    let alive = sent?;
    if !alive {
        return Ok(false);
    }
    let sha = digest.await.context("the read stopped")??;
    if let Some(want) = sha256 {
        if !sha.eq_ignore_ascii_case(want) {
            bail!("`{path}` reads as sha256 {sha}, not its manifest's {want}");
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_a_fixed_share_of_pages() {
        let ids: Vec<String> = (0..20_000)
            .map(|i| format!("sn{:08}_1890-01-01_ed-1_seq-{}", i % 997, i))
            .collect();
        let one = ids.iter().filter(|d| sampled(d, 100)).count();
        // 1% of 20,000 is 200; a fair hash lands well within ±60.
        assert!((140..=260).contains(&one), "{one}");
        // The same pages every time, and a larger sample holds the smaller.
        assert!(ids
            .iter()
            .filter(|d| sampled(d, 100))
            .all(|d| sampled(d, 500)));
        assert!(ids.iter().all(|d| sampled(d, 10_000)));
        assert!(!ids.iter().any(|d| sampled(d, 0)));
    }

    #[test]
    fn the_rule_is_sha256_mod_10000() {
        // sha256("sn84026749_1918-10-01_ed-1_seq-1"), first 8 bytes, mod 10,000.
        let id = "sn84026749_1918-10-01_ed-1_seq-1";
        let h = Sha256::digest(id.as_bytes());
        let v = u64::from_be_bytes(h[..8].try_into().unwrap()) % 10_000;
        assert!(sampled(id, u32::try_from(v).unwrap() + 1));
        assert!(!sampled(id, u32::try_from(v).unwrap()));
    }

    #[test]
    fn parts_end_at_line_boundaries() {
        let buf = b"aaaa\nbb\ncccccc\nd\n";
        assert_eq!(cut_point(buf, 8), 8);
        assert_eq!(cut_point(buf, 10), 8);
        assert_eq!(cut_point(buf, 5), 5);
        // A line longer than the limit is a part of its own.
        assert_eq!(cut_point(b"aaaaaaaa\nb\n", 3), 9);
        assert_eq!(cut_point(b"aaaaaaaa", 3), 8);
    }

    #[test]
    fn the_share_is_a_cut_in_10000() {
        assert_eq!(cut_of(1.0).unwrap(), 100);
        assert_eq!(cut_of(0.5).unwrap(), 50);
        assert_eq!(cut_of(100.0).unwrap(), 10_000);
        for bad in [0.0, 0.001, 0.005, 100.004, 101.0, -1.0, f64::NAN] {
            assert!(cut_of(bad).is_err(), "{bad}");
        }
    }
}
