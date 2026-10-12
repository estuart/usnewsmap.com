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
//! task on the node, `Retry-After` is honoured on 429 and 503, a 403 holds
//! the server the same way (a block isn't the page's fault), and every
//! request names the site in its User-Agent.
//!
//! **Item errors** (#414). Some items answer 503 at once, for hours, while
//! the rest of loc.gov answers normally: a fault in that item on LoC's
//! side, not a hold. An item JSON 503 counts as an item error, with
//! nothing held, only when all of these are true:
//! - it came back within [`ITEM_ERROR_FAST`], with no `Retry-After`;
//! - another item answered 2xx within [`ITEM_ERROR_OK_WITHIN`], or one
//!   paced request for the last item that answered 2xx (the *known-good*
//!   item) answers 2xx now;
//! - fewer than [`ITEM_ERROR_TITLES`] different titles, this one included,
//!   had item errors in the last [`ITEM_ERROR_WINDOW`].
//!
//! Anything else (a 429, a 403, a `Retry-After`, a slow 503, a 503 on the
//! image server, a failed check, or item errors spread over several titles)
//! holds the server as before. When unsure, it holds.

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
pub const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// An item JSON 503 slower than this is never an item error.
pub const ITEM_ERROR_FAST: Duration = Duration::from_secs(2);
/// Another item's 2xx this recent shows loc.gov is serving.
pub const ITEM_ERROR_OK_WITHIN: Duration = Duration::from_secs(60);
/// Item errors are remembered this long, by title.
pub const ITEM_ERROR_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Item errors from this many titles inside the window look like loc.gov,
/// not the items: the next one holds the server.
pub const ITEM_ERROR_TITLES: usize = 3;

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

    /// Until when callers are held, if a hold is still running.
    pub fn held_until(&self) -> Option<Instant> {
        let held = *self.held.lock().expect("pacer lock");
        held.filter(|h| *h > Instant::now())
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
    /// loc.gov answered 503 at once for this item while it served others
    /// (the module notes): the item is broken on LoC's side. Nothing was
    /// held; ask again later, not now.
    #[error("{0}")]
    Item(String),
}

/// The size asked of LoC's IIIF image server: the full scan, or scaled
/// to a percentage of it (IIIF's `pct:n` size).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    Full,
    Pct(u8),
}

impl ImageSize {
    /// The IIIF size parameter.
    pub fn iiif(self) -> String {
        match self {
            Self::Full => "full".into(),
            Self::Pct(p) => format!("pct:{p}"),
        }
    }
}

impl std::fmt::Display for ImageSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => f.write_str("full"),
            Self::Pct(p) => write!(f, "scaled:{p}"),
        }
    }
}

impl std::str::FromStr for ImageSize {
    type Err = String;
    /// `full`, or `scaled:<1-99>`.
    fn from_str(s: &str) -> Result<Self, String> {
        if s == "full" {
            return Ok(Self::Full);
        }
        s.strip_prefix("scaled:")
            .and_then(|p| p.parse::<u8>().ok())
            .filter(|p| (1..100).contains(p))
            .map(Self::Pct)
            .ok_or_else(|| format!("{s:?} isn't `full` or `scaled:<1-99>`"))
    }
}

/// A page's JPEG on the IIIF server, from its service base.
pub fn image_url(iiif_base: &str, size: ImageSize) -> String {
    format!("{iiif_base}/full/{}/0/default.jpg", size.iiif())
}

/// What's fetched for a page.
#[derive(Debug)]
pub struct Scan {
    /// The page's IIIF service base.
    pub iiif_base: String,
    pub jpeg: Bytes,
    /// How long the image request took (sent to last byte); zero from the cache.
    pub download: Duration,
    /// Read from the prefetch cache, not fetched.
    pub from_cache: bool,
    /// A buffer-set scan's URL on the scan server, to delete once the
    /// page's output is uploaded.
    pub evict: Option<String>,
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

/// What the API's answers say about telling an item error from a hold.
#[derive(Debug, Default)]
struct ApiHealth {
    /// The last item JSON URL that answered 2xx, and when (`None` for one
    /// set by [`LocClient::set_known_good`]).
    known_good: Option<(String, Option<Instant>)>,
    /// Item errors lately: when, and the title.
    item_errors: std::collections::VecDeque<(Instant, String)>,
}

/// What a check of the known-good item found.
enum Probe {
    Ok,
    /// It failed too: a hold, for at least this long if it said so.
    Failed(Option<Duration>),
}

pub struct LocClient {
    cfg: LocConfig,
    http: reqwest::Client,
    api: Pacer,
    tile: Pacer,
    items: moka::future::Cache<String, Pages>,
    health: Mutex<ApiHealth>,
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

/// A `Retry-After` value (seconds or an HTTP date), capped at an hour, and
/// whether the cap cut it.
fn retry_after(resp: &reqwest::Response) -> Option<(Duration, bool)> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let d = usnm_core::retry_after::parse(v, std::time::SystemTime::now())?;
    Some((d.min(MAX_RETRY_AFTER), d > MAX_RETRY_AFTER))
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
            health: Mutex::new(ApiHealth::default()),
            cfg,
            http,
            metrics: Metrics::global(),
        })
    }

    /// The item JSON URL of `key`'s issue.
    pub fn item_url(&self, key: &PageKey) -> String {
        format!("{}/item/{}/?fo=json", self.cfg.api_root, issue_id(key))
    }

    /// An item known to have answered 2xx before (from an earlier run),
    /// to check against when an item answers 503 before any item has
    /// answered in this process. One answered since takes its place.
    pub fn set_known_good(&self, url: String) {
        let mut h = self.health.lock().expect("health lock");
        if h.known_good.is_none() {
            h.known_good = Some((url, None));
        }
    }

    /// Whether a fast, bare 503 for item `url` of `title` is an item error
    /// rather than a hold (the module notes), and if not, how long a
    /// failed check asked to wait. Records it if it is.
    async fn item_error(&self, url: &str, title: &str) -> Result<(), Option<Duration>> {
        /// Whether item errors from too many titles, with this one, are
        /// inside the window.
        fn too_many_titles(h: &mut ApiHealth, title: &str) -> bool {
            let now = Instant::now();
            while h
                .item_errors
                .front()
                .is_some_and(|(t, _)| now.duration_since(*t) > ITEM_ERROR_WINDOW)
            {
                h.item_errors.pop_front();
            }
            let mut titles: std::collections::BTreeSet<&str> =
                h.item_errors.iter().map(|(_, t)| t.as_str()).collect();
            titles.insert(title);
            if titles.len() >= ITEM_ERROR_TITLES {
                tracing::warn!(
                    titles = titles.len(),
                    "item errors from several titles: holding instead"
                );
                return true;
            }
            false
        }
        let check = {
            let mut h = self.health.lock().expect("health lock");
            if too_many_titles(&mut h, title) {
                return Err(None);
            }
            let now = Instant::now();
            match &h.known_good {
                Some((good, _)) if good == url => return Err(None),
                Some((_, Some(at))) if now.duration_since(*at) <= ITEM_ERROR_OK_WITHIN => None,
                Some((good, _)) => Some(good.clone()),
                None => return Err(None),
            }
        };
        if let Some(good) = check {
            match self.probe(&good).await {
                Probe::Ok => {}
                Probe::Failed(d) => return Err(d),
            }
        }
        // Checked again with the record, under one lock: callers that
        // waited on probes together can't all pass the titles rule.
        let mut h = self.health.lock().expect("health lock");
        if too_many_titles(&mut h, title) {
            return Err(None);
        }
        h.item_errors.push_back((Instant::now(), title.to_owned()));
        Ok(())
    }

    /// One paced request for the known-good item, outside the cache.
    async fn probe(&self, url: &str) -> Probe {
        self.api.wait().await;
        match self.http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match read_limited(resp, MAX_ITEM_BYTES).await {
                    Ok(_) => {
                        self.health.lock().expect("health lock").known_good =
                            Some((url.to_owned(), Some(Instant::now())));
                        Probe::Ok
                    }
                    Err(_) => Probe::Failed(None),
                }
            }
            Ok(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    "the known-good item failed too"
                );
                Probe::Failed(retry_after(&resp).map(|(d, _)| d))
            }
            Err(e) => {
                tracing::warn!(error = %e.without_url(), "the known-good item failed too");
                Probe::Failed(None)
            }
        }
    }

    /// GET `url`, paced by `pacer`, retrying transient failures. Returns
    /// the body and how long the answering request took, from sending it to
    /// the end of the body (no pacing or retries). `title` is set for an
    /// item JSON, which can end in [`FetchError::Item`].
    async fn get(
        &self,
        url: &str,
        pacer: &Pacer,
        max: usize,
        title: Option<&str>,
    ) -> Result<(Bytes, Duration), FetchError> {
        let mut last = String::new();
        for attempt in 1..=self.cfg.attempts.max(1) {
            pacer.wait().await;
            let wait = self.cfg.backoff * attempt;
            let sent = Instant::now();
            match self.http.get(url).send().await {
                Ok(resp) if resp.status().is_success() => match read_limited(resp, max).await {
                    Ok(b) => {
                        if title.is_some() {
                            self.health.lock().expect("health lock").known_good =
                                Some((url.to_owned(), Some(Instant::now())));
                        }
                        return Ok((b, sent.elapsed()));
                    }
                    Err(e) => last = e,
                },
                Ok(resp) => {
                    let status = resp.status();
                    // A 403 is how a block looks: hold the server as for a
                    // 429, rather than dead-letter pages at the paced rate.
                    if matches!(
                        status,
                        StatusCode::TOO_MANY_REQUESTS
                            | StatusCode::SERVICE_UNAVAILABLE
                            | StatusCode::FORBIDDEN
                    ) {
                        let asked = retry_after(&resp);
                        let mut checked = None;
                        if let Some(title) = title {
                            // Any Retry-After, even one that can't be
                            // read, is a hold.
                            if status == StatusCode::SERVICE_UNAVAILABLE
                                && !resp.headers().contains_key(reqwest::header::RETRY_AFTER)
                                && sent.elapsed() < ITEM_ERROR_FAST
                            {
                                match self.item_error(url, title).await {
                                    Ok(()) => {
                                        tracing::warn!(
                                            url,
                                            title,
                                            "loc.gov answered 503 at once for this item while serving others: an item error, not holding"
                                        );
                                        return Err(FetchError::Item(format!(
                                            "{url} returned HTTP 503 at once while other items answered"
                                        )));
                                    }
                                    Err(d) => checked = d,
                                }
                            }
                        }
                        let capped = asked.is_some_and(|(_, c)| c);
                        let d = asked.map_or(wait, |(r, _)| r.max(wait));
                        let d = checked.map_or(d, |c| c.min(MAX_RETRY_AFTER).max(d));
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
                                // Retry-After over an hour, cut to one.
                                KeyValue::new("capped", capped),
                            ],
                        );
                        tracing::warn!(
                            status = status.as_u16(),
                            server,
                            wait_secs = d.as_secs(),
                            capped,
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

    /// Until when loc.gov's API or image server holds this node's requests.
    pub fn held_until(&self) -> Option<Instant> {
        match (self.api.held_until(), self.tile.held_until()) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        }
    }

    /// seq → IIIF base for `key`'s issue, from the cache or one API request.
    /// Only answers are cached: a 4xx on the issue fails this page and is
    /// asked again for the next, so a refusal can't empty a whole issue.
    async fn issue_pages(&self, key: &PageKey) -> Result<Pages, FetchError> {
        let issue = issue_id(key);
        let url = self.item_url(key);
        self.items
            .try_get_with(issue, async {
                match self
                    .get(&url, &self.api, MAX_ITEM_BYTES, Some(&key.lccn))
                    .await
                {
                    Ok((body, _)) => {
                        let item: Value = serde_json::from_slice(&body).map_err(|e| {
                            FetchError::Transient(format!("item JSON for {url}: {e}"))
                        })?;
                        Ok(Arc::new(page_images(&self.cfg.iiif_root, &item)))
                    }
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|e: Arc<FetchError>| match &*e {
                FetchError::Permanent(m) => FetchError::Permanent(m.clone()),
                FetchError::Transient(m) => FetchError::Transient(m.clone()),
                FetchError::Item(m) => FetchError::Item(m.clone()),
            })
    }

    /// The page's full-size JPEG.
    pub async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError> {
        self.scan_at(key, ImageSize::Full).await
    }

    /// The page's JPEG at `size` ([`image_url`]).
    pub async fn scan_at(&self, key: &PageKey, size: ImageSize) -> Result<Scan, FetchError> {
        let pages = self.issue_pages(key).await?;
        let base = pages
            .get(&key.seq)
            .cloned()
            .ok_or_else(|| FetchError::Permanent(format!("loc.gov lists no image for {key}")))?;
        let (jpeg, download) = self
            .get(&image_url(&base, size), &self.tile, MAX_IMAGE_BYTES, None)
            .await?;
        Ok(Scan {
            iiif_base: base,
            jpeg,
            download,
            from_cache: false,
            evict: None,
        })
    }
}

/// `<lccn>/<date>/ed-<n>`, an issue as loc.gov's item URLs name it.
pub fn issue_id(key: &PageKey) -> String {
    format!(
        "{}/{}/ed-{}",
        key.lccn,
        key.date.format("%Y-%m-%d"),
        key.edition
    )
}

pub(crate) async fn read_limited(mut resp: reqwest::Response, max: usize) -> Result<Bytes, String> {
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
            [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/nbu/nbu_gage_ver01/data/sn83045462/00271743700/1890050301/0365.jp2"},
             {"mimetype": "text/xml", "url": "https://tile.loc.gov/x.xml"}],
            [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/nbu/nbu_gage_ver01/data/sn83045462/00271743700/1890050301/0364.jp2"}],
            [{"mimetype": "text/xml", "url": "https://tile.loc.gov/y.xml"}]
        ]}]});
        let pages = page_images("https://tile.loc.gov/image-services/iiif/", &item);
        assert_eq!(pages.len(), 2);
        assert_eq!(
            pages[&1],
            "https://tile.loc.gov/image-services/iiif/service:ndnp:nbu:nbu_gage_ver01:data:sn83045462:00271743700:1890050301:0365"
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

    /// A stand-in loc.gov whose image server answers its first request
    /// with 403, and whose API refuses one issue with 404; counts both.
    async fn blocking_loc(images: Arc<AtomicU32>, refused: Arc<AtomicU32>) -> String {
        use axum::http::StatusCode as S;
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let item = serde_json::json!({"resources": [{"files": [
            [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/x/batch_x/data/sn1/1/1890050301/0001.jp2"}],
        ]}]});
        let app = axum::Router::new()
            .route(
                "/item/sn1/1890-05-03/ed-1/",
                axum::routing::get(move || {
                    let item = item.clone();
                    async move { axum::Json(item) }
                }),
            )
            .route(
                "/item/sn1/1890-05-04/ed-1/",
                axum::routing::get(move || {
                    let refused = refused.clone();
                    async move {
                        refused.fetch_add(1, Ordering::SeqCst);
                        S::NOT_FOUND
                    }
                }),
            )
            .route(
                "/iiif/{service}/full/full/0/default.jpg",
                axum::routing::get(move || {
                    let images = images.clone();
                    async move {
                        match images.fetch_add(1, Ordering::SeqCst) {
                            0 => S::FORBIDDEN.into_response(),
                            _ => b"jpeg".to_vec().into_response(),
                        }
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        root
    }

    #[tokio::test]
    async fn a_403_holds_the_server_and_an_issue_refusal_isnt_cached() {
        let (images, refused) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let root = blocking_loc(images.clone(), refused.clone()).await;
        let loc = LocClient::new(LocConfig {
            api_gap: Duration::from_millis(10),
            tile_gap: Duration::from_millis(10),
            attempts: 3,
            backoff: Duration::from_millis(300),
            api_root: root.clone(),
            iiif_root: format!("{root}/iiif/"),
        })
        .unwrap();
        assert_eq!(loc.held_until(), None);
        let t0 = std::time::Instant::now();
        let p1: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        // The 403 held the image server and was retried: not permanent.
        let s1 = loc.scan(&p1).await.unwrap();
        assert_eq!(&s1.jpeg[..], b"jpeg");
        assert_eq!(images.load(Ordering::SeqCst), 2);
        assert!(t0.elapsed() >= Duration::from_millis(300), "held");
        // A 4xx on an issue fails the page, and is asked again next time.
        let other: PageKey = "sn1/1890-05-04/ed-1/seq-1".parse().unwrap();
        for _ in 0..2 {
            assert!(matches!(
                loc.scan(&other).await,
                Err(FetchError::Permanent(_))
            ));
        }
        assert_eq!(refused.load(Ordering::SeqCst), 2);
    }

    /// How the stand-in answers an item.
    #[derive(Clone, Copy)]
    enum Answer {
        Ok,
        /// 503 at once, no Retry-After: a broken item.
        Bare503,
        /// 503 with `Retry-After: n`: a hold.
        RetryAfter(u32),
        /// 503 after 2.5 s, no Retry-After.
        Slow503,
        Too429,
        /// 503 at once with a `Retry-After` that can't be read.
        BadRetryAfter,
    }

    /// A stand-in loc.gov answering `/item/<lccn>/<date>/ed-1/` as
    /// `answers` says (keyed by `<lccn>/<date>`, anything else 404), and
    /// counting requests per item.
    async fn items_loc(
        answers: Vec<(&'static str, Answer)>,
    ) -> (String, Arc<Mutex<BTreeMap<String, u32>>>) {
        use axum::http::StatusCode as S;
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let answers: Arc<BTreeMap<String, Answer>> = Arc::new(
            answers
                .into_iter()
                .map(|(k, a)| (k.to_owned(), a))
                .collect(),
        );
        let hits: Arc<Mutex<BTreeMap<String, u32>>> = Arc::default();
        let h = hits.clone();
        let app = axum::Router::new().route(
            "/item/{lccn}/{date}/{ed}/",
            axum::routing::get(
                move |axum::extract::Path((lccn, date, _ed)): axum::extract::Path<(
                    String,
                    String,
                    String,
                )>| {
                    let answers = answers.clone();
                    let hits = h.clone();
                    async move {
                        let k = format!("{lccn}/{date}");
                        *hits.lock().unwrap().entry(k.clone()).or_default() += 1;
                        match answers.get(&k) {
                            None => S::NOT_FOUND.into_response(),
                            Some(Answer::Ok) => axum::Json(serde_json::json!({"resources": [{"files": [
                                [{"mimetype": "image/jp2", "url": "https://tile.loc.gov/storage-services/service/ndnp/x/batch_x/data/sn1/1/1890050301/0001.jp2"}],
                            ]}]}))
                            .into_response(),
                            Some(Answer::Bare503) => S::SERVICE_UNAVAILABLE.into_response(),
                            Some(Answer::RetryAfter(n)) => {
                                (S::SERVICE_UNAVAILABLE, [("retry-after", n.to_string())])
                                    .into_response()
                            }
                            Some(Answer::Slow503) => {
                                tokio::time::sleep(Duration::from_millis(2500)).await;
                                S::SERVICE_UNAVAILABLE.into_response()
                            }
                            Some(Answer::Too429) => S::TOO_MANY_REQUESTS.into_response(),
                            Some(Answer::BadRetryAfter) => {
                                (S::SERVICE_UNAVAILABLE, [("retry-after", "soon")]).into_response()
                            }
                        }
                    }
                },
            ),
        );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (root, hits)
    }

    fn items_client(root: &str, attempts: u32) -> LocClient {
        LocClient::new(LocConfig {
            api_gap: Duration::from_millis(10),
            tile_gap: Duration::from_millis(10),
            attempts,
            backoff: Duration::from_millis(400),
            api_root: root.to_owned(),
            iiif_root: format!("{root}/iiif/"),
        })
        .unwrap()
    }

    fn page(lccn: &str, date: &str) -> PageKey {
        format!("{lccn}/{date}/ed-1/seq-1").parse().unwrap()
    }

    /// Ask for `key`'s issue only (no image).
    async fn item(loc: &LocClient, key: &PageKey) -> Result<Pages, FetchError> {
        loc.issue_pages(key).await
    }

    #[tokio::test]
    async fn an_instant_503_for_one_item_while_others_answer_is_an_item_error() {
        let (root, hits) = items_loc(vec![
            ("sn1/1904-07-29", Answer::Ok),
            ("sn1/1904-07-30", Answer::Bare503),
            ("sn1/1904-07-31", Answer::Bare503),
        ])
        .await;
        let loc = items_client(&root, 3);
        item(&loc, &page("sn1", "1904-07-29")).await.unwrap();
        let t0 = std::time::Instant::now();
        let e = item(&loc, &page("sn1", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Item(_)), "{e:?}");
        // Nothing held and no retries.
        assert_eq!(loc.held_until(), None);
        assert!(t0.elapsed() < Duration::from_millis(400));
        assert_eq!(hits.lock().unwrap()["sn1/1904-07-30"], 1);
        // The title's next broken item: an item answered within the
        // minute, so no check is needed.
        let e = item(&loc, &page("sn1", "1904-07-31")).await.unwrap_err();
        assert!(matches!(e, FetchError::Item(_)), "{e:?}");
        assert_eq!(hits.lock().unwrap()["sn1/1904-07-29"], 1);
        assert_eq!(loc.held_until(), None);
    }

    #[tokio::test]
    async fn real_holds_still_hold() {
        let (root, hits) = items_loc(vec![
            ("sn1/1904-07-29", Answer::Ok),
            ("sn2/1904-07-30", Answer::RetryAfter(1)),
            ("sn3/1904-07-30", Answer::Too429),
            ("sn4/1904-07-30", Answer::Slow503),
            ("sn5/1904-07-30", Answer::BadRetryAfter),
        ])
        .await;
        let loc = items_client(&root, 1);
        item(&loc, &page("sn1", "1904-07-29")).await.unwrap();
        // A 503 with Retry-After is a hold, though another item answered.
        let e = item(&loc, &page("sn2", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        item(&loc, &page("sn1", "1904-07-29")).await.unwrap();
        // A 429 is a hold.
        let e = item(&loc, &page("sn3", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
        tokio::time::sleep(Duration::from_millis(500)).await;
        // A slow 503 is a hold.
        let e = item(&loc, &page("sn4", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
        assert_eq!(hits.lock().unwrap()["sn4/1904-07-30"], 1);
        tokio::time::sleep(Duration::from_millis(500)).await;
        item(&loc, &page("sn1", "1904-07-29")).await.unwrap();
        // A Retry-After that can't be read is still a hold.
        let e = item(&loc, &page("sn5", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
    }

    #[tokio::test]
    async fn with_nothing_known_good_an_instant_503_holds() {
        let (root, _) = items_loc(vec![("sn1/1904-07-30", Answer::Bare503)]).await;
        let loc = items_client(&root, 1);
        let e = item(&loc, &page("sn1", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
    }

    #[tokio::test]
    async fn an_old_known_good_item_is_asked_again_first() {
        let (root, hits) = items_loc(vec![
            ("sn1/1904-07-29", Answer::Ok),
            ("sn1/1904-07-30", Answer::Bare503),
            ("sn9/1904-07-29", Answer::Bare503),
            ("sn9/1904-07-30", Answer::Bare503),
        ])
        .await;
        // From an earlier run: checked with one request, which answers.
        let loc = items_client(&root, 1);
        loc.set_known_good(loc.item_url(&page("sn1", "1904-07-29")));
        let e = item(&loc, &page("sn1", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Item(_)), "{e:?}");
        assert_eq!(hits.lock().unwrap()["sn1/1904-07-29"], 1);
        assert_eq!(loc.held_until(), None);
        // A known-good item that fails too: a hold.
        let loc = items_client(&root, 1);
        loc.set_known_good(loc.item_url(&page("sn9", "1904-07-29")));
        let e = item(&loc, &page("sn9", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
    }

    #[tokio::test]
    async fn item_errors_across_titles_become_a_hold() {
        let (root, _) = items_loc(vec![
            ("sn1/1904-07-29", Answer::Ok),
            ("sn2/1904-07-30", Answer::Bare503),
            ("sn3/1904-07-30", Answer::Bare503),
            ("sn4/1904-07-30", Answer::Bare503),
        ])
        .await;
        let loc = items_client(&root, 1);
        item(&loc, &page("sn1", "1904-07-29")).await.unwrap();
        for t in ["sn2", "sn3"] {
            let e = item(&loc, &page(t, "1904-07-30")).await.unwrap_err();
            assert!(matches!(e, FetchError::Item(_)), "{t}: {e:?}");
        }
        assert_eq!(loc.held_until(), None);
        // A third title inside the window: loc.gov, not the items.
        let e = item(&loc, &page("sn4", "1904-07-30")).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
    }

    #[tokio::test]
    async fn concurrent_item_errors_after_a_restart_still_count_titles() {
        let (root, _) = items_loc(vec![
            ("sn1/1904-07-29", Answer::Ok),
            ("sn2/1904-07-30", Answer::Bare503),
            ("sn3/1904-07-30", Answer::Bare503),
            ("sn4/1904-07-30", Answer::Bare503),
            ("sn5/1904-07-30", Answer::Bare503),
        ])
        .await;
        let loc = Arc::new(items_client(&root, 1));
        // Known good from an earlier run: each caller checks it first.
        loc.set_known_good(loc.item_url(&page("sn1", "1904-07-29")));
        let mut tasks = Vec::new();
        for t in ["sn2", "sn3", "sn4", "sn5"] {
            let loc = loc.clone();
            tasks.push(tokio::spawn(async move {
                matches!(
                    item(&loc, &page(t, "1904-07-30")).await,
                    Err(FetchError::Item(_))
                )
            }));
        }
        let mut items = 0;
        for t in tasks {
            items += usize::from(t.await.unwrap());
        }
        assert_eq!(items, ITEM_ERROR_TITLES - 1);
        assert!(loc.held_until().is_some());
    }

    #[test]
    fn parses_image_sizes() {
        assert_eq!("full".parse(), Ok(ImageSize::Full));
        assert_eq!("scaled:50".parse(), Ok(ImageSize::Pct(50)));
        for bad in ["scaled:0", "scaled:100", "scaled:", "half", "pct:50"] {
            assert!(bad.parse::<ImageSize>().is_err(), "{bad}");
        }
        assert_eq!(ImageSize::Pct(50).to_string(), "scaled:50");
        assert_eq!(
            image_url(
                "https://tile.loc.gov/image-services/iiif/service:a:b",
                ImageSize::Pct(50)
            ),
            "https://tile.loc.gov/image-services/iiif/service:a:b/full/pct:50/0/default.jpg"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_pacer_reports_its_hold() {
        let pacer = Pacer::new(Duration::from_millis(10));
        assert_eq!(pacer.held_until(), None);
        pacer.hold(Duration::from_secs(3600));
        let until = pacer.held_until().unwrap();
        assert_eq!(until - Instant::now(), Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_secs(3601)).await;
        assert_eq!(pacer.held_until(), None);
    }
}
