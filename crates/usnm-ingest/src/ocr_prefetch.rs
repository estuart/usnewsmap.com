//! `usnm-ingest ocr-prefetch-manifest` (#368): the list of badly damaged
//! pages whose scans a node fetches ahead of the workers, built from the
//! damage tables of an index version (12 §12.5 Step 6).
//!
//! It reads the tables the version's list names
//! (`audit/damage/v<VERSION>/versions/<index_version>.json`). Before the
//! damage pass has written that list, `--partial` reads whichever batches
//! already have their tables (a table counts once its summary is there,
//! since the pass writes the summary last). A page goes in when its
//! `damage_rate` is above `--min-damage` (by default
//! `damage::BADLY_DAMAGED`, 0.25). Copies the version hides
//! (`duplicates.json`) are left out, and of the copies left, the one in the
//! batch that sorts first is kept: the scan is the same page on loc.gov
//! whichever batch carried its text.
//!
//! It writes, under `audit/damage/v<VERSION>/prefetch/<index_version>/`
//! (in `partial-<stamp>/` for a partial list):
//!
//! - `manifest.parquet`: `usnm_ocr_contract::prefetch`, in issue order.
//! - `summary.json`: counts by language, decade, batch, and language by
//!   decade, and the manifest's sha256.
//!
//! With `--exchange-url` (the exchange account's `ocr-out` container,
//! where the nodes can read), it also copies both to
//! `manifests/<index_version>/` there. `ocr-sync` reads only `pages/` and
//! `alto/` in that container, and its lifecycle rule removes the copy
//! after 30 days; the curated store keeps the original.
//!
//! The list is written once: a full manifest that is already there with
//! the same rows is kept and its summary and copy are finished, one with
//! other rows is refused, and a partial one gets a new stamp. It logs
//! counts, never text.

use std::collections::{BTreeMap, HashSet};

use anyhow::{bail, Context};
use arrow_array::{Array, Float64Array, Int16Array, StringArray};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usnm_core::damage;
use usnm_core::ids::PageKey;
use usnm_ocr_contract::prefetch::{self, ManifestRow};
use usnm_state::state::DUPLICATES_FILE;
use usnm_store::ObjectStore;

use crate::ocr_damage::{self, VersionList};

/// Tables read at once.
const READS: usize = 8;
/// Where copies for the nodes go in the exchange account's container.
pub const EXCHANGE_PREFIX: &str = "manifests";

/// What to build.
#[derive(Debug, Clone)]
pub struct Options {
    pub index_version: String,
    /// Read the tables there are when the version's list isn't written yet.
    pub partial: bool,
    /// Pages above this go in.
    pub min_damage: f64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            index_version: String::new(),
            partial: false,
            min_damage: damage::BADLY_DAMAGED,
        }
    }
}

/// `summary.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub format: String,
    pub index_version: String,
    pub metric_version: u32,
    pub min_damage: f64,
    /// Built before the damage pass had every table.
    pub partial: bool,
    pub built_at: DateTime<Utc>,
    /// Tables read.
    pub tables: usize,
    /// The version's batches without a table yet (partial lists only).
    pub tables_missing: usize,
    /// Rows above the minimum before duplicates were left out.
    pub rows_above_min: u64,
    /// Copies the version hides, or kept from a batch sorting first.
    pub duplicates_dropped: u64,
    pub pages: u64,
    pub by_language: BTreeMap<String, u64>,
    pub by_decade: BTreeMap<i32, u64>,
    /// By batch with its version (`nbu_gage_ver01`).
    pub by_batch: BTreeMap<String, u64>,
    pub by_language_decade: BTreeMap<String, BTreeMap<i32, u64>>,
    /// The manifest's path in the curated store, size and sha256.
    pub manifest: String,
    pub manifest_bytes: u64,
    pub manifest_sha256: String,
}

/// The folder a manifest goes in, under the damage tables' root.
pub fn dir(index_version: &str, partial_stamp: Option<&str>) -> String {
    let base = format!("{}/prefetch/{index_version}", ocr_damage::root());
    match partial_stamp {
        Some(s) => format!("{base}/partial-{s}"),
        None => base,
    }
}

/// The same folder in the exchange container.
pub fn exchange_dir(index_version: &str, partial_stamp: Option<&str>) -> String {
    match partial_stamp {
        Some(s) => format!("{EXCHANGE_PREFIX}/{index_version}/partial-{s}"),
        None => format!("{EXCHANGE_PREFIX}/{index_version}"),
    }
}

/// One table row above the minimum.
struct Damaged {
    doc_id: String,
    batch: String,
    batch_version: u16,
    lang: Option<String>,
    decision: String,
    damage_rate: f64,
}

/// A table's rows with a damage rate above `min`, reading only the
/// columns needed.
fn read_damaged(bytes: Bytes, min: f64) -> anyhow::Result<Vec<Damaged>> {
    const COLS: [&str; 6] = [
        "doc_id",
        "batch",
        "batch_version",
        "lang",
        "decision",
        "damage_rate",
    ];
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let schema = builder.schema().clone();
    let wanted: Vec<usize> = COLS
        .iter()
        .map(|n| {
            schema
                .index_of(n)
                .with_context(|| format!("damage table has no `{n}` column"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), wanted);
    let mut out = Vec::new();
    for batch in builder.with_projection(mask).build()? {
        let batch = batch?;
        macro_rules! col {
            ($name:expr, $t:ty) => {
                batch
                    .column_by_name($name)
                    .with_context(|| format!("damage table has no `{}` column", $name))?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let doc_id = col!("doc_id", StringArray);
        let name = col!("batch", StringArray);
        let version = col!("batch_version", Int16Array);
        let lang = col!("lang", StringArray);
        let decision = col!("decision", StringArray);
        let rate = col!("damage_rate", Float64Array);
        for i in 0..batch.num_rows() {
            if rate.is_null(i) || rate.value(i) <= min {
                continue;
            }
            out.push(Damaged {
                doc_id: doc_id.value(i).to_owned(),
                batch: name.value(i).to_owned(),
                batch_version: u16::try_from(version.value(i)).unwrap_or(0),
                lang: (!lang.is_null(i)).then(|| lang.value(i).to_owned()),
                decision: decision.value(i).to_owned(),
                damage_rate: rate.value(i),
            });
        }
    }
    Ok(out)
}

/// The tables to read: the version's list, or with `partial` the batches
/// whose summary is written. Returns the table paths and how many of the
/// version's batches have none yet.
async fn tables(
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    opts: &Options,
) -> anyhow::Result<(Vec<String>, usize, bool)> {
    let list_path = ocr_damage::versions_path(&opts.index_version);
    if let Some(b) = curated.get(&list_path).await? {
        let list: VersionList =
            serde_json::from_slice(&b).with_context(|| format!("reading {list_path}"))?;
        if list.metric_version != damage::VERSION {
            bail!(
                "{list_path} is metric v{}, this build reads v{}",
                list.metric_version,
                damage::VERSION
            );
        }
        if opts.partial {
            tracing::info!(
                index_version = %opts.index_version,
                "the damage pass is complete; building the full manifest, not a partial one"
            );
        }
        return Ok((
            list.batches.into_iter().map(|b| b.table).collect(),
            0,
            false,
        ));
    }
    if !opts.partial {
        bail!(
            "{list_path} isn't written yet (the damage pass hasn't finished): wait for it, or pass --partial"
        );
    }
    let batches = ocr_damage::version_batches(reference, &opts.index_version, &[]).await?;
    let checks: Vec<(String, bool)> = futures::stream::iter(batches.iter())
        .map(|b| async move {
            let (name, v) = (b.batch.as_str(), b.curated.version);
            let done = curated.exists(&ocr_damage::summary_path(name, v)).await?;
            Ok::<_, anyhow::Error>((ocr_damage::table_path(name, v), done))
        })
        .buffer_unordered(32)
        .try_collect()
        .await?;
    let missing = checks.iter().filter(|(_, d)| !d).count();
    let mut paths: Vec<String> = checks
        .into_iter()
        .filter(|(_, d)| *d)
        .map(|(p, _)| p)
        .collect();
    paths.sort();
    Ok((paths, missing, true))
}

/// Leave out hidden copies and keep one copy of each page (the batch
/// sorting first), and turn the rest into manifest rows. Returns the rows
/// and how many were dropped.
fn dedupe(mut rows: Vec<Damaged>, hidden: &HashSet<(String, String)>) -> (Vec<ManifestRow>, u64) {
    rows.sort_by(|a, b| (&a.doc_id, &a.batch).cmp(&(&b.doc_id, &b.batch)));
    let mut seen: HashSet<String> = HashSet::new();
    let mut dropped = 0u64;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if hidden.contains(&(r.doc_id.clone(), r.batch.clone())) || seen.contains(&r.doc_id) {
            dropped += 1;
            continue;
        }
        let Ok(key) = PageKey::from_doc_id(&r.doc_id) else {
            tracing::warn!(doc_id = r.doc_id, "a damage table row has a bad doc_id");
            dropped += 1;
            continue;
        };
        seen.insert(r.doc_id.clone());
        out.push(ManifestRow::new(
            &key,
            &r.batch,
            r.batch_version,
            r.lang,
            &r.decision,
            r.damage_rate,
        ));
    }
    prefetch::sort(&mut out);
    (out, dropped)
}

/// Counts by language, decade and batch.
pub fn count(rows: &[ManifestRow], summary: &mut Summary) {
    for r in rows {
        let lang = r.lang_or_und().to_owned();
        let decade = r.decade();
        *summary.by_language.entry(lang.clone()).or_default() += 1;
        *summary.by_decade.entry(decade).or_default() += 1;
        *summary
            .by_batch
            .entry(format!("{}_ver{:02}", r.batch, r.batch_version))
            .or_default() += 1;
        *summary
            .by_language_decade
            .entry(lang)
            .or_default()
            .entry(decade)
            .or_default() += 1;
    }
    summary.pages = rows.len() as u64;
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build the manifest and its summary and write them. `exchange` is the
/// exchange account's `ocr-out` container, if the nodes should get a copy.
pub async fn run(
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    exchange: Option<&dyn ObjectStore>,
    opts: &Options,
) -> anyhow::Result<Summary> {
    if !usnm_store::is_safe_segment(&opts.index_version) {
        bail!("unsafe index version {:?}", opts.index_version);
    }
    if !(0.0..1.0).contains(&opts.min_damage) {
        bail!("--min-damage must be at least 0 and below 1");
    }
    let (paths, missing, partial) = tables(curated, reference, opts).await?;
    tracing::info!(
        index_version = %opts.index_version,
        tables = paths.len(),
        tables_missing = missing,
        partial,
        "ocr prefetch manifest: reading damage tables"
    );
    let min = opts.min_damage;
    let damaged: Vec<Vec<Damaged>> = futures::stream::iter(paths.iter())
        .map(|p| async move {
            let bytes = curated
                .get(p)
                .await?
                .with_context(|| format!("damage table {p} not found"))?;
            tokio::task::spawn_blocking(move || read_damaged(Bytes::from(bytes), min))
                .await?
                .with_context(|| format!("reading {p}"))
        })
        .buffer_unordered(READS)
        .try_collect()
        .await?;
    let damaged: Vec<Damaged> = damaged.into_iter().flatten().collect();
    let rows_above_min = damaged.len() as u64;
    let dup_path = format!("{}/{DUPLICATES_FILE}", opts.index_version);
    let hidden: HashSet<(String, String)> = match reference.get(&dup_path).await? {
        Some(b) => serde_json::from_slice::<Vec<crate::dedup::Hidden>>(&b)
            .with_context(|| format!("reading {dup_path}"))?
            .into_iter()
            .map(|h| (h.doc_id, h.batch))
            .collect(),
        None => HashSet::new(),
    };
    let (rows, duplicates_dropped) = dedupe(damaged, &hidden);
    let now = Utc::now();
    let stamp = partial.then(|| now.format("%Y%m%dT%H%M%SZ").to_string());
    let folder = dir(&opts.index_version, stamp.as_deref());
    let manifest_path = format!("{folder}/manifest.parquet");
    let manifest = {
        let rows = rows.clone();
        tokio::task::spawn_blocking(move || prefetch::encode(&rows)).await??
    };
    let mut summary = Summary {
        format: prefetch::FORMAT.to_owned(),
        index_version: opts.index_version.clone(),
        metric_version: damage::VERSION,
        min_damage: min,
        partial,
        built_at: now,
        tables: paths.len(),
        tables_missing: missing,
        rows_above_min,
        duplicates_dropped,
        manifest: manifest_path.clone(),
        manifest_bytes: manifest.len() as u64,
        manifest_sha256: hex(&Sha256::digest(&manifest)),
        ..Summary::default()
    };
    count(&rows, &mut summary);
    let summary_json = serde_json::to_vec_pretty(&summary)?;
    // The manifest first: the summary marks it complete.
    if !curated
        .put_new(
            &manifest_path,
            manifest.clone(),
            "application/vnd.apache.parquet",
        )
        .await?
    {
        // Written once. The same rows again (a run that stopped before the
        // summary or the copy) go on to finish those; other rows are refused.
        let there = curated
            .get(&manifest_path)
            .await?
            .with_context(|| format!("{manifest_path} vanished"))?;
        if hex(&Sha256::digest(&there)) != summary.manifest_sha256 {
            bail!("{manifest_path} is already there with other rows: a full manifest is written once; delete it and its summary to build it again");
        }
        tracing::info!(manifest = %manifest_path, "ocr prefetch manifest already there with the same rows");
    }
    // A summary from an earlier run of the same rows is kept, and it is the
    // one copied and returned, so the stores agree.
    let summary_path = format!("{folder}/summary.json");
    let (summary, summary_json) = if curated
        .put_new(&summary_path, summary_json.clone(), "application/json")
        .await?
    {
        (summary, summary_json)
    } else {
        let there = curated
            .get(&summary_path)
            .await?
            .with_context(|| format!("{summary_path} vanished"))?;
        let kept: Summary =
            serde_json::from_slice(&there).with_context(|| format!("reading {summary_path}"))?;
        if kept.manifest_sha256 != summary.manifest_sha256 {
            bail!("{summary_path} is for another manifest; delete it and the manifest to build them again");
        }
        (kept, there)
    };
    if let Some(x) = exchange {
        let xdir = exchange_dir(&opts.index_version, stamp.as_deref());
        x.put(
            &format!("{xdir}/manifest.parquet"),
            manifest,
            "application/vnd.apache.parquet",
        )
        .await
        .context("copying the manifest to the exchange account")?;
        x.put(
            &format!("{xdir}/summary.json"),
            summary_json,
            "application/json",
        )
        .await
        .context("copying the summary to the exchange account")?;
        tracing::info!(path = %xdir, "ocr prefetch manifest copied to the exchange account");
    }
    tracing::info!(
        index_version = %opts.index_version,
        partial,
        tables = summary.tables,
        tables_missing = summary.tables_missing,
        pages = summary.pages,
        duplicates_dropped,
        manifest = %manifest_path,
        manifest_bytes = summary.manifest_bytes,
        manifest_sha256 = %summary.manifest_sha256,
        by_language = %serde_json::to_string(&summary.by_language)?,
        "ocr prefetch manifest written"
    );
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr_damage::{encode_table, summary_path, table_path, PageRow, VersionBatch};
    use chrono::NaiveDate;
    use std::sync::Arc;
    use usnm_state::state::{Curated, RunBatch};

    fn page(lccn: &str, y: i32, seq: u16, batch: &str, lang: &str, rate: Option<f64>) -> PageRow {
        let key = PageKey::new(lccn, NaiveDate::from_ymd_opt(y, 5, 3).unwrap(), 1, seq).unwrap();
        PageRow {
            doc_id: key.doc_id(),
            lccn: lccn.into(),
            date: key.date,
            edition: 1,
            seq,
            batch: batch.into(),
            batch_version: 1,
            text_status: "ok",
            lang: Some(lang.into()),
            decision: "detected",
            mixed_with: None,
            script: Some("Latn"),
            damage_rate: rate,
            function_share: Some(0.3),
            garbage_share: Some(0.0),
            words: 100,
            head: 10,
            near: 3,
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        curated: Arc<dyn ObjectStore>,
        reference: Arc<dyn ObjectStore>,
        exchange: Arc<dyn ObjectStore>,
    }

    /// Version `v1` with three batches; `batch_c` has no table yet. A page
    /// of sn2 is in both `batch_a` and `batch_b`.
    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let open = |n: &str| usnm_store::open(dir.path().join(n).to_str().unwrap()).unwrap();
        let (curated, reference, exchange) = (open("curated"), open("reference"), open("x"));
        let tables = [
            (
                "batch_a",
                vec![
                    page("sn1", 1885, 1, "batch_a", "deu", Some(0.62)),
                    page("sn1", 1885, 2, "batch_a", "deu", Some(0.25)),
                    page("sn1", 1885, 3, "batch_a", "deu", None),
                    page("sn2", 1912, 1, "batch_a", "eng", Some(0.4)),
                ],
            ),
            (
                "batch_b",
                vec![
                    page("sn2", 1912, 1, "batch_b", "eng", Some(0.5)),
                    page("sn3", 1907, 4, "batch_b", "srp", Some(0.3)),
                    page("sn3", 1907, 5, "batch_b", "srp", Some(0.1)),
                ],
            ),
        ];
        for (b, rows) in &tables {
            curated
                .put_new(&table_path(b, 1), encode_table(rows).unwrap(), "x")
                .await
                .unwrap();
            curated
                .put_new(&summary_path(b, 1), b"{}".to_vec(), "x")
                .await
                .unwrap();
        }
        let batches: Vec<RunBatch> = ["batch_a", "batch_b", "batch_c"]
            .iter()
            .map(|b| RunBatch {
                batch: (*b).into(),
                curated: Curated {
                    version: 1,
                    parts: vec![],
                    counts: String::new(),
                    pages: 0,
                    ok_pages: 0,
                    lccns: vec![],
                    source_sha256: String::new(),
                    first: String::new(),
                    last: String::new(),
                },
            })
            .collect();
        reference
            .put_new(
                "v1/batches.json",
                serde_json::to_vec(&batches).unwrap(),
                "x",
            )
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            curated,
            reference,
            exchange,
        }
    }

    fn opts(partial: bool) -> Options {
        Options {
            index_version: "v1".into(),
            partial,
            ..Options::default()
        }
    }

    #[tokio::test]
    async fn a_partial_manifest_from_the_tables_written_so_far() {
        let f = fixture().await;
        // No version list yet: refused without --partial.
        assert!(
            run(f.curated.as_ref(), f.reference.as_ref(), None, &opts(false))
                .await
                .is_err()
        );
        let s = run(
            f.curated.as_ref(),
            f.reference.as_ref(),
            Some(f.exchange.as_ref()),
            &opts(true),
        )
        .await
        .unwrap();
        assert!(s.partial);
        assert_eq!((s.tables, s.tables_missing), (2, 1));
        // 0.62, 0.4, 0.5 (a second copy), 0.3; 0.25 isn't above the line.
        assert_eq!(s.rows_above_min, 4);
        assert_eq!(s.duplicates_dropped, 1);
        assert_eq!(s.pages, 3);
        assert_eq!(
            s.by_language,
            BTreeMap::from([("deu".into(), 1), ("eng".into(), 1), ("srp".into(), 1)])
        );
        assert_eq!(
            s.by_decade,
            BTreeMap::from([(1880, 1), (1900, 1), (1910, 1)])
        );
        assert_eq!(
            s.by_batch,
            BTreeMap::from([("batch_a_ver01".into(), 2), ("batch_b_ver01".into(), 1)])
        );
        assert!(s
            .manifest
            .starts_with("audit/damage/v2/prefetch/v1/partial-"));
        let bytes = f.curated.get(&s.manifest).await.unwrap().unwrap();
        assert_eq!(hex(&Sha256::digest(&bytes)), s.manifest_sha256);
        let rows = prefetch::decode(Bytes::from(bytes)).unwrap();
        let ids: Vec<(&str, &str)> = rows
            .iter()
            .map(|r| (r.batch.as_str(), r.doc_id.as_str()))
            .collect();
        // The duplicated page is kept from the batch sorting first.
        assert_eq!(
            ids,
            [
                ("batch_a", "sn1_1885-05-03_ed-1_seq-1"),
                ("batch_a", "sn2_1912-05-03_ed-1_seq-1"),
                ("batch_b", "sn3_1907-05-03_ed-1_seq-4"),
            ]
        );
        assert!((rows[1].damage_rate - 0.4).abs() < 1e-12);
        // The nodes' copy, under manifests/.
        let stamp = s.manifest.split('/').nth(5).unwrap();
        let x = f
            .exchange
            .get(&format!("manifests/v1/{stamp}/manifest.parquet"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(hex(&Sha256::digest(&x)), s.manifest_sha256);
        assert!(f
            .exchange
            .exists(&format!("manifests/v1/{stamp}/summary.json"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn the_full_manifest_follows_the_version_list_and_drops_hidden_copies() {
        let f = fixture().await;
        let list = VersionList {
            index_version: "v1".into(),
            metric_version: damage::VERSION,
            batches: ["batch_a", "batch_b"]
                .iter()
                .map(|b| VersionBatch {
                    batch: (*b).into(),
                    version: 1,
                    table: table_path(b, 1),
                    summary: summary_path(b, 1),
                })
                .collect(),
        };
        f.curated
            .put_new(
                &ocr_damage::versions_path("v1"),
                serde_json::to_vec(&list).unwrap(),
                "x",
            )
            .await
            .unwrap();
        // The version keeps batch_b's copy of sn2's page.
        f.reference
            .put_new(
                "v1/duplicates.json",
                serde_json::to_vec(&serde_json::json!([
                    {"doc_id": "sn2_1912-05-03_ed-1_seq-1", "batch": "batch_a"}
                ]))
                .unwrap(),
                "x",
            )
            .await
            .unwrap();
        let s = run(f.curated.as_ref(), f.reference.as_ref(), None, &opts(true))
            .await
            .unwrap();
        assert!(!s.partial);
        assert_eq!(s.manifest, "audit/damage/v2/prefetch/v1/manifest.parquet");
        let rows = prefetch::decode(Bytes::from(
            f.curated.get(&s.manifest).await.unwrap().unwrap(),
        ))
        .unwrap();
        let sn2 = rows.iter().find(|r| r.lccn == "sn2").unwrap();
        assert_eq!(sn2.batch, "batch_b");
        assert!((sn2.damage_rate - 0.5).abs() < 1e-12);
        let summary: Summary = serde_json::from_slice(
            &f.curated
                .get("audit/damage/v2/prefetch/v1/summary.json")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(summary, s);
        // Written once: the same rows again finish what's missing (here the
        // exchange copy) and keep the summary, which is the one copied and
        // returned; other rows are refused.
        let rerun = run(
            f.curated.as_ref(),
            f.reference.as_ref(),
            Some(f.exchange.as_ref()),
            &opts(false),
        )
        .await
        .unwrap();
        assert_eq!(rerun, s);
        let copied: Summary = serde_json::from_slice(
            &f.exchange
                .get("manifests/v1/summary.json")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(copied, s);
        assert!(f
            .exchange
            .exists("manifests/v1/manifest.parquet")
            .await
            .unwrap());
        let again: Summary = serde_json::from_slice(
            &f.curated
                .get("audit/damage/v2/prefetch/v1/summary.json")
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(again, s);
        let o = Options {
            min_damage: 0.45,
            ..opts(false)
        };
        assert!(run(f.curated.as_ref(), f.reference.as_ref(), None, &o)
            .await
            .is_err());
    }
}
