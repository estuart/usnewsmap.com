//! Where the staging steps (`assess stage`, `surya stage`) get a page's scan.
//!
//! 1. The scan archive (ADR-0018), when `--scan-archive` names it: the blob
//!    `keep/<batch>/<lccn>/<date>/<edition>/<seq>.<jp2|jpg>`, checked
//!    against the sha256 (and, for a JP2, the md5) stored with it. The
//!    archive is ours, so it is not paced; any failure there (absent,
//!    refused, a bad checksum) falls through to LoC, logged.
//! 2. LoC's `tile.loc.gov`, through the batch route only: the batch's bag
//!    manifest and the issue's METS on the storage service (both kept in
//!    the batch cache) place the page and give its IIIF base. Nothing goes
//!    to www.loc.gov's item API.
//! 3. Neither: the page is missing.
//!
//! LoC politeness: one request at a time, at least `gap` (5 s or more)
//! between any two, the storage service and the image server sharing one
//! pacer; no retries. A 403, 429 or 5xx, a network failure, or an HTML page
//! where a file was expected (a bot challenge) is an error the caller turns
//! into a hold.
use bytes::Bytes;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::boinc::sha256;
use usnm_ocr_worker::{
    archive::Archive,
    batch::{self, jp2_url_of},
    loc::{image_url, BatchPage, FetchError, ImageSize, LocClient, LocConfig, ScanFormat},
};

pub const MIN_GAP: Duration = Duration::from_secs(5);

/// The LoC client the staging steps share: tile.loc.gov only, paced at
/// `gap`, one attempt, an HTML answer held as a block.
pub fn loc_client(gap: Duration, batch_cache: Option<PathBuf>) -> anyhow::Result<LocClient> {
    anyhow::ensure!(gap >= MIN_GAP, "--gap-secs must be at least 5");
    LocClient::new(LocConfig {
        api_gap: gap,
        tile_gap: gap,
        storage_gap: gap,
        storage_own_budget: false,
        attempts: 1,
        // Nothing more is sent after a 5xx: the run holds and stops.
        item_probes: false,
        html_is_block: true,
        batch_cache,
        ..LocConfig::default()
    })
}

/// The archive's read side, from `--scan-archive` (a container URL) and
/// the job's managed identity.
pub fn archive(url: Option<&str>) -> anyhow::Result<Option<Archive>> {
    url.filter(|u| !u.is_empty())
        .map(|u| Archive::new(u, usnm_store::credential::from_env()))
        .transpose()
}

/// The archive's name for a page's full-size scan.
pub fn blob_name(batch: &str, key: &PageKey, format: ScanFormat) -> String {
    format!(
        "keep/{batch}/{}/{}/{}/{}.{}",
        key.lccn,
        key.date.format("%Y-%m-%d"),
        key.edition,
        key.seq,
        format.ext()
    )
}

/// A scan, wherever it came from. `url` is always its tile.loc.gov URL
/// (provenance), `iiif_base` its IIIF service.
#[derive(Debug, Clone)]
pub struct Got {
    pub bytes: Bytes,
    pub url: String,
    pub iiif_base: String,
    pub from_archive: bool,
}

/// The two sources, in order.
pub struct Sources {
    pub loc: LocClient,
    pub archive: Option<Archive>,
    pub iiif_root: String,
    pub storage_root: String,
    pub max: usize,
}

/// A batch as `<name>_ver<NN>`, split; None if it isn't one.
pub fn split_batch(dir: &str) -> Option<(String, u16)> {
    let (name, v) = dir.rsplit_once("_ver")?;
    let v: u16 = v.parse().ok()?;
    (batch::batch_dir(name, v)? == dir).then(|| (name.to_owned(), v))
}

impl Sources {
    pub fn new(loc: LocClient, archive: Option<Archive>, max: usize) -> Self {
        let d = LocConfig::default();
        Self {
            loc,
            archive,
            iiif_root: d.iiif_root,
            storage_root: d.storage_root,
            max,
        }
    }

    /// The archive's copy of `name`, if it has one that checks out.
    async fn archived(&self, name: &str) -> Option<(Bytes, BTreeMap<String, String>)> {
        let a = self.archive.as_ref()?;
        match a.read(name, self.max).await {
            Ok(Some((b, meta))) => {
                if meta.get("sha256") != Some(&sha256(&b)) {
                    tracing_warn(name, "sha256 differs from the stored one");
                    return None;
                }
                Some((b, meta))
            }
            Ok(None) => None,
            Err(e) => {
                tracing_warn(name, &format!("{e:#}"));
                None
            }
        }
    }

    /// The IIIF base the archive stored for `name`, read from its
    /// properties only; None unless it is a IIIF base on tile.loc.gov.
    async fn archived_base(&self, name: &str) -> Option<String> {
        let a = self.archive.as_ref()?;
        match a.metadata_of(name).await {
            Ok(Some(meta)) => meta.get("iiif_base").filter(|b| self.base_ok(b)).cloned(),
            Ok(None) => None,
            Err(e) => {
                tracing_warn(name, &format!("{e:#}"));
                None
            }
        }
    }

    fn base_ok(&self, base: &str) -> bool {
        base.strip_prefix(&self.iiif_root)
            .and_then(|r| r.strip_prefix("service:"))
            .is_some_and(|r| {
                !r.is_empty()
                    && !r.contains("..")
                    && r.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'-' | b'.')
                    })
            })
    }

    /// The archive's full-size IIIF JPEG of the page in `batch`, if its
    /// stored URL is its stored base's JPEG.
    async fn archived_jpeg(&self, batch: &str, key: &PageKey) -> Option<Got> {
        let name = blob_name(batch, key, ScanFormat::Jpeg);
        let (bytes, meta) = self.archived(&name).await?;
        let base = meta.get("iiif_base").filter(|b| self.base_ok(b))?.clone();
        let url = image_url(&base, ImageSize::Full);
        if meta.get("loc_url") != Some(&url) {
            tracing_warn(&name, "stored URL is not its base's full JPEG");
            return None;
        }
        Some(Got {
            bytes,
            url,
            iiif_base: base,
            from_archive: true,
        })
    }

    /// The full-size IIIF JPEG of a page that is in one of `batches`
    /// (`(name, version)`, tried in order), for Surya. `Ok(None)`: no
    /// source has it.
    pub async fn jpeg(
        &self,
        key: &PageKey,
        batches: &[(String, u16)],
    ) -> Result<Option<Got>, FetchError> {
        for (b, _) in batches {
            if let Some(g) = self.archived_jpeg(b, key).await {
                return Ok(Some(g));
            }
        }
        // The archive's JP2 names the page's IIIF base: one image request.
        for (b, _) in batches {
            if let Some(base) = self
                .archived_base(&blob_name(b, key, ScanFormat::Jp2))
                .await
            {
                if let Some(g) = self.tile_jpeg(base).await? {
                    return Ok(Some(g));
                }
            }
        }
        for (b, v) in batches {
            match self.loc.resolve(key, b, *v).await {
                Ok(Some(p)) => {
                    // A 404 for this batch's image: another may still have it.
                    if let Some(g) = self.tile_jpeg(p.iiif_base).await? {
                        return Ok(Some(g));
                    }
                }
                Ok(None) | Err(FetchError::Permanent(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// The IIIF JPEG from tile.loc.gov; `None` for a 404.
    async fn tile_jpeg(&self, base: String) -> Result<Option<Got>, FetchError> {
        match self.loc.iiif_scan(base, ImageSize::Full).await {
            Ok(s) => Ok(Some(Got {
                bytes: s.bytes,
                url: s.url,
                iiif_base: s.iiif_base,
                from_archive: false,
            })),
            Err(FetchError::Permanent(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Where the page's JP2 is in its batch, with its md5 and IIIF base:
    /// from the archive's stored fields when it holds the JP2 (with its
    /// bytes, already checked), otherwise from the batch route.
    pub async fn jp2(
        &self,
        key: &PageKey,
        batch: &str,
        version: u16,
    ) -> Result<Option<(BatchPage, Got)>, FetchError> {
        let name = blob_name(batch, key, ScanFormat::Jp2);
        if let Some((bytes, meta)) = self.archived(&name).await {
            match self.page_of(&meta, &bytes) {
                Some(p) => {
                    let got = Got {
                        bytes,
                        url: p.url.clone(),
                        iiif_base: p.iiif_base.clone(),
                        from_archive: true,
                    };
                    return Ok(Some((p, got)));
                }
                None => tracing_warn(&name, "stored fields don't describe a JP2 of this batch"),
            }
        }
        let Some(p) = self.loc.resolve(key, batch, version).await? else {
            return Ok(None);
        };
        let Some(s) = self.loc.scan_resolved(key, &p, ImageSize::Full).await? else {
            return Ok(None);
        };
        let got = Got {
            bytes: s.bytes,
            url: s.url,
            iiif_base: s.iiif_base,
            from_archive: false,
        };
        Ok(Some((p, got)))
    }

    /// The archived JP2's place, if its stored URL is its stored base's JP2
    /// and its stored md5 matches the bytes.
    fn page_of(&self, meta: &BTreeMap<String, String>, bytes: &[u8]) -> Option<BatchPage> {
        let base = meta.get("iiif_base").filter(|b| self.base_ok(b))?;
        let url = meta.get("loc_url")?;
        // jp2_url_of wants the storage service's root, above `ndnp/`.
        let service_root = self.storage_root.strip_suffix("ndnp/")?;
        if jp2_url_of(&self.iiif_root, service_root, base).as_deref() != Some(url.as_str()) {
            return None;
        }
        let md5 = batch::parse_md5(meta.get("md5")?)?;
        (batch::md5(bytes) == md5).then(|| BatchPage {
            url: url.clone(),
            iiif_base: base.clone(),
            md5,
        })
    }

    /// The full-size IIIF JPEG of a page whose base is known (assessment
    /// OCR units): the archive's copy, else tile.loc.gov.
    pub async fn jpeg_of(
        &self,
        key: &PageKey,
        batch: &str,
        base: &str,
    ) -> Result<Option<Got>, FetchError> {
        if let Some(g) = self.archived_jpeg(batch, key).await {
            if g.iiif_base == base {
                return Ok(Some(g));
            }
        }
        self.tile_jpeg(base.to_owned()).await
    }
}

fn tracing_warn(name: &str, why: &str) {
    eprintln!("scan archive: {name}: {why}; going to LoC");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::State,
        http::{HeaderMap, Method, StatusCode},
        response::{IntoResponse, Response},
    };
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };
    use usnm_ocr_worker::{
        loc::iiif_base,
        standin::{Page as SPage, Scripted, StandIn},
    };
    use usnm_store::credential::{Credential, StaticToken};

    const BATCH: &str = "/storage-services/service/ndnp/xx/batch_xx_test_ver01/";
    const OTHER: &str = "/storage-services/service/ndnp/yy/batch_yy_other_ver02/";

    /// A blob's bytes and metadata.
    type Blob = (Vec<u8>, Vec<(String, String)>);

    /// A stand-in scan archive: blobs with metadata, every request counted.
    #[derive(Default)]
    struct Blobs {
        blobs: HashMap<String, Blob>,
        hits: Vec<(Method, String)>,
    }
    type Store = Arc<Mutex<Blobs>>;

    async fn blob(State(s): State<Store>, method: Method, uri: axum::http::Uri) -> Response {
        let name = uri.path().trim_start_matches("/scans/").to_owned();
        let mut s = s.lock().unwrap();
        s.hits.push((method.clone(), name.clone()));
        let Some((body, meta)) = s.blobs.get(&name).cloned() else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let mut h = HeaderMap::new();
        for (k, v) in meta {
            h.insert(
                axum::http::HeaderName::try_from(format!("x-ms-meta-{k}")).unwrap(),
                v.parse().unwrap(),
            );
        }
        if method == Method::HEAD {
            return (h, ()).into_response();
        }
        (h, body).into_response()
    }

    async fn archive_server() -> (Store, Archive) {
        let store = Store::default();
        let app = axum::Router::new().fallback(blob).with_state(store.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let a = Archive::new(
            &format!("http://{addr}/scans"),
            Arc::new(Credential::new(StaticToken("t".into()))),
        )
        .unwrap();
        (store, a)
    }

    fn sources(s: &StandIn, archive: Option<Archive>) -> Sources {
        let loc = LocClient::new(LocConfig {
            api_gap: Duration::from_millis(50),
            tile_gap: Duration::from_millis(50),
            storage_gap: Duration::from_millis(50),
            attempts: 1,
            backoff: Duration::from_millis(10),
            api_root: s.root.clone(),
            iiif_root: s.iiif_root(),
            storage_root: s.storage_root(),
            batches_root: s.batches_root(),
            item_probes: false,
            html_is_block: true,
            ..LocConfig::default()
        })
        .unwrap();
        Sources {
            loc,
            archive,
            iiif_root: s.iiif_root(),
            storage_root: s.storage_root(),
            max: 1 << 20,
        }
    }

    fn key() -> PageKey {
        PageKey::from_doc_id("sn1_1890-05-03_ed-1_seq-2").unwrap()
    }

    /// sn1's issue of 1890-05-03 in `batch_path`; the JP2 URL of seq 2.
    fn issue(s: &StandIn, batch_path: &str) -> String {
        s.add_issue(
            batch_path,
            "sn1",
            "0001A",
            "1890050301",
            &[
                SPage {
                    seq: 1,
                    frame: "0101",
                    jp2: b"jp2 one",
                },
                SPage {
                    seq: 2,
                    frame: "0099",
                    jp2: b"jp2 two",
                },
            ],
        );
        format!("{}{batch_path}data/sn1/0001A/1890050301/0099.jp2", s.root)
    }

    /// Puts the IIIF JPEG of `jp2` on the stand-in; its base.
    fn tile(s: &StandIn, jp2: &str, body: &[u8]) -> String {
        let base = iiif_base(&s.iiif_root(), jp2).unwrap();
        let url = image_url(&base, ImageSize::Full);
        s.put(url.strip_prefix(&s.root).unwrap(), body.to_vec());
        base
    }

    fn batches() -> Vec<(String, u16)> {
        vec![("yy_other".into(), 2), ("xx_test".into(), 1)]
    }

    fn put_blob(store: &Store, name: &str, body: &[u8], meta: &[(&str, &str)]) {
        let mut m: Vec<(String, String)> = meta
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        m.push(("sha256".into(), sha256(body)));
        store
            .lock()
            .unwrap()
            .blobs
            .insert(name.to_owned(), (body.to_vec(), m));
    }

    #[test]
    fn batches_split() {
        assert_eq!(split_batch("xx_test_ver01"), Some(("xx_test".into(), 1)));
        assert_eq!(split_batch("xx_test"), None);
        assert_eq!(split_batch("xx_test_ver1"), None);
        assert_eq!(split_batch("../x_ver01"), None);
    }

    #[tokio::test]
    async fn the_archive_jpeg_comes_first_and_loc_is_not_asked() {
        let s = StandIn::start().await;
        let jp2 = issue(&s, BATCH);
        let base = iiif_base(&s.iiif_root(), &jp2).unwrap();
        let url = image_url(&base, ImageSize::Full);
        let (store, a) = archive_server().await;
        let name = blob_name("xx_test", &key(), ScanFormat::Jpeg);
        assert_eq!(name, "keep/xx_test/sn1/1890-05-03/1/2.jpg");
        put_blob(
            &store,
            &name,
            b"held jpeg",
            &[("iiif_base", &base), ("loc_url", &url)],
        );
        let src = sources(&s, Some(a));
        let g = src.jpeg(&key(), &batches()).await.unwrap().unwrap();
        assert!(g.from_archive);
        assert_eq!(&g.bytes[..], b"held jpeg");
        assert_eq!(g.url, url);
        assert_eq!(s.hits(""), 0, "nothing went to LoC");
    }

    #[tokio::test]
    async fn a_bad_archive_copy_falls_through_to_loc() {
        let s = StandIn::start().await;
        let jp2 = issue(&s, BATCH);
        let base = tile(&s, &jp2, b"tile jpeg");
        let url = image_url(&base, ImageSize::Full);
        let (store, a) = archive_server().await;
        let name = blob_name("xx_test", &key(), ScanFormat::Jpeg);
        put_blob(
            &store,
            &name,
            b"held jpeg",
            &[("iiif_base", &base), ("loc_url", &url)],
        );
        // Damaged after it was stored.
        store.lock().unwrap().blobs.get_mut(&name).unwrap().0 = b"damaged".to_vec();
        let g = sources(&s, Some(a))
            .jpeg(&key(), &batches())
            .await
            .unwrap()
            .unwrap();
        assert!(!g.from_archive);
        assert_eq!(&g.bytes[..], b"tile jpeg");
    }

    #[tokio::test]
    async fn an_archived_jp2_gives_the_base_for_one_image_request() {
        let s = StandIn::start().await;
        let jp2 = issue(&s, BATCH);
        let base = tile(&s, &jp2, b"tile jpeg");
        let (store, a) = archive_server().await;
        put_blob(
            &store,
            &blob_name("xx_test", &key(), ScanFormat::Jp2),
            b"jp2 two",
            &[("iiif_base", &base), ("loc_url", &jp2)],
        );
        let g = sources(&s, Some(a))
            .jpeg(&key(), &batches())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&g.bytes[..], b"tile jpeg");
        assert_eq!(s.hits(""), 1, "only the image request");
        assert_eq!(s.hits("manifest-md5.txt"), 0);
        // The JP2 was only looked at, never read: Cold reads are billed.
        assert!(store
            .lock()
            .unwrap()
            .hits
            .iter()
            .all(|(m, n)| *m == Method::HEAD || !n.ends_with(".jp2")));
    }

    #[tokio::test]
    async fn without_the_archive_the_batch_route_places_the_page_and_no_item_api_is_asked() {
        let s = StandIn::start().await;
        // Only the second candidate batch has it; the first has no bag.
        let jp2 = issue(&s, BATCH);
        tile(&s, &jp2, b"tile jpeg");
        let src = sources(&s, None);
        let g = src.jpeg(&key(), &batches()).await.unwrap().unwrap();
        assert_eq!(&g.bytes[..], b"tile jpeg");
        assert_eq!(g.url, image_url(&g.iiif_base, ImageSize::Full));
        assert_eq!(s.hits("/batch_yy_other_ver02/manifest-md5.txt"), 1);
        assert_eq!(s.hits("/batch_xx_test_ver01/manifest-md5.txt"), 1);
        // The absent batch's Chronicling America lookup, then the METS and
        // the image: nothing else, and no item JSON (`/item/.../ed-1/`).
        assert_eq!(s.hits("/data/batches/yy_other_ver02/manifest-md5.txt"), 1);
        assert_eq!(s.hits("1890050301.xml"), 1);
        assert_eq!(s.hits(""), 5);
        assert_eq!(s.hits("ed-1/"), 0, "no item JSON");
        // Paced: one request at a time, the gap apart (less a little for
        // when the stand-in records each).
        let t = s.times("");
        let gaps: Vec<_> = t.windows(2).map(|w| w[1] - w[0]).collect();
        assert!(
            gaps.iter().all(|g| *g >= Duration::from_millis(40)),
            "{gaps:?}"
        );
    }

    #[tokio::test]
    async fn a_missing_image_moves_on_to_the_next_candidate() {
        let s = StandIn::start().await;
        // Both batches place the page; only the second's image is there,
        // and the archive's JP2 names the first's (absent) image.
        let first = issue(&s, OTHER);
        let first_base = iiif_base(&s.iiif_root(), &first).unwrap();
        let jp2 = issue(&s, BATCH);
        tile(&s, &jp2, b"tile jpeg");
        let (store, a) = archive_server().await;
        put_blob(
            &store,
            &blob_name("yy_other", &key(), ScanFormat::Jp2),
            b"jp2 two",
            &[("iiif_base", &first_base), ("loc_url", &first)],
        );
        let g = sources(&s, Some(a))
            .jpeg(&key(), &batches())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&g.bytes[..], b"tile jpeg");
        // The archived base's image, then the first batch's (the same), then the second's.
        assert_eq!(s.hits("default.jpg"), 3);
    }

    #[tokio::test]
    async fn no_source_is_missing() {
        let s = StandIn::start().await;
        let (_, a) = archive_server().await;
        let got = sources(&s, Some(a)).jpeg(&key(), &batches()).await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn a_challenge_page_is_a_block_not_a_decoding_error() {
        let s = StandIn::start().await;
        let jp2 = issue(&s, BATCH);
        let base = tile(&s, &jp2, b"tile jpeg");
        let path = image_url(&base, ImageSize::Full)
            .strip_prefix(&s.root)
            .unwrap()
            .to_owned();
        // A 200 with Cloudflare's "Just a moment..." page.
        s.script(&path, Scripted::Challenge(200));
        let src = sources(&s, None);
        let e = src
            .jpeg(&key(), &[("xx_test".into(), 1)])
            .await
            .unwrap_err();
        let m = e.to_string();
        assert!(matches!(e, FetchError::Transient(_)), "{m}");
        assert!(
            m.contains("Cloudflare challenge") && m.contains("holding"),
            "{m}"
        );
        assert!(src.loc.held_until().is_some());
        // A 403 challenge: a hold that says what it was.
        s.script(&path, Scripted::Challenge(403));
        let e = sources(&s, None)
            .jpeg(&key(), &[("xx_test".into(), 1)])
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("HTTP 403 (a Cloudflare challenge): blocked"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn the_assessment_jp2_comes_from_the_archive_when_its_md5_checks() {
        let s = StandIn::start().await;
        let jp2 = issue(&s, BATCH);
        let base = iiif_base(&s.iiif_root(), &jp2).unwrap();
        let (store, a) = archive_server().await;
        let md5 = usnm_ocr_worker::batch::md5_hex(b"jp2 two");
        put_blob(
            &store,
            &blob_name("xx_test", &key(), ScanFormat::Jp2),
            b"jp2 two",
            &[("iiif_base", &base), ("loc_url", &jp2), ("md5", &md5)],
        );
        let src = sources(&s, Some(a));
        let (p, g) = src.jp2(&key(), "xx_test", 1).await.unwrap().unwrap();
        assert!(g.from_archive);
        assert_eq!(p.url, jp2);
        assert_eq!(p.iiif_base, base);
        assert_eq!(s.hits(""), 0);
        // A wrong stored md5: the batch route, checked against the bag.
        let name = blob_name("xx_test", &key(), ScanFormat::Jp2);
        put_blob(
            &store,
            &name,
            b"jp2 two",
            &[
                ("iiif_base", &base),
                ("loc_url", &jp2),
                ("md5", &"0".repeat(32)),
            ],
        );
        let (_, g) = src.jp2(&key(), "xx_test", 1).await.unwrap().unwrap();
        assert!(!g.from_archive);
        assert_eq!(&g.bytes[..], b"jp2 two");
        assert_eq!(s.hits("0099.jp2"), 1);
    }
}
