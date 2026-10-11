//! The merged parts `ocr-sync` writes, one per title and import:
//! `ocr-local/pages/<lccn>/<run>.parquet` (12 §12.5 Step 8). The 16 overlay
//! columns come first, as `usnm_overlay::schema` has them, with `ocred_at`
//! set from the part blob's creation time on the server rather than the
//! node's clock; then where the row came from and both texts' scores.
//!
//! The release (Step 9) scores both texts again rather than trusting these
//! scores; they are for choosing a row per page, for `ocr-sync --report`
//! and for the operator's estimates.

use std::sync::Arc;

use anyhow::Context;
use arrow_array::builder::{Float64Builder, Int16Builder, Int32Builder, StringBuilder};
use arrow_array::{Array, ArrayRef, Float64Array, Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::{ArrowWriter, ProjectionMask};
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use usnm_overlay::OverlayRow;

/// One text's scores (`usnm_core::damage::score` against the title's
/// catalog languages plus English).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scores {
    /// The detected language (a catalog code) or `und`; none when the text
    /// had no words or LoC's score is unknown.
    pub lang: Option<String>,
    pub decision: Option<String>,
    pub damage_rate: Option<f64>,
    pub function_share: Option<f64>,
    pub head: Option<u32>,
}

impl Scores {
    pub fn of(text: &str, languages: &[&str]) -> Self {
        if text.trim().is_empty() {
            return Self::default();
        }
        let s = usnm_core::damage::score(text, languages);
        Self {
            lang: Some(s.lang),
            decision: Some(s.decision.as_str().to_owned()),
            damage_rate: s.damage_rate,
            function_share: s.function_share,
            head: Some(u32::try_from(s.damage.map_or(0, |d| d.head)).unwrap_or(u32::MAX)),
        }
    }
}

/// One imported page.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    /// `ocred_at` is the part blob's creation time on the server.
    pub row: OverlayRow,
    pub batch_version: u16,
    /// The part's path in `ocr-out`.
    pub source_part: String,
    pub node: String,
    /// `bulk` or `user`.
    pub reason: String,
    pub ours: Scores,
    pub loc: Scores,
}

/// The columns after the overlay's.
pub fn extra_fields() -> Vec<Field> {
    let s = |n: &str, null: bool| Field::new(n, DataType::Utf8, null);
    let f = |n: &str| Field::new(n, DataType::Float64, true);
    let i = |n: &str| Field::new(n, DataType::Int32, true);
    vec![
        Field::new("batch_version", DataType::Int16, false),
        s("source_part", false),
        s("node", false),
        s("reason", false),
        Field::new("metric_version", DataType::Int32, false),
        s("ours_lang", true),
        s("ours_decision", true),
        f("ours_damage_rate"),
        f("ours_function_share"),
        i("ours_head"),
        s("loc_lang", true),
        s("loc_decision", true),
        f("loc_damage_rate"),
        f("loc_function_share"),
        i("loc_head"),
    ]
}

pub fn schema() -> SchemaRef {
    let mut fields: Vec<Field> = usnm_overlay::schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.extend(extra_fields());
    Arc::new(Schema::new(fields))
}

/// A merged part holding `rows`.
pub fn encode(rows: &[Imported]) -> anyhow::Result<Vec<u8>> {
    let overlay: Vec<OverlayRow> = rows.iter().map(|r| r.row.clone()).collect();
    let mut cols = usnm_overlay::columns(&overlay)?;
    let mut version = Int16Builder::new();
    let mut source = StringBuilder::new();
    let mut node = StringBuilder::new();
    let mut reason = StringBuilder::new();
    let mut metric = Int32Builder::new();
    let mut scores: Vec<(
        StringBuilder,
        StringBuilder,
        Float64Builder,
        Float64Builder,
        Int32Builder,
    )> = (0..2)
        .map(|_| {
            (
                StringBuilder::new(),
                StringBuilder::new(),
                Float64Builder::new(),
                Float64Builder::new(),
                Int32Builder::new(),
            )
        })
        .collect();
    for r in rows {
        version.append_value(i16::try_from(r.batch_version).context("batch version")?);
        source.append_value(&r.source_part);
        node.append_value(&r.node);
        reason.append_value(&r.reason);
        metric.append_value(i32::try_from(usnm_core::damage::VERSION).unwrap_or(i32::MAX));
        for (b, s) in scores.iter_mut().zip([&r.ours, &r.loc]) {
            b.0.append_option(s.lang.as_deref());
            b.1.append_option(s.decision.as_deref());
            b.2.append_option(s.damage_rate);
            b.3.append_option(s.function_share);
            b.4.append_option(s.head.map(|h| i32::try_from(h).unwrap_or(i32::MAX)));
        }
    }
    cols.push(Arc::new(version.finish()));
    cols.push(Arc::new(source.finish()));
    cols.push(Arc::new(node.finish()));
    cols.push(Arc::new(reason.finish()));
    cols.push(Arc::new(metric.finish()));
    for mut b in scores {
        cols.push(Arc::new(b.0.finish()) as ArrayRef);
        cols.push(Arc::new(b.1.finish()));
        cols.push(Arc::new(b.2.finish()));
        cols.push(Arc::new(b.3.finish()));
        cols.push(Arc::new(b.4.finish()));
    }
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(6).expect("valid level"),
        ))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    w.write(&RecordBatch::try_new(schema(), cols)?)?;
    Ok(w.into_inner()?)
}

/// A merged part's row without its text, for `--report`.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredRow {
    pub doc_id: String,
    pub lccn: String,
    /// The base batch the page was queued from.
    pub batch: String,
    pub ocr_source: String,
    pub ocr_engine: String,
    /// The node that read it.
    pub node: String,
    /// The damage metric version `ocr-sync` scored the row with. Rows of
    /// another version than `usnm_core::damage::VERSION` are read with no
    /// scores (`ours` and `loc` empty): their rates don't compare with
    /// this version's, so they rank last and count in no estimate. The
    /// release scores the chosen row again either way.
    pub metric_version: u32,
    pub ocred_at: i64,
    pub ours: Scores,
    pub loc: Scores,
}

/// Read a merged part's rows, without the text column.
pub fn read_scores(bytes: Bytes) -> anyhow::Result<Vec<StoredRow>> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let schema = builder.schema().clone();
    let wanted: Vec<usize> = (0..schema.fields().len())
        .filter(|&i| schema.field(i).name() != "text")
        .collect();
    let mask = ProjectionMask::roots(builder.parquet_schema(), wanted);
    let mut out = Vec::new();
    for batch in builder.with_projection(mask).build()? {
        let b = batch?;
        macro_rules! col {
            ($name:expr, $t:ty) => {
                b.column_by_name($name)
                    .with_context(|| format!("merged part has no `{}` column", $name))?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let doc_id = col!("doc_id", StringArray);
        let lccn = col!("lccn", StringArray);
        let batch_name = col!("batch", StringArray);
        let source = col!("ocr_source", StringArray);
        let engine = col!("ocr_engine", StringArray);
        let node = col!("node", StringArray);
        let metric = col!("metric_version", Int32Array);
        let at = col!("ocred_at", arrow_array::TimestampMicrosecondArray);
        let s = |a: &StringArray, i: usize| (!a.is_null(i)).then(|| a.value(i).to_owned());
        let f = |a: &Float64Array, i: usize| (!a.is_null(i)).then(|| a.value(i));
        let h = |a: &Int32Array, i: usize| {
            (!a.is_null(i)).then(|| u32::try_from(a.value(i)).unwrap_or(0))
        };
        let mut sides = Vec::new();
        for side in ["ours", "loc"] {
            sides.push((
                col!(&format!("{side}_lang"), StringArray),
                col!(&format!("{side}_decision"), StringArray),
                col!(&format!("{side}_damage_rate"), Float64Array),
                col!(&format!("{side}_function_share"), Float64Array),
                col!(&format!("{side}_head"), Int32Array),
            ));
        }
        for i in 0..b.num_rows() {
            let score = |c: &(
                &StringArray,
                &StringArray,
                &Float64Array,
                &Float64Array,
                &Int32Array,
            )| Scores {
                lang: s(c.0, i),
                decision: s(c.1, i),
                damage_rate: f(c.2, i),
                function_share: f(c.3, i),
                head: h(c.4, i),
            };
            let current = u32::try_from(metric.value(i)).ok() == Some(usnm_core::damage::VERSION);
            out.push(StoredRow {
                doc_id: doc_id.value(i).to_owned(),
                lccn: lccn.value(i).to_owned(),
                batch: batch_name.value(i).to_owned(),
                ocr_source: source.value(i).to_owned(),
                ocr_engine: engine.value(i).to_owned(),
                node: node.value(i).to_owned(),
                metric_version: u32::try_from(metric.value(i)).unwrap_or(0),
                ocred_at: at.value(i),
                ours: if current {
                    score(&sides[0])
                } else {
                    Scores::default()
                },
                loc: if current {
                    score(&sides[1])
                } else {
                    Scores::default()
                },
            });
        }
    }
    Ok(out)
}

/// The `doc_id` and text of the rows of a merged part at the places in
/// `rows` (0-based, in file order), reading only those two columns and
/// keeping only those rows' text.
pub fn read_texts(
    bytes: Bytes,
    rows: &std::collections::HashSet<usize>,
) -> anyhow::Result<Vec<(usize, (String, String))>> {
    use arrow_array::LargeStringArray;
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let schema = builder.schema().clone();
    let wanted: Vec<usize> = ["doc_id", "text"]
        .iter()
        .map(|n| {
            schema
                .index_of(n)
                .with_context(|| format!("no `{n}` column"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), wanted);
    let mut out = Vec::with_capacity(rows.len());
    let mut at = 0usize;
    for batch in builder.with_projection(mask).build()? {
        let b = batch?;
        let doc_id = b
            .column_by_name("doc_id")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .context("column `doc_id` has an unexpected type")?;
        let text = b
            .column_by_name("text")
            .and_then(|c| c.as_any().downcast_ref::<LargeStringArray>())
            .context("column `text` has an unexpected type")?;
        for i in 0..b.num_rows() {
            if rows.contains(&(at + i)) {
                let t = if text.is_null(i) { "" } else { text.value(i) };
                out.push((at + i, (doc_id.value(i).to_owned(), t.to_owned())));
            }
        }
        at += b.num_rows();
    }
    Ok(out)
}
