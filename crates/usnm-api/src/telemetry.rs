//! Request telemetry and the API's metrics (08 §8.1.2, 09 §9.2).
//!
//! Every request except the health probes gets one `request` span, which
//! the Application Insights exporter turns into a request (`AppRequests`):
//! method, the matched route template (`/v1/aggregate`), status code and
//! duration, failed only on a 5xx. It never carries the raw path or query
//! string: query strings hold search text (09 §9.4.2). There is no console
//! line per request and no request count or duration metric: `AppRequests`
//! has them, and the copies were most of the API's log volume (#231).
//!
//! The metrics go to `AppMetrics` when telemetry is on; otherwise the
//! instruments come from the no-op global meter and record nothing.

use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::metrics::{Counter, Histogram, Meter, ObservableGauge};
use opentelemetry::KeyValue;
use tracing::field::Empty;
use tracing::Instrument;

use crate::error::ApiError;
use crate::AppState;

/// The cloud role the API reports as.
pub const SERVICE: usnm_telemetry::Service = usnm_telemetry::Service {
    name: "usnm-api",
    version: env!("CARGO_PKG_VERSION"),
};

/// Container Apps probes: neither traced nor logged.
const PROBES: [&str; 2] = ["/healthz", "/readyz"];

/// Route label for a request no API route matched: the site's files...
pub const SITE_ROUTE: &str = "(site)";
/// ...or an unknown path under `/v1` or `/api/v1` (and CORS preflights).
pub const NO_ROUTE: &str = "(no route)";

/// The API's instruments.
pub struct Metrics {
    /// Search backend time for one response, by `endpoint` (aggregate,
    /// hits) and `outcome` (ok, too_broad, timeout, error).
    backend: Histogram<f64>,
    /// Response cache lookups by `layer` (memory, blob) and `result` (hit, miss).
    cache: Counter<u64>,
    /// Requests refused as the client's fault, by `reason`.
    rejected: Counter<u64>,
    /// Searches that needed longer than a visitor waits, by `endpoint` and
    /// `outcome` (ok, timeout, error, abandoned), counted once each when they
    /// end; and searches that found no free computation slot (busy).
    slow_searches: Counter<u64>,
    /// How long a visitor's search waited for a computation slot, by
    /// `endpoint` and `outcome` (`slot`, or `busy` when the queue was full or
    /// it waited past its limit). Zero when a slot was free.
    queue_wait: Histogram<f64>,
    /// Reference-data reloads by `outcome` (published, failed).
    reloads: Counter<u64>,
    /// One warm-up run, by `trigger` (startup, publish, searcher_restart).
    prewarm_duration: Histogram<f64>,
    /// Warm-up queries by `outcome` (ok, timeout, error, skipped).
    prewarm_queries: Counter<u64>,
    /// Time of one warm-up query that ran, by `endpoint` and `source`
    /// (cache, computed).
    prewarm_query_duration: Histogram<f64>,
    /// Valid page views from the web app, by `outcome` (forwarded, bot,
    /// opted_out, other_origin, dropped, off).
    beacons: Counter<u64>,
    index_version: OnceLock<ObservableGauge<u64>>,
}

impl Metrics {
    pub fn new(meter: &Meter) -> Self {
        Self {
            backend: meter
                .f64_histogram("api.backend_duration_seconds")
                .with_unit("s")
                .with_description("Search backend time for one response, by endpoint and outcome")
                .build(),
            cache: meter
                .u64_counter("api.cache_lookups")
                .with_description("Response cache lookups by layer (memory, blob) and result")
                .build(),
            rejected: meter
                .u64_counter("api.rejected_queries")
                .with_description("Requests refused as the client's fault, by reason")
                .build(),
            slow_searches: meter
                .u64_counter("api.slow_searches")
                .with_description(
                    "Searches that took longer than a visitor waits, by endpoint and outcome \
                     (ok, timeout, error, abandoned, busy)",
                )
                .build(),
            queue_wait: meter
                .f64_histogram("api.search_queue_wait_seconds")
                .with_unit("s")
                .with_description(
                    "Time a search waited for a computation slot, by endpoint and outcome \
                     (slot, busy)",
                )
                .build(),
            reloads: meter
                .u64_counter("api.reference_reloads")
                .with_description("Reference-data reloads by outcome (published, failed)")
                .build(),
            prewarm_duration: meter
                .f64_histogram("api.prewarm_duration_seconds")
                .with_unit("s")
                .with_description(
                    "One cache warm-up run, by trigger (startup, publish, searcher_restart)",
                )
                .build(),
            prewarm_queries: meter
                .u64_counter("api.prewarm_queries")
                .with_description("Cache warm-up queries by outcome (ok, timeout, error, skipped)")
                .build(),
            prewarm_query_duration: meter
                .f64_histogram("api.prewarm_query_seconds")
                .with_unit("s")
                .with_description(
                    "One cache warm-up query that ran, by endpoint and source (cache, computed)",
                )
                .build(),
            beacons: meter
                .u64_counter("api.beacons")
                .with_description(
                    "Valid page views from the web app, by outcome (forwarded, bot, opted_out, \
                     other_origin, dropped, off)",
                )
                .build(),
            index_version: OnceLock::new(),
        }
    }

    /// From the global meter provider (`usnm_telemetry::init` installs it).
    pub fn global() -> Self {
        Self::new(&opentelemetry::global::meter(SERVICE.name))
    }

    pub(crate) fn backend<T>(
        &self,
        endpoint: &'static str,
        elapsed: Duration,
        r: &Result<T, ApiError>,
    ) {
        let outcome = match r {
            Ok(_) => "ok",
            Err(ApiError::TooBroad(_)) => "too_broad",
            Err(ApiError::Timeout) => "timeout",
            Err(_) => "error",
        };
        self.backend.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("endpoint", endpoint),
                KeyValue::new("outcome", outcome),
            ],
        );
    }

    pub(crate) fn cache(&self, layer: &'static str, hit: bool) {
        self.cache.add(
            1,
            &[
                KeyValue::new("layer", layer),
                KeyValue::new("result", if hit { "hit" } else { "miss" }),
            ],
        );
    }

    pub(crate) fn queue_wait(&self, endpoint: &'static str, elapsed: Duration, got_slot: bool) {
        self.queue_wait.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("endpoint", endpoint),
                KeyValue::new("outcome", if got_slot { "slot" } else { "busy" }),
            ],
        );
    }

    pub(crate) fn slow_search(&self, endpoint: &'static str, outcome: &'static str) {
        self.slow_searches.add(
            1,
            &[
                KeyValue::new("endpoint", endpoint),
                KeyValue::new("outcome", outcome),
            ],
        );
    }

    pub(crate) fn beacon(&self, outcome: &'static str) {
        self.beacons.add(1, &[KeyValue::new("outcome", outcome)]);
    }

    pub(crate) fn reload(&self, outcome: &'static str) {
        self.reloads.add(1, &[KeyValue::new("outcome", outcome)]);
    }

    pub(crate) fn prewarm_query(
        &self,
        endpoint: &'static str,
        source: &'static str,
        elapsed: Duration,
    ) {
        self.prewarm_query_duration.record(
            elapsed.as_secs_f64(),
            &[
                KeyValue::new("endpoint", endpoint),
                KeyValue::new("source", source),
            ],
        );
    }

    pub(crate) fn prewarm(&self, trigger: &'static str, report: &crate::prewarm::Report) {
        self.prewarm_duration.record(
            report.elapsed.as_secs_f64(),
            &[KeyValue::new("trigger", trigger)],
        );
        for (outcome, n) in [
            ("ok", report.ok),
            ("timeout", report.timed_out),
            ("error", report.failed),
            ("skipped", report.skipped),
        ] {
            if n > 0 {
                self.prewarm_queries
                    .add(n as u64, &[KeyValue::new("outcome", outcome)]);
            }
        }
    }
}

/// Report the serving index version as the gauge `api.index_version`: value
/// 1, with the version in the `index_version` attribute. Observed at each
/// export, so it always names the version being served. Once per state.
pub fn observe_index_version(state: &Arc<AppState>, meter: &Meter) {
    let weak: Weak<AppState> = Arc::downgrade(state);
    state.metrics.index_version.get_or_init(|| {
        meter
            .u64_observable_gauge("api.index_version")
            .with_description("1, with the index version being served as an attribute")
            .with_callback(move |observer| {
                if let Some(state) = weak.upgrade() {
                    let version = state.snapshot.load().refdata.version().to_owned();
                    observer.observe(1, &[KeyValue::new("index_version", version)]);
                }
            })
            .build()
    });
}

/// Why a request was refused as the client's fault, for `api.rejected_queries`.
/// [`ApiError`] attaches it to its response.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rejection(pub &'static str);

/// The `type` of a problem response (`/errors/backend-timeout`), recorded on
/// the request span as `usnm.problem`, so alerts can tell a slow search from
/// a broken one.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProblemType(pub &'static str);

/// The index version a response was computed from, attached by the search
/// endpoints' response cache for the request span.
#[derive(Debug, Clone)]
pub(crate) struct ServedVersion(pub String);

/// The route a request matched, written by [`record_route`] for [`track`].
#[derive(Clone, Default)]
struct RouteSlot(Arc<OnceLock<String>>);

/// Outermost middleware: the request span, and the rejected-query count.
pub(crate) async fn track(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if PROBES.contains(&path) {
        return next.run(req).await;
    }
    let api = is_api_path(path);
    let method = req.method().clone();
    let slot = RouteSlot::default();
    req.extensions_mut().insert(slot.clone());
    // Fields in OpenTelemetry's HTTP conventions, which the exporter maps to
    // the request's name, result code and success. No url.* fields: the raw
    // path and query stay out.
    let span = tracing::info_span!(
        "request",
        otel.name = %method,
        otel.kind = "server",
        otel.status_code = Empty,
        http.request.method = %method,
        http.route = Empty,
        http.response.status_code = Empty,
        usnm.index_version = Empty,
        usnm.problem = Empty,
    );
    let resp = next.run(req).instrument(span.clone()).await;
    let status = resp.status();
    if slot.0.get().is_none() {
        let r = if api { NO_ROUTE } else { SITE_ROUTE };
        span.record("http.route", r);
        span.record("otel.name", format!("{method} {r}"));
    }
    // The version the response came from; the one serving now for responses
    // that don't say (a reload may have swapped it in since).
    match resp.extensions().get::<ServedVersion>() {
        Some(ServedVersion(v)) => span.record("usnm.index_version", v.as_str()),
        None => span.record(
            "usnm.index_version",
            state.snapshot.load().refdata.version(),
        ),
    };
    if let Some(ProblemType(kind)) = resp.extensions().get::<ProblemType>() {
        span.record("usnm.problem", *kind);
    }
    span.record("http.response.status_code", status.as_u16());
    // Client errors (4xx) are the client's outcome, not a failure (09 §9.1).
    span.record(
        "otel.status_code",
        if status.as_u16() >= 500 {
            "ERROR"
        } else {
            "OK"
        },
    );
    drop(span);
    if let Some(Rejection(reason)) = resp.extensions().get::<Rejection>() {
        state
            .metrics
            .rejected
            .add(1, &[KeyValue::new("reason", *reason)]);
    }
    resp
}

/// Route middleware: tells [`track`] and the request span which route
/// template matched.
pub(crate) async fn record_route(req: Request, next: Next) -> Response {
    if let Some(matched) = req.extensions().get::<MatchedPath>() {
        let route = matched.as_str();
        if let Some(slot) = req.extensions().get::<RouteSlot>() {
            let _ = slot.0.set(route.to_owned());
        }
        let span = tracing::Span::current();
        span.record("http.route", route);
        span.record("otel.name", format!("{} {route}", req.method()));
    }
    next.run(req).await
}

fn is_api_path(path: &str) -> bool {
    ["/v1", "/api/v1"].iter().any(|p| {
        path.strip_prefix(p)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths() {
        assert!(is_api_path("/v1"));
        assert!(is_api_path("/v1/nope"));
        assert!(is_api_path("/api/v1/aggregate"));
        assert!(!is_api_path("/v10"));
        assert!(!is_api_path("/"));
        assert!(!is_api_path("/assets/app.js"));
    }
}
