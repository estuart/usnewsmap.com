//! Scans read from the local cache that `prefetch` fills (#368, ADR-0015
//! decision 9), before asking LoC.
//!
//! A page's full scan is at `<dir>/<batch>/<lccn>/<date>/<edition>/<seq>.jpg`
//! with its sidecar beside it (`prefetch::Sidecar`). It is used only when
//! the sidecar names this page and batch at full size, its URL is the IIIF
//! image URL of its base on the configured image server (tile.loc.gov), and
//! the file's size and sha256 match the sidecar. Then the result records
//! that base as `image_url`, so `ocr-sync`'s check on it holds, and the
//! worker records the scan's sha256 in the ALTO. Anything else, a missing
//! or partial file included, goes to LoC as before.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use usnm_core::ids::PageKey;

use crate::loc::{image_url, FetchError, ImageSize, Scan, MAX_IMAGE_BYTES};
use crate::prefetch::{page_path, sha256, sidecar_path, Sidecar};
use crate::worker::ScanSource;

/// Cache reads in flight at once. The cache is an NFS mount (`hard`), so
/// a read can block for as long as the server is away, and a blocking read
/// can't be cancelled: at most this many threads can be stuck that way, and
/// pages that find no free slot go to LoC.
pub const MAX_CACHE_READS: usize = 2;
/// How long a page waits for its cache read before it goes to LoC (the
/// read itself carries on, holding its slot, until it returns).
pub const CACHE_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The cache in front of another source (LoC).
#[derive(Debug)]
pub struct CachedScans {
    pub dir: PathBuf,
    /// Where IIIF bases must point (`https://tile.loc.gov/image-services/iiif/`).
    pub iiif_root: String,
    pub inner: Arc<dyn ScanSource>,
    /// Bounds the blocking cache reads ([`MAX_CACHE_READS`]).
    pub reads: Arc<tokio::sync::Semaphore>,
}

impl CachedScans {
    pub fn new(dir: PathBuf, iiif_root: String, inner: Arc<dyn ScanSource>) -> Self {
        Self {
            dir,
            iiif_root,
            inner,
            reads: Arc::new(tokio::sync::Semaphore::new(MAX_CACHE_READS)),
        }
    }
}

/// A file of at most `max` bytes.
fn read_bounded(path: &Path, max: usize) -> Option<Vec<u8>> {
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(max as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= max).then_some(buf)
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

impl CachedScans {
    /// The page's scan from the cache, if it is there and checks out.
    pub fn read(&self, key: &PageKey, batch: &str) -> Option<Scan> {
        let path = page_path(
            &self.dir,
            batch,
            &key.lccn,
            key.date,
            key.edition,
            key.seq,
            ImageSize::Full,
        );
        let side: Sidecar =
            serde_json::from_slice(&read_bounded(&sidecar_path(&path), 64 * 1024)?).ok()?;
        if side.doc_id != key.doc_id()
            || side.batch != batch
            || side.resolution != ImageSize::Full.to_string()
            || !base_ok(&self.iiif_root, &side.iiif_base)
            || side.loc_url != image_url(&side.iiif_base, ImageSize::Full)
        {
            return None;
        }
        let jpeg = read_bounded(&path, MAX_IMAGE_BYTES)?;
        if jpeg.len() as u64 != side.bytes || sha256(&jpeg) != side.sha256 {
            tracing::warn!(doc_id = %key.doc_id(), "a cached scan doesn't match its sidecar; fetching it");
            return None;
        }
        Some(Scan {
            iiif_base: side.iiif_base,
            jpeg: Bytes::from(jpeg),
            download: Duration::ZERO,
            from_cache: true,
        })
    }
}

#[async_trait]
impl ScanSource for CachedScans {
    async fn scan(&self, key: &PageKey, batch: &str) -> Result<Scan, FetchError> {
        let found = match Arc::clone(&self.reads).try_acquire_owned() {
            Ok(slot) => {
                let cache = CachedScans {
                    dir: self.dir.clone(),
                    iiif_root: self.iiif_root.clone(),
                    inner: Arc::clone(&self.inner),
                    reads: Arc::clone(&self.reads),
                };
                let (k, b) = (key.clone(), batch.to_owned());
                // The slot goes with the read, so a read stuck on the mount
                // keeps it until it returns.
                let read = tokio::task::spawn_blocking(move || {
                    let _slot = slot;
                    cache.read(&k, &b)
                });
                match tokio::time::timeout(CACHE_READ_TIMEOUT, read).await {
                    Ok(r) => r.ok().flatten(),
                    Err(_) => {
                        tracing::warn!(doc_id = %key.doc_id(), "the scan cache didn't answer; fetching");
                        None
                    }
                }
            }
            Err(_) => {
                tracing::warn!(doc_id = %key.doc_id(), "every scan cache read is busy; fetching");
                None
            }
        };
        match found {
            Some(s) => Ok(s),
            None => self.inner.scan(key, batch).await,
        }
    }

    fn held_until(&self) -> Option<tokio::time::Instant> {
        self.inner.held_until()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const ROOT: &str = "https://tile.loc.gov/image-services/iiif/";

    #[derive(Debug, Default)]
    struct Loc(AtomicU32);

    #[async_trait]
    impl ScanSource for Loc {
        async fn scan(&self, _key: &PageKey, _batch: &str) -> Result<Scan, FetchError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Scan {
                iiif_base: format!("{ROOT}service:from:loc"),
                jpeg: Bytes::from_static(b"loc"),
                download: Duration::ZERO,
                from_cache: false,
            })
        }
    }

    fn put(dir: &Path, key: &PageKey, batch: &str, body: &[u8], edit: impl FnOnce(&mut Sidecar)) {
        let path = page_path(
            dir,
            batch,
            &key.lccn,
            key.date,
            key.edition,
            key.seq,
            ImageSize::Full,
        );
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
        };
        edit(&mut side);
        std::fs::write(sidecar_path(&path), serde_json::to_vec(&side).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn goes_to_loc_when_every_cache_read_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let loc = Arc::new(Loc::default());
        let cache = CachedScans::new(dir.path().to_owned(), ROOT.into(), loc.clone());
        let key: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        put(dir.path(), &key, "batch_x", b"jpeg bytes", |_| {});
        // Reads stuck on the mount hold every slot.
        let _stuck = Arc::clone(&cache.reads)
            .acquire_many_owned(MAX_CACHE_READS as u32)
            .await
            .unwrap();
        assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn reads_a_verified_scan_and_falls_back_to_loc_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let loc = Arc::new(Loc::default());
        let cache = CachedScans::new(dir.path().to_owned(), ROOT.into(), loc.clone());
        let key: PageKey = "sn1/1890-05-03/ed-1/seq-1".parse().unwrap();
        // Nothing cached: LoC.
        let s = cache.scan(&key, "batch_x").await.unwrap();
        assert!(!s.from_cache);
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
        // Cached and matching.
        put(dir.path(), &key, "batch_x", b"jpeg bytes", |_| {});
        let s = cache.scan(&key, "batch_x").await.unwrap();
        assert!(s.from_cache);
        assert_eq!(&s.jpeg[..], b"jpeg bytes");
        assert!(s.iiif_base.ends_with(":0001"));
        assert_eq!(loc.0.load(Ordering::SeqCst), 1);
        // Another batch's copy isn't this one's.
        assert!(!cache.scan(&key, "batch_y").await.unwrap().from_cache);
        // A sidecar whose URL isn't on the image server, or doesn't match
        // the bytes, or names a scaled scan: LoC.
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
            put(dir.path(), &key, "batch_x", b"jpeg bytes", edit);
            assert!(!cache.scan(&key, "batch_x").await.unwrap().from_cache);
            assert_eq!(loc.0.load(Ordering::SeqCst), before + 1);
        }
    }
}
