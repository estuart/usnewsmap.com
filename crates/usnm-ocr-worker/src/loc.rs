//! Page scans from the Library of Congress.
//!
//! **The batch route** (#368, the fetcher's first choice): a page's original
//! JP2 from its batch's directory on `tile.loc.gov`'s storage service
//! ([`crate::batch`]). One `manifest-md5.txt` per batch (kept on disk)
//! names every file and its md5; one METS per issue (kept on disk too)
//! maps seq to the JP2's frame number; the JP2 itself is checked against
//! the bag's md5. Its IIIF base is built from the same path. Nothing goes
//! to www.loc.gov. [`LocClient::resolve`] answers `None` when the batch,
//! the issue, its METS or the page isn't there, and only then does the
//! caller go to the item API.
//!
//! **The item API route** (the worker's, and the fetcher's fallback): for
//! each issue, one request to loc.gov's item JSON
//! (`https://www.loc.gov/item/{lccn}/{date}/ed-{n}/?fo=json`) gives every
//! page's JP2 file; its path names the page's IIIF service
//! (`https://tile.loc.gov/image-services/iiif/service:…`), and the scan is
//! that base plus `/full/full/0/default.jpg`. The item JSON is cached (LRU),
//! so the pages of one issue cost one API request.
//!
//! **Pacing.** loc.gov's API, its image server and its storage service are
//! each paced by a gap between requests shared by every task on the node
//! (the storage service shares the image server's pacer unless told it has
//! a budget of its own: LoC publishes a limit for each, but not whether
//! they are counted together). `Retry-After` is honoured on 429 and 503, a
//! 403 holds the server the same way (a block isn't the page's fault), and
//! every request names the site in its User-Agent. A hold is never worked
//! around: a page whose batch route is held fails as transient and doesn't
//! go to the item API.
//!
//! **Item errors** (#414). Some items answer 503 at once, for hours, while
//! the rest of loc.gov answers normally: a fault in that item on LoC's
//! side, not a hold. An item JSON 503, or a METS or JP2 503 on the storage
//! service, counts as an item error, with nothing held, only when all of
//! these are true:
//! - on the storage service, it is the second 503 for that file, after the
//!   first held the server as usual (503s under load look the same there);
//! - it came back within [`ITEM_ERROR_FAST`], with no `Retry-After`;
//! - another request to that server answered 2xx within
//!   [`ITEM_ERROR_OK_WITHIN`], or one paced request for the last small file
//!   that answered 2xx there (the *known-good* item) answers 2xx now;
//! - fewer than [`ITEM_ERROR_TITLES`] different titles, this one included,
//!   had item errors on that server in the last [`ITEM_ERROR_WINDOW`].
//!
//! Anything else (a 429, a 403, a `Retry-After`, a slow 503, a 503 on the
//! image server or for a batch manifest, a failed check, or item errors
//! spread over several titles) holds the server as before. When unsure, it
//! holds.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use opentelemetry::KeyValue;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::Instant;
use usnm_core::ids::PageKey;

use crate::batch::{self, BagIndex, Md5Sum};
use crate::telemetry::Metrics;

pub const USER_AGENT: &str = concat!(
    "usnewsmap-ocr-worker/",
    env!("CARGO_PKG_VERSION"),
    " (+https://usnewsmap.com)"
);

/// The largest item JSON read.
const MAX_ITEM_BYTES: usize = 16 * 1024 * 1024;
/// The largest scan read (full-size JPEGs and JP2s are a few MB).
pub const MAX_IMAGE_BYTES: usize = 128 * 1024 * 1024;
/// The largest batch manifest read (a 10,000-page batch's is about 3 MB).
const MAX_BAG_BYTES: usize = 128 * 1024 * 1024;
/// The largest issue METS read (about 16 KB; a signed one 55 KB).
const MAX_METS_BYTES: usize = 4 * 1024 * 1024;
/// A file at most this big may be asked for again to check that its
/// server is answering (the known-good item).
const PROBE_MAX_BYTES: usize = 1024 * 1024;
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

/// Where IIIF bases point.
pub const IIIF_ROOT: &str = "https://tile.loc.gov/image-services/iiif/";
/// The storage service, under which every batch's files are.
pub const STORAGE_SERVICE_ROOT: &str = "https://tile.loc.gov/storage-services/service/";

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

impl FetchError {
    fn cloned(&self) -> Self {
        match self {
            Self::Permanent(m) => Self::Permanent(m.clone()),
            Self::Transient(m) => Self::Transient(m.clone()),
            Self::Item(m) => Self::Item(m.clone()),
        }
    }
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

/// What a scan's bytes are: LoC's IIIF JPEG, or the batch's original JP2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScanFormat {
    #[default]
    Jpeg,
    Jp2,
}

impl ScanFormat {
    /// The file extension.
    pub fn ext(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Jp2 => "jp2",
        }
    }

    /// The name in sidecars: `jpeg` or `jp2`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Jp2 => "jp2",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "jpeg" => Some(Self::Jpeg),
            "jp2" => Some(Self::Jp2),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Jp2 => "image/jp2",
        }
    }
}

/// Which of LoC's servers a request goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    /// www.loc.gov's JSON API.
    Api,
    /// tile.loc.gov's IIIF image server.
    Tile,
    /// tile.loc.gov's storage service (batch files).
    Storage,
}

impl Server {
    pub fn name(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Tile => "tile",
            Self::Storage => "storage",
        }
    }
}

/// A page's JPEG on the IIIF server, from its service base.
pub fn image_url(iiif_base: &str, size: ImageSize) -> String {
    format!("{iiif_base}/full/{}/0/default.jpg", size.iiif())
}

/// What's fetched for a page.
#[derive(Debug)]
pub struct Scan {
    /// The page's IIIF service base: its id, whatever the bytes are.
    pub iiif_base: String,
    /// The URL the bytes came from, on tile.loc.gov: the IIIF JPEG, or the
    /// JP2 on the storage service.
    pub url: String,
    pub format: ScanFormat,
    pub bytes: Bytes,
    /// For a JP2, the md5 the batch's bag manifest lists, which the bytes
    /// were checked against.
    pub md5: Option<String>,
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
    /// Between loc.gov API requests: 5 s, www.loc.gov's `Crawl-Delay`.
    pub api_gap: Duration,
    pub tile_gap: Duration,
    /// Between storage-service requests (batch manifests, METS, JP2s).
    pub storage_gap: Duration,
    /// The storage service gets a pacer of its own. Otherwise it shares the
    /// image server's, at the longer of the two gaps, and a hold on either
    /// holds both.
    pub storage_own_budget: bool,
    pub attempts: u32,
    /// The wait after a failed attempt, times the attempt number.
    pub backoff: Duration,
    /// `https://www.loc.gov` (a test server in tests).
    pub api_root: String,
    /// Where IIIF bases point (`https://tile.loc.gov/image-services/iiif/`).
    pub iiif_root: String,
    /// NDNP's batches on the storage service
    /// (`https://tile.loc.gov/storage-services/service/ndnp/`).
    pub storage_root: String,
    /// Chronicling America's batch directories, which redirect to the
    /// storage service (`https://chroniclingamerica.loc.gov/data/batches/`).
    pub batches_root: String,
    /// Where batch manifests and issue METS are kept, so each is fetched
    /// once. None: memory only.
    pub batch_cache: Option<PathBuf>,
}

impl Default for LocConfig {
    fn default() -> Self {
        Self {
            api_gap: Duration::from_millis(5000),
            tile_gap: Duration::from_millis(1000),
            storage_gap: Duration::from_millis(1000),
            storage_own_budget: false,
            attempts: 5,
            backoff: Duration::from_secs(60),
            api_root: "https://www.loc.gov".into(),
            iiif_root: IIIF_ROOT.into(),
            storage_root: format!("{STORAGE_SERVICE_ROOT}ndnp/"),
            batches_root: "https://chroniclingamerica.loc.gov/data/batches/".into(),
            batch_cache: None,
        }
    }
}

type Pages = Arc<BTreeMap<u16, String>>;

/// A batch's bag manifest, and where its files are.
#[derive(Debug)]
pub struct Bag {
    /// `<batch>_ver<NN>`.
    pub dir: String,
    /// The batch's directory on the storage service, ending in `/`.
    pub url: String,
    pub index: BagIndex,
}

/// A page found through its batch.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchPage {
    /// The JP2 on the storage service.
    pub url: String,
    pub iiif_base: String,
    /// The bag manifest's md5 for the JP2.
    pub md5: Md5Sum,
}

/// What a server's answers say about telling an item error from a hold.
#[derive(Debug, Default)]
struct ApiHealth {
    /// The last small file (at most [`PROBE_MAX_BYTES`]) that answered
    /// 2xx, or one set by [`LocClient::set_known_good`]: what a check asks
    /// for again.
    known_good: Option<String>,
    /// When anything on the server last answered 2xx.
    last_ok: Option<Instant>,
    /// Item errors lately: when, and the title.
    item_errors: std::collections::VecDeque<(Instant, String)>,
}

/// What a check of the known-good item found.
enum Probe {
    Ok,
    /// It failed too: a hold, for at least this long if it said so.
    Failed(Option<Duration>),
}

/// A successful GET.
struct Got {
    body: Bytes,
    /// Sending to the end of the body.
    took: Duration,
}

pub struct LocClient {
    cfg: LocConfig,
    http: reqwest::Client,
    /// The same, following no redirect, for [`LocClient::locate`].
    http_here: reqwest::Client,
    api: Pacer,
    tile: Arc<Pacer>,
    storage: Arc<Pacer>,
    items: moka::future::Cache<String, Pages>,
    /// By batch directory; `None` for a batch LoC doesn't have.
    bags: moka::future::Cache<String, Option<Arc<Bag>>>,
    /// By `<batch dir>/<lccn>/<issue dir>`; `None` for an issue whose METS
    /// isn't there.
    mets: moka::future::Cache<String, Option<Pages>>,
    health: Mutex<ApiHealth>,
    storage_health: Mutex<ApiHealth>,
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

/// Write `body` to `path` through a `.part` file and a rename.
async fn write_file(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    tokio::fs::write(&part, body).await?;
    tokio::fs::rename(&part, path).await
}

impl LocClient {
    pub fn new(cfg: LocConfig) -> anyhow::Result<Self> {
        let http_here = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(180))
            // Redirects within a host only. Chronicling America's batch
            // redirect to the storage service is followed by hand
            // ([`LocClient::locate`]), once checked and paced.
            .redirect(reqwest::redirect::Policy::custom(|a| {
                let same = a.previous().last().is_some_and(|p| {
                    p.host_str() == a.url().host_str() && p.scheme() == a.url().scheme()
                });
                if a.previous().len() >= 5 || !same {
                    a.stop()
                } else {
                    a.follow()
                }
            }))
            .build()?;
        let (tile, storage) = if cfg.storage_own_budget {
            (
                Arc::new(Pacer::new(cfg.tile_gap)),
                Arc::new(Pacer::new(cfg.storage_gap)),
            )
        } else {
            let shared = Arc::new(Pacer::new(cfg.tile_gap.max(cfg.storage_gap)));
            (Arc::clone(&shared), shared)
        };
        Ok(Self {
            api: Pacer::new(cfg.api_gap),
            tile,
            storage,
            items: moka::future::Cache::builder()
                .max_capacity(256)
                .time_to_live(Duration::from_secs(3600))
                .build(),
            bags: moka::future::Cache::builder()
                .max_capacity(32)
                .time_to_live(Duration::from_secs(6 * 3600))
                .build(),
            mets: moka::future::Cache::builder()
                .max_capacity(4096)
                .time_to_live(Duration::from_secs(3600))
                .build(),
            health: Mutex::new(ApiHealth::default()),
            storage_health: Mutex::new(ApiHealth::default()),
            cfg,
            http,
            http_here,
            metrics: Metrics::global(),
        })
    }

    fn pacer(&self, server: Server) -> &Pacer {
        match server {
            Server::Api => &self.api,
            Server::Tile => &self.tile,
            Server::Storage => &self.storage,
        }
    }

    /// The item-error record of a server that has items (not the image
    /// server, whose 503s always hold).
    fn health(&self, server: Server) -> Option<&Mutex<ApiHealth>> {
        match server {
            Server::Api => Some(&self.health),
            Server::Storage => Some(&self.storage_health),
            Server::Tile => None,
        }
    }

    /// The item JSON URL of `key`'s issue.
    pub fn item_url(&self, key: &PageKey) -> String {
        format!("{}/item/{}/?fo=json", self.cfg.api_root, issue_id(key))
    }

    /// An item known to have answered 2xx before (from an earlier run),
    /// to check against when an item answers 503 before any item has
    /// answered in this process. One answered since takes its place.
    pub fn set_known_good(&self, url: String) {
        self.set_known_good_on(Server::Api, url);
    }

    /// The same, for `server`: a small file there, such as an issue METS.
    pub fn set_known_good_on(&self, server: Server, url: String) {
        if let Some(h) = self.health(server) {
            let mut h = h.lock().expect("health lock");
            if h.known_good.is_none() {
                h.known_good = Some(url);
            }
        }
    }

    /// Whether a fast, bare 503 for item `url` of `title` on `server` is
    /// an item error rather than a hold (the module notes), and if not, how
    /// long a failed check asked to wait. Records it if it is.
    async fn item_error(
        &self,
        server: Server,
        url: &str,
        title: &str,
    ) -> Result<(), Option<Duration>> {
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
        let Some(health) = self.health(server) else {
            return Err(None);
        };
        let check = {
            let mut h = health.lock().expect("health lock");
            if too_many_titles(&mut h, title) {
                return Err(None);
            }
            let now = Instant::now();
            if h.known_good.as_deref() == Some(url) {
                return Err(None);
            }
            match (&h.known_good, h.last_ok) {
                (_, Some(at)) if now.duration_since(at) <= ITEM_ERROR_OK_WITHIN => None,
                (Some(good), _) => Some(good.clone()),
                (None, _) => return Err(None),
            }
        };
        if let Some(good) = check {
            match self.probe(server, &good).await {
                Probe::Ok => {}
                Probe::Failed(d) => return Err(d),
            }
        }
        // Checked again with the record, under one lock: callers that
        // waited on probes together can't all pass the titles rule.
        let mut h = health.lock().expect("health lock");
        if too_many_titles(&mut h, title) {
            return Err(None);
        }
        h.item_errors.push_back((Instant::now(), title.to_owned()));
        Ok(())
    }

    /// One paced request for the known-good item, outside the caches.
    async fn probe(&self, server: Server, url: &str) -> Probe {
        self.pacer(server).wait().await;
        match self.http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match read_limited(resp, MAX_ITEM_BYTES).await {
                    Ok(_) => {
                        if let Some(h) = self.health(server) {
                            let mut h = h.lock().expect("health lock");
                            h.known_good = Some(url.to_owned());
                            h.last_ok = Some(Instant::now());
                        }
                        Probe::Ok
                    }
                    Err(_) => Probe::Failed(None),
                }
            }
            Ok(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    server = server.name(),
                    "the known-good item failed too"
                );
                Probe::Failed(retry_after(&resp).map(|(d, _)| d))
            }
            Err(e) => {
                tracing::warn!(error = %e.without_url(), server = server.name(), "the known-good item failed too");
                Probe::Failed(None)
            }
        }
    }

    /// A 2xx from `server` for an item of a title: it is serving. Only a
    /// small answer becomes the known-good item, so a check never asks for
    /// megabytes.
    fn answered(&self, server: Server, url: &str, len: usize) {
        if let Some(h) = self.health(server) {
            let mut h = h.lock().expect("health lock");
            h.last_ok = Some(Instant::now());
            if len <= PROBE_MAX_BYTES {
                h.known_good = Some(url.to_owned());
            }
        }
    }

    /// GET `url`, paced by `server`'s pacer, retrying transient failures.
    /// `title` is set for an item (an item JSON, a METS, a JP2), which can
    /// end in [`FetchError::Item`].
    async fn get(
        &self,
        url: &str,
        server: Server,
        max: usize,
        title: Option<&str>,
    ) -> Result<Got, FetchError> {
        self.fetch(url, server, max, title, false)
            .await
            .map(|g| g.expect("a 404 is an error unless asked for"))
    }

    /// [`Self::get`], with `missing_ok` answering `None` for a 404 or 410.
    async fn fetch(
        &self,
        url: &str,
        server: Server,
        max: usize,
        title: Option<&str>,
        missing_ok: bool,
    ) -> Result<Option<Got>, FetchError> {
        let pacer = self.pacer(server);
        let mut last = String::new();
        // The last answer for this file was a fast, bare 503.
        let mut bare_before = false;
        for attempt in 1..=self.cfg.attempts.max(1) {
            // Whether the answer just before this one was a fast, bare
            // 503; any other answer in between clears it.
            let prev_bare = std::mem::take(&mut bare_before);
            pacer.wait().await;
            let wait = self.cfg.backoff * attempt;
            let sent = Instant::now();
            match self.http.get(url).send().await {
                Ok(resp) if resp.status().is_success() => match read_limited(resp, max).await {
                    Ok(b) => {
                        if title.is_some() {
                            self.answered(server, url, b.len());
                        }
                        return Ok(Some(Got {
                            body: b,
                            took: sent.elapsed(),
                        }));
                    }
                    Err(e) => last = e,
                },
                Ok(resp) => {
                    let status = resp.status();
                    if missing_ok && matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) {
                        return Ok(None);
                    }
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
                        // Any Retry-After, even one that can't be read, is
                        // a hold.
                        let bare = status == StatusCode::SERVICE_UNAVAILABLE
                            && !resp.headers().contains_key(reqwest::header::RETRY_AFTER)
                            && sent.elapsed() < ITEM_ERROR_FAST;
                        // On the storage service, where 503s under load
                        // look the same, only a second fast, bare 503 for
                        // the same file in a row, after the first held, can
                        // be an item error.
                        let may_be_item = server == Server::Api || prev_bare;
                        bare_before = bare;
                        if let (Some(title), Some(_), true, true) =
                            (title, self.health(server), may_be_item, bare)
                        {
                            match self.item_error(server, url, title).await {
                                Ok(()) => {
                                    tracing::warn!(
                                        url,
                                        title,
                                        server = server.name(),
                                        "loc.gov answered 503 at once for this item while serving others: an item error, not holding"
                                    );
                                    return Err(FetchError::Item(format!(
                                        "{url} returned HTTP 503 at once while other items answered"
                                    )));
                                }
                                Err(d) => checked = d,
                            }
                        }
                        let capped = asked.is_some_and(|(_, c)| c);
                        let d = asked.map_or(wait, |(r, _)| r.max(wait));
                        let d = checked.map_or(d, |c| c.min(MAX_RETRY_AFTER).max(d));
                        // Everyone waits, not just this request.
                        pacer.hold(d);
                        self.metrics.loc_slowdowns.add(
                            1,
                            &[
                                KeyValue::new("status", i64::from(status.as_u16())),
                                KeyValue::new("server", server.name()),
                                KeyValue::new("retry_after", asked.is_some()),
                                // Retry-After over an hour, cut to one.
                                KeyValue::new("capped", capped),
                            ],
                        );
                        tracing::warn!(
                            status = status.as_u16(),
                            server = server.name(),
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

    /// Until when loc.gov's API, image server or storage service holds this
    /// node's requests.
    pub fn held_until(&self) -> Option<Instant> {
        [
            self.api.held_until(),
            self.tile.held_until(),
            self.storage.held_until(),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// seq → IIIF base for `key`'s issue, from the cache or one API request.
    /// Only answers are cached: a 4xx on the issue fails this page and is
    /// asked again for the next, so a refusal can't empty a whole issue.
    async fn issue_pages(&self, key: &PageKey) -> Result<Pages, FetchError> {
        let issue = issue_id(key);
        let url = self.item_url(key);
        self.items
            .try_get_with(issue, async {
                let got = self
                    .get(&url, Server::Api, MAX_ITEM_BYTES, Some(&key.lccn))
                    .await?;
                let item: Value = serde_json::from_slice(&got.body)
                    .map_err(|e| FetchError::Transient(format!("item JSON for {url}: {e}")))?;
                Ok(Arc::new(page_images(&self.cfg.iiif_root, &item)))
            })
            .await
            .map_err(|e: Arc<FetchError>| e.cloned())
    }

    /// The page's full-size JPEG, through the item API.
    pub async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError> {
        self.scan_at(key, ImageSize::Full).await
    }

    /// The page's JPEG at `size` ([`image_url`]), through the item API.
    pub async fn scan_at(&self, key: &PageKey, size: ImageSize) -> Result<Scan, FetchError> {
        let pages = self.issue_pages(key).await?;
        let base = pages
            .get(&key.seq)
            .cloned()
            .ok_or_else(|| FetchError::Permanent(format!("loc.gov lists no image for {key}")))?;
        self.iiif_scan(base, size).await
    }

    /// The JPEG at `size` of the page with IIIF base `base`.
    pub async fn iiif_scan(&self, base: String, size: ImageSize) -> Result<Scan, FetchError> {
        let url = image_url(&base, size);
        let got = self.get(&url, Server::Tile, MAX_IMAGE_BYTES, None).await?;
        Ok(Scan {
            iiif_base: base,
            url,
            format: ScanFormat::Jpeg,
            bytes: got.body,
            md5: None,
            download: got.took,
            from_cache: false,
            evict: None,
        })
    }

    /// Where the batch's files go in the disk cache.
    fn cache_dir(&self, dir: &str) -> Option<PathBuf> {
        self.cfg.batch_cache.as_ref().map(|c| c.join(dir))
    }

    /// The batch's bag manifest: from the disk cache, or fetched once (the
    /// guessed storage path, then Chronicling America's redirect). `None`
    /// when LoC has no such batch.
    pub async fn bag(&self, batch: &str, version: u16) -> Result<Option<Arc<Bag>>, FetchError> {
        let Some(dir) = batch::batch_dir(batch, version) else {
            return Ok(None);
        };
        self.bags
            .try_get_with(dir.clone(), async { self.load_bag(&dir).await })
            .await
            .map_err(|e: Arc<FetchError>| e.cloned())
    }

    /// Where `url` redirects, without following it: one paced request on
    /// the storage service's pacer. `None` if it answers anything but a
    /// redirect (a 404, or the file itself, which isn't the storage
    /// service). A 429, 503 or 403 holds the storage service as any of its
    /// answers would.
    async fn locate(&self, url: &str) -> Result<Option<String>, FetchError> {
        for attempt in 1..=self.cfg.attempts.max(1) {
            self.storage.wait().await;
            let resp = match self.http_here.get(url).send().await {
                Ok(r) => r,
                Err(e) => {
                    if attempt == self.cfg.attempts.max(1) {
                        return Err(FetchError::Transient(format!(
                            "giving up on {url}: {}",
                            e.without_url()
                        )));
                    }
                    tokio::time::sleep(self.cfg.backoff * attempt).await;
                    continue;
                }
            };
            let status = resp.status();
            if status.is_redirection() {
                let to = resp
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .and_then(|l| resp.url().join(l).ok());
                return Ok(to.map(|u| u.to_string()));
            }
            if matches!(
                status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::SERVICE_UNAVAILABLE
                    | StatusCode::FORBIDDEN
            ) {
                let wait = self.cfg.backoff * attempt;
                let d = retry_after(&resp).map_or(wait, |(r, _)| r.max(wait));
                self.storage.hold(d);
                tracing::warn!(
                    status = status.as_u16(),
                    server = "storage",
                    wait_secs = d.as_secs(),
                    "loc.gov asked us to slow down"
                );
                continue;
            }
            return Ok(None);
        }
        Err(FetchError::Transient(format!("giving up on {url}: held")))
    }

    /// Seed the storage service's known-good item from the last JP2 an
    /// earlier run fetched: its issue's METS, as kept in the batch cache
    /// (the plain or the signed one, whichever is there), so a check never
    /// asks for a file that isn't on LoC.
    pub fn seed_storage_known_good(&self, jp2_url: &str) {
        let Some(rel) = jp2_url.strip_prefix(&self.cfg.storage_root) else {
            return;
        };
        // <inst>/batch_<dir>/data/<lccn>/<reel>/<issue>/<frame>.jp2
        let segs: Vec<&str> = rel.split('/').collect();
        let [_, bdir, "data", lccn, _, issue, _] = segs[..] else {
            return;
        };
        let (Some(bdir), Some(cache), Some(base)) = (
            bdir.strip_prefix("batch_"),
            self.cfg.batch_cache.as_ref(),
            jp2_url.rsplit_once('/').map(|(b, _)| b),
        ) else {
            return;
        };
        for name in [format!("{issue}.xml"), format!("{issue}_1.xml")] {
            if cache
                .join(bdir)
                .join("mets")
                .join(lccn)
                .join(&name)
                .is_file()
            {
                self.set_known_good_on(Server::Storage, format!("{base}/{name}"));
                return;
            }
        }
    }

    async fn load_bag(&self, dir: &str) -> Result<Option<Arc<Bag>>, FetchError> {
        let cache = self.cache_dir(dir);
        if let Some(c) = &cache {
            let text = tokio::fs::read_to_string(c.join("manifest-md5.txt")).await;
            let url = tokio::fs::read_to_string(c.join("url")).await;
            if let (Ok(text), Ok(url)) = (text, url) {
                let url = url.trim().to_owned();
                if url.starts_with(&self.cfg.storage_root) && url.ends_with('/') {
                    return Ok(Some(Arc::new(Bag {
                        dir: dir.to_owned(),
                        url,
                        index: BagIndex::parse(&text),
                    })));
                }
            }
        }
        let guess = batch::storage_batch_url(&self.cfg.storage_root, dir);
        let mut found = self
            .fetch(
                &format!("{guess}manifest-md5.txt"),
                Server::Storage,
                MAX_BAG_BYTES,
                None,
                true,
            )
            .await?
            .map(|g| (guess.clone(), g));
        if found.is_none() {
            // Chronicling America knows where the batch is: follow it, but
            // only onto the storage service.
            let via = format!("{}{dir}/manifest-md5.txt", self.cfg.batches_root);
            if let Some(to) = self.locate(&via).await? {
                match to
                    .strip_suffix("manifest-md5.txt")
                    .filter(|u| u.starts_with(&self.cfg.storage_root))
                {
                    Some(u) => {
                        found = self
                            .fetch(&to, Server::Storage, MAX_BAG_BYTES, None, true)
                            .await?
                            .map(|g| (u.to_owned(), g));
                    }
                    None => {
                        tracing::warn!(
                            batch = dir,
                            to,
                            "the batch's redirect leaves the storage service; not following it"
                        );
                    }
                }
            }
        }
        let Some((url, got)) = found else {
            tracing::warn!(
                batch = dir,
                "LoC has no manifest for this batch; its pages go through the item API"
            );
            return Ok(None);
        };
        let text = String::from_utf8_lossy(&got.body).into_owned();
        let index = BagIndex::parse(&text);
        tracing::info!(
            batch = dir,
            url,
            issues = index.issues(),
            jp2s = index.jp2s,
            conflicts = index.conflicts,
            "batch manifest read"
        );
        if let Some(c) = &cache {
            let w = async {
                write_file(&c.join("manifest-md5.txt"), text.as_bytes()).await?;
                write_file(&c.join("url"), url.as_bytes()).await
            };
            if let Err(e) = w.await {
                tracing::warn!(batch = dir, error = %e, "can't keep the batch manifest on disk");
            }
        }
        Ok(Some(Arc::new(Bag {
            dir: dir.to_owned(),
            url,
            index,
        })))
    }

    /// seq → frame for an issue of `bag`, from its METS (checked against
    /// the bag's md5), kept on disk and in memory. `None` if the METS isn't
    /// there; one that doesn't parse is an error.
    async fn issue_frames(
        &self,
        bag: &Bag,
        lccn: &str,
        dir: &str,
    ) -> Result<Option<Pages>, FetchError> {
        let Some(files) = bag.index.issue(lccn, dir) else {
            return Ok(None);
        };
        let Some((name, md5)) = files.mets_file(dir) else {
            return Ok(None);
        };
        let key = format!("{}/{lccn}/{dir}", bag.dir);
        let disk = self
            .cache_dir(&bag.dir)
            .map(|c| c.join("mets").join(lccn).join(&name));
        let url = batch::file_url(&bag.url, lccn, &files.reel, dir, &name);
        self.mets
            .try_get_with(key, async {
                let mut body = None;
                if let Some(p) = &disk {
                    if let Ok(b) = tokio::fs::read(p).await {
                        if batch::md5(&b) == md5 {
                            body = Some(Bytes::from(b));
                        }
                    }
                }
                let body = match body {
                    Some(b) => b,
                    None => {
                        let Some(got) = self
                            .fetch(&url, Server::Storage, MAX_METS_BYTES, Some(lccn), true)
                            .await?
                        else {
                            tracing::warn!(url, "the issue's METS isn't there");
                            return Ok(None);
                        };
                        if batch::md5(&got.body) != md5 {
                            return Err(FetchError::Transient(format!(
                                "{url}: md5 {} isn't the bag's {}",
                                batch::md5_hex(&got.body),
                                batch::hex(&md5)
                            )));
                        }
                        if let Some(p) = &disk {
                            if let Err(e) = write_file(p, &got.body).await {
                                tracing::warn!(url, error = %e, "can't keep the METS on disk");
                            }
                        }
                        got.body
                    }
                };
                match batch::mets_pages(&body) {
                    Ok(p) => Ok(Some(Arc::new(p))),
                    Err(e) => {
                        // Not a page the batch route can't place: the METS
                        // is the bag's, so something is wrong; fail it
                        // rather than go round through the item API.
                        Err(FetchError::Transient(format!(
                            "{url}: can't read the issue's METS: {e}"
                        )))
                    }
                }
            })
            .await
            .map_err(|e: Arc<FetchError>| e.cloned())
    }

    /// Where `key`'s JP2 is, through its batch (`batch`, `version`), and
    /// its IIIF base and md5. `None` when the batch route can't place it:
    /// no such batch, the issue or its METS isn't in the bag, or the METS
    /// lists no JP2 for the seq. A hold or a failure is an error, never
    /// `None`, so it is never sent round the item API.
    pub async fn resolve(
        &self,
        key: &PageKey,
        batch: &str,
        version: u16,
    ) -> Result<Option<BatchPage>, FetchError> {
        let Some(bag) = self.bag(batch, version).await? else {
            return Ok(None);
        };
        let dir = batch::issue_dir(key.date, key.edition);
        let Some(frames) = self.issue_frames(&bag, &key.lccn, &dir).await? else {
            return Ok(None);
        };
        let Some(files) = bag.index.issue(&key.lccn, &dir) else {
            return Ok(None);
        };
        let Some(frame) = frames.get(&key.seq) else {
            return Ok(None);
        };
        let Some(md5) = files.jp2.get(frame) else {
            return Ok(None);
        };
        let url = batch::file_url(
            &bag.url,
            &key.lccn,
            &files.reel,
            &dir,
            &format!("{frame}.jp2"),
        );
        let Some(base) = iiif_base(&self.cfg.iiif_root, &url) else {
            return Ok(None);
        };
        Ok(Some(BatchPage {
            url,
            iiif_base: base,
            md5: *md5,
        }))
    }

    /// The page `resolve` found: the JP2, checked against the bag's md5, at
    /// full size; the IIIF JPEG of its base when scaled. `None` if the JP2
    /// answers 404 though the bag lists it.
    pub async fn scan_resolved(
        &self,
        key: &PageKey,
        page: &BatchPage,
        size: ImageSize,
    ) -> Result<Option<Scan>, FetchError> {
        if let ImageSize::Pct(_) = size {
            return self.iiif_scan(page.iiif_base.clone(), size).await.map(Some);
        }
        let Some(got) = self
            .fetch(
                &page.url,
                Server::Storage,
                MAX_IMAGE_BYTES,
                Some(&key.lccn),
                true,
            )
            .await?
        else {
            tracing::warn!(url = page.url, "a JP2 the bag lists isn't there");
            return Ok(None);
        };
        let md5 = batch::md5(&got.body);
        if md5 != page.md5 {
            return Err(FetchError::Transient(format!(
                "{}: md5 {} isn't the bag's {}",
                page.url,
                batch::hex(&md5),
                batch::hex(&page.md5)
            )));
        }
        Ok(Some(Scan {
            iiif_base: page.iiif_base.clone(),
            url: page.url.clone(),
            format: ScanFormat::Jp2,
            bytes: got.body,
            md5: Some(batch::hex(&md5)),
            download: got.took,
            from_cache: false,
            evict: None,
        }))
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
            ..LocConfig::default()
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
        assert_eq!(&s1.bytes[..], b"jpeg");
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
            ..LocConfig::default()
        })
        .unwrap();
        assert_eq!(loc.held_until(), None);
        let t0 = std::time::Instant::now();
        let p1: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        // The 403 held the image server and was retried: not permanent.
        let s1 = loc.scan(&p1).await.unwrap();
        assert_eq!(&s1.bytes[..], b"jpeg");
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
            ..LocConfig::default()
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

    use crate::standin::{Page, Scripted, StandIn};

    const BATCH: &str = "/storage-services/service/ndnp/xx/batch_xx_test_ver01/";

    /// A client of `s` whose storage requests are `gap` apart.
    fn batch_client(s: &StandIn, gap: Duration, cache: Option<PathBuf>, own: bool) -> LocClient {
        LocClient::new(LocConfig {
            api_gap: Duration::from_millis(10),
            tile_gap: Duration::from_millis(10),
            storage_gap: gap,
            storage_own_budget: own,
            attempts: 3,
            backoff: Duration::from_millis(10),
            api_root: s.root.clone(),
            iiif_root: s.iiif_root(),
            storage_root: s.storage_root(),
            batches_root: s.batches_root(),
            batch_cache: cache,
        })
        .unwrap()
    }

    /// One issue of sn1 (1890-05-03, ed. 1) in batch `xx_test_ver01`,
    /// whose pages' frames aren't in seq order.
    fn one_issue(s: &StandIn) {
        s.add_issue(
            BATCH,
            "sn1",
            "0001A",
            "1890050301",
            &[
                Page {
                    seq: 1,
                    frame: "0101",
                    jp2: b"jp2 page one",
                },
                Page {
                    seq: 2,
                    frame: "0099",
                    jp2: b"jp2 page two",
                },
            ],
        );
    }

    fn key(seq: u16) -> PageKey {
        format!("sn1/1890-05-03/ed-1/seq-{seq}").parse().unwrap()
    }

    #[tokio::test]
    async fn resolves_through_the_batch_and_checks_the_md5() {
        let s = StandIn::start().await;
        one_issue(&s);
        let cache = tempfile::tempdir().unwrap();
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        let p2 = loc.resolve(&key(2), "xx_test", 1).await.unwrap().unwrap();
        assert!(p2
            .url
            .ends_with(&format!("{BATCH}data/sn1/0001A/1890050301/0099.jp2")));
        assert_eq!(
            p2.iiif_base,
            format!(
                "{}service:ndnp:xx:batch_xx_test_ver01:data:sn1:0001A:1890050301:0099",
                s.iiif_root()
            )
        );
        let scan = loc
            .scan_resolved(&key(2), &p2, ImageSize::Full)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&scan.bytes[..], b"jp2 page two");
        assert_eq!(scan.format, ScanFormat::Jp2);
        assert_eq!(scan.url, p2.url);
        assert_eq!(
            scan.md5.as_deref(),
            Some(batch::md5_hex(b"jp2 page two").as_str())
        );
        // One manifest and one METS for the issue's two pages.
        let p1 = loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        assert!(p1.url.ends_with("/0101.jp2"));
        assert_eq!(
            (s.hits("manifest-md5.txt"), s.hits("1890050301.xml")),
            (1, 1)
        );
        // Nothing LoC doesn't list: no seq 3, no such issue, no such batch.
        assert_eq!(loc.resolve(&key(3), "xx_test", 1).await.unwrap(), None);
        let other: PageKey = "sn1/1890-05-04/ed-1/seq-1".parse().unwrap();
        assert_eq!(loc.resolve(&other, "xx_test", 1).await.unwrap(), None);
        assert_eq!(loc.resolve(&key(1), "xx_none", 1).await.unwrap(), None);
        assert_eq!(loc.resolve(&key(1), "Bad-Batch", 1).await.unwrap(), None);
        // A JP2 whose bytes aren't the bag's: transient, never used.
        s.put(
            &format!("{BATCH}data/sn1/0001A/1890050301/0101.jp2"),
            b"changed".to_vec(),
        );
        let e = loc
            .scan_resolved(&key(1), &p1, ImageSize::Full)
            .await
            .unwrap_err();
        assert!(
            matches!(e, FetchError::Transient(ref m) if m.contains("md5")),
            "{e:?}"
        );
        // A JP2 the bag lists that answers 404: None, for the item API.
        s.put(
            &format!("{BATCH}data/sn1/0001A/1890050301/0101.jp2"),
            Vec::new(),
        );
        s.script(
            &format!("{BATCH}data/sn1/0001A/1890050301/0101.jp2"),
            Scripted::Status(404),
        );
        assert!(loc
            .scan_resolved(&key(1), &p1, ImageSize::Full)
            .await
            .unwrap()
            .is_none());
        // A scaled scan is the IIIF JPEG of the same base.
        s.put(
            "/iiif/service:ndnp:xx:batch_xx_test_ver01:data:sn1:0001A:1890050301:0099/full/pct:50/0/default.jpg",
            b"half".to_vec(),
        );
        let half = loc
            .scan_resolved(&key(2), &p2, ImageSize::Pct(50))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (&half.bytes[..], half.format),
            (&b"half"[..], ScanFormat::Jpeg)
        );

        // A new process with the same cache asks LoC for neither again.
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        assert_eq!(
            loc.resolve(&key(2), "xx_test", 1).await.unwrap().unwrap(),
            p2
        );
        assert_eq!(
            (
                s.hits(&format!("{BATCH}manifest-md5.txt")),
                s.hits("1890050301.xml")
            ),
            (1, 1)
        );
        assert!(cache.path().join("xx_test_ver01/manifest-md5.txt").exists());
        assert!(cache
            .path()
            .join("xx_test_ver01/mets/sn1/1890050301.xml")
            .exists());
        // A METS on disk that isn't the bag's is fetched again.
        std::fs::write(
            cache.path().join("xx_test_ver01/mets/sn1/1890050301.xml"),
            b"x",
        )
        .unwrap();
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        loc.resolve(&key(2), "xx_test", 1).await.unwrap().unwrap();
        assert_eq!(s.hits("1890050301.xml"), 2);
    }

    #[tokio::test]
    async fn follows_chronicling_americas_redirect_onto_the_storage_service_only() {
        let s = StandIn::start().await;
        // The batch isn't under its name's first word.
        let elsewhere = "/storage-services/service/ndnp/yy/batch_xx_test_ver01/";
        s.add_issue(
            elsewhere,
            "sn1",
            "0001A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: b"jp2",
            }],
        );
        s.redirect(
            "/data/batches/xx_test_ver01/manifest-md5.txt",
            &format!("{elsewhere}manifest-md5.txt"),
        );
        let loc = batch_client(&s, Duration::from_millis(10), None, false);
        let p = loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        assert!(p.url.contains("/ndnp/yy/batch_xx_test_ver01/data/"));
        // A redirect off the storage service isn't followed at all.
        s.put("/other/manifest-md5.txt", "x".to_owned());
        s.redirect(
            "/data/batches/xx_off_ver01/manifest-md5.txt",
            "/other/manifest-md5.txt",
        );
        assert_eq!(loc.resolve(&key(1), "xx_off", 1).await.unwrap(), None);
        assert_eq!(s.hits("/other/manifest-md5.txt"), 0);
    }

    #[tokio::test]
    async fn the_storage_pacer_spaces_requests_and_honours_holds() {
        let s = StandIn::start().await;
        one_issue(&s);
        let jp2 = format!("{BATCH}data/sn1/0001A/1890050301/0099.jp2");
        s.script(&jp2, Scripted::RetryAfter(503, 1));
        let loc = batch_client(&s, Duration::from_millis(200), None, false);
        let t0 = std::time::Instant::now();
        let p = loc.resolve(&key(2), "xx_test", 1).await.unwrap().unwrap();
        let scan = loc
            .scan_resolved(&key(2), &p, ImageSize::Full)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&scan.bytes[..], b"jp2 page two");
        assert!(
            t0.elapsed() >= Duration::from_secs(1),
            "waited out Retry-After"
        );
        let mut t: Vec<std::time::Instant> = ["manifest-md5.txt", ".xml", ".jp2"]
            .iter()
            .flat_map(|x| s.times(x))
            .collect();
        t.sort();
        // manifest, METS, JP2 (503), JP2: each at least the gap apart, the
        // last after the hold.
        assert_eq!(t.len(), 4);
        for w in t.windows(2) {
            assert!(
                w[1] - w[0] >= Duration::from_millis(150),
                "{:?}",
                w[1] - w[0]
            );
        }
        assert!(t[3] - t[2] >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_storage_hold_holds_the_image_server_unless_it_has_its_own_budget() {
        let s = StandIn::start().await;
        one_issue(&s);
        let manifest = format!("{BATCH}manifest-md5.txt");
        for own in [false, true] {
            s.script(&manifest, Scripted::RetryAfter(429, 3600));
            let loc = batch_client(&s, Duration::from_millis(10), None, own);
            let r = tokio::time::timeout(
                Duration::from_millis(500),
                loc.resolve(&key(1), "xx_test", 1),
            )
            .await;
            assert!(r.is_err(), "still held");
            assert!(loc.storage.held_until().is_some());
            assert_eq!(loc.tile.held_until().is_some(), !own, "own budget: {own}");
            assert!(loc.held_until().is_some());
        }
    }

    #[tokio::test]
    async fn a_403_on_the_storage_service_holds_and_isnt_sent_round_the_api() {
        let s = StandIn::start().await;
        one_issue(&s);
        let jp2 = format!("{BATCH}data/sn1/0001A/1890050301/0101.jp2");
        for _ in 0..3 {
            s.script(&jp2, Scripted::Status(403));
        }
        let loc = batch_client(&s, Duration::from_millis(10), None, false);
        let p = loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        let e = loc
            .scan_resolved(&key(1), &p, ImageSize::Full)
            .await
            .unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(loc.held_until().is_some());
        assert_eq!(s.hits("/item/sn1/1890-05-03/ed-1/"), 0);
    }

    #[tokio::test]
    async fn a_fast_bare_503_for_one_issue_on_the_storage_service_is_an_item_error() {
        let s = StandIn::start().await;
        one_issue(&s);
        s.add_issue(
            BATCH,
            "sn2",
            "0002A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: b"x",
            }],
        );
        let loc = batch_client(&s, Duration::from_millis(10), None, false);
        // sn1's METS answers; sn2's answers 503 at once, twice: the
        // first holds, the second is an item error.
        loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        for _ in 0..2 {
            s.script(
                &format!("{BATCH}data/sn2/0002A/1890050301/1890050301.xml"),
                Scripted::Status(503),
            );
        }
        let sn2: PageKey = "sn2/1890-05-03/ed-1/seq-1".parse().unwrap();
        let e = loc.resolve(&sn2, "xx_test", 1).await.unwrap_err();
        assert!(matches!(e, FetchError::Item(_)), "{e:?}");
        assert_eq!(
            s.hits(&format!("{BATCH}data/sn2/0002A/1890050301/1890050301.xml")),
            2
        );
        // Only the first one's 10 ms hold.
        assert_eq!(loc.held_until(), None);
        // A single 503 for a file that answers next time is only a hold.
        s.script(
            &format!("{BATCH}data/sn1/0001A/1890050301/0101.jp2"),
            Scripted::Status(503),
        );
        let p1 = loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        loc.scan_resolved(&key(1), &p1, ImageSize::Full)
            .await
            .unwrap()
            .unwrap();
        // A slow 503 is a hold.
        s.script(
            &format!("{BATCH}data/sn2/0002A/1890050301/1890050301.xml"),
            Scripted::Slow(503, Duration::from_millis(2500)),
        );
        let mut cfg = batch_client(&s, Duration::from_millis(10), None, false).cfg;
        cfg.attempts = 1;
        cfg.backoff = Duration::from_secs(5);
        let slow = LocClient::new(cfg).unwrap();
        slow.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        let e = slow.resolve(&sn2, "xx_test", 1).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
        assert!(slow.held_until().is_some());
        // A manifest's 503 always holds, however fast.
        s.script(&format!("{BATCH}manifest-md5.txt"), Scripted::Status(503));
        let mut cfg = batch_client(&s, Duration::from_millis(10), None, false).cfg;
        cfg.backoff = Duration::from_secs(5);
        let loc = LocClient::new(cfg).unwrap();
        let r = tokio::time::timeout(
            Duration::from_millis(300),
            loc.resolve(&key(1), "xx_test", 1),
        )
        .await;
        assert!(r.is_err(), "held");
        assert!(loc.held_until().is_some());
    }

    #[tokio::test]
    async fn a_jp2_never_becomes_the_known_good_item() {
        let s = StandIn::start().await;
        let big = vec![7u8; PROBE_MAX_BYTES + 1];
        s.add_issue(
            BATCH,
            "sn1",
            "0001A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: &big,
            }],
        );
        let cache = tempfile::tempdir().unwrap();
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        // A new process: the METS comes from the disk cache, so the only
        // storage answer it gets is the JP2.
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        let p = loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        loc.scan_resolved(&key(1), &p, ImageSize::Full)
            .await
            .unwrap()
            .unwrap();
        let h = loc.storage_health.lock().unwrap();
        assert!(h.last_ok.is_some());
        assert_eq!(h.known_good, None);
    }

    #[tokio::test]
    async fn the_storage_known_good_seed_is_a_mets_on_disk() {
        let s = StandIn::start().await;
        let cache = tempfile::tempdir().unwrap();
        let loc = batch_client(
            &s,
            Duration::from_millis(10),
            Some(cache.path().into()),
            false,
        );
        let jp2 = format!(
            "{}xx/batch_xx_test_ver01/data/sn1/0001A/1890050301/0001.jp2",
            s.storage_root()
        );
        // Nothing on disk: no seed.
        loc.seed_storage_known_good(&jp2);
        assert_eq!(loc.storage_health.lock().unwrap().known_good, None);
        // Only the signed METS is kept: that one.
        let mets = cache.path().join("xx_test_ver01/mets/sn1");
        std::fs::create_dir_all(&mets).unwrap();
        std::fs::write(mets.join("1890050301_1.xml"), b"x").unwrap();
        loc.seed_storage_known_good(&jp2);
        assert_eq!(
            loc.storage_health.lock().unwrap().known_good.as_deref(),
            Some(jp2.replace("0001.jp2", "1890050301_1.xml").as_str())
        );
    }

    #[tokio::test]
    async fn only_two_bare_503s_in_a_row_on_storage_make_an_item_error() {
        let s = StandIn::start().await;
        one_issue(&s);
        s.add_issue(
            BATCH,
            "sn2",
            "0002A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: b"x",
            }],
        );
        let mets2 = format!("{BATCH}data/sn2/0002A/1890050301/1890050301.xml");
        let mut cfg = batch_client(&s, Duration::from_millis(10), None, false).cfg;
        cfg.attempts = 2;
        let loc = LocClient::new(cfg).unwrap();
        loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        // A 503 with Retry-After, then a bare one: the bare one is the
        // first of its kind, so it holds; the request gives up transient.
        s.script(&mets2, Scripted::RetryAfter(503, 1));
        s.script(&mets2, Scripted::Status(503));
        let sn2: PageKey = "sn2/1890-05-03/ed-1/seq-1".parse().unwrap();
        let e = loc.resolve(&sn2, "xx_test", 1).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
    }

    #[tokio::test]
    async fn a_mets_that_doesnt_parse_fails_rather_than_going_round() {
        let s = StandIn::start().await;
        let mets = s.add_issue(
            BATCH,
            "sn1",
            "0001A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: b"x",
            }],
        );
        // A broken METS with the bag's md5 for it.
        let broken = b"<mets><dmdSec".to_vec();
        s.put(&mets, broken.clone());
        s.put(
            &format!("{BATCH}manifest-md5.txt"),
            format!(
                "{}  data/sn1/0001A/1890050301/1890050301.xml\n{}  data/sn1/0001A/1890050301/0001.jp2\n",
                batch::md5_hex(&broken),
                batch::md5_hex(b"x")
            ),
        );
        let loc = batch_client(&s, Duration::from_millis(10), None, false);
        let e = loc.resolve(&key(1), "xx_test", 1).await.unwrap_err();
        assert!(
            matches!(e, FetchError::Transient(ref m) if m.contains("METS")),
            "{e:?}"
        );
    }

    #[tokio::test]
    async fn another_failure_between_bare_503s_clears_the_first() {
        let s = StandIn::start().await;
        one_issue(&s);
        s.add_issue(
            BATCH,
            "sn2",
            "0002A",
            "1890050301",
            &[Page {
                seq: 1,
                frame: "0001",
                jp2: b"x",
            }],
        );
        let mets2 = format!("{BATCH}data/sn2/0002A/1890050301/1890050301.xml");
        let mut cfg = batch_client(&s, Duration::from_millis(10), None, false).cfg;
        cfg.attempts = 3;
        let loc = LocClient::new(cfg).unwrap();
        loc.resolve(&key(1), "xx_test", 1).await.unwrap().unwrap();
        for a in [
            Scripted::Status(503),
            Scripted::Status(500),
            Scripted::Status(503),
        ] {
            s.script(&mets2, a);
        }
        let sn2: PageKey = "sn2/1890-05-03/ed-1/seq-1".parse().unwrap();
        let e = loc.resolve(&sn2, "xx_test", 1).await.unwrap_err();
        assert!(matches!(e, FetchError::Transient(_)), "{e:?}");
    }
}
