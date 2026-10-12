//! `usnm-ocrdash`: a read-only dashboard of the on-prem OCR pipeline's
//! local side (12 §12.2.5), for the LAN only. It reads the scan fetcher's
//! files, polls each worker's status, and takes the Proxmox hosts' stats,
//! and serves one HTML page and the same data as JSON. It never shows page
//! text: pages appear by doc_id, with counts, sizes, times and engine tags.

pub mod fetch;
pub mod hosts;
pub mod html;
pub mod net;
pub mod nodes;
pub mod sset;
pub mod tail;
pub mod view;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Request, State};
use axum::http::{header, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use chrono::Utc;

use crate::fetch::Fetch;
use crate::hosts::Hosts;
use crate::net::Allow;
use crate::nodes::Nodes;
use crate::sset::Ssets;
use crate::view::{Settings, View};

pub struct App {
    pub fetch: Mutex<Fetch>,
    pub nodes: Mutex<Nodes>,
    pub hosts: Mutex<Hosts>,
    pub ssets: Mutex<Ssets>,
    pub seed_plan_path: Option<PathBuf>,
    pub seed_plan: Mutex<Option<BTreeMap<String, String>>>,
    pub settings: Settings,
    /// Who may ask anything.
    pub allow: Allow,
    /// Host → the addresses that may push its stats (its own).
    pub pushers: BTreeMap<String, Allow>,
    /// Serial Set fetch → the addresses that may push its status (the
    /// node that runs it).
    pub sset_pushers: BTreeMap<String, Allow>,
}

impl App {
    /// Read the files: the fetcher's, the hosts', the seed plan. Blocking.
    pub fn poll_files(&self) {
        let now = Utc::now();
        self.fetch.lock().expect("fetch lock").poll(now);
        self.hosts.lock().expect("hosts lock").poll(now);
        if let Some(p) = &self.seed_plan_path {
            let plan = match std::fs::read(p) {
                Ok(b) => match view::read_seed_plan(&b) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "can't read the seed plan");
                        None
                    }
                },
                Err(_) => None,
            };
            *self.seed_plan.lock().expect("seed lock") = plan;
        }
    }

    /// Ask every node, at once.
    pub async fn poll_nodes(self: &Arc<Self>, client: &reqwest::Client) {
        let targets: Vec<(usize, String, u64, String)> = {
            let n = self.nodes.lock().expect("nodes lock");
            n.nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (i, n.url.clone(), n.offset, n.journal_id.clone()))
                .collect()
        };
        let mut set = tokio::task::JoinSet::new();
        for (i, url, offset, id) in targets {
            let client = client.clone();
            set.spawn(async move { (i, Nodes::poll(&client, &url, offset, &id).await) });
        }
        while let Some(r) = set.join_next().await {
            if let Ok((i, r)) = r {
                self.nodes
                    .lock()
                    .expect("nodes lock")
                    .apply(i, r, Utc::now());
            }
        }
    }

    pub fn view(&self) -> View {
        let mut fetch = self.fetch.lock().expect("fetch lock");
        let nodes = self.nodes.lock().expect("nodes lock");
        let hosts = self.hosts.lock().expect("hosts lock");
        let ssets = self.ssets.lock().expect("ssets lock");
        let seed = self.seed_plan.lock().expect("seed lock").clone();
        view::build(
            &mut fetch,
            &nodes,
            &hosts,
            &ssets,
            seed,
            &self.settings,
            Utc::now(),
        )
    }
}

async fn allow_only(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let ip = peer.ip().to_canonical();
    if app.allow.contains(ip) {
        next.run(req).await
    } else {
        tracing::info!(peer = %ip, path = req.uri().path(), "refused: not on --allow");
        StatusCode::FORBIDDEN.into_response()
    }
}

/// Runs the blocking view work off the async threads.
async fn view_of(app: Arc<App>) -> Result<View, StatusCode> {
    tokio::task::spawn_blocking(move || app.view())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn page(State(app): State<Arc<App>>) -> Response {
    match view_of(app).await {
        Ok(v) => (
            [(header::CACHE_CONTROL, "no-store")],
            Html(html::render(&v)),
        )
            .into_response(),
        Err(s) => s.into_response(),
    }
}

async fn api(State(app): State<Arc<App>>, Path(part): Path<String>) -> Response {
    let Some(part) = part.strip_suffix(".json") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let v = match view_of(app).await {
        Ok(v) => v,
        Err(s) => return s.into_response(),
    };
    let body = match part {
        "view" => serde_json::to_value(&v),
        "hosts" => serde_json::to_value(&v.hosts),
        "fetch" => serde_json::to_value(&v.fetch),
        "nodes" => serde_json::to_value(&v.nodes),
        "serialset" => serde_json::to_value(&v.serial_set),
        "next" => serde_json::to_value(&v.next),
        "ocred" => serde_json::to_value(&v.ocred),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match body {
        Ok(b) => ([(header::CACHE_CONTROL, "no-store")], Json(b)).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// A host pushes its stats: only from its own address, and only its own.
async fn put_host(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let ip = peer.ip().to_canonical();
    if !app.pushers.get(&name).is_some_and(|a| a.contains(ip)) {
        tracing::info!(peer = %ip, host = name, "refused a host push");
        return StatusCode::FORBIDDEN.into_response();
    }
    match hosts::parse(&body, Some(&name)) {
        Ok(s) => {
            app.hosts.lock().expect("hosts lock").put(s, Utc::now());
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}\n")).into_response(),
    }
}

/// The node running the Serial Set fetch pushes its progress: only from
/// its own address.
async fn put_sset(
    State(app): State<Arc<App>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let ip = peer.ip().to_canonical();
    if !app.sset_pushers.get(&name).is_some_and(|a| a.contains(ip)) {
        tracing::info!(peer = %ip, sset = name, "refused a Serial Set push");
        return StatusCode::FORBIDDEN.into_response();
    }
    match sset::parse(&body) {
        Ok(s) => {
            app.ssets
                .lock()
                .expect("ssets lock")
                .put(&name, s, Utc::now());
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, format!("{e:#}\n")).into_response(),
    }
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/api/{part}", get(api))
        .route("/api/host/{name}", put(put_host))
        .route("/api/sset/{name}", put(put_sset))
        .layer(DefaultBodyLimit::max(hosts::MAX_HOST_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), allow_only))
        .with_state(app)
}

/// Plain HTTP when the page itself is served over HTTPS: every request is
/// redirected to `https://<authority>` (the name, and the port unless 443)
/// with the same path and query, except
/// the hosts' stats pushes (`PUT /api/host/{name}`) and the Serial Set
/// status push (`PUT /api/sset/{name}`), which carry no secret,
/// are checked by address, and come from `curl` that doesn't follow
/// redirects. The same `--allow` as the page.
pub fn redirect_router(app: Arc<App>, authority: &str) -> Router {
    let base = format!("https://{authority}");
    let redirect = move |uri: Uri| async move {
        let path = uri.path_and_query().map_or("/", |p| p.as_str());
        Redirect::permanent(&format!("{base}{path}"))
    };
    Router::new()
        // Any other method on the push path is redirected too, not a 405.
        .route("/api/host/{name}", put(put_host).fallback(redirect.clone()))
        .route("/api/sset/{name}", put(put_sset).fallback(redirect.clone()))
        .fallback(redirect)
        .layer(DefaultBodyLimit::max(hosts::MAX_HOST_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), allow_only))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn app(dir: &std::path::Path) -> Arc<App> {
        Arc::new(App {
            fetch: Mutex::new(Fetch::new(dir.to_owned(), None)),
            nodes: Mutex::new(Nodes::open(dir.join("state"), &[]).unwrap()),
            hosts: Mutex::new(Hosts::new(Vec::new())),
            ssets: Mutex::new(Ssets::default()),
            seed_plan_path: None,
            seed_plan: Mutex::new(None),
            settings: Settings {
                next_n: 10,
                refresh_secs: 30,
                guest_prefix: "usnm-".into(),
                workbook_url: None,
            },
            allow: Allow::new(vec!["198.51.100.0/24".parse().unwrap()]),
            pushers: [(
                "pve".to_owned(),
                Allow::new(vec!["198.51.100.12".parse().unwrap()]),
            )]
            .into(),
            sset_pushers: [(
                "sset".to_owned(),
                Allow::new(vec!["198.51.100.13".parse().unwrap()]),
            )]
            .into(),
        })
    }

    async fn call(
        app: Arc<App>,
        from: &str,
        method: &str,
        uri: &str,
        body: &str,
    ) -> (StatusCode, String) {
        call_on(router(app), from, method, uri, body).await.0
    }

    async fn call_on(
        r: Router,
        from: &str,
        method: &str,
        uri: &str,
        body: &str,
    ) -> ((StatusCode, String), Option<String>) {
        let peer: SocketAddr = format!("{from}:40000").parse().unwrap();
        let r = r
            .layer(MockConnectInfo(peer))
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::from(body.to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let location = r
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        ((status, String::from_utf8_lossy(&b).into_owned()), location)
    }

    #[tokio::test]
    async fn plain_http_redirects_to_https_but_takes_host_pushes() {
        let dir = tempfile::tempdir().unwrap();
        let a = app(dir.path());
        let plain = || redirect_router(a.clone(), "ocr.example.org");
        let ((s, _), to) = call_on(plain(), "198.51.100.50", "GET", "/", "").await;
        assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(to.as_deref(), Some("https://ocr.example.org/"));
        let ((s, _), to) =
            call_on(plain(), "198.51.100.50", "GET", "/api/fetch.json?x=1", "").await;
        assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            to.as_deref(),
            Some("https://ocr.example.org/api/fetch.json?x=1")
        );
        let ((s, _), to) = call_on(plain(), "198.51.100.50", "GET", "/api/host/pve", "").await;
        assert_eq!(s, StatusCode::PERMANENT_REDIRECT);
        assert_eq!(to.as_deref(), Some("https://ocr.example.org/api/host/pve"));
        let ((s, _), _) = call_on(plain(), "203.0.113.3", "GET", "/", "").await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let stats = r#"{"schema":1,"host":"pve","pools":[]}"#;
        let ((s, _), _) = call_on(plain(), "198.51.100.12", "PUT", "/api/host/pve", stats).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let ((s, _), _) = call_on(plain(), "198.51.100.50", "PUT", "/api/host/pve", stats).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn serves_the_lan_only_and_takes_a_host_push_from_that_host_only() {
        let dir = tempfile::tempdir().unwrap();
        let a = app(dir.path());
        let (s, body) = call(a.clone(), "198.51.100.50", "GET", "/", "").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("Scan downloads"));
        assert_eq!(
            call(a.clone(), "203.0.113.3", "GET", "/", "").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(a.clone(), "198.51.100.50", "GET", "/api/fetch.json", "")
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            call(a.clone(), "198.51.100.50", "GET", "/api/other.json", "")
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let stats = r#"{"schema":1,"host":"pve","pools":[{"name":"<script>","health":"ONLINE"}]}"#;
        assert_eq!(
            call(a.clone(), "198.51.100.50", "PUT", "/api/host/pve", stats)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(a.clone(), "198.51.100.12", "PUT", "/api/host/pve", stats)
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                a.clone(),
                "198.51.100.12",
                "PUT",
                "/api/host/pve",
                r#"{"host":"pve-big"}"#
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let (_, body) = call(a.clone(), "198.51.100.50", "GET", "/", "").await;
        assert!(body.contains("&lt;script&gt;"));
        assert!(!body.contains("<code><script>"));
        let (_, json) = call(a.clone(), "198.51.100.50", "GET", "/api/hosts.json", "").await;
        assert!(json.contains("\"name\":\"pve\""));
        let sset = r#"{"unit":"usnm-sset.service","active":"active","to_fetch":900,"curated":120}"#;
        let put_sset =
            |from: &'static str, name: &'static str| call(a.clone(), from, "PUT", name, sset);
        assert_eq!(
            put_sset("198.51.100.50", "/api/sset/sset").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            put_sset("198.51.100.13", "/api/sset/other").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            put_sset("198.51.100.13", "/api/sset/sset").await.0,
            StatusCode::NO_CONTENT
        );
        let (_, json) = call(a.clone(), "198.51.100.50", "GET", "/api/serialset.json", "").await;
        assert!(json.contains("\"curated\":120"));
        let (_, page) = call(a, "198.51.100.50", "GET", "/", "").await;
        assert!(page.contains("Serial Set"));
    }
}
