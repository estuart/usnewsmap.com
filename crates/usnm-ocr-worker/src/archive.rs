//! Upload the keep-set scans to the scan archive (#442, ADR-0018): an Azure
//! Blob container on the Cold tier, written once and read by the BOINC API
//! and, if ever needed, to rebuild the local copy.
//!
//! One blob per scan, named like the file under the scans root
//! (`keep/<batch>/<lccn>/<date>/<edition>/<seq>.jp2`). The sidecar's fields
//! become the blob's metadata. Before a scan goes up it is checked against
//! its sidecar (size, sha256, and the bag's md5 for a JP2); the request
//! carries `Content-MD5`, so the service rejects bytes damaged in transit.
//! A blob that already exists is never written again: if it matches the
//! local file it is counted as present, if not it is reported as a
//! conflict. `verify` reads properties only, never the bytes, because a
//! Cold read is billed per GB.
//!
//! Access is by Entra ID only (`usnm_store::credential`), as for every
//! account here: no key, no SAS.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use base64::Engine as _;
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use usnm_store::credential::Credential;

use crate::batch;
use crate::prefetch::{matches_sidecar, sidecar_path, Sidecar};

/// Blob service REST API version, as `usnm_store::blob`.
const API_VERSION: &str = "2023-11-03";
const TIER: &str = "Cold";

/// What to do with each scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Upload what is missing.
    Upload,
    /// Say what an upload would do, writing nothing.
    DryRun,
    /// Compare each blob's properties with the local file; write nothing.
    Verify,
}

pub struct Options {
    /// The scans root, holding `keep/`.
    pub scans: PathBuf,
    /// The set to archive: `keep` (the buffer set is deleted after OCR and
    /// would only pay the Cold early-delete charge).
    pub set: String,
    /// `https://<account>.blob.core.windows.net/<container>`.
    pub container_url: String,
    pub mode: Mode,
    /// Stop after this many scans (0: all).
    pub limit: usize,
    /// Scans in flight.
    pub concurrency: usize,
    /// A file of the scans already archived (name, size, mtime). A scan
    /// listed there with the same size and mtime is skipped without being
    /// read or looked up, so a run with nothing new costs no requests.
    /// Verify ignores it and checks every scan.
    pub state: Option<PathBuf>,
}

/// What a run did, printed as JSON.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub scanned: u64,
    /// Recorded as archived in the state file and unchanged: not read.
    pub skipped: u64,
    pub uploaded: u64,
    pub uploaded_bytes: u64,
    /// Already in the archive and matching.
    pub present: u64,
    /// Would be uploaded (dry run) .
    pub would_upload: u64,
    /// Local file does not match its sidecar, or has none: not uploaded.
    pub bad_local: u64,
    /// A blob of that name exists and differs from the local file.
    pub conflicts: u64,
    /// Verify: not in the archive.
    pub missing: u64,
    /// Verify: in the archive with different size, md5 or tier.
    pub mismatched: u64,
    /// Failed requests (the run goes on; run it again).
    pub errors: u64,
    /// The first few problems, for the log.
    pub problems: Vec<String>,
}

impl Report {
    fn add(&mut self, o: Outcome, name: &str, bytes: u64) {
        let mut problem = |what: &str| {
            if self.problems.len() < 20 {
                self.problems.push(format!("{what}: {name}"));
            }
        };
        match o {
            Outcome::Uploaded => {
                self.uploaded += 1;
                self.uploaded_bytes += bytes;
            }
            Outcome::Present => self.present += 1,
            Outcome::WouldUpload => self.would_upload += 1,
            Outcome::BadLocal => {
                self.bad_local += 1;
                problem("local file does not match its sidecar");
            }
            Outcome::Conflict => {
                self.conflicts += 1;
                problem("a different blob has this name");
            }
            Outcome::Missing => {
                self.missing += 1;
                problem("not in the archive");
            }
            Outcome::Mismatch(why) => {
                self.mismatched += 1;
                problem(&why);
            }
            Outcome::Error(e) => {
                self.errors += 1;
                problem(&e);
            }
        }
        self.scanned += 1;
    }

    /// Whether everything seen is in the archive and right.
    pub fn clean(&self) -> bool {
        self.bad_local + self.conflicts + self.missing + self.mismatched + self.errors == 0
    }
}

#[derive(Debug)]
enum Outcome {
    Uploaded,
    Present,
    WouldUpload,
    BadLocal,
    Conflict,
    Missing,
    Mismatch(String),
    Error(String),
}

/// What HEAD says about a blob.
#[derive(Debug)]
struct Props {
    size: u64,
    md5_b64: Option<String>,
    tier: Option<String>,
    /// The `sha256` metadata the uploader stored.
    sha256: Option<String>,
}

pub struct Archive {
    base: String,
    credential: Arc<Credential>,
    http: reqwest::Client,
}

impl Archive {
    /// `container_url` must be an Azure Blob container URL: https, a blob
    /// host, one path segment, no query (no SAS), no credentials. Plain
    /// http is allowed for a loopback test server only.
    pub fn new(container_url: &str, credential: Arc<Credential>) -> anyhow::Result<Self> {
        let url = Url::parse(container_url).context("container url")?;
        let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
        let blob_host = url.host_str().is_some_and(|h| {
            let h = h.to_ascii_lowercase();
            usnm_store::blob::BLOB_HOST_SUFFIXES
                .iter()
                .any(|s| h.len() > s.len() && h.ends_with(s))
        });
        match url.scheme() {
            "https" if blob_host || loopback => {}
            "http" if loopback => {}
            _ => {
                bail!("the archive must be an https Azure Blob endpoint (*.blob.core.windows.net)")
            }
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("the archive url must not carry a query (SAS tokens are not used)");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("the archive url must not carry credentials");
        }
        let container = url.path().trim_matches('/');
        if container.is_empty() || container.contains('/') {
            bail!("the archive url must name exactly one container");
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(600))
            // The bearer token goes to the configured host and nowhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            base: format!("{}/{container}", url.origin().ascii_serialization()),
            credential,
            http,
        })
    }

    async fn request(&self, method: Method, name: &str) -> anyhow::Result<reqwest::RequestBuilder> {
        // The same rule as `usnm_store::blob::BlobStore`: plain segments only.
        usnm_store::validate_path(name)
            .map_err(|_| anyhow::anyhow!("not a plain blob path: {name}"))?;
        let token = self.credential.token().await?;
        Ok(self
            .http
            .request(method, format!("{}/{}", self.base, encode_path(name)))
            .bearer_auth(token)
            .header("x-ms-version", API_VERSION)
            .header(
                "x-ms-date",
                httpdate::fmt_http_date(std::time::SystemTime::now()),
            ))
    }

    async fn head(&self, name: &str) -> anyhow::Result<Option<Props>> {
        let resp = self.request(Method::HEAD, name).await?.send().await?;
        match resp.status() {
            StatusCode::NOT_FOUND => Ok(None),
            StatusCode::OK => {
                let h = resp.headers();
                let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_owned);
                Ok(Some(Props {
                    size: get("content-length")
                        .and_then(|v| v.parse().ok())
                        .context("no content-length")?,
                    md5_b64: get("content-md5"),
                    tier: get("x-ms-access-tier"),
                    sha256: get("x-ms-meta-sha256"),
                }))
            }
            s => bail!("head returned {s}"),
        }
    }

    /// Create the blob, only if there is none, on the Cold tier.
    async fn put(&self, name: &str, body: Vec<u8>, side: &Sidecar) -> anyhow::Result<PutResult> {
        let md5 = base64::engine::general_purpose::STANDARD.encode(batch::md5(&body));
        let content_type = if side.format == "jp2" {
            "image/jp2"
        } else {
            "image/jpeg"
        };
        let mut req = self
            .request(Method::PUT, name)
            .await?
            .header("x-ms-blob-type", "BlockBlob")
            .header("x-ms-access-tier", TIER)
            .header("If-None-Match", "*")
            .header("Content-MD5", &md5)
            .header("x-ms-blob-content-md5", &md5)
            .header("x-ms-blob-content-type", content_type)
            .header(
                "x-ms-blob-cache-control",
                "public, max-age=31536000, immutable",
            );
        for (k, v) in metadata(side) {
            req = req.header(format!("x-ms-meta-{k}"), v);
        }
        let resp = req.body(body).send().await?;
        match resp.status() {
            StatusCode::CREATED => Ok(PutResult::Created),
            // Someone wrote it first: BlobAlreadyExists.
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED => Ok(PutResult::Exists),
            s => bail!("put returned {s}"),
        }
    }

    async fn one(&self, scan: &Path, name: &str, mode: Mode) -> (Outcome, u64) {
        match self.one_inner(scan, name, mode).await {
            Ok(r) => r,
            Err(e) => (Outcome::Error(format!("{e:#} ({name})")), 0),
        }
    }

    async fn one_inner(
        &self,
        scan: &Path,
        name: &str,
        mode: Mode,
    ) -> anyhow::Result<(Outcome, u64)> {
        let side = match tokio::fs::read(sidecar_path(scan)).await {
            Ok(b) => serde_json::from_slice::<Sidecar>(&b).ok(),
            Err(_) => None,
        };
        let bytes = tokio::fs::read(scan)
            .await
            .with_context(|| format!("reading {}", scan.display()))?;
        // The sidecar must describe this file: its extension's format and
        // the full resolution, as `prefetch::verified` requires.
        let ext = scan.extension().and_then(|e| e.to_str()).unwrap_or("");
        let Some(side) = side
            .filter(|s| s.resolution == "full" && s.format == ext && matches_sidecar(&bytes, s))
        else {
            return Ok((Outcome::BadLocal, 0));
        };
        let size = bytes.len() as u64;
        let local_md5 = base64::engine::general_purpose::STANDARD.encode(batch::md5(&bytes));
        let existing = self.head(name).await?;
        if let Some(p) = &existing {
            return Ok((compare(p, size, &local_md5, &side.sha256, mode), size));
        }
        match mode {
            Mode::Verify => Ok((Outcome::Missing, size)),
            Mode::DryRun => Ok((Outcome::WouldUpload, size)),
            Mode::Upload => match self.put(name, bytes, &side).await? {
                PutResult::Created => Ok((Outcome::Uploaded, size)),
                PutResult::Exists => {
                    // Written between our HEAD and our PUT.
                    let p = self.head(name).await?.context("blob vanished")?;
                    Ok((
                        compare(&p, size, &local_md5, &side.sha256, Mode::Upload),
                        size,
                    ))
                }
            },
        }
    }
}

enum PutResult {
    Created,
    Exists,
}

/// An existing blob against the local file. In verify mode the tier must
/// also be Cold; elsewhere it is left alone (the lifecycle rule moves a
/// stray blob to Cold, and a rewrite would cost an early delete).
fn compare(p: &Props, size: u64, local_md5: &str, local_sha256: &str, mode: Mode) -> Outcome {
    if p.size != size {
        return if mode == Mode::Verify {
            Outcome::Mismatch(format!("size {} in the archive, {size} here", p.size))
        } else {
            Outcome::Conflict
        };
    }
    // The stored MD5 must be there and equal in every mode: a same-sized
    // blob without one is not known to hold these bytes.
    match p.md5_b64.as_deref() {
        Some(m) if m == local_md5 => {}
        other => {
            return if mode == Mode::Verify {
                Outcome::Mismatch(if other.is_none() {
                    "no stored md5".into()
                } else {
                    "md5 differs".into()
                })
            } else {
                Outcome::Conflict
            };
        }
    }
    if mode == Mode::Verify {
        if p.tier.as_deref() != Some(TIER) {
            return Outcome::Mismatch(format!(
                "tier is {}",
                p.tier.as_deref().unwrap_or("unknown")
            ));
        }
        match p.sha256.as_deref() {
            None => return Outcome::Mismatch("no stored sha256 metadata".into()),
            Some(m) if !m.eq_ignore_ascii_case(local_sha256) => {
                return Outcome::Mismatch("sha256 metadata differs".into())
            }
            Some(_) => {}
        }
    }
    Outcome::Present
}

/// The sidecar's fields as blob metadata: names are identifiers, values
/// plain ASCII.
fn metadata(side: &Sidecar) -> BTreeMap<&'static str, String> {
    let mut m = BTreeMap::new();
    m.insert("doc_id", side.doc_id.clone());
    m.insert("batch", side.batch.clone());
    m.insert("batch_version", side.batch_version.to_string());
    m.insert("loc_url", side.loc_url.clone());
    m.insert("iiif_base", side.iiif_base.clone());
    m.insert("resolution", side.resolution.clone());
    m.insert("fetched_at", side.fetched_at.to_rfc3339());
    m.insert("bytes", side.bytes.to_string());
    m.insert("sha256", side.sha256.clone());
    m.insert("format", side.format.clone());
    if let Some(md5) = &side.md5 {
        m.insert("md5", md5.clone());
    }
    m.retain(|_, v| v.is_ascii() && !v.contains(['\r', '\n']));
    m
}

/// A blob name as a URL path: the segments are already plain, but encode
/// anything outside the unreserved set.
fn encode_path(name: &str) -> String {
    name.split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// The scans under `<scans>/<set>`: full-size `.jp2` and `.jpg` files, in
/// path order, with their blob names. Scaled sample files (`.pct<n>.jpg`),
/// sidecars and consumed markers are left out.
pub fn list(scans: &Path, set: &str) -> anyhow::Result<Vec<(PathBuf, String)>> {
    if !matches!(set, "keep" | "buffer") {
        bail!("--set must be keep or buffer");
    }
    let root = scans.join(set);
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            // A directory that vanishes mid-walk is skipped; a missing
            // root is a wrong path or an unmounted pool, not an empty set.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir != root => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        for entry in rd {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(path);
            } else if ty.is_file() && is_scan(&path) {
                let rel = path.strip_prefix(scans)?;
                let name = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((path, name));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

/// What identifies one version of a local file: size, mtime in
/// nanoseconds, inode and ctime in nanoseconds. A same-sized replacement
/// (written to a new file and renamed over the old one) changes the inode,
/// ctime and mtime.
type Stamp = (u64, u64, u64, u64);

/// The archived-scans file: one `name<TAB>size<TAB>mtime-ns<TAB>inode<TAB>
/// ctime-ns` per line. A missing file is an empty state; a line in any other
/// shape (the earlier three-field form, or a damaged one) is ignored, so
/// that scan is checked against the archive again, which is safe.
fn load_state(path: &Path) -> anyhow::Result<HashMap<String, Stamp>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut m = HashMap::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if let [n, a, b, c, d] = f[..] {
            if let (Ok(a), Ok(b), Ok(c), Ok(d)) = (a.parse(), b.parse(), c.parse(), d.parse()) {
                m.insert(n.to_owned(), (a, b, c, d));
            }
        }
    }
    Ok(m)
}

/// The stamp of a scan's file.
fn stat(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::metadata(path).ok()?;
    let ns = |sec: i64, nsec: i64| {
        u64::try_from(sec)
            .ok()?
            .checked_mul(1_000_000_000)?
            .checked_add(u64::try_from(nsec).ok()?)
    };
    Some((
        md.len(),
        ns(md.mtime(), md.mtime_nsec())?,
        md.ino(),
        ns(md.ctime(), md.ctime_nsec())?,
    ))
}

fn is_scan(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    (name.ends_with(".jp2") || name.ends_with(".jpg")) && !name.contains(".pct")
}

/// Run over the whole set (or `limit` scans), `concurrency` at a time.
pub async fn run(opts: Options, credential: Arc<Credential>) -> anyhow::Result<Report> {
    let archive = Arc::new(Archive::new(&opts.container_url, credential)?);
    let mut files = list(&opts.scans, &opts.set)?;
    if opts.limit > 0 {
        files.truncate(opts.limit);
    }
    let mut report = Report::default();
    // Verify checks everything; the other modes skip what is recorded.
    let use_state = opts.mode != Mode::Verify;
    let done_before = match (&opts.state, use_state) {
        (Some(p), true) => load_state(p)?,
        _ => HashMap::new(),
    };
    let mut state_out = match (&opts.state, opts.mode) {
        (Some(p), Mode::Upload) => {
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .with_context(|| format!("opening {}", p.display()))?,
            )
        }
        _ => None,
    };
    let mut tasks = tokio::task::JoinSet::new();
    let mut next = files.into_iter();
    let width = opts.concurrency.max(1);
    loop {
        while tasks.len() < width {
            let Some((path, name)) = next.next() else {
                break;
            };
            let st = stat(&path);
            if st.is_some() && done_before.get(&name) == st.as_ref() {
                report.skipped += 1;
                continue;
            }
            let a = archive.clone();
            let mode = opts.mode;
            tasks.spawn(async move {
                let (o, bytes) = a.one(&path, &name, mode).await;
                (o, name, bytes, st)
            });
        }
        let Some(done) = tasks.join_next().await else {
            break;
        };
        let (o, name, bytes, st) = done?;
        if let (Some(f), Some((size, mtime, ino, ctime)), Outcome::Uploaded | Outcome::Present) =
            (state_out.as_mut(), st, &o)
        {
            // A failed append only means the scan is looked up again.
            let _ = writeln!(f, "{name}\t{size}\t{mtime}\t{ino}\t{ctime}");
        }
        report.add(o, &name, bytes);
        if report.scanned % 1000 == 0 {
            tracing::info!(
                scanned = report.scanned,
                uploaded = report.uploaded,
                "archive progress"
            );
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prefetch::sha256;
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method as M, StatusCode as S, Uri};
    use axum::response::{IntoResponse, Response};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use usnm_store::credential::StaticToken;

    #[derive(Clone, Debug)]
    struct Stored {
        body: Vec<u8>,
        headers: HashMap<String, String>,
    }

    #[derive(Default)]
    struct Mock {
        blobs: Mutex<HashMap<String, Stored>>,
        puts: Mutex<u32>,
    }

    async fn handler(
        State(m): State<Arc<Mock>>,
        method: M,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer tok"
        );
        let name = uri.path().trim_start_matches('/').to_owned();
        let mut blobs = m.blobs.lock().unwrap();
        match method {
            M::HEAD => match blobs.get(&name) {
                None => S::NOT_FOUND.into_response(),
                Some(b) => {
                    let mut r = Response::builder()
                        .status(200)
                        .header("content-length", b.body.len())
                        .header("x-ms-access-tier", b.headers["x-ms-access-tier"].clone());
                    if let Some(md5) = b.headers.get("content-md5") {
                        r = r.header("content-md5", md5);
                    }
                    if let Some(sha) = b.headers.get("x-ms-meta-sha256") {
                        r = r.header("x-ms-meta-sha256", sha);
                    }
                    r.body(axum::body::Body::empty()).unwrap()
                }
            },
            M::PUT => {
                *m.puts.lock().unwrap() += 1;
                if blobs.contains_key(&name) && headers.get("if-none-match").is_some() {
                    return S::CONFLICT.into_response();
                }
                // The service checks Content-MD5 against the bytes.
                let want = headers.get("content-md5").unwrap().to_str().unwrap();
                let got = base64::engine::general_purpose::STANDARD.encode(batch::md5(&body));
                if want != got {
                    return S::BAD_REQUEST.into_response();
                }
                let h = headers
                    .iter()
                    .map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap().to_owned()))
                    .collect();
                blobs.insert(
                    name,
                    Stored {
                        body: body.to_vec(),
                        headers: h,
                    },
                );
                S::CREATED.into_response()
            }
            _ => S::METHOD_NOT_ALLOWED.into_response(),
        }
    }

    async fn serve(m: Arc<Mock>) -> String {
        let app = axum::Router::new().fallback(handler).with_state(m);
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}/scans")
    }

    fn cred() -> Arc<Credential> {
        Arc::new(Credential::new(StaticToken("tok".into())))
    }

    fn put_scan(root: &Path, rel: &str, bytes: &[u8], good: bool) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let side = Sidecar {
            doc_id: "d".into(),
            batch: "b".into(),
            batch_version: 1,
            loc_url: "https://tile.loc.gov/storage-services/x.jp2".into(),
            iiif_base: "https://tile.loc.gov/image-services/iiif/x".into(),
            resolution: "full".into(),
            fetched_at: "2026-10-01T00:00:00Z".parse().unwrap(),
            bytes: bytes.len() as u64,
            sha256: if good { sha256(bytes) } else { "0".repeat(64) },
            download_ms: 1,
            user_agent: "t".into(),
            format: "jp2".into(),
            md5: Some(batch::md5_hex(bytes)),
        };
        std::fs::write(sidecar_path(&path), serde_json::to_vec(&side).unwrap()).unwrap();
    }

    fn opts(dir: &Path, url: String, mode: Mode) -> Options {
        Options {
            scans: dir.to_owned(),
            set: "keep".into(),
            container_url: url,
            mode,
            limit: 0,
            concurrency: 2,
            state: None,
        }
    }

    #[tokio::test]
    async fn uploads_once_cold_with_metadata_and_skips_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(
            dir.path(),
            "keep/b1/sn1/1890-01-01/ed-1/seq-1.jp2",
            b"one",
            true,
        );
        put_scan(
            dir.path(),
            "keep/b1/sn1/1890-01-01/ed-1/seq-2.jp2",
            b"two!",
            true,
        );
        // Left out: a bad sidecar (counted), a scaled sample and a buffer scan.
        put_scan(
            dir.path(),
            "keep/b1/sn1/1890-01-01/ed-1/seq-3.jp2",
            b"bad",
            false,
        );
        put_scan(
            dir.path(),
            "keep/b1/sn1/1890-01-01/ed-1/seq-4.pct50.jpg",
            b"s",
            true,
        );
        put_scan(
            dir.path(),
            "buffer/b1/sn1/1890-01-01/ed-1/seq-5.jp2",
            b"buf",
            true,
        );
        let m = Arc::new(Mock::default());
        let url = serve(m.clone()).await;

        let r = run(opts(dir.path(), url.clone(), Mode::Upload), cred())
            .await
            .unwrap();
        assert_eq!(
            (r.scanned, r.uploaded, r.uploaded_bytes, r.bad_local),
            (3, 2, 7, 1)
        );
        assert!(!r.clean());
        let blobs = m.blobs.lock().unwrap().clone();
        let b = &blobs["scans/keep/b1/sn1/1890-01-01/ed-1/seq-1.jp2"];
        assert_eq!(b.body, b"one");
        assert_eq!(b.headers["x-ms-access-tier"], "Cold");
        assert_eq!(b.headers["if-none-match"], "*");
        assert_eq!(b.headers["x-ms-blob-type"], "BlockBlob");
        assert_eq!(b.headers["x-ms-meta-sha256"], sha256(b"one"));
        assert_eq!(b.headers["x-ms-blob-content-type"], "image/jp2");
        assert!(!blobs
            .keys()
            .any(|k| k.contains("seq-5") || k.contains("pct50")));

        // A second run writes nothing.
        let r = run(opts(dir.path(), url.clone(), Mode::Upload), cred())
            .await
            .unwrap();
        assert_eq!((r.uploaded, r.present), (0, 2));
        assert_eq!(*m.puts.lock().unwrap(), 2);

        // Verify reads properties only and passes for the two good scans.
        let r = run(opts(dir.path(), url, Mode::Verify), cred())
            .await
            .unwrap();
        assert_eq!(
            (r.present, r.mismatched, r.missing, r.bad_local),
            (2, 0, 0, 1)
        );
    }

    #[tokio::test]
    async fn never_overwrites_a_different_blob() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"new bytes", true);
        let m = Arc::new(Mock::default());
        m.blobs.lock().unwrap().insert(
            "scans/keep/b/s/d/e/seq-1.jp2".into(),
            Stored {
                body: b"old".to_vec(),
                headers: HashMap::from([("x-ms-access-tier".into(), "Cold".into())]),
            },
        );
        let url = serve(m.clone()).await;
        let r = run(opts(dir.path(), url, Mode::Upload), cred())
            .await
            .unwrap();
        assert_eq!((r.conflicts, r.uploaded), (1, 0));
        assert_eq!(*m.puts.lock().unwrap(), 0);
        assert_eq!(
            m.blobs.lock().unwrap().values().next().unwrap().body,
            b"old"
        );
    }

    #[tokio::test]
    async fn dry_run_and_verify_write_nothing() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let m = Arc::new(Mock::default());
        let url = serve(m.clone()).await;
        let r = run(opts(dir.path(), url.clone(), Mode::DryRun), cred())
            .await
            .unwrap();
        assert_eq!(r.would_upload, 1);
        let r = run(opts(dir.path(), url, Mode::Verify), cred())
            .await
            .unwrap();
        assert_eq!(r.missing, 1);
        assert_eq!(*m.puts.lock().unwrap(), 0);
        assert!(m.blobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn verify_flags_a_warm_tier() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let m = Arc::new(Mock::default());
        let md5 = base64::engine::general_purpose::STANDARD.encode(batch::md5(b"abc"));
        m.blobs.lock().unwrap().insert(
            "scans/keep/b/s/d/e/seq-1.jp2".into(),
            Stored {
                body: b"abc".to_vec(),
                headers: HashMap::from([
                    ("x-ms-access-tier".into(), "Hot".into()),
                    ("content-md5".into(), md5),
                ]),
            },
        );
        let url = serve(m).await;
        let r = run(opts(dir.path(), url, Mode::Verify), cred())
            .await
            .unwrap();
        assert_eq!(r.mismatched, 1);
        assert!(r.problems[0].contains("tier is Hot"));
    }

    async fn verify_with_meta(sha: Option<String>) -> Report {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let m = Arc::new(Mock::default());
        let md5 = base64::engine::general_purpose::STANDARD.encode(batch::md5(b"abc"));
        let mut headers = HashMap::from([
            ("x-ms-access-tier".into(), "Cold".into()),
            ("content-md5".into(), md5),
        ]);
        if let Some(s) = sha {
            headers.insert("x-ms-meta-sha256".into(), s);
        }
        m.blobs.lock().unwrap().insert(
            "scans/keep/b/s/d/e/seq-1.jp2".into(),
            Stored {
                body: b"abc".to_vec(),
                headers,
            },
        );
        let url = serve(m).await;
        run(opts(dir.path(), url, Mode::Verify), cred())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn verify_checks_the_sha256_metadata() {
        let r = verify_with_meta(None).await;
        assert_eq!(r.mismatched, 1);
        assert!(r.problems[0].contains("no stored sha256"));
        let r = verify_with_meta(Some("f".repeat(64))).await;
        assert_eq!(r.mismatched, 1);
        assert!(r.problems[0].contains("sha256 metadata differs"));
        let r = verify_with_meta(Some(sha256(b"abc"))).await;
        assert_eq!((r.present, r.mismatched), (1, 0));
    }

    #[tokio::test]
    async fn a_same_sized_blob_without_md5_is_not_present_or_cached() {
        for mode in [Mode::Upload, Mode::DryRun] {
            let dir = tempfile::tempdir().unwrap();
            put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
            let m = Arc::new(Mock::default());
            m.blobs.lock().unwrap().insert(
                "scans/keep/b/s/d/e/seq-1.jp2".into(),
                Stored {
                    body: b"xyz".to_vec(),
                    headers: HashMap::from([("x-ms-access-tier".into(), "Cold".into())]),
                },
            );
            let url = serve(m.clone()).await;
            let state = dir.path().join("state.tsv");
            let mut o = opts(dir.path(), url, mode);
            o.state = Some(state.clone());
            let r = run(o, cred()).await.unwrap();
            assert_eq!((r.present, r.conflicts, r.uploaded), (0, 1, 0), "{mode:?}");
            assert_eq!(*m.puts.lock().unwrap(), 0);
            let cached = std::fs::read_to_string(&state).unwrap_or_default();
            assert!(cached.is_empty(), "{mode:?}: {cached}");
        }
    }

    #[tokio::test]
    async fn a_sidecar_must_match_the_extension_and_be_full_size() {
        let dir = tempfile::tempdir().unwrap();
        // A jp2 file whose sidecar says jpg.
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let p = dir.path().join("keep/b/s/d/e/seq-1.jp2");
        let mut side: Sidecar =
            serde_json::from_slice(&std::fs::read(sidecar_path(&p)).unwrap()).unwrap();
        side.format = "jpg".into();
        std::fs::write(sidecar_path(&p), serde_json::to_vec(&side).unwrap()).unwrap();
        // A sidecar for a scaled copy.
        put_scan(dir.path(), "keep/b/s/d/e/seq-2.jp2", b"abd", true);
        let p2 = dir.path().join("keep/b/s/d/e/seq-2.jp2");
        let mut side: Sidecar =
            serde_json::from_slice(&std::fs::read(sidecar_path(&p2)).unwrap()).unwrap();
        side.resolution = "pct50".into();
        std::fs::write(sidecar_path(&p2), serde_json::to_vec(&side).unwrap()).unwrap();
        let m = Arc::new(Mock::default());
        let url = serve(m.clone()).await;
        let r = run(opts(dir.path(), url, Mode::Upload), cred())
            .await
            .unwrap();
        assert_eq!((r.bad_local, r.uploaded), (2, 0));
        assert!(m.blobs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_same_size_replacement_in_the_same_second_is_rechecked() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let m = Arc::new(Mock::default());
        let url = serve(m.clone()).await;
        let state = dir.path().join("archived.tsv");
        let go = |mode| {
            let mut o = opts(dir.path(), url.clone(), mode);
            o.state = Some(state.clone());
            o
        };
        let r = run(go(Mode::Upload), cred()).await.unwrap();
        assert_eq!(r.uploaded, 1);
        // Replace with other bytes of the same length, through a rename.
        let tmp = dir.path().join("new.jp2");
        put_scan(dir.path(), "new.jp2", b"xyz", true);
        let live = dir.path().join("keep/b/s/d/e/seq-1.jp2");
        std::fs::rename(&tmp, &live).unwrap();
        std::fs::rename(sidecar_path(&tmp), sidecar_path(&live)).unwrap();
        let r = run(go(Mode::Upload), cred()).await.unwrap();
        assert_eq!(r.skipped, 0);
        // Looked up again: the blob differs, so it is reported.
        assert_eq!(r.conflicts, 1);
    }

    #[test]
    fn old_three_field_state_lines_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.tsv");
        std::fs::write(&p, "keep/a.jp2\t3\t100\nkeep/b.jp2\t3\t1\t2\t3\n").unwrap();
        let m = load_state(&p).unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m["keep/b.jp2"], (3, 1, 2, 3));
    }

    #[tokio::test]
    async fn a_missing_scans_root_fails_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let m = Arc::new(Mock::default());
        let url = serve(m).await;
        // No keep/ directory at all.
        assert!(run(opts(dir.path(), url, Mode::Upload), cred())
            .await
            .is_err());
        assert!(list(&dir.path().join("nope"), "keep").is_err());
    }

    #[tokio::test]
    async fn requests_reject_names_the_store_would() {
        let a = Archive::new("https://acct.blob.core.windows.net/scans", cred()).unwrap();
        for bad in ["keep/../x.jp2", "keep//x.jp2", "/keep/x.jp2", "keep/./x"] {
            assert!(a.request(Method::HEAD, bad).await.is_err(), "{bad}");
        }
        assert!(a
            .request(Method::HEAD, "keep/b/s/d/e/seq-1.jp2")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn the_state_file_makes_a_rerun_free() {
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/b/s/d/e/seq-1.jp2", b"abc", true);
        let m = Arc::new(Mock::default());
        let url = serve(m.clone()).await;
        let state = dir.path().join("state/archived.tsv");
        let mut o = opts(dir.path(), url.clone(), Mode::Upload);
        o.state = Some(state.clone());
        let r = run(o, cred()).await.unwrap();
        assert_eq!((r.uploaded, r.skipped), (1, 0));

        // Nothing new: skipped with no read and no request.
        m.blobs.lock().unwrap().clear();
        let mut o = opts(dir.path(), url.clone(), Mode::Upload);
        o.state = Some(state.clone());
        let r = run(o, cred()).await.unwrap();
        assert_eq!((r.scanned, r.skipped, r.uploaded), (0, 1, 0));
        assert!(m.blobs.lock().unwrap().is_empty());

        // Verify ignores the state and finds the blob gone.
        let mut o = opts(dir.path(), url, Mode::Verify);
        o.state = Some(state);
        let r = run(o, cred()).await.unwrap();
        assert_eq!((r.skipped, r.missing), (0, 1));
    }

    #[test]
    fn the_container_url_is_checked() {
        for bad in [
            "https://192.0.2.1/scans",
            "https://example.com/scans",
            "http://acct.blob.core.windows.net/scans",
            "https://acct.blob.core.windows.net/scans?sv=2022",
            "https://acct.blob.core.windows.net/",
            "https://acct.blob.core.windows.net/scans/keep",
            "https://user:pw@acct.blob.core.windows.net/scans",
        ] {
            assert!(Archive::new(bad, cred()).is_err(), "{bad}");
        }
        assert!(Archive::new("https://acct.blob.core.windows.net/scans", cred()).is_ok());
    }

    #[test]
    fn names_are_encoded_and_scans_listed_in_order() {
        assert_eq!(encode_path("keep/a b/c+d.jp2"), "keep/a%20b/c%2Bd.jp2");
        let dir = tempfile::tempdir().unwrap();
        put_scan(dir.path(), "keep/z/seq-1.jp2", b"1", true);
        put_scan(dir.path(), "keep/a/seq-1.jpg", b"2", true);
        let names: Vec<_> = list(dir.path(), "keep")
            .unwrap()
            .into_iter()
            .map(|x| x.1)
            .collect();
        assert_eq!(names, ["keep/a/seq-1.jpg", "keep/z/seq-1.jp2"]);
        assert!(list(dir.path(), "other").is_err());
    }
}
