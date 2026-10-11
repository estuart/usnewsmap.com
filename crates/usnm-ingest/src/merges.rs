//! Merging a new index before it is published (05 §5.5.1, 08 §8.4.1).
//!
//! Every cold search opens each split of the indexes it names, so a sealed
//! index should be a few large splits, not the hundreds the writer cuts
//! while it ingests. The writer node merges them in the background; the
//! release waits for it in two steps before it publishes:
//!
//! 1. **Settle:** with the index still open, until the merge planner has
//!    nothing running or queued and the split list has stopped changing.
//! 2. **Finalize:** the index's ingest source is disabled, which shuts its
//!    merge pipeline down; on the way out, the merge policy merges the small
//!    splits that are left (`max_finalize_merge_operations`). The release
//!    waits for the pipeline to finish and the splits to settle again.
//!
//! Quickwit 0.9.1 doesn't restart a merge pipeline that fails once it has
//! been told to stop, and its uploader can fail a merge: to move the merged
//! split into its local cache, it measures the merge's scratch folder while
//! the merge executor deletes the input splits it downloaded there, and a
//! folder that disappears between the listing and its size fails the upload
//! ("No such file or directory"). The pipeline then never finishes its final
//! merges and the splits never change (issue #84). The release watches the writer's output for that, and for a
//! node that stays idle without finishing, and runs the final merges again:
//! it re-enables the source, sends one document the index's strict mapping
//! rejects (so a source whose shards were closed and deleted gets a new,
//! empty one and Quickwit starts its pipelines), waits for the new merge
//! pipeline, and disables the source again. A new pipeline reads the index's
//! unmerged splits from the metastore. After three tries the release fails.
//!
//! The wait is bounded, and the result is checked: the splits must hold
//! exactly the documents sent, and no more of them than the merge policy
//! leaves. The writer's output is watched for a full disk and for failed
//! merges; either stops the release, so nothing is published half merged.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Serialize;
use serde_json::Value;

use crate::activity::Reporter;
use crate::progress;
use crate::sink::INDEX_TEMPLATE;
use crate::state::MergeProgress;

/// The ingest API's source in every index (Quickwit's ingest v2).
pub const INGEST_SOURCE: &str = "_ingest-source";

/// Splits listed per request.
const PAGE: usize = 1000;

/// A setting from the index template's `indexing_settings`, e.g.
/// `split_num_docs_target`: the merge checks read the policy the indexes
/// are created with, so the two can't drift apart.
pub fn template_setting(key: &str) -> Option<u64> {
    INDEX_TEMPLATE.lines().find_map(|line| {
        let (k, v) = line.trim().split_once(':')?;
        (k == key).then(|| v.trim().parse().ok()).flatten()
    })
}

/// The most splits a fully merged index of `docs` documents can have.
/// Splits reach `split_num_docs_target` documents and stop merging, so there
/// are at most `docs / target` of those. Below that size, every split left
/// after the planner settles is in a final merge (at most
/// `max_finalize_merge_operations`) except fewer than three, the smallest
/// final merge.
pub fn max_splits(docs: u64) -> u64 {
    let target = template_setting("split_num_docs_target").unwrap_or(10_000_000);
    let finals = template_setting("max_finalize_merge_operations").unwrap_or(0);
    docs / target.max(1) + finals + 2
}

/// The most splits a fully merged index can have when its splits are
/// partitioned (05 §5.5.5): Quickwit merges only splits of one partition,
/// and runs its final merges in each, so every partition can be left with
/// its own [`max_splits`]. `docs` are the documents of each partition.
pub fn max_partitioned_splits(docs: impl IntoIterator<Item = u64>) -> u64 {
    docs.into_iter().map(max_splits).sum()
}

/// What the writer node's output has said about the release so far: read
/// line by line as the node writes it (`sink::forward`).
#[derive(Debug, Default)]
pub struct NodeEvents(Mutex<Events>);

#[derive(Debug, Default)]
struct Events {
    /// Why the writer process ended, once it has.
    exited: Option<String>,
    disk_full: Option<String>,
    merge_failed: Option<String>,
    /// Each index's `_ingest-source` merge pipeline, by index id.
    pipelines: BTreeMap<String, Pipeline>,
}

/// What the writer's output has said about one index's `_ingest-source`
/// merge pipeline. The flags are about its current run: Quickwit logs
/// "spawning merge pipeline" for a new pipeline and for each restart.
#[derive(Debug, Default)]
struct Pipeline {
    spawns: u32,
    /// The indexing service told it to run its final merges and stop.
    stopping: bool,
    /// It failed: the line. Quickwit restarts it unless it was `stopping`.
    failed: Option<String>,
    /// It ran its final merges and exited.
    completed: bool,
}

/// Where an index's final merges stand, from the writer's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalMerges {
    /// Not asked for yet: the pipeline runs, or Quickwit will restart it.
    Pending,
    /// The pipeline was told to stop and is merging what is left.
    Running,
    /// The pipeline ran its final merges and exited.
    Done,
    /// The pipeline failed after it was told to stop: Quickwit won't restart
    /// it, so nothing more will merge. The writer's line.
    Stopped(String),
}

/// What a writer line says about an `_ingest-source` merge pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PipelineEvent {
    Spawned,
    Stopping,
    Failed,
    Completed,
}

impl PipelineEvent {
    fn of(line: &str) -> Option<Self> {
        if !line.contains(&format!("source_id={INGEST_SOURCE}")) {
            return None;
        }
        [
            ("spawning merge pipeline", Self::Spawned),
            ("shutting down orphan merge pipeline", Self::Stopping),
            ("merge pipeline failed", Self::Failed),
            ("merge pipeline completed successfully", Self::Completed),
        ]
        .into_iter()
        .find_map(|(text, event)| line.contains(text).then_some(event))
    }
}

/// The first 300 characters of a line, for an error message.
fn excerpt(line: &str) -> String {
    line.chars().take(300).collect()
}

impl NodeEvents {
    pub fn observe(&self, line: &str) {
        let disk_full = line.contains("No space left on device") || line.contains("os error 28");
        let merge_failed = line.contains("failed to merge splits")
            || line.contains("merge scheduler service is dead");
        let event = PipelineEvent::of(line);
        if !(disk_full || merge_failed || event.is_some()) {
            return;
        }
        let mut e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if disk_full && e.disk_full.is_none() {
            e.disk_full = Some(excerpt(line));
        }
        if merge_failed && e.merge_failed.is_none() {
            e.merge_failed = Some(excerpt(line));
        }
        // `index_uid=pages-delta-20261008-1:01M3…`: the field, not the span
        // (`spawn_merge_pipeline{index_uid=…`).
        let id = line
            .split_whitespace()
            .find_map(|w| w.strip_prefix("index_uid="))
            .and_then(|uid| uid.split(':').next());
        if let (Some(event), Some(id)) = (event, id) {
            let p = e.pipelines.entry(id.to_owned()).or_default();
            match event {
                PipelineEvent::Spawned => {
                    *p = Pipeline {
                        spawns: p.spawns + 1,
                        ..Pipeline::default()
                    }
                }
                PipelineEvent::Stopping => p.stopping = true,
                PipelineEvent::Failed => p.failed = Some(excerpt(line)),
                PipelineEvent::Completed => p.completed = true,
            }
        }
    }

    /// Record that the writer process has ended, and why (`sink::describe_exit`).
    pub fn writer_exited(&self, why: &str) {
        let mut e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        e.exited.get_or_insert_with(|| why.to_owned());
    }

    /// Why the writer process ended, if it has.
    pub fn exit(&self) -> Option<String> {
        let e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        e.exited.clone()
    }

    /// Fails once the writer has exited, run out of disk, or failed a merge.
    pub fn check(&self) -> anyhow::Result<()> {
        let e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(why) = &e.exited {
            bail!("{why}; not publishing");
        }
        if let Some(line) = &e.disk_full {
            bail!("the Quickwit writer ran out of disk; not publishing. Its output: {line}");
        }
        if let Some(line) = &e.merge_failed {
            bail!(
                "a Quickwit merge failed; not publishing a half-merged index. Its output: {line}"
            );
        }
        Ok(())
    }

    /// `err`, from a request to the writer, with the reason the writer
    /// exited in front if it has (or does within `grace`): a writer the
    /// kernel killed shows up first as "connection refused", and the exit
    /// status takes a moment to be reaped.
    pub async fn explain(&self, err: anyhow::Error, grace: Duration) -> anyhow::Error {
        let until = tokio::time::Instant::now() + grace;
        loop {
            if let Some(why) = self.exit() {
                return err.context(why);
            }
            if tokio::time::Instant::now() >= until {
                return err;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Where `index_id`'s final merges stand.
    pub fn final_merges(&self, index_id: &str) -> FinalMerges {
        let e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match e.pipelines.get(index_id) {
            Some(p) if p.completed => FinalMerges::Done,
            Some(Pipeline {
                stopping: true,
                failed: Some(line),
                ..
            }) => FinalMerges::Stopped(line.clone()),
            Some(p) if p.stopping => FinalMerges::Running,
            _ => FinalMerges::Pending,
        }
    }

    /// How many times Quickwit has started `index_id`'s merge pipeline.
    pub fn spawns(&self, index_id: &str) -> u32 {
        let e = self.0.lock().unwrap_or_else(|e| e.into_inner());
        e.pipelines.get(index_id).map_or(0, |p| p.spawns)
    }
}

/// `(running, queued)` merge operations, from the node's Prometheus metrics.
/// Quickwit registers the gauges with the first merge, so a missing one is 0.
pub fn merge_gauges(metrics: &str) -> (u64, u64) {
    let gauge = |name: &str| {
        metrics
            .lines()
            .find_map(|line| {
                let value = line.strip_prefix(name)?.strip_prefix(' ')?;
                let v: f64 = value.trim().parse().ok()?;
                (v >= 0.0).then_some(v as u64)
            })
            .unwrap_or(0)
    };
    (
        gauge("quickwit_indexing_ongoing_merge_operations"),
        gauge("quickwit_indexing_pending_merge_operations"),
    )
}

/// How long a failed request to the writer waits for its exit to be reaped.
pub const EXIT_GRACE: Duration = Duration::from_secs(5);

/// `r`, with the writer's exit in front of an error if it has exited.
pub async fn explained<T>(events: Option<&NodeEvents>, r: anyhow::Result<T>) -> anyhow::Result<T> {
    match (r, events) {
        (Err(e), Some(ev)) => Err(ev.explain(e, EXIT_GRACE).await),
        (r, _) => r,
    }
}

/// Decides when merging has settled: no merge running or queued, and the
/// same splits, for `needed` polls in a row.
#[derive(Debug)]
pub struct Settle {
    needed: u32,
    stable: u32,
    last: Option<Vec<String>>,
}

impl Settle {
    pub fn new(needed: u32) -> Self {
        Self {
            needed: needed.max(1),
            stable: 0,
            last: None,
        }
    }

    /// Record one poll; `true` once settled. `split_ids` must be sorted.
    pub fn observe(&mut self, idle: bool, split_ids: &[String]) -> bool {
        let same = self.last.as_deref() == Some(split_ids);
        self.stable = if idle && same { self.stable + 1 } else { 0 };
        if !same {
            self.last = Some(split_ids.to_vec());
        }
        self.stable >= self.needed
    }
}

/// One published split.
#[derive(Debug, Clone, PartialEq)]
pub struct Split {
    pub id: String,
    pub docs: u64,
    pub bytes: u64,
    /// The size of its footer (the hotcache and the file list), which a
    /// searcher reads first and keeps in `split_footer_cache_capacity`;
    /// `None` if the metastore doesn't say.
    pub footer_bytes: Option<u64>,
    /// Quickwit's partition: 0 unless the index has a `partition_key`
    /// (05 §5.5.5).
    pub partition_id: u64,
    /// The decades its pages are in, from its `decade:` tags; empty for an
    /// index without them.
    pub decades: Vec<u16>,
}

/// An index's published splits, as the release log and manifest report them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexLayout {
    pub index_id: String,
    pub splits: u64,
    pub docs: u64,
    /// The split files' total size.
    pub bytes: u64,
    pub smallest_split_docs: u64,
    pub largest_split_docs: u64,
    /// The splits' footers in all: what a searcher's split footer cache needs
    /// to hold them all (#125). Absent when a split doesn't give its footer,
    /// and in manifests written before releases recorded it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub footer_bytes: Option<u64>,
    /// The largest single footer; absent like `footer_bytes`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub largest_footer_bytes: Option<u64>,
    /// Splits per decade, for an index partitioned by decade (05 §5.5.5),
    /// where every split holds one; absent otherwise.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub splits_by_decade: BTreeMap<u16, u64>,
}

impl IndexLayout {
    pub fn of(index_id: &str, splits: &[Split]) -> Self {
        let footers: Option<Vec<u64>> = splits.iter().map(|s| s.footer_bytes).collect();
        Self {
            index_id: index_id.to_owned(),
            splits: splits.len() as u64,
            docs: splits.iter().map(|s| s.docs).sum(),
            bytes: splits.iter().map(|s| s.bytes).sum(),
            smallest_split_docs: splits.iter().map(|s| s.docs).min().unwrap_or(0),
            largest_split_docs: splits.iter().map(|s| s.docs).max().unwrap_or(0),
            footer_bytes: footers.as_ref().map(|f| f.iter().sum()),
            largest_footer_bytes: footers
                .as_ref()
                .map(|f| f.iter().copied().max().unwrap_or(0)),
            // Only a partitioned index (a partition key, so nonzero
            // partition ids): a tagged delta's split can also hold one decade.
            splits_by_decade: if splits
                .iter()
                .all(|s| s.partition_id != 0 && s.decades.len() == 1)
            {
                let mut by = BTreeMap::new();
                for s in splits {
                    *by.entry(s.decades[0]).or_default() += 1;
                }
                by
            } else {
                BTreeMap::new()
            },
        }
    }

    /// One "index layout" line.
    pub fn log(&self) {
        tracing::info!(
            index = %self.index_id,
            splits = self.splits,
            docs = self.docs,
            mb = (self.bytes as f64 / (1024.0 * 1024.0)).round() as u64,
            smallest_split_docs = self.smallest_split_docs,
            largest_split_docs = self.largest_split_docs,
            footer_bytes = self.footer_bytes,
            largest_footer_bytes = self.largest_footer_bytes,
            splits_by_decade = ?self.splits_by_decade,
            "index layout"
        );
    }
}

/// The splits in one page of `GET /api/v1/indexes/{id}/splits`.
fn parse_splits(v: &Value) -> anyhow::Result<Vec<Split>> {
    v["splits"]
        .as_array()
        .context("the split list has no `splits`")?
        .iter()
        .map(|s| {
            Ok(Split {
                id: s["split_id"]
                    .as_str()
                    .context("a split has no id")?
                    .to_owned(),
                docs: s["num_docs"].as_u64().context("a split has no num_docs")?,
                // The footer ends the split file.
                bytes: s["footer_offsets"]["end"].as_u64().unwrap_or(0),
                footer_bytes: footer_bytes(&s["footer_offsets"]),
                partition_id: s["partition_id"].as_u64().unwrap_or(0),
                decades: decade_tags(&s["tags"]),
            })
        })
        .collect()
}

/// The decades in a split's tags (`decade:1890`), sorted.
fn decade_tags(tags: &Value) -> Vec<u16> {
    let mut decades: Vec<u16> = tags
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t.as_str()?.strip_prefix("decade:")?.parse().ok())
        .collect();
    decades.sort_unstable();
    decades.dedup();
    decades
}

/// A split's footer size from its `footer_offsets` (`{"start", "end"}`,
/// byte offsets in the split file).
fn footer_bytes(offsets: &Value) -> Option<u64> {
    offsets["end"]
        .as_u64()?
        .checked_sub(offsets["start"].as_u64()?)
}

/// The writer node's REST API, for merges and splits.
pub struct Node<'a> {
    pub http: &'a reqwest::Client,
    pub base: &'a str,
}

impl Node<'_> {
    /// Every published split of `index_id`, sorted by id.
    pub async fn splits(&self, index_id: &str) -> anyhow::Result<Vec<Split>> {
        let mut all = Vec::new();
        loop {
            let v: Value = self
                .http
                .get(format!("{}/api/v1/indexes/{index_id}/splits", self.base))
                .query(&[
                    ("split_states", "Published"),
                    ("offset", &all.len().to_string()),
                    ("limit", &PAGE.to_string()),
                ])
                .send()
                .await?
                .error_for_status()
                .with_context(|| format!("listing the splits of `{index_id}`"))?
                .json()
                .await?;
            let page = parse_splits(&v)?;
            let n = page.len();
            all.extend(page);
            if n < PAGE {
                break;
            }
        }
        all.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(all)
    }

    async fn gauges(&self) -> anyhow::Result<(u64, u64)> {
        let text = self
            .http
            .get(format!("{}/metrics", self.base))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(merge_gauges(&text))
    }

    /// Stop ingesting into `index_id`, which runs its final merges.
    pub async fn close(&self, index_id: &str) -> anyhow::Result<()> {
        self.toggle(index_id, false).await
    }

    /// Enable `index_id`'s ingest source again and have Quickwit start its
    /// pipelines, without adding a document: a new merge pipeline reads the
    /// index's unmerged splits from the metastore.
    ///
    /// Quickwit starts an ingest source's pipelines only while it has shards,
    /// and deletes them once they have been idle for 15 minutes and indexed.
    /// A document the index's strict mapping rejects opens a shard (an empty
    /// one) and is not written.
    async fn reopen(&self, index_id: &str) -> anyhow::Result<()> {
        self.toggle(index_id, true).await?;
        let v: Value = self
            .http
            .post(format!("{}/api/v1/{index_id}/ingest", self.base))
            .body(format!("{{\"{REOPEN_FIELD}\": true}}\n"))
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("reopening `{index_id}`"))?
            .json()
            .await?;
        let ingested = v["num_ingested_docs"].as_u64();
        if ingested != Some(0) || v["num_rejected_docs"].as_u64() != Some(1) {
            bail!("reopening `{index_id}`: the writer did not reject the empty document: {v}");
        }
        Ok(())
    }

    async fn toggle(&self, index_id: &str, enable: bool) -> anyhow::Result<()> {
        let resp = self
            .http
            .put(format!(
                "{}/api/v1/indexes/{index_id}/sources/{INGEST_SOURCE}/toggle",
                self.base
            ))
            .json(&serde_json::json!({ "enable": enable }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "{} ingest into `{index_id}` returned {status}: {}",
                if enable { "enabling" } else { "disabling" },
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(())
    }
}

/// A field no index mapping has: a document with only this field is
/// rejected (`mode: strict`), which `Node::reopen` relies on.
const REOPEN_FIELD: &str = "usnm_reopen";

/// Tries at the final merges before the release gives up.
pub const FINAL_MERGE_TRIES: u32 = 3;

/// How the release waits for merges.
#[derive(Debug, Clone)]
pub struct MergeWait {
    /// Between polls of the node's metrics and split list.
    pub poll: Duration,
    /// Polls in a row with nothing changing before merging counts as settled.
    pub stable_polls: u32,
    /// For both steps together; past it the release fails.
    pub timeout: Duration,
    /// Without the node's output to say the final merges are done, how long
    /// to wait after disabling the source before trusting a quiet node.
    pub finalize_grace: Duration,
    /// With it: how long the node may stay idle after the source is disabled
    /// without the final merges finishing before they are run again, and how
    /// long a reopened index may take to start its pipeline. Quickwit asks a
    /// pipeline to stop within 30 s (its actor heartbeat), and its merges
    /// show as running.
    pub finalize_stall: Duration,
    /// Where the wait's step and counts are recorded for the status page
    /// (`ops/activity`).
    pub report: Reporter,
}

impl Default for MergeWait {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(10),
            stable_polls: 3,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            finalize_grace: Duration::from_secs(60),
            finalize_stall: Duration::from_secs(600),
            report: Reporter::off(),
        }
    }
}

/// 4 hours: the merges left when a full rebuild of the corpus stops
/// ingesting (about 1.7 hours for 23.8M pages), with room to spare (08 §8.4).
pub const DEFAULT_TIMEOUT_SECS: u64 = 14400;

/// Lines of "release merges" progress, at most this often.
const LOG_EVERY: Duration = Duration::from_secs(30);

/// Wait until `index_id` is merged as far as its merge policy goes, close
/// it, and check the result (see the module docs). `expected` is the number
/// of documents sent; `work_dir` is where the writer keeps its data. Nothing
/// counts as sealed after `wait.timeout`, and a request to the writer that
/// hangs past it (plus one poll) is abandoned.
pub async fn seal(
    node: &Node<'_>,
    index_id: &str,
    expected: u64,
    events: Option<&NodeEvents>,
    wait: &MergeWait,
    work_dir: Option<&Path>,
) -> anyhow::Result<IndexLayout> {
    let closed = AtomicBool::new(false);
    let polls = poll_until_sealed(node, index_id, expected, events, wait, work_dir, &closed);
    match tokio::time::timeout(wait.timeout + wait.poll, polls).await {
        Ok(result) => result,
        Err(_) => {
            // A rerun of the final merges may have reopened the index: close
            // it again before the caller stops the writer.
            let reclosed = if closed.load(Ordering::SeqCst) {
                match tokio::time::timeout(CLOSE_TIMEOUT, node.close(index_id)).await {
                    Ok(Ok(())) => String::new(),
                    Ok(Err(e)) => format!(", and closing it again failed: {e:#}"),
                    Err(_) => ", and closing it again got no answer".to_owned(),
                }
            } else {
                String::new()
            };
            bail!(
                "merges into `{index_id}` did not finish within {} s (a request to the writer was \
                 still waiting){reclosed}; not publishing",
                wait.timeout.as_secs()
            )
        }
    }
}

async fn poll_until_sealed(
    node: &Node<'_>,
    index_id: &str,
    expected: u64,
    events: Option<&NodeEvents>,
    wait: &MergeWait,
    work_dir: Option<&Path>,
    closed: &AtomicBool,
) -> anyhow::Result<IndexLayout> {
    let start = tokio::time::Instant::now();
    let deadline = start + wait.timeout;
    let mut last_log: Option<tokio::time::Instant> = None;
    let mut step = "settle";
    let mut closed_at: Option<tokio::time::Instant> = None;
    let mut settle = Settle::new(wait.stable_polls);
    let mut tries = 0;
    loop {
        if let Some(e) = events {
            e.check()?;
        }
        let (running, queued) = explained(events, node.gauges().await).await?;
        let splits = explained(events, node.splits(index_id).await).await?;
        let ids: Vec<String> = splits.iter().map(|s| s.id.clone()).collect();
        let settled = settle.observe(running == 0 && queued == 0, &ids);
        wait.report.merges(MergeProgress {
            step: step.to_owned(),
            splits: splits.len() as u64,
            merges_running: running,
            merges_queued: queued,
        });
        if last_log.is_none_or(|t| t.elapsed() >= LOG_EVERY) {
            last_log = Some(tokio::time::Instant::now());
            tracing::info!(
                index = index_id,
                step,
                splits = splits.len(),
                merges_running = running,
                merges_queued = queued,
                elapsed_secs = start.elapsed().as_secs(),
                disk_free_mb = work_dir
                    .and_then(progress::disk_free)
                    .map(|b| b / (1024 * 1024)),
                "release merges"
            );
        }
        match closed_at {
            None if settled => {
                tracing::info!(
                    index = index_id,
                    splits = splits.len(),
                    "merges settled; closing the index for its final merges"
                );
                closed.store(true, Ordering::SeqCst);
                explained(events, node.close(index_id).await).await?;
                closed_at = Some(tokio::time::Instant::now());
                tries = 1;
                step = "finalize";
                settle = Settle::new(wait.stable_polls);
            }
            Some(at) if settled => {
                // An index that was never written has no ingest-source
                // pipeline, so no "completed" line will come; there is
                // nothing for it to merge.
                let state = match events {
                    _ if expected == 0 => FinalMerges::Done,
                    Some(e) => e.final_merges(index_id),
                    None if at.elapsed() >= wait.finalize_grace => FinalMerges::Done,
                    None => FinalMerges::Pending,
                };
                let why = match state {
                    FinalMerges::Done if tokio::time::Instant::now() <= deadline => {
                        return check(index_id, expected, &splits);
                    }
                    FinalMerges::Done => None,
                    FinalMerges::Stopped(line) => {
                        Some(format!("its merge pipeline failed: {line}"))
                    }
                    // Idle all this time with the source disabled: whatever
                    // happened, nothing is merging and nothing will.
                    _ if events.is_some() && at.elapsed() >= wait.finalize_stall => Some(format!(
                        "the writer stayed idle for {} s without finishing them",
                        at.elapsed().as_secs()
                    )),
                    _ => None,
                };
                if let (Some(why), Some(e)) = (why, events) {
                    if tries >= FINAL_MERGE_TRIES {
                        bail!(
                            "the final merges of `{index_id}` did not finish in {tries} tries ({why}); \
                             not publishing a half-merged index"
                        );
                    }
                    tracing::warn!(
                        index = index_id,
                        splits = splits.len(),
                        attempt = tries + 1,
                        "the final merges stopped ({why}); reopening the index to run them again"
                    );
                    rerun_final_merges(node, index_id, e, wait, deadline).await?;
                    closed_at = Some(tokio::time::Instant::now());
                    tries += 1;
                    settle = Settle::new(wait.stable_polls);
                }
            }
            _ => {}
        }
        if tokio::time::Instant::now() + wait.poll > deadline {
            bail!(
                "merges into `{index_id}` did not finish within {} s ({step}: {} splits, {running} \
                 merges running, {queued} queued); not publishing",
                wait.timeout.as_secs(),
                splits.len()
            );
        }
        tokio::time::sleep(wait.poll).await;
    }
}

/// Reopen `index_id`, wait for Quickwit to start a new merge pipeline for
/// it, and close it again: the new pipeline runs the final merges. `seal`
/// does this when they stop; public for the tests against a real writer.
pub async fn rerun_final_merges(
    node: &Node<'_>,
    index_id: &str,
    events: &NodeEvents,
    wait: &MergeWait,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let spawns = events.spawns(index_id);
    let started = tokio::time::Instant::now();
    let until = deadline.min(started + wait.finalize_stall);
    let no_pipeline = || {
        anyhow::anyhow!(
            "reopened `{index_id}` to run its final merges again, but the writer started no \
             merge pipeline for it within {} s; not publishing a half-merged index",
            started.elapsed().as_secs()
        )
    };
    // Closed again however this ends, so a later writer run starts nothing
    // for it: below, or by the guard if `seal` gives up (and drops this)
    // while a request is still waiting.
    let mut guard = CloseOnDrop::new(node, index_id);
    let reopened = tokio::time::timeout_at(until, async {
        explained(Some(events), node.reopen(index_id).await).await?;
        while events.spawns(index_id) == spawns {
            events.check()?;
            if tokio::time::Instant::now() + wait.poll > until {
                return Err(no_pipeline());
            }
            tokio::time::sleep(wait.poll).await;
        }
        Ok(())
    })
    .await
    .unwrap_or_else(|_| Err(no_pipeline()));
    let closed = match tokio::time::timeout(CLOSE_TIMEOUT, node.close(index_id)).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "the writer did not answer within {} s",
            CLOSE_TIMEOUT.as_secs()
        )),
    };
    guard.armed = closed.is_err();
    match (reopened, closed) {
        (Err(e), Err(c)) => Err(anyhow::anyhow!(
            "{e:#}; closing it again also failed: {c:#}"
        )),
        (Err(e), _) => Err(e),
        (Ok(()), closed) => explained(Some(events), closed).await,
    }
}

/// How long disabling a reopened index's source may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Disables an index's ingest source when dropped while armed, from a task
/// of its own: `rerun_final_merges` reopens the index, and the release must
/// not leave it open if `seal` gives up on it halfway.
struct CloseOnDrop {
    http: reqwest::Client,
    base: String,
    index_id: String,
    armed: bool,
}

impl CloseOnDrop {
    fn new(node: &Node<'_>, index_id: &str) -> Self {
        Self {
            http: node.http.clone(),
            base: node.base.to_owned(),
            index_id: index_id.to_owned(),
            armed: true,
        }
    }
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if !self.armed {
            return;
        }
        let (http, base, index_id) = (
            self.http.clone(),
            std::mem::take(&mut self.base),
            std::mem::take(&mut self.index_id),
        );
        rt.spawn(async move {
            let node = Node {
                http: &http,
                base: &base,
            };
            match tokio::time::timeout(CLOSE_TIMEOUT, node.close(&index_id)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(index = %index_id, "could not close the reopened index: {e:#}")
                }
                Err(_) => tracing::warn!(
                    index = %index_id,
                    "could not close the reopened index: the writer did not answer"
                ),
            }
        });
    }
}

/// The merged index holds every document, in no more splits than the
/// merge policy leaves.
fn check(index_id: &str, expected: u64, splits: &[Split]) -> anyhow::Result<IndexLayout> {
    let layout = IndexLayout::of(index_id, splits);
    if layout.docs != expected {
        bail!(
            "after merging, `{index_id}` holds {} documents in {} splits, expected {expected}",
            layout.docs,
            layout.splits
        );
    }
    let mut partitions: BTreeMap<u64, u64> = BTreeMap::new();
    for s in splits {
        *partitions.entry(s.partition_id).or_default() += s.docs;
    }
    let ceiling = max_partitioned_splits(partitions.values().copied());
    if layout.splits > ceiling {
        let across = match partitions.len() {
            0 | 1 => String::new(),
            n => format!(" in {n} partitions"),
        };
        bail!(
            "merging left `{index_id}` in {} splits; its merge policy leaves at most {ceiling} \
             for {expected} documents{across}. Not publishing a half-merged index",
            layout.splits
        );
    }
    Ok(layout)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    use super::*;

    #[test]
    fn the_template_sets_the_merge_policy_the_checks_assume() {
        assert_eq!(template_setting("split_num_docs_target"), Some(30_000));
        assert_eq!(template_setting("max_finalize_merge_operations"), Some(5));
        assert!(INDEX_TEMPLATE.contains("type: limit_merge"));
        assert_eq!(template_setting("no_such_setting"), None);
        // 274,480 documents: nine full splits, up to five final merges, two left over.
        assert_eq!(max_splits(274_480), 9 + 5 + 2);
        assert_eq!(max_splits(51_764), 1 + 7);
        // The whole corpus: about 800 splits.
        assert_eq!(max_splits(23_800_000), 793 + 7);
    }

    #[test]
    fn reads_the_merge_gauges() {
        let metrics = "# HELP quickwit_indexing_ongoing_merge_operations Number of ongoing merge operations\n\
                       # TYPE quickwit_indexing_ongoing_merge_operations gauge\n\
                       quickwit_indexing_ongoing_merge_operations 1\n\
                       quickwit_indexing_pending_merge_operations 3\n\
                       quickwit_indexing_pending_merge_bytes 12345\n";
        assert_eq!(merge_gauges(metrics), (1, 3));
        assert_eq!(
            merge_gauges(
                "quickwit_indexing_pending_merge_operations 2\nquickwit_indexing_ongoing_merge_operations 0.0\n"
            ),
            (0, 2)
        );
        // A labelled series isn't the total; before the first merge there
        // are no gauges at all.
        assert_eq!(
            merge_gauges("quickwit_indexing_ongoing_merge_operations{x=\"y\"} 2\n"),
            (0, 0)
        );
        assert_eq!(merge_gauges(""), (0, 0));
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn settles_after_enough_quiet_polls() {
        let mut s = Settle::new(3);
        let a = ids(&["a", "b", "c"]);
        let merged = ids(&["d"]);
        assert!(!s.observe(true, &a)); // first sight: nothing to compare yet
        assert!(!s.observe(false, &a)); // a merge started
        assert!(!s.observe(true, &merged)); // it finished: the splits changed
        assert!(!s.observe(true, &merged));
        assert!(!s.observe(true, &merged));
        assert!(s.observe(true, &merged));
        // A change starts the count again.
        assert!(!s.observe(true, &a));
    }

    #[test]
    fn watches_the_writer_output() {
        let e = NodeEvents::default();
        e.observe("2026-09-29T09:09:18Z  WARN quickwit_config: peer seeds are empty");
        assert!(e.check().is_ok());
        assert_eq!(
            e.final_merges("pages-delta-20261008-1"),
            FinalMerges::Pending
        );

        e.observe(
            "2026-10-02T06:59:13Z  INFO quickwit_indexing::actors::merge_pipeline: merge pipeline \
             completed successfully index_uid=pages-delta-20261008-1:01M3XP9DEKWZK8TW8TXB27ZM7Y \
             source_id=_ingest-source generation=1",
        );
        // The ingest API v1 source's pipeline is another one.
        e.observe(
            "2026-10-02T06:59:13Z  INFO quickwit_indexing::actors::merge_pipeline: merge pipeline \
             completed successfully index_uid=pages-base-20261001-1:01M3XP9DEKWZK8TW8TXB27ZM7Y \
             source_id=_ingest-api-source generation=1",
        );
        assert_eq!(e.final_merges("pages-delta-20261008-1"), FinalMerges::Done);
        assert_eq!(
            e.final_merges("pages-base-20261001-1"),
            FinalMerges::Pending
        );
        assert!(e.check().is_ok());

        e.observe(
            "2026-09-29T09:27:26Z ERROR quickwit_actors::spawn_builder: actor-exit \
             exit_status=Failure(No space left on device (os error 28) at path \"/work/quickwit/qwdata/indexing\")",
        );
        let err = e.check().unwrap_err().to_string();
        assert!(err.contains("ran out of disk"), "{err}");
        assert!(err.contains("os error 28"), "{err}");
    }

    /// The writer's lines from CI run 37088490670 (issue #84): the final
    /// merge's upload failed, and Quickwit left the pipeline stopped.
    #[test]
    fn follows_a_merge_pipeline_through_failures_and_restarts() {
        let id = "pages-delta-20261015-1";
        let e = NodeEvents::default();
        let spawned = "2026-10-03T02:06:57.809Z  INFO spawn_merge_pipeline{index_uid=pages-delta-20261015-1:01M3ZR7CS2DXSCJPAT30QTW46Q generation=0}: \
                       quickwit_indexing::actors::merge_pipeline: spawning merge pipeline \
                       index_uid=pages-delta-20261015-1:01M3ZR7CS2DXSCJPAT30QTW46Q source_id=_ingest-source root_dir=/tmp/x";
        let stopping = "2026-10-03T02:07:21.807Z  INFO quickwit_indexing::actors::indexing_service: shutting down \
                        orphan merge pipeline index_uid=pages-delta-20261015-1:01M3ZR7CS2DXSCJPAT30QTW46Q source_id=_ingest-source";
        let failed = "2026-10-03T02:07:22.836Z ERROR quickwit_indexing::actors::merge_pipeline: merge pipeline failed \
                      index_uid=pages-delta-20261015-1:01M3ZR7CS2DXSCJPAT30QTW46Q source_id=_ingest-source generation=1 \
                      healthy_actors=[] failed_or_unhealthy_actors=[\"MergePublisher-young-tVo8\"]";
        let completed = "2026-10-03T02:09:19.983Z  INFO quickwit_indexing::actors::merge_pipeline: merge pipeline \
                         completed successfully index_uid=pages-delta-20261015-1:01M3ZR7CS2DXSCJPAT30QTW46Q \
                         source_id=_ingest-source generation=1";
        e.observe(spawned);
        assert_eq!(
            (e.final_merges(id), e.spawns(id)),
            (FinalMerges::Pending, 1)
        );
        // A failure while it runs: Quickwit restarts it after a heartbeat.
        e.observe(failed);
        assert_eq!(e.final_merges(id), FinalMerges::Pending);
        e.observe(spawned);
        assert_eq!(
            (e.final_merges(id), e.spawns(id)),
            (FinalMerges::Pending, 2)
        );
        e.observe(stopping);
        assert_eq!(e.final_merges(id), FinalMerges::Running);
        // A failure once it was told to stop: nothing restarts it.
        e.observe(failed);
        match e.final_merges(id) {
            FinalMerges::Stopped(line) => assert!(line.contains("MergePublisher"), "{line}"),
            other => panic!("{other:?}"),
        }
        // Told to stop while it waited to be restarted: the same.
        let e2 = NodeEvents::default();
        e2.observe(spawned);
        e2.observe(failed);
        e2.observe(stopping);
        assert!(matches!(e2.final_merges(id), FinalMerges::Stopped(_)));
        // A new pipeline starts over; "failed" is not a failed merge.
        e.observe(spawned);
        assert_eq!(
            (e.final_merges(id), e.spawns(id)),
            (FinalMerges::Pending, 3)
        );
        e.observe(stopping);
        e.observe(completed);
        assert_eq!(e.final_merges(id), FinalMerges::Done);
        assert!(e.check().is_ok());
        // Another source's pipeline is not this one.
        e.observe(&spawned.replace("_ingest-source", "_ingest-api-source"));
        assert_eq!(e.spawns(id), 3);
    }

    #[test]
    fn a_failed_merge_stops_the_release() {
        let e = NodeEvents::default();
        e.observe(
            "2026-09-29T14:03:59Z ERROR merge{merge_split_id=01M3PQ9H}:merge_executor: \
             quickwit_indexing::actors::merge_executor: failed to merge splits task=Merge(…)",
        );
        let err = e.check().unwrap_err().to_string();
        assert!(err.contains("half-merged"), "{err}");
    }

    #[test]
    fn layout_and_checks() {
        let splits = vec![
            Split {
                id: "a".into(),
                docs: 1_000_000,
                bytes: 23_000_000_000,
                footer_bytes: Some(3_500_000),
                partition_id: 0,
                decades: vec![],
            },
            Split {
                id: "b".into(),
                docs: 40_000,
                bytes: 920_000_000,
                footer_bytes: Some(92_377),
                partition_id: 0,
                decades: vec![],
            },
        ];
        let l = check("idx", 1_040_000, &splits).unwrap();
        assert_eq!(
            l,
            IndexLayout {
                index_id: "idx".into(),
                splits: 2,
                docs: 1_040_000,
                bytes: 23_920_000_000,
                smallest_split_docs: 40_000,
                largest_split_docs: 1_000_000,
                footer_bytes: Some(3_592_377),
                largest_footer_bytes: Some(3_500_000),
                splits_by_decade: BTreeMap::new(),
            }
        );
        // In the log line and the manifest.
        assert_eq!(
            serde_json::to_value(&l).unwrap(),
            serde_json::json!({
                "index_id": "idx", "splits": 2, "docs": 1_040_000, "bytes": 23_920_000_000_u64,
                "smallest_split_docs": 40_000, "largest_split_docs": 1_000_000,
                "footer_bytes": 3_592_377, "largest_footer_bytes": 3_500_000,
            })
        );
        // A split that doesn't give its footer: no footer total, rather than
        // one too small. The manifest leaves the fields out.
        let mut unknown = splits.clone();
        unknown[1].footer_bytes = None;
        let l = IndexLayout::of("idx", &unknown);
        assert_eq!((l.footer_bytes, l.largest_footer_bytes), (None, None));
        let v = serde_json::to_value(&l).unwrap();
        assert!(v.get("footer_bytes").is_none() && v.get("largest_footer_bytes").is_none());
        let empty = IndexLayout::of("empty", &[]);
        assert_eq!(
            (empty.footer_bytes, empty.largest_footer_bytes),
            (Some(0), Some(0))
        );
        let err = check("idx", 1_040_001, &splits).unwrap_err().to_string();
        assert!(err.contains("expected 1040001"), "{err}");

        let many: Vec<Split> = (0..20)
            .map(|i| Split {
                id: format!("s{i:02}"),
                docs: 7_000,
                bytes: 1,
                footer_bytes: None,
                partition_id: 0,
                decades: vec![],
            })
            .collect();
        let err = check("idx", 140_000, &many).unwrap_err().to_string();
        assert!(err.contains("at most 11"), "{err}");
        assert_eq!(IndexLayout::of("empty", &[]).splits, 0);

        // Partitioned by decade (05 §5.5.5): each partition merges on its
        // own, so each may keep its own small splits.
        let decades: Vec<Split> = (0..20)
            .map(|i| Split {
                partition_id: 100 + i / 5,
                decades: vec![1860 + 10 * (i / 5) as u16],
                ..many[i as usize].clone()
            })
            .collect();
        let l = check("idx", 140_000, &decades).unwrap();
        assert_eq!(
            l.splits_by_decade,
            BTreeMap::from([(1860, 5), (1870, 5), (1880, 5), (1890, 5)])
        );
        assert_eq!(
            serde_json::to_value(&l).unwrap()["splits_by_decade"],
            serde_json::json!({"1860": 5, "1870": 5, "1880": 5, "1890": 5})
        );
        // 4 partitions of 35,000 pages: at most 4 × (1 + 5 + 2).
        assert_eq!(max_partitioned_splits([35_000; 4]), 32);
        assert_eq!(max_partitioned_splits([140_000]), max_splits(140_000));
        let mut crowded = decades.clone();
        for s in &mut crowded {
            s.partition_id = 7;
        }
        let err = check("idx", 140_000, &crowded).unwrap_err().to_string();
        assert!(
            err.contains("at most 11") && !err.contains("partitions"),
            "{err}"
        );
        let tiny: Vec<Split> = (0..40)
            .map(|i| Split {
                docs: 1,
                partition_id: i / 20,
                ..decades[0].clone()
            })
            .collect();
        let err = check("idx", 40, &tiny).unwrap_err().to_string();
        assert!(
            err.contains("at most 14") && err.contains("in 2 partitions"),
            "{err}"
        );
        // A split with several decades (a tagged delta) or none: no count.
        let mut mixed = decades.clone();
        mixed[0].decades = vec![1860, 1870];
        assert!(IndexLayout::of("idx", &mixed).splits_by_decade.is_empty());
        // A tagged delta whose splits each hold one decade isn't partitioned.
        let tagged: Vec<Split> = decades
            .iter()
            .map(|s| Split {
                partition_id: 0,
                ..s.clone()
            })
            .collect();
        assert!(IndexLayout::of("idx", &tagged).splits_by_decade.is_empty());
    }

    #[test]
    fn parses_a_split_list() {
        let v = serde_json::json!({"offset": 0, "size": 1, "splits": [{
            "split_state": "Published", "split_id": "01M3XPHH", "num_docs": 40000,
            "footer_offsets": {"start": 919982920, "end": 920075297}, "num_merge_ops": 1,
        }]});
        let tagged = serde_json::json!({"splits": [{
            "split_id": "t", "num_docs": 1, "partition_id": 3264326757911759461_u64,
            "tags": ["decade!", "decade:1900", "decade:1890", "lccn:x"],
        }]});
        let t = &parse_splits(&tagged).unwrap()[0];
        assert_eq!(
            (t.partition_id, t.decades.clone()),
            (3264326757911759461, vec![1890, 1900])
        );
        assert_eq!(
            parse_splits(&v).unwrap(),
            vec![Split {
                id: "01M3XPHH".into(),
                docs: 40000,
                bytes: 920_075_297,
                footer_bytes: Some(92_377),
                partition_id: 0,
                decades: vec![],
            }]
        );
        assert!(parse_splits(&serde_json::json!({"message": "index `x` not found"})).is_err());
        // Without offsets, or with ones that make no sense: no footer size.
        let v = serde_json::json!({"splits": [
            {"split_id": "a", "num_docs": 1},
            {"split_id": "b", "num_docs": 1, "footer_offsets": {"end": 10}},
            {"split_id": "c", "num_docs": 1, "footer_offsets": {"start": 11, "end": 10}},
        ]});
        assert!(parse_splits(&v)
            .unwrap()
            .iter()
            .all(|s| s.footer_bytes.is_none()));
    }

    /// A fake writer node: merges run for a few polls, then the planner
    /// settles; closing the index runs one final merge.
    #[derive(Default)]
    struct Fake {
        polls: AtomicU32,
        closed_at: Mutex<Option<u32>>,
    }

    impl Fake {
        fn now(&self) -> u32 {
            self.polls.load(Ordering::SeqCst)
        }
        fn phase(&self) -> (u64, Vec<(&'static str, u64)>) {
            let t = self.now();
            match *self.closed_at.lock().unwrap() {
                // Final merge: two polls running, then one split.
                Some(c) if t < c + 2 => (1, vec![("m1", 900), ("s9", 50), ("s10", 50)]),
                Some(_) => (0, vec![("f1", 1000)]),
                // Before closing: merging for three polls, then three splits.
                None if t < 3 => (1, vec![("s1", 400), ("s2", 500), ("s9", 50), ("s10", 50)]),
                None => (0, vec![("m1", 900), ("s9", 50), ("s10", 50)]),
            }
        }
    }

    async fn fake_node(finish_line: Option<Arc<NodeEvents>>) -> (String, Arc<Fake>) {
        use axum::routing::{get, put};
        let fake = Arc::new(Fake::default());
        let f1 = fake.clone();
        let f2 = fake.clone();
        let f3 = fake.clone();
        let app = axum::Router::new()
            .route(
                "/metrics",
                get(move || async move {
                    f1.polls.fetch_add(1, Ordering::SeqCst);
                    let (running, _) = f1.phase();
                    format!(
                        "quickwit_indexing_ongoing_merge_operations {running}\n\
                         quickwit_indexing_pending_merge_operations 0\n"
                    )
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(move || async move {
                    let (_, splits) = f2.phase();
                    axum::Json(serde_json::json!({"splits": splits.iter().map(|(id, docs)| {
                        serde_json::json!({"split_id": id, "num_docs": docs, "footer_offsets": {"end": docs * 10}})
                    }).collect::<Vec<_>>()}))
                }),
            )
            .route(
                "/api/v1/indexes/{id}/sources/_ingest-source/toggle",
                put(move || async move {
                    *f3.closed_at.lock().unwrap() = Some(f3.now());
                    if let Some(e) = &finish_line {
                        e.observe(
                            "INFO merge pipeline completed successfully index_uid=idx:01M3 \
                             source_id=_ingest-source generation=1",
                        );
                    }
                    "null"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), fake)
    }

    fn quick() -> MergeWait {
        MergeWait {
            poll: Duration::from_millis(5),
            stable_polls: 2,
            timeout: Duration::from_secs(10),
            finalize_grace: Duration::from_millis(50),
            finalize_stall: Duration::from_millis(300),
            report: Reporter::off(),
        }
    }

    #[tokio::test]
    async fn waits_for_merges_then_the_final_merge() {
        let events = Arc::new(NodeEvents::default());
        let (base, fake) = fake_node(Some(events.clone())).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let layout = seal(&node, "idx", 1000, Some(&events), &quick(), None)
            .await
            .unwrap();
        assert_eq!((layout.splits, layout.docs), (1, 1000));
        // Closed only after the planner had settled on three splits.
        assert!(fake.closed_at.lock().unwrap().unwrap() >= 5);
    }

    #[tokio::test]
    async fn without_the_node_output_waits_out_the_grace_period() {
        let (base, _) = fake_node(None).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let started = std::time::Instant::now();
        let layout = seal(&node, "idx", 1000, None, &quick(), None)
            .await
            .unwrap();
        assert_eq!(layout.splits, 1);
        assert!(started.elapsed() >= quick().finalize_grace);
    }

    #[tokio::test]
    async fn gives_up_at_the_timeout() {
        // The final merge never reports done.
        let events = Arc::new(NodeEvents::default());
        let (base, _) = fake_node(None).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let wait = MergeWait {
            timeout: Duration::from_millis(300),
            ..quick()
        };
        let err = seal(&node, "idx", 1000, Some(&events), &wait, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not finish"), "{err}");
        assert!(err.contains("finalize"), "{err}");
    }

    #[tokio::test]
    async fn a_writer_that_stops_answering_hits_the_same_deadline() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/metrics",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                ""
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let node = Node {
            http: &http,
            base: &base,
        };
        let wait = MergeWait {
            timeout: Duration::from_millis(200),
            ..quick()
        };
        let started = std::time::Instant::now();
        let err = seal(&node, "idx", 1000, None, &wait, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not finish within"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_full_disk_stops_the_wait() {
        let events = Arc::new(NodeEvents::default());
        events.observe("ERROR No space left on device (os error 28)");
        let (base, fake) = fake_node(None).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let err = seal(&node, "idx", 1000, Some(&events), &quick(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("ran out of disk"), "{err}");
        assert!(fake.closed_at.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_writer_that_dies_is_named_instead_of_the_failed_request() {
        let events = NodeEvents::default();
        let refused = anyhow::anyhow!("error sending request: Connection refused");
        // Not exited (yet): the request's own error, after the grace period.
        let err = events
            .explain(anyhow::anyhow!("timed out"), Duration::from_millis(50))
            .await;
        assert_eq!(format!("{err:#}"), "timed out");

        events.writer_exited("the Quickwit writer was killed by signal 9 (SIGKILL)");
        // Only the first exit counts.
        events.writer_exited("later");
        let err = format!("{:#}", events.explain(refused, Duration::ZERO).await);
        assert!(
            err.starts_with("the Quickwit writer was killed by signal 9 (SIGKILL): "),
            "{err}"
        );
        assert!(err.contains("Connection refused"), "{err}");
        let err = events.check().unwrap_err().to_string();
        assert!(
            err.contains("signal 9") && err.contains("not publishing"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_merge_wait_reports_a_writer_that_died() {
        use axum::http::StatusCode;
        use axum::routing::get;
        // The node stops answering once it has "died".
        let events = Arc::new(NodeEvents::default());
        let polls = Arc::new(AtomicU32::new(0));
        let (ev, p) = (events.clone(), polls.clone());
        let app = axum::Router::new()
            .route(
                "/metrics",
                get(move || async move {
                    if p.fetch_add(1, Ordering::SeqCst) < 2 {
                        return (
                            StatusCode::OK,
                            "quickwit_indexing_ongoing_merge_operations 1\n",
                        );
                    }
                    ev.writer_exited("the Quickwit writer was killed by signal 9 (SIGKILL)");
                    (StatusCode::BAD_GATEWAY, "")
                }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|| async { axum::Json(serde_json::json!({"splits": []})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let node = Node {
            http: &http,
            base: &base,
        };
        let err = seal(&node, "idx", 1000, Some(&events), &quick(), None)
            .await
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("signal 9"), "{err}");
        assert_eq!(polls.load(Ordering::SeqCst), 3);
    }

    /// What a scripted writer does when the release closes the index.
    #[derive(Clone, Copy, PartialEq)]
    enum OnClose {
        /// Quickwit 0.9.1's uploader race (issue #84): the pipeline fails.
        Fail,
        /// Nothing at all.
        Nothing,
        /// The final merge: one split.
        Merge,
    }

    /// A writer whose final merges go as `script` says, one entry per close;
    /// reopening the index starts a new merge pipeline if `spawns`.
    struct Scripted {
        events: Arc<NodeEvents>,
        script: Vec<OnClose>,
        spawns: bool,
        toggles: Mutex<Vec<bool>>,
        probes: AtomicU32,
        merged: std::sync::atomic::AtomicBool,
    }

    const SPAWNED: &str = "INFO spawn_merge_pipeline{index_uid=idx:01M3 generation=0}: merge_pipeline: \
                           spawning merge pipeline index_uid=idx:01M3 source_id=_ingest-source root_dir=/x";
    const STOPPING: &str =
        "INFO indexing_service: shutting down orphan merge pipeline index_uid=idx:01M3 \
                            source_id=_ingest-source";
    const FAILED: &str = "ERROR merge_pipeline: merge pipeline failed index_uid=idx:01M3 \
                          source_id=_ingest-source generation=1";
    const COMPLETED: &str = "INFO merge_pipeline: merge pipeline completed successfully \
                             index_uid=idx:01M3 source_id=_ingest-source generation=1";

    async fn scripted(script: &[OnClose], spawns: bool) -> (String, Arc<Scripted>) {
        use axum::routing::{get, post, put};
        let w = Arc::new(Scripted {
            events: Arc::new(NodeEvents::default()),
            script: script.to_vec(),
            spawns,
            toggles: Mutex::default(),
            probes: AtomicU32::new(0),
            merged: Default::default(),
        });
        w.events.observe(SPAWNED);
        let (w1, w2, w3) = (w.clone(), w.clone(), w.clone());
        let app = axum::Router::new()
            .route(
                "/metrics",
                get(|| async { "quickwit_indexing_ongoing_merge_operations 0\n" }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(move || async move {
                    let splits: &[(&str, u64)] = if w1.merged.load(Ordering::SeqCst) {
                        &[("m1", 1000)]
                    } else {
                        &[("s1", 400), ("s2", 600)]
                    };
                    axum::Json(
                        serde_json::json!({"splits": splits.iter().map(|(id, docs)| {
                        serde_json::json!({"split_id": id, "num_docs": docs})
                    }).collect::<Vec<_>>()}),
                    )
                }),
            )
            .route(
                "/api/v1/indexes/{id}/sources/_ingest-source/toggle",
                put(move |axum::Json(body): axum::Json<Value>| async move {
                    let enable = body["enable"].as_bool().unwrap();
                    let closes = {
                        let mut t = w2.toggles.lock().unwrap();
                        t.push(enable);
                        t.iter().filter(|e| !**e).count()
                    };
                    if !enable {
                        let step = w2
                            .script
                            .get(closes - 1)
                            .copied()
                            .unwrap_or(OnClose::Nothing);
                        if step != OnClose::Nothing {
                            w2.events.observe(STOPPING);
                        }
                        match step {
                            OnClose::Fail => w2.events.observe(FAILED),
                            OnClose::Merge => {
                                w2.merged.store(true, Ordering::SeqCst);
                                w2.events.observe(COMPLETED);
                            }
                            OnClose::Nothing => {}
                        }
                    }
                    "null"
                }),
            )
            .route(
                "/api/v1/{id}/ingest",
                post(move |body: String| async move {
                    // Only the reopening document, which the mapping rejects.
                    assert_eq!(body, "{\"usnm_reopen\": true}\n");
                    w3.probes.fetch_add(1, Ordering::SeqCst);
                    if w3.spawns {
                        w3.events.observe(SPAWNED);
                    }
                    axum::Json(serde_json::json!({
                        "num_docs_for_processing": 1, "num_ingested_docs": 0, "num_rejected_docs": 1
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), w)
    }

    async fn seal_scripted(
        script: &[OnClose],
        spawns: bool,
    ) -> (anyhow::Result<IndexLayout>, Arc<Scripted>) {
        let (base, w) = scripted(script, spawns).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let r = seal(&node, "idx", 1000, Some(&w.events), &quick(), None).await;
        (r, w)
    }

    /// Issue #84: the final merge failed and Quickwit left its pipeline
    /// stopped, so the release waited out its timeout with six splits.
    #[tokio::test]
    async fn final_merges_that_fail_are_run_again() {
        let (r, w) = seal_scripted(&[OnClose::Fail, OnClose::Merge], true).await;
        let layout = r.unwrap();
        assert_eq!((layout.splits, layout.docs), (1, 1000));
        // Closed, reopened with one rejected document, closed again.
        assert_eq!(*w.toggles.lock().unwrap(), [false, true, false]);
        assert_eq!(w.probes.load(Ordering::SeqCst), 1);
        assert_eq!(w.events.spawns("idx"), 2);
    }

    #[tokio::test]
    async fn final_merges_that_never_finish_are_run_again() {
        let (r, w) = seal_scripted(&[OnClose::Nothing, OnClose::Merge], true).await;
        assert_eq!(r.unwrap().splits, 1);
        assert_eq!(*w.toggles.lock().unwrap(), [false, true, false]);
    }

    #[tokio::test]
    async fn final_merges_give_up_after_three_tries() {
        let (r, w) = seal_scripted(&[OnClose::Fail; 4], true).await;
        let err = r.unwrap_err().to_string();
        assert!(err.contains("did not finish in 3 tries"), "{err}");
        assert!(err.contains("merge pipeline failed"), "{err}");
        assert!(err.contains("half-merged"), "{err}");
        assert_eq!(
            *w.toggles.lock().unwrap(),
            [false, true, false, true, false]
        );
    }

    #[tokio::test]
    async fn a_reopened_index_that_starts_no_pipeline_is_closed_and_not_published() {
        let (r, w) = seal_scripted(&[OnClose::Fail, OnClose::Merge], false).await;
        let err = r.unwrap_err().to_string();
        assert!(err.contains("started no merge pipeline"), "{err}");
        // Sealed again all the same.
        assert_eq!(*w.toggles.lock().unwrap(), [false, true, false]);
    }

    /// Quickwit also runs the final merges by itself once the index's shards
    /// have been idle for 15 minutes and are deleted: closing the index then
    /// starts nothing, and the merges are already done.
    /// The merge wait gives up (drops the rerun) while the reopening request
    /// still waits: the index is closed again all the same.
    #[tokio::test]
    async fn a_rerun_dropped_while_reopening_closes_the_index() {
        use axum::routing::{post, put};
        let events = Arc::new(NodeEvents::default());
        events.observe(SPAWNED);
        let toggles = Arc::new(Mutex::new(Vec::new()));
        let t = toggles.clone();
        let app = axum::Router::new()
            .route(
                "/api/v1/indexes/{id}/sources/_ingest-source/toggle",
                put(move |axum::Json(body): axum::Json<Value>| async move {
                    t.lock().unwrap().push(body["enable"].as_bool().unwrap());
                    "null"
                }),
            )
            .route(
                "/api/v1/{id}/ingest",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "{}"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let node = Node {
            http: &http,
            base: &base,
        };
        let wait = MergeWait {
            finalize_stall: Duration::from_secs(60),
            ..quick()
        };
        let far = tokio::time::Instant::now() + Duration::from_secs(60);
        let rerun = rerun_final_merges(&node, "idx", &events, &wait, far);
        assert!(tokio::time::timeout(Duration::from_millis(300), rerun)
            .await
            .is_err());
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while toggles.lock().unwrap().len() < 2 {
            assert!(
                std::time::Instant::now() < until,
                "{:?}",
                toggles.lock().unwrap()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*toggles.lock().unwrap(), [true, false]);
    }

    /// `seal` gives up while a rerun's reopening request still waits: it
    /// closes the index again before it returns, so the caller can stop the
    /// writer right away.
    #[tokio::test]
    async fn a_seal_that_gives_up_mid_rerun_closes_the_index_before_returning() {
        use axum::routing::{get, post, put};
        let events = Arc::new(NodeEvents::default());
        events.observe(SPAWNED);
        let toggles = Arc::new(Mutex::new(Vec::new()));
        let (ev, t) = (events.clone(), toggles.clone());
        let app = axum::Router::new()
            .route(
                "/metrics",
                get(|| async { "quickwit_indexing_ongoing_merge_operations 0\n" }),
            )
            .route(
                "/api/v1/indexes/{id}/splits",
                get(|| async {
                    axum::Json(
                        serde_json::json!({"splits": [{"split_id": "s1", "num_docs": 1000}]}),
                    )
                }),
            )
            .route(
                "/api/v1/indexes/{id}/sources/_ingest-source/toggle",
                put(move |axum::Json(body): axum::Json<Value>| async move {
                    let enable = body["enable"].as_bool().unwrap();
                    t.lock().unwrap().push(enable);
                    if !enable {
                        ev.observe(STOPPING);
                        ev.observe(FAILED);
                    }
                    "null"
                }),
            )
            .route(
                "/api/v1/{id}/ingest",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "{}"
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = reqwest::Client::new();
        let base = format!("http://{addr}");
        let node = Node {
            http: &http,
            base: &base,
        };
        let wait = MergeWait {
            timeout: Duration::from_millis(400),
            finalize_stall: Duration::from_secs(60),
            ..quick()
        };
        let err = seal(&node, "idx", 1000, Some(&events), &wait, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not publishing"), "{err}");
        let t = toggles.lock().unwrap().clone();
        assert_eq!(t.first(), Some(&false), "{t:?}");
        assert!(t.contains(&true), "{t:?}");
        assert_eq!(t.last(), Some(&false), "{t:?}");
    }

    #[tokio::test]
    async fn final_merges_that_ran_before_the_close_count() {
        let (base, w) = scripted(&[OnClose::Nothing], true).await;
        w.events.observe(STOPPING);
        w.events.observe(COMPLETED);
        w.merged.store(true, Ordering::SeqCst);
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let layout = seal(&node, "idx", 1000, Some(&w.events), &quick(), None)
            .await
            .unwrap();
        assert_eq!(layout.splits, 1);
        assert_eq!(*w.toggles.lock().unwrap(), [false]);
    }

    #[tokio::test]
    async fn a_merge_that_loses_documents_is_not_published() {
        let events = Arc::new(NodeEvents::default());
        let (base, _) = fake_node(Some(events.clone())).await;
        let http = reqwest::Client::new();
        let node = Node {
            http: &http,
            base: &base,
        };
        let err = seal(&node, "idx", 1001, Some(&events), &quick(), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("expected 1001"), "{err}");
    }
}
