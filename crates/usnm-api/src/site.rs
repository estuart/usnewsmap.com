//! The web app, served by the API from its built files (`USNM_SITE_DIR`,
//! ADR-0009): same origin as `/v1`, deployed in the same image.
//!
//! - Files come from the directory, preferring the `.br` or `.gz` copy the
//!   image build wrote next to each one.
//! - Paths that look like app routes (no file extension, not under
//!   `/assets/`) get `index.html`, so client-side routes survive a reload.
//!   The app's own pages (`/`, `/datasets`, `/privacy`) are 200; any other
//!   such path gets the same shell with a 404, and the app shows a not-found
//!   page. Missing files are a plain 404. `/privacy` gets its own title,
//!   canonical link and `og:url` in the shell, since it is indexed.
//! - `/status`, the dataset details page's old path, is redirected (301) to
//!   `/datasets` with its query, so existing links keep working.
//! - Hashed assets are cached for a year; `index.html` is revalidated on
//!   every load so a release shows up at once.
//! - Search permalinks (`/?q=…`) and `/datasets` carry `X-Robots-Tag: noindex`,
//!   so crawlers that don't run the app see it too.
//! - Requests for `www.` + the site's host are redirected to the host itself.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::handler::HandlerWithoutStateExt;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tower_http::services::ServeDir;

/// Headers on every response (the site's and the API's). `connect-src`
/// needs only `'self'` for the API; the rest is the basemap and tiles.
pub const SECURITY_HEADERS: [(&str, &str); 4] = [
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob: https:; font-src 'self' https://tiles.openfreemap.org; \
         connect-src 'self' https://*.blob.core.windows.net https://tiles.openfreemap.org; \
         worker-src 'self' blob:; child-src blob:; object-src 'none'; base-uri 'self'; \
         form-action 'self'",
    ),
    ("referrer-policy", "strict-origin-when-cross-origin"),
    ("x-content-type-options", "nosniff"),
    (
        "permissions-policy",
        "geolocation=(), camera=(), microphone=()",
    ),
];

/// The longest file name the filesystem takes (Linux `NAME_MAX`).
const NAME_MAX: usize = 255;

/// The longest name `ServeDir` can look up: it also probes `<name>.br` and
/// `<name>.gz`, which must fit `NAME_MAX` too. The site has no names near it.
const NAME_LIMIT: usize = NAME_MAX - ".br".len();

/// The site as a service for the API router's fallback.
pub fn router(dir: PathBuf) -> Router {
    let index = dir.join("index.html");
    let spa = {
        let index = index.clone();
        move |req: Request| {
            let index = index.clone();
            async move { app_route(req, index).await }
        }
    };
    let files = ServeDir::new(dir)
        .precompressed_br()
        .precompressed_gzip()
        .fallback(spa.into_service());
    // A path segment too long to be a file name (or to take the `.br` or `.gz`
    // suffix) can't name one of ours, and `ServeDir` answers the filesystem's
    // "file name too long" with a 500, so such paths skip it and are answered
    // like any other path with no file.
    let long_names = move |req: Request, next: Next| {
        let index = index.clone();
        async move {
            if req
                .uri()
                .path()
                .split('/')
                .any(|s| decoded_len(s) > NAME_LIMIT)
            {
                app_route(req, index).await
            } else {
                next.run(req).await
            }
        }
    };
    Router::new()
        .fallback_service(files)
        .layer(middleware::from_fn(long_names))
        .layer(middleware::from_fn(cache_control))
        .layer(middleware::from_fn(robots_tag))
}

/// How many bytes a path segment is once percent-decoded, as `ServeDir`
/// decodes it before it names a file: each valid `%XX` escape is one byte.
fn decoded_len(segment: &str) -> usize {
    let b = segment.as_bytes();
    let (mut i, mut n) = (0, 0);
    while i < b.len() {
        let escape = b[i] == b'%'
            && i + 2 < b.len()
            && b[i + 1].is_ascii_hexdigit()
            && b[i + 2].is_ascii_hexdigit();
        i += if escape { 3 } else { 1 };
        n += 1;
    }
    n
}

/// The paths the app renders a page for (`web/src/route.ts`), with or
/// without a trailing slash. `/index.html` is the home page's file, which
/// `ServeDir` serves directly; since tower-http 0.7 it no longer serves a
/// file at a path with a trailing slash, so `/index.html/` comes here.
fn is_app_page(path: &str) -> bool {
    matches!(
        path.trim_end_matches('/'),
        "" | "/datasets" | "/privacy" | "/index.html"
    )
}

/// The dataset details page's path before it was renamed (#294).
const OLD_DATASETS_PATH: &str = "/status";

/// Where a request for `/status` goes: `/datasets`, keeping the query.
fn datasets_redirect(uri: &Uri) -> Option<String> {
    (uri.path().trim_end_matches('/') == OLD_DATASETS_PATH).then(|| match uri.query() {
        Some(q) => format!("/datasets?{q}"),
        None => "/datasets".to_owned(),
    })
}

/// The shell for `path`: the privacy page's head names that page, not the
/// home page, because search engines index it.
fn shell_for(path: &str, index: Vec<u8>) -> Vec<u8> {
    if path.trim_end_matches('/') != "/privacy" {
        return index;
    }
    let Ok(html) = String::from_utf8(index) else {
        return Vec::new();
    };
    html.replacen(
        "<title>US News Map</title>",
        "<title>Privacy · US News Map</title>",
        1,
    )
    .replacen(
        r#"<link rel="canonical" href="https://usnewsmap.com/" />"#,
        r#"<link rel="canonical" href="https://usnewsmap.com/privacy" />"#,
        1,
    )
    .replacen(
        r#"<meta property="og:url" content="https://usnewsmap.com/" />"#,
        r#"<meta property="og:url" content="https://usnewsmap.com/privacy" />"#,
        1,
    )
    .into_bytes()
}

/// A path with no file behind it: `index.html` for app routes (404 unless
/// the app has a page there), else a plain 404.
async fn app_route(req: Request, index: PathBuf) -> Response {
    let path = req.uri().path();
    // The last segment ignoring a trailing slash, so `/favicon.svg/` is a
    // missing file (a plain 404), not an app route.
    let last = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    if path.starts_with("/assets/") || (last.contains('.') && !is_app_page(path)) {
        // HEAD gets the status without a body.
        if req.method() == Method::HEAD {
            return StatusCode::NOT_FOUND.into_response();
        }
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    if let Some(location) = datasets_redirect(req.uri()) {
        return match HeaderValue::from_str(&location) {
            Ok(location) => (
                StatusCode::MOVED_PERMANENTLY,
                [(header::LOCATION, location)],
            )
                .into_response(),
            Err(_) => (StatusCode::BAD_REQUEST, "bad request").into_response(),
        };
    }
    let status = if is_app_page(path) {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let html = (header::CONTENT_TYPE, "text/html; charset=utf-8".to_owned());
    // HEAD: the headers GET would send, without sending the body.
    if req.method() == Method::HEAD {
        return match tokio::fs::read(&index).await {
            Ok(body) => (
                status,
                [
                    html,
                    (
                        header::CONTENT_LENGTH,
                        shell_for(path, body).len().to_string(),
                    ),
                ],
            )
                .into_response(),
            Err(e) => {
                tracing::error!(error = %e, path = %index.display(), "site index missing");
                (StatusCode::NOT_FOUND, "not found").into_response()
            }
        };
    }
    match tokio::fs::read(&index).await {
        Ok(body) => (status, [html], shell_for(path, body)).into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %index.display(), "site index missing");
            (StatusCode::NOT_FOUND, "not found").into_response()
        }
    }
}

async fn cache_control(req: Request, next: Next) -> Response {
    let immutable = req.uri().path().starts_with("/assets/");
    let mut resp = next.run(req).await;
    let html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    // The 404 app shell names this release's assets, like the 200 one.
    if !resp.status().is_success() && !(html && resp.status() == StatusCode::NOT_FOUND) {
        return resp;
    }
    let value = if immutable {
        "public, max-age=31536000, immutable"
    } else if html {
        "no-cache"
    } else {
        "public, max-age=3600"
    };
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    resp
}

/// Pages that shouldn't appear in search results: search permalinks
/// (`/?q=…`), which are endless variations on the home page, and the
/// dataset details page, which changes often (and its old path, `/status`).
fn noindex(uri: &Uri) -> bool {
    match uri.path().trim_end_matches('/') {
        "/datasets" | OLD_DATASETS_PATH => true,
        "" | "/index.html" => uri
            .query()
            .is_some_and(|q| form_urlencoded::parse(q.as_bytes()).any(|(k, _)| k == "q")),
        _ => false,
    }
}

async fn robots_tag(req: Request, next: Next) -> Response {
    let noindex = noindex(req.uri());
    let mut resp = next.run(req).await;
    if noindex {
        resp.headers_mut().insert(
            header::HeaderName::from_static("x-robots-tag"),
            HeaderValue::from_static("noindex"),
        );
    }
    resp
}

/// Redirects requests for `www.{host}` to `https://{host}`, keeping the path
/// and query (301). `host` is the site's canonical hostname, lowercase.
pub async fn www_redirect(State(host): State<Arc<str>>, req: Request, next: Next) -> Response {
    // HTTP/2 carries the host in the URI's authority, HTTP/1.1 in `Host`.
    let requested = req.uri().host().or_else(|| {
        req.headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.split(':').next().unwrap_or(h))
    });
    let is_www = requested.is_some_and(|h| {
        let (h, host) = (h.as_bytes(), host.as_bytes());
        h.len() == host.len() + 4
            && h[..4].eq_ignore_ascii_case(b"www.")
            && h[4..].eq_ignore_ascii_case(host)
    });
    if !is_www {
        return next.run(req).await;
    }
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    let location = format!("https://{host}{path}");
    match HeaderValue::from_str(&location) {
        Ok(location) => (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "bad request").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_pages() {
        for path in [
            "/",
            "//",
            "/datasets",
            "/datasets/",
            "/privacy",
            "/privacy/",
            "/index.html",
            "/index.html/",
        ] {
            assert!(is_app_page(path), "{path}");
        }
        for path in [
            "/search/gold",
            "/statuses",
            "/does-not-exist",
            "/status",
            "/status/",
            "/datasets/x",
            "/status/x",
            "/privacy/x",
            "/index.html/x",
            "/favicon.svg/",
        ] {
            assert!(!is_app_page(path), "{path}");
        }
    }

    #[test]
    fn the_privacy_page_names_itself_in_the_head() {
        let index = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/index.html"),
        )
        .unwrap();
        let home = String::from_utf8(shell_for("/", index.clone().into_bytes())).unwrap();
        assert_eq!(home, index);
        let privacy = String::from_utf8(shell_for("/privacy/", index.into_bytes())).unwrap();
        assert!(privacy.contains("<title>Privacy · US News Map</title>"));
        assert!(
            privacy.contains(r#"<link rel="canonical" href="https://usnewsmap.com/privacy" />"#)
        );
        assert!(privacy
            .contains(r#"<meta property="og:url" content="https://usnewsmap.com/privacy" />"#));
        assert!(!privacy.contains(r#"href="https://usnewsmap.com/" />"#));
    }

    #[test]
    fn noindex_pages() {
        let yes = [
            "/?q=gold",
            "/?from=1896-06-01&q=%22cross+of+gold%22",
            "/datasets",
            "/datasets/?x=1",
            "/status",
            "/status/?x=1",
        ];
        for uri in yes {
            assert!(noindex(&uri.parse().unwrap()), "{uri}");
        }
        let no = [
            "/",
            "/?",
            "/?t=1896-07-01",
            "/?qq=1",
            "/privacy",
            "/privacy?utm_source=x",
            "/favicon.svg",
            "/search?q=gold",
        ];
        for uri in no {
            assert!(!noindex(&uri.parse().unwrap()), "{uri}");
        }
    }

    #[test]
    fn the_old_status_path_redirects_to_datasets() {
        let to = |uri: &str| datasets_redirect(&uri.parse().unwrap());
        assert_eq!(to("/status").as_deref(), Some("/datasets"));
        assert_eq!(to("/status/").as_deref(), Some("/datasets"));
        assert_eq!(to("/status?x=1&y=2").as_deref(), Some("/datasets?x=1&y=2"));
        for uri in ["/datasets", "/status/x", "/statuses", "/", "/v1/status"] {
            assert_eq!(to(uri), None, "{uri}");
        }
    }
}
