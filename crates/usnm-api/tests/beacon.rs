//! `POST /v1/beacon`: what the web app may send, what reaches Application
//! Insights (a fake ingestion endpoint) as a page view, and what never does.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use opentelemetry::metrics::MeterProvider as _;
use serde_json::Value;
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;
use usnm_api::config::{Config, RateLimit};
use usnm_api::refdata::RefData;
use usnm_api::telemetry::{Metrics, SERVICE};
use usnm_api::{app, AppState};
use usnm_search::memory::MemoryBackend;
use usnm_store::LocalStore;
use usnm_telemetry::testing::{fake_ingestion, Plain, Seen};
use usnm_telemetry::PageViews;

const IKEY: &str = "00000000-0000-0000-0000-000000000000";
const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1";
/// The visitor's address as the Container Apps ingress appends it.
const CLIENT: &str = "203.0.113.7";

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data")
}

fn config() -> Config {
    let mut c = Config::from_lookup(|_| None).unwrap();
    c.data_dir = data_dir();
    c.reference_url = data_dir().display().to_string();
    c.rate_limit = None;
    c
}

async fn state(cfg: Config, page_views: Option<PageViews>, metrics: Metrics) -> Arc<AppState> {
    let refdata = RefData::load(&LocalStore::new(data_dir())).await.unwrap();
    Arc::new(
        AppState::new(cfg, Arc::new(MemoryBackend::new()), refdata)
            .with_page_views(page_views)
            .with_metrics(metrics),
    )
}

/// A browser's page view: text/plain like `navigator.sendBeacon`, from the
/// site's origin, through the ingress; `with` replaces or adds headers.
fn post(with: &[(&str, &str)]) -> axum::http::request::Builder {
    let mut headers = vec![
        ("content-type", "text/plain;charset=UTF-8"),
        ("origin", "https://usnewsmap.com"),
        ("user-agent", CHROME),
        ("x-forwarded-for", CLIENT),
    ];
    for &(name, value) in with {
        headers.retain(|(n, _)| *n != name);
        headers.push((name, value));
    }
    headers
        .into_iter()
        .fold(Request::post("/v1/beacon"), |b, (n, v)| b.header(n, v))
}

async fn send(state: &Arc<AppState>, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn beacon(state: &Arc<AppState>, body: &str) -> (StatusCode, Value) {
    send(state, post(&[]).body(Body::from(body.to_owned())).unwrap()).await
}

/// Every envelope of type `base_type` the fake endpoint received.
fn envelopes(seen: &Seen, base_type: &str) -> Vec<Value> {
    seen.uploads()
        .iter()
        .flat_map(|u| match serde_json::from_str::<Value>(&u.json).unwrap() {
            Value::Array(items) => items,
            other => vec![other],
        })
        .filter(|e| e["data"]["baseType"] == base_type)
        .collect()
}

/// A subscriber for tests that don't check telemetry. Without one, a test
/// running on another thread can leave the request span's callsite disabled
/// for the test that exports it.
fn quiet() -> tracing::subscriber::DefaultGuard {
    tracing::subscriber::set_default(tracing_subscriber::registry())
}

/// Console output captured from the JSON log layer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn page_views_are_forwarded_with_route_referrer_and_coarse_client() {
    let (endpoint, seen) = fake_ingestion().await;
    let conn = format!("InstrumentationKey={IKEY};IngestionEndpoint={endpoint}");
    let page_views =
        PageViews::start(&conn, Plain::with_token("tok"), "usnm-web", "usnm-api:test").unwrap();
    let (tracer, meter_provider) =
        usnm_telemetry::providers(&conn, Plain::with_token("tok"), SERVICE).unwrap();
    let meter = meter_provider.meter(SERVICE.name);
    let console = Captured::default();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer().json().with_writer({
            let c = console.clone();
            move || c.clone()
        }))
        .with(usnm_telemetry::otel_layer(&tracer, SERVICE));
    let guard = tracing::subscriber::set_default(subscriber);
    let state = state(config(), Some(page_views.clone()), Metrics::new(&meter)).await;

    // A landing from a search engine, with campaign tags; then the status page.
    let (status, _) = beacon(
        &state,
        r#"{"route":"search","title":"US News Map","referrer_origin":"https://www.Google.com",
            "utm_source":"NewsLetter","utm_medium":"email","utm_campaign":"Fall-2026"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let req = post(&[("user-agent", IPHONE)])
        .body(Body::from(
            r#"{"route":"datasets","referrer_origin":"internal"}"#,
        ))
        .unwrap();
    assert_eq!(send(&state, req).await.0, StatusCode::NO_CONTENT);
    // fetch() with keepalive sends JSON; a referrer on the site itself is internal.
    let req = post(&[("content-type", "application/json")])
        .body(Body::from(
            r#"{"route":"not-found","referrer_origin":"https://www.usnewsmap.com"}"#,
        ))
        .unwrap();
    assert_eq!(send(&state, req).await.0, StatusCode::NO_CONTENT);
    // Accepted, not forwarded: a crawler, Do Not Track, GPC, another site's page.
    for (name, value) in [
        (
            "user-agent",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        ),
        ("dnt", "1"),
        ("sec-gpc", "1"),
        ("origin", "https://elsewhere.example"),
    ] {
        let req = post(&[(name, value)])
            .body(Body::from(r#"{"route":"privacy","utm_source":"skipped"}"#))
            .unwrap();
        assert_eq!(send(&state, req).await.0, StatusCode::NO_CONTENT, "{name}");
    }
    // Refused.
    let (status, _) = beacon(&state, r#"{"route":"search","q":"zebrasecret"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    page_views.flush().await;
    drop(guard);
    tokio::task::spawn_blocking(move || {
        tracer.shutdown().unwrap();
        meter_provider.shutdown().unwrap();
    })
    .await
    .unwrap();

    for u in seen.uploads() {
        assert_eq!(u.path, "/v2.1/track");
        assert_eq!(u.authorization.as_deref(), Some("Bearer tok"));
    }
    let views = envelopes(&seen, "PageViewData");
    assert_eq!(views.len(), 3, "{}", seen.all_json());
    let first = &views[0];
    assert_eq!(first["name"], "Microsoft.ApplicationInsights.PageView");
    assert_eq!(first["iKey"], IKEY);
    assert_eq!(first["tags"]["ai.cloud.role"], "usnm-web");
    assert_eq!(first["tags"]["ai.location.ip"], CLIENT);
    assert_eq!(first["tags"]["ai.internal.sdkVersion"], "usnm-api:test");
    let data = &first["data"]["baseData"];
    assert_eq!(data["name"], "search");
    assert_eq!(data["url"], "https://usnewsmap.com/");
    assert_eq!(
        data["properties"],
        serde_json::json!({
            "browser": "Chrome",
            "device_type": "desktop",
            "referrer_origin": "https://www.google.com",
            "title": "US News Map",
            "utm_campaign": "fall-2026",
            "utm_medium": "email",
            "utm_source": "newsletter",
        })
    );
    let data = &views[1]["data"]["baseData"];
    assert_eq!(data["name"], "datasets");
    assert_eq!(data["url"], "https://usnewsmap.com/datasets");
    assert_eq!(
        data["properties"],
        serde_json::json!({
            "browser": "Safari",
            "device_type": "mobile",
            "referrer_origin": "internal",
        })
    );
    let data = &views[2]["data"]["baseData"];
    assert_eq!(data["name"], "not-found");
    assert!(data.get("url").is_none(), "{data}");
    assert_eq!(data["properties"]["referrer_origin"], "internal");

    // The address is only the geolocation tag, and the user agent only its family.
    let all = seen.all_json();
    for view in &views {
        let props = view["data"]["baseData"]["properties"].to_string();
        assert!(!props.contains(CLIENT), "{props}");
    }
    assert!(!all.contains("AppleWebKit"), "{all}");
    assert!(
        !all.contains("zebrasecret") && !all.contains("skipped"),
        "{all}"
    );

    // Counted by outcome; the refused one as a rejected request.
    let metric = |name: &str, key: &str, value: &str| -> f64 {
        envelopes(&seen, "MetricData")
            .iter()
            .map(|e| &e["data"]["baseData"])
            .filter(|d| d["properties"][key] == value)
            .flat_map(|d| d["metrics"].as_array().unwrap().clone())
            .filter(|m| m["name"] == name)
            .map(|m| m["value"].as_f64().unwrap())
            .sum()
    };
    assert_eq!(metric("api.beacons", "outcome", "forwarded"), 3.0);
    assert_eq!(metric("api.beacons", "outcome", "bot"), 1.0);
    assert_eq!(metric("api.beacons", "outcome", "opted_out"), 2.0);
    assert_eq!(metric("api.beacons", "outcome", "other_origin"), 1.0);
    assert_eq!(metric("api.rejected_queries", "reason", "bad_beacon"), 1.0);

    // The requests are exported by route like any other, without their bodies.
    let requests: Vec<String> = envelopes(&seen, "RequestData")
        .iter()
        .map(|e| e["data"]["baseData"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(requests.len(), 8);
    assert!(
        requests.iter().all(|n| n == "POST /v1/beacon"),
        "{requests:?}"
    );

    // Nothing from a page view reaches the console.
    let console = String::from_utf8(console.0.lock().unwrap().clone()).unwrap();
    for secret in [
        CLIENT,
        "google",
        "newsletter",
        "fall-2026",
        "zebrasecret",
        "iPhone",
        "Chrome/",
    ] {
        assert!(
            !console.contains(secret),
            "`{secret}` was logged: {console}"
        );
    }
}

#[tokio::test]
async fn a_beacon_cannot_carry_search_text() {
    let _guard = quiet();
    let (endpoint, seen) = fake_ingestion().await;
    let conn = format!("InstrumentationKey={IKEY};IngestionEndpoint={endpoint}");
    let page_views = PageViews::start(&conn, Plain::with_token("tok"), "usnm-web", "t").unwrap();
    let state = state(config(), Some(page_views.clone()), Metrics::global()).await;
    let refused = [
        // Unknown fields: the search, its filters, a path or a URL.
        r#"{"route":"search","q":"zebrasecret"}"#,
        r#"{"route":"search","query":"zebrasecret"}"#,
        r#"{"route":"search","path":"/?q=zebrasecret"}"#,
        r#"{"route":"search","url":"https://usnewsmap.com/?q=zebrasecret"}"#,
        r#"{"route":"search","filters":{"state":"GA"},"x":"zebrasecret"}"#,
        // Routes are page names, never paths.
        r#"{"route":"/?q=zebrasecret"}"#,
        r#"{"route":"search?q=zebrasecret"}"#,
        r#"{"route":"/"}"#,
        // Referrers are origins, never URLs.
        r#"{"route":"search","referrer_origin":"https://usnewsmap.com/?q=zebrasecret"}"#,
        r#"{"route":"search","referrer_origin":"https://www.google.com/search?q=zebrasecret"}"#,
        // Not an object, or a duplicate field.
        r#"["search","zebrasecret"]"#,
        r#"{"route":"search","route":"datasets"}"#,
        r#"{"route":"search","title":null,"utm_source":{"q":"zebrasecret"}}"#,
        "",
    ];
    for body in refused {
        let (status, problem) = beacon(&state, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(problem["type"], "/errors/bad-beacon", "{body}");
        assert!(!problem.to_string().contains("zebrasecret"), "{problem}");
    }
    page_views.flush().await;
    assert!(
        envelopes(&seen, "PageViewData").is_empty(),
        "{}",
        seen.all_json()
    );
}

#[tokio::test]
async fn size_media_type_and_method_are_checked() {
    let _guard = quiet();
    let state = state(config(), None, Metrics::global()).await;
    // Accepted and dropped without Application Insights.
    assert_eq!(
        beacon(&state, r#"{"route":"search"}"#).await.0,
        StatusCode::NO_CONTENT
    );

    let big = format!(r#"{{"route":"search","title":"{}"}}"#, "x".repeat(3000));
    let (status, problem) = beacon(&state, &big).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(problem["type"], "/errors/too-large");
    // No Content-Length (a chunked body) is cut off at the limit too.
    let chunked = Body::from_stream(futures::stream::iter(
        (0..3).map(|_| Ok::<_, std::io::Error>("x".repeat(1000))),
    ));
    let (status, _) = send(&state, post(&[]).body(chunked).unwrap()).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

    for media in [
        "application/x-www-form-urlencoded",
        "multipart/form-data; boundary=x",
    ] {
        let req = post(&[("content-type", media)])
            .body(Body::from(r#"{"route":"search"}"#))
            .unwrap();
        let (status, problem) = send(&state, req).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{media}");
        assert_eq!(problem["type"], "/errors/unsupported-media-type");
    }
    let req = Request::post("/v1/beacon")
        .body(Body::from(r#"{"route":"search"}"#))
        .unwrap();
    assert_eq!(
        send(&state, req).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );

    let get = Request::get("/v1/beacon").body(Body::empty()).unwrap();
    assert_eq!(send(&state, get).await.0, StatusCode::METHOD_NOT_ALLOWED);
    // Also under the /api/v1 prefix, like every route.
    let req = Request::post("/api/v1/beacon")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"route":"privacy"}"#))
        .unwrap();
    assert_eq!(send(&state, req).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn beacons_share_the_clients_rate_limit() {
    let _guard = quiet();
    let mut cfg = config();
    cfg.rate_limit = Some(RateLimit {
        per_minute: NonZeroU32::new(1).unwrap(),
        burst: NonZeroU32::new(2).unwrap(),
    });
    let state = state(cfg, None, Metrics::global()).await;
    let body = r#"{"route":"search"}"#;
    assert_eq!(beacon(&state, body).await.0, StatusCode::NO_CONTENT);
    assert_eq!(beacon(&state, body).await.0, StatusCode::NO_CONTENT);
    let (status, problem) = beacon(&state, body).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(problem["type"], "/errors/rate-limited");
    // Another client has its own bucket.
    let req = post(&[("x-forwarded-for", "198.51.100.1")])
        .body(Body::from(body))
        .unwrap();
    assert_eq!(send(&state, req).await.0, StatusCode::NO_CONTENT);
}
