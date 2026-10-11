//! Cache warm-up before a version serves: at a reload and at start.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt;
use usnm_api::config::Config;
use usnm_api::prewarm::{self, Trigger};
use usnm_api::{app, reload_if_changed, spawn_startup_warm_up, AppState, Engine, Loader};
use usnm_core::params::Filters;
use usnm_core::query::Node;
use usnm_core::text::Analyzers;
use usnm_core::time::BucketSpec;
use usnm_search::memory::MemoryBackend;
use usnm_search::{
    Capabilities, CubeCell, HitsPage, HitsQuery, IndexSet, SearchBackend, SearchError, Summary,
};
use usnm_store::LocalStore;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data")
}

fn config() -> Config {
    config_from(|_| None)
}

/// The test settings over the configuration `lookup` gives.
fn config_from(lookup: impl Fn(&str) -> Option<String>) -> Config {
    let mut c = Config::from_lookup(lookup).unwrap();
    c.data_dir = data_dir();
    c.reference_url = data_dir().display().to_string();
    c.rate_limit = None;
    c.prewarm_retry_first = Duration::from_millis(10);
    c
}

fn fixture_backend() -> MemoryBackend {
    let mut backend = MemoryBackend::new();
    for id in ["pages-base-fixture", "pages-delta-fixture-1"] {
        let path = data_dir().join("indexes").join(format!("{id}.jsonl"));
        let docs: Vec<_> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<usnm_search::PageDoc>(l).unwrap())
            .collect();
        backend.add_index(id, docs);
    }
    backend
}

/// The fixture corpus behind a counter, optionally slow or failing.
struct Counting {
    inner: MemoryBackend,
    calls: AtomicUsize,
    delay: Duration,
    fail: bool,
    /// Calls that fail before the backend starts answering (a searcher that
    /// is still starting).
    fail_first: AtomicUsize,
    /// Every call is refused as a bad request (a Quickwit 4xx).
    reject: bool,
    /// The query of each summary call (one per search), in order.
    searched: std::sync::Mutex<Vec<String>>,
}

impl Counting {
    fn new(delay: Duration, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: fixture_backend(),
            calls: AtomicUsize::new(0),
            delay,
            fail,
            fail_first: AtomicUsize::new(0),
            reject: false,
            searched: Default::default(),
        })
    }

    fn rejecting() -> Arc<Self> {
        Arc::new(Self {
            inner: fixture_backend(),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
            fail: false,
            fail_first: AtomicUsize::new(0),
            reject: true,
            searched: Default::default(),
        })
    }

    fn failing_first(n: usize) -> Arc<Self> {
        let b = Self::new(Duration::ZERO, false);
        b.fail_first.store(n, Ordering::SeqCst);
        b
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn enter(&self) -> Result<(), SearchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        if self.fail {
            return Err(SearchError::Backend("engine down".into()));
        }
        if self.reject {
            return Err(SearchError::Rejected(
                "quickwit returned 400 Bad Request".into(),
            ));
        }
        // A compare-exchange loop rather than fetch_update, which newer toolchains deprecate (in
        // favour of try_update, newer than the workspace's rust-version).
        let starting = {
            let mut current = self.fail_first.load(Ordering::SeqCst);
            loop {
                let Some(next) = current.checked_sub(1) else {
                    break false;
                };
                match self.fail_first.compare_exchange(
                    current,
                    next,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break true,
                    Err(actual) => current = actual,
                }
            }
        };
        if starting {
            return Err(SearchError::Backend("quickwit returned 500".into()));
        }
        Ok(())
    }
}

#[async_trait]
impl SearchBackend for Counting {
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    async fn summary(
        &self,
        i: &IndexSet,
        q: &Node,
        f: &Filters,
        s: &BucketSpec,
    ) -> Result<Summary, SearchError> {
        self.searched.lock().unwrap().push(q.to_string());
        self.enter().await?;
        self.inner.summary(i, q, f, s).await
    }
    async fn cube(
        &self,
        i: &IndexSet,
        q: &Node,
        f: &Filters,
        s: &BucketSpec,
        shards: &[u8],
    ) -> Result<Vec<CubeCell>, SearchError> {
        self.enter().await?;
        self.inner.cube(i, q, f, s, shards).await
    }
    async fn hits(
        &self,
        i: &IndexSet,
        q: &Node,
        f: &Filters,
        p: &HitsQuery,
    ) -> Result<HitsPage, SearchError> {
        self.enter().await?;
        self.inner.hits(i, q, f, p).await
    }
    async fn american_stories_only(
        &self,
        i: &IndexSet,
        q: &Node,
        f: &Filters,
    ) -> Result<u64, SearchError> {
        self.enter().await?;
        self.inner.american_stories_only(i, q, f).await
    }
    async fn health(&self) -> Result<(), SearchError> {
        Ok(())
    }
}

/// A fresh copy of the fixture reference data the test can publish into.
fn temp_reference(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("usnm-prewarm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("fixture-v1")).unwrap();
    for entry in std::fs::read_dir(data_dir().join("fixture-v1")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(
            &path,
            dir.join("fixture-v1").join(path.file_name().unwrap()),
        )
        .unwrap();
    }
    std::fs::copy(data_dir().join("current.json"), dir.join("current.json")).unwrap();
    dir
}

/// Publish `fixture-v2`: the same indexes with its own reference snapshot.
fn publish_v2(dir: &Path) {
    std::fs::create_dir_all(dir.join("fixture-v2")).unwrap();
    for entry in std::fs::read_dir(dir.join("fixture-v1")).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(
            &path,
            dir.join("fixture-v2").join(path.file_name().unwrap()),
        )
        .unwrap();
    }
    let manifest = dir.join("fixture-v2/manifest.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&manifest).unwrap()).unwrap();
    m["index_version"] = "fixture-v2".into();
    std::fs::write(manifest, m.to_string()).unwrap();
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("current.json")).unwrap()).unwrap();
    current["index_version"] = "fixture-v2".into();
    current["reference"] = "fixture-v2".into();
    std::fs::write(dir.join("current.json"), current.to_string()).unwrap();
}

/// A state serving `fixture-v1` from `dir`, with `backend` shared across versions.
async fn reloading_state(dir: &Path, cfg: Config, backend: Arc<Counting>) -> AppState {
    let loader = Loader {
        reference: Arc::new(LocalStore::new(dir)),
        engine: Engine::Shared(backend),
        congress_search: cfg.congress_search,
    };
    let snapshot = loader.snapshot().await.unwrap();
    AppState::with_loader(cfg, snapshot, Some(loader))
}

async fn get(state: &Arc<AppState>, uri: &str) -> (StatusCode, Value) {
    let resp = app(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn readyz(state: &Arc<AppState>) -> StatusCode {
    let resp = app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    resp.status()
}

/// Everything the web app loads for each example at `version`: the places,
/// the search and the coverage it links to. Returns the backend calls it cost
/// and how many distinct responses it loaded.
async fn load_examples(state: &Arc<AppState>, backend: &Counting, version: &str) -> (usize, usize) {
    let before = backend.calls();
    let mut coverages = std::collections::HashSet::new();
    let mut found = 0;
    let (status, _) = get(state, &format!("/v1/places?v={version}")).await;
    assert_eq!(status, StatusCode::OK);
    for ex in prewarm::examples() {
        let (status, body) = get(
            state,
            &format!("/v1/aggregate?{}&v={version}", ex.aggregate),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", ex.id);
        assert_eq!(body["index_version"], version);
        // Most examples are outside the fixtures' 1895-1897; a search that
        // finds nothing still costs the backend a call.
        if body["total"]["hits"].as_u64().unwrap() > 0 {
            found += 1;
        }
        let coverage = body["cube"]["baseline_ref"].as_str().unwrap().to_owned();
        let (status, body) = get(state, &coverage).await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", ex.id);
        coverages.insert(coverage);
    }
    assert!(found > 0, "no example matches the fixtures");
    let distinct = 1 + prewarm::examples().len() + coverages.len();
    (backend.calls() - before, distinct)
}

async fn visit_examples(state: &Arc<AppState>, backend: &Counting, version: &str) -> usize {
    load_examples(state, backend, version).await.0
}

fn persisted(dir: &Path, version: &str) -> usize {
    std::fs::read_dir(dir.join(version).join("f7"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[tokio::test]
async fn a_new_version_is_warmed_before_it_is_swapped_in() {
    let dir = temp_reference("publish");
    let cache = dir.join("cache");
    let mut cfg = config();
    cfg.persist_after = Duration::ZERO;
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(
        reloading_state(&dir, cfg, backend.clone())
            .await
            .with_response_store(Arc::new(LocalStore::new(&cache))),
    );

    publish_v2(&dir);
    assert!(reload_if_changed(&state).await.unwrap());
    assert_eq!(state.snapshot.load().refdata.version(), "fixture-v2");
    assert!(backend.calls() > 0, "the warm-up searched the new version");

    // What the web app loads for each example is already cached for v2: no
    // backend call.
    let (calls, distinct) = load_examples(&state, &backend, "fixture-v2").await;
    assert_eq!(calls, 0);
    // A request without `v` (a shared link) has the same cache key.
    let before = backend.calls();
    let ex = &prewarm::examples()[0];
    let (status, body) = get(&state, &format!("/v1/aggregate?{}", ex.aggregate)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["index_version"], "fixture-v2");
    assert_eq!(backend.calls(), before);

    // The slow ones (all of them, with `persist_after` = 0) were persisted
    // under the new version, for replicas that start later.
    for _ in 0..100 {
        if persisted(&cache, "fixture-v2") == distinct {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(persisted(&cache, "fixture-v2"), distinct);
    assert_eq!(persisted(&cache, "fixture-v1"), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// With American Stories' text searched, the aggregate's extra count is
/// part of the response the warm-up caches: visitors cost no backend call.
#[tokio::test]
async fn the_american_stories_only_count_is_warmed_with_the_aggregate() {
    let dir = temp_reference("american-stories");
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(reloading_state(&dir, config(), backend.clone()).await);
    publish_v2(&dir);
    let path = dir.join("current.json");
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    current["american_stories"] = usnm_core::american_stories::VERSION.into();
    std::fs::write(&path, current.to_string()).unwrap();
    assert!(reload_if_changed(&state).await.unwrap());

    let (calls, _) = load_examples(&state, &backend, "fixture-v2").await;
    assert_eq!(calls, 0);
    let mut counted = 0;
    for ex in prewarm::examples() {
        let (_, body) = get(
            &state,
            &format!("/v1/aggregate?{}&v=fixture-v2", ex.aggregate),
        )
        .await;
        let only = body["total"]["american_stories_only"].as_u64();
        assert!(only.is_some(), "{}: {body}", ex.id);
        counted += only.unwrap();
    }
    assert!(
        counted > 0,
        "no example matches only in American Stories' text"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// With `USNM_AMERICAN_STORIES_SEARCH=false`, a version published with
/// American Stories' text is warmed and served without it, and the warm-up
/// fills the same (marked) cache keys visitors' requests read.
#[tokio::test]
async fn a_version_is_warmed_without_american_stories_text_when_switched_off() {
    let dir = temp_reference("american-stories-off");
    let backend = Counting::new(Duration::ZERO, false);
    let mut cfg = config();
    cfg.american_stories_search = false;
    let state = Arc::new(reloading_state(&dir, cfg, backend.clone()).await);
    publish_v2(&dir);
    let path = dir.join("current.json");
    let mut current: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    current["american_stories"] = usnm_core::american_stories::VERSION.into();
    std::fs::write(&path, current.to_string()).unwrap();
    assert!(reload_if_changed(&state).await.unwrap());
    let rd = &state.snapshot.load().refdata;
    assert!(rd.has_american_stories() && !rd.searches_american_stories());

    let (calls, _) = load_examples(&state, &backend, "fixture-v2").await;
    assert_eq!(calls, 0);
    for ex in prewarm::examples() {
        let (_, body) = get(
            &state,
            &format!("/v1/aggregate?{}&v=fixture-v2", ex.aggregate),
        )
        .await;
        assert!(body["total"]["hits"].is_u64(), "{}: {body}", ex.id);
        assert!(
            body["total"].get("american_stories_only").is_none(),
            "{}: {body}",
            ex.id
        );
    }
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(
        report.examples_warm,
        prewarm::examples().len(),
        "{report:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn warm_up_reports_what_it_ran() {
    let dir = temp_reference("report");
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(reloading_state(&dir, config(), backend.clone()).await);
    let snap = state.snapshot.load_full();
    let report = prewarm::run(&state, snap.clone(), Trigger::Startup).await;
    let queries = 1 + 2 * prewarm::examples().len();
    assert_eq!(report.queries, queries);
    assert_eq!(report.ok, queries, "{report:?}");
    assert_eq!(visit_examples(&state, &backend, "fixture-v1").await, 0);

    // Again: everything is in the in-process cache already.
    let n = prewarm::examples().len();
    assert_eq!(report.examples, n);
    assert_eq!(report.examples_warm, n, "{report:?}");
    assert_eq!(report.computed, n, "{report:?}");
    assert_eq!(report.cached, 0, "{report:?}");
    let calls = backend.calls();
    let again = prewarm::run(&state, snap, Trigger::Startup).await;
    assert_eq!(again.ok, queries);
    assert_eq!(backend.calls(), calls);
    // Read from the in-process cache: nothing computed.
    assert_eq!(again.cached, n, "{again:?}");
    assert_eq!(again.computed, 0, "{again:?}");
    assert_eq!(again.examples_warm, n, "{again:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_slow_backend_holds_the_swap_for_the_budget_at_most() {
    let dir = temp_reference("slow");
    let mut cfg = config();
    cfg.prewarm_query_timeout = Duration::from_millis(200);
    cfg.prewarm_budget = Duration::from_millis(500);
    cfg.prewarm_startup_budget = Duration::from_millis(500);
    let backend = Counting::new(Duration::from_secs(30), false);
    let state = Arc::new(reloading_state(&dir, cfg, backend.clone()).await);

    // Each search runs into the per-query limit, and the last ones into the budget.
    let started = Instant::now();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert!(started.elapsed() < Duration::from_secs(2), "{report:?}");
    assert_eq!(
        report.ok, 1,
        "only the places, which need no search: {report:?}"
    );
    assert!(report.timed_out >= 2, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(
        report.ok + report.timed_out + report.skipped,
        1 + prewarm::examples().len(),
        "no coverage without its search: {report:?}"
    );
    // Every example was either computed (and timed out) or skipped.
    assert_eq!(report.examples_warm, 0, "{report:?}");
    assert_eq!(report.cached, 0, "{report:?}");
    assert_eq!(
        report.computed + report.skipped,
        prewarm::examples().len(),
        "{report:?}"
    );

    // A publish swaps once the budget is spent, and v1 serves meanwhile.
    publish_v2(&dir);
    let started = Instant::now();
    let reload = {
        let state = state.clone();
        tokio::spawn(async move { reload_if_changed(&state).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, meta) = get(&state, "/v1/meta").await;
    assert_eq!(
        meta["index_version"], "fixture-v1",
        "v1 serves during the warm-up"
    );
    assert!(reload.await.unwrap().unwrap());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(state.snapshot.load().refdata.version(), "fixture-v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_searcher_that_is_still_starting_is_retried() {
    let dir = temp_reference("starting");
    let backend = Counting::failing_first(2);
    let state = Arc::new(reloading_state(&dir, config(), backend.clone()).await);
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    let queries = 1 + 2 * prewarm::examples().len();
    assert_eq!(report.ok, queries, "{report:?}");
    assert_eq!(report.failed, 0, "{report:?}");
    assert_eq!(visit_examples(&state, &backend, "fixture-v1").await, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn slow_failures_share_one_limit_per_query() {
    let dir = temp_reference("slow-failing");
    let backend = Counting::new(Duration::from_millis(100), true);
    let mut cfg = config();
    let limit = Duration::from_millis(350);
    cfg.prewarm_query_timeout = limit;
    let n = prewarm::examples().len();
    // Room for every example to use its whole limit, however many there are.
    cfg.prewarm_startup_budget = limit * n as u32 + Duration::from_secs(5);
    let state = Arc::new(reloading_state(&dir, cfg, backend).await);
    let started = std::time::Instant::now();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    // Every example got its turn: none used up the run's budget.
    assert_eq!(report.skipped, 0, "{report:?}");
    assert_eq!(report.failed + report.timed_out, n, "{report:?}");
    // Each gave up at about its own limit. Resetting the limit on every
    // attempt would take about three times as long here.
    let bound = limit * n as u32 + Duration::from_millis(800);
    assert!(
        started.elapsed() < bound,
        "{:?} >= {bound:?}",
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_rejected_request_is_not_retried() {
    let dir = temp_reference("rejecting");
    let backend = Counting::rejecting();
    let state = Arc::new(reloading_state(&dir, config(), backend.clone()).await);
    let before = backend.calls();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(report.failed, prewarm::examples().len(), "{report:?}");
    // One call per example search: none of them was tried again.
    assert_eq!(backend.calls() - before, prewarm::examples().len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_failing_backend_does_not_block_the_swap() {
    let dir = temp_reference("failing");
    let backend = Counting::new(Duration::ZERO, true);
    let state = Arc::new(reloading_state(&dir, config(), backend).await);
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(report.ok, 1, "{report:?}");
    assert_eq!(report.failed, prewarm::examples().len(), "{report:?}");

    publish_v2(&dir);
    assert!(reload_if_changed(&state).await.unwrap());
    assert_eq!(state.snapshot.load().refdata.version(), "fixture-v2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn not_ready_until_warm() {
    let dir = temp_reference("ready");
    let backend = Counting::new(Duration::from_millis(20), false);
    let state = Arc::new(reloading_state(&dir, config(), backend.clone()).await);
    assert_eq!(readyz(&state).await, StatusCode::OK);

    let warming = spawn_startup_warm_up(state.clone());
    assert_eq!(readyz(&state).await, StatusCode::SERVICE_UNAVAILABLE);
    warming.await.unwrap();
    assert_eq!(readyz(&state).await, StatusCode::OK);
    // Ready means warm.
    assert_eq!(visit_examples(&state, &backend, "fixture-v1").await, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn by_default_not_ready_until_the_warm_up_ends() {
    // The default cap is the startup budget plus a margin, so a warm-up that
    // computes for its whole budget holds readiness to the end of it rather
    // than to a fixed cap: a rollout keeps the old revision serving until
    // this replica is warm (#265).
    let dir = temp_reference("budget");
    let mut cfg = config_from(|k| (k == "USNM_PREWARM_STARTUP_BUDGET_SECS").then(|| "1".into()));
    assert_eq!(cfg.ready_cap, Duration::from_secs(61));
    cfg.search_timeout = Duration::from_millis(200);
    // Every query outlasts the budget.
    let backend = Counting::new(Duration::from_secs(30), false);
    let state = Arc::new(reloading_state(&dir, cfg, backend.clone()).await);

    let started = Instant::now();
    let warming = spawn_startup_warm_up(state.clone());
    tokio::time::sleep(Duration::from_millis(600)).await;
    // A 300 ms cap would have let visitors in by now.
    assert_eq!(readyz(&state).await, StatusCode::SERVICE_UNAVAILABLE);
    warming.await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(1) && waited < Duration::from_secs(5),
        "{waited:?}"
    );
    assert_eq!(readyz(&state).await, StatusCode::OK);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn ready_at_the_cap_even_if_still_warming() {
    let dir = temp_reference("cap");
    let mut cfg = config();
    cfg.ready_cap = Duration::from_millis(300);
    cfg.search_timeout = Duration::from_millis(200);
    let backend = Counting::new(Duration::from_secs(30), false);
    let state = Arc::new(reloading_state(&dir, cfg, backend.clone()).await);

    let started = Instant::now();
    let warming = spawn_startup_warm_up(state.clone());
    assert_eq!(readyz(&state).await, StatusCode::SERVICE_UNAVAILABLE);
    warming.await.unwrap();
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(300) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
    assert_eq!(readyz(&state).await, StatusCode::OK);
    // The warm-up carries on past the cap: the first search is still running.
    assert_eq!(backend.calls(), 1);

    // A visitor asking for that search waits on the warm-up's computation,
    // but only up to the visitor's own limit, then hears it is still
    // computing; the warm-up keeps going.
    let first = &prewarm::examples()[0];
    let started = Instant::now();
    let (status, body) = get(&state, &format!("/v1/aggregate?{}", first.aggregate)).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "computing");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        backend.calls(),
        1,
        "the visitor joined the warm-up's search"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A search log record for `q` (query syntax) over 1896, as the API writes them.
fn record(q: &str, day: chrono::NaiveDate) -> usnm_api::searchlog::Record {
    usnm_api::searchlog::Record {
        v: 1,
        day,
        source: "api".into(),
        q: q.into(),
        query: q.into(),
        mode: None,
        near: None,
        fuzzy: 0,
        from: chrono::NaiveDate::from_ymd_opt(1896, 1, 1).unwrap(),
        to: chrono::NaiveDate::from_ymd_opt(1896, 12, 31).unwrap(),
        state: vec![],
        lccn: vec![],
        lang: vec![],
        front: false,
        corpus: None,
        series: Vec::new(),
        bucket: "month".into(),
        pages: Some(3),
        index_version: Some("fixture-v1".into()),
    }
}

#[tokio::test]
async fn the_most_frequent_logged_searches_are_warmed_after_the_examples() {
    use usnm_api::searchlog::{day_path, LogConfig, SearchLog};
    use usnm_store::ObjectStore;

    let dir = temp_reference("from-log");
    let log_dir = dir.join("searches");
    let store = Arc::new(LocalStore::new(&log_dir));
    let yesterday = chrono::Utc::now().date_naive().pred_opt().unwrap();
    // "silver" three times, "gold" twice, "cholera" once: with room for
    // two, the warm-up takes silver and gold.
    let lines: Vec<String> = ["silver", "gold", "silver", "cholera", "gold", "silver"]
        .iter()
        .map(|q| serde_json::to_string(&record(q, yesterday)).unwrap())
        .collect();
    store
        .put(
            &day_path(yesterday),
            lines.join("\n").into_bytes(),
            "application/x-ndjson",
        )
        .await
        .unwrap();
    let log = SearchLog::start(
        store,
        LogConfig {
            flush_interval: Duration::from_secs(3600),
            ..LogConfig::default()
        },
        &opentelemetry::global::meter("test"),
    );
    let mut cfg = config();
    cfg.prewarm_top_searches = 2;
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(
        reloading_state(&dir, cfg, backend.clone())
            .await
            .with_search_log(log.clone()),
    );

    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(report.from_log, 2, "{report:?}");
    // Places, then a search and its coverage per example and per logged search.
    let queries = 1 + 2 * prewarm::examples().len() + 2 * 2;
    assert_eq!(report.queries, queries, "{report:?}");
    assert_eq!(report.ok, queries, "{report:?}");

    // Both are in the cache now, in the form the web app asks for them.
    let calls = backend.calls();
    for q in ["silver", "gold"] {
        let (status, body) = get(
            &state,
            &format!("/v1/aggregate?q={q}&mode=phrase&from=1896-01-01&to=1896-12-31&bucket=month&v=fixture-v1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(backend.calls(), calls, "served from the cache");
    // The one not in the top two was not run.
    let (status, _) = get(
        &state,
        "/v1/aggregate?q=cholera&from=1896-01-01&to=1896-12-31&bucket=month&v=fixture-v1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(backend.calls() > calls);

    log.shutdown(Duration::from_secs(5)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn logged_searches_wait_for_the_examples_and_the_budget() {
    use usnm_api::searchlog::{day_path, LogConfig, SearchLog};
    use usnm_store::ObjectStore;

    let dir = temp_reference("from-log-budget");
    let store = Arc::new(LocalStore::new(dir.join("searches")));
    let yesterday = chrono::Utc::now().date_naive().pred_opt().unwrap();
    let lines: Vec<String> = ["silver", "gold"]
        .iter()
        .map(|q| serde_json::to_string(&record(q, yesterday)).unwrap())
        .collect();
    store
        .put(
            &day_path(yesterday),
            lines.join("\n").into_bytes(),
            "application/x-ndjson",
        )
        .await
        .unwrap();
    let log = SearchLog::start(
        store,
        LogConfig::default(),
        &opentelemetry::global::meter("test"),
    );
    // Each search takes 100 ms: the budget ends during the examples.
    let mut cfg = config();
    cfg.prewarm_budget = Duration::from_millis(250);
    cfg.prewarm_startup_budget = Duration::from_millis(250);
    let backend = Counting::new(Duration::from_millis(100), false);
    let state = Arc::new(
        reloading_state(&dir, cfg, backend)
            .await
            .with_search_log(log.clone()),
    );
    let started = Instant::now();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert!(started.elapsed() < Duration::from_secs(2), "{report:?}");
    assert_eq!(report.from_log, 0, "{report:?}");
    log.shutdown(Duration::from_secs(5)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A store that counts the reads in flight at once, each held `delay`.
#[derive(Debug)]
struct Gauged {
    inner: LocalStore,
    delay: Duration,
    reading: AtomicUsize,
    most: AtomicUsize,
    reads: AtomicUsize,
}

impl Gauged {
    fn new(dir: &Path, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            inner: LocalStore::new(dir),
            delay,
            reading: AtomicUsize::new(0),
            most: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl usnm_store::ObjectStore for Gauged {
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, usnm_store::StoreError> {
        let now = self.reading.fetch_add(1, Ordering::SeqCst) + 1;
        self.most.fetch_max(now, Ordering::SeqCst);
        self.reads.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        let found = self.inner.get(path).await;
        self.reading.fetch_sub(1, Ordering::SeqCst);
        found
    }
    async fn put_new(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<bool, usnm_store::StoreError> {
        self.inner.put_new(path, body, content_type).await
    }
    async fn put(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(), usnm_store::StoreError> {
        self.inner.put(path, body, content_type).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, usnm_store::StoreError> {
        self.inner.list(prefix).await
    }
}

/// After a start, the searches the persistent cache holds (an earlier
/// replica's warm-up wrote them) are read, a few at a time, not computed:
/// a slow searcher costs the run nothing, and the budget meant for
/// computations doesn't cut it short.
#[tokio::test]
async fn after_a_start_cached_searches_are_read_not_computed() {
    let dir = temp_reference("blob-first");
    let cache = dir.join("cache");
    // An earlier replica: computes everything and persists it.
    let mut cfg = config();
    cfg.persist_after = Duration::ZERO;
    let earlier = Counting::new(Duration::ZERO, false);
    let first = Arc::new(
        reloading_state(&dir, cfg, earlier.clone())
            .await
            .with_response_store(Arc::new(LocalStore::new(&cache))),
    );
    let report = prewarm::run(&first, first.snapshot.load_full(), Trigger::Startup).await;
    let n = prewarm::examples().len();
    assert_eq!(report.computed, n, "{report:?}");
    let (_, distinct) = load_examples(&first, &earlier, "fixture-v1").await;
    for _ in 0..200 {
        if persisted(&cache, "fixture-v1") == distinct {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(persisted(&cache, "fixture-v1"), distinct);

    // A new replica with a searcher that would take 30 s per call, and a
    // budget far too short to compute anything.
    let mut cfg = config();
    cfg.prewarm_startup_budget = Duration::from_secs(3);
    let slow = Counting::new(Duration::from_secs(30), false);
    let store = Gauged::new(&cache, Duration::from_millis(20));
    let state = Arc::new(
        reloading_state(&dir, cfg, slow.clone())
            .await
            .with_response_store(store.clone()),
    );
    let started = Instant::now();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert!(started.elapsed() < Duration::from_secs(3), "{report:?}");
    assert_eq!(slow.calls(), 0, "nothing was computed: {report:?}");
    assert_eq!(report.cached, n, "{report:?}");
    assert_eq!(report.computed, 0, "{report:?}");
    assert_eq!(report.examples_warm, n, "{report:?}");
    assert_eq!(report.skipped, 0, "{report:?}");
    assert_eq!(report.ok, report.queries, "{report:?}");
    // One read per example, several at once, never more than the bound.
    assert_eq!(store.reads.load(Ordering::SeqCst), n);
    let most = store.most.load(Ordering::SeqCst);
    assert!(
        most > 1 && most <= prewarm::LOADS_AT_ONCE,
        "{most} reads at once"
    );
    // Visitors find every example in the in-process cache.
    let reads = store.reads.load(Ordering::SeqCst);
    assert_eq!(visit_examples(&state, &slow, "fixture-v1").await, 0);
    assert_eq!(store.reads.load(Ordering::SeqCst), reads);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The examples visitors searched most (in the search log) are computed
/// first, then the rest in the file's order.
#[tokio::test]
async fn the_most_searched_examples_are_warmed_first() {
    use usnm_api::searchlog::{day_path, LogConfig, SearchLog};
    use usnm_core::params::{RawParams, SearchRequest};
    use usnm_store::ObjectStore;

    let dir = temp_reference("order");
    let store = Arc::new(LocalStore::new(dir.join("searches")));
    let yesterday = chrono::Utc::now().date_naive().pred_opt().unwrap();
    let bounds = (
        chrono::NaiveDate::from_ymd_opt(1770, 1, 1).unwrap(),
        chrono::NaiveDate::from_ymd_opt(1963, 12, 31).unwrap(),
    );
    let ex = prewarm::examples();
    let request = |e: &prewarm::Example| {
        SearchRequest::from_raw(
            &RawParams::parse(&e.aggregate).unwrap(),
            bounds,
            Analyzers::default(),
        )
        .unwrap()
    };
    // The search log's record of a visitor running example `e`.
    let clicked = |e: &prewarm::Example| {
        let req = request(e);
        let mut r = record(&req.query.to_string(), yesterday);
        r.from = req.filters.from;
        r.to = req.filters.to;
        r.bucket = req.bucket.as_str().into();
        r.lang = req.filters.langs.clone();
        r.state = req.filters.states.clone();
        assert_eq!(
            usnm_api::searchlog::canonical(&r, bounds, Analyzers::default()),
            Some(req.canonical()),
            "{}",
            e.id
        );
        serde_json::to_string(&r).unwrap()
    };
    let (last, tenth) = (&ex[ex.len() - 1], &ex[9]);
    let lines = [
        clicked(tenth),
        clicked(last),
        clicked(last),
        clicked(tenth),
        clicked(last),
    ];
    store
        .put(
            &day_path(yesterday),
            lines.join("\n").into_bytes(),
            "application/x-ndjson",
        )
        .await
        .unwrap();
    let log = SearchLog::start(
        store,
        LogConfig {
            flush_interval: Duration::from_secs(3600),
            ..LogConfig::default()
        },
        &opentelemetry::global::meter("test"),
    );
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(
        reloading_state(&dir, config(), backend.clone())
            .await
            .with_search_log(log.clone()),
    );
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(report.examples_warm, ex.len(), "{report:?}");
    assert_eq!(
        report.from_log, 0,
        "the examples aren't run twice: {report:?}"
    );

    let mut searched: Vec<String> = Vec::new();
    for q in backend.searched.lock().unwrap().iter() {
        if !searched.contains(q) {
            searched.push(q.clone());
        }
    }
    let expected: Vec<String> = [last, tenth, &ex[0], &ex[1]]
        .iter()
        .map(|e| request(e).query.to_string())
        .collect();
    assert_eq!(&searched[..4], &expected[..]);
    log.shutdown(Duration::from_secs(5)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a start, the warm-up doesn't start a search while every visitor
/// slot is taken: it waits, within its budget.
#[tokio::test]
async fn a_starting_warm_up_gives_way_to_visitors() {
    let dir = temp_reference("give-way");
    let mut cfg = config();
    cfg.compute_concurrency = 1;
    cfg.prewarm_startup_budget = Duration::from_millis(400);
    let backend = Counting::new(Duration::from_secs(2), false);
    let state = Arc::new(reloading_state(&dir, cfg, backend.clone()).await);

    // A visitor's search holds the only slot.
    let visitor = {
        let state = state.clone();
        tokio::spawn(async move {
            get(
                &state,
                "/v1/aggregate?q=silver&from=1896-01-01&to=1896-12-31&bucket=month",
            )
            .await
        })
    };
    while state.flights.free_slots() > 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let calls = backend.calls();
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    assert_eq!(backend.calls(), calls, "no warm-up search: {report:?}");
    assert_eq!(report.computed, 0, "{report:?}");
    assert_eq!(report.skipped, prewarm::examples().len(), "{report:?}");
    assert!(report.gave_way >= Duration::from_millis(300), "{report:?}");
    visitor.abort();

    // A publish doesn't wait: the old version serves meanwhile.
    let mut cfg = config();
    cfg.compute_concurrency = 1;
    let quick = Counting::new(Duration::ZERO, false);
    let state = Arc::new(reloading_state(&dir, cfg, quick).await);
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Publish).await;
    assert_eq!(report.gave_way, Duration::ZERO, "{report:?}");
    assert_eq!(report.examples_warm, prewarm::examples().len());
    let _ = std::fs::remove_dir_all(&dir);
}

/// `examples_warm` counts the examples still in the in-process cache when
/// the run ends: a cache too small for them all has evicted some.
#[tokio::test]
async fn examples_warm_counts_what_the_cache_still_holds() {
    use usnm_core::params::RawParams;

    let dir = temp_reference("evicted");
    let mut cfg = config();
    // Room for a few responses, not for every example's.
    cfg.cache_bytes = 64 * 1024;
    let backend = Counting::new(Duration::ZERO, false);
    let state = Arc::new(reloading_state(&dir, cfg, backend).await);
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    let n = prewarm::examples().len();
    assert_eq!(report.computed, n, "{report:?}");
    assert_eq!(report.skipped, 0, "{report:?}");

    let snap = state.snapshot.load_full();
    state.cache.run_pending_tasks().await;
    let held = prewarm::examples()
        .iter()
        .filter(|e| {
            let raw = RawParams::parse(&e.aggregate).unwrap();
            let canonical = snap.refdata.search_request(&raw).unwrap().canonical();
            state
                .cache
                .contains_key(&format!("fixture-v1|aggregate|{canonical}"))
        })
        .count();
    assert!(held < n, "the cache held all {n} examples");
    assert_eq!(report.examples_warm, held, "{report:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
