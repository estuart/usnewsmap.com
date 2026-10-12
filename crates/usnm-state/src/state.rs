//! Typed pipeline state on top of [`DocStore`] (05 §5.9.1): batches (the
//! work queue), issues, the single-writer lock and index runs.

use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{DateTime, Duration, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docs::{DocStore, Versioned};

pub const BATCHES: &str = "batches";
pub const ISSUES: &str = "issues";
pub const INDEX_RUNS: &str = "index_runs";
pub const OPS: &str = "ops";
/// The `ops` item that paces bulk downloads from LoC across every worker.
pub const FETCH_PACER: &str = "loc-bulk-pacer";
/// The `ops` lock held by the one release (Quickwit writer) at a time (08 §8.4.1).
pub const WRITER_LOCK: &str = "quickwit-writer";
/// Deltas a version may carry before the next release compacts (08 §8.4.1).
pub const MAX_DELTAS: usize = 8;
/// The `ops` item a running release updates with how far it has got.
pub const RELEASE_PROGRESS: &str = "release-progress";
/// The `ops` item the ingest job (`run`, `titles-sync`, `release`) keeps up
/// to date with the step it is on, for the status page's "Right now" line.
pub const ACTIVITY: &str = "activity";
/// An `ops/activity` item not rewritten for this long, and not ended,
/// belongs to an execution that stopped without saying (it writes a
/// heartbeat every minute).
pub const ACTIVITY_STALE_SECS: i64 = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    Queued,
    Downloading,
    Curated,
    Failed,
}

impl BatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Curated => "curated",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub owner: String,
    pub until: DateTime<Utc>,
}

/// The committed output of one curated batch version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Curated {
    pub version: u16,
    /// Object paths in the curated store.
    pub parts: Vec<String>,
    /// Pages per (lccn, day), for baselines without re-reading the parts.
    pub counts: String,
    pub pages: u64,
    /// Pages with `text_status = ok` (the ones that are indexed).
    pub ok_pages: u64,
    pub lccns: Vec<String>,
    pub source_sha256: String,
    pub first: String,
    pub last: String,
}

/// One LoC batch (partition key `/batch`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    pub batch: String,
    /// The newest version seen; the one to curate.
    pub version: u16,
    pub source_url: String,
    #[serde(default)]
    pub source_sha256: Option<String>,
    pub ocr_source: String,
    pub status: BatchStatus,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub lease: Option<Lease>,
    /// The last committed curation (may be an older version while a newer one is queued).
    #[serde(default)]
    pub curated: Option<Curated>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
    /// When `curated` was committed. `updated_at` changes with every later
    /// write (a newer version queued, a claim), so throughput counts this.
    /// Absent on batches curated before it was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curated_at: Option<DateTime<Utc>>,
}

/// One issue (lccn + date + edition), partition key `/lccn`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub lccn: String,
    pub date: String,
    pub edition: u16,
    pub pages: u32,
    pub empty_pages: u32,
    pub batch: String,
    pub batch_version: u16,
    pub ocr_source: String,
}

/// One batch a version was built from, at the curation it was built from.
/// A version's list lives in its reference snapshot (`{version}/batches.json`),
/// not in the [`IndexRun`] item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunBatch {
    pub batch: String,
    pub curated: Curated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Building,
    Published,
    Failed,
}

/// The file in a version's reference snapshot that lists the batches it was
/// built from (a JSON array of [`RunBatch`]).
pub const RUN_BATCHES_FILE: &str = "batches.json";

/// The file in a version's reference snapshot with every title's published
/// pages, `{lccn: pages}` (the status page's pages by language). Snapshots
/// written before it existed don't have it.
pub const TITLE_PAGES_FILE: &str = "title_pages.json";

/// The file in a version's reference snapshot with the dates of every
/// title's first and last published page, `{lccn: {first, last}}`
/// ([`TitleDates`]): the same pages as [`TITLE_PAGES_FILE`], by date.
/// Snapshots written before it existed don't have it.
pub const TITLE_DATES_FILE: &str = "title_dates.json";

/// The first and last days with a published page of one title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleDates {
    pub first: chrono::NaiveDate,
    pub last: chrono::NaiveDate,
}

/// The file in a version's reference snapshot with the copies of duplicated
/// pages that searches must not see: `[{doc_id, batch}]`, the losing copies
/// an earlier index of the version holds (04 §4.7). Snapshots written before
/// it existed don't have it.
pub const DUPLICATES_FILE: &str = "duplicates.json";

/// The file in a version's reference snapshot with pages published per place
/// and day for each distinct set of title languages ([`LanguageBaselines`]),
/// so a language filter keeps exact baselines. Snapshots written before it
/// existed don't have it.
pub const LANGUAGE_BASELINES_FILE: &str = "language_baselines.json";

/// The file in a version's reference snapshot with pages published per place
/// and day for each distinct set of page languages (05 §5.5.8, #289), in
/// [`LanguageBaselines`]' shape: the title-language counts with every page
/// whose own languages differ from its title's moved to their set. Only
/// versions whose `current.json` has `page_language` have it.
pub const PAGE_LANGUAGE_BASELINES_FILE: &str = "page_language_baselines.json";

/// The file in a version's reference snapshot with the pages its main
/// indexes hold in a set of page languages other than their title's (05
/// §5.5.8), so a delta can count them again: the release, not the API,
/// reads it. Only versions with page languages have it.
pub const PAGE_LANGUAGES_FILE: &str = "page_languages.json";

/// The file in a version's reference snapshot with the pages whose main-index
/// document holds the Latin-script text of our own OCR rather than LoC's
/// ([`JaLatin`], #203, 04 §4.8). Only versions with such pages have it.
pub const JA_LATIN_FILE: &str = "ja_latin.json";

/// The pages a main index of the version holds with the Latin-script text
/// our Japanese OCR read rather than LoC's, keyed by `doc_id`: pages LoC
/// ships without text (`loc_text = missing`; their English ads, mastheads
/// and sections), and pages whose LoC text our reading beats (`garbled`,
/// `short`, `empty`, `mixed`). The API marks their hits as our OCR, as it
/// does the Japanese index's.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JaLatin {
    pub pages: std::collections::BTreeMap<String, JaLatinPage>,
}

/// One page of [`JaLatin`]: the batch its document carries (the one whose
/// archive had no `ocr.txt` for it) and who read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JaLatinPage {
    pub batch: String,
    /// Why our OCR read the page (the overlay's `loc_text`): `missing` for a
    /// page with no LoC copy in the version, else the page's LoC copy.
    pub loc_text: String,
    pub ocr_source: String,
    pub ocr_engine: String,
}

/// Pages published per place and day, split by the languages the pages' title
/// lists. A page counts in exactly one set, so any selection of languages
/// sums the sets that share one of them without counting a page twice.
/// Titles that list no language are left out: no language filter matches them.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageBaselines {
    pub sets: Vec<LanguageSet>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanguageSet {
    /// Catalog language codes, sorted and without repeats.
    pub languages: Vec<String>,
    /// `{place id: sorted (day, pages)}`, as in `baselines.json`.
    pub baselines: std::collections::BTreeMap<String, Vec<(u32, u32)>>,
}

/// One index build (partition key `/index_version`).
///
/// The item holds only bounded fields. The version's batch list, which grows
/// with the corpus (500–650 bytes per batch), is in the reference store at
/// [`IndexRun::batch_list`], so the item stays far below Cosmos DB's 2 MB
/// item limit however many batches a version has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRun {
    pub id: String,
    pub index_version: String,
    pub full: bool,
    /// Every index the version serves (base + deltas).
    pub indexes: Vec<String>,
    /// The index this run wrote; empty when it wrote none (a release that
    /// only removes the published Congress set, #229).
    pub new_index: String,
    /// Batches in the version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_count: Option<u64>,
    /// Path of the version's batch list in the reference store
    /// (`{version}/batches.json`), set once the snapshot is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_list: Option<String>,
    /// The batch list inline, as runs written before it moved to the
    /// reference snapshot have it. New runs leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batches: Option<Vec<RunBatch>>,
    pub status: RunStatus,
    pub docs: u64,
    /// Distinct pages in the version: a page in two batches counts once.
    pub pages: u64,
    /// Copies of pages that also ship in another batch of the version, left
    /// out of `pages` (04 §4.7). Runs before this was recorded leave it out.
    #[serde(default)]
    pub duplicate_pages: u64,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub previous_version: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// When the run was marked failed. Runs that failed before this was
    /// recorded leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,
    /// What built the version (#161): commit, Quickwit, feature versions and
    /// the index templates' checksums (`usnm_ingest::build_info::summary`).
    /// The version's manifest has the templates in full. Runs before this
    /// was recorded leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<serde_json::Value>,
}

/// How far the running release has got: the `ops/release-progress` item,
/// rewritten every 30 s while an index is built. It is a separate item, not
/// part of the [`IndexRun`], so these writes never change the run's ETag,
/// which the release threads through its own conditional updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseProgress {
    pub index_version: String,
    pub docs_sent: u64,
    pub docs_expected: u64,
    pub mb_sent: f64,
    pub updated_at: DateTime<Utc>,
}

/// The step an ingest job execution is on (`ops/activity`), in the order a
/// `run` takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Reading LoC's batch listing and queueing new batches.
    Listing,
    /// Downloading and processing (curating) queued batches.
    Downloading,
    /// Fetching newspaper (title) records from loc.gov (titles-sync).
    Titles,
    /// Sending pages to the new search index.
    Indexing,
    /// Waiting for the new index's merges.
    Merging,
    /// Writing the reference snapshot and the version pointer.
    Publishing,
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A new version went live.
    Published,
    /// It finished with nothing new to publish.
    NothingNew,
    /// titles-sync didn't fetch every title in time (LoC rate limits it);
    /// a full rebuild then publishes nothing and the next execution goes on.
    TitlesLeft,
    /// It failed; `error` says why.
    Failed,
}

/// Where the merge wait is (`crate::merges` in the ingest crate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeProgress {
    /// `settle` (the index is open and merging) or `finalize` (closed, last merges).
    pub step: String,
    pub splits: u64,
    pub merges_running: u64,
    pub merges_queued: u64,
}

/// What the ingest job execution is doing: the `ops/activity` item. One
/// execution of `caj-usnm-ingest` runs at a time (it has parallelism 1);
/// the backfill job's curate workers don't write it. The job rewrites it
/// on every step change and at least once a minute while it runs, so a
/// reader can tell a live execution from one that stopped without saying.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    /// The `usnm-ingest` command: `run`, `titles-sync` or `release`.
    pub command: String,
    /// The writer's process id ([`crate`] users show its last characters only).
    pub owner: String,
    pub started_at: DateTime<Utc>,
    pub step: Step,
    pub step_started_at: DateTime<Utc>,
    /// The last write: a heartbeat while the execution runs.
    pub updated_at: DateTime<Utc>,
    /// The step's progress, where it counts something (titles fetched,
    /// batches curated); indexing reports in `ops/release-progress`.
    #[serde(default)]
    pub done: Option<u64>,
    #[serde(default)]
    pub total: Option<u64>,
    /// titles-sync sends nothing to loc.gov until then (LoC rate limited it).
    #[serde(default)]
    pub paused_until: Option<DateTime<Utc>>,
    /// The version being built, from the indexing step on.
    #[serde(default)]
    pub index_version: Option<String>,
    #[serde(default)]
    pub merge: Option<MergeProgress>,
    /// Set when the execution ends, with how.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub outcome: Option<Outcome>,
    /// The error an execution failed with (the API sanitizes it).
    #[serde(default)]
    pub error: Option<String>,
    /// The item as the previous execution left it (ended, or stale: it
    /// stopped without saying), without its own `previous`. Kept by
    /// [`State::set_activity`] when a new execution takes the item over, so
    /// the status page can say how the last execution ended while the next
    /// one runs. Unreadable values are dropped rather than failing the item.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient"
    )]
    pub previous: Option<Box<Activity>>,
}

/// `T` if the value parses as one, else `None`.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

/// Typed access to the pipeline's state.
#[derive(Clone)]
pub struct State {
    pub docs: Arc<dyn DocStore>,
}

fn to_value<T: Serialize>(item: &T) -> anyhow::Result<Value> {
    Ok(serde_json::to_value(item)?)
}

fn from<T: DeserializeOwned>(v: Versioned) -> anyhow::Result<(T, String)> {
    let id = v.doc["id"].as_str().unwrap_or("?").to_owned();
    let item = serde_json::from_value(v.doc).with_context(|| format!("state item `{id}`"))?;
    Ok((item, v.etag))
}

impl State {
    pub fn new(docs: Arc<dyn DocStore>) -> Self {
        Self { docs }
    }

    // ---------------------------------------------------------------- batches

    pub async fn batch(&self, batch: &str) -> anyhow::Result<Option<(Batch, String)>> {
        self.docs
            .get(BATCHES, batch, batch)
            .await?
            .map(from)
            .transpose()
    }

    pub async fn batches(&self, statuses: &[BatchStatus]) -> anyhow::Result<Vec<(Batch, String)>> {
        let values: Vec<&str> = statuses.iter().map(|s| s.as_str()).collect();
        self.docs
            .list(BATCHES, "status", &values)
            .await?
            .into_iter()
            .map(from)
            .collect()
    }

    pub async fn create_batch(&self, b: &Batch) -> anyhow::Result<bool> {
        Ok(self
            .docs
            .create(BATCHES, &b.batch, &to_value(b)?)
            .await?
            .is_some())
    }

    /// Replace if unchanged since read; returns the new ETag, or `None` if another writer won.
    pub async fn replace_batch(&self, b: &Batch, etag: &str) -> anyhow::Result<Option<String>> {
        self.docs
            .replace(BATCHES, &b.batch, &to_value(b)?, etag)
            .await
    }

    pub async fn upsert_issue(&self, i: &Issue) -> anyhow::Result<()> {
        self.docs.upsert(ISSUES, &i.lccn, &to_value(i)?).await
    }

    // ------------------------------------------------------------ index runs

    pub async fn run(&self, version: &str) -> anyhow::Result<Option<(IndexRun, String)>> {
        self.docs
            .get(INDEX_RUNS, version, version)
            .await?
            .map(from)
            .transpose()
    }

    pub async fn create_run(&self, r: &IndexRun) -> anyhow::Result<bool> {
        Ok(self
            .docs
            .create(INDEX_RUNS, &r.index_version, &to_value(r)?)
            .await?
            .is_some())
    }

    pub async fn update_run(&self, r: &IndexRun, etag: &str) -> anyhow::Result<String> {
        self.docs
            .replace(INDEX_RUNS, &r.index_version, &to_value(r)?, etag)
            .await?
            .with_context(|| format!("index run `{}` changed concurrently", r.id))
    }

    // ------------------------------------------------------------------- ops

    /// The published version, mirrored from `current.json` (05 §5.9.1).
    pub async fn current_version(&self) -> anyhow::Result<Option<String>> {
        Ok(self
            .docs
            .get(OPS, "current", "current")
            .await?
            .and_then(|v| v.doc["index_version"].as_str().map(str::to_owned)))
    }

    pub async fn set_current_version(&self, version: &str) -> anyhow::Result<()> {
        let doc = serde_json::json!({
            "id": "current", "kind": "current", "index_version": version,
            "updated_at": Utc::now(),
        });
        self.docs.upsert(OPS, "current", &doc).await
    }

    /// Take the single-writer lock (08 §8.4.1). Fails if another live holder has it.
    pub async fn lock(&self, name: &str, owner: &str, ttl: Duration) -> anyhow::Result<()> {
        self.lock_or_take_over(name, owner, ttl, &|_| false)
            .await
            .map(drop)
    }

    /// [`State::lock`], but a holder that `gone` says has stopped is taken
    /// over at once instead of when its lease runs out; returns that holder.
    /// `gone` is the caller's to make safe: [`State::lock`] passes one that
    /// never says so, which every Cosmos lock uses.
    pub async fn lock_or_take_over(
        &self,
        name: &str,
        owner: &str,
        ttl: Duration,
        gone: &(dyn Fn(&str) -> bool + Send + Sync),
    ) -> anyhow::Result<Option<String>> {
        check_lock_name(name)?;
        // A lost ETag race just means re-reading: it may have been this
        // owner's own concurrent renewal, which is fine.
        for _ in 0..5 {
            let doc = serde_json::json!({
                "id": name, "kind": name, "owner": owner, "until": Utc::now() + ttl,
            });
            let Some(held) = self.docs.get(OPS, name, name).await? else {
                if self.docs.create(OPS, name, &doc).await?.is_some() {
                    return Ok(None);
                }
                continue;
            };
            let until: Option<DateTime<Utc>> =
                serde_json::from_value(held.doc["until"].clone()).ok();
            let holder = held.doc["owner"].as_str().unwrap_or("?");
            let live = holder != owner && until.is_some_and(|u| u > Utc::now());
            if live && !gone(holder) {
                bail!(
                    "lock `{name}` is held by `{holder}` until {}",
                    until.expect("checked")
                );
            }
            let taken = live.then(|| holder.to_owned());
            if self
                .docs
                .replace(OPS, name, &doc, &held.etag)
                .await?
                .is_some()
            {
                return Ok(taken);
            }
        }
        bail!("lock `{name}` kept changing concurrently")
    }

    /// Reserve the next bulk-download slot and return when it starts. Every
    /// worker and job execution shares one egress IP, and LoC allows 10 bulk
    /// requests per 10 minutes per IP (04 §4.1), so slots come from one item,
    /// `interval` apart, and none start before a recorded block lifts.
    pub async fn reserve_fetch_slot(&self, interval: Duration) -> anyhow::Result<DateTime<Utc>> {
        for _ in 0..20 {
            let now = Utc::now();
            let held = self.docs.get(OPS, FETCH_PACER, FETCH_PACER).await?;
            let (next, blocked) = pacer(held.as_ref());
            let slot = next.map_or(now, |n| n.max(now));
            if self
                .set_pacer(held.as_ref(), slot + interval, blocked)
                .await?
            {
                return Ok(slot);
            }
        }
        bail!("the download pacer kept changing concurrently")
    }

    /// Whether a download may start now: `false` while a recorded block is in
    /// force. A slot reserved before the block was recorded is void, so its
    /// holder asks this after waiting and reserves again if it must. A plain
    /// read isn't enough: Cosmos's Session consistency is per process, so
    /// another worker's block may not be visible yet. Rewriting the item with
    /// its ETag only succeeds against the latest version.
    pub async fn fetch_allowed(&self) -> anyhow::Result<bool> {
        for _ in 0..20 {
            let Some(held) = self.docs.get(OPS, FETCH_PACER, FETCH_PACER).await? else {
                return Ok(true);
            };
            if pacer(Some(&held)).1.is_some_and(|u| u > Utc::now()) {
                return Ok(false);
            }
            if self
                .docs
                .replace(OPS, FETCH_PACER, &held.doc, &held.etag)
                .await?
                .is_some()
            {
                return Ok(true);
            }
        }
        bail!("the download pacer kept changing concurrently")
    }

    /// Hold every worker's bulk downloads until `until`: LoC blocks an IP for
    /// about an hour once it answers 429, and each retry extends the block.
    pub async fn block_fetches(&self, until: DateTime<Utc>) -> anyhow::Result<()> {
        for _ in 0..20 {
            let held = self.docs.get(OPS, FETCH_PACER, FETCH_PACER).await?;
            let (next, blocked) = pacer(held.as_ref());
            if blocked.is_some_and(|b| b >= until) {
                return Ok(());
            }
            let next = next.map_or(until, |n| n.max(until));
            if self.set_pacer(held.as_ref(), next, Some(until)).await? {
                return Ok(());
            }
        }
        bail!("the download pacer kept changing concurrently")
    }

    /// Write the pacer, if it hasn't changed since `held` was read.
    async fn set_pacer(
        &self,
        held: Option<&Versioned>,
        next: DateTime<Utc>,
        blocked_until: Option<DateTime<Utc>>,
    ) -> anyhow::Result<bool> {
        let doc = serde_json::json!({
            "id": FETCH_PACER, "kind": FETCH_PACER, "next": next,
            "blocked_until": blocked_until,
        });
        Ok(match held {
            Some(h) => self.docs.replace(OPS, FETCH_PACER, &doc, &h.etag).await?,
            None => self.docs.create(OPS, FETCH_PACER, &doc).await?,
        }
        .is_some())
    }

    /// Record the running release's progress (unconditional: only the
    /// writer-lock holder writes it).
    pub async fn set_release_progress(&self, p: &ReleaseProgress) -> anyhow::Result<()> {
        let mut doc = to_value(p)?;
        doc["id"] = RELEASE_PROGRESS.into();
        doc["kind"] = RELEASE_PROGRESS.into();
        self.docs.upsert(OPS, RELEASE_PROGRESS, &doc).await
    }

    /// Record what the ingest job is doing. Executions can overlap (one
    /// started by hand while another runs), so this writes only over its
    /// own item, an ended one or a stale one, and only if the item hasn't
    /// changed since it was read. Returns whether it wrote.
    ///
    /// Taking over another execution's item keeps that item as `previous`
    /// (how the last execution ended); rewriting its own keeps the
    /// `previous` already there.
    pub async fn set_activity(&self, a: &Activity) -> anyhow::Result<bool> {
        let mut doc = to_value(a)?;
        doc["id"] = ACTIVITY.into();
        doc["kind"] = ACTIVITY.into();
        let Some(held) = self.docs.get(OPS, ACTIVITY, ACTIVITY).await? else {
            return Ok(self.docs.create(OPS, ACTIVITY, &doc).await?.is_some());
        };
        let other = held.doc["owner"].as_str().is_some_and(|o| o != a.owner);
        let ended = !held.doc["ended_at"].is_null();
        let updated: Option<DateTime<Utc>> =
            serde_json::from_value(held.doc["updated_at"].clone()).ok();
        let live =
            updated.is_some_and(|u| Utc::now() - u <= Duration::seconds(ACTIVITY_STALE_SECS));
        if other && !ended && live {
            return Ok(false);
        }
        if a.previous.is_none() {
            let previous = if other {
                previous_of(&held.doc)
            } else {
                held.doc.get("previous").cloned().unwrap_or(Value::Null)
            };
            if !previous.is_null() {
                doc["previous"] = previous;
            }
        }
        Ok(self
            .docs
            .replace(OPS, ACTIVITY, &doc, &held.etag)
            .await?
            .is_some())
    }

    pub async fn unlock(&self, name: &str, owner: &str) -> anyhow::Result<()> {
        check_lock_name(name)?;
        if let Some(held) = self.docs.get(OPS, name, name).await? {
            if held.doc["owner"].as_str() == Some(owner) {
                let doc = serde_json::json!({
                    "id": name, "kind": name, "owner": "", "until": Utc::now(),
                });
                self.docs.replace(OPS, name, &doc, &held.etag).await?;
            }
        }
        Ok(())
    }
}

/// A lock's name is its item id in `ops`, which Cosmos puts in the document
/// path, so it can't hold `/`, `\`, `?` or `#`. The in-memory store takes
/// any id, so this check is what keeps tests honest.
fn check_lock_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() || name.contains(['/', '\\', '?', '#']) {
        bail!(
            "lock name `{name}` is not a valid Cosmos id (empty, or holds `/`, `\\`, `?` or `#`)"
        );
    }
    Ok(())
}

/// An `ops/activity` item as another execution's `previous`: its own
/// fields, without the store's (`id`, `kind`, `_etag` and the like) and
/// without its `previous`, so the chain stays one deep.
fn previous_of(held: &Value) -> Value {
    match held {
        Value::Object(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| {
                    !matches!(k.as_str(), "id" | "kind" | "previous") && !k.starts_with('_')
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        _ => Value::Null,
    }
}

/// The pacer's next free slot and the end of its recorded block, if any.
pub fn pacer(held: Option<&Versioned>) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let at = |field: &str| -> Option<DateTime<Utc>> {
        held.and_then(|h| serde_json::from_value(h.doc[field].clone()).ok())
    };
    (at("next"), at("blocked_until"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::MemoryDocs;

    #[tokio::test]
    async fn download_slots_are_spaced_across_concurrent_workers() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        let gap = Duration::seconds(75);
        let mut slots = Vec::new();
        for _ in 0..3 {
            let (a, b) = tokio::join!(s.reserve_fetch_slot(gap), s.reserve_fetch_slot(gap));
            slots.push(a.unwrap());
            slots.push(b.unwrap());
        }
        slots.sort();
        for pair in slots.windows(2) {
            assert!(pair[1] - pair[0] >= gap, "{slots:?}");
        }
    }

    #[tokio::test]
    async fn a_block_holds_every_later_slot() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        let until = Utc::now() + Duration::hours(1);
        s.block_fetches(until).await.unwrap();
        // An earlier block never shortens a later one.
        s.block_fetches(Utc::now()).await.unwrap();
        assert_eq!(
            s.reserve_fetch_slot(Duration::seconds(75)).await.unwrap(),
            until
        );
    }

    #[tokio::test]
    async fn a_block_after_a_reservation_is_seen_by_its_holder() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        assert!(s.fetch_allowed().await.unwrap());
        s.reserve_fetch_slot(Duration::seconds(75)).await.unwrap();
        assert!(s.fetch_allowed().await.unwrap());
        let later = Utc::now() + Duration::hours(1);
        s.block_fetches(later).await.unwrap();
        assert!(!s.fetch_allowed().await.unwrap());
        // Reserving keeps the block on record.
        s.reserve_fetch_slot(Duration::seconds(75)).await.unwrap();
        assert!(!s.fetch_allowed().await.unwrap());
        // A block that has passed doesn't hold anything.
        let s2 = State::new(Arc::new(MemoryDocs::default()));
        let earlier = Utc::now() - Duration::seconds(1);
        s2.block_fetches(earlier).await.unwrap();
        assert!(s2.fetch_allowed().await.unwrap());
    }

    #[tokio::test]
    async fn concurrent_renewals_by_the_holder_both_succeed() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        s.lock("w", "a", Duration::hours(1)).await.unwrap();
        for _ in 0..20 {
            let (x, y) = tokio::join!(
                s.lock("w", "a", Duration::hours(1)),
                s.lock("w", "a", Duration::hours(1))
            );
            x.unwrap();
            y.unwrap();
        }
    }

    #[tokio::test]
    async fn lock_names_that_cosmos_cannot_hold_are_refused() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        for name in ["ops/congress-globe", "a\\b", "a?b", "a#b", ""] {
            assert!(
                s.lock(name, "a", Duration::hours(1)).await.is_err(),
                "{name:?}"
            );
            assert!(s.unlock(name, "a").await.is_err(), "{name:?}");
        }
        s.lock("congress-globe", "a", Duration::hours(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn writer_lock_is_exclusive_until_it_expires_or_is_released() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        s.lock("quickwit-writer", "a", Duration::hours(2))
            .await
            .unwrap();
        // Re-entrant for the same owner, exclusive for others.
        s.lock("quickwit-writer", "a", Duration::hours(2))
            .await
            .unwrap();
        assert!(s
            .lock("quickwit-writer", "b", Duration::hours(2))
            .await
            .is_err());
        s.unlock("quickwit-writer", "b").await.unwrap();
        assert!(s
            .lock("quickwit-writer", "b", Duration::hours(2))
            .await
            .is_err());
        s.unlock("quickwit-writer", "a").await.unwrap();
        s.lock("quickwit-writer", "b", Duration::seconds(-1))
            .await
            .unwrap();
        // An expired lease can be taken over.
        s.lock("quickwit-writer", "c", Duration::hours(1))
            .await
            .unwrap();
    }

    /// A live holder is taken over only when `gone` says so; the plain
    /// lock never does.
    #[tokio::test]
    async fn a_gone_holder_is_taken_over_before_its_lease_runs_out() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        let ttl = Duration::minutes(30);
        s.lock("congress-serialset", "a", ttl).await.unwrap();
        let gone_a = |holder: &str| holder == "a";
        assert!(s.lock("congress-serialset", "b", ttl).await.is_err());
        let err = s
            .lock_or_take_over("congress-serialset", "b", ttl, &|_| false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("held by `a`"), "{err}");
        let taken = s
            .lock_or_take_over("congress-serialset", "b", ttl, &gone_a)
            .await
            .unwrap();
        assert_eq!(taken.as_deref(), Some("a"));
        // `b` holds it now, and `gone` says nothing about it.
        assert!(s
            .lock_or_take_over("congress-serialset", "c", ttl, &gone_a)
            .await
            .is_err());
        // Its own renewal, a free lock and an expired one take nothing over.
        let renewed = s
            .lock_or_take_over("congress-serialset", "b", ttl, &gone_a)
            .await
            .unwrap();
        assert_eq!(renewed, None);
        s.unlock("congress-serialset", "b").await.unwrap();
        let free = s
            .lock_or_take_over("congress-serialset", "c", Duration::seconds(-1), &|_| true)
            .await
            .unwrap();
        assert_eq!(free, None);
        let expired = s
            .lock_or_take_over("congress-serialset", "d", ttl, &|_| true)
            .await
            .unwrap();
        assert_eq!(expired, None);
    }

    fn activity(owner: &str, at: DateTime<Utc>) -> Activity {
        Activity {
            command: "run".into(),
            owner: owner.into(),
            started_at: at,
            step: Step::Titles,
            step_started_at: at,
            updated_at: at,
            done: None,
            total: None,
            paused_until: None,
            index_version: None,
            merge: None,
            ended_at: None,
            outcome: None,
            error: None,
            previous: None,
        }
    }

    async fn held(s: &State) -> Activity {
        let v = s.docs.get(OPS, ACTIVITY, ACTIVITY).await.unwrap().unwrap();
        from::<Activity>(v).unwrap().0
    }

    #[tokio::test]
    async fn a_new_execution_keeps_how_the_last_one_ended() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        let now = Utc::now();
        let mut first = activity("first", now);
        assert!(s.set_activity(&first).await.unwrap());
        assert!(held(&s).await.previous.is_none());
        first.ended_at = Some(now);
        first.outcome = Some(Outcome::TitlesLeft);
        first.error = Some("titles-sync reached its deadline".into());
        assert!(s.set_activity(&first).await.unwrap());

        // The next execution takes over; the first's end is kept.
        let mut second = activity("second", now);
        assert!(s.set_activity(&second).await.unwrap());
        let got = held(&s).await;
        assert_eq!(got.owner, "second");
        let previous = got.previous.unwrap();
        assert_eq!(
            (previous.owner.as_str(), previous.outcome),
            ("first", Some(Outcome::TitlesLeft))
        );
        assert!(previous.previous.is_none());

        // Its own heartbeats and its end keep it.
        second.step = Step::Indexing;
        assert!(s.set_activity(&second).await.unwrap());
        second.ended_at = Some(now);
        second.outcome = Some(Outcome::Failed);
        assert!(s.set_activity(&second).await.unwrap());
        assert_eq!(held(&s).await.previous.unwrap().owner, "first");

        // The one after keeps only the second: the chain stays one deep.
        assert!(s.set_activity(&activity("third", now)).await.unwrap());
        let previous = held(&s).await.previous.unwrap();
        assert_eq!(
            (previous.owner.as_str(), previous.outcome),
            ("second", Some(Outcome::Failed))
        );
        assert!(previous.previous.is_none());
        let raw = s
            .docs
            .get(OPS, ACTIVITY, ACTIVITY)
            .await
            .unwrap()
            .unwrap()
            .doc;
        assert!(raw["previous"].get("id").is_none() && raw["previous"].get("kind").is_none());
    }

    #[tokio::test]
    async fn a_stale_execution_is_kept_as_it_was_left() {
        let s = State::new(Arc::new(MemoryDocs::default()));
        let old = Utc::now() - Duration::seconds(ACTIVITY_STALE_SECS + 60);
        assert!(s.set_activity(&activity("killed", old)).await.unwrap());
        assert!(s.set_activity(&activity("next", Utc::now())).await.unwrap());
        let previous = held(&s).await.previous.unwrap();
        assert_eq!(
            (
                previous.owner.as_str(),
                previous.ended_at,
                previous.updated_at
            ),
            ("killed", None, old)
        );
    }

    #[test]
    fn an_unreadable_previous_is_dropped() {
        let mut v = serde_json::to_value(activity("a", Utc::now())).unwrap();
        v["previous"] = serde_json::json!({"owner": 3});
        let a: Activity = serde_json::from_value(v).unwrap();
        assert!(a.previous.is_none());
    }
}
