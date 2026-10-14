//! The runs record (ADR-0016 decision 7, 13 §13.8.3): one row per engine
//! read in a receive that writes a page, as the create-only object
//! `runs/<lccn>_<date>_ed-<n>.<run_id>.parquet` with key-value metadata
//! `usnm.runs.schema=1`. `ocr-sync` reads it with [`read`], which is
//! strict, since a node is outside Azure. The survey writes the same
//! [`RunRecord`] as JSON lines with `"schema": 1`.
//!
//! Nothing writes it yet: the worker's provenance step (13 §13.11 step 8)
//! does.

use std::sync::Arc;

use anyhow::{bail, ensure, Context};
use arrow_array::builder::{
    BooleanBuilder, Float64Builder, StringBuilder, TimestampMicrosecondBuilder, UInt32Builder,
};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, RecordBatch, StringArray,
    TimestampMicrosecondArray, UInt32Array,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef, TimeUnit};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The record's schema version, in the Parquet metadata and in the
/// survey's JSON lines.
pub const SCHEMA: u32 = 1;
const SCHEMA_KEY: &str = "usnm.runs.schema";
/// The prefix in `ocr-out`.
pub const PREFIX: &str = "runs";
/// Engine reads in one receive: a cascade's two and the candidates that
/// answered `unsupported` or `bad_input` before them.
pub const MAX_ROWS: usize = 64;
/// What a record's pages may expand to: 64 rows of some 60 columns,
/// strings included, are far below this.
const MAX_UNCOMPRESSED: i64 = 8 << 20;

/// One engine read (13 §13.8.3). Optional fields are null where a read
/// has nothing to say (a refused candidate has no layout).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    // Identity
    pub run_id: String,
    pub doc_id: String,
    pub node: String,
    pub worker_version: String,
    pub contract: u32,
    /// The full `ocr_engine` tag (13 §13.8.1); the overlay row's for the
    /// kept read.
    pub ocr_engine: String,
    // Engine
    pub engine_id: String,
    pub family: String,
    pub engine_version: String,
    /// `<name> <version> (<lang>)`.
    pub adapter: String,
    // Artifacts
    /// `<registry>/usnm-ocr/<family>@sha256:<digest>`, or `local`.
    pub image_ref: String,
    pub image_digest: String,
    pub models_digest: String,
    /// The profile's model files and their sha256s, as JSON.
    pub models_json: String,
    // Configuration
    pub profile: String,
    /// The route's options over the profile's defaults, as JSON.
    pub options_json: String,
    pub scale: f64,
    /// Tiles the page was cut into, 0 for none.
    pub tiles: u32,
    // Routing
    pub route_id: String,
    pub routing_version: u32,
    pub routing_digest: Option<String>,
    pub candidate_rank: u32,
    /// How the kept read was chosen: `route`, or `cascade:min_damage`.
    pub selection: Option<String>,
    pub selected: bool,
    // Outcome
    /// The adapter's status (`ok`, `unsupported`, `bad_input`, ...).
    pub status: String,
    pub error_kind: Option<String>,
    // Resources
    pub seconds_wall: f64,
    pub seconds_cpu: Option<f64>,
    pub peak_mem_mb: Option<f64>,
    pub instance_peak_mem_mb: Option<f64>,
    pub oom: bool,
    pub instance_restarts: u32,
    /// Start to `hello`, for a cold start.
    pub model_load_seconds: Option<f64>,
    pub models_loaded: String,
    pub cold_start: bool,
    // Input
    pub image_sha256: String,
    pub image_width: u32,
    pub image_height: u32,
    pub dpi: u32,
    pub dpi_source: String,
    // Layout
    pub granularity: String,
    pub reading_order: Option<String>,
    pub markup: String,
    pub blocks: Option<u32>,
    pub lines: Option<u32>,
    pub words: Option<u32>,
    pub mean_conf: Option<f64>,
    pub text_chars: Option<u32>,
    pub text_sha256: Option<String>,
    /// The adapter's `engine_stats`, keys sorted.
    pub engine_stats_json: String,
    // Damage
    /// The message's `score`.
    pub damage_before: Option<f64>,
    /// Null for `jpn`, `und` and languages without a word list.
    pub damage_after: Option<f64>,
    // Page
    pub language: String,
    pub script: Option<String>,
    /// `bulk` or `user`.
    pub reason: String,
    pub queued_at: Option<DateTime<Utc>>,
    /// The node's clock when it wrote the ALTO.
    pub node_finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    U32,
    F64,
    Bool,
    Time,
}

/// The columns in order, with their kind and whether they can be null.
const COLUMNS: &[(&str, Kind, bool)] = &[
    ("run_id", Kind::Str, false),
    ("doc_id", Kind::Str, false),
    ("node", Kind::Str, false),
    ("worker_version", Kind::Str, false),
    ("contract", Kind::U32, false),
    ("ocr_engine", Kind::Str, false),
    ("engine_id", Kind::Str, false),
    ("family", Kind::Str, false),
    ("engine_version", Kind::Str, false),
    ("adapter", Kind::Str, false),
    ("image_ref", Kind::Str, false),
    ("image_digest", Kind::Str, false),
    ("models_digest", Kind::Str, false),
    ("models_json", Kind::Str, false),
    ("profile", Kind::Str, false),
    ("options_json", Kind::Str, false),
    ("scale", Kind::F64, false),
    ("tiles", Kind::U32, false),
    ("route_id", Kind::Str, false),
    ("routing_version", Kind::U32, false),
    ("routing_digest", Kind::Str, true),
    ("candidate_rank", Kind::U32, false),
    ("selection", Kind::Str, true),
    ("selected", Kind::Bool, false),
    ("status", Kind::Str, false),
    ("error_kind", Kind::Str, true),
    ("seconds_wall", Kind::F64, false),
    ("seconds_cpu", Kind::F64, true),
    ("peak_mem_mb", Kind::F64, true),
    ("instance_peak_mem_mb", Kind::F64, true),
    ("oom", Kind::Bool, false),
    ("instance_restarts", Kind::U32, false),
    ("model_load_seconds", Kind::F64, true),
    ("models_loaded", Kind::Str, false),
    ("cold_start", Kind::Bool, false),
    ("image_sha256", Kind::Str, false),
    ("image_width", Kind::U32, false),
    ("image_height", Kind::U32, false),
    ("dpi", Kind::U32, false),
    ("dpi_source", Kind::Str, false),
    ("granularity", Kind::Str, false),
    ("reading_order", Kind::Str, true),
    ("markup", Kind::Str, false),
    ("blocks", Kind::U32, true),
    ("lines", Kind::U32, true),
    ("words", Kind::U32, true),
    ("mean_conf", Kind::F64, true),
    ("text_chars", Kind::U32, true),
    ("text_sha256", Kind::Str, true),
    ("engine_stats_json", Kind::Str, false),
    ("damage_before", Kind::F64, true),
    ("damage_after", Kind::F64, true),
    ("language", Kind::Str, false),
    ("script", Kind::Str, true),
    ("reason", Kind::Str, false),
    ("queued_at", Kind::Time, true),
    ("node_finished_at", Kind::Time, false),
];

/// The Parquet schema of a runs record.
pub fn schema() -> SchemaRef {
    let fields = COLUMNS
        .iter()
        .map(|(name, kind, null)| {
            let t = match kind {
                Kind::Str => DataType::Utf8,
                Kind::U32 => DataType::UInt32,
                Kind::F64 => DataType::Float64,
                Kind::Bool => DataType::Boolean,
                Kind::Time => DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            };
            Field::new(*name, t, *null)
        })
        .collect::<Vec<_>>();
    Arc::new(Schema::new(fields))
}

fn column(rows: &[Map<String, Value>], name: &str, kind: Kind) -> anyhow::Result<ArrayRef> {
    let vals = rows.iter().map(|r| r.get(name).filter(|v| !v.is_null()));
    Ok(match kind {
        Kind::Str => {
            let mut b = StringBuilder::new();
            for v in vals {
                b.append_option(v.and_then(Value::as_str));
            }
            Arc::new(b.finish())
        }
        Kind::U32 => {
            let mut b = UInt32Builder::new();
            for v in vals {
                b.append_option(
                    v.and_then(Value::as_u64)
                        .and_then(|x| u32::try_from(x).ok()),
                );
            }
            Arc::new(b.finish())
        }
        Kind::F64 => {
            let mut b = Float64Builder::new();
            for v in vals {
                b.append_option(v.and_then(Value::as_f64));
            }
            Arc::new(b.finish())
        }
        Kind::Bool => {
            let mut b = BooleanBuilder::new();
            for v in vals {
                b.append_option(v.and_then(Value::as_bool));
            }
            Arc::new(b.finish())
        }
        Kind::Time => {
            let mut b = TimestampMicrosecondBuilder::new().with_timezone("UTC");
            for v in vals {
                let t = v
                    .and_then(Value::as_str)
                    .map(|s| DateTime::parse_from_rfc3339(s).map(|t| t.timestamp_micros()))
                    .transpose()
                    .with_context(|| format!("{name} isn't a time"))?;
                b.append_option(t);
            }
            Arc::new(b.finish())
        }
    })
}

/// Checks what [`read`] checks, so a node never writes a record the
/// pickup refuses: 1 to [`MAX_ROWS`] rows of one `run_id` and `doc_id`,
/// exactly one `selected` and it `ok`.
pub fn validate(rows: &[RunRecord]) -> anyhow::Result<()> {
    ensure!(
        !rows.is_empty() && rows.len() <= MAX_ROWS,
        "{} rows, not 1 to {MAX_ROWS}",
        rows.len()
    );
    let first = &rows[0];
    ensure!(
        !first.run_id.is_empty() && !first.doc_id.is_empty(),
        "no run_id or doc_id"
    );
    for r in rows {
        ensure!(
            r.run_id == first.run_id && r.doc_id == first.doc_id,
            "rows of different runs or pages"
        );
        ensure!(r.scale.is_finite() && r.scale > 0.0, "scale {}", r.scale);
        // Measurements are finite and not negative; ratios are 0 to 1.
        let measures = [
            ("seconds_wall", Some(r.seconds_wall)),
            ("seconds_cpu", r.seconds_cpu),
            ("peak_mem_mb", r.peak_mem_mb),
            ("instance_peak_mem_mb", r.instance_peak_mem_mb),
            ("model_load_seconds", r.model_load_seconds),
        ];
        for (name, v) in measures {
            if let Some(v) = v {
                ensure!(v.is_finite() && v >= 0.0, "{name} {v}");
            }
        }
        let ratios = [
            ("mean_conf", r.mean_conf),
            ("damage_before", r.damage_before),
            ("damage_after", r.damage_after),
        ];
        for (name, v) in ratios {
            if let Some(v) = v {
                ensure!((0.0..=1.0).contains(&v), "{name} {v}");
            }
        }
    }
    let selected: Vec<&RunRecord> = rows.iter().filter(|r| r.selected).collect();
    ensure!(
        selected.len() == 1,
        "{} selected rows, not 1",
        selected.len()
    );
    ensure!(
        selected[0].status == "ok",
        "the selected read's status is {}",
        selected[0].status
    );
    Ok(())
}

/// The record as Parquet bytes, within [`crate::engine::limits::RUNS`].
pub fn write(rows: &[RunRecord]) -> anyhow::Result<Vec<u8>> {
    validate(rows)?;
    let maps: Vec<Map<String, Value>> = rows
        .iter()
        .map(|r| match serde_json::to_value(r) {
            Ok(Value::Object(m)) => Ok(m),
            _ => bail!("a runs row didn't serialize"),
        })
        .collect::<anyhow::Result<_>>()?;
    let cols = COLUMNS
        .iter()
        .map(|(n, k, _)| column(&maps, n, *k))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let batch = RecordBatch::try_new(schema(), cols)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
            SCHEMA_KEY.to_owned(),
            SCHEMA.to_string(),
        )]))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    w.write(&batch)?;
    let out = w.into_inner()?;
    ensure!(
        out.len() <= crate::engine::limits::RUNS,
        "runs record is {} bytes, over {}",
        out.len(),
        crate::engine::limits::RUNS
    );
    // The pickup's reader must take what a node writes, its bound on the
    // uncompressed size included.
    read(Bytes::from(out.clone())).context("the written runs record doesn't read back")?;
    Ok(out)
}

fn cell(a: &ArrayRef, i: usize, kind: Kind) -> anyhow::Result<Value> {
    if a.is_null(i) {
        return Ok(Value::Null);
    }
    let any = a.as_any();
    Ok(match kind {
        Kind::Str => Value::from(any.downcast_ref::<StringArray>().context("type")?.value(i)),
        Kind::U32 => Value::from(any.downcast_ref::<UInt32Array>().context("type")?.value(i)),
        Kind::F64 => Value::from(any.downcast_ref::<Float64Array>().context("type")?.value(i)),
        Kind::Bool => Value::from(any.downcast_ref::<BooleanArray>().context("type")?.value(i)),
        Kind::Time => {
            let us = any
                .downcast_ref::<TimestampMicrosecondArray>()
                .context("type")?
                .value(i);
            let t = DateTime::<Utc>::from_timestamp_micros(us).context("a time out of range")?;
            Value::from(t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
        }
    })
}

/// Reads a runs record strictly: at most [`crate::engine::limits::RUNS`]
/// bytes, `usnm.runs.schema=1`, exactly the schema of [`schema`], and rows
/// that pass [`validate`].
pub fn read(bytes: Bytes) -> anyhow::Result<Vec<RunRecord>> {
    ensure!(
        bytes.len() <= crate::engine::limits::RUNS,
        "runs record is {} bytes, over {}",
        bytes.len(),
        crate::engine::limits::RUNS
    );
    let file = bytes.clone();
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    // A node wrote it: bound what decoding allocates before decoding.
    usnm_overlay::check_uncompressed(&file, builder.metadata(), MAX_UNCOMPRESSED)
        .context("runs record")?;
    let version = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == SCHEMA_KEY))
        .and_then(|k| k.value.clone());
    ensure!(
        version.as_deref() == Some("1"),
        "not a runs record (schema {version:?})"
    );
    ensure!(
        builder.schema().fields() == schema().fields(),
        "a runs record with another schema"
    );
    let mut rows = Vec::new();
    for batch in builder.with_batch_size(MAX_ROWS + 1).build()? {
        let batch = batch?;
        ensure!(
            rows.len() + batch.num_rows() <= MAX_ROWS,
            "over {MAX_ROWS} rows"
        );
        for i in 0..batch.num_rows() {
            let mut m = Map::new();
            for (c, (name, kind, _)) in COLUMNS.iter().enumerate() {
                m.insert((*name).to_owned(), cell(batch.column(c), i, *kind)?);
            }
            rows.push(serde_json::from_value::<RunRecord>(Value::Object(m))?);
        }
    }
    validate(&rows)?;
    Ok(rows)
}

/// One survey line: the record with `"schema": 1`.
pub fn to_json_line(r: &RunRecord) -> anyhow::Result<String> {
    let mut v = serde_json::to_value(r)?;
    if let Value::Object(m) = &mut v {
        m.insert("schema".into(), Value::from(SCHEMA));
    }
    let mut s = serde_json::to_string(&v)?;
    s.push('\n');
    Ok(s)
}

/// One survey line back, refusing another schema version.
pub fn from_json_line(line: &str) -> anyhow::Result<RunRecord> {
    let mut v: Value = serde_json::from_str(line)?;
    let m = v.as_object_mut().context("a runs line isn't an object")?;
    let schema = m.remove("schema").and_then(|s| s.as_u64());
    ensure!(
        schema == Some(u64::from(SCHEMA)),
        "a runs line of schema {schema:?}"
    );
    Ok(serde_json::from_value(v)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn row() -> RunRecord {
        RunRecord {
            run_id: "nuc1.20261020T140312123456Z".into(),
            doc_id: "sn84026749_1907-05-01_ed-1_seq-4".into(),
            node: "nuc1".into(),
            worker_version: "0.1.0+1dcc5c3".into(),
            contract: 1,
            ocr_engine: "tesseract/5.5.3 profile=deu image=sha256:3f9a0c1d2e4b models=sha256:8c2d71aa09fe route=german-pre1920@r1 contract=1".into(),
            engine_id: "tesseract-5.5".into(),
            family: "tesseract".into(),
            engine_version: "5.5.3".into(),
            adapter: "usnm-ocr-adapter-tesseract 0.1.0 (rust)".into(),
            image_ref: "local".into(),
            image_digest: "sha256:3f9a".into(),
            models_digest: "sha256:8c2d".into(),
            models_json: "{}".into(),
            profile: "deu".into(),
            options_json: "{\"psm\":3}".into(),
            scale: 1.0,
            tiles: 0,
            route_id: "german-pre1920".into(),
            routing_version: 1,
            routing_digest: None,
            candidate_rank: 0,
            selection: Some("route".into()),
            selected: true,
            status: "ok".into(),
            error_kind: None,
            seconds_wall: 41.5,
            seconds_cpu: Some(40.25),
            peak_mem_mb: Some(512.0),
            instance_peak_mem_mb: None,
            oom: false,
            instance_restarts: 0,
            model_load_seconds: Some(0.5),
            models_loaded: "per_page".into(),
            cold_start: true,
            image_sha256: "02".into(),
            image_width: 5412,
            image_height: 7208,
            dpi: 300,
            dpi_source: "assumed".into(),
            granularity: "word".into(),
            reading_order: None,
            markup: "none".into(),
            blocks: Some(12),
            lines: Some(400),
            words: Some(6034),
            mean_conf: Some(0.71),
            text_chars: Some(30000),
            text_sha256: Some("ab".into()),
            engine_stats_json: "{}".into(),
            damage_before: Some(0.62),
            damage_after: None,
            language: "deu".into(),
            script: Some("Latf".into()),
            reason: "bulk".into(),
            queued_at: None,
            node_finished_at: "2026-10-20T14:04:31.123456Z".parse().unwrap(),
        }
    }

    #[test]
    fn round_trips_through_parquet_and_json_lines() {
        let mut refused = row();
        refused.selected = false;
        refused.status = "unsupported".into();
        refused.candidate_rank = 0;
        let rows = vec![
            refused,
            RunRecord {
                candidate_rank: 1,
                ..row()
            },
        ];
        let bytes = write(&rows).unwrap();
        assert_eq!(read(Bytes::from(bytes)).unwrap(), rows);
        let line = to_json_line(&rows[1]).unwrap();
        assert!(line.contains("\"schema\":1"));
        assert_eq!(from_json_line(&line).unwrap(), rows[1]);
        assert!(from_json_line(&line.replace("\"schema\":1", "\"schema\":2")).is_err());
    }

    #[test]
    fn refuses_what_the_pickup_would() {
        assert!(write(&[]).is_err());
        let mut none = row();
        none.selected = false;
        assert!(write(&[none]).is_err(), "no selected read");
        let mut failed = row();
        failed.status = "failed".into();
        assert!(write(&[failed]).is_err(), "the kept read failed");
        let mut other = row();
        other.selected = false;
        other.doc_id = "x".into();
        assert!(write(&[row(), other]).is_err(), "rows of two pages");
        assert!(read(Bytes::from_static(b"not parquet")).is_err());
        for bad in [f64::NAN, -1.0, f64::INFINITY] {
            let mut r = row();
            r.seconds_wall = bad;
            assert!(write(&[r]).is_err(), "seconds_wall {bad}");
        }
        let mut r = row();
        r.mean_conf = Some(1.5);
        assert!(write(&[r]).is_err());
        // A string that compresses well but expands past the reader's
        // bound is refused when written, not at the pickup.
        let mut huge = row();
        huge.models_json = "a".repeat(9 << 20);
        assert!(write(&[huge]).is_err());
        let big = Bytes::from(vec![0u8; crate::engine::limits::RUNS + 1]);
        assert!(read(big).is_err());
    }
}
