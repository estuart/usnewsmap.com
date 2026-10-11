//! `usnm-ingest ocr-damage` (12 §12.5 Step 6, #296): the damage metric
//! (`usnm_core::damage`) over every curated page of an index version, the
//! parity gate against the Python audit, and the parity check against
//! `ja-ocr/damage_v2.py`'s pages.
//!
//! **Full pass** (no `--sample-pct`, `--compare-csv` or `--parity`): each
//! of the version's batches (its `batches.json`) is read from its curated
//! parts and every page's LoC text is scored against its title's catalog
//! languages plus English. The tables are keyed by curation, not by index
//! version: `audit/damage/v<VERSION>/<batch>_ver<NN>.parquet` (one row per
//! page, [`schema`]) and a summary per title beside it (`.summary.json`),
//! written in that order. A batch version whose summary is already there is
//! skipped, so a later run pays only for new batches, and a run stopped by
//! `--max-runtime-secs` is continued by the next. Once every batch of the
//! version has its tables, `audit/damage/v<VERSION>/versions/<version>.json`
//! lists them; the seeder and `ocr-sync` read that list.
//!
//! **Gate** (`--sample-pct`, `--compare-csv`): the pages the Python audit's
//! sampler picks (`quality.py`'s `sampled`: BLAKE2b with an 8-byte digest of
//! the doc_id, read big-endian, mod 10,000, below round(pct × 100)) over
//! every curated row of the version, duplicate copies included, are scored
//! and compared with the audit's per-page CSV, joined on doc_id and batch.
//! The CSV rounds its scores to 4 decimals, so Rust's are formatted the same
//! way (`{:.4}`, trailing zeros trimmed) before they're compared. It writes
//! no tables, only the result under `audit/damage/v<VERSION>/gate/`, and
//! fails below [`GATE_AGREEMENT`].
//!
//! **Parity** (`--parity`): the pages of a file `jaocr.py damage-parity`
//! wrote, scored again here (`usnm_core::damage::parity`).
//!
//! Every mode logs counts and up to [`MAX_LISTED`] doc_ids, never text.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use arrow_array::builder::{
    Date32Builder, Float64Builder, Int16Builder, Int32Builder, StringBuilder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use blake2::digest::consts::U8;
use blake2::{Blake2b, Digest};
use bytes::Bytes;
use chrono::{NaiveDate, Utc};
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use usnm_core::damage::{self, parity, Decision};
use usnm_core::text::TextStatus;
use usnm_state::state::{RunBatch, RUN_BATCHES_FILE};
use usnm_store::ObjectStore;

use crate::curated::{read_part, CuratedRow};

/// The share of compared pages that must agree for the gate to pass.
pub const GATE_AGREEMENT: f64 = 0.9999;
/// The most doc_ids a report lists.
pub const MAX_LISTED: usize = 50;
const PROGRESS_EVERY: Duration = Duration::from_secs(60);
const NO_TEXT: &str = "no_text";

/// Where this metric version's tables live in the curated store.
pub fn root() -> String {
    format!("audit/damage/v{}", damage::VERSION)
}

/// A batch version's page table.
pub fn table_path(batch: &str, version: u16) -> String {
    format!("{}/{batch}_ver{version:02}.parquet", root())
}

/// A batch version's per-title summary, written after its table.
pub fn summary_path(batch: &str, version: u16) -> String {
    format!("{}/{batch}_ver{version:02}.summary.json", root())
}

/// The list of tables that make up an index version.
pub fn versions_path(index_version: &str) -> String {
    format!("{}/versions/{index_version}.json", root())
}

/// What to do.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The index version whose batches to score.
    pub index_version: String,
    /// Only these batches (`name` or `name_verNN`).
    pub batches: Vec<String>,
    /// Score only the pages the audit's sampler picks at this percentage.
    pub sample_pct: Option<f64>,
    /// The audit's per-page CSV (`.csv.gz`, in the curated store) to compare with.
    pub compare_csv: Option<String>,
    /// A parity file (`.jsonl.zst`, in the curated store) to check.
    pub parity: Option<String>,
    /// Stop starting batches after this.
    pub deadline: Option<Instant>,
    /// Batches scored at once.
    pub parallel: usize,
}

/// The audit's sampler (`quality.py`'s `sampled`): whether `doc_id` is in a
/// sample cut at `cut` of 10,000.
pub fn sampled(doc_id: &str, cut: u64) -> bool {
    let digest = Blake2b::<U8>::digest(doc_id.as_bytes());
    let h = u64::from_be_bytes(digest.as_slice().try_into().expect("8 bytes"));
    h % 10_000 < cut
}

/// `round(pct * 100)` as Python rounds (half to even).
pub fn sample_cut(pct: f64) -> anyhow::Result<u64> {
    if !(pct > 0.0 && pct <= 100.0) {
        bail!("--sample-pct must be above 0 and at most 100, not {pct}");
    }
    let cut = (pct * 100.0).round_ties_even();
    if cut < 1.0 {
        bail!("--sample-pct {pct} samples nothing: use at least 0.01");
    }
    Ok(cut as u64)
}

/// A score as the audit's CSV writes it: 4 decimals, trailing zeros and
/// point trimmed, empty for none.
pub fn csv_num(x: Option<f64>) -> String {
    match x {
        None => String::new(),
        Some(x) => {
            let s = format!("{x:.4}");
            s.trim_end_matches('0').trim_end_matches('.').to_owned()
        }
    }
}

/// One scored page.
#[derive(Debug, Clone, PartialEq)]
pub struct PageRow {
    pub doc_id: String,
    pub lccn: String,
    pub date: NaiveDate,
    pub edition: u16,
    pub seq: u16,
    /// The base batch name.
    pub batch: String,
    pub batch_version: u16,
    pub text_status: &'static str,
    /// The detected language; None for a page without text.
    pub lang: Option<String>,
    /// detected, mixed, und, or no_text.
    pub decision: &'static str,
    pub mixed_with: Option<String>,
    pub script: Option<&'static str>,
    pub damage_rate: Option<f64>,
    pub function_share: Option<f64>,
    pub garbage_share: Option<f64>,
    pub words: u32,
    pub head: u32,
    pub near: u32,
}

fn status_str(s: TextStatus) -> &'static str {
    match s {
        TextStatus::Ok => "ok",
        TextStatus::Short => "short",
        TextStatus::Empty => "empty",
    }
}

fn clamp(n: impl TryInto<u32>) -> u32 {
    n.try_into().unwrap_or(u32::MAX)
}

/// Score one curated row against its title's languages.
pub fn score_row(row: &CuratedRow, languages: &[&str]) -> PageRow {
    let k = &row.key;
    let mut out = PageRow {
        doc_id: k.doc_id(),
        lccn: k.lccn.clone(),
        date: k.date,
        edition: k.edition,
        seq: k.seq,
        batch: row.batch.clone(),
        batch_version: row.batch_version,
        text_status: status_str(row.status),
        lang: None,
        decision: NO_TEXT,
        mixed_with: None,
        script: None,
        damage_rate: None,
        function_share: None,
        garbage_share: None,
        words: 0,
        head: 0,
        near: 0,
    };
    let text = match (&row.status, &row.text) {
        (TextStatus::Ok, Some(t)) if !t.is_empty() => t,
        _ => return out,
    };
    let s = damage::score(text, languages);
    out.decision = s.decision.as_str();
    out.lang = Some(s.lang);
    out.mixed_with = s.mixed_with;
    out.script = Some(s.script);
    out.damage_rate = s.damage_rate;
    out.function_share = s.function_share;
    out.garbage_share = s.garbage_share;
    out.words = clamp(s.words);
    out.head = clamp(s.damage.map_or(0, |d| d.head));
    out.near = clamp(s.damage.map_or(0, |d| d.near));
    out
}

/// The page table's columns.
pub fn schema() -> SchemaRef {
    let s = |n: &str, null: bool| Field::new(n, DataType::Utf8, null);
    let f = |n: &str| Field::new(n, DataType::Float64, true);
    let i = |n: &str| Field::new(n, DataType::Int32, false);
    Arc::new(Schema::new(vec![
        s("doc_id", false),
        s("lccn", false),
        Field::new("date", DataType::Date32, false),
        Field::new("edition", DataType::Int16, false),
        Field::new("seq", DataType::Int16, false),
        s("batch", false),
        Field::new("batch_version", DataType::Int16, false),
        s("text_status", false),
        s("lang", true),
        s("decision", false),
        s("mixed_with", true),
        s("script", true),
        f("damage_rate"),
        f("function_share"),
        f("garbage_share"),
        i("words"),
        i("head"),
        i("near"),
    ]))
}

fn unix_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid");
    i32::try_from((d - epoch).num_days()).expect("dates fit in i32 days")
}

/// The page table of a batch version, as Parquet.
pub fn encode_table(rows: &[PageRow]) -> anyhow::Result<Vec<u8>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(6).expect("valid level"),
        ))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    for chunk in rows.chunks(8192) {
        let mut doc_id = StringBuilder::new();
        let mut lccn = StringBuilder::new();
        let mut date = Date32Builder::new();
        let mut edition = Int16Builder::new();
        let mut seq = Int16Builder::new();
        let mut batch = StringBuilder::new();
        let mut batch_version = Int16Builder::new();
        let mut status = StringBuilder::new();
        let mut lang = StringBuilder::new();
        let mut decision = StringBuilder::new();
        let mut mixed_with = StringBuilder::new();
        let mut script = StringBuilder::new();
        let mut damage_rate = Float64Builder::new();
        let mut function_share = Float64Builder::new();
        let mut garbage_share = Float64Builder::new();
        let mut words = Int32Builder::new();
        let mut head = Int32Builder::new();
        let mut near = Int32Builder::new();
        let small = |v: u16| i16::try_from(v).context("edition, seq or version above 32767");
        let int = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
        for r in chunk {
            doc_id.append_value(&r.doc_id);
            lccn.append_value(&r.lccn);
            date.append_value(unix_days(r.date));
            edition.append_value(small(r.edition)?);
            seq.append_value(small(r.seq)?);
            batch.append_value(&r.batch);
            batch_version.append_value(small(r.batch_version)?);
            status.append_value(r.text_status);
            lang.append_option(r.lang.as_deref());
            decision.append_value(r.decision);
            mixed_with.append_option(r.mixed_with.as_deref());
            script.append_option(r.script);
            damage_rate.append_option(r.damage_rate);
            function_share.append_option(r.function_share);
            garbage_share.append_option(r.garbage_share);
            words.append_value(int(r.words));
            head.append_value(int(r.head));
            near.append_value(int(r.near));
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(doc_id.finish()),
            Arc::new(lccn.finish()),
            Arc::new(date.finish()),
            Arc::new(edition.finish()),
            Arc::new(seq.finish()),
            Arc::new(batch.finish()),
            Arc::new(batch_version.finish()),
            Arc::new(status.finish()),
            Arc::new(lang.finish()),
            Arc::new(decision.finish()),
            Arc::new(mixed_with.finish()),
            Arc::new(script.finish()),
            Arc::new(damage_rate.finish()),
            Arc::new(function_share.finish()),
            Arc::new(garbage_share.finish()),
            Arc::new(words.finish()),
            Arc::new(head.finish()),
            Arc::new(near.finish()),
        ];
        w.write(&RecordBatch::try_new(schema(), cols)?)?;
    }
    Ok(w.into_inner()?)
}

/// One title's pages in a batch version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TitleSummary {
    pub lccn: String,
    pub pages: u64,
    /// Pages with text that were scored.
    pub text_pages: u64,
    /// Pages with a damage rate.
    pub damage_pages: u64,
    pub und: u64,
    pub mixed: u64,
    pub damage_rate_median: Option<f64>,
    /// Pages above `damage::DAMAGED`.
    pub damaged: u64,
    /// Pages above `damage::BADLY_DAMAGED`.
    pub badly_damaged: u64,
    /// Pages by detected language (mixed pages under their first).
    pub languages: BTreeMap<String, u64>,
}

/// A batch version's summary file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BatchSummary {
    pub metric_version: u32,
    pub batch: String,
    pub batch_version: u16,
    pub pages: u64,
    pub titles: Vec<TitleSummary>,
}

fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    Some(if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    })
}

/// Summarize a batch version's pages by title.
pub fn summarize(batch: &str, version: u16, rows: &[PageRow]) -> BatchSummary {
    let mut by: BTreeMap<&str, (TitleSummary, Vec<f64>)> = BTreeMap::new();
    for r in rows {
        let (t, rates) = by.entry(&r.lccn).or_insert_with(|| {
            (
                TitleSummary {
                    lccn: r.lccn.clone(),
                    ..TitleSummary::default()
                },
                Vec::new(),
            )
        });
        t.pages += 1;
        if r.decision == NO_TEXT {
            continue;
        }
        t.text_pages += 1;
        t.und += u64::from(r.decision == Decision::Und.as_str());
        t.mixed += u64::from(r.decision == Decision::Mixed.as_str());
        if let Some(lang) = &r.lang {
            *t.languages.entry(lang.clone()).or_default() += 1;
        }
        if let Some(d) = r.damage_rate {
            t.damage_pages += 1;
            t.damaged += u64::from(d > damage::DAMAGED);
            t.badly_damaged += u64::from(d > damage::BADLY_DAMAGED);
            rates.push(d);
        }
    }
    BatchSummary {
        metric_version: damage::VERSION,
        batch: batch.to_owned(),
        batch_version: version,
        pages: rows.len() as u64,
        titles: by
            .into_values()
            .map(|(mut t, rates)| {
                t.damage_rate_median = median(rates);
                t
            })
            .collect(),
    }
}

/// The catalog languages of each title: the version's titles snapshot, then
/// the working catalog for any it lacks (as `quality.py`'s `load_titles`).
pub async fn title_languages(
    reference: &dyn ObjectStore,
    index_version: &str,
) -> anyhow::Result<HashMap<String, Vec<String>>> {
    #[derive(Deserialize)]
    struct T {
        lccn: String,
        #[serde(default)]
        languages: Vec<String>,
    }
    let mut out = HashMap::new();
    // The version's snapshot is required, as in `quality.py`; the working
    // catalog only fills in titles it lacks.
    let snapshot = format!("{index_version}/titles.json");
    for (path, required) in [(snapshot, true), ("catalog/titles.json".to_owned(), false)] {
        let Some(body) = reference.get(&path).await? else {
            if required {
                bail!("{path} not found: the version's titles snapshot is incomplete");
            }
            continue;
        };
        let titles: Vec<T> =
            serde_json::from_slice(&body).with_context(|| format!("reading {path}"))?;
        for t in titles {
            out.entry(t.lccn).or_insert(t.languages);
        }
    }
    Ok(out)
}

/// The batches of an index version, filtered by `only` (base names or
/// `name_verNN`; empty: all).
pub async fn version_batches(
    reference: &dyn ObjectStore,
    index_version: &str,
    only: &[String],
) -> anyhow::Result<Vec<RunBatch>> {
    if !usnm_store::is_safe_segment(index_version) {
        bail!("unsafe index version {index_version:?}");
    }
    let path = format!("{index_version}/{RUN_BATCHES_FILE}");
    let body = reference
        .get(&path)
        .await?
        .with_context(|| format!("{path} not found: is {index_version} a published version?"))?;
    let all: Vec<RunBatch> =
        serde_json::from_slice(&body).with_context(|| format!("reading {path}"))?;
    if only.is_empty() {
        return Ok(all);
    }
    let wanted: HashSet<&str> = only.iter().map(String::as_str).collect();
    let out: Vec<RunBatch> = all
        .into_iter()
        .filter(|b| {
            wanted.contains(b.batch.as_str())
                || wanted.contains(format!("{}_ver{:02}", b.batch, b.curated.version).as_str())
        })
        .collect();
    if out.len() != only.len() {
        bail!(
            "--batches names {} batches, {index_version} has {} of them",
            only.len(),
            out.len()
        );
    }
    Ok(out)
}

/// Score a batch version's pages: every page, or those `cut` samples.
async fn score_batch(
    curated: &dyn ObjectStore,
    b: &RunBatch,
    titles: &Arc<HashMap<String, Vec<String>>>,
    cut: Option<u64>,
) -> anyhow::Result<Vec<PageRow>> {
    let mut rows = Vec::new();
    for part in &b.curated.parts {
        let bytes = curated
            .get(part)
            .await?
            .with_context(|| format!("curated part {part} not found"))?;
        let titles = Arc::clone(titles);
        let part = part.clone();
        let scored = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<PageRow>> {
            let mut out = Vec::new();
            read_part(Bytes::from(bytes), true, |row| {
                if cut.is_some_and(|c| !sampled(&row.key.doc_id(), c)) {
                    return Ok(());
                }
                let langs: Vec<&str> = titles
                    .get(&row.key.lccn)
                    .map(|l| l.iter().map(String::as_str).collect())
                    .unwrap_or_default();
                out.push(score_row(&row, &langs));
                Ok(())
            })
            .with_context(|| format!("reading {part}"))?;
            Ok(out)
        })
        .await??;
        rows.extend(scored);
    }
    Ok(rows)
}

/// What a full pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct PassReport {
    pub batches: usize,
    pub scored: usize,
    pub skipped: usize,
    /// Batches left for the next run (the deadline passed).
    pub left: usize,
    pub pages: u64,
    /// Whether the version's list was written by this run.
    pub version_list_written: bool,
}

/// The full pass.
pub async fn full_pass(
    curated: Arc<dyn ObjectStore>,
    reference: &dyn ObjectStore,
    opts: &Options,
) -> anyhow::Result<PassReport> {
    let batches = version_batches(reference, &opts.index_version, &opts.batches).await?;
    let titles = Arc::new(title_languages(reference, &opts.index_version).await?);
    let started = Instant::now();
    let total = batches.len();
    tracing::info!(
        index_version = %opts.index_version,
        batches = total,
        metric_version = damage::VERSION,
        "ocr damage starting"
    );
    let report = Arc::new(tokio::sync::Mutex::new(PassReport {
        batches: total,
        ..PassReport::default()
    }));
    let last_progress = Arc::new(tokio::sync::Mutex::new(Instant::now()));
    futures::stream::iter(batches.iter())
        .map(|b| {
            let curated = Arc::clone(&curated);
            let titles = Arc::clone(&titles);
            let report = Arc::clone(&report);
            let last_progress = Arc::clone(&last_progress);
            async move {
                let (name, v) = (b.batch.as_str(), b.curated.version);
                if curated.exists(&summary_path(name, v)).await? {
                    report.lock().await.skipped += 1;
                    return Ok::<(), anyhow::Error>(());
                }
                if opts.deadline.is_some_and(|d| Instant::now() >= d) {
                    report.lock().await.left += 1;
                    return Ok(());
                }
                let rows = score_batch(curated.as_ref(), b, &titles, None).await?;
                let pages = rows.len() as u64;
                let batch = name.to_owned();
                let (table, summary) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                    Ok((
                        encode_table(&rows)?,
                        serde_json::to_vec(&summarize(&batch, v, &rows))?,
                    ))
                })
                .await??;
                // The summary last: it marks the batch version done.
                if !curated
                    .put_new(
                        &table_path(name, v),
                        table,
                        "application/vnd.apache.parquet",
                    )
                    .await?
                {
                    // An earlier run wrote it and stopped before the summary.
                    // Tables are create-only: to rescore, delete both files.
                    tracing::warn!(
                        batch = name,
                        version = v,
                        "ocr damage table kept from an earlier run"
                    );
                }
                curated
                    .put_new(&summary_path(name, v), summary, "application/json")
                    .await?;
                let mut r = report.lock().await;
                r.scored += 1;
                r.pages += pages;
                let mut last = last_progress.lock().await;
                if last.elapsed() >= PROGRESS_EVERY {
                    *last = Instant::now();
                    let secs = started.elapsed().as_secs_f64().max(1.0);
                    tracing::info!(
                        batches_done = r.scored + r.skipped,
                        batches = total,
                        pages = r.pages,
                        pages_per_sec = (r.pages as f64 / secs).round(),
                        elapsed_s = secs.round(),
                        "ocr damage progress"
                    );
                }
                Ok(())
            }
        })
        .buffer_unordered(opts.parallel.max(1))
        .try_collect::<Vec<()>>()
        .await?;
    let mut report = Arc::try_unwrap(report)
        .expect("every task is done")
        .into_inner();
    if report.left == 0 && opts.batches.is_empty() {
        let list = VersionList {
            index_version: opts.index_version.clone(),
            metric_version: damage::VERSION,
            batches: batches
                .iter()
                .map(|b| VersionBatch {
                    batch: b.batch.clone(),
                    version: b.curated.version,
                    table: table_path(&b.batch, b.curated.version),
                    summary: summary_path(&b.batch, b.curated.version),
                })
                .collect(),
        };
        report.version_list_written = curated
            .put_new(
                &versions_path(&opts.index_version),
                serde_json::to_vec_pretty(&list)?,
                "application/json",
            )
            .await?;
    }
    tracing::info!(
        index_version = %opts.index_version,
        batches = report.batches,
        scored = report.scored,
        skipped = report.skipped,
        left = report.left,
        pages = report.pages,
        version_list_written = report.version_list_written,
        elapsed_s = started.elapsed().as_secs(),
        "ocr damage finished"
    );
    Ok(report)
}

/// `versions/<index_version>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionList {
    pub index_version: String,
    pub metric_version: u32,
    pub batches: Vec<VersionBatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionBatch {
    pub batch: String,
    pub version: u16,
    pub table: String,
    pub summary: String,
}

/// One page of the audit's per-page CSV, as far as the gate compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvPage {
    pub title_languages: String,
    pub language: String,
    pub decision: String,
    pub function_share: String,
    pub damage_rate: String,
    pub garbage_share: String,
}

/// Split one CSV line (RFC 4180, as Python's csv module writes it).
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (false, '"') if cur.is_empty() => quoted = true,
            (false, ',') => out.push(std::mem::take(&mut cur)),
            (_, c) => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The audit's per-page CSV (gzipped), by (doc_id, batch with `_verNN`).
pub fn read_audit_csv(gz: &[u8]) -> anyhow::Result<HashMap<(String, String), CsvPage>> {
    let mut text = String::new();
    flate2::read::MultiGzDecoder::new(gz)
        .read_to_string(&mut text)
        .context("the audit CSV isn't gzipped UTF-8")?;
    let mut lines = text.lines();
    let header = csv_fields(lines.next().context("the audit CSV is empty")?);
    let col = |name: &str| {
        header
            .iter()
            .position(|h| h == name)
            .with_context(|| format!("the audit CSV has no `{name}` column"))
    };
    let (doc_id, batch, langs, language, decision, fs, dr, gs) = (
        col("doc_id")?,
        col("batch")?,
        col("title_languages")?,
        col("language")?,
        col("decision")?,
        col("function_share")?,
        col("damage_rate")?,
        col("garbage_share")?,
    );
    let mut out = HashMap::new();
    for (i, line) in lines.enumerate() {
        if line.is_empty() {
            continue;
        }
        let f = csv_fields(line);
        if f.len() != header.len() {
            bail!(
                "audit CSV line {}: {} fields, not {}",
                i + 2,
                f.len(),
                header.len()
            );
        }
        out.insert(
            (f[doc_id].clone(), f[batch].clone()),
            CsvPage {
                title_languages: f[langs].clone(),
                language: f[language].clone(),
                decision: f[decision].clone(),
                function_share: f[fs].clone(),
                damage_rate: f[dr].clone(),
                garbage_share: f[gs].clone(),
            },
        );
    }
    Ok(out)
}

/// How the gate went.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct GateReport {
    pub index_version: String,
    pub sample_pct: Option<f64>,
    /// Pages the sampler picked from the version's curated rows.
    pub sampled: u64,
    /// Rows in the audit's CSV.
    pub csv_rows: u64,
    /// CSV rows with a scored page to compare.
    pub compared: u64,
    /// CSV rows the sample has no page for.
    pub missing: u64,
    /// Compared pages with the same language and decision.
    pub same_language: u64,
    /// Compared pages with the same language, decision and three scores.
    pub agree: u64,
    pub damage_rate_differs: u64,
    pub function_share_differs: u64,
    pub garbage_share_differs: u64,
    /// Pages whose title languages differ from the CSV's (the catalog changed).
    pub title_languages_differ: u64,
    /// agree / csv_rows.
    pub agreement: Option<f64>,
    pub passed: bool,
    /// Up to `MAX_LISTED` doc_ids that differ or are missing.
    pub differing: Vec<String>,
}

/// Compare scored pages with the audit's CSV.
pub fn compare_with_csv(
    rows: &[PageRow],
    csv: &HashMap<(String, String), CsvPage>,
    titles: &HashMap<String, Vec<String>>,
) -> GateReport {
    let mut by_key: HashMap<(&str, String), &PageRow> = HashMap::new();
    for r in rows {
        by_key.insert(
            (
                r.doc_id.as_str(),
                format!("{}_ver{:02}", r.batch, r.batch_version),
            ),
            r,
        );
    }
    let mut g = GateReport {
        csv_rows: csv.len() as u64,
        ..GateReport::default()
    };
    let mut keys: Vec<&(String, String)> = csv.keys().collect();
    keys.sort();
    for key in keys {
        let want = &csv[key];
        let Some(r) = by_key.get(&(key.0.as_str(), key.1.clone())) else {
            g.missing += 1;
            if g.differing.len() < MAX_LISTED {
                g.differing.push(key.0.clone());
            }
            continue;
        };
        g.compared += 1;
        let langs = titles.get(&r.lccn).map(|l| l.join(" ")).unwrap_or_default();
        g.title_languages_differ += u64::from(langs != want.title_languages);
        let language = if r.decision == NO_TEXT {
            NO_TEXT
        } else {
            r.lang.as_deref().unwrap_or("")
        };
        let same_language = language == want.language && r.decision == want.decision;
        let dr = csv_num(r.damage_rate) == want.damage_rate;
        let fs = csv_num(r.function_share) == want.function_share;
        let gs = csv_num(r.garbage_share) == want.garbage_share;
        g.same_language += u64::from(same_language);
        g.damage_rate_differs += u64::from(!dr);
        g.function_share_differs += u64::from(!fs);
        g.garbage_share_differs += u64::from(!gs);
        if same_language && dr && fs && gs {
            g.agree += 1;
        } else if g.differing.len() < MAX_LISTED {
            g.differing.push(key.0.clone());
        }
    }
    g.agreement = (g.csv_rows > 0).then(|| g.agree as f64 / g.csv_rows as f64);
    g.passed = g.agreement.is_some_and(|a| a >= GATE_AGREEMENT);
    g
}

/// The gate: score the sample, compare it with the audit's CSV, write the result.
pub async fn gate(
    curated: Arc<dyn ObjectStore>,
    reference: &dyn ObjectStore,
    opts: &Options,
) -> anyhow::Result<GateReport> {
    let pct = opts.sample_pct.context("the gate needs --sample-pct")?;
    let cut = sample_cut(pct)?;
    let batches = version_batches(reference, &opts.index_version, &opts.batches).await?;
    let titles = Arc::new(title_languages(reference, &opts.index_version).await?);
    let path = opts
        .compare_csv
        .as_deref()
        .context("the gate needs --compare-csv")?;
    let gz = curated
        .get(path)
        .await?
        .with_context(|| format!("{path} not found"))?;
    let csv = tokio::task::spawn_blocking(move || read_audit_csv(&gz)).await??;
    let started = Instant::now();
    tracing::info!(index_version = %opts.index_version, batches = batches.len(), sample_pct = pct, "ocr damage gate starting");
    let rows: Vec<PageRow> = futures::stream::iter(batches.iter())
        .map(|b| {
            let curated = Arc::clone(&curated);
            let titles = Arc::clone(&titles);
            async move { score_batch(curated.as_ref(), b, &titles, Some(cut)).await }
        })
        .buffer_unordered(opts.parallel.max(1))
        .try_concat()
        .await?;
    let mut report = compare_with_csv(&rows, &csv, &titles);
    report.index_version = opts.index_version.clone();
    report.sample_pct = Some(pct);
    report.sampled = rows.len() as u64;
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    let path = format!(
        "{}/gate/{}-{pct}pct-{stamp}.json",
        root(),
        opts.index_version
    );
    curated
        .put_new(
            &path,
            serde_json::to_vec_pretty(&report)?,
            "application/json",
        )
        .await?;
    tracing::info!(
        index_version = %report.index_version,
        sampled = report.sampled,
        csv_rows = report.csv_rows,
        compared = report.compared,
        missing = report.missing,
        same_language = report.same_language,
        agree = report.agree,
        damage_rate_differs = report.damage_rate_differs,
        function_share_differs = report.function_share_differs,
        garbage_share_differs = report.garbage_share_differs,
        title_languages_differ = report.title_languages_differ,
        agreement = ?report.agreement,
        passed = report.passed,
        differing = ?report.differing,
        path = %path,
        elapsed_s = started.elapsed().as_secs(),
        "ocr damage gate"
    );
    Ok(report)
}

/// How a parity file checked.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ParityReport {
    pub pages: usize,
    pub agree: usize,
    pub differ: usize,
    /// Up to `MAX_LISTED` doc_ids that differ.
    pub differing: Vec<String>,
    /// Whether the file was scored by the damage_v2.py this build was checked against.
    pub damage_v2_sha256: String,
}

/// Check a parity file (`.jsonl.zst`) from the curated store.
pub async fn parity_check(curated: &dyn ObjectStore, path: &str) -> anyhow::Result<ParityReport> {
    let body = curated
        .get(path)
        .await?
        .with_context(|| format!("{path} not found"))?;
    let report = tokio::task::spawn_blocking(move || -> anyhow::Result<ParityReport> {
        let text = String::from_utf8(zstd::decode_all(&body[..]).context("not zstd")?)
            .context("not UTF-8")?;
        let (header, records) = parity::parse(&text)?;
        let r = parity::check(&records);
        Ok(ParityReport {
            pages: r.pages,
            agree: r.agree,
            differ: r.differ.len(),
            differing: r
                .differ
                .iter()
                .take(MAX_LISTED)
                .map(|(d, _)| d.clone())
                .collect(),
            damage_v2_sha256: header.damage_v2_sha256,
        })
    })
    .await??;
    tracing::info!(
        path,
        pages = report.pages,
        agree = report.agree,
        differ = report.differ,
        differing = ?report.differing,
        damage_v2_sha256 = %report.damage_v2_sha256,
        "ocr damage parity"
    );
    Ok(report)
}

/// Run the command: parity, gate or full pass. Fails when parity or the gate does.
pub async fn run(
    curated: Arc<dyn ObjectStore>,
    reference: &dyn ObjectStore,
    opts: &Options,
) -> anyhow::Result<()> {
    if let Some(path) = &opts.parity {
        let r = parity_check(curated.as_ref(), path).await?;
        if r.differ > 0 {
            bail!(
                "{} of {} parity pages differ from damage_v2.py",
                r.differ,
                r.pages
            );
        }
        if opts.sample_pct.is_none() {
            return Ok(());
        }
    }
    if opts.sample_pct.is_some() {
        let g = gate(curated, reference, opts).await?;
        if !g.passed {
            bail!(
                "the gate failed: {} of {} audit pages agree ({:?}, at least {GATE_AGREEMENT} needed)",
                g.agree,
                g.csv_rows,
                g.agreement
            );
        }
        return Ok(());
    }
    full_pass(curated, reference, opts).await.map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_matches_quality_py() {
        // quality.sampled, in Python: blake2b(doc_id, digest_size=8), big-endian, mod 10,000.
        let h = |d: &str| {
            let digest = Blake2b::<U8>::digest(d.as_bytes());
            u64::from_be_bytes(digest.as_slice().try_into().unwrap()) % 10_000
        };
        assert_eq!(h("sn84026749_1902-12-01_ed-1_seq-1"), PY_H);
        let n = 20_000;
        let picked = (0..n)
            .filter(|i| sampled(&format!("doc-{i}"), 1000))
            .count();
        assert!((1800..2200).contains(&picked), "{picked}");
        assert_eq!(sample_cut(10.0).unwrap(), 1000);
        assert_eq!(
            sample_cut(0.005).unwrap_err().to_string(),
            "--sample-pct 0.005 samples nothing: use at least 0.01"
        );
        assert!(sample_cut(0.0).is_err() && sample_cut(101.0).is_err());
    }

    // python3 -c "import hashlib; print(int.from_bytes(hashlib.blake2b(b'sn84026749_1902-12-01_ed-1_seq-1', digest_size=8).digest(), 'big') % 10000)"
    const PY_H: u64 = 6167;

    #[test]
    fn csv_numbers_as_python_writes_them() {
        assert_eq!(csv_num(None), "");
        assert_eq!(csv_num(Some(0.0)), "0");
        assert_eq!(csv_num(Some(1.0)), "1");
        assert_eq!(csv_num(Some(0.1)), "0.1");
        assert_eq!(csv_num(Some(0.03125)), "0.0312");
        assert_eq!(csv_num(Some(3.0 / 7.0)), "0.4286");
        assert_eq!(
            csv_fields("a,\"b,c\",\"d\"\"e\",,f"),
            ["a", "b,c", "d\"e", "", "f"]
        );
    }

    use crate::curated::PartWriter;
    use chrono::DateTime;
    use usnm_core::ids::PageKey;
    use usnm_state::state::Curated;

    const ENGLISH: &str = "The council of the city met on Tuesday evening at the court house, and after a long \
        debate it was voted that the new school should be built on the lot which the board had bought last year. ";
    const GERMAN: &str = "Der Stadtrath hat am Dienstag beschlossen, dass die neue Schule auf dem Grundstück \
        gebaut werden soll, welches die Behörde im vorigen Jahre gekauft hat. Es ist noch nicht bekannt, wann. ";

    struct Fixture {
        _dir: tempfile::TempDir,
        curated: Arc<dyn ObjectStore>,
        reference: Arc<dyn ObjectStore>,
    }

    /// Two batches of an index version `v1`: an English title's pages (one
    /// damaged, one empty) and a German title's.
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let curated = usnm_store::open(dir.path().join("curated").to_str().unwrap()).unwrap();
        let reference = usnm_store::open(dir.path().join("reference").to_str().unwrap()).unwrap();
        let at = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let day = NaiveDate::from_ymd_opt(1890, 3, 1).unwrap();
        let mut batches = Vec::new();
        for (batch, lccn, pages) in [
            (
                "batch_a",
                "sn84026749",
                vec![
                    ENGLISH.repeat(4),
                    ENGLISH.replace("the ", "tbe ").repeat(4),
                    String::new(),
                ],
            ),
            ("batch_b", "sn86071815", vec![GERMAN.repeat(4)]),
        ] {
            let mut w = PartWriter::new();
            for (i, text) in pages.iter().enumerate() {
                let key = PageKey::new(lccn, day, 1, u16::try_from(i + 1).unwrap()).unwrap();
                assert!(w
                    .push(CuratedRow::from_ocr(key, text, batch, 2, "loc", at))
                    .unwrap()
                    .is_none());
            }
            let part = format!("pages/{batch}_ver02/part-0.parquet");
            curated
                .put_new(&part, w.finish_part().unwrap().unwrap(), "x")
                .await
                .unwrap();
            batches.push(RunBatch {
                batch: batch.into(),
                curated: Curated {
                    version: 2,
                    parts: vec![part],
                    counts: String::new(),
                    pages: pages.len() as u64,
                    ok_pages: 0,
                    lccns: vec![lccn.into()],
                    source_sha256: String::new(),
                    first: String::new(),
                    last: String::new(),
                },
            });
        }
        let put = |path: &str, v: serde_json::Value| {
            let reference = Arc::clone(&reference);
            let path = path.to_owned();
            async move {
                reference
                    .put_new(&path, serde_json::to_vec(&v).unwrap(), "x")
                    .await
                    .unwrap()
            }
        };
        put("v1/batches.json", serde_json::to_value(&batches).unwrap()).await;
        put(
            "v1/titles.json",
            serde_json::json!([{"lccn": "sn84026749", "languages": ["eng"]}]),
        )
        .await;
        put(
            "catalog/titles.json",
            serde_json::json!([{"lccn": "sn86071815", "languages": ["ger", "eng"]}]),
        )
        .await;
        Fixture {
            _dir: dir,
            curated,
            reference,
        }
    }

    fn opts() -> Options {
        Options {
            index_version: "v1".into(),
            parallel: 2,
            ..Options::default()
        }
    }

    async fn table(curated: &dyn ObjectStore, path: &str) -> Vec<RecordBatch> {
        let bytes = curated.get(path).await.unwrap().unwrap();
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
            .unwrap()
            .build()
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[tokio::test]
    async fn full_pass_writes_tables_once() {
        let f = fixture().await;
        let r = full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &opts())
            .await
            .unwrap();
        assert_eq!(
            (r.batches, r.scored, r.skipped, r.left, r.pages),
            (2, 2, 0, 0, 4)
        );
        assert!(r.version_list_written);
        let a = table(f.curated.as_ref(), &table_path("batch_a", 2)).await;
        assert_eq!(a[0].schema(), schema());
        assert_eq!(a[0].num_rows(), 3);
        let s: BatchSummary = serde_json::from_slice(
            &f.curated
                .get(&summary_path("batch_a", 2))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        let t = &s.titles[0];
        assert_eq!(
            (t.pages, t.text_pages, t.damage_pages, t.badly_damaged),
            (3, 2, 2, 1)
        );
        assert_eq!(t.languages.get("eng"), Some(&2));
        // The German title's languages come from the working catalog.
        let b: BatchSummary = serde_json::from_slice(
            &f.curated
                .get(&summary_path("batch_b", 2))
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(b.titles[0].languages.get("ger"), Some(&1));
        let list: VersionList =
            serde_json::from_slice(&f.curated.get(&versions_path("v1")).await.unwrap().unwrap())
                .unwrap();
        assert_eq!(list.batches.len(), 2);
        // A second run on the same version writes nothing.
        let again = full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &opts())
            .await
            .unwrap();
        assert_eq!(
            (again.scored, again.skipped, again.version_list_written),
            (0, 2, false)
        );
    }

    #[tokio::test]
    async fn a_deadline_leaves_batches_for_the_next_run() {
        let f = fixture().await;
        let o = Options {
            deadline: Some(Instant::now()),
            ..opts()
        };
        let r = full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &o)
            .await
            .unwrap();
        assert_eq!((r.scored, r.left, r.version_list_written), (0, 2, false));
        let only = Options {
            batches: vec!["batch_b_ver02".into()],
            ..opts()
        };
        let r = full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &only)
            .await
            .unwrap();
        assert_eq!((r.batches, r.scored, r.version_list_written), (1, 1, false));
        let r = full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &opts())
            .await
            .unwrap();
        assert_eq!((r.scored, r.skipped, r.version_list_written), (1, 1, true));
        assert!(
            title_languages(f.reference.as_ref(), "v2").await.is_err(),
            "no titles snapshot"
        );
        let bad = Options {
            batches: vec!["batch_c".into()],
            ..opts()
        };
        assert!(
            full_pass(Arc::clone(&f.curated), f.reference.as_ref(), &bad)
                .await
                .is_err()
        );
    }

    fn csv_gz(rows: &[[&str; 8]]) -> Vec<u8> {
        use std::io::Write;
        let mut text = String::from(
            "doc_id,lccn,date,batch,title_language,title_languages,status,language,decision,mixed_with,share,\
             runner_up,runner_up_share,script,function_share,damage_rate,garbage_share,words\n",
        );
        for [doc_id, batch, langs, language, decision, fs, dr, gs] in rows {
            text += &format!("{doc_id},x,1890-03-01,{batch},eng,{langs},ok,{language},{decision},,,,,latin,{fs},{dr},{gs},1\n");
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(text.as_bytes()).unwrap();
        gz.finish().unwrap()
    }

    #[tokio::test]
    async fn gate_compares_the_sample_with_the_audit_csv() {
        let f = fixture().await;
        // Without the audit's CSV there is nothing to pass.
        let o = Options {
            sample_pct: Some(100.0),
            ..opts()
        };
        assert!(gate(Arc::clone(&f.curated), f.reference.as_ref(), &o)
            .await
            .is_err());
        // The CSV as the audit would write it for the Rust scores.
        let titles = title_languages(f.reference.as_ref(), "v1").await.unwrap();
        let rows = score_batch(
            f.curated.as_ref(),
            &version_batches(f.reference.as_ref(), "v1", &[])
                .await
                .unwrap()[0],
            &Arc::new(titles.clone()),
            None,
        )
        .await
        .unwrap();
        let damaged = &rows[1];
        let (fs, dr, gs) = (
            csv_num(damaged.function_share),
            csv_num(damaged.damage_rate),
            csv_num(damaged.garbage_share),
        );
        let good = csv_num(rows[0].function_share);
        let csv = csv_gz(&[
            [
                &rows[0].doc_id,
                "batch_a_ver02",
                "eng",
                "eng",
                "detected",
                &good,
                "0",
                &csv_num(rows[0].garbage_share),
            ],
            [
                &damaged.doc_id,
                "batch_a_ver02",
                "eng",
                "eng",
                "detected",
                &fs,
                &dr,
                &gs,
            ],
            [
                &rows[2].doc_id,
                "batch_a_ver02",
                "eng",
                "no_text",
                "no_text",
                "",
                "",
                "",
            ],
        ]);
        f.curated
            .put_new("audit/pages.csv.gz", csv, "x")
            .await
            .unwrap();
        let o = Options {
            sample_pct: Some(100.0),
            compare_csv: Some("audit/pages.csv.gz".into()),
            ..opts()
        };
        let report = gate(Arc::clone(&f.curated), f.reference.as_ref(), &o)
            .await
            .unwrap();
        assert_eq!(
            (
                report.csv_rows,
                report.compared,
                report.agree,
                report.missing
            ),
            (3, 3, 3, 0)
        );
        assert!(report.passed, "{report:?}");
        // One rate off in the 4th decimal: that page differs, and 2 of 3 fails the gate.
        let off = format!("{:.4}", damaged.damage_rate.unwrap() + 0.0001);
        let csv = csv_gz(&[
            [
                &rows[0].doc_id,
                "batch_a_ver02",
                "eng",
                "eng",
                "detected",
                &good,
                "0",
                &csv_num(rows[0].garbage_share),
            ],
            [
                &damaged.doc_id,
                "batch_a_ver02",
                "eng",
                "eng",
                "detected",
                &fs,
                &off,
                &gs,
            ],
            [
                &rows[2].doc_id,
                "batch_a_ver02",
                "eng",
                "no_text",
                "no_text",
                "",
                "",
                "",
            ],
        ]);
        f.curated
            .put_new("audit/off.csv.gz", csv, "x")
            .await
            .unwrap();
        let o = Options {
            compare_csv: Some("audit/off.csv.gz".into()),
            ..o
        };
        let report = gate(Arc::clone(&f.curated), f.reference.as_ref(), &o)
            .await
            .unwrap();
        assert_eq!(
            (report.agree, report.damage_rate_differs, report.passed),
            (2, 1, false)
        );
        assert_eq!(report.differing, [damaged.doc_id.as_str()]);
        assert!(run(Arc::clone(&f.curated), f.reference.as_ref(), &o)
            .await
            .is_err());
        // The gate wrote its results and no tables.
        assert!(!f.curated.exists(&table_path("batch_a", 2)).await.unwrap());
    }

    #[tokio::test]
    async fn parity_file_from_the_curated_store() {
        let f = fixture().await;
        let fixture = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../usnm-core/tests/data/damage-parity.jsonl.zst"
        ))
        .unwrap();
        f.curated
            .put_new("audit/damage-parity/x.jsonl.zst", fixture, "x")
            .await
            .unwrap();
        let r = parity_check(f.curated.as_ref(), "audit/damage-parity/x.jsonl.zst")
            .await
            .unwrap();
        assert_eq!((r.differ, r.agree), (0, r.pages));
        assert!(r.pages >= 60);
    }
}
