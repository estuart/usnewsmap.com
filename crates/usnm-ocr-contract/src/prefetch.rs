//! The scan prefetch manifest (#368): the badly damaged pages of an index
//! version whose scans are fetched onto a node's disk ahead of the workers.
//!
//! `usnm-ingest ocr-prefetch-manifest` writes it from the damage tables
//! (12 §12.5 Step 6) as Parquet, one row per page, in issue order (batch,
//! lccn, date, edition, seq) so the fetcher asks loc.gov for each issue's
//! item JSON once. `usnm-ocr-worker prefetch` reads it on the node.
//!
//! The curated store has no image URL for a page: the bulk OCR archives
//! name the reel, which the IIIF id needs, and curation drops it. So a row
//! carries the issue's loc.gov item URL, and the fetcher resolves the
//! page's IIIF service from the item JSON, as the worker does, and records
//! the URL it fetched beside the scan.

use std::sync::Arc;

use anyhow::{bail, Context};
use arrow_array::builder::{Date32Builder, Float64Builder, Int16Builder, StringBuilder};
use arrow_array::{
    Array, ArrayRef, Date32Array, Float64Array, Int16Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use chrono::{Datelike, NaiveDate};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use usnm_core::ids::PageKey;

/// The manifest's format version, in the Parquet key-value metadata.
pub const FORMAT: &str = "usnm-prefetch-manifest/1";
const FORMAT_KEY: &str = "usnm.format";

/// One page to fetch.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestRow {
    pub doc_id: String,
    /// The base batch name (`nbu_gage`).
    pub batch: String,
    pub batch_version: u16,
    pub lccn: String,
    pub date: NaiveDate,
    pub edition: u16,
    pub seq: u16,
    /// The language the damage pass detected, as a MARC code (`ger`);
    /// `None` for `und`.
    pub lang: Option<String>,
    /// `detected` or `mixed` (the damage table's `decision`).
    pub decision: String,
    pub damage_rate: f64,
    /// The issue's loc.gov item page ([`item_url`]).
    pub item_url: String,
}

impl ManifestRow {
    /// A row for `key` with its item URL filled in.
    pub fn new(
        key: &PageKey,
        batch: &str,
        batch_version: u16,
        lang: Option<String>,
        decision: &str,
        damage_rate: f64,
    ) -> Self {
        Self {
            doc_id: key.doc_id(),
            batch: batch.to_owned(),
            batch_version,
            lccn: key.lccn.clone(),
            date: key.date,
            edition: key.edition,
            seq: key.seq,
            lang,
            decision: decision.to_owned(),
            damage_rate,
            item_url: item_url(key),
        }
    }

    pub fn key(&self) -> anyhow::Result<PageKey> {
        Ok(PageKey::new(&self.lccn, self.date, self.edition, self.seq)?)
    }

    /// The decade of the issue (1890 for 1897).
    pub fn decade(&self) -> i32 {
        self.date.year().div_euclid(10) * 10
    }

    /// The language for counts: the detected one, or `und`.
    pub fn lang_or_und(&self) -> &str {
        self.lang.as_deref().unwrap_or("und")
    }
}

/// The issue's loc.gov item page. The fetcher asks for its JSON
/// (`?fo=json`), whose `resources[0].files` names each page's JP2 and so
/// its IIIF service.
pub fn item_url(key: &PageKey) -> String {
    format!(
        "https://www.loc.gov/item/{}/{}/ed-{}/",
        key.lccn,
        key.date.format("%Y-%m-%d"),
        key.edition
    )
}

/// The manifest's columns.
pub fn schema() -> SchemaRef {
    let s = |n: &str, null: bool| Field::new(n, DataType::Utf8, null);
    let small = |n: &str| Field::new(n, DataType::Int16, false);
    Arc::new(Schema::new(vec![
        s("doc_id", false),
        s("batch", false),
        small("batch_version"),
        s("lccn", false),
        Field::new("date", DataType::Date32, false),
        small("edition"),
        small("seq"),
        s("lang", true),
        s("decision", false),
        Field::new("damage_rate", DataType::Float64, false),
        s("item_url", false),
    ]))
}

fn unix_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid");
    i32::try_from((d - epoch).num_days()).expect("dates fit in i32 days")
}

fn from_unix_days(n: i32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(1970, 1, 1)?.checked_add_signed(chrono::Duration::days(n.into()))
}

/// Sort rows into issue order, the order the manifest is written in.
pub fn sort(rows: &mut [ManifestRow]) {
    rows.sort_by(|a, b| {
        (&a.batch, &a.lccn, a.date, a.edition, a.seq, &a.doc_id)
            .cmp(&(&b.batch, &b.lccn, b.date, b.edition, b.seq, &b.doc_id))
    });
}

/// The manifest as Parquet (zstd), rows in the order given.
pub fn encode(rows: &[ManifestRow]) -> anyhow::Result<Vec<u8>> {
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(6).expect("valid level"),
        ))
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
            FORMAT_KEY.to_owned(),
            FORMAT.to_owned(),
        )]))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    let small = |v: u16| i16::try_from(v).context("edition, seq or version above 32767");
    for chunk in rows.chunks(8192) {
        let mut doc_id = StringBuilder::new();
        let mut batch = StringBuilder::new();
        let mut version = Int16Builder::new();
        let mut lccn = StringBuilder::new();
        let mut date = Date32Builder::new();
        let mut edition = Int16Builder::new();
        let mut seq = Int16Builder::new();
        let mut lang = StringBuilder::new();
        let mut decision = StringBuilder::new();
        let mut rate = Float64Builder::new();
        let mut item = StringBuilder::new();
        for r in chunk {
            doc_id.append_value(&r.doc_id);
            batch.append_value(&r.batch);
            version.append_value(small(r.batch_version)?);
            lccn.append_value(&r.lccn);
            date.append_value(unix_days(r.date));
            edition.append_value(small(r.edition)?);
            seq.append_value(small(r.seq)?);
            lang.append_option(r.lang.as_deref());
            decision.append_value(&r.decision);
            rate.append_value(r.damage_rate);
            item.append_value(&r.item_url);
        }
        let cols: Vec<ArrayRef> = vec![
            Arc::new(doc_id.finish()),
            Arc::new(batch.finish()),
            Arc::new(version.finish()),
            Arc::new(lccn.finish()),
            Arc::new(date.finish()),
            Arc::new(edition.finish()),
            Arc::new(seq.finish()),
            Arc::new(lang.finish()),
            Arc::new(decision.finish()),
            Arc::new(rate.finish()),
            Arc::new(item.finish()),
        ];
        w.write(&RecordBatch::try_new(schema(), cols)?)?;
    }
    Ok(w.into_inner()?)
}

/// Read a manifest, checking its format, its columns and every row: the
/// doc_id must be its key's, and the item URL the one [`item_url`] gives.
pub fn decode(bytes: Bytes) -> anyhow::Result<Vec<ManifestRow>> {
    decode_where(bytes, |_| true)
}

/// [`decode`], keeping only the rows `keep` accepts, so a reader that
/// wants part of a manifest never holds the rest (every row is still
/// checked).
pub fn decode_where(
    bytes: Bytes,
    mut keep: impl FnMut(&ManifestRow) -> bool,
) -> anyhow::Result<Vec<ManifestRow>> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let format = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == FORMAT_KEY))
        .and_then(|k| k.value.clone());
    if format.as_deref() != Some(FORMAT) {
        bail!("not a prefetch manifest ({FORMAT_KEY} is {format:?}, not {FORMAT:?})");
    }
    let mut out = Vec::new();
    for batch in builder.build()? {
        let batch = batch?;
        macro_rules! col {
            ($name:expr, $t:ty) => {
                batch
                    .column_by_name($name)
                    .with_context(|| format!("the manifest has no `{}` column", $name))?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let doc_id = col!("doc_id", StringArray);
        let batch_name = col!("batch", StringArray);
        let version = col!("batch_version", Int16Array);
        let lccn = col!("lccn", StringArray);
        let date = col!("date", Date32Array);
        let edition = col!("edition", Int16Array);
        let seq = col!("seq", Int16Array);
        let lang = col!("lang", StringArray);
        let decision = col!("decision", StringArray);
        let rate = col!("damage_rate", Float64Array);
        let item = col!("item_url", StringArray);
        let small = |a: &Int16Array, i: usize| u16::try_from(a.value(i)).ok();
        for i in 0..batch.num_rows() {
            let row = ManifestRow {
                doc_id: doc_id.value(i).to_owned(),
                batch: batch_name.value(i).to_owned(),
                batch_version: small(version, i).context("negative batch version")?,
                lccn: lccn.value(i).to_owned(),
                date: from_unix_days(date.value(i)).context("date out of range")?,
                edition: small(edition, i).context("negative edition")?,
                seq: small(seq, i).context("negative seq")?,
                lang: (!lang.is_null(i)).then(|| lang.value(i).to_owned()),
                decision: decision.value(i).to_owned(),
                damage_rate: rate.value(i),
                item_url: item.value(i).to_owned(),
            };
            let key = row
                .key()
                .with_context(|| format!("manifest row {}", row.doc_id))?;
            if key.doc_id() != row.doc_id || item_url(&key) != row.item_url {
                bail!("manifest row {} doesn't match its key", row.doc_id);
            }
            if !safe_batch(&row.batch) {
                bail!("manifest row {} has an unsafe batch name", row.doc_id);
            }
            if keep(&row) {
                out.push(row);
            }
        }
    }
    Ok(out)
}

/// A batch name safe as one path segment (it becomes a directory on the
/// node): `[A-Za-z0-9_]`. LoC's names are mostly lowercase, but some aren't
/// (`curiv_joshuaTree`, `lu_Cypher`).
fn safe_batch(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A page's set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Set {
    Keep,
    Buffer,
}

impl Set {
    pub fn dir(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Buffer => "buffer",
        }
    }
}

/// The keep set is every page not detected as English, and every page of
/// the worst English batches; the rest is the buffer set.
pub fn set_of(r: &ManifestRow) -> Set {
    let lang = r.lang_or_und();
    let iso = usnm_core::lang::marc_to_iso3(lang).unwrap_or(lang);
    if iso != "eng" || WORST_ENGLISH.contains(&r.batch.as_str()) {
        Set::Keep
    } else {
        Set::Buffer
    }
}

/// The base names of the worst English batches (ADR-0015).
pub const WORST_ENGLISH: [&str; 5] = [
    "nn_kant",
    "txdn_japan",
    "nn_bentham",
    "nn_carson",
    "kyu_frenchie",
];

/// A page's fetch group, lowest first: German before the 1920s, the worst
/// English batches, Czech, Serbian, Russian, then everything else (the
/// order the maintainer set for a fetch that can't hold every page).
/// The damage tables give MARC codes (`ger`, `cze`); ISO 639-3 (`deu`,
/// `ces`) is accepted too.
pub fn priority(r: &ManifestRow) -> u8 {
    let lang = r.lang_or_und();
    let iso = usnm_core::lang::marc_to_iso3(lang).unwrap_or(lang);
    match iso {
        "deu" if r.decade() < 1920 => 0,
        _ if WORST_ENGLISH.contains(&r.batch.as_str()) => 1,
        "ces" => 2,
        "srp" => 3,
        "rus" => 4,
        _ => 5,
    }
}

/// An issue: (lccn, date, edition).
pub type Issue = (String, NaiveDate, u16);

/// The issue's best (lowest) [`priority`] group seen so far, raised by
/// `r` if it is a keep page. Feed it every keep row of the manifest, so an
/// issue sorts as the fetcher sorts it even when only some of its pages
/// are kept for later.
pub fn note_priority(best: &mut std::collections::HashMap<Issue, u8>, r: &ManifestRow) {
    if set_of(r) == Set::Keep {
        let e = best
            .entry((r.lccn.clone(), r.date, r.edition))
            .or_insert(u8::MAX);
        *e = (*e).min(priority(r));
    }
}

/// Sorts keep rows as the fetcher takes them (its `--order priority`): by
/// issue, each issue in its best page's group from `best`, then (lccn,
/// date, edition, seq).
pub fn sort_keep(rows: &mut [ManifestRow], best: &std::collections::HashMap<Issue, u8>) {
    rows.sort_by_cached_key(|r| {
        let issue = (r.lccn.clone(), r.date, r.edition);
        (best.get(&issue).copied().unwrap_or(u8::MAX), issue, r.seq)
    });
}

/// The keep set's pages in the order the fetcher takes them. The seeder
/// queues fetched pages in this order (12 Step 11).
pub fn keep_order(rows: Vec<ManifestRow>) -> Vec<ManifestRow> {
    let mut best = std::collections::HashMap::new();
    for r in &rows {
        note_priority(&mut best, r);
    }
    let mut rows: Vec<ManifestRow> = rows
        .into_iter()
        .filter(|r| set_of(r) == Set::Keep)
        .collect();
    sort_keep(&mut rows, &best);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(doc: &str, batch: &str, rate: f64) -> ManifestRow {
        let key = PageKey::from_doc_id(doc).unwrap();
        ManifestRow::new(&key, batch, 1, Some("deu".into()), "detected", rate)
    }

    #[test]
    fn round_trips_and_sorts_into_issue_order() {
        let mut rows = vec![
            row("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", 0.5),
            row("sn1_1890-05-03_ed-1_seq-1", "nbu_gage", 0.3),
            row("sn0_1901-01-01_ed-2_seq-9", "nbu_gage", 0.9),
            row("sn9_1850-01-01_ed-1_seq-1", "az_abc", 0.26),
        ];
        rows[3].lang = None;
        sort(&mut rows);
        let ids: Vec<&str> = rows.iter().map(|r| r.doc_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "sn9_1850-01-01_ed-1_seq-1",
                "sn0_1901-01-01_ed-2_seq-9",
                "sn1_1890-05-03_ed-1_seq-1",
                "sn1_1890-05-03_ed-1_seq-2"
            ]
        );
        let back = decode(Bytes::from(encode(&rows).unwrap())).unwrap();
        assert_eq!(back, rows);
        let some = decode_where(Bytes::from(encode(&rows).unwrap()), |r| r.lccn == "sn1").unwrap();
        assert_eq!(some.len(), 2);
        assert_eq!(back[0].lang_or_und(), "und");
        assert_eq!(back[1].decade(), 1900);
        assert_eq!(
            back[1].item_url,
            "https://www.loc.gov/item/sn0/1901-01-01/ed-2/"
        );
    }

    #[test]
    fn checks_rows_it_leaves_out() {
        let mut bad = row("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", 0.5);
        bad.item_url = "https://evil.example/".into();
        let good = row("sn2_1890-05-03_ed-1_seq-1", "nbu_gage", 0.5);
        let bytes = Bytes::from(encode(&[good, bad]).unwrap());
        assert!(decode_where(bytes, |r| r.lccn == "sn2").is_err());
    }

    #[test]
    fn refuses_a_row_that_doesnt_match_its_key_or_another_file() {
        let mut r = row("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", 0.5);
        r.item_url = "https://evil.example/".into();
        assert!(decode(Bytes::from(encode(&[r]).unwrap())).is_err());
        // LoC's mixed-case names are fine; a path isn't.
        let r = row("sn1_1890-05-03_ed-1_seq-2", "curiv_joshuaTree", 0.5);
        assert_eq!(decode(Bytes::from(encode(&[r]).unwrap())).unwrap().len(), 1);
        let mut r = row("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", 0.5);
        r.batch = "../x".into();
        assert!(decode(Bytes::from(encode(&[r]).unwrap())).is_err());
        // A Parquet file without the format marker.
        let props = WriterProperties::builder().build();
        let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props)).unwrap();
        w.write(&RecordBatch::new_empty(schema())).unwrap();
        let other = w.into_inner().unwrap();
        assert!(decode(Bytes::from(other)).is_err());
    }

    #[test]
    fn keep_order_is_the_fetchers() {
        let mut rows = vec![
            row("sn2_1925-01-01_ed-1_seq-1", "nbu_gage", 0.5),
            row("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", 0.5),
            row("sn3_1890-05-03_ed-1_seq-1", "nn_kant", 0.5),
            row("sn1_1890-05-03_ed-1_seq-1", "nbu_gage", 0.3),
            row("sn4_1890-05-03_ed-1_seq-1", "nbu_gage", 0.3),
        ];
        rows[0].lang = Some("ger".into());
        rows[2].lang = Some("eng".into());
        rows[4].lang = Some("eng".into());
        let ids: Vec<String> = keep_order(rows).into_iter().map(|r| r.doc_id).collect();
        assert_eq!(
            ids,
            [
                "sn1_1890-05-03_ed-1_seq-1",
                "sn1_1890-05-03_ed-1_seq-2",
                "sn3_1890-05-03_ed-1_seq-1",
                "sn2_1925-01-01_ed-1_seq-1",
            ]
        );
    }
}
