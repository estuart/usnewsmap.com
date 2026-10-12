//! Curated Congress pages (#229 design §7): one Parquet row per page under
//! `congress/{series}/{unit}/{attempt}/part-{nnnn}.parquet` in the curated
//! store, and a `counts.json` beside them, written last, that marks the
//! attempt finished and holds the pages per day for the baselines. An
//! attempt that read its unit in full and found nothing to keep writes
//! `empty.json` instead ([`Empty`]).
//!
//! A unit is what a source is fetched by: a UNT item for the Globe, a
//! Serial Set volume, an Annals or Register volume. Attempts are numbered
//! from 1 and never rewritten; the release takes each unit's newest
//! finished attempt. The rows keep everything a fetch learned, the state
//! data for later maps included (`states`, `places`, `speakers`, JSON
//! columns), so a later index can use more of it without fetching again.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Context;
use arrow_array::builder::{
    BooleanBuilder, Date32Builder, FixedSizeBinaryBuilder, Int32Builder, LargeStringBuilder,
    StringBuilder, TimestampMicrosecondBuilder, UInt32Builder,
};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, FixedSizeBinaryArray, Int32Array, LargeStringArray,
    RecordBatch, StringArray, TimestampMicrosecondArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bytes::Bytes;
use chrono::{DateTime, NaiveDate, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usnm_store::ObjectStore;

use super::parse::Speaker;
use super::{CongressPage, StateRef};
use crate::source::hex;

/// Where curated Congress pages live in the curated store.
pub const PREFIX: &str = "congress";

/// The file that marks an attempt finished.
pub const COUNTS: &str = "counts.json";

/// The version of the curated rows and `counts.json`.
pub const SCHEMA_VERSION: u32 = 1;

/// One curated page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CuratedPage {
    pub doc_id: String,
    /// Who served the text: `unt`, `govinfo`, `lcg` (Congress.gov scans).
    pub source: String,
    /// One of [`super::SERIES`].
    pub series: String,
    /// The unit it was fetched in (`metadc29274`).
    pub unit_id: String,
    pub unit_ordinal: u32,
    pub package_id: Option<String>,
    /// The page's place in its item or PDF, from 1.
    pub seq: u32,
    pub printed_page: Option<String>,
    pub appendix: bool,
    pub date: NaiveDate,
    /// `day`, or `month`/`year` for a Serial Set document dated no closer.
    pub date_precision: String,
    /// `heading`, `carried`, `appendix_heading` or `session` (design §5).
    pub date_from: String,
    pub congress: Option<u16>,
    pub session: Option<String>,
    pub chamber: Option<String>,
    pub doc_class: Option<String>,
    pub doc_number: Option<String>,
    pub citation: Option<String>,
    pub serial: Option<String>,
    pub title: Option<String>,
    /// `ok`, `short` or `empty` (04 §4.5); `text` is only kept when `ok`.
    pub text_status: String,
    pub text: Option<String>,
    pub text_chars: i32,
    pub text_sha256: [u8; 32],
    pub ocr_source: String,
    pub ocr_engine: Option<String>,
    pub link: Option<String>,
    pub states: Vec<StateRef>,
    /// Places as the source gives them (MODS `geographic`); none for UNT.
    pub places: Vec<String>,
    pub speakers: Vec<Speaker>,
    pub fetched_at: DateTime<Utc>,
}

impl CuratedPage {
    /// The page as the release indexes it.
    pub fn to_page(&self) -> CongressPage {
        let mut states: Vec<StateRef> = Vec::new();
        for s in &self.states {
            if !states.iter().any(|o| o.state == s.state && o.via == s.via) {
                states.push(StateRef {
                    state: s.state.clone(),
                    via: s.via.clone(),
                    value: None,
                });
            }
        }
        CongressPage {
            doc_id: self.doc_id.clone(),
            series: self.series.clone(),
            unit_ordinal: self.unit_ordinal,
            seq: self.seq,
            date: self.date,
            congress: self.congress,
            session: self.session.clone(),
            chamber: self.chamber.clone(),
            title: self.title.clone(),
            citation: self.citation.clone(),
            printed_page: self.printed_page.clone(),
            package_id: self.package_id.clone(),
            link: self.link.clone(),
            text: self.text.clone().unwrap_or_default(),
            ocr_source: Some(self.ocr_source.clone()),
            ocr_engine: self.ocr_engine.clone(),
            states,
            speakers: self
                .speakers
                .iter()
                .filter_map(|s| s.bioguide.clone())
                .collect(),
        }
    }

    /// Whether the page is indexed: it has text (short and empty pages
    /// count in the baselines only, as for newspapers).
    pub fn indexable(&self) -> bool {
        self.text_status == "ok" && self.text.is_some()
    }
}

fn schema() -> SchemaRef {
    let s = |n: &str| Field::new(n, DataType::Utf8, false);
    let o = |n: &str| Field::new(n, DataType::Utf8, true);
    Arc::new(Schema::new(vec![
        s("doc_id"),
        s("source"),
        s("series"),
        s("unit_id"),
        Field::new("unit_ordinal", DataType::UInt32, false),
        o("package_id"),
        Field::new("seq", DataType::UInt32, false),
        o("printed_page"),
        Field::new("appendix", DataType::Boolean, false),
        Field::new("date", DataType::Date32, false),
        s("date_precision"),
        s("date_from"),
        Field::new("congress", DataType::UInt32, true),
        o("session"),
        o("chamber"),
        o("doc_class"),
        o("doc_number"),
        o("citation"),
        o("serial"),
        o("title"),
        s("text_status"),
        Field::new("text", DataType::LargeUtf8, true),
        Field::new("text_chars", DataType::Int32, false),
        Field::new("text_sha256", DataType::FixedSizeBinary(32), false),
        s("ocr_source"),
        o("ocr_engine"),
        o("link"),
        // JSON arrays (#229 design §6).
        s("states"),
        s("places"),
        s("speakers"),
        Field::new(
            "fetched_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
    ]))
}

fn unix_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid");
    i32::try_from((d - epoch).num_days()).expect("dates fit in i32 days")
}

fn to_batch(rows: &[CuratedPage]) -> anyhow::Result<RecordBatch> {
    let n = rows.len();
    let str_col = |f: &dyn Fn(&CuratedPage) -> &str| {
        let mut b = StringBuilder::new();
        for r in rows {
            b.append_value(f(r));
        }
        Arc::new(b.finish()) as ArrayRef
    };
    let opt_col = |f: &dyn Fn(&CuratedPage) -> Option<&str>| {
        let mut b = StringBuilder::new();
        for r in rows {
            b.append_option(f(r));
        }
        Arc::new(b.finish()) as ArrayRef
    };
    let json_col = |f: &dyn Fn(&CuratedPage) -> serde_json::Result<String>| {
        let mut b = StringBuilder::new();
        for r in rows {
            b.append_value(f(r)?);
        }
        Ok::<ArrayRef, serde_json::Error>(Arc::new(b.finish()))
    };
    let mut unit_ordinal = UInt32Builder::with_capacity(n);
    let mut seq = UInt32Builder::with_capacity(n);
    let mut appendix = BooleanBuilder::with_capacity(n);
    let mut date = Date32Builder::with_capacity(n);
    let mut congress = UInt32Builder::with_capacity(n);
    let mut text = LargeStringBuilder::new();
    let mut chars = Int32Builder::with_capacity(n);
    let mut sha = FixedSizeBinaryBuilder::with_capacity(n, 32);
    let mut at = TimestampMicrosecondBuilder::with_capacity(n).with_timezone("UTC");
    for r in rows {
        unit_ordinal.append_value(r.unit_ordinal);
        seq.append_value(r.seq);
        appendix.append_value(r.appendix);
        date.append_value(unix_days(r.date));
        congress.append_option(r.congress.map(u32::from));
        text.append_option(r.text.as_deref());
        chars.append_value(r.text_chars);
        sha.append_value(r.text_sha256)?;
        at.append_value(r.fetched_at.timestamp_micros());
    }
    let cols: Vec<ArrayRef> = vec![
        str_col(&|r| &r.doc_id),
        str_col(&|r| &r.source),
        str_col(&|r| &r.series),
        str_col(&|r| &r.unit_id),
        Arc::new(unit_ordinal.finish()),
        opt_col(&|r| r.package_id.as_deref()),
        Arc::new(seq.finish()),
        opt_col(&|r| r.printed_page.as_deref()),
        Arc::new(appendix.finish()),
        Arc::new(date.finish()),
        str_col(&|r| &r.date_precision),
        str_col(&|r| &r.date_from),
        Arc::new(congress.finish()),
        opt_col(&|r| r.session.as_deref()),
        opt_col(&|r| r.chamber.as_deref()),
        opt_col(&|r| r.doc_class.as_deref()),
        opt_col(&|r| r.doc_number.as_deref()),
        opt_col(&|r| r.citation.as_deref()),
        opt_col(&|r| r.serial.as_deref()),
        opt_col(&|r| r.title.as_deref()),
        str_col(&|r| &r.text_status),
        Arc::new(text.finish()),
        Arc::new(chars.finish()),
        Arc::new(sha.finish()),
        str_col(&|r| &r.ocr_source),
        opt_col(&|r| r.ocr_engine.as_deref()),
        opt_col(&|r| r.link.as_deref()),
        json_col(&|r| serde_json::to_string(&r.states))?,
        json_col(&|r| serde_json::to_string(&r.places))?,
        json_col(&|r| serde_json::to_string(&r.speakers))?,
        Arc::new(at.finish()),
    ];
    Ok(RecordBatch::try_new(schema(), cols)?)
}

/// One Parquet part of `rows`.
pub fn write_part(rows: &[CuratedPage]) -> anyhow::Result<Vec<u8>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(6)?))
        .set_max_row_group_row_count(Some(4096))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    for chunk in rows.chunks(4096) {
        w.write(&to_batch(chunk)?)?;
    }
    Ok(w.into_inner()?)
}

/// Every row of a part.
pub fn read_part(bytes: Bytes) -> anyhow::Result<Vec<CuratedPage>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch?;
        macro_rules! col {
            ($name:expr, $t:ty) => {
                batch
                    .column_by_name($name)
                    .with_context(|| format!("congress part has no `{}` column", $name))?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let s = |name: &str| -> anyhow::Result<&StringArray> {
            batch
                .column_by_name(name)
                .with_context(|| format!("congress part has no `{name}` column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .with_context(|| format!("column `{name}` has an unexpected type"))
        };
        let get = |a: &StringArray, i: usize| a.value(i).to_owned();
        let opt = |a: &StringArray, i: usize| (!a.is_null(i)).then(|| a.value(i).to_owned());
        let (doc_id, source, series, unit_id) =
            (s("doc_id")?, s("source")?, s("series")?, s("unit_id")?);
        let unit_ordinal = col!("unit_ordinal", UInt32Array);
        let package_id = s("package_id")?;
        let seq = col!("seq", UInt32Array);
        let printed_page = s("printed_page")?;
        let appendix = col!("appendix", BooleanArray);
        let date = col!("date", Date32Array);
        let (precision, date_from) = (s("date_precision")?, s("date_from")?);
        let congress = col!("congress", UInt32Array);
        let (session, chamber, doc_class, doc_number) = (
            s("session")?,
            s("chamber")?,
            s("doc_class")?,
            s("doc_number")?,
        );
        let (citation, serial, title, status) =
            (s("citation")?, s("serial")?, s("title")?, s("text_status")?);
        let text = col!("text", LargeStringArray);
        let chars = col!("text_chars", Int32Array);
        let sha = col!("text_sha256", FixedSizeBinaryArray);
        let (ocr_source, ocr_engine, link) = (s("ocr_source")?, s("ocr_engine")?, s("link")?);
        let (states, places, speakers) = (s("states")?, s("places")?, s("speakers")?);
        let at = col!("fetched_at", TimestampMicrosecondArray);
        for i in 0..batch.num_rows() {
            out.push(CuratedPage {
                doc_id: get(doc_id, i),
                source: get(source, i),
                series: get(series, i),
                unit_id: get(unit_id, i),
                unit_ordinal: unit_ordinal.value(i),
                package_id: opt(package_id, i),
                seq: seq.value(i),
                printed_page: opt(printed_page, i),
                appendix: appendix.value(i),
                date: crate::curated::from_unix_days(date.value(i)),
                date_precision: get(precision, i),
                date_from: get(date_from, i),
                congress: (!congress.is_null(i))
                    .then(|| u16::try_from(congress.value(i)))
                    .transpose()?,
                session: opt(session, i),
                chamber: opt(chamber, i),
                doc_class: opt(doc_class, i),
                doc_number: opt(doc_number, i),
                citation: opt(citation, i),
                serial: opt(serial, i),
                title: opt(title, i),
                text_status: get(status, i),
                text: (!text.is_null(i)).then(|| text.value(i).to_owned()),
                text_chars: chars.value(i),
                text_sha256: sha.value(i).try_into().context("sha256 width")?,
                ocr_source: get(ocr_source, i),
                ocr_engine: opt(ocr_engine, i),
                link: opt(link, i),
                states: serde_json::from_str(states.value(i)).context("states")?,
                places: serde_json::from_str(places.value(i)).context("places")?,
                speakers: serde_json::from_str(speakers.value(i)).context("speakers")?,
                fetched_at: DateTime::from_timestamp_micros(at.value(i))
                    .context("fetched_at out of range")?,
            });
        }
    }
    Ok(out)
}

/// A part an attempt wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub rows: u64,
}

/// `counts.json`: what one finished attempt holds. The release reads these
/// alone to decide whether the Congress index changed, and sums `days` into
/// the Congress baselines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub schema: u32,
    pub series: String,
    pub source: String,
    pub unit: String,
    pub attempt: u32,
    pub title: Option<String>,
    pub congress: Option<u16>,
    pub session: Option<String>,
    /// Every page, whatever its text.
    pub pages: u64,
    /// Pages with text, which the index holds.
    pub indexable: u64,
    /// Pages by `text_status`, `date_from`, and in the appendix.
    pub text_status: BTreeMap<String, u64>,
    pub date_from: BTreeMap<String, u64>,
    pub appendix_pages: u64,
    /// Pages naming at least one member.
    pub pages_with_speakers: u64,
    /// Pages per day (`usnm_core::time::day_number`), sorted.
    pub days: Vec<(u32, u32)>,
    pub parts: Vec<PartRef>,
    pub finished_at: DateTime<Utc>,
    /// The latest date the source gave for what the attempt read (GovInfo's
    /// sitemap `lastmod`, the newest of the volume's packages), so a later
    /// fetch knows the unit changed. None for sources without one (UNT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_lastmod: Option<String>,
    /// A fingerprint of every package the attempt read with its `lastmod`
    /// (the Serial Set's `Volume::fingerprint`), so a later fetch sees a
    /// package added, dropped or changed even when the newest `lastmod`
    /// didn't move. None for sources without one, and for attempts written
    /// before it was recorded (the Serial Set fetches those again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_packages: Option<String>,
}

impl Counts {
    /// The counts of `rows`, written as `parts`.
    pub fn of(
        series: &str,
        source: &str,
        unit: &str,
        attempt: u32,
        rows: &[CuratedPage],
        parts: Vec<PartRef>,
        finished_at: DateTime<Utc>,
    ) -> Self {
        let mut text_status = BTreeMap::new();
        let mut date_from = BTreeMap::new();
        let mut days: BTreeMap<u32, u32> = BTreeMap::new();
        for r in rows {
            *text_status.entry(r.text_status.clone()).or_default() += 1;
            *date_from.entry(r.date_from.clone()).or_default() += 1;
            *days.entry(usnm_core::time::day_number(r.date)).or_default() += 1;
        }
        let first = rows.first();
        Self {
            schema: SCHEMA_VERSION,
            series: series.to_owned(),
            source: source.to_owned(),
            unit: unit.to_owned(),
            attempt,
            title: first.and_then(|r| r.title.clone()),
            congress: first.and_then(|r| r.congress),
            session: first.and_then(|r| r.session.clone()),
            pages: rows.len() as u64,
            indexable: rows.iter().filter(|r| r.indexable()).count() as u64,
            text_status,
            date_from,
            appendix_pages: rows.iter().filter(|r| r.appendix).count() as u64,
            pages_with_speakers: rows.iter().filter(|r| !r.speakers.is_empty()).count() as u64,
            days: days.into_iter().collect(),
            parts,
            finished_at,
            source_lastmod: None,
            source_packages: None,
        }
    }
}

/// `congress/{series}/{unit}`.
pub fn unit_dir(series: &str, unit: &str) -> String {
    format!("{PREFIX}/{series}/{unit}")
}

/// `congress/{series}/{unit}/{attempt}` (four digits).
pub fn attempt_dir(series: &str, unit: &str, attempt: u32) -> String {
    format!("{}/{attempt:04}", unit_dir(series, unit))
}

/// A finished attempt found in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub series: String,
    pub unit: String,
    pub attempt: u32,
    /// The path of its `counts.json`.
    pub counts_path: String,
}

/// The attempt paths under `congress/`: (series, unit, attempt, file).
fn attempt_files(paths: &[String]) -> impl Iterator<Item = (&str, &str, u32, &str)> {
    paths.iter().filter_map(|p| {
        let rest = p.strip_prefix(PREFIX)?.strip_prefix('/')?;
        let mut it = rest.splitn(4, '/');
        let (series, unit, attempt, file) = (it.next()?, it.next()?, it.next()?, it.next()?);
        let attempt: u32 = attempt.parse().ok().filter(|_| attempt.len() == 4)?;
        Some((series, unit, attempt, file))
    })
}

/// `congress/{series}`: what a fetch of one series lists, so it reads
/// nothing outside its own series (the on-prem Serial Set runner's grant
/// covers `congress/serialset/` alone, ADR-0008).
pub fn series_dir(series: &str) -> String {
    format!("{PREFIX}/{series}")
}

/// Each unit's newest finished attempt, by series then unit.
pub async fn finished(store: &dyn ObjectStore) -> anyhow::Result<Vec<Finished>> {
    finished_under(store, PREFIX).await
}

/// [`finished`], for one series: lists `congress/{series}/` only.
pub async fn finished_in(store: &dyn ObjectStore, series: &str) -> anyhow::Result<Vec<Finished>> {
    finished_under(store, &series_dir(series)).await
}

async fn finished_under(store: &dyn ObjectStore, prefix: &str) -> anyhow::Result<Vec<Finished>> {
    let paths = store.list(prefix).await?;
    let mut newest: BTreeMap<(String, String), Finished> = BTreeMap::new();
    for (series, unit, attempt, file) in attempt_files(&paths) {
        if file != COUNTS {
            continue;
        }
        let key = (series.to_owned(), unit.to_owned());
        if newest.get(&key).is_none_or(|f| f.attempt < attempt) {
            newest.insert(
                key,
                Finished {
                    series: series.to_owned(),
                    unit: unit.to_owned(),
                    attempt,
                    counts_path: format!("{}/{COUNTS}", attempt_dir(series, unit, attempt)),
                },
            );
        }
    }
    Ok(newest.into_values().collect())
}

/// The next attempt number of a unit: one past every attempt started.
pub async fn next_attempt(
    store: &dyn ObjectStore,
    series: &str,
    unit: &str,
) -> anyhow::Result<u32> {
    let paths = store.list(&unit_dir(series, unit)).await?;
    Ok(attempt_files(&paths)
        .map(|(_, _, a, _)| a)
        .max()
        .map_or(1, |a| a + 1))
}

/// Write one finished attempt of a unit: its parts, then `counts.json`.
/// Create-only: an attempt someone else started fails here.
pub async fn write_attempt(
    store: &dyn ObjectStore,
    series: &str,
    source: &str,
    unit: &str,
    rows: &[CuratedPage],
    now: DateTime<Utc>,
) -> anyhow::Result<Counts> {
    write_attempt_from(store, series, source, unit, rows, now, None, None).await
}

/// [`write_attempt`], recording the source's `lastmod` and package
/// fingerprint for what it read ([`Counts::source_lastmod`],
/// [`Counts::source_packages`]).
#[allow(clippy::too_many_arguments)]
pub async fn write_attempt_from(
    store: &dyn ObjectStore,
    series: &str,
    source: &str,
    unit: &str,
    rows: &[CuratedPage],
    now: DateTime<Utc>,
    source_lastmod: Option<String>,
    source_packages: Option<String>,
) -> anyhow::Result<Counts> {
    let attempt = next_attempt(store, series, unit).await?;
    let dir = attempt_dir(series, unit, attempt);
    let body = write_part(rows)?;
    let path = format!("{dir}/part-0000.parquet");
    let part = PartRef {
        path: path.clone(),
        sha256: hex(&Sha256::digest(&body)),
        bytes: body.len() as u64,
        rows: rows.len() as u64,
    };
    if !store
        .put_new(&path, body, "application/vnd.apache.parquet")
        .await?
    {
        anyhow::bail!("`{path}` already exists: another fetch wrote this attempt");
    }
    let counts = Counts {
        source_lastmod,
        source_packages,
        ..Counts::of(series, source, unit, attempt, rows, vec![part], now)
    };
    let counts_path = format!("{dir}/{COUNTS}");
    if !store
        .put_new(
            &counts_path,
            serde_json::to_vec_pretty(&counts)?,
            "application/json",
        )
        .await?
    {
        anyhow::bail!("`{counts_path}` already exists");
    }
    Ok(counts)
}

/// The file that marks an attempt finished with nothing to keep, in place
/// of `counts.json` and parts: `congress/{series}/{unit}/{attempt}/empty.json`.
pub const EMPTY: &str = "empty.json";

/// `empty.json`: a unit whose source was read in full and gave no pages (a
/// Serial Set volume all of whose documents are left out for good). It
/// takes an attempt number like any attempt, so whichever of a unit's
/// newest empty attempt and newest finished one has the higher number is
/// the unit's state: the fetch skips the unit until its packages change
/// (`source_packages`), and the release leaves out a finished attempt an
/// empty one came after ([`superseded_by_empty`]). [`finished`] doesn't
/// list it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {
    pub schema: u32,
    pub series: String,
    pub unit: String,
    pub attempt: u32,
    pub finished_at: DateTime<Utc>,
    /// As in [`Counts::source_lastmod`].
    pub source_lastmod: Option<String>,
    /// As in [`Counts::source_packages`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_packages: Option<String>,
    /// What was left out, with why (`{id}: undated`, `{id}: no_pdf`).
    pub left_out: Vec<String>,
}

/// Write an empty attempt of a unit (create-only, as [`write_attempt`]).
pub async fn write_empty(
    store: &dyn ObjectStore,
    series: &str,
    unit: &str,
    now: DateTime<Utc>,
    source_lastmod: Option<String>,
    source_packages: Option<String>,
    left_out: Vec<String>,
) -> anyhow::Result<Empty> {
    let attempt = next_attempt(store, series, unit).await?;
    let empty = Empty {
        schema: SCHEMA_VERSION,
        series: series.to_owned(),
        unit: unit.to_owned(),
        attempt,
        finished_at: now,
        source_lastmod,
        source_packages,
        left_out,
    };
    let path = format!("{}/{EMPTY}", attempt_dir(series, unit, attempt));
    if !store
        .put_new(
            &path,
            serde_json::to_vec_pretty(&empty)?,
            "application/json",
        )
        .await?
    {
        anyhow::bail!("`{path}` already exists: another fetch wrote this attempt");
    }
    Ok(empty)
}

/// Each unit's newest empty attempt, by (series, unit).
pub async fn empties(store: &dyn ObjectStore) -> anyhow::Result<BTreeMap<(String, String), Empty>> {
    empties_under(store, PREFIX).await
}

/// [`empties`], for one series: lists `congress/{series}/` only.
pub async fn empties_in(
    store: &dyn ObjectStore,
    series: &str,
) -> anyhow::Result<BTreeMap<(String, String), Empty>> {
    empties_under(store, &series_dir(series)).await
}

async fn empties_under(
    store: &dyn ObjectStore,
    prefix: &str,
) -> anyhow::Result<BTreeMap<(String, String), Empty>> {
    let paths = store.list(prefix).await?;
    let mut newest: BTreeMap<(String, String), u32> = BTreeMap::new();
    for (series, unit, attempt, file) in attempt_files(&paths) {
        if file == EMPTY {
            let slot = newest
                .entry((series.to_owned(), unit.to_owned()))
                .or_default();
            *slot = (*slot).max(attempt);
        }
    }
    let mut out = BTreeMap::new();
    for ((series, unit), attempt) in newest {
        let path = format!("{}/{EMPTY}", attempt_dir(&series, &unit, attempt));
        let bytes = store
            .get(&path)
            .await?
            .with_context(|| format!("`{path}` vanished"))?;
        let e: Empty = serde_json::from_slice(&bytes).context(path.clone())?;
        anyhow::ensure!(
            e.schema == SCHEMA_VERSION
                && e.series == series
                && e.unit == unit
                && e.attempt == attempt,
            "`{path}` describes another attempt, or has schema {}",
            e.schema
        );
        out.insert((series, unit), e);
    }
    Ok(out)
}

/// Whether the unit of finished attempt `f` has a later empty attempt.
pub fn superseded_by_empty(empties: &BTreeMap<(String, String), Empty>, f: &Finished) -> bool {
    empties
        .get(&(f.series.clone(), f.unit.clone()))
        .is_some_and(|e| e.attempt > f.attempt)
}

/// A finished attempt's `counts.json`, with its sha256.
pub async fn read_counts(
    store: &dyn ObjectStore,
    f: &Finished,
) -> anyhow::Result<(Counts, String)> {
    let bytes = store
        .get(&f.counts_path)
        .await?
        .with_context(|| format!("`{}` vanished", f.counts_path))?;
    let counts: Counts = serde_json::from_slice(&bytes).context(f.counts_path.clone())?;
    anyhow::ensure!(
        counts.schema == SCHEMA_VERSION,
        "`{}` has schema {}; this release reads {SCHEMA_VERSION}",
        f.counts_path,
        counts.schema
    );
    Ok((counts, hex(&Sha256::digest(&bytes))))
}

/// The rows of a part, checked against its entry in `counts.json`.
pub async fn read_checked(
    store: &dyn ObjectStore,
    part: &PartRef,
) -> anyhow::Result<Vec<CuratedPage>> {
    let bytes = store
        .get(&part.path)
        .await?
        .with_context(|| format!("congress part `{}` is missing", part.path))?;
    anyhow::ensure!(
        hex(&Sha256::digest(&bytes)) == part.sha256 && bytes.len() as u64 == part.bytes,
        "congress part `{}` doesn't match its counts.json",
        part.path
    );
    let rows = read_part(Bytes::from(bytes)).with_context(|| part.path.clone())?;
    anyhow::ensure!(
        rows.len() as u64 == part.rows,
        "congress part `{}` has {} rows; counts.json says {}",
        part.path,
        rows.len(),
        part.rows
    );
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn row(seq: u32, text: Option<&str>) -> CuratedPage {
        CuratedPage {
            doc_id: format!("unt-metadc29274_p{seq}"),
            source: "unt".into(),
            series: "globe".into(),
            unit_id: "metadc29274".into(),
            unit_ordinal: 7,
            package_id: Some("metadc29274".into()),
            seq,
            printed_page: (seq > 2).then(|| (seq - 2).to_string()),
            appendix: seq > 4,
            date: NaiveDate::from_ymd_opt(1838, 3, 28 + seq.min(2)).unwrap(),
            date_precision: "day".into(),
            date_from: "heading".into(),
            congress: Some(25),
            session: Some("2".into()),
            chamber: Some("senate".into()),
            doc_class: None,
            doc_number: None,
            citation: Some("Cong. Globe, 25th Cong., 2nd Sess. 1".into()),
            serial: None,
            title: Some("The Congressional Globe, Volume 6".into()),
            text_status: if text.is_some() { "ok" } else { "empty" }.into(),
            text: text.map(str::to_owned),
            text_chars: text.map_or(0, |t| t.chars().count() as i32),
            text_sha256: Sha256::digest(text.unwrap_or_default().as_bytes()).into(),
            ocr_source: "unt".into(),
            ocr_engine: None,
            link: Some("https://example.org/p".into()),
            states: vec![StateRef {
                state: "AL".into(),
                via: "speaker".into(),
                value: Some("CLAY of Alabama".into()),
            }],
            places: vec![],
            speakers: vec![],
            fetched_at: DateTime::from_timestamp(1_790_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn parts_round_trip() {
        let rows = vec![
            row(1, Some("Mr CLAY of Alabama moved")),
            row(2, None),
            row(5, Some("x")),
        ];
        let back = read_part(Bytes::from(write_part(&rows).unwrap())).unwrap();
        assert_eq!(back, rows);
    }

    #[tokio::test]
    async fn the_newest_finished_attempt_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = usnm_store::LocalStore::new(dir.path());
        let at = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let rows = vec![row(1, Some("Mr CLAY of Alabama moved")), row(2, None)];
        let c1 = write_attempt(&store, "globe", "unt", "metadc29274", &rows, at)
            .await
            .unwrap();
        assert_eq!(c1.attempt, 1);
        assert_eq!(c1.pages, 2);
        assert_eq!(c1.indexable, 1);
        assert_eq!(c1.days.len(), 2);
        // An attempt that never finished (no counts.json) doesn't count, but
        // takes its number.
        store
            .put_new(
                "congress/globe/metadc29274/0002/part-0000.parquet",
                vec![1],
                "application/octet-stream",
            )
            .await
            .unwrap();
        let f = finished(&store).await.unwrap();
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].attempt, 1);
        let c3 = write_attempt(&store, "globe", "unt", "metadc29274", &rows[..1], at)
            .await
            .unwrap();
        assert_eq!(c3.attempt, 3);
        let f = finished(&store).await.unwrap();
        assert_eq!(f[0].attempt, 3);
        let (counts, _) = read_counts(&store, &f[0]).await.unwrap();
        let back = read_checked(&store, &counts.parts[0]).await.unwrap();
        assert_eq!(back, rows[..1]);
    }
}
