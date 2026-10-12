//! The list of pages whose scans are on pve-big (#368, 12 Step 11).
//!
//! The seeder runs in Azure and can't reach the scan server on the LAN, so
//! a node relays the server's index: `usnm-ocr-worker publish-fetched`
//! reads `GET /index/full.jsonl` and writes this list to the exchange
//! container as `fetched/<manifest version>/<node>.<stamp>.parquet`, which
//! `ocr-sync`'s pickup never lists (it reads `pages/` and `alto/`). The
//! seeder reads the newest list and queues only pages on it.
//!
//! A node wrote it, so the cloud reads it as untrusted: a format key,
//! a size cap, a row cap and every row checked (a doc_id that is its page
//! key's, a safe batch name). A page listed that isn't there costs only a
//! fetch from LoC at the worker's pacing.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{bail, Context};
use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use usnm_core::ids::PageKey;

/// The list's format, in the Parquet key-value metadata.
pub const FORMAT: &str = "usnm-fetched-scans/1";
const FORMAT_KEY: &str = "usnm.format";
const MANIFEST_KEY: &str = "usnm.manifest";
/// The prefix in `ocr-out`.
pub const PREFIX: &str = "fetched";
/// The largest list read (about 20 bytes a page compressed).
pub const MAX_BYTES: usize = 128 << 20;
/// The most pages a list may hold: the first manifest's 1.92 million pages
/// with room. The reader keeps 8 bytes a page (a hash), so a full list
/// costs the pickup job (1 GiB) about 64 MiB of set and one decoded
/// record batch at a time.
pub const MAX_PAGES: usize = 4_000_000;
/// The most bytes a list's columns may decompress to (about 40 bytes a
/// page at [`MAX_PAGES`]).
pub const MAX_UNCOMPRESSED: i64 = 160 << 20;

/// A page whose full-size scan is on the scan server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fetched {
    /// The base batch the scan is filed under.
    pub batch: String,
    pub doc_id: String,
}

/// A list read back: the pages as 64-bit hashes of (batch, doc_id), so a
/// list a node made large costs little memory. A collision can only make a
/// page that isn't fetched look fetched, which costs a LoC fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct List {
    /// The manifest's index version (`pages-v20261010-2`).
    pub manifest: String,
    pages: HashSet<u64>,
}

impl List {
    pub fn contains(&self, batch: &str, doc_id: &str) -> bool {
        self.pages.contains(&key(batch, doc_id))
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// A page's hash in a [`List`]: SipHash with fixed keys, so the same in
/// every process of a build.
fn key(batch: &str, doc_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    #[allow(deprecated)] // SipHasher: stable, keyed with zeros; no DoS concern here.
    let mut h = std::hash::SipHasher::new();
    batch.hash(&mut h);
    doc_id.hash(&mut h);
    h.finish()
}

/// `fetched/<manifest version>/`.
pub fn dir(manifest: &str) -> String {
    format!("{PREFIX}/{manifest}")
}

/// `fetched/<manifest version>/<node>.<stamp>.parquet`; the stamp
/// (`YYYYMMDDTHHMMSSZ`) sorts by time.
pub fn path(manifest: &str, node: &str, stamp: &str) -> String {
    format!("{}/{node}.{stamp}.parquet", dir(manifest))
}

/// The time in a list's path (`YYYYMMDDTHHMMSSZ`), checked as a UTC time.
/// The seeder picks the newest list by the store's creation time, not by
/// this name, which a node chose.
pub fn stamp_of(path: &str) -> Option<chrono::NaiveDateTime> {
    let name = path.rsplit('/').next()?.strip_suffix(".parquet")?;
    let (_, stamp) = name.rsplit_once('.')?;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ").ok()
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("batch", DataType::Utf8, false),
        Field::new("doc_id", DataType::Utf8, false),
    ]))
}

/// A batch name safe as one path segment, as the manifest's.
fn safe_batch(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Fetched {
    /// A safe batch name and a doc_id that is its page key's.
    pub fn check(&self) -> anyhow::Result<()> {
        check(self)
    }
}

fn check(p: &Fetched) -> anyhow::Result<()> {
    if !safe_batch(&p.batch) {
        bail!("fetched list: an unsafe batch name");
    }
    let key = PageKey::from_doc_id(&p.doc_id).context("fetched list: a bad doc_id")?;
    if key.doc_id() != p.doc_id {
        bail!("fetched list: a doc_id that isn't its key's");
    }
    Ok(())
}

/// The list as Parquet (zstd), pages sorted.
pub fn encode(manifest: &str, pages: &HashSet<Fetched>) -> anyhow::Result<Vec<u8>> {
    if pages.len() > MAX_PAGES {
        bail!("fetched list: {} pages, more than {MAX_PAGES}", pages.len());
    }
    let mut sorted: Vec<&Fetched> = pages.iter().collect();
    sorted.sort();
    for p in &sorted {
        check(p)?;
    }
    let kv = |k: &str, v: &str| parquet::file::metadata::KeyValue::new(k.to_owned(), v.to_owned());
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(
            ZstdLevel::try_new(6).expect("valid level"),
        ))
        .set_key_value_metadata(Some(vec![
            kv(FORMAT_KEY, FORMAT),
            kv(MANIFEST_KEY, manifest),
        ]))
        .build();
    let mut w = ArrowWriter::try_new(Vec::new(), schema(), Some(props))?;
    for chunk in sorted.chunks(65_536) {
        let mut batch = StringBuilder::new();
        let mut doc = StringBuilder::new();
        for p in chunk {
            batch.append_value(&p.batch);
            doc.append_value(&p.doc_id);
        }
        let cols: Vec<ArrayRef> = vec![Arc::new(batch.finish()), Arc::new(doc.finish())];
        w.write(&RecordBatch::try_new(schema(), cols)?)?;
    }
    Ok(w.into_inner()?)
}

/// Reads a list, checking its size, format, row count and every row.
pub fn decode(bytes: Bytes) -> anyhow::Result<List> {
    if bytes.len() > MAX_BYTES {
        bail!("fetched list: {} bytes, more than {MAX_BYTES}", bytes.len());
    }
    let file = bytes.clone();
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    // A node wrote it: bound what decoding allocates before decoding.
    usnm_overlay::check_uncompressed(&file, builder.metadata(), MAX_UNCOMPRESSED)
        .context("fetched list")?;
    let meta = builder.metadata().file_metadata();
    let kv = |k: &str| {
        meta.key_value_metadata()
            .and_then(|kv| kv.iter().find(|e| e.key == k))
            .and_then(|e| e.value.clone())
    };
    if kv(FORMAT_KEY).as_deref() != Some(FORMAT) {
        bail!("not a fetched list ({FORMAT_KEY} isn't {FORMAT:?})");
    }
    let manifest = kv(MANIFEST_KEY).context("fetched list: no manifest version")?;
    let rows = usize::try_from(meta.num_rows()).unwrap_or(usize::MAX);
    if rows > MAX_PAGES {
        bail!("fetched list: {rows} rows, more than {MAX_PAGES}");
    }
    let mut pages: HashSet<u64> = HashSet::with_capacity(rows.min(1 << 20));
    for batch in builder.build()? {
        let batch = batch?;
        let col = |name: &str| -> anyhow::Result<StringArray> {
            batch
                .column_by_name(name)
                .with_context(|| format!("fetched list: no `{name}` column"))?
                .as_any()
                .downcast_ref::<StringArray>()
                .cloned()
                .with_context(|| format!("fetched list: `{name}` isn't text"))
        };
        let (b, d) = (col("batch")?, col("doc_id")?);
        for i in 0..batch.num_rows() {
            let p = Fetched {
                batch: b.value(i).to_owned(),
                doc_id: d.value(i).to_owned(),
            };
            check(&p)?;
            pages.insert(key(&p.batch, &p.doc_id));
        }
        if pages.len() > MAX_PAGES {
            bail!("fetched list: more than {MAX_PAGES} pages");
        }
    }
    Ok(List { manifest, pages })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(batch: &str, doc: &str) -> Fetched {
        Fetched {
            batch: batch.into(),
            doc_id: doc.into(),
        }
    }

    #[test]
    fn round_trips() {
        let pages: HashSet<Fetched> = [
            page("nbu_gage", "sn1_1890-05-03_ed-1_seq-2"),
            page("nbu_gage", "sn1_1890-05-03_ed-1_seq-1"),
            page("lu_Cypher", "sn2_1901-01-01_ed-2_seq-9"),
        ]
        .into_iter()
        .collect();
        let back = decode(Bytes::from(encode("pages-v20261010-2", &pages).unwrap())).unwrap();
        assert_eq!(back.manifest, "pages-v20261010-2");
        assert_eq!(back.len(), 3);
        assert!(pages.iter().all(|p| back.contains(&p.batch, &p.doc_id)));
        assert!(!back.contains("nbu_gage", "sn2_1901-01-01_ed-2_seq-9"));
    }

    #[test]
    fn refuses_bad_rows_and_other_files() {
        for bad in [
            page("../x", "sn1_1890-05-03_ed-1_seq-1"),
            page("nbu_gage", "sn1_1890-05-03_ed-1_seq-01"),
            page("nbu_gage", "not a page"),
        ] {
            assert!(encode("v", &[bad].into_iter().collect()).is_err());
        }
        let manifest = crate::prefetch::encode(&[]).unwrap();
        assert!(decode(Bytes::from(manifest)).is_err());
        assert!(decode(Bytes::from_static(b"nope")).is_err());
    }

    #[test]
    fn paths_carry_a_sortable_stamp() {
        let p = path(
            "pages-v20261010-2",
            "usnm-ocr-node2-arcs-prod",
            "20261012T010500Z",
        );
        assert_eq!(
            p,
            "fetched/pages-v20261010-2/usnm-ocr-node2-arcs-prod.20261012T010500Z.parquet"
        );
        assert_eq!(
            stamp_of(&p).map(|t| t.to_string()),
            Some("2026-10-12 01:05:00".into())
        );
        assert_eq!(stamp_of("fetched/v/node.parquet"), None);
        assert_eq!(stamp_of("fetched/v/node.99999999T999999Z.parquet"), None);
    }
}
