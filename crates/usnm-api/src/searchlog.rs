//! The anonymous search log (06 §6.8, ADR-0012).
//!
//! Every search a visitor runs on the site is kept, indefinitely, in the
//! `searches` container as one line of JSON ([`Record`]): the words, the
//! match mode and filters, the number of pages matched, the index version
//! and the UTC day. Nothing that identifies or links a person is stored
//! with it: no address, user agent, referrer, cookie, request or session id,
//! location, or time of day.
//!
//! A search counts once, on `/v1/aggregate`, which the site requests once
//! per search; the coverage and hits requests that follow aren't counted.
//! It counts when the response is a 200, from a cache or not. Not counted
//! ([`admit`]): the warm-up's queries (they don't pass through the handler),
//! requests with `DNT: 1` or `Sec-GPC: 1`, crawler, script and headless user
//! agents, and requests that don't come from the site's own pages.
//!
//! The handler hands a record to a bounded queue and never waits: when the
//! queue is full the record is dropped and counted. A background task
//! batches the records and, every `flush_interval` and at shutdown, writes
//! each day's batch in random order to its own blob,
//! `staging/{day}/{random id}.jsonl`, create-only. A failed write is retried
//! under the same name, so a write that succeeded without the API hearing
//! back isn't stored twice. Records for a day that still aren't staged
//! [`STAGE_UNTIL`] after it ends are dropped. [`CLOSE_AFTER`] after a day
//! ends, a replica lists that day's staged batches, shuffles all of their
//! lines together and writes them once to `days/{day}.jsonl`, which is never
//! rewritten. A lifecycle rule deletes staged batches after
//! [`STAGING_DAYS`] days. So the permanent file's line order says nothing
//! about when in the day a search ran, and the only time a record carries is
//! its day.
//!
//! Search text never goes to the console or to telemetry, write errors
//! included: errors name the blob path (`staging/2026-09-29/….jsonl`) and the
//! number of records, never their content.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use chrono::{DateTime, Days, NaiveDate, Utc};
use opentelemetry::metrics::{Counter, Meter};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use usnm_core::params::{RawParams, SearchRequest};
use usnm_core::query::{Mode, MAX_QUERY_CHARS};
use usnm_core::text::Analyzers;
use usnm_store::ObjectStore;

// The same crawler, script and headless classification as page views (06 §6.3.7).
use crate::routes::is_bot;

/// The record format; bump it when a field changes meaning.
pub const FORMAT: u32 = 1;

/// Records for a day are staged until this long after it ends (UTC), then
/// dropped as lost if they still aren't, so none can arrive after the day
/// file is written.
pub const STAGE_UNTIL: Duration = Duration::from_secs(45 * 60);

/// A day is written to `days/` this long after it ends (UTC): after
/// [`STAGE_UNTIL`] plus [`WRITE_LIMIT`], with room for clock skew.
pub const CLOSE_AFTER: Duration = Duration::from_secs(3600);

/// The longest one batch write may take; a slower one counts as failed and
/// is retried under the same name.
pub const WRITE_LIMIT: Duration = Duration::from_secs(60);

/// Staged batches are deleted this many days after they are written (the
/// `expire-search-staging` lifecycle rule in `infra/modules/storage.bicep`).
/// A day not written to `days/` within that time is lost.
pub const STAGING_DAYS: u64 = 7;

/// How often a replica looks for ended days to write to `days/`.
const CLOSE_CHECK: Duration = Duration::from_secs(3600);

const CONTENT_TYPE: &str = "application/x-ndjson";

/// One search, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// [`FORMAT`].
    pub v: u32,
    /// The UTC day of the search: the only time kept.
    pub day: NaiveDate,
    /// `api` (recorded by the API). `import` is reserved for records copied
    /// from elsewhere; none exist.
    pub source: String,
    /// The words as submitted: trimmed, runs of spaces and control
    /// characters folded to one space, at most 256 characters. Imports have
    /// the canonical query here too.
    pub q: String,
    /// The query as the API ran it, in canonical form: the same rendering
    /// as `q=` in the response's canonical URL (06 §6.3.1).
    pub query: String,
    /// `phrase`, `all`, `any` or `near`; `null` when none was sent (query
    /// syntax), and for imports.
    pub mode: Option<String>,
    /// The `near` distance, with `mode` `near`.
    pub near: Option<u8>,
    pub fuzzy: u8,
    /// The date range searched, after clamping to the corpus.
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub state: Vec<String>,
    pub lccn: Vec<String>,
    pub lang: Vec<String>,
    pub front: bool,
    /// The corpus searched when it isn't the newspapers (#229): `congress`
    /// or `all`. Absent for newspaper searches, so their records are as
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub corpus: Option<String>,
    /// The Congress series filter (#229); absent when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<String>,
    /// The bucket asked for (`year`, `month`, `week` or `day`).
    pub bucket: String,
    /// Pages matched (`total.hits`); `null` when unknown (imports).
    pub pages: Option<u64>,
    /// The index version searched; `null` for imports.
    pub index_version: Option<String>,
}

/// Whether a search request goes in the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Record,
    /// Not recorded, with the reason for `api.search_log`.
    Excluded(&'static str),
}

/// Decide from the request headers alone. `site_host` is the site's
/// canonical hostname (`usnewsmap.com`).
pub fn admit(headers: &HeaderMap, site_host: &str) -> Admission {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    if header("dnt") == Some("1") || header("sec-gpc") == Some("1") {
        return Admission::Excluded("opted_out");
    }
    // The site calls the API on its own origin. Browsers send Sec-Fetch-Site
    // with every fetch, and Origin only on some; either one has to say the
    // request came from the site, and neither may say otherwise. Requests
    // typed into the address bar (`none`), from other sites and from
    // scripts without these headers don't count.
    let own = format!("https://{site_host}");
    let origin = header("origin");
    let fetch_site = header("sec-fetch-site");
    if origin.is_some_and(|o| o != own)
        || fetch_site.is_some_and(|s| s != "same-origin")
        || (origin.is_none() && fetch_site.is_none())
    {
        return Admission::Excluded("other_origin");
    }
    if is_bot(header("user-agent").unwrap_or_default()) {
        return Admission::Excluded("bot");
    }
    Admission::Record
}

/// What the handler knows about a search, before the response exists.
#[derive(Debug, Clone)]
pub(crate) struct Search {
    q: String,
    mode: Option<Mode>,
    near: Option<u8>,
    fuzzy: u8,
    req: SearchRequest,
    index_version: String,
}

impl Search {
    /// From a request that validated (`req` was built from `raw`).
    pub(crate) fn new(raw: &RawParams, req: &SearchRequest, index_version: &str) -> Self {
        let mode = raw.get("mode").and_then(Mode::parse);
        let near = match mode {
            Some(Mode::Near) => raw.get("near").and_then(|n| n.parse().ok()),
            _ => None,
        };
        Self {
            q: normalize(raw.get("q").unwrap_or_default()),
            mode,
            near,
            fuzzy: raw
                .get("fuzzy")
                .and_then(|f| f.parse().ok())
                .unwrap_or_default(),
            req: req.clone(),
            index_version: index_version.to_owned(),
        }
    }
}

/// The words as submitted, trimmed, with runs of whitespace and control
/// characters folded to one space and at most [`MAX_QUERY_CHARS`] characters
/// (the API refuses longer queries anyway).
pub fn normalize(q: &str) -> String {
    let spaced: String = q
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let folded = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    folded.chars().take(MAX_QUERY_CHARS).collect()
}

/// A search with its response body (for the page count), queued for the writer.
pub(crate) struct Entry {
    at: DateTime<Utc>,
    search: Search,
    body: Arc<Vec<u8>>,
}

impl Entry {
    pub(crate) fn new(at: DateTime<Utc>, search: Search, body: Arc<Vec<u8>>) -> Self {
        Self { at, search, body }
    }

    /// The stored record: the day of `at`, never the time.
    pub(crate) fn record(&self) -> Record {
        let s = &self.search;
        let f = &s.req.filters;
        Record {
            v: FORMAT,
            day: self.at.date_naive(),
            source: "api".to_owned(),
            q: s.q.clone(),
            query: s.req.query.to_string(),
            mode: s.mode.map(|m| m.as_str().to_owned()),
            near: s.near,
            fuzzy: s.fuzzy,
            from: f.from,
            to: f.to,
            state: f.states.clone(),
            lccn: f.lccns.clone(),
            lang: f.langs.clone(),
            front: f.front_only,
            corpus: (s.req.source != usnm_core::params::Source::News)
                .then(|| s.req.source.as_str().to_owned()),
            series: f.series.clone(),
            bucket: s.req.bucket.as_str().to_owned(),
            pages: pages(&self.body),
            index_version: Some(s.index_version.clone()),
        }
    }
}

/// `total.hits` from an aggregate response body.
fn pages(body: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Body {
        total: Total,
    }
    #[derive(Deserialize)]
    struct Total {
        hits: u64,
    }
    serde_json::from_slice::<Body>(body)
        .ok()
        .map(|b| b.total.hits)
}

/// `staging/{day}`: the day's batches, one blob each.
pub fn staging_dir(day: NaiveDate) -> String {
    format!("staging/{day}")
}

/// A new batch's blob: `staging/{day}/{32 random hex digits}.jsonl`.
fn batch_path(day: NaiveDate) -> String {
    let keys = RandomState::new();
    format!(
        "{}/{:016x}{:016x}.jsonl",
        staging_dir(day),
        keys.hash_one(0u8),
        keys.hash_one(1u8)
    )
}

/// `days/{day}.jsonl`: the day's records in random order, written once.
pub fn day_path(day: NaiveDate) -> String {
    format!("days/{day}.jsonl")
}

/// Shuffle in place (Fisher-Yates), keyed by a fresh random SipHash key.
fn shuffle<T>(items: &mut [T]) {
    let keys = RandomState::new();
    for i in (1..items.len()).rev() {
        let j = (keys.hash_one(i) % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

/// Tuning for the writer.
#[derive(Debug, Clone)]
pub struct LogConfig {
    /// How often batches are written.
    pub flush_interval: Duration,
    /// Records the handlers can queue before new ones are dropped.
    pub queue: usize,
    /// A batch this big is written at once.
    pub max_batch: usize,
    /// Records kept for another try after failed writes; beyond this the
    /// oldest batches are dropped.
    pub max_pending: usize,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            flush_interval: Duration::from_secs(300),
            queue: 1024,
            max_batch: 2000,
            max_pending: 20_000,
        }
    }
}

#[derive(Clone)]
struct LogMetrics {
    /// Searches by `outcome`: queued, dropped (queue full), written, lost
    /// (a failed or late write), and the exclusions (opted_out, bot, other_origin).
    records: Counter<u64>,
    /// Blob operations by `op` (stage, close) and `outcome` (ok, error).
    writes: Counter<u64>,
}

impl LogMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            records: meter
                .u64_counter("api.search_log")
                .with_description(
                    "Searches for the search log by outcome (queued, dropped, written, lost, \
                     opted_out, bot, other_origin)",
                )
                .build(),
            writes: meter
                .u64_counter("api.search_log_writes")
                .with_description("Search log blob writes by op (stage, close) and outcome")
                .build(),
        }
    }

    fn records(&self, outcome: &'static str, n: usize) {
        if n > 0 {
            self.records
                .add(n as u64, &[KeyValue::new("outcome", outcome)]);
        }
    }

    fn write(&self, op: &'static str, ok: bool) {
        self.writes.add(
            1,
            &[
                KeyValue::new("op", op),
                KeyValue::new("outcome", if ok { "ok" } else { "error" }),
            ],
        );
    }
}

/// The handlers' side of the search log.
pub struct SearchLog {
    /// Where the log is written, and read back by the cache warm-up.
    store: Arc<dyn ObjectStore>,
    tx: mpsc::Sender<Entry>,
    stop: watch::Sender<bool>,
    worker: Mutex<Option<JoinHandle<()>>>,
    metrics: LogMetrics,
}

impl std::fmt::Debug for SearchLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchLog").finish_non_exhaustive()
    }
}

impl SearchLog {
    /// Start the writer on the current runtime.
    pub fn start(store: Arc<dyn ObjectStore>, config: LogConfig, meter: &Meter) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(config.queue.max(1));
        let (stop, stopped) = watch::channel(false);
        let metrics = LogMetrics::new(meter);
        let worker = Writer {
            store: store.clone(),
            config,
            rx,
            stopped,
            pending: Vec::new(),
            batches: Vec::new(),
            closed: BTreeSet::new(),
            metrics: metrics.clone(),
        };
        Arc::new(Self {
            store,
            tx,
            stop,
            worker: Mutex::new(Some(tokio::spawn(worker.run()))),
            metrics,
        })
    }

    /// The store the log is written to.
    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    /// Queue a search without waiting; a full queue drops it.
    pub(crate) fn record(&self, entry: Entry) {
        match self.tx.try_send(entry) {
            Ok(()) => self.metrics.records("queued", 1),
            Err(_) => self.metrics.records("dropped", 1),
        }
    }

    /// Count a search that isn't recorded.
    pub(crate) fn excluded(&self, reason: &'static str) {
        self.metrics.records(reason, 1);
    }

    /// Write what's queued and stop the writer, waiting at most `limit`.
    pub async fn shutdown(&self, limit: Duration) {
        let _ = self.stop.send(true);
        let worker = self.worker.lock().expect("not poisoned").take();
        if let Some(worker) = worker {
            if tokio::time::timeout(limit, worker).await.is_err() {
                tracing::warn!("search log: the last write didn't finish in time");
            }
        }
    }
}

/// One day's records, shuffled, under the blob name it keeps across retries.
struct Batch {
    day: NaiveDate,
    path: String,
    body: Vec<u8>,
    records: usize,
}

struct Writer {
    store: Arc<dyn ObjectStore>,
    config: LogConfig,
    rx: mpsc::Receiver<Entry>,
    stopped: watch::Receiver<bool>,
    /// Records not yet in a batch, in arrival order.
    pending: Vec<Record>,
    /// Batches waiting to be written (again), oldest first.
    batches: Vec<Batch>,
    /// Days already written to `days/` (or found there).
    closed: BTreeSet<NaiveDate>,
    metrics: LogMetrics,
}

impl Writer {
    async fn run(mut self) {
        let mut flush = tokio::time::interval(self.config.flush_interval);
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        flush.tick().await;
        // The first tick is immediate: close days that ended while no
        // replica was running.
        let mut close = tokio::time::interval(CLOSE_CHECK);
        close.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = self.stopped.changed() => break,
                entry = self.rx.recv() => match entry {
                    Some(e) => {
                        self.pending.push(e.record());
                        if self.pending.len() >= self.config.max_batch {
                            self.flush().await;
                        }
                    }
                    None => break,
                },
                _ = flush.tick() => self.flush().await,
                _ = close.tick() => self.close_days(Utc::now()).await,
            }
        }
        while let Ok(e) = self.rx.try_recv() {
            self.pending.push(e.record());
        }
        self.flush().await;
    }

    /// Turn the pending records into one batch per day, shuffled, then write
    /// every batch not yet written. A batch whose write fails is retried
    /// under the same name at the next flush, until its day's
    /// [`STAGE_UNTIL`] has passed.
    async fn flush(&mut self) {
        let mut by_day: BTreeMap<NaiveDate, Vec<Record>> = BTreeMap::new();
        for r in self.pending.drain(..) {
            by_day.entry(r.day).or_default().push(r);
        }
        for (day, mut records) in by_day {
            shuffle(&mut records);
            let mut body = Vec::new();
            for r in &records {
                serde_json::to_writer(&mut body, r).expect("records serialize");
                body.push(b'\n');
            }
            self.batches.push(Batch {
                day,
                path: batch_path(day),
                body,
                records: records.len(),
            });
        }
        let mut kept = Vec::new();
        for batch in std::mem::take(&mut self.batches) {
            // The clock is read before each write, and a write gets at most
            // WRITE_LIMIT, so every write ends before the day can close.
            if !stageable(batch.day, Utc::now()) || self.closed.contains(&batch.day) {
                // Its day file is written, or about to be.
                self.metrics.records("lost", batch.records);
                continue;
            }
            // Create-only: `false` means an earlier try wrote it.
            let write = self
                .store
                .put_new(&batch.path, batch.body.clone(), CONTENT_TYPE);
            let written = match tokio::time::timeout(WRITE_LIMIT, write).await {
                Ok(r) => r,
                Err(_) => Err(usnm_store::StoreError::Io(format!(
                    "put `{}`: no answer in {} s",
                    batch.path,
                    WRITE_LIMIT.as_secs()
                ))),
            };
            match written {
                Ok(_) => {
                    self.metrics.write("stage", true);
                    self.metrics.records("written", batch.records);
                }
                Err(e) => {
                    self.metrics.write("stage", false);
                    // The path and count only: never a record's content.
                    tracing::warn!(
                        error = %e,
                        records = batch.records,
                        "search log write failed; keeping the batch for the next try"
                    );
                    kept.push(batch);
                }
            }
        }
        let mut total: usize = kept.iter().map(|b| b.records).sum();
        while total > self.config.max_pending {
            let oldest = kept.remove(0);
            total -= oldest.records;
            self.metrics.records("lost", oldest.records);
        }
        self.batches = kept;
    }

    /// Write `days/{day}.jsonl` for each day that ended at least
    /// [`CLOSE_AFTER`] before `now` and still has a staging file.
    async fn close_days(&mut self, now: DateTime<Utc>) {
        let today = now.date_naive();
        for back in 1..STAGING_DAYS {
            let Some(day) = today.checked_sub_days(Days::new(back)) else {
                continue;
            };
            if !closable(day, now) || self.closed.contains(&day) {
                continue;
            }
            match close_day(self.store.as_ref(), day).await {
                Ok(()) => {
                    self.metrics.write("close", true);
                    self.closed.insert(day);
                }
                Err(e) => {
                    self.metrics.write("close", false);
                    tracing::warn!(error = %e, %day, "search log: writing the day file failed");
                }
            }
        }
        if let Some(oldest) = today.checked_sub_days(Days::new(STAGING_DAYS)) {
            self.closed.retain(|d| *d >= oldest);
        }
    }
}

/// Whether `day` ended at least `after` before `now`.
fn ended(day: NaiveDate, now: DateTime<Utc>, after: Duration) -> bool {
    day.succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .is_some_and(|end| {
            now.signed_duration_since(end.and_utc())
                .to_std()
                .is_ok_and(|d| d >= after)
        })
}

/// Whether records for `day` may still be staged at `now`.
pub fn stageable(day: NaiveDate, now: DateTime<Utc>) -> bool {
    !ended(day, now, STAGE_UNTIL)
}

/// Whether `day` can be written to `days/` at `now`.
pub fn closable(day: NaiveDate, now: DateTime<Utc>) -> bool {
    ended(day, now, CLOSE_AFTER)
}

/// Largest day file read back for the warm-up.
const MAX_DAY_BYTES: usize = 32 * 1024 * 1024;

/// Most distinct searches the warm-up counts; past this, searches not yet
/// seen are skipped (those already counted keep counting), so memory stays
/// bounded however varied the log is.
const MAX_DISTINCT: usize = 50_000;

/// The cache key part of the search a record describes: the canonical query
/// string `/v1/aggregate` computes it under (06 §6.3.1), for the corpus
/// `bounds` and the `analyzers` of the version serving now. `None` if it no
/// longer parses, and for a search of Congress pages (`corpus` or `series`
/// set), which isn't warmed yet (#229).
pub fn canonical(
    r: &Record,
    bounds: (NaiveDate, NaiveDate),
    analyzers: Analyzers,
) -> Option<String> {
    // Its other fields alone would rebuild a newspaper search.
    if r.corpus.is_some() || !r.series.is_empty() {
        return None;
    }
    let mut s = form_urlencoded::Serializer::new(String::new());
    // The canonical query, without a mode, parses to the same search
    // (`Node`'s Display is its canonical rendering).
    s.append_pair("q", &r.query);
    s.append_pair("from", &r.from.to_string());
    s.append_pair("to", &r.to.to_string());
    s.append_pair("bucket", &r.bucket);
    for (key, list) in [("state", &r.state), ("lccn", &r.lccn), ("lang", &r.lang)] {
        if !list.is_empty() {
            s.append_pair(key, &list.join(","));
        }
    }
    if r.front {
        s.append_pair("front", "true");
    }
    let raw = RawParams::parse(&s.finish()).ok()?;
    SearchRequest::from_raw(&raw, bounds, analyzers)
        .ok()
        .map(|req| req.canonical())
}

/// The `n` searches made most often in the `days` days before `today`
/// (UTC), from the day files: canonical aggregate query strings, most
/// frequent first, ties in canonical order, so the result depends only on
/// the files. Searches that matched no pages are fast anyway and left out,
/// as are searches of Congress pages, which aren't warmed yet (#229).
/// At most [`MAX_DISTINCT`] different searches are counted.
/// A day file that can't be read is logged with its day and skipped; no
/// search text is ever logged.
pub async fn top_searches(
    store: &dyn ObjectStore,
    today: NaiveDate,
    days: u64,
    n: usize,
    bounds: (NaiveDate, NaiveDate),
    analyzers: Analyzers,
) -> Vec<String> {
    if n == 0 {
        return Vec::new();
    }
    let mut counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for back in 1..=days {
        let Some(day) = today.checked_sub_days(Days::new(back)) else {
            break;
        };
        let body = match store.get(&day_path(day)).await {
            Ok(Some(body)) => body,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!(%day, error = %e, "warm-up: reading a search log day failed");
                continue;
            }
        };
        if body.len() > MAX_DAY_BYTES {
            tracing::warn!(%day, bytes = body.len(), "warm-up: search log day too large; skipped");
            continue;
        }
        // Parsing a large day is CPU work: off the async workers.
        let keys = tokio::task::spawn_blocking(move || {
            body.split(|c| *c == b'\n')
                .filter_map(|line| serde_json::from_slice::<Record>(line).ok())
                .filter(|r| r.pages != Some(0))
                .filter_map(|r| canonical(&r, bounds, analyzers))
                .collect::<Vec<String>>()
        })
        .await
        .unwrap_or_default();
        for key in keys {
            if let Some(n) = counts.get_mut(&key) {
                *n += 1;
            } else if counts.len() < MAX_DISTINCT {
                counts.insert(key, 1);
            }
        }
    }
    let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
    ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(n);
    ranked.into_iter().map(|(key, _)| key).collect()
}

/// Shuffle all of a day's staged lines together and write them to its day
/// file, unless the file exists (another replica wrote it) or nothing was
/// staged.
pub async fn close_day(
    store: &dyn ObjectStore,
    day: NaiveDate,
) -> Result<(), usnm_store::StoreError> {
    if store.exists(&day_path(day)).await? {
        return Ok(());
    }
    let mut staged = Vec::new();
    for path in store.list(&staging_dir(day)).await? {
        if let Some(batch) = store.get(&path).await? {
            staged.push(batch);
        }
    }
    let mut lines: Vec<&[u8]> = staged
        .iter()
        .flat_map(|b| b.split(|c| *c == b'\n'))
        .filter(|l| !l.trim_ascii().is_empty())
        .collect();
    if lines.is_empty() {
        return Ok(());
    }
    shuffle(&mut lines);
    let mut body = Vec::with_capacity(staged.iter().map(Vec::len).sum::<usize>() + 1);
    for line in lines {
        body.extend_from_slice(line);
        body.push(b'\n');
    }
    store.put_new(&day_path(day), body, CONTENT_TYPE).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn site() -> Vec<(&'static str, &'static str)> {
        vec![("user-agent", CHROME), ("sec-fetch-site", "same-origin")]
    }

    #[test]
    fn a_browser_on_the_site_is_recorded() {
        assert_eq!(admit(&headers(&site()), "usnewsmap.com"), Admission::Record);
        let mut h = site();
        h.push(("origin", "https://usnewsmap.com"));
        assert_eq!(admit(&headers(&h), "usnewsmap.com"), Admission::Record);
        // Origin alone (no Sec-Fetch-Site) is enough.
        let h = headers(&[("user-agent", CHROME), ("origin", "https://usnewsmap.com")]);
        assert_eq!(admit(&h, "usnewsmap.com"), Admission::Record);
        // DNT: 0 is not an opt-out.
        let mut h = site();
        h.push(("dnt", "0"));
        assert_eq!(admit(&headers(&h), "usnewsmap.com"), Admission::Record);
    }

    #[test]
    fn dnt_and_gpc_are_honored() {
        for opt in [("dnt", "1"), ("sec-gpc", "1"), ("dnt", " 1 ")] {
            let mut h = site();
            h.push(opt);
            assert_eq!(
                admit(&headers(&h), "usnewsmap.com"),
                Admission::Excluded("opted_out"),
                "{opt:?}"
            );
        }
    }

    #[test]
    fn other_origins_are_excluded() {
        let other = Admission::Excluded("other_origin");
        for extra in [
            vec![("origin", "https://example.com")],
            vec![("origin", "https://www.usnewsmap.com")],
            vec![("origin", "http://usnewsmap.com")],
            vec![("sec-fetch-site", "cross-site")],
            vec![("sec-fetch-site", "same-site")],
            // Typed into the address bar.
            vec![("sec-fetch-site", "none")],
        ] {
            let mut h = vec![("user-agent", CHROME)];
            h.extend(extra.iter().copied());
            if !extra.iter().any(|(k, _)| *k == "sec-fetch-site") {
                h.push(("sec-fetch-site", "same-origin"));
            }
            assert_eq!(admit(&headers(&h), "usnewsmap.com"), other, "{extra:?}");
        }
        // Neither header: not a page on the site.
        assert_eq!(
            admit(&headers(&[("user-agent", CHROME)]), "usnewsmap.com"),
            other
        );
    }

    #[test]
    fn bots_are_excluded() {
        for ua in [
            "",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/140.0.0.0 Safari/537.36",
            "curl/8.7.1",
            "python-requests/2.32",
        ] {
            let h = headers(&[("user-agent", ua), ("sec-fetch-site", "same-origin")]);
            assert_eq!(
                admit(&h, "usnewsmap.com"),
                Admission::Excluded("bot"),
                "{ua}"
            );
        }
    }

    #[test]
    fn queries_are_trimmed_folded_and_capped() {
        assert_eq!(normalize("  Cross   of\tGold \n"), "Cross of Gold");
        assert_eq!(normalize("a\u{0}b\u{7}c"), "a b c");
        assert_eq!(normalize(&"é".repeat(300)).chars().count(), MAX_QUERY_CHARS);
    }

    #[test]
    fn shuffle_keeps_every_item() {
        let mut v: Vec<u32> = (0..1000).collect();
        shuffle(&mut v);
        assert_ne!(v, (0..1000).collect::<Vec<_>>());
        v.sort_unstable();
        assert_eq!(v, (0..1000).collect::<Vec<_>>());
        let mut one = [7];
        shuffle(&mut one);
        assert_eq!(one, [7]);
    }

    fn search(query: &str) -> Search {
        let raw = RawParams::parse(query).unwrap();
        let bounds = (
            NaiveDate::from_ymd_opt(1770, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(1963, 12, 31).unwrap(),
        );
        let req = SearchRequest::from_raw(&raw, bounds, Analyzers::default()).unwrap();
        Search::new(&raw, &req, "pages-v1")
    }

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn a_record_holds_the_search_and_its_day_only() {
        let body = Arc::new(br#"{"total":{"hits":1234}}"#.to_vec());
        let entry = Entry::new(
            at("2026-09-29T23:59:59.999Z"),
            search("q=++Cross+of+Gold+&mode=phrase&from=1896-06-01&to=1896-12-31&state=sc,GA&bucket=week&v=pages-v1"),
            body.clone(),
        );
        let json = serde_json::to_value(entry.record()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "v": 1,
                "day": "2026-09-29",
                "source": "api",
                "q": "Cross of Gold",
                "query": "\"cross of gold\"",
                "mode": "phrase",
                "near": null,
                "fuzzy": 0,
                "from": "1896-06-01",
                "to": "1896-12-31",
                "state": ["GA", "SC"],
                "lccn": [],
                "lang": [],
                "front": false,
                "bucket": "week",
                "pages": 1234,
                "index_version": "pages-v1"
            })
        );
        // Nothing finer than the day, and nothing about the client.
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in [
            "ip",
            "user_agent",
            "referrer",
            "session",
            "request_id",
            "timestamp",
            "time",
            "hour",
            "city",
            "country",
        ] {
            assert!(!keys.contains(&forbidden), "{forbidden}");
        }
        let text = json.to_string();
        assert!(!text.contains("23:59"), "{text}");
        // The day is the UTC day of the search.
        let next = Entry::new(at("2026-09-30T00:00:00Z"), search("q=gold"), body.clone());
        assert_eq!(
            next.record().day,
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
        let offset = Entry::new(at("2026-09-29T21:30:00-05:00"), search("q=gold"), body);
        assert_eq!(
            offset.record().day,
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
    }

    #[test]
    fn modes_near_and_syntax() {
        let body = Arc::new(Vec::new());
        let r = Entry::new(
            at("2026-09-29T12:00:00Z"),
            search("q=gold+silver&mode=near&near=5"),
            body.clone(),
        )
        .record();
        assert_eq!((r.mode.as_deref(), r.near), (Some("near"), Some(5)));
        assert_eq!(r.query, "\"gold silver\"~5");
        let r = Entry::new(
            at("2026-09-29T12:00:00Z"),
            search("q=gold+OR+silver&front=true&lang=ENG"),
            body,
        )
        .record();
        assert_eq!((r.mode, r.near, r.pages), (None, None, None));
        assert_eq!(r.query, "gold OR silver");
        assert!(r.front);
        assert_eq!(r.lang, ["eng"]);
        assert_eq!(r.bucket, "year");
    }

    #[test]
    fn records_are_staged_until_45_minutes_after_their_day() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        assert!(stageable(day, at("2026-09-29T12:00:00Z")));
        assert!(stageable(day, at("2026-09-30T00:44:59Z")));
        assert!(!stageable(day, at("2026-09-30T00:45:00Z")));
        // A write that starts at the limit ends 14 minutes before the day closes.
        assert!(STAGE_UNTIL + WRITE_LIMIT + Duration::from_secs(14 * 60) <= CLOSE_AFTER);
    }

    #[test]
    fn batch_names_are_random_and_valid_paths() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let (a, b) = (batch_path(day), batch_path(day));
        assert_ne!(a, b);
        assert!(a.starts_with("staging/2026-09-29/") && a.ends_with(".jsonl"));
        usnm_store::validate_path(&a).unwrap();
    }

    #[test]
    fn a_day_closes_an_hour_after_it_ends() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        assert!(!closable(day, at("2026-09-29T23:00:00Z")));
        assert!(!closable(day, at("2026-09-30T00:59:59Z")));
        assert!(closable(day, at("2026-09-30T01:00:00Z")));
        assert!(closable(day, at("2026-10-03T12:00:00Z")));
    }

    fn bounds() -> (NaiveDate, NaiveDate) {
        (
            NaiveDate::from_ymd_opt(1770, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(1963, 12, 31).unwrap(),
        )
    }

    #[test]
    fn a_record_gives_back_its_request_s_cache_key() {
        let body = Arc::new(br#"{"total":{"hits":3}}"#.to_vec());
        for query in [
            "q=Cross+of+Gold&mode=phrase&from=1896-06-01&to=1896-12-31&state=sc,GA",
            "q=yellow+fever&mode=near&near=5&bucket=month",
            "q=gold+OR+silver&front=true&lang=ENG&lccn=sn84026749",
            "q=radio&mode=phrase&bucket=month",
            "q=telegra*+-wireless",
            "q=influenza&fuzzy=1&mode=all",
            "q=%22free+silver%22~3&from=1890-01-01",
        ] {
            let raw = RawParams::parse(query).unwrap();
            let req = SearchRequest::from_raw(&raw, bounds(), Analyzers::default()).unwrap();
            let entry = Entry::new(
                at("2026-09-29T12:00:00Z"),
                Search::new(&raw, &req, "v"),
                body.clone(),
            );
            assert_eq!(
                canonical(&entry.record(), bounds(), Analyzers::default()).as_deref(),
                Some(req.canonical().as_str()),
                "{query}"
            );
        }
    }

    /// Congress searches (#229) aren't warmed yet: their records give no
    /// key, rather than the newspaper search their other fields describe.
    #[test]
    fn congress_searches_give_no_cache_key() {
        let body = Arc::new(br#"{"total":{"hits":3}}"#.to_vec());
        let record = |query: &str| {
            let raw = RawParams::parse(query).unwrap();
            let req = SearchRequest::from_raw(&raw, bounds(), Analyzers::default()).unwrap();
            Entry::new(
                at("2026-09-29T12:00:00Z"),
                Search::new(&raw, &req, "v"),
                body.clone(),
            )
            .record()
        };
        for query in [
            "q=gold&source=congress&from=1850-01-01&to=1850-12-31",
            "q=gold&source=congress&series=globe",
            "q=gold&source=all",
        ] {
            let r = record(query);
            assert!(r.corpus.is_some(), "{query}");
            assert_eq!(
                canonical(&r, bounds(), Analyzers::default()),
                None,
                "{query}"
            );
        }
        // A series alone marks one too.
        let mut r = record("q=gold");
        r.series = vec!["globe".into()];
        assert_eq!(canonical(&r, bounds(), Analyzers::default()), None);
        // A newspaper search's key is as before.
        let r = record("q=gold&from=1896-01-01&to=1896-12-31");
        assert_eq!((r.corpus.as_deref(), r.series.len()), (None, 0));
        assert_eq!(
            canonical(&r, bounds(), Analyzers::default()).as_deref(),
            Some("bucket=week&from=1896-01-01&q=gold&to=1896-12-31")
        );
    }

    fn line(q: &str, pages: u64) -> String {
        let raw = RawParams::parse(q).unwrap();
        let req = SearchRequest::from_raw(&raw, bounds(), Analyzers::default()).unwrap();
        let body = Arc::new(format!(r#"{{"total":{{"hits":{pages}}}}}"#).into_bytes());
        let entry = Entry::new(
            at("2026-09-29T12:00:00Z"),
            Search::new(&raw, &req, "v"),
            body,
        );
        serde_json::to_string(&entry.record()).unwrap()
    }

    fn key(q: &str) -> String {
        SearchRequest::from_raw(
            &RawParams::parse(q).unwrap(),
            bounds(),
            Analyzers::default(),
        )
        .unwrap()
        .canonical()
    }

    #[tokio::test]
    async fn the_most_frequent_searches_come_first() {
        let dir = std::env::temp_dir().join(format!("usnm-top-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = usnm_store::LocalStore::new(&dir);
        let today = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let day = |back: u64| today.checked_sub_days(Days::new(back)).unwrap();
        // Two days in the window, one older, and today's (not a day file yet).
        let lines = |ls: &[String]| ls.join("\n").into_bytes();
        store
            .put(
                &day_path(day(1)),
                lines(&[
                    line("q=radio&bucket=month", 9),
                    line("q=Radio&mode=phrase&bucket=month", 9),
                    line("q=television", 4),
                    line("q=nothing", 0),
                    line("q=nothing", 0),
                    line("q=nothing", 0),
                    "not json".to_owned(),
                ]),
                CONTENT_TYPE,
            )
            .await
            .unwrap();
        store
            .put(
                &day_path(day(3)),
                lines(&[
                    line("q=yellow+fever&mode=near&near=5", 2),
                    line("q=television", 4),
                    line("q=cholera", 1),
                ]),
                CONTENT_TYPE,
            )
            .await
            .unwrap();
        store
            .put(&day_path(day(9)), lines(&[line("q=old", 1)]), CONTENT_TYPE)
            .await
            .unwrap();
        let top = top_searches(&store, today, 7, 10, bounds(), Analyzers::default()).await;
        // Counts 2, 2, then 1s in canonical order; zero-hit searches and
        // days outside the window are left out.
        assert_eq!(
            top,
            [
                key("q=radio&bucket=month"),
                key("q=television"),
                // `q=%22yellow…` sorts before `q=cholera`.
                key("q=yellow+fever&mode=near&near=5"),
                key("q=cholera"),
            ]
        );
        assert_eq!(
            top_searches(&store, today, 7, 1, bounds(), Analyzers::default())
                .await
                .len(),
            1
        );
        assert!(
            top_searches(&store, today, 7, 0, bounds(), Analyzers::default())
                .await
                .is_empty()
        );
        assert_eq!(
            top_searches(&store, today, 7, 10, bounds(), Analyzers::default()).await,
            top
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_counts_come_from_the_response() {
        assert_eq!(
            pages(br#"{"total":{"hits":42,"places":3},"x":[1]}"#),
            Some(42)
        );
        assert_eq!(pages(b"not json"), None);
    }
}
