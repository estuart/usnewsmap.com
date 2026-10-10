//! Page scans from the Library of Congress, fetched the way
//! `ja-ocr/jaocr.py` fetches them.
//!
//! For each issue, one request to loc.gov's item JSON
//! (`https://www.loc.gov/item/{lccn}/{date}/ed-{n}/?fo=json`) gives every
//! page's JP2 file; its path names the page's IIIF service
//! (`https://tile.loc.gov/image-services/iiif/service:…`), and the scan is
//! that base plus `/full/full/0/default.jpg`. The item JSON is cached (LRU),
//! so the pages of one issue cost one API request. loc.gov's API and its
//! image server are each paced by a gap between requests shared by every
//! task on the node, `Retry-After` is honoured on 429 and 503, and every
//! request names the site in its User-Agent.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use opentelemetry::KeyValue;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use usnm_core::ids::PageKey;

use crate::telemetry::Metrics;

pub const USER_AGENT: &str = concat!(
    "usnewsmap-ocr-worker/",
    env!("CARGO_PKG_VERSION"),
    " (+https://usnewsmap.com)"
);

/// The largest item JSON read.
const MAX_ITEM_BYTES: usize = 16 * 1024 * 1024;
/// The largest scan read (full-size JPEGs are a few MB).
const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// A minimum gap between requests, shared by every task holding it. Each
/// caller reserves the next free slot, so N concurrent callers are spaced
/// `gap` apart, not sent together.
#[derive(Debug)]
pub struct Pacer {
    gap: Duration,
    next: Mutex<Option<Instant>>,
    /// Set by [`Pacer::hold`]: no request before this.
    held: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(gap: Duration) -> Self {
        Self {
            gap,
            next: Mutex::new(None),
            held: Mutex::new(None),
        }
    }

    fn reserve(&self) -> Instant {
        let mut next = self.next.lock().expect("pacer lock");
        let now = Instant::now();
        let at = next.map_or(now, |n| n.max(now));
        *next = Some(at + self.gap);
        at
    }

    /// Wait for this caller's slot. A caller already waiting when a hold
    /// is set checks it on waking and takes a new slot after it.
    pub async fn wait(&self) {
        loop {
            tokio::time::sleep_until(self.reserve()).await;
            let held = *self.held.lock().expect("pacer lock");
            if held.is_none_or(|h| h <= Instant::now()) {
                return;
            }
        }
    }

    /// Hold every caller for at least `d` from now (the server said to wait).
    pub fn hold(&self, d: Duration) {
        let until = Instant::now() + d;
        let mut held = self.held.lock().expect("pacer lock");
        *held = Some(held.map_or(until, |h| h.max(until)));
        let mut next = self.next.lock().expect("pacer lock");
        *next = Some(next.map_or(until, |n| n.max(until)));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The page or issue isn't there, or LoC refused the request: retrying won't help.
    #[error("{0}")]
    Permanent(String),
    #[error("{0}")]
    Transient(String),
}

/// What's fetched for a page.
#[derive(Debug)]
pub struct Scan {
    /// The page's IIIF service base.
    pub iiif_base: String,
    pub jpeg: Bytes,
}

#[derive(Debug, Clone)]
pub struct LocConfig {
    pub api_gap: Duration,
    pub tile_gap: Duration,
    pub attempts: u32,
    /// The wait after a failed attempt, times the attempt number.
    pub backoff: Duration,
    /// `https://www.loc.gov` (a test server in tests).
    pub api_root: String,
    /// Where IIIF bases point (`https://tile.loc.gov/image-services/iiif/`).
    pub iiif_root: String,
}

impl Default for LocConfig {
    fn default() -> Self {
        Self {
            api_gap: Duration::from_millis(3500),
            tile_gap: Duration::from_millis(1000),
            attempts: 5,
            backoff: Duration::from_secs(60),
            api_root: "https://www.loc.gov".into(),
            iiif_root: "https://tile.loc.gov/image-services/iiif/".into(),
        }
    }
}

type Pages = Arc<BTreeMap<u16, String>>;

pub struct LocClient {
    cfg: LocConfig,
    http: reqwest::Client,
    api: Pacer,
    tile: Pacer,
    items: moka::future::Cache<String, Pages>,
    metrics: Metrics,
}

impl std::fmt::Debug for LocClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocClient")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// The IIIF service base for a JP2 URL on LoC's storage service, as
/// `jaocr.py iiif_base` builds it.
pub fn iiif_base(iiif_root: &str, jp2_url: &str) -> Option<String> {
    let path = jp2_url
        .split_once("/storage-services/service/")?
        .1
        .strip_suffix(".jp2")?;
    if path.is_empty()
        || !path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.'))
        || path.contains("..")
    {
        return None;
    }
    Some(format!("{iiif_root}service:{}", path.replace('/', ":")))
}

/// seq → IIIF base from loc.gov's issue JSON. `resources[0].files` is in
/// page (seq) order, which isn't the order of the image file names.
pub fn page_images(iiif_root: &str, item: &Value) -> BTreeMap<u16, String> {
    let files = item["resources"][0]["files"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut out = BTreeMap::new();
    for (i, page) in files.iter().enumerate() {
        let Ok(seq) = u16::try_from(i + 1) else { break };
        let jp2 = page.as_array().and_then(|fs| {
            fs.iter()
                .find(|f| f["mimetype"] == "image/jp2")
                .and_then(|f| f["url"].as_str())
        });
        if let Some(base) = jp2.and_then(|u| iiif_base(iiif_root, u)) {
            out.insert(seq, base);
        }
    }
    out
}

/// A `Retry-After` value: seconds or an HTTP date.
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    let d = match v.parse::<u64>() {
        Ok(s) => Duration::from_secs(s),
        Err(_) => httpdate::parse_http_date(v)
            .ok()?
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default(),
    };
    Some(d.min(MAX_RETRY_AFTER))
}

impl LocClient {
    pub fn new(cfg: LocConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(180))
            .build()?;
        Ok(Self {
            api: Pacer::new(cfg.api_gap),
            tile: Pacer::new(cfg.tile_gap),
            items: moka::future::Cache::builder()
                .max_capacity(256)
                .time_to_live(Duration::from_secs(3600))
                .build(),
            cfg,
            http,
            metrics: Metrics::global(),
        })
    }

    /// GET `url`, paced by `pacer`, retrying transient failures.
    async fn get(&self, url: &str, pacer: &Pacer, max: usize) -> Result<Bytes, FetchError> {
        let mut last = String::new();
        for attempt in 1..=self.cfg.attempts.max(1) {
            pacer.wait().await;
            let wait = self.cfg.backoff * attempt;
            match self.http.get(url).send().await {
                Ok(resp) if resp.status().is_success() => match read_limited(resp, max).await {
                    Ok(b) => return Ok(b),
                    Err(e) => last = e,
                },
                Ok(resp) => {
                    let status = resp.status();
                    if matches!(
                        status,
                        StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                    ) {
                        let asked = retry_after(&resp);
                        let d = asked.map_or(wait, |r| r.max(wait));
                        // Everyone waits, not just this request.
                        pacer.hold(d);
                        let server = if std::ptr::eq(pacer, &self.api) {
                            "api"
                        } else {
                            "tile"
                        };
                        self.metrics.loc_slowdowns.add(
                            1,
                            &[
                                KeyValue::new("status", i64::from(status.as_u16())),
                                KeyValue::new("server", server),
                                KeyValue::new("retry_after", asked.is_some()),
                            ],
                        );
                        tracing::warn!(
                            status = status.as_u16(),
                            wait_secs = d.as_secs(),
                            "loc.gov asked us to slow down"
                        );
                        last = format!("HTTP {}", status.as_u16());
                        continue;
                    }
                    // 408 is a timeout, worth retrying like a 5xx.
                    if status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT {
                        return Err(FetchError::Permanent(format!(
                            "{url} returned HTTP {}",
                            status.as_u16()
                        )));
                    }
                    last = format!("HTTP {}", status.as_u16());
                }
                Err(e) => last = e.without_url().to_string(),
            }
            if attempt < self.cfg.attempts {
                tracing::warn!(url, attempt, error = %last, wait_secs = wait.as_secs(), "retrying");
                tokio::time::sleep(wait).await;
            }
        }
        Err(FetchError::Transient(format!("giving up on {url}: {last}")))
    }

    /// seq → IIIF base for `key`'s issue, from the cache or one API request.
    /// An issue loc.gov doesn't have is cached as no pages.
    async fn issue_pages(&self, key: &PageKey) -> Result<Pages, FetchError> {
        let issue = format!(
            "{}/{}/ed-{}",
            key.lccn,
            key.date.format("%Y-%m-%d"),
            key.edition
        );
        let url = format!("{}/item/{issue}/?fo=json", self.cfg.api_root);
        self.items
            .try_get_with(issue, async {
                match self.get(&url, &self.api, MAX_ITEM_BYTES).await {
                    Ok(body) => {
                        let item: Value = serde_json::from_slice(&body).map_err(|e| {
                            FetchError::Transient(format!("item JSON for {url}: {e}"))
                        })?;
                        Ok(Arc::new(page_images(&self.cfg.iiif_root, &item)))
                    }
                    Err(FetchError::Permanent(_)) => Ok(Arc::new(BTreeMap::new())),
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|e: Arc<FetchError>| match &*e {
                FetchError::Permanent(m) => FetchError::Permanent(m.clone()),
                FetchError::Transient(m) => FetchError::Transient(m.clone()),
            })
    }

    /// The page's full-size JPEG.
    pub async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError> {
        let pages = self.issue_pages(key).await?;
        let base = pages
            .get(&key.seq)
            .cloned()
            .ok_or_else(|| FetchError::Permanent(format!("loc.gov lists no image for {key}")))?;
        let jpeg = self
            .get(
                &format!("{base}/full/full/0/default.jpg"),
                &self.tile,
                MAX_IMAGE_BYTES,
            )
            .await?;
        Ok(Scan {
            iiif_base: base,
            jpeg,
        })
    }
}

async fn read_limited(mut resp: reqwest::Response, max: usize) -> Result<Bytes, String> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(format!("response over {max} bytes"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        if body.len() + chunk.len() > max {
            return Err(format!("response over {max} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn the_pacer_spaces_concurrent_callers() {
        let pacer = Arc::new(Pacer::new(Duration::from_millis(3500)));
        let t0 = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let p = pacer.clone();
            tasks.push(tokio::spawn(async move {
                p.wait().await;
                Instant::now() - t0
            }));
        }
        let mut at: Vec<Duration> = Vec::new();
        for t in tasks {
            at.push(t.await.unwrap());
        }
        at.sort();
        assert_eq!(
            at,
            [0, 3500, 7000, 10500].map(Duration::from_millis).to_vec()
        );
        // A caller already waiting when a hold is set waits it out too.
        let p = pacer.clone();
        let t2 = Instant::now();
        let waiting = tokio::spawn(async move {
            p.wait().await;
            Instant::now()
        });
        tokio::task::yield_now().await;
        pacer.hold(Duration::from_secs(30));
        assert!(waiting.await.unwrap() - t2 >= Duration::from_secs(30));
        // Told to hold: the next caller waits it out.
        pacer.hold(Duration::from_secs(60));
        let t1 = Instant::now();
        pacer.wait().await;
        assert_eq!(Instant::now() - t1, Duration::from_secs(60));
    }

    #[test]
    fn builds_iiif_bases_from_the_item_json() {
        let item = serde_json::json!({"resources": [{"files": [
            [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/nbu/batch_nbu_gage_ver01/data/sn83045462/00271743700/1890050301/0365.jp2"},
             {"mimetype": "text/xml", "url": "https://tile.loc.gov/x.xml"}],
            [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/nbu/batch_nbu_gage_ver01/data/sn83045462/00271743700/1890050301/0364.jp2"}],
            [{"mimetype": "text/xml", "url": "https://tile.loc.gov/y.xml"}]
        ]}]});
        let pages = page_images("https://tile.loc.gov/image-services/iiif/", &item);
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[&1],
            "https://tile.loc.gov/image-services/iiif/service:ndnp:nbu:batch_nbu_gage_ver01:data:sn83045462:00271743700:1890050301:0365"
        );
        assert!(pages[&2].ends_with(":0364"));
        assert_eq!(iiif_base("r/", "https://elsewhere/x.jp2"), None);
        assert_eq!(
            iiif_base(
                "r/",
                "https://tile.loc.gov/storage-services/service/a/../b.jp2"
            ),
            None
        );
    }

    /// A stand-in loc.gov: one issue (two pages), counting item requests;
    /// the first image request is answered 429 with Retry-After, the
    /// second 408.
    async fn fake_loc(items: Arc<AtomicU32>, images: Arc<AtomicU32>) -> String {
        use axum::http::StatusCode as S;
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let jp2 = |n: &str| {
            format!("https://tile.loc.gov/storage-services/service/ndnp/x/batch_x/data/sn1/1/1890050301/{n}.jp2")
        };
        let item = serde_json::json!({"resources": [{"files": [
            [{"mimetype": "image/jp2", "url": jp2("0001")}],
            [{"mimetype": "image/jp2", "url": jp2("0002")}],
        ]}]});
        let app = axum::Router::new()
            .route(
                "/item/sn1/1890-05-03/ed-1/",
                axum::routing::get(move |headers: axum::http::HeaderMap| {
                    let item = item.clone();
                    let items = items.clone();
                    async move {
                        assert!(headers["user-agent"]
                            .to_str()
                            .unwrap()
                            .starts_with("usnewsmap-ocr-worker/"));
                        items.fetch_add(1, Ordering::SeqCst);
                        axum::Json(item)
                    }
                }),
            )
            .route(
                "/iiif/{service}/full/full/0/default.jpg",
                axum::routing::get(move || {
                    let images = images.clone();
                    async move {
                        match images.fetch_add(1, Ordering::SeqCst) {
                            0 => (S::TOO_MANY_REQUESTS, [("retry-after", "2")]).into_response(),
                            1 => S::REQUEST_TIMEOUT.into_response(),
                            _ => b"jpeg".to_vec().into_response(),
                        }
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        root
    }

    #[tokio::test]
    async fn one_item_request_per_issue_and_retry_after_is_honoured() {
        let (items, images) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let root = fake_loc(items.clone(), images.clone()).await;
        let loc = LocClient::new(LocConfig {
            api_gap: Duration::from_millis(10),
            tile_gap: Duration::from_millis(10),
            attempts: 3,
            backoff: Duration::from_millis(10),
            api_root: root.clone(),
            iiif_root: format!("{root}/iiif/"),
        })
        .unwrap();
        let t0 = std::time::Instant::now();
        let p1: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        let p2: PageKey = "sn1/1890-05-03/ed-1/seq-2".parse().unwrap();
        let s1 = loc.scan(&p1).await.unwrap();
        assert!(
            t0.elapsed() >= Duration::from_secs(2),
            "waited out Retry-After"
        );
        // The 408 was retried, not taken as permanent.
        assert_eq!(&s1.jpeg[..], b"jpeg");
        assert_eq!(images.load(Ordering::SeqCst), 3);
        assert!(s1
            .iiif_base
            .ends_with("service:ndnp:x:batch_x:data:sn1:1:1890050301:0001"));
        loc.scan(&p2).await.unwrap();
        assert_eq!(items.load(Ordering::SeqCst), 1);
        let p3: PageKey = "sn1/1890-05-03/ed-1/seq-3".parse().unwrap();
        assert!(matches!(loc.scan(&p3).await, Err(FetchError::Permanent(_))));
        // An issue loc.gov doesn't have: permanent, and asked once.
        let other: PageKey = "sn1/1890-05-04/ed-1/seq-1".parse().unwrap();
        assert!(matches!(
            loc.scan(&other).await,
            Err(FetchError::Permanent(_))
        ));
        assert!(matches!(
            loc.scan(&other).await,
            Err(FetchError::Permanent(_))
        ));
    }
}
