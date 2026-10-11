//! `usnm-ocr-worker serve-scans` (#368): the prefetched scans over HTTP on
//! the LAN, from the scan-fetch container on pve-big, so the OCR nodes read
//! them with the same async, time-bounded client they use for LoC and
//! mount nothing (ADR-0015 decision 9).
//!
//! - `GET /scans/<set>/<batch>/<lccn>/<date>/<ed>/<seq>.jpg` and the
//!   sidecar `<…>.jpg.json`, from the scan root, read-only.
//! - `DELETE /scans/buffer/<…>.jpg` removes a buffer-set scan and its
//!   sidecar (the sidecar first, so a half-done delete never leaves a scan
//!   that looks valid): a worker sends it once its output for the page is
//!   uploaded. A keep-set path is refused with 403.
//! - `PUT /manifests/<version>/{manifest.parquet,summary.json}` writes a
//!   prefetch manifest from a node (`pull-manifest.sh`), through a temporary
//!   file and a rename.
//!
//! Every request must come from an address on `--allow` (the nodes); the
//! container's nftables rule says the same. Plain HTTP: the scans are LoC's
//! public images, and a worker checks each one's size and sha256 against
//! its sidecar and the sidecar's URL against tile.loc.gov before use. Each
//! request is logged (method, path, status, peer); no page text exists here.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// The addresses allowed to ask anything.
    pub allow: Vec<IpAddr>,
}

/// A path under `/scans/`: `keep` or `buffer`, then safe segments, ending
/// in `.jpg` or `.jpg.json`. Returns the set.
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
    (ok && (last.ends_with(".jpg") || last.ends_with(".jpg.json"))).then_some(set)
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
    match tokio::fs::read(opts.scans.join(&path)).await {
        Ok(b) => {
            let ct = if path.ends_with(".json") {
                "application/json"
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
        Some("buffer") if path.ends_with(".jpg") => {}
        Some("keep") => return StatusCode::FORBIDDEN.into_response(),
        _ => return StatusCode::BAD_REQUEST.into_response(),
    }
    let scan = opts.scans.join(&path);
    let side = tokio::fs::remove_file(sidecar(&scan)).await;
    let jpg = tokio::fs::remove_file(&scan).await;
    let missing = |r: &std::io::Result<()>| {
        matches!(r, Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    };
    if missing(&side) && missing(&jpg) {
        return StatusCode::NOT_FOUND.into_response();
    }
    for r in [&side, &jpg] {
        if let Err(e) = r {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path, error = %e, "can't delete a buffer scan");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn put_manifest(
    State(opts): State<Arc<ServeOptions>>,
    UrlPath((version, file)): UrlPath<(String, String)>,
    body: Bytes,
) -> Response {
    if !usnm_store::is_safe_segment(&version)
        || !matches!(file.as_str(), "manifest.parquet" | "summary.json")
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let dir = opts.manifests.join(&version);
    let write = async {
        tokio::fs::create_dir_all(&dir).await?;
        let tmp = dir.join(format!(".{file}.part"));
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, dir.join(&file)).await
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

/// Serve until `stop` turns true.
pub async fn serve(
    listen: SocketAddr,
    opts: ServeOptions,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(%listen, allow = ?opts.allow, scans = %opts.scans.display(), "scan server listening");
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
        assert_eq!(scan_rel("keep/nbu_gage/sn1/1890-05-03/1/7.jpg"), Some("keep"));
        assert_eq!(
            scan_rel("buffer/curiv_joshuaTree/sn1/1890-05-03/1/7.jpg.json"),
            Some("buffer")
        );
        for bad in [
            "other/a/b.jpg",
            "keep/../x/1.jpg",
            "keep/a/.hidden.jpg",
            "keep/a//1.jpg",
            "keep/a/1.txt",
            "keep",
            "buffer/a b/1.jpg",
        ] {
            assert_eq!(scan_rel(bad), None, "{bad}");
        }
    }

    async fn server(allow: Vec<IpAddr>) -> (tempfile::TempDir, String) {
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
        let (dir, root) = server(vec!["127.0.0.1".parse().unwrap()]).await;
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
        assert!(dir.path().join("scans/keep/b/sn1/1890-05-03/1/1.jpg").exists());
        let buf = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(buf.status(), 204);
        let gone = dir.path().join("scans/buffer/b/sn1/1890-05-03/1");
        assert!(!gone.join("1.jpg").exists() && !gone.join("1.jpg.json").exists());
        let again = http
            .delete(url("/scans/buffer/b/sn1/1890-05-03/1/1.jpg"))
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), 404);
        // A path out of the tree, and a sidecar delete, are refused.
        for p in ["/scans/buffer/../keep/b/1.jpg", "/scans/buffer/b/1.jpg.json"] {
            let r = http.delete(url(p)).send().await.unwrap();
            assert!(r.status() == 400 || r.status() == 404, "{p}: {}", r.status());
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
    async fn refuses_addresses_not_allowed() {
        let (_dir, root) = server(vec!["192.168.0.243".parse().unwrap()]).await;
        let r = reqwest::get(format!("{root}/scans/keep/b/sn1/1890-05-03/1/1.jpg"))
            .await
            .unwrap();
        assert_eq!(r.status(), 403);
    }
}
