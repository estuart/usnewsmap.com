//! `usnm-ocr-worker serve-scans` (#368): the prefetched scans over HTTP on
//! the LAN, from the scan-fetch container on pve-big, so the OCR nodes read
//! them with the same async, time-bounded client they use for LoC and
//! mount nothing (ADR-0015 decision 9).
//!
//! - `GET /scans/<set>/<batch>/<lccn>/<date>/<ed>/<seq>.<jp2|jpg>` and
//!   the sidecar `<…>.json`, from the scan root, read-only. A `.jp2` is
//!   the batch's original JP2, a `.jpg` the IIIF JPEG.
//! - `DELETE /scans/buffer/<…>.<jp2|jpg>` removes a buffer-set scan: its
//!   sidecar is renamed `<…>.done` first (so a half-done delete never
//!   leaves a scan that looks valid, and the fetcher knows not to fetch the
//!   page again), then the scan goes. A worker sends it once its output for
//!   the page is uploaded. A keep-set path is refused with 403.
//! - `GET /index/<tag>.jsonl` (`full`, or `pct<n>`): the fetcher's index,
//!   one JSON line per fetched scan (its sidecar: doc_id, batch,
//!   batch_version, loc_url, iiif_base, resolution, fetched_at, bytes,
//!   sha256, download_ms, user_agent, format and, for a JP2, md5),
//!   appended as scans land. It is how a
//!   seeder learns which pages are cached: a line means the scan was
//!   written; a buffer scan may since have been read and deleted.
//! - `PUT /manifests/<version>/{manifest.parquet,summary.json}` writes a
//!   prefetch manifest from a node (`pull-manifest.sh`), through a temporary
//!   file and a rename.
//!
//! Every request must come from a host on `--allow` (the nodes, by host
//! name: no private address is written into the repository). The names are
//! resolved at startup and again every `--allow-refresh-secs`; the
//! container's nftables rule lets in only its own LAN. Plain HTTP: the scans are LoC's
//! public images, and a worker checks each one's size, sha256 and md5
//! against its sidecar and the sidecar's URL against tile.loc.gov before use. Each
//! request is logged (method, path, status, peer); no page text exists here.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path as UrlPath, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::Router;

/// The largest manifest accepted.
const MAX_PUT_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    /// `/tank/ocr/scans`: holds `keep/` and `buffer/`.
    pub scans: PathBuf,
    /// `/tank/ocr/manifests`.
    pub manifests: PathBuf,
    /// The hosts allowed to ask anything.
    pub allow: Arc<AllowList>,
}

/// The clients allowed, by host name (an address works too), and what each
/// name resolved to last.
#[derive(Debug)]
pub struct AllowList {
    addrs: RwLock<BTreeMap<String, Vec<IpAddr>>>,
}

/// `name`'s addresses, canonical (an IPv4-mapped IPv6 address as IPv4),
/// sorted.
async fn lookup(name: &str) -> std::io::Result<Vec<IpAddr>> {
    let mut ips: Vec<IpAddr> = tokio::net::lookup_host((name, 0))
        .await?
        .map(|a| a.ip().to_canonical())
        .collect();
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        return Err(std::io::Error::other("no addresses"));
    }
    Ok(ips)
}

impl AllowList {
    /// Resolves every name; fails if any doesn't resolve, so a server never
    /// starts without one of its clients.
    pub async fn resolve(names: &[String]) -> anyhow::Result<Self> {
        anyhow::ensure!(!names.is_empty(), "no hosts to allow");
        let mut addrs = BTreeMap::new();
        for name in names {
            let name = name.trim();
            anyhow::ensure!(!name.is_empty(), "an empty host name in --allow");
            let ips = lookup(name)
                .await
                .map_err(|e| anyhow::anyhow!("--allow {name}: doesn't resolve: {e}"))?;
            addrs.insert(name.to_owned(), ips);
        }
        Ok(Self {
            addrs: RwLock::new(addrs),
        })
    }

    /// Resolves every name again. A name that doesn't resolve now keeps its
    /// last addresses, so a resolver that's away locks no node out.
    pub async fn refresh(&self) {
        let names: Vec<String> = self.addrs.read().unwrap().keys().cloned().collect();
        for name in names {
            match lookup(&name).await {
                Ok(ips) => {
                    let mut addrs = self.addrs.write().unwrap();
                    let old = addrs.insert(name.clone(), ips.clone());
                    if old.as_ref() != Some(&ips) {
                        tracing::info!(host = name, addrs = ?ips, "allowed host's addresses changed");
                    }
                }
                Err(e) => {
                    tracing::warn!(host = name, error = %e, "allowed host doesn't resolve; keeping its last addresses");
                }
            }
        }
    }

    pub fn contains(&self, ip: &IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.addrs
            .read()
            .unwrap()
            .values()
            .any(|ips| ips.contains(&ip))
    }

    /// Each host and its addresses, for the log.
    pub fn snapshot(&self) -> BTreeMap<String, Vec<IpAddr>> {
        self.addrs.read().unwrap().clone()
    }
}

/// A scan file's name: `.jpg` or `.jp2`.
fn is_scan(name: &str) -> bool {
    name.ends_with(".jpg") || name.ends_with(".jp2")
}

/// A path under `/scans/`: `keep` or `buffer`, then safe segments, ending
/// in a scan (`.jpg`, `.jp2`) or its sidecar (`.jpg.json`, `.jp2.json`).
/// Returns the set.
pub fn scan_rel(path: &str) -> Option<&str> {
    let segs: Vec<&str> = path.split('/').collect();
    if segs.len() < 2 || segs.len() > 8 {
        return None;
    }
    let set = segs[0];
    if set != "keep" && set != "buffer" {
        return None;
    }
    let ok = segs.iter().all(|s| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    });
    let last = segs[segs.len() - 1];
    let file = last.strip_suffix(".json").unwrap_or(last);
    (ok && is_scan(file)).then_some(set)
}

/// `rel` under `root`, built from its checked segments only, so it can't
/// leave the root.
fn under(root: &Path, rel: &str) -> Option<PathBuf> {
    scan_rel(rel)?;
    let mut p = root.to_owned();
    for seg in rel.split('/') {
        if matches!(seg, "." | "..") {
            return None;
        }
        p.push(seg);
    }
    p.starts_with(root).then_some(p)
}

/// A manifest version's directory: one safe segment, rebuilt from its
/// checked characters, directly under `root`.
fn under_manifests(root: &Path, version: &str) -> Option<PathBuf> {
    if !usnm_store::is_safe_segment(version) {
        return None;
    }
    let clean: String = version
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect();
    let dir = root.join(clean);
    (dir.parent() == Some(root)).then_some(dir)
}

fn sidecar(scan: &Path) -> PathBuf {
    let mut p = scan.as_os_str().to_owned();
    p.push(".json");
    PathBuf::from(p)
}

async fn allow_only(
    State(opts): State<Arc<ServeOptions>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let ip = peer.ip().to_canonical();
    let resp = if opts.allow.contains(&ip) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    };
    tracing::info!(
        method = %method,
        path,
        status = resp.status().as_u16(),
        peer = %ip,
        "scan server request"
    );
    resp
}

async fn get_scan(
    State(opts): State<Arc<ServeOptions>>,
    UrlPath(path): UrlPath<String>,
) -> Response {
    if scan_rel(&path).is_none() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(file) = under(&opts.scans, &path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match tokio::fs::read(file).await {
        Ok(b) => {
            let ct = if path.ends_with(".json") {
                "application/json"
            } else if path.ends_with(".jp2") {
                "image/jp2"
            } else {
                "image/jpeg"
            };
            ([(header::CONTENT_TYPE, ct)], b).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(path, error = %e, "can't read a scan");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn delete_scan(
    State(opts): State<Arc<ServeOptions>>,
    UrlPath(path): UrlPath<String>,
) -> Response {
    match scan_rel(&path) {
        Some("buffer") if is_scan(&path) => {}
        Some("keep") => return StatusCode::FORBIDDEN.into_response(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }
    let Some(scan) = under(&opts.scans, &path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut done = scan.as_os_str().to_owned();
    done.push(".done");
    let done = PathBuf::from(done);
    // The marker first: the scan goes only once the page is marked done
    // (now, or by an earlier delete), so it is never fetched again.
    match tokio::fs::rename(sidecar(&scan), &done).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !tokio::fs::try_exists(&done).await.unwrap_or(false) {
                return StatusCode::NOT_FOUND.into_response();
            }
        }
        Err(e) => {
            tracing::warn!(path, error = %e, "can't mark a buffer scan done");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    match tokio::fs::remove_file(&scan).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::warn!(path, error = %e, "can't delete a buffer scan");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_index(
    State(opts): State<Arc<ServeOptions>>,
    UrlPath(name): UrlPath<String>,
) -> Response {
    // `full.jsonl` or `pct<1-99>.jsonl`, mapped to a constant-shaped name.
    let Some(tag) = name.strip_suffix(".jsonl") else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let ok = tag == "full"
        || tag
            .strip_prefix("pct")
            .and_then(|p| p.parse::<u8>().ok())
            .is_some_and(|p| (1..100).contains(&p) && tag == format!("pct{p}"));
    if !ok {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match tokio::fs::read(opts.scans.join(format!("index-{tag}.jsonl"))).await {
        Ok(b) => ([(header::CONTENT_TYPE, "application/x-ndjson")], b).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(tag, error = %e, "can't read the index");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn put_manifest(
    State(opts): State<Arc<ServeOptions>>,
    UrlPath((version, file)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response {
    // The file name is one of two constants, never the request's string.
    let file: &'static str = match file.as_str() {
        "manifest.parquet" => "manifest.parquet",
        "summary.json" => "summary.json",
        _ => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Some(dir) = under_manifests(&opts.manifests, &version) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let write = async {
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = dir.join(format!(".{file}.part"));
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, dir.join(file)).await
    };
    match write.await {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => {
            tracing::warn!(version, file, error = %e, "can't write a manifest");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The server's routes.
pub fn router(opts: ServeOptions) -> Router {
    let opts = Arc::new(opts);
    Router::new()
        .route("/scans/{*path}", get(get_scan).delete(delete_scan))
        .route("/index/{name}", get(get_index))
        .route(
            "/manifests/{version}/{file}",
            put(put_manifest).layer(DefaultBodyLimit::max(MAX_PUT_BYTES)),
        )
        .layer(middleware::from_fn_with_state(
            Arc::clone(&opts),
            allow_only,
        ))
        .with_state(opts)
}

/// Serve until `stop` turns true, resolving the allowed hosts again every
/// `allow_refresh`.
pub async fn serve(
    listen: SocketAddr,
    opts: ServeOptions,
    allow_refresh: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, allow = ?opts.allow.snapshot(), scans = %opts.scans.display(), "scan server listening");
    let allow = Arc::clone(&opts.allow);
    let mut refresh_stop = stop.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(allow_refresh.max(Duration::from_secs(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = async { let _ = refresh_stop.wait_for(|s| *s).await; } => break,
            }
            allow.refresh().await;
        }
    });
    axum::serve(
        listener,
        router(opts).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stop.wait_for(|s| *s).await;
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_paths_are_checked() {
        assert_eq!(
            scan_rel("keep/nbu_gage/sn1/1890-05-03/1/7.jpg"),
            Some("keep")
        );
        assert_eq!(
            scan_rel("buffer/curiv_joshuaTree/sn1/1890-05-03/1/7.jpg.json"),
            Some("buffer")
        );
        assert_eq!(
            scan_rel("keep/mdu_garlic/sn1/1904-07-29/1/7.jp2"),
            Some("keep")
        );
        assert_eq!(
            scan_rel("keep/mdu_garlic/sn1/1904-07-29/1/7.jp2.json"),
            Some("keep")
        );
        for bad in [
            "other/a/b.jpg",
            "keep/../x/1.jpg",
            "keep/a/.hidden.jpg",
            "keep/a//1.jpg",
            "keep/a/1.txt",
            "keep/a/1.jp2.done",
            "keep/a/1.json",
            "keep",
            "buffer/a b/1.jpg",
        ] {
            assert_eq!(scan_rel(bad), None, "{bad}");
        }
    }

    async fn server(allow: &[&str]) -> (tempfile::TempDir, String) {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        let allow = Arc::new(AllowList::resolve(&allow).await.unwrap());
        let dir = tempfile::tempdir().unwrap();
        let scans = dir.path().join("scans");
        for set in ["keep", "buffer"] {
            let d = scans.join(set).join("b/sn1/1890-05-03/1");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("1.jpg"), b"jpeg").unwrap();
            std::fs::write(d.join("1.jpg.json"), b"{}").unwrap();
        }
        let opts = ServeOptions {
            scans,
            manifests: dir.path().join("manifests"),
            allow,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router(opts).into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap()
        });
        (dir, root)
    }

    #[tokio::test]
    async fn serves_scans_deletes_only_buffer_and_takes_manifests() {
        let (dir, root) = server(&["127.0.0.1"]).await;
        let http = reqwest::Client::new();
        let url = |p: &str| format!("{root}{p}");
        let r = http
            .get(url("/scans/keep/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(&r.bytes().await.unwrap()[..], b"jpeg");
        let missing = http
            .get(url("/scans/keep/b/sn1/1890-05-03/1/2.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        // Keep scans can't be deleted; buffer scans can, sidecar and all.
        let keep = http
            .delete(url("/scans/keep/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(keep.status(), 403);
        assert!(dir
            .path()
            .join("scans/keep/b/sn1/1890-05-03/1/1.jpg")
            .exists());
        let buf = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(buf.status(), 204);
        let gone = dir.path().join("scans/buffer/b/sn1/1890-05-03/1");
        assert!(!gone.join("1.jpg").exists() && !gone.join("1.jpg.json").exists());
        // The sidecar stays as the page's consumed marker.
        assert!(gone.join("1.jpg.done").exists());
        let again = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        // A repeat (a worker's retry) succeeds: the page is marked done.
        assert_eq!(again.status(), 204);
        let never = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/1/9.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(never.status(), 404);
        // The scan stays if the page can't be marked done.
        let d = dir.path().join("scans/buffer/b/sn1/1890-05-03/2");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("1.jpg"), b"jpeg").unwrap();
        std::fs::create_dir_all(d.join("1.jpg.json")).unwrap();
        std::fs::create_dir_all(d.join("1.jpg.done/x")).unwrap();
        let blocked = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/2/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), 500);
        assert!(d.join("1.jpg").exists());
        // A path out of the tree, and a sidecar delete, are refused.
        for p in [
            "/scans/buffer/../keep/b/1.jpg",
            "/scans/buffer/b/1.jpg.json",
        ] {
            let r = http.delete(url(p)).send().await.unwrap();
            // The client resolves `..` to a keep path, refused too.
            assert!(
                [400, 403, 404].contains(&r.status().as_u16()),
                "{p}: {}",
                r.status()
            );
        }
        // The index, by tag.
        std::fs::write(dir.path().join("scans/index-full.jsonl"), b"{}\n").unwrap();
        let idx = http.get(url("/index/full.jsonl")).send().await.unwrap();
        assert_eq!(idx.status(), 200);
        assert_eq!(&idx.bytes().await.unwrap()[..], b"{}\n");
        for bad in [
            "/index/../x.jsonl",
            "/index/pct0.jsonl",
            "/index/pct050.jsonl",
            "/index/full.json",
        ] {
            let r = http.get(url(bad)).send().await.unwrap();
            assert!(
                [400, 404].contains(&r.status().as_u16()),
                "{bad}: {}",
                r.status()
            );
        }
        let put = http
            .put(url("/manifests/pages-v1/manifest.parquet"))
            .body("rows")
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 201);
        assert_eq!(
            std::fs::read(dir.path().join("manifests/pages-v1/manifest.parquet")).unwrap(),
            b"rows"
        );
        let other = http
            .put(url("/manifests/pages-v1/other.bin"))
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 400);
    }

    #[tokio::test]
    async fn serves_and_deletes_jp2s_beside_jpegs() {
        let (dir, root) = server(&["127.0.0.1"]).await;
        let issue = dir.path().join("scans/buffer/b/sn1/1890-05-03/1");
        std::fs::write(issue.join("2.jp2"), b"jp2").unwrap();
        std::fs::write(issue.join("2.jp2.json"), b"{}").unwrap();
        let http = reqwest::Client::new();
        let url = |p: &str| format!("{root}/scans/buffer/b/sn1/1890-05-03/1/{p}");
        let jp2 = http.get(url("2.jp2")).send().await.unwrap();
        assert_eq!(jp2.status(), 200);
        assert_eq!(jp2.headers()["content-type"], "image/jp2");
        assert_eq!(&jp2.bytes().await.unwrap()[..], b"jp2");
        let jpg = http.get(url("1.jpg")).send().await.unwrap();
        assert_eq!(jpg.headers()["content-type"], "image/jpeg");
        let side = http.get(url("2.jp2.json")).send().await.unwrap();
        assert_eq!(side.headers()["content-type"], "application/json");
        let del = http.delete(url("2.jp2")).send().await.unwrap();
        assert_eq!(del.status(), 204);
        assert!(!issue.join("2.jp2").exists() && issue.join("2.jp2.done").exists());
        // The JPEG beside it is untouched.
        assert!(issue.join("1.jpg").exists());
    }

    #[tokio::test]
    async fn refuses_addresses_not_allowed() {
        // An RFC 5737 documentation address: never this test's client.
        let (_dir, root) = server(&["192.0.2.243"]).await;
        let r = reqwest::get(format!("{root}/scans/keep/b/sn1/1890-05-03/1/1.jpg"))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }

    #[tokio::test]
    async fn allows_hosts_by_name() {
        let (_dir, root) = server(&["localhost"]).await;
        let r = reqwest::get(format!("{root}/scans/keep/b/sn1/1890-05-03/1/1.jpg"))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    #[tokio::test]
    async fn refuses_to_start_with_a_name_that_doesnt_resolve() {
        let names = vec!["localhost".to_string(), "nothing.invalid".to_string()];
        assert!(AllowList::resolve(&names).await.is_err());
        assert!(AllowList::resolve(&[]).await.is_err());
    }

    #[tokio::test]
    async fn a_refresh_keeps_a_name_that_stops_resolving() {
        let list = AllowList::resolve(&["127.0.0.1".to_string()])
            .await
            .unwrap();
        // As if this name had resolved at startup and its resolver were away now.
        list.addrs
            .write()
            .unwrap()
            .insert("nothing.invalid".into(), vec!["192.0.2.7".parse().unwrap()]);
        list.refresh().await;
        assert!(list.contains(&"192.0.2.7".parse().unwrap()));
        assert!(list.contains(&"::ffff:127.0.0.1".parse().unwrap()));
        assert!(!list.contains(&"198.51.100.1".parse().unwrap()));
    }
}
