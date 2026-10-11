//! US News Map public search API (06).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderValue, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use moka::future::Cache;
use tokio::sync::{Mutex, OwnedMutexGuard, Semaphore};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use usnm_search::SearchBackend;
use usnm_store::ObjectStore;

pub mod config;
pub mod error;
pub mod flights;
pub mod prewarm;
pub mod ratelimit;
pub mod refdata;
mod routes;
pub mod searcher_metrics;
pub mod searchlog;
pub mod site;
pub mod status;
pub mod telemetry;
pub mod version;
pub mod versions;

use config::Config;
use error::ApiError;
use ratelimit::Limiter;
use refdata::{Current, RefData};

/// One published version: its reference data and the backend that serves its
/// index set. Swapped atomically so a request never mixes versions.
pub struct Snapshot {
    pub refdata: RefData,
    pub backend: Arc<dyn SearchBackend>,
    /// The version names a Congress set this API would search, but it
    /// couldn't be loaded or prepared, so the snapshot serves without it
    /// (#229). A refresh of the same version tries it again
    /// ([`reload_if_changed`]), so a one-off engine error doesn't turn
    /// Congress search off on this replica for the version's whole life.
    pub congress_dropped: bool,
}

/// How a snapshot's search backend is obtained.
pub enum Engine {
    /// Load exactly the JSONL indexes the version names from `{dir}/{id}.jsonl`.
    Memory { indexes_dir: PathBuf },
    /// A shared engine (Quickwit): the index set travels with the reference data.
    Shared(Arc<dyn SearchBackend>),
}

/// Builds snapshots from the reference store (Blob or a local directory).
pub struct Loader {
    pub reference: Arc<dyn ObjectStore>,
    pub engine: Engine,
    /// `USNM_CONGRESS_SEARCH` ([`Config::congress_search`], #229), applied
    /// before a version's Congress index set is loaded or prepared, so
    /// switching it off keeps a broken Congress index from holding back the
    /// newspapers.
    pub congress_search: bool,
}

impl Loader {
    pub async fn current(&self) -> Result<Current, String> {
        refdata::read_current(self.reference.as_ref()).await
    }

    /// The snapshot for whatever `current.json` names now.
    pub async fn snapshot(&self) -> Result<Snapshot, String> {
        let current = self.current().await?;
        self.load(current).await
    }

    pub async fn load(&self, current: Current) -> Result<Snapshot, String> {
        let mut refdata = RefData::load_for(self.reference.as_ref(), current).await?;
        // The setting applies before the Congress set is loaded or prepared,
        // which only the set the version will search is (#229).
        refdata.congress_search = self.congress_search;
        let congress = refdata.congress_index_set();
        let (backend, congress_loaded): (Arc<dyn SearchBackend>, Result<(), String>) =
            match &self.engine {
                Engine::Shared(b) => (b.clone(), Ok(())),
                Engine::Memory { indexes_dir } => {
                    let dir = indexes_dir.clone();
                    let mut ids = refdata.current.indexes.clone();
                    if let Some(ja) = &refdata.current.ja {
                        ids.extend(ja.indexes.iter().cloned());
                    }
                    let congress_ids = congress
                        .as_ref()
                        .map(|c| c.ids().to_vec())
                        .unwrap_or_default();
                    tokio::task::spawn_blocking(move || {
                        let mut backend = usnm_search::memory::MemoryBackend::new();
                        add_memory_indexes(&mut backend, &dir, &ids)?;
                        // On their own: a Congress index that won't load
                        // leaves the newspapers' (#229).
                        let congress = add_memory_indexes(&mut backend, &dir, &congress_ids);
                        Ok::<_, String>((Arc::new(backend) as Arc<dyn SearchBackend>, congress))
                    })
                    .await
                    .map_err(|e| e.to_string())??
                }
            };
        let set = refdata.index_set();
        backend.prepare(&set).await.map_err(|e| e.to_string())?;
        // A version goes live only with its Japanese index searchable too.
        if let Some(ja) = refdata.ja_index_set() {
            backend.prepare(&ja).await.map_err(|e| e.to_string())?;
        }
        // Its Congress index, when it has one, goes live only when it's
        // searchable, but never holds back the newspapers (#229).
        let mut congress_dropped = false;
        if let Some(congress) = congress {
            let ready = match congress_loaded {
                Ok(()) => backend.prepare(&congress).await.map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            if let Err(e) = ready {
                tracing::warn!(
                    version = refdata.version(),
                    error = %e,
                    "the Congress index set isn't searchable; serving the version without it"
                );
                refdata.current.congress = None;
                congress_dropped = true;
            }
        }
        Ok(Snapshot {
            refdata,
            backend,
            congress_dropped,
        })
    }
}

/// Apply the settings that change how a version is searched to a snapshot
/// before it serves or warms up.
fn configure(config: &Config, snapshot: &mut Snapshot) {
    snapshot.refdata.american_stories_search = config.american_stories_search;
    if snapshot.refdata.american_stories_setting_ignored() {
        tracing::warn!(
            version = snapshot.refdata.version(),
            "USNM_AMERICAN_STORIES_SEARCH=false has no effect on this version: its indexes \
             search LoC's and American Stories' text in one field (text_layout 2)"
        );
    }
    snapshot.refdata.congress_search = config.congress_search;
}

pub struct AppState {
    pub config: Config,
    pub snapshot: ArcSwap<Snapshot>,
    /// `None` disables hot reload.
    pub loader: Option<Loader>,
    /// Serialized responses keyed by `{index_version}|{endpoint}|{canonical}`.
    pub cache: Cache<String, Arc<Vec<u8>>>,
    /// Persistent response cache (06 §6.5), read on in-process misses.
    pub responses: Option<Arc<dyn ObjectStore>>,
    pub limiter: Option<Limiter>,
    /// Caps concurrent backend queries across all requests.
    pub permits: Semaphore,
    /// Response computations in flight, and the slots searches take.
    pub flights: flights::Flights,
    /// The pipeline status document (`/v1/status`).
    pub status: status::StatusService,
    pub metrics: telemetry::Metrics,
    /// Where `/v1/beacon` forwards the web app's page views; `None` (no
    /// Application Insights) accepts and drops them.
    pub page_views: Option<usnm_telemetry::PageViews>,
    /// Set while the first version warms up after a start: `/readyz` says
    /// not ready (see [`spawn_startup_warm_up`]).
    pub warming: AtomicBool,
    /// Runs the warm-ups one at a time (see [`WarmUps`]).
    pub(crate) warm_ups: WarmUps,
    /// The anonymous search log (`None` records nothing).
    pub search_log: Option<Arc<searchlog::SearchLog>>,
}

impl AppState {
    pub fn new(config: Config, backend: Arc<dyn SearchBackend>, refdata: RefData) -> Self {
        Self::with_loader(
            config,
            Snapshot {
                refdata,
                backend,
                congress_dropped: false,
            },
            None,
        )
    }

    pub fn with_loader(config: Config, mut snapshot: Snapshot, loader: Option<Loader>) -> Self {
        configure(&config, &mut snapshot);
        let cache = Cache::builder()
            .max_capacity(config.cache_bytes)
            .weigher(|k: &String, v: &Arc<Vec<u8>>| {
                u32::try_from(k.len() + v.len()).unwrap_or(u32::MAX)
            })
            .time_to_live(Duration::from_secs(24 * 3600))
            .build();
        Self {
            limiter: config
                .rate_limit
                .map(|l| Limiter::new(l, config.trusted_proxy_hops)),
            permits: Semaphore::new(config.backend_concurrency),
            flights: flights::Flights::new(config.compute_concurrency, config.search_queue),
            status: status::StatusService::new(status::PipelineSource::None, config.status_refresh),
            config,
            snapshot: ArcSwap::from_pointee(snapshot),
            loader,
            cache,
            responses: None,
            metrics: telemetry::Metrics::global(),
            page_views: None,
            warming: AtomicBool::new(false),
            warm_ups: WarmUps::default(),
            search_log: None,
        }
    }

    pub fn with_response_store(mut self, store: Arc<dyn ObjectStore>) -> Self {
        self.responses = Some(store);
        self
    }

    /// Record visitors' searches in `log` (see [`searchlog`]).
    pub fn with_search_log(mut self, log: Arc<searchlog::SearchLog>) -> Self {
        self.search_log = Some(log);
        self
    }

    /// Read the pipeline state (read-only) for `/v1/status`.
    pub fn with_pipeline(mut self, source: status::PipelineSource) -> Self {
        self.status = status::StatusService::new(source, self.config.status_refresh);
        self
    }

    /// Forward the web app's page views (`/v1/beacon`) to Application Insights.
    pub fn with_page_views(mut self, page_views: Option<usnm_telemetry::PageViews>) -> Self {
        self.page_views = page_views;
        self
    }

    /// Record metrics with these instruments instead of the global meter's.
    pub fn with_metrics(mut self, metrics: telemetry::Metrics) -> Self {
        self.metrics = metrics;
        self
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    let origins: Vec<HeaderValue> = state
        .config
        .allowed_origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([Method::GET]);

    let v1 = Router::new()
        .route("/meta", get(routes::meta))
        .route("/places", get(routes::places))
        .route("/titles", get(routes::titles))
        .route("/titles/{lccn}", get(routes::title))
        .route("/aggregate", get(routes::aggregate))
        .route("/hits", get(routes::hits))
        .route("/days", get(routes::days))
        .route("/coverage", get(routes::coverage))
        .route("/status", get(routes::status))
        .route("/versions", get(routes::versions))
        .route("/beacon", post(routes::beacon))
        .route_layer(middleware::from_fn_with_state(state.clone(), rate_limit))
        // Unknown API paths are problem details, never the site's index.
        .fallback(|| async { ApiError::NotFound("no such endpoint".to_owned()) });

    let site = state.config.site_dir.clone();
    let site_host: Arc<str> = state.config.site_host.as_str().into();
    let mut router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(routes::readyz))
        .nest("/v1", v1.clone())
        // Kept for clients that used the same-origin `/api/v1` prefix.
        .nest("/api/v1", v1)
        .route_layer(middleware::from_fn(telemetry::record_route))
        .with_state(state.clone());
    if let Some(dir) = site {
        router = router.fallback_service(site::router(dir));
    }
    // Inside the header layers, so redirects carry the security headers too.
    router = router.layer(middleware::from_fn_with_state(
        site_host,
        site::www_redirect,
    ));
    for (name, value) in site::SECURITY_HEADERS {
        router = router.layer(SetResponseHeaderLayer::if_not_present(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        ));
    }
    router
        .layer(cors)
        .layer(CompressionLayer::new())
        // One span per request with the route template, never the path or
        // query: query strings carry search text (09 §9.4.2).
        .layer(middleware::from_fn_with_state(state, telemetry::track))
}

async fn rate_limit(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    if let Some(limiter) = &state.limiter {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0.ip());
        if let Err(wait) = limiter.check(req.headers(), peer) {
            return ApiError::RateLimited(wait).into_response();
        }
    }
    next.run(req).await
}

/// Add the named JSONL indexes to an in-memory backend.
fn add_memory_indexes(
    backend: &mut usnm_search::memory::MemoryBackend,
    dir: &std::path::Path,
    ids: &[String],
) -> Result<(), String> {
    use std::io::BufRead;
    for id in ids {
        let path = dir.join(format!("{id}.jsonl"));
        let file = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut docs = Vec::new();
        for line in std::io::BufReader::new(file).lines() {
            let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
            docs.push(
                serde_json::from_str::<usnm_search::PageDoc>(&line)
                    .map_err(|e| format!("{}: {e}", path.display()))?,
            );
        }
        tracing::info!(index = %id, docs = docs.len(), "loaded memory index");
        backend.add_index(id, docs);
    }
    Ok(())
}

/// If `current.json` names a different version than the one serving, build
/// the new snapshot, warm it up and swap it in atomically. Returns whether it
/// swapped. The old version serves until the swap; a warm-up that fails or
/// runs out of budget is logged and the swap goes ahead.
pub async fn reload_if_changed(state: &Arc<AppState>) -> Result<bool, String> {
    let Some(loader) = &state.loader else {
        return Ok(false);
    };
    let result = reload(state, loader).await;
    match &result {
        Ok(true) => state.metrics.reload("published"),
        Ok(false) => {}
        Err(_) => state.metrics.reload("failed"),
    }
    result
}

async fn reload(state: &Arc<AppState>, loader: &Loader) -> Result<bool, String> {
    let next = loader.current().await?;
    let serving = state.snapshot.load_full();
    if next.index_version == serving.refdata.version() {
        // The same version: worth another load only to bring back a Congress
        // set it dropped (#229). It goes live only if that set is searchable
        // now, and without a warm-up: the version's newspaper searches are
        // already warm, under the same keys.
        if !serving.congress_dropped {
            return Ok(false);
        }
        let mut snapshot = loader.load(next).await?;
        if snapshot.congress_dropped {
            return Ok(false);
        }
        configure(&state.config, &mut snapshot);
        tracing::info!(
            version = snapshot.refdata.version(),
            "the Congress index set is searchable now"
        );
        state.snapshot.store(Arc::new(snapshot));
        return Ok(true);
    }
    let mut snapshot = loader.load(next).await?;
    configure(&state.config, &mut snapshot);
    let snapshot = Arc::new(snapshot);
    // Held through the swap, so a searcher restart's warm-up waiting for
    // its turn warms the new version.
    let _turn = state.warm_ups.claim().turn().await;
    prewarm::run(state, snapshot.clone(), prewarm::Trigger::Publish).await;
    tracing::info!(
        version = snapshot.refdata.version(),
        american_stories = snapshot.refdata.searches_american_stories(),
        "publishing new index version"
    );
    state.snapshot.store(snapshot);
    Ok(true)
}

/// Warm up the serving version after a start, reporting not ready until it
/// finishes or `config.ready_cap` passes. The default cap is past the
/// warm-up's budget, so in a rollout the old revision keeps the traffic
/// until this replica is warm. Past the cap (a hung warm-up, or a short cap
/// set where nothing else serves) the replica becomes ready and the warm-up
/// carries on within its own budget.
pub fn spawn_startup_warm_up(state: Arc<AppState>) -> tokio::task::JoinHandle<()> {
    state.warming.store(true, Ordering::Relaxed);
    let claim = state.warm_ups.claim();
    tokio::spawn(async move {
        let run = tokio::spawn({
            let state = state.clone();
            async move {
                let _turn = claim.turn().await;
                let snapshot = state.snapshot.load_full();
                prewarm::run(&state, snapshot, prewarm::Trigger::Startup).await
            }
        });
        if tokio::time::timeout(state.config.ready_cap, run)
            .await
            .is_err()
        {
            tracing::warn!(
                cap_ms = state.config.ready_cap.as_millis() as u64,
                "warm-up still running at the readiness cap; reporting ready"
            );
        }
        state.warming.store(false, Ordering::Relaxed);
    })
}

/// Runs the warm-ups one at a time, whatever started them: a start, a
/// publish or a searcher restart (06 §6.5): two at once would only take
/// turns on the warm-up's one computation slot, and a restart's run must
/// come after a run that may have searched the old searcher. Each waits for
/// its turn, first come first served, and none is dropped, except that a
/// searcher restart seen while another restart's warm-up is still waiting
/// shares that one: at most one waits.
#[derive(Default)]
pub(crate) struct WarmUps {
    turn: Arc<Mutex<()>>,
    /// Set from a searcher restart until its warm-up's turn comes.
    restart_waiting: AtomicBool,
}

/// A place in [`WarmUps`]' line: the turn itself if it was free, else the
/// wait for it.
enum Claim {
    Now(OwnedMutexGuard<()>),
    Wait(Arc<Mutex<()>>),
}

impl WarmUps {
    /// Take the turn now if no warm-up holds it (and none is waiting for it),
    /// so a warm-up claimed before another is spawned runs first.
    fn claim(&self) -> Claim {
        match self.turn.clone().try_lock_owned() {
            Ok(turn) => Claim::Now(turn),
            Err(_) => Claim::Wait(self.turn.clone()),
        }
    }
}

impl Claim {
    /// The turn, held while the warm-up runs.
    async fn turn(self) -> OwnedMutexGuard<()> {
        match self {
            Self::Now(turn) => turn,
            Self::Wait(turn) => turn.lock_owned().await,
        }
    }
}

/// Warm up the serving version again after the searcher restarted under
/// this running replica (its own app, #267, or the sidecar alone): the new
/// searcher's caches are empty. The replica stays ready; it is warm itself.
/// A restart seen while another warm-up runs (a start's, a publish's or an
/// earlier restart's) is warmed after it: that run may have searched the
/// searcher before it restarted. `None` when a restart's warm-up is already
/// waiting for its turn, which warms this one too.
pub fn spawn_searcher_warm_up(state: Arc<AppState>) -> Option<tokio::task::JoinHandle<()>> {
    if state.warm_ups.restart_waiting.swap(true, Ordering::AcqRel) {
        tracing::info!("the searcher restarted again; the warm-up waiting for its turn covers it");
        return None;
    }
    let claim = state.warm_ups.claim();
    if matches!(claim, Claim::Wait(_)) {
        tracing::info!("the searcher restarted during a warm-up; warming it again after that one");
    }
    Some(tokio::spawn(async move {
        let _turn = claim.turn().await;
        state
            .warm_ups
            .restart_waiting
            .store(false, Ordering::Release);
        let snapshot = state.snapshot.load_full();
        prewarm::run(&state, snapshot, prewarm::Trigger::SearcherRestart).await;
    }))
}

/// Poll `current.json` every `refresh_interval` (no-op without a loader), and
/// prune idle rate-limit buckets.
pub fn spawn_background(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(state.config.refresh_interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Some(limiter) = &state.limiter {
                limiter.housekeeping();
            }
            if let Err(e) = reload_if_changed(&state).await {
                tracing::warn!(error = %e, "reload failed; keeping current version");
            }
        }
    });
}
