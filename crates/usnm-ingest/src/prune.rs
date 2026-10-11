//! `prune`: delete the Quickwit indexes no kept version names (#245).
//!
//! Every release builds new indexes and never deletes old ones, so the
//! index store keeps every version ever built, and every rebuild that was
//! stopped part way. Prune keeps:
//!
//! - the **serving** version (`current.json`): every index it lists, its
//!   Japanese and Congress index sets included;
//! - its **rollback**, the version `current.json` names as
//!   `previous_version`;
//! - any published version newer than the serving one, which is only there
//!   after someone rolled `current.json` back by hand: rolling forward
//!   again must still work. A newer run that wrote its snapshot's manifest
//!   counts as published whatever its status: a release writes the
//!   manifest before `current.json` and records the publish after it, and
//!   that last step can fail (`PublishedUnrecorded`).
//!
//! A kept version's indexes are what any record of it names: `current.json`
//! (the serving one), its run in the pipeline state (`indexes`,
//! `new_index`), the `indexes` of its snapshot's `manifest.json`, and the
//! Japanese and Congress index ids its name gives (`pages-ja-{date}-{n}`,
//! `congress-{date}-{n}`), whether or not they exist.
//!
//! Every other index of ours (`pages-…`, `congress-…`) is a candidate:
//! older versions and stopped or failed rebuilds. Indexes with other names
//! are left alone. Without `--delete` prune only lists the candidates with
//! the size of their published splits. It deletes through Quickwit
//! (`DELETE /api/v1/indexes/{id}`), which removes the split files and the
//! index from the metastore together, never with raw blob deletes.
//!
//! It takes the writer lock like a release (08 §8.4.1), so it never runs
//! beside one, and refuses to run when `current.json` can't be read or a
//! kept version's indexes can't be worked out. Before each delete it reads
//! `current.json` and the runs again, and stops if what it keeps changed;
//! a delete still running when the lock can't be renewed is given up at
//! once, and the caller stops its Quickwit node.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::Value;
use usnm_state::summary::{RunSummary, RUN_FIELDS};
use usnm_store::ObjectStore;

use crate::merges;
use crate::release::WriterLease;
use crate::state::{RunStatus, State, INDEX_RUNS};
use crate::{congress, ocr_ja};

/// The index id prefixes releases create: main indexes (`pages-base-`,
/// `pages-delta-`), the Japanese pages' (`pages-ja-`) and Congress's
/// (`congress-`). Prune never touches an index without one of them.
pub const PREFIXES: [&str; 2] = ["pages-", "congress-"];

/// How long one index's delete may take: Quickwit deletes every split file
/// before it answers, and a full base has about a thousand.
const DELETE_TIMEOUT: Duration = Duration::from_secs(3600);

/// Whether `index_id` is one a release creates.
pub fn is_ours(index_id: &str) -> bool {
    PREFIXES.iter().any(|p| index_id.starts_with(p))
}

/// The versions prune keeps and every index they name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kept {
    pub serving: String,
    pub rollback: Option<String>,
    /// Published versions newer than the serving one (after a rollback by hand).
    pub newer: Vec<String>,
    pub indexes: BTreeSet<String>,
}

/// An index prune would delete, or deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub index_id: String,
    pub splits: u64,
    pub docs: u64,
    /// The size of its published splits' files.
    pub bytes: u64,
}

/// What prune found, and did.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Only listed (no `--delete`).
    pub dry_run: bool,
    pub kept: Kept,
    /// Indexes of ours that no kept version names, oldest name first.
    pub candidates: Vec<Candidate>,
    /// Their published splits' size in all.
    pub total_bytes: u64,
    /// The candidates deleted (empty in a dry run).
    pub deleted: Vec<String>,
    /// Kept indexes Quickwit doesn't have (a derived id with no index, or an
    /// index of an older layout); nothing to do, listed for the record.
    pub kept_absent: Vec<String>,
    /// Indexes with other names, left alone.
    pub other: Vec<String>,
}

/// The ids in a JSON array of strings (none if it isn't one).
fn ids(v: &Value) -> impl Iterator<Item = String> + '_ {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_str())
        .filter(|i| !i.is_empty())
        .map(str::to_owned)
}

/// Every index run, by version. An item that can't be read stops prune:
/// it could be a version to keep.
async fn runs(state: &State) -> anyhow::Result<BTreeMap<String, RunSummary>> {
    state
        .docs
        .select(INDEX_RUNS, &RUN_FIELDS)
        .await
        .context("reading the index runs")?
        .into_iter()
        .map(|v| {
            let id = v["index_version"].as_str().unwrap_or("?").to_owned();
            let run: RunSummary = serde_json::from_value(v)
                .with_context(|| format!("index run `{id}` can't be read; refusing to prune"))?;
            Ok((run.index_version.clone(), run))
        })
        .collect()
}

/// The indexes `version` names in the pipeline state and its snapshot's
/// manifest, plus the Japanese and Congress ids its name gives; and whether
/// either record names any (its main indexes). A manifest whose `indexes`
/// isn't a list of objects with an `index_id` stops prune.
async fn version_indexes(
    reference: &dyn ObjectStore,
    runs: &BTreeMap<String, RunSummary>,
    version: &str,
) -> anyhow::Result<(BTreeSet<String>, bool)> {
    let mut indexes = BTreeSet::new();
    let mut found = false;
    if let Some(run) = runs.get(version) {
        indexes.extend(run.indexes.iter().filter(|i| !i.is_empty()).cloned());
        if !run.new_index.is_empty() {
            indexes.insert(run.new_index.clone());
        }
        found = !indexes.is_empty();
    }
    let path = format!("{version}/manifest.json");
    if let Some(bytes) = reference
        .get(&path)
        .await
        .with_context(|| format!("reading {path}"))?
    {
        let manifest: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("{path} isn't JSON; refusing to prune"))?;
        let listed = match &manifest["indexes"] {
            Value::Null => Vec::new(),
            Value::Array(layout) => layout
                .iter()
                .map(|l| {
                    l["index_id"]
                        .as_str()
                        .filter(|i| !i.is_empty())
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
                .with_context(|| {
                    format!("{path} has an index without an index_id; refusing to prune")
                })?,
            _ => bail!("{path}'s indexes isn't a list; refusing to prune"),
        };
        found |= !listed.is_empty();
        indexes.extend(listed);
    }
    indexes.insert(ocr_ja::index_id(version));
    indexes.insert(congress::index_id(version));
    Ok((indexes, found))
}

/// The versions to keep and their indexes. Fails, so nothing is deleted,
/// when `current.json` or a record of a kept version can't be read.
pub async fn kept(reference: &dyn ObjectStore, state: &State) -> anyhow::Result<Kept> {
    let bytes = reference
        .get("current.json")
        .await
        .context("reading current.json; refusing to prune")?
        .context("there is no current.json; refusing to prune without a published version")?;
    let current: Value =
        serde_json::from_slice(&bytes).context("current.json isn't JSON; refusing to prune")?;
    let serving = current["index_version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("current.json has no index_version; refusing to prune")?
        .to_owned();
    let mut indexes: BTreeSet<String> = ids(&current["indexes"]).collect();
    if indexes.is_empty() {
        bail!("current.json lists no indexes; refusing to prune");
    }
    indexes.extend(ids(&current["ja"]["indexes"]));
    indexes.extend(ids(&current["congress"]["indexes"]));

    let runs = runs(state).await?;
    let Some(serving_run) = runs.get(&serving) else {
        bail!(
            "current.json names `{serving}`, which has no index run in the pipeline state; \
             refusing to prune (a release repairs this)"
        );
    };
    indexes.extend(version_indexes(reference, &runs, &serving).await?.0);

    let rollback = current["previous_version"]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .or_else(|| serving_run.previous_version.clone());
    if let Some(r) = &rollback {
        let (ids, found) = version_indexes(reference, &runs, r).await?;
        if !found {
            bail!(
                "neither an index run nor {r}/manifest.json of the rollback version `{r}` names \
                 its indexes, so they are unknown; refusing to prune"
            );
        }
        indexes.extend(ids);
    }

    let mut newer = Vec::new();
    for run in runs.values() {
        if run.index_version == serving || run.started_at <= serving_run.started_at {
            continue;
        }
        let v = &run.index_version;
        // Published, or maybe published without the record (above).
        let path = format!("{v}/manifest.json");
        let published = run.status == RunStatus::Published
            || reference
                .get(&path)
                .await
                .with_context(|| format!("reading {path}"))?
                .is_some();
        if published {
            let (ids, found) = version_indexes(reference, &runs, v).await?;
            if !found {
                bail!(
                    "neither the index run nor {v}/manifest.json of the published version `{v}` \
                     names its indexes, so they are unknown; refusing to prune"
                );
            }
            newer.push(v.clone());
            indexes.extend(ids);
        }
    }
    Ok(Kept {
        serving,
        rollback,
        newer,
        indexes,
    })
}

/// Every index id Quickwit has (`GET /api/v1/indexes`), sorted.
pub async fn list_indexes(http: &reqwest::Client, base: &str) -> anyhow::Result<Vec<String>> {
    let v: Value = http
        .get(format!("{base}/api/v1/indexes"))
        .send()
        .await?
        .error_for_status()
        .context("listing the indexes")?
        .json()
        .await?;
    let mut ids = v
        .as_array()
        .context("the index list isn't an array")?
        .iter()
        .map(|m| {
            m["index_config"]["index_id"]
                .as_str()
                // `{index_id}:{ulid}`
                .or_else(|| m["index_uid"].as_str()?.split(':').next())
                .map(str::to_owned)
                .context("an index in the list has no id")
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    ids.sort();
    Ok(ids)
}

/// Delete `index_id` and its split files. One already gone is fine.
async fn delete_index(http: &reqwest::Client, base: &str, index_id: &str) -> anyhow::Result<()> {
    let r = http
        .delete(format!("{base}/api/v1/indexes/{index_id}"))
        .timeout(DELETE_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("deleting `{index_id}`"))?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        tracing::warn!(index = index_id, "already deleted");
        return Ok(());
    }
    let status = r.status();
    if !status.is_success() {
        let body = r.text().await.unwrap_or_default();
        bail!("deleting `{index_id}`: {status}: {body}");
    }
    Ok(())
}

/// List the indexes no kept version names, with their sizes, and with
/// `delete` delete them. The caller holds the writer lock (`lease`) and a
/// Quickwit node on the index store at `base`.
pub async fn prune(
    state: &State,
    reference: &dyn ObjectStore,
    base: &str,
    delete: bool,
    lease: &WriterLease,
) -> anyhow::Result<Report> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let kept = kept(reference, state).await?;
    tracing::info!(
        serving = %kept.serving,
        rollback = kept.rollback.as_deref().unwrap_or(""),
        newer = ?kept.newer,
        indexes = ?kept.indexes,
        "prune keeps"
    );
    let all = list_indexes(&http, base).await?;
    let (ours, other): (Vec<String>, Vec<String>) = all.iter().cloned().partition(|i| is_ours(i));
    let node = merges::Node { http: &http, base };
    let mut candidates = Vec::new();
    for id in ours.iter().filter(|i| !kept.indexes.contains(*i)) {
        let splits = node.splits(id).await?;
        let c = Candidate {
            index_id: id.clone(),
            splits: splits.len() as u64,
            docs: splits.iter().map(|s| s.docs).sum(),
            bytes: splits.iter().map(|s| s.bytes).sum(),
        };
        tracing::info!(
            index = %c.index_id,
            splits = c.splits,
            docs = c.docs,
            bytes = c.bytes,
            gib = gib(c.bytes),
            "prune candidate"
        );
        candidates.push(c);
    }
    let total_bytes = candidates.iter().map(|c| c.bytes).sum();
    let kept_absent = kept
        .indexes
        .iter()
        .filter(|i| !all.contains(i))
        .cloned()
        .collect();
    tracing::info!(
        candidates = candidates.len(),
        total_bytes,
        total_gib = gib(total_bytes),
        dry_run = !delete,
        other = ?other,
        "prune plan"
    );

    let mut deleted = Vec::new();
    if delete && !candidates.is_empty() {
        for c in &candidates {
            lease.check()?;
            // Nothing may have changed what is kept since the plan: a
            // rollback by hand writes current.json without the lock, and a
            // delete can take minutes. So before every delete.
            let again = self::kept(reference, state).await?;
            if again != kept {
                bail!(
                    "current.json or the index runs changed while pruning; {} deleted ({}), \
                     run it again",
                    deleted.len(),
                    deleted.join(", ")
                );
            }
            // The plan never holds a kept index; checked again here.
            if kept.indexes.contains(&c.index_id) || !is_ours(&c.index_id) {
                bail!("`{}` is kept; refusing to delete it", c.index_id);
            }
            // A delete can outlast the lock (DELETE_TIMEOUT); give it up as
            // soon as a renewal fails, and the caller stops the node.
            tokio::select! {
                r = delete_index(&http, base, &c.index_id) => r?,
                () = lease.lost() => {
                    lease.check()?;
                    bail!("the writer lock was lost while deleting `{}`", c.index_id)
                }
            }
            tracing::info!(index = %c.index_id, bytes = c.bytes, "deleted index");
            deleted.push(c.index_id.clone());
        }
        tracing::info!(deleted = deleted.len(), total_bytes, "pruned");
    }
    Ok(Report {
        dry_run: !delete,
        kept,
        candidates,
        total_bytes,
        deleted,
        kept_absent,
        other,
    })
}

fn gib(bytes: u64) -> f64 {
    (bytes as f64 / f64::from(1u32 << 30) * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;
    use usnm_store::LocalStore;

    use super::*;
    use crate::docs::{DocStore, MemoryDocs};
    use crate::release::{lock_writer, unlock_writer};

    fn run(version: &str, indexes: &[&str], new_index: &str, day: u32, status: &str) -> Value {
        json!({
            "id": version, "index_version": version, "full": indexes.len() == 1,
            "indexes": indexes, "new_index": new_index, "status": status,
            "docs": 1, "pages": 1,
            "started_at": format!("2026-10-{day:02}T03:00:00Z"),
        })
    }

    struct Env {
        _dir: tempfile::TempDir,
        state: State,
        reference: Arc<LocalStore>,
    }

    /// Three versions (a base, a delta on it, a new base), a stopped
    /// rebuild and a failed one, as the pipeline state has them, with
    /// `pages-v20261015-1` serving.
    async fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let docs = MemoryDocs::default();
        for r in [
            run(
                "pages-v20261001-1",
                &["pages-base-20261001-1"],
                "pages-base-20261001-1",
                1,
                "published",
            ),
            run(
                "pages-v20261008-1",
                &["pages-base-20261001-1", "pages-delta-20261008-1"],
                "pages-delta-20261008-1",
                8,
                "published",
            ),
            run(
                "pages-v20261012-1",
                &["pages-base-20261012-1"],
                "pages-base-20261012-1",
                12,
                "failed",
            ),
            run(
                "pages-v20261015-1",
                &["pages-base-20261015-1"],
                "pages-base-20261015-1",
                15,
                "published",
            ),
        ] {
            let id = r["id"].as_str().unwrap().to_owned();
            docs.upsert(INDEX_RUNS, &id, &r).await.unwrap();
        }
        let reference = Arc::new(LocalStore::new(dir.path().join("reference")));
        put(
            &reference,
            "current.json",
            json!({
                "index_version": "pages-v20261015-1",
                "indexes": ["pages-base-20261015-1"],
                "ja": {"indexes": ["pages-ja-20261015-1"], "fold": 2},
                "previous_version": "pages-v20261008-1",
            }),
        )
        .await;
        // The rollback's manifest names its Japanese index too.
        put(
            &reference,
            "pages-v20261008-1/manifest.json",
            json!({"index_version": "pages-v20261008-1", "indexes": [
                {"index_id": "pages-base-20261001-1"}, {"index_id": "pages-delta-20261008-1"},
                {"index_id": "pages-ja-20261008-1"}]}),
        )
        .await;
        Env {
            _dir: dir,
            state: State::new(Arc::new(docs)),
            reference,
        }
    }

    async fn put(store: &LocalStore, path: &str, v: Value) {
        store
            .put(path, serde_json::to_vec(&v).unwrap(), "application/json")
            .await
            .unwrap();
    }

    #[test]
    fn only_our_indexes_are_candidates() {
        assert!(is_ours("pages-base-20261006-2"));
        assert!(is_ours("pages-delta-20260929-3"));
        assert!(is_ours("pages-ja-20261008-1"));
        assert!(is_ours("congress-20261010-1"));
        assert!(!is_ours("otel-logs-v0_7"));
        assert!(!is_ours("fixture"));
    }

    #[tokio::test]
    async fn keeps_the_serving_version_and_its_rollback() {
        let e = env().await;
        let k = kept(e.reference.as_ref(), &e.state).await.unwrap();
        assert_eq!(k.serving, "pages-v20261015-1");
        assert_eq!(k.rollback.as_deref(), Some("pages-v20261008-1"));
        assert!(k.newer.is_empty());
        for id in [
            "pages-base-20261015-1",
            "pages-ja-20261015-1",
            "congress-20261015-1",
            "pages-base-20261001-1",
            "pages-delta-20261008-1",
            "pages-ja-20261008-1",
            "congress-20261008-1",
        ] {
            assert!(k.indexes.contains(id), "{id}: {k:?}");
        }
        assert!(!k.indexes.contains("pages-base-20261012-1"));
        assert!(!k.indexes.contains("pages-ja-20261001-1"));
    }

    #[tokio::test]
    async fn refuses_without_a_readable_current_json() {
        let e = env().await;
        std::fs::remove_file(e._dir.path().join("reference/current.json")).unwrap();
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(format!("{err:#}").contains("no current.json"), "{err:#}");
        std::fs::write(e._dir.path().join("reference/current.json"), b"{not json").unwrap();
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(format!("{err:#}").contains("refusing to prune"), "{err:#}");
        put(
            &e.reference,
            "current.json",
            json!({"index_version": "pages-v20261015-1"}),
        )
        .await;
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(format!("{err:#}").contains("lists no indexes"), "{err:#}");
    }

    #[tokio::test]
    async fn refuses_when_the_rollback_is_unknown() {
        let e = env().await;
        put(
            &e.reference,
            "current.json",
            json!({"index_version": "pages-v20261015-1", "indexes": ["pages-base-20261015-1"],
                   "previous_version": "pages-v20260901-1"}),
        )
        .await;
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("`pages-v20260901-1`"),
            "{err:#}"
        );
        // And when the serving version has no run.
        put(
            &e.reference,
            "current.json",
            json!({"index_version": "pages-v20261099-1", "indexes": ["pages-base-20261099-1"]}),
        )
        .await;
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(format!("{err:#}").contains("no index run"), "{err:#}");
    }

    #[tokio::test]
    async fn refuses_when_the_rollback_manifest_names_no_indexes() {
        // The rollback with no run: only its manifest can name its indexes.
        let e = env().await;
        put(
            &e.reference,
            "current.json",
            json!({"index_version": "pages-v20261015-1", "indexes": ["pages-base-20261015-1"],
                   "previous_version": "pages-v20260920-1"}),
        )
        .await;
        for (manifest, says) in [
            (json!({}), "unknown"),
            (json!({"indexes": []}), "unknown"),
            (json!({"indexes": "pages-base-20260920-1"}), "isn't a list"),
            (
                json!({"indexes": [{"index": "pages-base-20260920-1"}]}),
                "without an index_id",
            ),
        ] {
            put(&e.reference, "pages-v20260920-1/manifest.json", manifest).await;
            let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
            assert!(format!("{err:#}").contains(says), "{err:#}");
        }
        put(
            &e.reference,
            "pages-v20260920-1/manifest.json",
            json!({"indexes": [{"index_id": "pages-base-20260920-1"}]}),
        )
        .await;
        let k = kept(e.reference.as_ref(), &e.state).await.unwrap();
        assert!(k.indexes.contains("pages-base-20260920-1"));
    }

    #[tokio::test]
    async fn a_rollback_by_hand_keeps_the_newer_version() {
        let e = env().await;
        put(
            &e.reference,
            "current.json",
            json!({"index_version": "pages-v20261008-1",
                   "indexes": ["pages-base-20261001-1", "pages-delta-20261008-1"],
                   "previous_version": "pages-v20261001-1"}),
        )
        .await;
        let k = kept(e.reference.as_ref(), &e.state).await.unwrap();
        assert_eq!(k.newer, ["pages-v20261015-1"]);
        assert!(k.indexes.contains("pages-base-20261015-1"));
        // The failed run isn't a version.
        assert!(!k.indexes.contains("pages-base-20261012-1"));
    }

    #[tokio::test]
    async fn a_newer_run_with_a_snapshot_counts_as_published() {
        // A release writes its manifest, then current.json, then records the
        // publish, which can fail: the run stays `building` though the
        // version went live. Rolled back by hand, it's still kept.
        let e = env().await;
        for (version, day) in [("pages-v20261020-1", 20), ("pages-v20261021-1", 21)] {
            let index = version.replace("pages-v", "pages-base-");
            let r = run(version, &[&index], &index, day, "building");
            e.state.docs.upsert(INDEX_RUNS, version, &r).await.unwrap();
        }
        put(
            &e.reference,
            "pages-v20261020-1/manifest.json",
            json!({"indexes": [{"index_id": "pages-base-20261020-1"}]}),
        )
        .await;
        let k = kept(e.reference.as_ref(), &e.state).await.unwrap();
        assert_eq!(k.newer, ["pages-v20261020-1"]);
        assert!(k.indexes.contains("pages-base-20261020-1"));
        // Without a snapshot it never reached current.json.
        assert!(!k.indexes.contains("pages-base-20261021-1"));
    }

    /// A Quickwit node's index API: lists, splits and deletes, recording
    /// what was deleted.
    async fn fake_quickwit(indexes: &[&str]) -> (String, Arc<Mutex<Vec<String>>>) {
        use axum::extract::Path;
        use axum::routing::get;
        let live: Arc<Mutex<Vec<String>>> =
            Arc::new(Mutex::new(indexes.iter().map(|s| s.to_string()).collect()));
        let deleted = Arc::new(Mutex::new(Vec::new()));
        let (l1, l2, d) = (live.clone(), live.clone(), deleted.clone());
        let app = axum::Router::new()
            .route(
                "/api/v1/indexes",
                get(move || async move {
                    let ids = l1.lock().unwrap().clone();
                    axum::Json(json!(ids
                        .iter()
                        .map(|id| json!({"index_uid": format!("{id}:01JABC"), "index_config": {"index_id": id}}))
                        .collect::<Vec<_>>()))
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|Path(id): Path<String>| async move {
                    // Two splits; their sizes from the index's date.
                    let n: u64 = id.rsplit('-').nth(1).unwrap().parse::<u64>().unwrap() % 100;
                    axum::Json(json!({"splits": [
                        {"split_id": "a", "num_docs": n, "footer_offsets": {"start": 10, "end": n * 1000}},
                        {"split_id": "b", "num_docs": 1, "footer_offsets": {"start": 10, "end": 500}},
                    ]}))
                }),
            )
            .route(
                "/api/v1/indexes/{id}",
                axum::routing::delete(move |Path(id): Path<String>| async move {
                    let mut live = l2.lock().unwrap();
                    let Some(at) = live.iter().position(|i| *i == id) else {
                        return (axum::http::StatusCode::NOT_FOUND, "{}".to_owned());
                    };
                    live.remove(at);
                    d.lock().unwrap().push(id);
                    (axum::http::StatusCode::OK, "[]".to_owned())
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), deleted)
    }

    const LIVE: [&str; 9] = [
        "congress-20261015-1",
        "otel-logs-v0_7",
        "pages-base-20261001-1",
        "pages-base-20261012-1",
        "pages-base-20261015-1",
        "pages-delta-20261008-1",
        "pages-ja-20261001-1",
        "pages-ja-20261008-1",
        "pages-ja-20261015-1",
    ];

    #[tokio::test]
    async fn a_dry_run_lists_the_candidates_and_deletes_nothing() {
        let e = env().await;
        let (base, deleted) = fake_quickwit(&LIVE).await;
        let lease = lock_writer(&e.state, "pruner").await.unwrap();
        let r = prune(&e.state, e.reference.as_ref(), &base, false, &lease)
            .await
            .unwrap();
        unlock_writer(&e.state, "pruner", lease).await.unwrap();
        assert!(r.dry_run);
        let ids: Vec<&str> = r.candidates.iter().map(|c| c.index_id.as_str()).collect();
        assert_eq!(ids, ["pages-base-20261012-1", "pages-ja-20261001-1"]);
        // 12 × 1000 + 500, and 1 × 1000 + 500.
        assert_eq!(r.candidates[0].bytes, 12_500);
        assert_eq!(r.candidates[0].splits, 2);
        assert_eq!(r.candidates[0].docs, 13);
        assert_eq!(r.total_bytes, 12_500 + 1_500);
        assert_eq!(r.other, ["otel-logs-v0_7"]);
        assert!(r.kept_absent.contains(&"congress-20261008-1".to_owned()));
        assert!(r.deleted.is_empty());
        assert!(deleted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deletes_only_what_no_kept_version_names() {
        let e = env().await;
        let (base, deleted) = fake_quickwit(&LIVE).await;
        let lease = lock_writer(&e.state, "pruner").await.unwrap();
        let r = prune(&e.state, e.reference.as_ref(), &base, true, &lease)
            .await
            .unwrap();
        assert!(!r.dry_run);
        assert_eq!(r.deleted, ["pages-base-20261012-1", "pages-ja-20261001-1"]);
        assert_eq!(*deleted.lock().unwrap(), r.deleted);
        // Again: nothing left to delete.
        let r = prune(&e.state, e.reference.as_ref(), &base, true, &lease)
            .await
            .unwrap();
        assert!(r.candidates.is_empty() && r.deleted.is_empty());
        unlock_writer(&e.state, "pruner", lease).await.unwrap();
        assert_eq!(deleted.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn stops_when_what_it_keeps_changes_between_deletes() {
        use axum::extract::Path;
        use axum::routing::{delete, get};
        let e = env().await;
        // Deleting the first candidate rolls current.json back by hand to a
        // version whose Japanese index is the second candidate.
        let current = e._dir.path().join("reference/current.json");
        let app = axum::Router::new()
            .route(
                "/api/v1/indexes",
                get(|| async {
                    axum::Json(json!(["pages-base-20261012-1", "pages-ja-20261001-1"]
                        .iter()
                        .map(|id| json!({"index_config": {"index_id": id}}))
                        .collect::<Vec<_>>()))
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|| async { axum::Json(json!({"splits": []})) }),
            )
            .route(
                "/api/v1/indexes/{id}",
                delete(move |Path(id): Path<String>| async move {
                    assert_eq!(id, "pages-base-20261012-1");
                    let v = json!({"index_version": "pages-v20261008-1",
                        "indexes": ["pages-base-20261001-1", "pages-delta-20261008-1"],
                        "previous_version": "pages-v20261001-1"});
                    std::fs::write(&current, serde_json::to_vec(&v).unwrap()).unwrap();
                    "[]"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let lease = lock_writer(&e.state, "pruner").await.unwrap();
        let err = prune(
            &e.state,
            e.reference.as_ref(),
            &format!("http://{addr}"),
            true,
            &lease,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("changed while pruning; 1 deleted (pages-base-20261012-1)"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn gives_up_a_delete_when_the_lock_is_lost() {
        use axum::routing::{delete, get};
        use std::sync::atomic::{AtomicBool, Ordering};
        let e = env().await;
        let app = axum::Router::new()
            .route(
                "/api/v1/indexes",
                get(|| async {
                    axum::Json(json!([{"index_config": {"index_id": "pages-base-20261012-1"}}]))
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|| async { axum::Json(json!({"splits": []})) }),
            )
            // A delete that never answers.
            .route(
                "/api/v1/indexes/{id}",
                delete(std::future::pending::<&'static str>),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let lost = Arc::new(AtomicBool::new(false));
        let lease = WriterLease::for_test(lost.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            lost.store(true, Ordering::SeqCst);
        });
        let err = tokio::time::timeout(
            Duration::from_secs(10),
            prune(
                &e.state,
                e.reference.as_ref(),
                &format!("http://{addr}"),
                true,
                &lease,
            ),
        )
        .await
        .expect("gave up within seconds")
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("could not be renewed"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn never_runs_beside_a_release() {
        let e = env().await;
        let _release = lock_writer(&e.state, "releaser").await.unwrap();
        let err = lock_writer(&e.state, "pruner").await.err().unwrap();
        assert!(format!("{err:#}").contains("held by `releaser`"), "{err:#}");
    }

    #[tokio::test]
    async fn an_unreadable_run_stops_it() {
        let e = env().await;
        e.state
            .docs
            .upsert(INDEX_RUNS, "pages-v20261020-1", &json!({"id": "pages-v20261020-1", "index_version": "pages-v20261020-1", "status": "published"}))
            .await
            .unwrap();
        let err = kept(e.reference.as_ref(), &e.state).await.unwrap_err();
        assert!(format!("{err:#}").contains("refusing to prune"), "{err:#}");
    }

    #[tokio::test]
    async fn a_failed_delete_stops_it() {
        use axum::routing::{delete, get};
        let e = env().await;
        let app = axum::Router::new()
            .route(
                "/api/v1/indexes",
                get(|| async {
                    axum::Json(json!([{"index_config": {"index_id": "pages-base-20261012-1"}}]))
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|| async { axum::Json(json!({"splits": []})) }),
            )
            .route(
                "/api/v1/indexes/{id}",
                delete(|| async {
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        "storage error",
                    )
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let lease = lock_writer(&e.state, "pruner").await.unwrap();
        let err = prune(
            &e.state,
            e.reference.as_ref(),
            &format!("http://{addr}"),
            true,
            &lease,
        )
        .await
        .unwrap_err();
        assert!(format!("{err:#}").contains("storage error"), "{err:#}");
    }
}
