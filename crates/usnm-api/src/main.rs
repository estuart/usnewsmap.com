use std::sync::Arc;

use usnm_api::config::{BackendKind, Config};
use usnm_api::searchlog::{LogConfig, SearchLog};
use usnm_api::status::PipelineSource;
use usnm_api::{
    app, searcher_metrics, spawn_background, spawn_startup_warm_up, telemetry, AppState, Engine,
    Loader,
};
use usnm_search::quickwit::QuickwitBackend;
use usnm_state::cosmos::CosmosDocs;
use usnm_store::credential;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let exporters = usnm_telemetry::init(telemetry::SERVICE);
    let result = serve(exporters.page_views()).await;
    // A failure is one JSON line (not also plain text from Rust's handler).
    let code = match &result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "usnm-api failed");
            std::process::ExitCode::FAILURE
        }
    };
    // Flush what's buffered before exit, on every path.
    exporters.shutdown().await;
    code
}

async fn serve(
    page_views: Option<usnm_telemetry::PageViews>,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    // Each published version gets a fresh snapshot. The memory backend reloads
    // exactly the indexes the new version names; Quickwit is shared because
    // the index set travels with the reference data.
    let mut searcher = None;
    let engine = match &config.backend {
        BackendKind::Memory => Engine::Memory {
            indexes_dir: config.data_dir.join("indexes"),
        },
        // A search's limit fits a warm-up query and a search computation
        // (the handlers cut each one at its own limit); index lookups and
        // health checks keep the short one.
        BackendKind::Quickwit(url) => {
            let qw = Arc::new(
                QuickwitBackend::new(url, config.search_timeout)?.with_search_timeout(
                    config
                        .search_timeout
                        .max(config.prewarm_query_timeout)
                        .max(config.compute_cap),
                ),
            );
            searcher = Some(qw.clone());
            Engine::Shared(qw)
        }
    };
    let loader = Loader {
        reference: usnm_store::open(&config.reference_url)?,
        engine,
        congress_search: config.congress_search,
    };
    let snapshot = initial_snapshot(&loader).await?;
    tracing::info!(
        version = snapshot.refdata.version(),
        synthetic = snapshot.refdata.current.synthetic,
        backend = ?config.backend,
        reference = ?loader.reference,
        "starting usnm-api"
    );
    let responses = config
        .response_cache_url
        .as_deref()
        .map(usnm_store::open)
        .transpose()?;
    // The pipeline state, read-only, for /v1/status. The status page would
    // rather say "busy" than wait out Cosmos throttling for minutes.
    let pipeline = match (&config.cosmos_endpoint, &config.state_file) {
        (Some(endpoint), _) => PipelineSource::Docs(Arc::new(
            CosmosDocs::new(
                endpoint,
                usnm_state::DATABASE,
                credential::from_env_for(credential::COSMOS_RESOURCE),
            )?
            .with_rate_limit_retry(std::time::Duration::from_secs(10)),
        )),
        (None, Some(path)) => PipelineSource::File(path.clone()),
        (None, None) => PipelineSource::None,
    };
    tracing::info!(
        pipeline_state = match &pipeline {
            PipelineSource::Docs(_) => "cosmos",
            PipelineSource::File(_) => "file",
            PipelineSource::None => "none",
        },
        "status page source"
    );
    // The anonymous search log (06 §6.8): its writer stages a batch every few
    // minutes and once more at shutdown.
    let search_log = config
        .search_log_url
        .as_deref()
        .map(usnm_store::open)
        .transpose()?
        .map(|store| {
            tracing::info!(store = ?store, "search log enabled");
            SearchLog::start(
                store,
                LogConfig {
                    flush_interval: config.search_log_flush,
                    ..LogConfig::default()
                },
                &opentelemetry::global::meter(telemetry::SERVICE.name),
            )
        });
    let bind = config.bind.clone();
    let mut state = AppState::with_loader(config, snapshot, Some(loader))
        .with_pipeline(pipeline)
        .with_page_views(page_views);
    if let Some(store) = responses {
        tracing::info!(store = ?store, "persistent response cache enabled");
        state = state.with_response_store(store);
    }
    if let Some(log) = &search_log {
        state = state.with_search_log(log.clone());
    }
    let state = Arc::new(state);
    {
        // Whether searches cover American Stories' text (05 §5.5.4): the
        // setting, whether the version has the text, and the outcome.
        let snap = state.snapshot.load();
        tracing::info!(
            version = snap.refdata.version(),
            setting = state.config.american_stories_search,
            in_version = snap.refdata.has_american_stories(),
            searched = snap.refdata.searches_american_stories(),
            text_layout = snap.refdata.text_layout().version(),
            "American Stories' text search (USNM_AMERICAN_STORIES_SEARCH)"
        );
    }
    telemetry::observe_index_version(
        &state,
        &opentelemetry::global::meter(telemetry::SERVICE.name),
    );
    // The searcher sidecar's caches, thread pools and runtimes (#125,
    // #251), read over localhost.
    if let Some(qw) = searcher.filter(|_| !state.config.searcher_metrics_interval.is_zero()) {
        searcher_metrics::spawn(
            state.config.searcher_metrics_interval,
            &opentelemetry::global::meter(telemetry::SERVICE.name),
            move || {
                let qw = qw.clone();
                async move { qw.searcher_metrics().await.map_err(|e| e.to_string()) }
            },
        );
    }
    // Listening (so liveness passes) but not ready until the caches are warm.
    spawn_startup_warm_up(state.clone());
    spawn_background(state.clone());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    tracing::info!(%bind, "listening");
    // Peer addresses feed the rate limiter when there is no trusted proxy.
    let served = axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await;
    // Container Apps allows 30 s after SIGTERM; the requests have drained.
    if let Some(log) = search_log {
        log.shutdown(std::time::Duration::from_secs(10)).await;
    }
    served?;
    Ok(())
}

/// The first snapshot, retried while the search sidecar starts alongside the
/// API (and while a new identity's role assignments propagate).
async fn initial_snapshot(loader: &Loader) -> Result<usnm_api::Snapshot, String> {
    const ATTEMPTS: u32 = 30;
    let mut delay = std::time::Duration::from_secs(1);
    let mut attempt = 1;
    loop {
        match loader.snapshot().await {
            Ok(s) => return Ok(s),
            Err(e) if attempt < ATTEMPTS => {
                tracing::warn!(error = %e, attempt, "initial load failed; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(15));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
    tracing::info!("shutting down");
}
