//! Scans read from the prefetch cache over HTTP before asking LoC (#368,
//! ADR-0015 decision 9). The scan server in the scan-fetch container on
//! pve-big (`serve-scans`) serves `/scans/<set>/<batch>/<lccn>/<date>/<ed>/<seq>.<jp2|jpg>`
//! and its sidecar; the node mounts nothing.
//!
//! The worker asks for the keep set's copy, then the buffer set's, with
//! the same async client settings it uses for LoC: a 2 s connect timeout and
//! a bounded total, so a server that is away costs a page seconds, not a
//! stuck thread. In each set it asks for the JP2 (fetched through the
//! page's batch) and then the JPEG (fetched before the batch route, or
//! through the item API). A scan is used only when its sidecar names this
//! page and batch at full size in that format, its URL is its IIIF base's
//! file on tile.loc.gov (the IIIF image URL for a JPEG, the storage
//! service's JP2 for a JP2), and the bytes' size, sha256 and, where the
//! sidecar has one, the bag's md5 match it. Then the result records that
//! base as `image_url`, so `ocr-sync`'s check on it holds, and the ALTO
//! records the file's URL, its sha256 and its md5. A miss, a timeout, a 5xx
//! or a scan that doesn't check out goes to LoC at once.
//!
//! A buffer-set scan is deleted (`DELETE`) once the page's output is
//! uploaded, best effort; the server refuses to delete keep-set scans.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use usnm_core::ids::PageKey;

use crate::batch::jp2_url_of;
use crate::loc::{image_url, FetchError, ImageSize, Scan, ScanFormat, MAX_IMAGE_BYTES, USER_AGENT};
use crate::prefetch::{matches_sidecar, Set, Sidecar};
use crate::worker::ScanSource;

/// The cache's client: quick to give up, so LoC is never far behind.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest sidecar read.
const MAX_SIDECAR_BYTES: usize = 64 * 1024;

/// The cache in front of another source (LoC).
#[derive(Debug)]
pub struct CachedScans {
    /// The scan server, `http://<host>:<port>`.
    pub base: String,
    /// Where IIIF bases must point (`https://tile.loc.gov/image-services/iiif/`).
    pub iiif_root: String,
    /// Where JP2s must be (`https://tile.loc.gov/storage-services/service/`).
    pub storage_root: String,
    pub inner: Arc<dyn ScanSource>,
    http: reqwest::Client,
}

/// A IIIF base under `root`, shaped as [`crate::loc::iiif_base`] builds them.
fn base_ok(root: &str, base: &str) -> bool {
    base.strip_prefix(root)
        .and_then(|r| r.strip_prefix("service:"))
        .is_some_and(|r| {
            !r.is_empty()
                && !r.contains("..")
                && r.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.'))
        })
}

/// What a lookup in one set found.
enum Found {
    Scan(Box<Scan>),
    /// Not in this set: try the next.
    Missing,
    /// The server failed or the scan didn't check out: go to LoC.
    GiveUp,
}

impl CachedScans {
    /// `base` must be an `http` or `https` URL with a host (and a valid
    /// port, if any); anything else fails here, at startup.
    pub fn new(
        base: &str,
        iiif_root: String,
        storage_root: String,
        inner: Arc<dyn ScanSource>,
    ) -> anyhow::Result<Self> {
        let u = reqwest::Url::parse(base)
            .map_err(|e| anyhow::anyhow!("--scan-cache-url {base:?}: {e}"))?;
        anyhow::ensure!(
            matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()),
            "--scan-cache-url {base:?} must be http(s)://host[:port]"
        );
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            iiif_root,
            storage_root,
            inner,
            http: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()?,
        })
    }

    /// The scan's URL on the server, in `set` and `format`.
    pub fn url(&self, set: Set, key: &PageKey, batch: &str, format: ScanFormat) -> String {
        format!(
            "{}/scans/{}/{batch}/{}/{}/{}/{}.{}",
            self.base,
            set.dir(),
            key.lccn,
            key.date.format("%Y-%m-%d"),
            key.edition,
            key.seq,
            format.ext()
        )
    }

    /// Whether `side`'s URL is its base's file in `format` on tile.loc.gov.
    fn url_ok(&self, side: &Sidecar, format: ScanFormat) -> bool {
        base_ok(&self.iiif_root, &side.iiif_base)
            && match format {
                ScanFormat::Jpeg => side.loc_url == image_url(&side.iiif_base, ImageSize::Full),
                ScanFormat::Jp2 => {
                    jp2_url_of(&self.iiif_root, &self.storage_root, &side.iiif_base).as_deref()
                        == Some(side.loc_url.as_str())
                }
            }
    }

    async fn get(&self, url: &str, max: usize) -> Result<Option<Bytes>, String> {
        let resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status().as_u16()));
        }
        // Read in chunks and stop at the cap, as for LoC.
        crate::loc::read_limited(resp, max).await.map(Some)
    }

    /// The page in `set`: its JP2, else its JPEG.
    async fn lookup(&self, set: Set, key: &PageKey, batch: &str) -> Found {
        for format in [ScanFormat::Jp2, ScanFormat::Jpeg] {
            match self.lookup_as(set, key, batch, format).await {
                Found::Missing => continue,
                found => return found,
            }
        }
        Found::Missing
    }

    async fn lookup_as(&self, set: Set, key: &PageKey, batch: &str, format: ScanFormat) -> Found {
        let url = self.url(set, key, batch, format);
        let doc_id = key.doc_id();
        let side = match self.get(&format!("{url}.json"), MAX_SIDECAR_BYTES).await {
            Ok(Some(b)) => b,
            Ok(None) => return Found::Missing,
            Err(e) => {
                tracing::warn!(
                    doc_id,
                    error = e,
                    "the scan cache failed; fetching from LoC"
                );
                return Found::GiveUp;
            }
        };
        let Ok(side) = serde_json::from_slice::<Sidecar>(&side) else {
            tracing::warn!(
                doc_id,
                "a cached scan's sidecar doesn't parse; fetching from LoC"
            );
            return Found::GiveUp;
        };
        if side.doc_id != doc_id
            || side.batch != batch
            || side.resolution != ImageSize::Full.to_string()
            || side.format != format.name()
            || !self.url_ok(&side, format)
        {
            tracing::warn!(
                doc_id,
                "a cached scan's sidecar doesn't match the page; fetching from LoC"
            );
            return Found::GiveUp;
        }
        let bytes = match self.get(&url, MAX_IMAGE_BYTES).await {
            Ok(Some(b)) => b,
            Ok(None) => return Found::Missing,
            Err(e) => {
                tracing::warn!(
                    doc_id,
                    error = e,
                    "the scan cache failed; fetching from LoC"
                );
                return Found::GiveUp;
            }
        };
        if !matches_sidecar(&bytes, &side) {
            tracing::warn!(
                doc_id,
                "a cached scan doesn't match its sidecar; fetching from LoC"
            );
            return Found::GiveUp;
        }
        Found::Scan(Box::new(Scan {
            iiif_base: side.iiif_base,
            url: side.loc_url,
            format,
            bytes,
            md5: side.md5,
            download: Duration::ZERO,
            from_cache: true,
            evict: (set == Set::Buffer).then_some(url),
        }))
    }
}

#[async_trait]
impl ScanSource for CachedScans {
    async fn scan(&self, key: &PageKey, batch: &str) -> Result<Scan, FetchError> {
        // One bound on the whole lookup (both sets' sidecars and the scan),
        // so LoC is never more than REQUEST_TIMEOUT away.
        let lookup = async {
            for set in [Set::Keep, Set::Buffer] {
                match self.lookup(set, key, batch).await {
                    Found::Scan(s) => return Some(*s),
                    Found::Missing => continue,
                    Found::GiveUp => break,
                }
            }
            None
        };
        match tokio::time::timeout(REQUEST_TIMEOUT, lookup).await {
            Ok(Some(s)) => return Ok(s),
            Ok(None) => {}
            Err(_) => {
                tracing::warn!(doc_id = %key.doc_id(), "the scan cache was too slow; fetching from LoC")
            }
        }
        self.inner.scan(key, batch).await
    }

    /// Never: a LoC hold doesn't stop pages being read from the cache, so
    /// the worker keeps taking messages. A page that misses the cache
    /// during a hold waits in LoC's pacer, and may be abandoned if the hold
    /// outlasts its budget, as a page did before the hold check.
    fn held_until(&self) -> Option<tokio::time::Instant> {
        None
    }

    async fn evict(&self, url: &str) {
        match self.http.delete(url).send().await {
            Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok(r) => tracing::warn!(
                url,
                status = r.status().as_u16(),
                "couldn't delete a buffer scan"
            ),
            Err(e) => {
                tracing::warn!(url, error = %e.without_url(), "couldn't delete a buffer scan")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::sha256;
    use crate::serve::{router, AllowList, ServeOptions};
    use chrono::Utc;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    const ROOT: &str = "https://tile.loc.gov/image-services/iiif/";
    const STORAGE: &str = "https://tile.loc.gov/storage-services/service/";

    #[derive(Debug, Default)]
    struct Loc(AtomicU32);

    #[async_trait]
    impl ScanSource for Loc {
        async fn scan(&self, _key: &PageKey, _batch: &str) -> Result<Scan, FetchError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Scan {
                iiif_base: format!("{ROOT}service:from:loc"),
                url: format!("{ROOT}service:from:loc/full/full/0/default.jpg"),
                format: ScanFormat::Jpeg,
                bytes: Bytes::from_static(b"loc"),
                md5: None,
                download: Duration::ZERO,
                from_cache: false,
                evict: None,
            })
        }
    }

    fn put(
        dir: &Path,
        set: &str,
        key: &PageKey,
        batch: &str,
        body: &[u8],
        edit: impl FnOnce(&mut Sidecar),
    ) {
        let path = dir
            .join(set)
            .join(batch)
            .join(&key.lccn)
            .join(key.date.format("%Y-%m-%d").to_string())
            .join(key.edition.to_string())
            .join(format!("{}.jpg", key.seq));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let base = format!("{ROOT}service:ndnp:x:batch_x:data:sn1:1:1890050301:0001");
        let mut side = Sidecar {
            doc_id: key.doc_id(),
            batch: batch.into(),
            batch_version: 1,
            loc_url: image_url(&base, ImageSize::Full),
            iiif_base: base,
            resolution: "full".into(),
            fetched_at: Utc::now(),
            bytes: body.len() as u64,
            sha256: sha256(body),
            download_ms: 1,
            user_agent: "x".into(),
            format: "jpeg".into(),
            md5: None,
        };
        edit(&mut side);
        let mut sp = path.into_os_string();
        sp.push(".json");
        std::fs::write(sp, serde_json::to_vec(&side).unwrap()).unwrap();
    }

    async fn server(scans: &Path) -> String {
        let opts = ServeOptions {
            scans: scans.to_owned(),
            manifests: scans.join("m"),
            allow: Arc::new(
                AllowList::resolve(&["127.0.0.1".to_string()])
                    .await
                    .unwrap(),
            ),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(opts).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        base
    }

    #[tokio::test]
    async fn reads_keep_then_buffer_and_falls_back_to_loc() {
        let dir = tempfile::tempdir().unwrap();
        let base = server(dir.path()).await;
        let loc = Arc::new(Loc::default());
        let cache = CachedScans::new(&base, ROOT.into(), STORAGE.into(), loc.clone()).unwrap();
        let key: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        // Nothing cached: LoC.
        assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
        // In the buffer: read, and marked for eviction.
        put(dir.path(), "buffer", &key, "batch_x", b"jpeg bytes", |_| {});
        let s = cache.scan(&key, "batch_x").await.unwrap();
        assert!(s.from_cache);
        assert_eq!(&s.bytes[..], b"jpeg bytes");
        let url = s.evict.clone().unwrap();
        assert!(url.ends_with("/scans/buffer/batch_x/sn1/1890-05-03/1/1.jpg"));
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
        // Evicting removes it; the next read goes to LoC.
        cache.evict(&url).await;
        assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
        // In the keep set: read, never evicted.
        put(dir.path(), "keep", &key, "batch_x", b"keep bytes", |_| {});
        let s = cache.scan(&key, "batch_x").await.unwrap();
        assert!(s.from_cache && s.evict.is_none());
        // Another batch's copy isn't this one's.
        assert!(!cache.scan(&key, "batch_y").await.unwrap().from_cache);
        // A sidecar off the image server, a wrong sha256, a scaled scan or
        // a URL not its base's: LoC.
        for edit in [
            Box::new(|s: &mut Sidecar| {
                s.iiif_base = "https://evil.example/iiif/service:x".into();
                s.loc_url = image_url(&s.iiif_base, ImageSize::Full);
            }) as Box<dyn FnOnce(&mut Sidecar)>,
            Box::new(|s: &mut Sidecar| s.sha256 = sha256(b"other")),
            Box::new(|s: &mut Sidecar| s.resolution = "scaled:50".into()),
            Box::new(|s: &mut Sidecar| {
                s.loc_url = format!("{ROOT}service:y/full/full/0/default.jpg")
            }),
        ] {
            let before = loc.0.load(Ordering::SeqCst);
            put(dir.path(), "keep", &key, "batch_x", b"keep bytes", edit);
            assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
            assert_eq!(loc.0.load(Ordering::SeqCst), before + 1);
        }
    }

    /// A JP2 fetched through the batch, with its sidecar.
    fn put_jp2(dir: &Path, set: &str, key: &PageKey, body: &[u8], edit: impl FnOnce(&mut Sidecar)) {
        let path = dir
            .join(set)
            .join("batch_x")
            .join(&key.lccn)
            .join(key.date.format("%Y-%m-%d").to_string())
            .join(key.edition.to_string())
            .join(format!("{}.jp2", key.seq));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        let base = format!("{ROOT}service:ndnp:x:batch_x_ver01:data:sn1:1:1890050301:0007");
        let mut side = Sidecar {
            doc_id: key.doc_id(),
            batch: "batch_x".into(),
            batch_version: 1,
            loc_url: format!("{STORAGE}ndnp/x/batch_x_ver01/data/sn1/1/1890050301/0007.jp2"),
            iiif_base: base,
            resolution: "full".into(),
            fetched_at: Utc::now(),
            bytes: body.len() as u64,
            sha256: sha256(body),
            download_ms: 1,
            user_agent: "x".into(),
            format: "jp2".into(),
            md5: Some(crate::batch::md5_hex(body)),
        };
        edit(&mut side);
        let mut sp = path.into_os_string();
        sp.push(".json");
        std::fs::write(sp, serde_json::to_vec(&side).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn reads_jp2s_and_jpegs_side_by_side() {
        let dir = tempfile::tempdir().unwrap();
        let base = server(dir.path()).await;
        let loc = Arc::new(Loc::default());
        let cache = CachedScans::new(&base, ROOT.into(), STORAGE.into(), loc.clone()).unwrap();
        let p1: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        let p2: PageKey = "sn1/1890-05-03/ed-1/seq-2".parse().unwrap();
        // Page 1 was fetched as a JPEG before the batch route, page 2 as a JP2.
        put(dir.path(), "keep", &p1, "batch_x", b"old jpeg", |_| {});
        put_jp2(dir.path(), "keep", &p2, b"jp2 bytes", |_| {});
        let s1 = cache.scan(&p1, "batch_x").await.unwrap();
        assert!(s1.from_cache);
        assert_eq!(
            (s1.format, &s1.bytes[..]),
            (ScanFormat::Jpeg, &b"old jpeg"[..])
        );
        assert!(s1.url.ends_with("/full/full/0/default.jpg") && s1.md5.is_none());
        let s2 = cache.scan(&p2, "batch_x").await.unwrap();
        assert!(s2.from_cache);
        assert_eq!(
            (s2.format, &s2.bytes[..]),
            (ScanFormat::Jp2, &b"jp2 bytes"[..])
        );
        assert!(s2.url.starts_with(STORAGE) && s2.url.ends_with("/0007.jp2"));
        assert_eq!(
            s2.md5.as_deref(),
            Some(crate::batch::md5_hex(b"jp2 bytes").as_str())
        );
        assert_eq!(loc.0.load(Ordering::SeqCst), 0);
        // Both there: the JP2.
        put_jp2(dir.path(), "keep", &p1, b"new jp2", |_| {});
        assert_eq!(
            cache.scan(&p1, "batch_x").await.unwrap().format,
            ScanFormat::Jp2
        );
        // A buffer JP2 is evicted by its own URL.
        let p3: PageKey = "sn1/1890-05-03/ed-1/seq-3".parse().unwrap();
        put_jp2(dir.path(), "buffer", &p3, b"buffer jp2", |_| {});
        let s3 = cache.scan(&p3, "batch_x").await.unwrap();
        assert!(s3
            .evict
            .unwrap()
            .ends_with("/scans/buffer/batch_x/sn1/1890-05-03/1/3.jp2"));
        // A JP2 whose md5, URL or format doesn't check out: LoC.
        for edit in [
            Box::new(|s: &mut Sidecar| s.md5 = Some(crate::batch::md5_hex(b"other")))
                as Box<dyn FnOnce(&mut Sidecar)>,
            Box::new(|s: &mut Sidecar| {
                s.loc_url = format!("{STORAGE}ndnp/x/batch_x_ver01/data/sn1/1/1890050301/0008.jp2")
            }),
            Box::new(|s: &mut Sidecar| s.loc_url = "https://evil.example/0007.jp2".into()),
            Box::new(|s: &mut Sidecar| s.format = "jpeg".into()),
            Box::new(|s: &mut Sidecar| s.md5 = None),
        ] {
            let before = loc.0.load(Ordering::SeqCst);
            put_jp2(dir.path(), "keep", &p2, b"jp2 bytes", edit);
            assert!(!cache.scan(&p2, "batch_x").await.unwrap().from_cache);
            assert_eq!(loc.0.load(Ordering::SeqCst), before + 1);
        }
    }

    #[test]
    fn refuses_a_bad_cache_url_at_startup() {
        let loc: Arc<dyn ScanSource> = Arc::new(Loc::default());
        for bad in ["http://", "http://host:99999", "ftp://host", "host:8380"] {
            assert!(
                CachedScans::new(bad, ROOT.into(), STORAGE.into(), Arc::clone(&loc)).is_err(),
                "{bad}"
            );
        }
        // A scan server by host name, and by an RFC 5737 documentation
        // address.
        for good in ["http://scans.example.org:8380", "http://192.0.2.5:8380"] {
            assert!(
                CachedScans::new(good, ROOT.into(), STORAGE.into(), Arc::clone(&loc)).is_ok(),
                "{good}"
            );
        }
    }

    #[tokio::test]
    async fn a_server_that_is_away_costs_seconds_not_a_thread() {
        let loc = Arc::new(Loc::default());
        // Nothing listens there.
        let cache = CachedScans::new(
            "http://127.0.0.1:9",
            ROOT.into(),
            STORAGE.into(),
            loc.clone(),
        )
        .unwrap();
        let key: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        let t0 = std::time::Instant::now();
        assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
        assert!(t0.elapsed() < CONNECT_TIMEOUT * 2);
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
    }
}
