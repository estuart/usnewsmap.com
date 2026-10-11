//! Cache warm-up before a version serves (06 §6.5).
//!
//! The first search against a cold index can take longer than the request
//! timeout, so before a replica serves a version (at start, and before a
//! reload swaps a new one in) it runs what the site's first visitors load:
//! the places layer, and each example search from the home page with the
//! coverage cube its response links to; then, with what is left of the
//! budget, the searches visitors made most often recently, read from the
//! search log (06 §6.8), each with its coverage cube. The requests go through
//! the same handlers as visitors' requests, pinned to the new version, so the
//! search engine's caches and the response caches (in-process, and the
//! persistent cache for slow ones) hold exactly the entries those visitors
//! will ask for.
//!
//! The examples run in the order visitors pick them: those searched most in
//! the search log first, then the rest in the file's order (the home page
//! shows them in a random order per visit, so no order of its own comes
//! first).
//!
//! After a start, a search whose response a cache already holds (the
//! persistent cache, filled by an earlier warm-up or a visitor) is only read
//! into the in-process cache, [`LOADS_AT_ONCE`] at a time: that needs no
//! search. Only the misses are computed. Before a publish nothing is cached
//! for the new version yet, and the run computes every search, which warms
//! the search engine's caches for it too.
//!
//! Computations run one at a time on the warm-up's own slot: searches are
//! CPU-bound, and on a 2-vCPU searcher two at once each took twice as long
//! (06 §6.5). After a start, a computation starts only while no visitor's
//! search is running or queued: once started it keeps the searcher busy
//! until it ends, and a visitor who arrives meanwhile shares the searcher
//! with it. A publish doesn't wait: the old version serves visitors
//! meanwhile, and a run that waited for them through a busy spell could spend
//! its budget and swap in a version it hadn't warmed. Each computation has
//! `config.prewarm_query_timeout` (the searcher's own limit per call still
//! applies), all within `config.prewarm_budget` before a publish, or
//! `config.prewarm_startup_budget` after a start. Nothing here fails: a
//! warm-up that times out or errors is logged and the version serves anyway.
//!
//! Searches from the log are logged by rank only (`search-log-3`), never by
//! their text, and their errors by kind only: an error message can quote
//! the query.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::http::{StatusCode, Uri};
use axum::response::Response;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde::Deserialize;

use usnm_core::params::RawParams;

use crate::error::ApiError;
use crate::routes::{self, Ctx};
use crate::{AppState, Snapshot};

/// A warm-up query slower than this gets its own log line.
const SLOW: Duration = Duration::from_secs(1);

/// Largest response body read back to find the coverage link.
const MAX_BODY: usize = 64 * 1024 * 1024;

/// Responses read from the persistent cache at once after a start. They
/// need no search, only a Blob read (tens of milliseconds) and the coverage
/// cube, which is computed from reference data.
pub const LOADS_AT_ONCE: usize = 8;

/// How often a computation waiting for visitors' searches looks again.
const GIVE_WAY_POLL: Duration = Duration::from_millis(200);

/// The home page's example searches, shared with the web app.
const EXAMPLES_JSON: &str = include_str!("../../../web/src/examples.json");

/// One entry of the examples file. Every field is declared, the ones the
/// warm-up doesn't use too, and unknown ones are an error, so this module's
/// tests fail on an entry the web app's `Example` (web/src/examples.ts)
/// wouldn't have.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Example {
    pub id: String,
    /// The `/v1/aggregate` query string the web app sends for this example,
    /// without `v` (web/src/examples.test.ts checks it).
    pub aggregate: String,
    /// The card's title and line of text on the home page.
    pub title: String,
    pub blurb: String,
    /// The view a click opens (the web app's `Partial<ViewState>`).
    pub view: serde_json::Map<String, serde_json::Value>,
    /// The span of years the example belongs to, such as "1860-1877".
    pub era: String,
}

static EXAMPLES: LazyLock<Vec<Example>> = LazyLock::new(|| {
    serde_json::from_str(EXAMPLES_JSON).unwrap_or_else(|e| {
        // Checked by this module's tests; never block serving over it.
        tracing::error!(error = %e, "web/src/examples.json is unreadable; warming places only");
        Vec::new()
    })
});

/// The example searches the warm-up runs.
pub fn examples() -> &'static [Example] {
    &EXAMPLES
}

/// Why a warm-up ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The replica started.
    Startup,
    /// A new version is about to be swapped in.
    Publish,
}

impl Trigger {
    /// The limit on a run: past the readiness cap a starting replica serves
    /// visitors while it warms, so its run stays short; a publish only
    /// delays the swap, so it can afford to warm every example.
    fn budget(self, config: &crate::config::Config) -> Duration {
        match self {
            Self::Startup => config.prewarm_startup_budget,
            Self::Publish => config.prewarm_budget,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Publish => "publish",
        }
    }
}

/// Attempts per warm-up query when the backend fails.
const RETRY_ATTEMPTS: u32 = 6;
/// The longest pause between attempts.
const RETRY_PAUSE_MAX: Duration = Duration::from_secs(10);

/// What one warm-up run did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Every query, places and coverage included, skipped ones too.
    pub queries: usize,
    pub ok: usize,
    pub timed_out: usize,
    pub failed: usize,
    /// Not run because the budget ran out.
    pub skipped: usize,
    /// Searches from the search log the run warmed or tried (each with its
    /// coverage cube, which is counted in `queries` too); the ones it didn't
    /// reach count in `skipped`.
    pub from_log: usize,
    /// The home page's examples.
    pub examples: usize,
    /// Examples whose search is in the in-process cache when the run ends
    /// (an entry the run added can have been evicted since).
    pub examples_warm: usize,
    /// Searches (examples and logged ones) a cache already held, read
    /// without a search.
    pub cached: usize,
    /// Searches the run computed (or tried to), whatever came of them.
    pub computed: usize,
    /// Time computations waited for visitors' searches to end (after a
    /// start).
    pub gave_way: Duration,
    pub elapsed: Duration,
}

enum Outcome {
    /// The response, if the handler served one (a cache read has none).
    Ok(Option<Response>),
    TimedOut,
    Failed(String),
    Skipped,
}

impl Outcome {
    /// Where the answer came from: a cache read ([`Run::load`]) or a
    /// response the handler served from the in-process cache is `Cache`;
    /// anything the handler computed, or tried to, is `Computed`.
    fn source(&self) -> Source {
        match self {
            Self::Ok(None) => Source::Cache,
            Self::Ok(Some(resp)) if resp.extensions().get::<routes::FromCache>().is_some() => {
                Source::Cache
            }
            _ => Source::Computed,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Ok(_) => "ok",
            Self::TimedOut => "timeout",
            Self::Failed(_) => "error",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Clone, Copy)]
enum Endpoint {
    Places,
    Aggregate,
    Coverage,
}

impl Endpoint {
    fn as_str(self) -> &'static str {
        match self {
            Self::Places => "places",
            Self::Aggregate => "aggregate",
            Self::Coverage => "coverage",
        }
    }
}

/// Where a warm-up query's answer came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Read from the in-process or persistent cache, or served by the
    /// handler from the in-process cache: nothing computed.
    Cache,
    /// Computed by the handler (places and coverage from reference data,
    /// searches on the searcher), or by another request it waited on.
    Computed,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Computed => "computed",
        }
    }
}

/// One search the run warms: an example or a search from the log.
struct Search {
    /// The example's id, or `search-log-{rank}`.
    label: String,
    /// The aggregate query string, without `v`.
    query: String,
    /// The in-process cache key; `None` if the query doesn't parse (the
    /// handler then reports why).
    key: Option<String>,
    /// From the search log: logged by rank, its errors by kind.
    private: bool,
}

impl Search {
    fn example(&self) -> bool {
        !self.private
    }
}

/// A search read from a cache, and its coverage cube.
struct Loaded {
    elapsed: Duration,
    coverage: Option<(Outcome, Duration)>,
}

struct Run<'a> {
    state: &'a Arc<AppState>,
    snap: Arc<Snapshot>,
    /// The version, as a `v` query value.
    v: String,
    trigger: Trigger,
    deadline: Instant,
    report: Report,
}

impl Run<'_> {
    /// One warm-up request through its handler, labelled `label` in the
    /// logs. A `private` one came from the search log: its error is logged by
    /// kind only.
    async fn query(
        &mut self,
        endpoint: Endpoint,
        label: &str,
        uri: &str,
        private: bool,
    ) -> Option<Response> {
        let (outcome, elapsed) = self.attempt(endpoint, uri, private).await;
        self.record(endpoint, label, outcome, elapsed)
    }

    async fn attempt(&self, endpoint: Endpoint, uri: &str, private: bool) -> (Outcome, Duration) {
        let started = Instant::now();
        let outcome = match uri.parse::<Uri>() {
            Err(_) if private => Outcome::Failed("invalid uri".to_owned()),
            Err(e) => Outcome::Failed(e.to_string()),
            Ok(uri) => self.call(endpoint, &uri, private).await,
        };
        (outcome, started.elapsed())
    }

    /// Count and log one query's outcome; its response, if it has one.
    fn record(
        &mut self,
        endpoint: Endpoint,
        label: &str,
        outcome: Outcome,
        elapsed: Duration,
    ) -> Option<Response> {
        let source = outcome.source();
        self.report.queries += 1;
        match &outcome {
            Outcome::Ok(_) => self.report.ok += 1,
            Outcome::TimedOut => self.report.timed_out += 1,
            Outcome::Failed(_) => self.report.failed += 1,
            Outcome::Skipped => self.report.skipped += 1,
        }
        if !matches!(outcome, Outcome::Skipped) {
            self.state
                .metrics
                .prewarm_query(endpoint.as_str(), source.as_str(), elapsed);
        }
        let version = self.snap.refdata.version();
        let ms = elapsed.as_millis() as u64;
        match &outcome {
            Outcome::Failed(error) => tracing::warn!(
                version,
                endpoint = endpoint.as_str(),
                example = label,
                source = source.as_str(),
                ms,
                error = %error,
                "warm-up query failed"
            ),
            Outcome::Skipped => {}
            _ if elapsed >= SLOW => tracing::info!(
                version,
                endpoint = endpoint.as_str(),
                example = label,
                source = source.as_str(),
                outcome = outcome.label(),
                ms,
                "slow warm-up query"
            ),
            _ => {}
        }
        match outcome {
            Outcome::Ok(resp) => resp,
            _ => None,
        }
    }

    /// One query, retrying backend errors with a doubling pause while the
    /// run's budget allows. On the first prod start the searcher sidecar
    /// answered 500 for its first seconds and three warm-up queries failed
    /// within 5 ms each.
    async fn call(&self, endpoint: Endpoint, uri: &Uri, private: bool) -> Outcome {
        // One limit for the query across its attempts, so retries can't
        // stretch one example past `prewarm_query_timeout`.
        let deadline =
            (Instant::now() + self.state.config.prewarm_query_timeout).min(self.deadline);
        let mut pause = self.state.config.prewarm_retry_first;
        let mut attempt = 1;
        loop {
            let (outcome, retry) = self.call_once(endpoint, uri, deadline, private).await;
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !retry || attempt >= RETRY_ATTEMPTS || remaining <= pause {
                return outcome;
            }
            tracing::info!(
                endpoint = endpoint.as_str(),
                attempt,
                pause_ms = pause.as_millis() as u64,
                "warm-up query hit a backend error; retrying"
            );
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(RETRY_PAUSE_MAX);
            attempt += 1;
        }
    }

    /// One attempt, and whether a failure is worth retrying (the backend,
    /// not the request, failed).
    async fn call_once(
        &self,
        endpoint: Endpoint,
        uri: &Uri,
        deadline: Instant,
        private: bool,
    ) -> (Outcome, bool) {
        let describe = |e: &ApiError| {
            if private {
                kind(e).to_owned()
            } else {
                format!("{e:?}")
            }
        };
        let now = Instant::now();
        if self.deadline <= now {
            return (Outcome::Skipped, false);
        }
        let limit = deadline.saturating_duration_since(now);
        if limit.is_zero() {
            return (Outcome::TimedOut, false);
        }
        let ctx = Ctx {
            snap: self.snap.clone(),
            timeout: limit,
            warm_up: true,
        };
        let state = self.state;
        let served = tokio::time::timeout(limit, async move {
            match endpoint {
                Endpoint::Places => routes::places_in(state, ctx, uri).await,
                Endpoint::Aggregate => routes::aggregate_in(state, ctx, uri, None).await,
                Endpoint::Coverage => routes::coverage_in(state, ctx, uri).await,
            }
        })
        .await;
        match served {
            Err(_) | Ok(Err(ApiError::Timeout)) => (Outcome::TimedOut, false),
            Ok(Err(e @ ApiError::Backend(_))) => (Outcome::Failed(describe(&e)), true),
            Ok(Err(e)) => (Outcome::Failed(describe(&e)), false),
            Ok(Ok(resp)) if resp.status() == StatusCode::OK => (Outcome::Ok(Some(resp)), false),
            Ok(Ok(resp)) => {
                let retry = resp.status().is_server_error();
                (Outcome::Failed(format!("status {}", resp.status())), retry)
            }
        }
    }

    /// Read every search a cache already holds into the in-process cache,
    /// [`LOADS_AT_ONCE`] at a time, with its coverage cube; the searches
    /// none holds, in their order, to compute.
    async fn load_cached<'s>(&mut self, searches: &'s [Search]) -> Vec<&'s Search> {
        let started = Instant::now();
        let this = &*self;
        // Boxed: the borrows in an unboxed future here are too general for
        // the compiler to prove the task `Send`.
        let reads: Vec<BoxFuture<'_, (&Search, Option<Loaded>)>> = searches
            .iter()
            .map(|s| async move { (s, this.load(s).await) }.boxed())
            .collect();
        let loaded: Vec<(&Search, Option<Loaded>)> = futures::stream::iter(reads)
            .buffered(LOADS_AT_ONCE)
            .collect()
            .await;
        let mut misses = Vec::new();
        for (search, loaded) in loaded {
            let Some(loaded) = loaded else {
                misses.push(search);
                continue;
            };
            self.report.cached += 1;
            if search.private {
                self.report.from_log += 1;
            }
            self.record(
                Endpoint::Aggregate,
                &search.label,
                Outcome::Ok(None),
                loaded.elapsed,
            );
            if let Some((outcome, elapsed)) = loaded.coverage {
                self.record(Endpoint::Coverage, &search.label, outcome, elapsed);
            }
        }
        tracing::info!(
            version = self.snap.refdata.version(),
            cached = searches.len() - misses.len(),
            missed = misses.len(),
            ms = started.elapsed().as_millis() as u64,
            "warm-up read the cached searches"
        );
        misses
    }

    /// `search` from the in-process or persistent cache, and its coverage
    /// cube; `None` if neither cache holds it, or the budget is spent.
    async fn load(&self, search: &Search) -> Option<Loaded> {
        let key = search.key.as_deref()?;
        let left = self.deadline.saturating_duration_since(Instant::now());
        let started = Instant::now();
        let read = routes::read_cached(self.state, self.snap.refdata.version(), key);
        let body = tokio::time::timeout(left, read).await.ok()??;
        let elapsed = started.elapsed();
        let coverage = match coverage_link(&body) {
            Some(uri) => Some(self.attempt(Endpoint::Coverage, &uri, search.private).await),
            None => None,
        };
        Some(Loaded { elapsed, coverage })
    }

    /// Compute `search` through its handler, then its coverage cube.
    async fn compute(&mut self, search: &Search) {
        if self.trigger == Trigger::Startup {
            self.give_way().await;
        }
        let uri = format!("/v1/aggregate?{}&v={}", search.query, self.v);
        let (outcome, elapsed) = self
            .attempt(Endpoint::Aggregate, &uri, search.private)
            .await;
        if !matches!(outcome, Outcome::Skipped) {
            // A visitor may have computed it since the cache reads.
            match outcome.source() {
                Source::Cache => self.report.cached += 1,
                Source::Computed => self.report.computed += 1,
            }
            if search.private {
                self.report.from_log += 1;
            }
        }
        let resp = self.record(Endpoint::Aggregate, &search.label, outcome, elapsed);
        let Some(resp) = resp else {
            return;
        };
        if let Some(coverage) = baseline_ref(resp).await {
            self.query(Endpoint::Coverage, &search.label, &coverage, search.private)
                .await;
        }
    }

    /// Wait while a visitor's search is running or queued (within the
    /// budget), so the warm-up starts a search only on an idle searcher. The
    /// warm-up's own searches run on its own slot and don't count.
    async fn give_way(&mut self) {
        if self.state.flights.visitor_searches() == 0 {
            return;
        }
        let started = Instant::now();
        while self.state.flights.visitor_searches() > 0 {
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            tokio::time::sleep(GIVE_WAY_POLL.min(left)).await;
        }
        self.report.gave_way += started.elapsed();
    }
}

/// An error's kind, without its message.
fn kind(e: &ApiError) -> &'static str {
    match e {
        ApiError::Params(_) | ApiError::BadRequest(_) => "bad_parameter",
        ApiError::Unsupported(_) => "unsupported",
        ApiError::NotFound(_) => "not_found",
        ApiError::TooBroad(_) => "too_broad",
        ApiError::Timeout => "timeout",
        ApiError::Busy => "busy",
        ApiError::Backend(_) => "backend",
        ApiError::BackendRejected(_) => "backend_rejected",
        ApiError::RateLimited(_) => "rate_limited",
        ApiError::BadBeacon(_) | ApiError::TooLarge(_) | ApiError::MediaType(_) => "bad_request",
    }
}

/// Only the part of an aggregate response the warm-up reads.
#[derive(Deserialize)]
struct Linked {
    cube: LinkedCube,
}

#[derive(Deserialize)]
struct LinkedCube {
    baseline_ref: Option<String>,
}

/// The coverage link (`cube.baseline_ref`) in an aggregate response body,
/// which the web app fetches next.
fn coverage_link(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Linked>(body)
        .ok()?
        .cube
        .baseline_ref
}

/// [`coverage_link`] in an aggregate response.
async fn baseline_ref(resp: Response) -> Option<String> {
    let bytes = axum::body::to_bytes(resp.into_body(), MAX_BODY)
        .await
        .ok()?;
    coverage_link(&bytes)
}

/// Limit on reading the search log for the warm-up.
const LOG_READ_LIMIT: Duration = Duration::from_secs(30);

/// The most frequent recent searches in the search log, as canonical
/// aggregate query strings, most frequent first, with room for the
/// examples among them. Empty without a search log, with
/// `prewarm_top_searches` at 0, or when reading takes longer than `left`.
async fn ranked(state: &AppState, snap: &Snapshot, left: Duration) -> Vec<String> {
    let n = state.config.prewarm_top_searches;
    let Some(log) = state.search_log.as_deref().filter(|_| n > 0) else {
        return Vec::new();
    };
    let read = crate::searchlog::top_searches(
        log.store().as_ref(),
        chrono::Utc::now().date_naive(),
        state.config.prewarm_log_days,
        n + examples().len(),
        snap.refdata.bounds(),
        snap.refdata.analyzers(),
    );
    match tokio::time::timeout(left.min(LOG_READ_LIMIT), read).await {
        Ok(top) => top,
        Err(_) => {
            tracing::warn!("warm-up: reading the search log timed out; skipping its searches");
            Vec::new()
        }
    }
}

/// The order the examples are warmed in, as indexes into `canonical` (each
/// example's canonical query, if it parses): those in `ranked` (most
/// searched first) in its order, then the rest in their own order.
pub fn order(canonical: &[Option<String>], ranked: &[String]) -> Vec<usize> {
    let rank = |i: usize| {
        canonical[i]
            .as_ref()
            .and_then(|c| ranked.iter().position(|r| r == c))
    };
    let mut order: Vec<usize> = (0..canonical.len()).collect();
    // Stable: unranked examples keep the file's order.
    order.sort_by_key(|&i| rank(i).map_or((1, 0), |r| (0, r)));
    order
}

/// Warm the caches for `snap`'s version: the places layer, then each example
/// search and its coverage cube, as the web app requests them, then the most
/// frequent searches from the search log the same way while the budget
/// lasts. After a start, searches a cache holds are read, not computed.
/// Logs one line for the run and records `api.prewarm_*`.
pub async fn run(state: &Arc<AppState>, snap: Arc<Snapshot>, trigger: Trigger) -> Report {
    let started = Instant::now();
    let version = snap.refdata.version().to_owned();
    let v: String = form_urlencoded::byte_serialize(version.as_bytes()).collect();
    let deadline = started + trigger.budget(&state.config);
    let mut run = Run {
        state,
        snap: snap.clone(),
        v: v.clone(),
        trigger,
        deadline,
        report: Report {
            examples: examples().len(),
            ..Report::default()
        },
    };
    run.query(Endpoint::Places, "", &format!("/v1/places?v={v}"), false)
        .await;

    let canonical: Vec<Option<String>> = examples()
        .iter()
        .map(|ex| {
            let raw = RawParams::parse(&ex.aggregate).ok()?;
            snap.refdata
                .search_request(&raw)
                .ok()
                .map(|r| r.canonical())
        })
        .collect();
    let left = deadline.saturating_duration_since(Instant::now());
    let ranked = if left.is_zero() {
        Vec::new()
    } else {
        ranked(state, &snap, left).await
    };
    // The handler's key for a canonical query string, which parses to the
    // same search; `None` if it doesn't parse (the handler says why).
    let key = |canonical: &str| {
        let raw = RawParams::parse(canonical).ok()?;
        let req = snap.refdata.search_request(&raw).ok()?;
        Some(routes::aggregate_key(&snap.refdata, canonical, &req.query))
    };
    let mut searches: Vec<Search> = order(&canonical, &ranked)
        .into_iter()
        .map(|i| Search {
            label: examples()[i].id.clone(),
            query: examples()[i].aggregate.clone(),
            key: canonical[i].as_deref().and_then(key),
            private: false,
        })
        .collect();
    let logged: Vec<&String> = ranked
        .iter()
        .filter(|k| !canonical.iter().any(|c| c.as_ref() == Some(*k)))
        .take(state.config.prewarm_top_searches)
        .collect();
    searches.extend(logged.into_iter().enumerate().map(|(rank, k)| Search {
        label: format!("search-log-{}", rank + 1),
        query: k.clone(),
        key: key(k),
        private: true,
    }));

    let pending: Vec<&Search> = match trigger {
        Trigger::Startup => run.load_cached(&searches).await,
        Trigger::Publish => searches.iter().collect(),
    };
    for search in pending {
        run.compute(search).await;
    }
    // What visitors will find: the in-process cache is bounded in bytes
    // (`USNM_CACHE_MB`), so a later entry can have evicted an earlier one.
    state.cache.run_pending_tasks().await;
    run.report.examples_warm = searches
        .iter()
        .filter(|s| s.example())
        .filter(|s| s.key.as_ref().is_some_and(|k| state.cache.contains_key(k)))
        .count();

    let mut report = run.report;
    report.elapsed = started.elapsed();
    tracing::info!(
        trigger = trigger.as_str(),
        version,
        queries = report.queries,
        ok = report.ok,
        timed_out = report.timed_out,
        failed = report.failed,
        skipped = report.skipped,
        from_log = report.from_log,
        examples = report.examples,
        examples_warm = report.examples_warm,
        cached = report.cached,
        computed = report.computed,
        gave_way_ms = report.gave_way.as_millis() as u64,
        ms = report.elapsed.as_millis() as u64,
        "warm-up finished"
    );
    state.metrics.prewarm(trigger.as_str(), &report);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn the_examples_file_has_exactly_the_declared_fields() {
        // EXAMPLES logs and falls back to none on an error; parse it here
        // so the error itself fails the test.
        let ex: Vec<Example> = serde_json::from_str(EXAMPLES_JSON)
            .unwrap_or_else(|e| panic!("web/src/examples.json: {e}"));
        assert_eq!(ex.len(), examples().len());
        let extra = r#"[{"id": "a", "aggregate": "q=a", "title": "A", "blurb": "B.",
            "view": {"q": "a"}, "era": "1860-1877", "note": "x"}]"#;
        let err = serde_json::from_str::<Vec<Example>>(extra).unwrap_err();
        assert!(err.to_string().contains("unknown field `note`"), "{err}");
        let missing = r#"[{"id": "a", "aggregate": "q=a", "title": "A", "blurb": "B.",
            "view": {"q": "a"}}]"#;
        let err = serde_json::from_str::<Vec<Example>>(missing).unwrap_err();
        assert!(err.to_string().contains("missing field `era`"), "{err}");
    }

    #[test]
    fn examples_are_valid_aggregate_queries() {
        let ex = examples();
        assert!(!ex.is_empty(), "web/src/examples.json lists no examples");
        let bounds = (
            NaiveDate::from_ymd_opt(1770, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(1963, 12, 31).unwrap(),
        );
        for e in ex {
            let raw = RawParams::parse(&e.aggregate).unwrap();
            raw.reject_unknown(&[]).unwrap();
            assert!(raw.get("v").is_none(), "{}: `v` is added per version", e.id);
            // Both analyzer versions, as the version served may have either.
            for a in usnm_core::text::Analyzer::ALL {
                let analyzers = usnm_core::text::Analyzers::all(a);
                usnm_core::params::SearchRequest::from_raw(&raw, bounds, analyzers)
                    .unwrap_or_else(|err| panic!("{}: {err} ({a:?})", e.id));
            }
        }
    }

    fn some(keys: &[&str]) -> Vec<Option<String>> {
        keys.iter().map(|k| Some((*k).to_owned())).collect()
    }

    #[test]
    fn the_most_searched_examples_come_first_then_the_files_order() {
        let canonical = some(&["a", "b", "c", "d", "e"]);
        let ranked: Vec<String> = ["d", "x", "b"].iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(order(&canonical, &ranked), vec![3, 1, 0, 2, 4]);
    }

    #[test]
    fn without_a_search_log_the_files_order_stands() {
        let mut canonical = some(&["a", "b", "c"]);
        canonical[1] = None;
        assert_eq!(order(&canonical, &[]), vec![0, 1, 2]);
        assert_eq!(order(&canonical, &["c".to_owned()]), vec![2, 0, 1]);
    }

    #[test]
    fn the_coverage_link_is_read_without_the_rest() {
        let body = br#"{"total":{"hits":3},"cube":{"cells":[1,2],"baseline_ref":"/v1/coverage?bucket=year"}}"#;
        assert_eq!(
            coverage_link(body).as_deref(),
            Some("/v1/coverage?bucket=year")
        );
        assert_eq!(coverage_link(br#"{"cube":{"baseline_ref":null}}"#), None);
        assert_eq!(coverage_link(b"not json"), None);
    }
}
