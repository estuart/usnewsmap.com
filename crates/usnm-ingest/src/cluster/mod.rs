//! The experimental search cluster (#238, #239; docs/operations.md, "Search
//! cluster experiment"): Quickwit nodes as Container Apps, each one replica,
//! and the tooling that measures them on a sample of the corpus.
//!
//! - [`node`]: a node's start. It advertises the replica's IP, registers it
//!   in the seed registry (a blob per node), takes the other nodes' entries
//!   as peer seeds, and becomes Quickwit.
//! - [`members`]: the cluster as a node sees it (`/api/v1/cluster`), and the
//!   counters each node's Prometheus endpoint reports.
//! - [`sample`]: a fixed share of the published version's pages, as the
//!   documents a full release would build for them (NDJSON parts).
//! - [`load`]: those documents into a new index on the cluster, timed step
//!   by step: sending, committed, merges settled, sealed.
//! - [`flame`]: a searcher's CPU by thread and function, from the flame
//!   graph of Quickwit's own profiler (#251).
//! - [`grpc`]: one search over Quickwit's gRPC API, for the per-split
//!   resource stats it reports only there (#251).
//! - [`local`]: an index copied from Blob to the replica's disk and served
//!   from there (#251's local-disk test).
//! - [`set`]: a sample packaged for reuse in the archival account: the
//!   batches' LoC archives in one tar and the documents in one file.
//! - [`bench`]: the benchmark searches against the cluster's root, the way
//!   the API runs them (`usnm_search::plan::aggregate`), cold, warm and at
//!   rising concurrency, with each node's share of the leaf work.
//!
//! The `usnm-qwcluster` binary runs each of them.

pub mod bench;
pub mod flame;
pub mod grpc;
pub mod load;
pub mod local;
pub mod members;
pub mod node;
pub mod sample;
pub mod set;

/// One JSON report line in the job's console (`qwcluster report`), which
/// `scripts/qwcluster-report.py` reads back from Log Analytics.
pub fn log_report(kind: &str, report: &serde_json::Value) {
    tracing::info!(kind, report = %report, "qwcluster report");
}

/// Take `prefix` for one build of a sample or set before writing anything
/// there: `{prefix}/building.json`, created only if absent. A finished build
/// (its manifest exists) or another build under way (or one that failed)
/// keeps the prefix, so two executions never write the same objects; build
/// under another name instead.
pub async fn claim(store: &dyn usnm_store::ObjectStore, prefix: &str) -> anyhow::Result<()> {
    if store.exists(&format!("{prefix}/manifest.json")).await? {
        anyhow::bail!("`{prefix}` already exists and is never replaced; build under another name");
    }
    let mark = serde_json::json!({
        "started_at": chrono::Utc::now(),
        "by": crate::owner_id(),
    });
    if !store
        .put_new(
            &format!("{prefix}/building.json"),
            serde_json::to_vec(&mark)?,
            "application/json",
        )
        .await?
    {
        anyhow::bail!(
            "another build of `{prefix}` started (or one failed: see {prefix}/building.json); build under another name"
        );
    }
    Ok(())
}
