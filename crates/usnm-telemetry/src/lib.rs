//! Logs, traces and metrics for `usnm-api`, `usnm-ingest` (08 §8.1.2,
//! 09 §9.2) and the on-prem OCR worker (ADR-0015).
//!
//! Logs are JSON lines on stdout, which Container Apps sends to Log Analytics
//! (`ContainerAppConsoleLogs`). When `APPLICATIONINSIGHTS_CONNECTION_STRING`
//! is set, spans and metrics also go to Application Insights through
//! OpenTelemetry. The resource has local auth disabled (ADR-0009), so every
//! upload carries an Entra token for the service's managed identity
//! (Monitoring Metrics Publisher on the component; on an Arc-enabled node,
//! the machine's identity through HIMDS); the connection string
//! only names the ingestion endpoint and the instrumentation key. Without it
//! (local runs, CI) nothing is exported.
//!
//! The Container Apps managed OpenTelemetry agent can't authenticate with
//! Entra, so each service exports directly.
//!
//! The API also forwards the web app's page views ([`pageviews`]) through
//! the same client, since a browser can't get an Entra token for ingestion.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::trace::{SdkTracerProvider, Tracer};
use opentelemetry_sdk::Resource;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use usnm_store::credential::{self, Credential};

pub mod pageviews;
#[cfg(feature = "testing")]
pub mod testing;

pub use pageviews::{PageView, PageViews};

pub const CONNECTION_STRING_VAR: &str = "APPLICATIONINSIGHTS_CONNECTION_STRING";

/// The cloud role of the web app's page views, which the API forwards.
pub const WEB_ROLE: &str = "usnm-web";

/// How often metrics are exported (and once more at shutdown).
const METRICS_INTERVAL: Duration = Duration::from_secs(30);

/// How long shutdown waits in all. The SDK gives each provider 5 s to
/// export what it holds; an upload still retrying after that is dropped.
const SHUTDOWN_WAIT: Duration = Duration::from_secs(30);

/// Which program is reporting. `name` becomes `service.name`, which
/// Application Insights shows as the cloud role (`AppRoleName`).
#[derive(Debug, Clone, Copy)]
pub struct Service {
    pub name: &'static str,
    pub version: &'static str,
}

/// The exporters, if telemetry is on. Call [`Telemetry::shutdown`] before the
/// process exits: whatever is still buffered is lost otherwise.
#[derive(Default)]
pub struct Telemetry {
    tracer: Option<SdkTracerProvider>,
    meter: Option<SdkMeterProvider>,
    page_views: Option<PageViews>,
}

impl Telemetry {
    pub fn is_on(&self) -> bool {
        self.tracer.is_some() || self.meter.is_some()
    }

    /// The uploader for the web app's page views, if telemetry is on.
    pub fn page_views(&self) -> Option<PageViews> {
        self.page_views.clone()
    }

    /// Flush and stop the exporters.
    pub async fn shutdown(self) {
        if !self.is_on() {
            return;
        }
        let Self {
            tracer,
            meter,
            page_views,
        } = self;
        if let Some(p) = page_views {
            p.flush().await;
        }
        // The SDK's shutdown blocks until the export thread is done, and the
        // export itself runs on this runtime (EntraClient), so block elsewhere.
        let stop = tokio::task::spawn_blocking(move || {
            let traces = tracer.map(|p| p.shutdown());
            let metrics = meter.map(|p| p.shutdown());
            (traces, metrics)
        });
        match tokio::time::timeout(SHUTDOWN_WAIT, stop).await {
            Ok(Ok((traces, metrics))) => {
                for (what, r) in [("traces", traces), ("metrics", metrics)] {
                    if let Some(Err(e)) = r {
                        tracing::warn!(target: "telemetry", error = %e, "flushing {what} failed");
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(target: "telemetry", error = %e, "telemetry shutdown failed")
            }
            Err(_) => tracing::warn!(target: "telemetry", "telemetry shutdown timed out"),
        }
    }
}

/// Choices beyond the service name, for [`init_with`].
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The cloud role instance (`service.instance.id`), e.g. a node's name.
    /// Unset, Container Apps' replica name is used if there is one.
    pub instance: Option<String>,
    /// Start the uploader for the web app's page views (the API's).
    pub page_views: bool,
}

/// The resource Entra tokens for ingestion are requested for (scope
/// `https://monitor.azure.com//.default`).
pub const TOKEN_RESOURCE: &str = credential::MONITOR_RESOURCE;

/// Install the log subscriber (JSON on stdout, `RUST_LOG`, default `info`)
/// and, if the connection string is set, the Application Insights exporters,
/// with the meter provider as the global one. Must be called from inside the
/// Tokio runtime, before anything records a metric.
pub fn init(service: Service) -> Telemetry {
    init_with(
        service,
        Options {
            page_views: true,
            ..Options::default()
        },
    )
}

/// [`init`], with [`Options`].
pub fn init_with(service: Service, options: Options) -> Telemetry {
    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = || tracing_subscriber::fmt::layer().json();
    let conn = std::env::var(CONNECTION_STRING_VAR)
        .ok()
        .filter(|v| !v.trim().is_empty());
    let Some(conn) = conn else {
        tracing_subscriber::registry()
            .with(filter())
            .with(json())
            .init();
        return Telemetry::default();
    };
    let client = EntraClient::new(
        credential::from_env_for(TOKEN_RESOURCE),
        tokio::runtime::Handle::current(),
    );
    match providers_for(&conn, client.clone(), service, options.instance.as_deref()) {
        Ok((tracer, meter)) => {
            opentelemetry::global::set_meter_provider(meter.clone());
            tracing_subscriber::registry()
                .with(filter())
                .with(json())
                .with(otel_layer(&tracer, service))
                .init();
            tracing::info!(target: "telemetry", "exporting traces and metrics to Application Insights");
            let sdk = format!("{}:{}", service.name, service.version);
            let page_views = options
                .page_views
                .then(|| {
                    PageViews::start(&conn, client, WEB_ROLE, &sdk)
                        .inspect_err(|e| tracing::warn!(target: "telemetry", error = %e, "page views are off"))
                        .ok()
                })
                .flatten();
            Telemetry {
                tracer: Some(tracer),
                meter: Some(meter),
                page_views,
            }
        }
        // Telemetry never stops the service: log and carry on without it.
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter())
                .with(json())
                .init();
            tracing::warn!(target: "telemetry", error = %e, "Application Insights connection string not usable; telemetry is off");
            Telemetry::default()
        }
    }
}

/// The `tracing` layer that turns spans (and the events inside them) into
/// OpenTelemetry spans for `provider`.
pub fn otel_layer<S>(
    provider: &SdkTracerProvider,
    service: Service,
) -> OpenTelemetryLayer<S, Tracer>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
{
    tracing_opentelemetry::layer().with_tracer(provider.tracer(service.name))
}

/// The trace and meter providers exporting to the Application Insights
/// resource named by `connection_string`, through `client`.
pub fn providers<C>(
    connection_string: &str,
    client: C,
    service: Service,
) -> anyhow::Result<(SdkTracerProvider, SdkMeterProvider)>
where
    C: HttpClient + Clone + 'static,
{
    providers_for(connection_string, client, service, None)
}

/// [`providers`], reporting as cloud role instance `instance` if given.
pub fn providers_for<C>(
    connection_string: &str,
    client: C,
    service: Service,
    instance: Option<&str>,
) -> anyhow::Result<(SdkTracerProvider, SdkMeterProvider)>
where
    C: HttpClient + Clone + 'static,
{
    let exporter = opentelemetry_application_insights::Exporter::new_from_connection_string(
        connection_string,
        client,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?
    .with_retry_notify(|e, wait| {
        tracing::warn!(target: "telemetry", error = %e, wait_ms = wait.as_millis() as u64, "Application Insights upload failed; retrying");
    });
    let resource = resource(service, instance);
    let tracer = SdkTracerProvider::builder()
        .with_batch_exporter(exporter.clone())
        .with_resource(resource.clone())
        .build();
    let reader = PeriodicReader::builder(exporter)
        .with_interval(METRICS_INTERVAL)
        .build();
    let meter = SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .build();
    Ok((tracer, meter))
}

/// `service.name` becomes the cloud role; `instance` (or the replica, set
/// by Container Apps), the revision and the job execution tell instances
/// and runs apart.
fn resource(service: Service, instance: Option<&str>) -> Resource {
    let mut attrs = vec![KeyValue::new("service.version", service.version)];
    if let Some(i) = instance {
        attrs.push(KeyValue::new("service.instance.id", i.to_owned()));
    }
    for (var, key) in [
        ("CONTAINER_APP_REPLICA_NAME", "service.instance.id"),
        ("CONTAINER_APP_REVISION", "usnm.revision"),
        ("CONTAINER_APP_JOB_NAME", "usnm.job"),
        ("CONTAINER_APP_JOB_EXECUTION_NAME", "usnm.job_execution"),
    ] {
        if instance.is_some() && key == "service.instance.id" {
            continue;
        }
        if let Some(v) = std::env::var(var).ok().filter(|v| !v.is_empty()) {
            attrs.push(KeyValue::new(key, v));
        }
    }
    Resource::builder()
        .with_service_name(service.name)
        .with_attributes(attrs)
        .build()
}

/// An [`HttpClient`] for the Application Insights exporter that signs every
/// upload with an Entra token (`Authorization: Bearer`) and sends it to
/// `/v2.1/track`, the ingestion path that accepts Entra tokens (`/v2/track`
/// answers 400 on a resource with local auth disabled).
///
/// The SDK exports from its own threads, outside Tokio; each request is
/// spawned onto `runtime`, where reqwest and the credential run.
#[derive(Clone)]
pub struct EntraClient {
    http: reqwest::Client,
    credential: Arc<Credential>,
    runtime: tokio::runtime::Handle,
}

impl fmt::Debug for EntraClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntraClient")
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl EntraClient {
    pub fn new(credential: Arc<Credential>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("static client config"),
            credential,
            runtime,
        }
    }

    async fn send(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let token = self.credential.token().await?;
        let mut req: reqwest::Request = request.try_into()?;
        let path = entra_path(req.url().path());
        req.url_mut().set_path(&path);
        let mut auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))?;
        auth.set_sensitive(true);
        req.headers_mut()
            .insert(reqwest::header::AUTHORIZATION, auth);
        let resp = self.http.execute(req).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.bytes().await?;
        // The exporter reports failures only through the SDK's own logs;
        // say plainly why Application Insights refused an upload.
        if !status.is_success() && status.as_u16() != 206 {
            tracing::warn!(
                target: "telemetry",
                status = status.as_u16(),
                body = %String::from_utf8_lossy(&body).chars().take(300).collect::<String>(),
                "Application Insights refused telemetry"
            );
        }
        let mut out = Response::builder().status(status).body(body)?;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

#[async_trait]
impl HttpClient for EntraClient {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        let this = self.clone();
        self.runtime
            .spawn(async move { this.send(request).await })
            .await?
    }
}

/// `…/v2/track` → `…/v2.1/track`; other paths unchanged.
fn entra_path(path: &str) -> String {
    match path.strip_suffix("/v2/track") {
        Some(prefix) => format!("{prefix}/v2.1/track"),
        None => path.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use usnm_store::credential::StaticToken;

    #[test]
    fn tokens_are_for_azure_monitor() {
        assert_eq!(TOKEN_RESOURCE, "https://monitor.azure.com/");
    }

    #[test]
    fn the_instance_names_the_role_instance() {
        let r = resource(
            Service {
                name: "usnm-ocr-worker",
                version: "0",
            },
            Some("nuc1"),
        );
        let get = |k: &'static str| r.get(&opentelemetry::Key::from_static_str(k));
        assert_eq!(get("service.name").unwrap().as_str(), "usnm-ocr-worker");
        assert_eq!(get("service.instance.id").unwrap().as_str(), "nuc1");
    }

    #[test]
    fn entra_uploads_go_to_v2_1() {
        assert_eq!(entra_path("/v2/track"), "/v2.1/track");
        assert_eq!(entra_path("/prefix/v2/track"), "/prefix/v2.1/track");
        assert_eq!(entra_path("/v2.1/track"), "/v2.1/track");
        assert_eq!(entra_path("/other"), "/other");
    }

    #[tokio::test]
    async fn uploads_carry_the_bearer_token() {
        type Seen = Arc<Mutex<Vec<(String, Option<String>, Vec<u8>)>>>;
        let seen: Seen = Arc::default();
        let app = axum::Router::new().fallback({
            let seen = seen.clone();
            move |req: axum::extract::Request| async move {
                let path = req.uri().path().to_owned();
                let auth = req
                    .headers()
                    .get("authorization")
                    .map(|v| v.to_str().unwrap().to_owned());
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                seen.lock().unwrap().push((path, auth, body.to_vec()));
                r#"{"itemsReceived":1,"itemsAccepted":1,"errors":[]}"#
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = EntraClient::new(
            Arc::new(Credential::new(StaticToken("tok".into()))),
            tokio::runtime::Handle::current(),
        );
        let req = Request::post(format!("http://{addr}/v2/track"))
            .header("content-type", "application/json")
            .body(Bytes::from_static(b"[]"))
            .unwrap();
        let resp = client.send_bytes(req).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert!(resp.body().starts_with(b"{\"itemsReceived\""));
        let seen = seen.lock().unwrap();
        assert_eq!(
            *seen,
            [(
                "/v2.1/track".to_owned(),
                Some("Bearer tok".to_owned()),
                b"[]".to_vec()
            )]
        );
    }
}
