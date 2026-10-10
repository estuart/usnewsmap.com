//! OCR overlay parts: our own OCR of pages, one Parquet row per page, keyed
//! by `doc_id` (04 §4.8, #139, #296).
//!
//! The Japanese OCR job (`ja-ocr/jaocr.py write_part`) and the on-prem OCR
//! worker (`usnm-ocr-worker`) write parts with this schema; a release reads
//! them (`usnm-ingest`'s `ocr_ja`). The columns are identity (`doc_id`,
//! `page_key`, `lccn`, `date`, `edition`, `seq`, `batch`), provenance
//! (`ocr_source`, `ocr_engine`, `loc_text`, `image_url`, `ocred_at`) and the
//! text (`text_status`, `text`, `text_chars`, `text_sha256`). Every field is
//! nullable, as pyarrow writes them.

use std::sync::Arc;

use anyhow::{bail, Context};
use arrow_array::builder::{
    Date32Builder, FixedSizeBinaryBuilder, Int16Builder, Int32Builder, LargeStringBuilder,
    StringBuilder, TimestampMicrosecondBuilder,
};
use arrow_array::{
    Array, ArrayRef, Date32Array, FixedSizeBinaryArray, Int16Array, LargeStringArray, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bytes::Bytes;
use chrono::{DateTime, NaiveDate, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use sha2::{Digest, Sha256};
use usnm_core::ids::PageKey;
use usnm_core::text::{text_status, TextStatus};

/// The overlay schema, column for column as `jaocr.py write_part` declares it.
pub fn schema() -> SchemaRef {
    let s = |n: &str| Field::new(n, DataType::Utf8, true);
    Arc::new(Schema::new(vec![
        s("doc_id"),
        s("page_key"),
        s("lccn"),
        Field::new("date", DataType::Date32, true),
        Field::new("edition", DataType::Int16, true),
        Field::new("seq", DataType::Int16, true),
        s("batch"),
        s("ocr_source"),
        s("ocr_engine"),
        s("loc_text"),
        s("text_status"),
        Field::new("text", DataType::LargeUtf8, true),
        Field::new("text_chars", DataType::Int32, true),
        Field::new("text_sha256", DataType::FixedSizeBinary(32), true),
        s("image_url"),
        Field::new(
            "ocred_at",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        ),
    ]))
}

/// `ok`, `short` or `empty`, as the curated parts spell them.
pub fn status_str(s: TextStatus) -> &'static str {
    match s {
        TextStatus::Ok => "ok",
        TextStatus::Short => "short",
        TextStatus::Empty => "empty",
    }
}

/// One row to write.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayRow {
    pub key: PageKey,
    pub batch: String,
    pub ocr_source: String,
    pub ocr_engine: String,
    /// Why the page was OCRed (for the Japanese job, why LoC's text wasn't used).
    pub loc_text: String,
    /// The page's text, already normalized; kept whatever its status.
    pub text: String,
    /// The IIIF service base the image came from.
    pub image_url: String,
    pub ocred_at: DateTime<Utc>,
}

impl OverlayRow {
    /// `ok`, `short` or `empty` by [`usnm_core::text::text_status`].
    pub fn status(&self) -> TextStatus {
        text_status(&self.text)
    }
}

fn unix_days(d: NaiveDate) -> i32 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid");
    i32::try_from((d - epoch).num_days()).expect("dates fit in i32 days")
}

fn from_unix_days(n: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid") + chrono::Duration::days(i64::from(n))
}

/// A part holding `rows`, zstd-compressed.
pub fn write_part(rows: &[OverlayRow]) -> anyhow::Result<Vec<u8>> {
    let n = rows.len();
    let mut doc_id = StringBuilder::new();
    let mut page_key = StringBuilder::new();
    let mut lccn = StringBuilder::new();
    let mut date = Date32Builder::with_capacity(n);
    let mut edition = Int16Builder::with_capacity(n);
    let mut seq = Int16Builder::with_capacity(n);
    let mut batch = StringBuilder::new();
    let mut source = StringBuilder::new();
    let mut engine = StringBuilder::new();
    let mut loc_text = StringBuilder::new();
    let mut status = StringBuilder::new();
    let mut text = LargeStringBuilder::new();
    let mut chars = Int32Builder::with_capacity(n);
    let mut sha = FixedSizeBinaryBuilder::with_capacity(n, 32);
    let mut image = StringBuilder::new();
    let mut at = TimestampMicrosecondBuilder::with_capacity(n).with_timezone("UTC");
    let small = |v: u16| i16::try_from(v).context("edition or seq above 32767");
    for r in rows {
        let k = &r.key;
        doc_id.append_value(k.doc_id());
        page_key.append_value(k.to_string());
        lccn.append_value(&k.lccn);
        date.append_value(unix_days(k.date));
        edition.append_value(small(k.edition)?);
        seq.append_value(small(k.seq)?);
        batch.append_value(&r.batch);
        source.append_value(&r.ocr_source);
        engine.append_value(&r.ocr_engine);
        loc_text.append_value(&r.loc_text);
        status.append_value(status_str(r.status()));
        text.append_value(&r.text);
        chars.append_value(i32::try_from(r.text.chars().count()).unwrap_or(i32::MAX));
        sha.append_value(Sha256::digest(r.text.as_bytes()))?;
        image.append_value(&r.image_url);
        at.append_value(r.ocred_at.timestamp_micros());
    }
    let cols: Vec<ArrayRef> = vec![
        Arc::new(doc_id.finish()),
        Arc::new(page_key.finish()),
        Arc::new(lccn.finish()),
        Arc::new(date.finish()),
        Arc::new(edition.finish()),
        Arc::new(seq.finish()),
        Arc::new(batch.finish()),
        Arc::new(source.finish()),
        Arc::new(engine.finish()),
        Arc::new(loc_text.finish()),
        Arc::new(status.finish()),
        Arc::new(text.finish()),
        Arc::new(chars.finish()),
        Arc::new(sha.finish()),
        Arc::new(image.finish()),
        Arc::new(at.finish()),
    ];
    let record = RecordBatch::try_new(schema(), cols)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(6).expect("valid level"),
        ))
        .build();
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, schema(), Some(props))?;
    w.write(&record)?;
    w.close()?;
    Ok(buf)
}

/// One row as a release reads it: the columns it needs.
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayPage {
    pub key: PageKey,
    pub batch: String,
    pub loc_text: String,
    pub text_status: String,
    pub text: Option<String>,
    pub ocr_source: String,
    pub ocr_engine: String,
    /// Microseconds since the Unix epoch.
    pub ocred_at: i64,
}

/// The rows of one part. Fails on a missing column, a column of another
/// type, or a `doc_id` that isn't its page's.
pub fn read_part(bytes: Bytes) -> anyhow::Result<Vec<OverlayPage>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes)?.build()?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch?;
        let col = |name: &str| {
            batch
                .column_by_name(name)
                .with_context(|| format!("overlay part has no `{name}` column"))
        };
        macro_rules! typed {
            ($name:expr, $t:ty) => {
                col($name)?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let doc_id = typed!("doc_id", StringArray);
        let lccn = typed!("lccn", StringArray);
        let date = typed!("date", Date32Array);
        let edition = typed!("edition", Int16Array);
        let seq = typed!("seq", Int16Array);
        let batch_name = typed!("batch", StringArray);
        let loc_text = typed!("loc_text", StringArray);
        let status = typed!("text_status", StringArray);
        let text = typed!("text", LargeStringArray);
        let source = typed!("ocr_source", StringArray);
        let engine = typed!("ocr_engine", StringArray);
        let at = typed!("ocred_at", TimestampMicrosecondArray);
        let _sha = typed!("text_sha256", FixedSizeBinaryArray);
        let unsigned = |v: i16| u16::try_from(v).context("negative edition or seq");
        for i in 0..batch.num_rows() {
            let key = PageKey::new(
                lccn.value(i),
                from_unix_days(date.value(i)),
                unsigned(edition.value(i))?,
                unsigned(seq.value(i))?,
            )?;
            if key.doc_id() != doc_id.value(i) {
                bail!(
                    "row {i}: doc_id `{}` doesn't match its page `{key}`",
                    doc_id.value(i)
                );
            }
            out.push(OverlayPage {
                key,
                batch: batch_name.value(i).to_owned(),
                loc_text: loc_text.value(i).to_owned(),
                text_status: status.value(i).to_owned(),
                text: (!text.is_null(i)).then(|| text.value(i).to_owned()),
                ocr_source: source.value(i).to_owned(),
                ocr_engine: engine.value(i).to_owned(),
                ocred_at: at.value(i),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> OverlayRow {
        OverlayRow {
            key: "sn83045462/1890-05-03/ed-2/seq-7".parse().unwrap(),
            batch: "batch_dlc_test_ver01".into(),
            ocr_source: "usnm-tesseract".into(),
            ocr_engine: "tesseract-5.5.1/deu_latf+deu".into(),
            loc_text: "bulk".into(),
            text: text.into(),
            image_url: "https://tile.loc.gov/image-services/iiif/service:ndnp:x".into(),
            ocred_at: DateTime::from_timestamp(1_791_000_000, 123_000).unwrap(),
        }
    }

    #[test]
    fn round_trips_with_the_python_schema() {
        let long = "Die Zeitung erscheint jeden Sonnabend in Lincoln.";
        let bytes = write_part(&[row(long), row("kurz")]).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes.clone())).unwrap();
        assert_eq!(reader.schema().as_ref(), schema().as_ref());
        let pages = read_part(Bytes::from(bytes.clone())).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].key.doc_id(), "sn83045462_1890-05-03_ed-2_seq-7");
        assert_eq!(pages[0].text_status, "ok");
        assert_eq!(pages[0].text.as_deref(), Some(long));
        assert_eq!(pages[1].text_status, "short");
        assert_eq!(pages[0].ocred_at, 1_791_000_000_000_123);
        let batch = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let chars = batch
            .column_by_name("text_chars")
            .unwrap()
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .unwrap();
        assert_eq!(chars.value(0), long.chars().count() as i32);
        let sha = batch
            .column_by_name("text_sha256")
            .unwrap()
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(sha.value(0), Sha256::digest(long.as_bytes()).as_slice());
    }
}
