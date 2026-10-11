//! `POST /v1/beacon`: one page view from the web app (06 §6.3.7), forwarded
//! to Application Insights as a page view (`AppPageViews`).
//!
//! The body is a small JSON object with the page's route name and, on the
//! first page of a visit, where the visitor came from. It can't carry a path
//! or a query string: unknown fields are refused and the route is one of the
//! app's page names. Nothing in it is logged. The visitor's address goes to
//! Application Insights only as the tag it derives the location from, and
//! the user agent only as a device type and browser family.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::error::ApiError;
use crate::ratelimit::client_ip;
use crate::AppState;

/// Largest body accepted, in bytes.
pub const MAX_BODY: usize = 2048;
/// Longest title, in characters.
pub const MAX_TITLE: usize = 100;
/// Longest `utm_*` value, in characters.
pub const MAX_UTM: usize = 64;
/// Longest referrer origin, in characters.
pub const MAX_ORIGIN: usize = 253;

/// The app's pages (`web/src/route.ts`) and the path each is served at.
/// `not-found` is any other path, so it has no URL.
pub const ROUTES: [(&str, Option<&str>); 4] = [
    ("search", Some("/")),
    ("datasets", Some("/datasets")),
    ("privacy", Some("/privacy")),
    ("not-found", None),
];

/// What the web app sends. Any other field is refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Beacon {
    route: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    referrer_origin: Option<String>,
    #[serde(default)]
    utm_source: Option<String>,
    #[serde(default)]
    utm_medium: Option<String>,
    #[serde(default)]
    utm_campaign: Option<String>,
}

/// A beacon that passed validation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Valid {
    pub route: &'static str,
    pub path: Option<&'static str>,
    pub title: Option<String>,
    /// `direct`, `internal` or a lowercase origin.
    pub referrer: String,
    pub utm: Vec<(&'static str, String)>,
}

pub async fn beacon(State(state): State<Arc<AppState>>, req: Request) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let (parts, body) = req.into_parts();
    match accept(&state, &parts.headers, body).await {
        Ok(valid) => {
            let outcome = forward(&state, &parts.headers, peer, valid);
            state.metrics.beacon(outcome);
            let mut resp = StatusCode::NO_CONTENT.into_response();
            resp.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            resp
        }
        Err(e) => e.into_response(),
    }
}

async fn accept(state: &AppState, headers: &HeaderMap, body: Body) -> Result<Valid, ApiError> {
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase());
    // sendBeacon sends a string as text/plain; fetch sends application/json.
    if !matches!(media.as_deref(), Some("application/json" | "text/plain")) {
        return Err(ApiError::MediaType(
            "send the page view as application/json or text/plain".to_owned(),
        ));
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY as u64) {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| too_large())?;
    // Only an object: serde would also read a struct from an array.
    let object = bytes.trim_ascii_start().first() == Some(&b'{');
    let parsed = object.then(|| serde_json::from_slice::<Beacon>(&bytes));
    let beacon = parsed.and_then(Result::ok).ok_or_else(|| {
        ApiError::BadBeacon(
            "the body must be a JSON object with route and optionally title, \
             referrer_origin, utm_source, utm_medium and utm_campaign"
                .to_owned(),
        )
    })?;
    validate(beacon, &state.config.site_host).map_err(ApiError::BadBeacon)
}

fn too_large() -> ApiError {
    ApiError::TooLarge(format!("the body must be at most {MAX_BODY} bytes"))
}

fn validate(b: Beacon, site_host: &str) -> Result<Valid, String> {
    let Some(&(route, path)) = ROUTES.iter().find(|(r, _)| *r == b.route) else {
        return Err("route must be one of search, status, privacy, not-found".to_owned());
    };
    // Before trimming, which would hide leading or trailing ones.
    for (key, value) in [
        ("title", &b.title),
        ("referrer_origin", &b.referrer_origin),
        ("utm_source", &b.utm_source),
        ("utm_medium", &b.utm_medium),
        ("utm_campaign", &b.utm_campaign),
    ] {
        if value
            .as_deref()
            .is_some_and(|v| v.chars().any(char::is_control))
        {
            return Err(format!("{key} must not contain control characters"));
        }
    }
    let title = match b.title.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(t) if t.chars().count() > MAX_TITLE => {
            return Err(format!("title must be at most {MAX_TITLE} characters"))
        }
        Some(t) => Some(t.to_owned()),
    };
    let referrer = match b.referrer_origin.as_deref().map(str::trim) {
        None | Some("") => "direct".to_owned(),
        Some("internal") => "internal".to_owned(),
        Some(o) => {
            let origin = parse_origin(o).ok_or_else(|| {
                "referrer_origin must be empty, internal, or an origin such as https://example.com"
                    .to_owned()
            })?;
            if same_site(&origin, site_host) {
                "internal".to_owned()
            } else {
                origin
            }
        }
    };
    let mut utm = Vec::new();
    for (key, value) in [
        ("utm_source", b.utm_source),
        ("utm_medium", b.utm_medium),
        ("utm_campaign", b.utm_campaign),
    ] {
        let Some(v) = value.map(|v| v.trim().to_lowercase()) else {
            continue;
        };
        if v.is_empty() {
            continue;
        }
        if v.chars().count() > MAX_UTM {
            return Err(format!("{key} must be at most {MAX_UTM} characters"));
        }
        utm.push((key, v));
    }
    Ok(Valid {
        route,
        path,
        title,
        referrer,
        utm,
    })
}

/// `scheme://host[:port]`, lowercase, for http(s) origins with nothing else:
/// no user, path, query or fragment.
fn parse_origin(s: &str) -> Option<String> {
    if s.len() > MAX_ORIGIN {
        return None;
    }
    let s = s.to_ascii_lowercase();
    let (scheme, authority) = s.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let (host, port) = match authority.strip_prefix('[') {
        // [IPv6]:port
        Some(rest) => {
            let (addr, after) = rest.split_once(']')?;
            addr.parse::<std::net::Ipv6Addr>().ok()?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':')?),
            };
            (&authority[..addr.len() + 2], port)
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    if let Some(p) = port {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let dns = |h: &str| {
        !h.is_empty()
            && h.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
    };
    if !host.starts_with('[') && !dns(host) {
        return None;
    }
    Some(s)
}

/// Whether an origin is the site itself or one of its subdomains.
fn same_site(origin: &str, site_host: &str) -> bool {
    let authority = origin.split_once("://").map_or("", |(_, a)| a);
    let host = authority.split(':').next().unwrap_or_default();
    host == site_host
        || host
            .strip_suffix(site_host)
            .is_some_and(|sub| sub.ends_with('.'))
}

/// Forward a valid page view unless the client opted out, looks like a bot
/// or came from another site. Returns the outcome for `api.beacons`.
fn forward(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    valid: Valid,
) -> &'static str {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // Do Not Track and Global Privacy Control, in case the app sent anyway.
    if header("dnt").map(str::trim) == Some("1") || header("sec-gpc").map(str::trim) == Some("1") {
        return "opted_out";
    }
    // Browsers send Origin with a POST; a page on another site posting here
    // doesn't count as a visit.
    if let Some(origin) = header("origin") {
        let own = format!("https://{}", state.config.site_host);
        if origin != own && !state.config.allowed_origins.iter().any(|o| o == origin) {
            return "other_origin";
        }
    }
    let ua = header("user-agent").unwrap_or_default();
    if is_bot(ua) {
        return "bot";
    }
    let Some(sink) = &state.page_views else {
        return "off";
    };
    let mut properties = std::collections::BTreeMap::new();
    properties.insert("referrer_origin".to_owned(), valid.referrer);
    for (k, v) in valid.utm {
        properties.insert(k.to_owned(), v);
    }
    if let Some(t) = valid.title {
        properties.insert("title".to_owned(), t);
    }
    properties.insert("device_type".to_owned(), device_type(ua).to_owned());
    properties.insert("browser".to_owned(), browser(ua).to_owned());
    let view = usnm_telemetry::PageView {
        name: valid.route.to_owned(),
        url: valid
            .path
            .map(|p| format!("https://{}{p}", state.config.site_host)),
        client_ip: client_ip(headers, peer, state.config.trusted_proxy_hops),
        properties,
    };
    if sink.track(view) {
        "forwarded"
    } else {
        "dropped"
    }
}

/// Crawlers, link previews, monitors, scripts and headless browsers.
pub(crate) fn is_bot(ua: &str) -> bool {
    const MARKERS: [&str; 24] = [
        "bot",
        "crawl",
        "spider",
        "slurp",
        "headless",
        "lighthouse",
        "pagespeed",
        "preview",
        "facebookexternalhit",
        "embedly",
        "monitor",
        "uptime",
        "curl/",
        "wget/",
        "python",
        "go-http-client",
        "java/",
        "okhttp",
        "axios/",
        "node-fetch",
        "undici",
        "httpclient",
        "phantomjs",
        "selenium",
    ];
    let ua = ua.to_ascii_lowercase();
    ua.trim().is_empty() || !ua.starts_with("mozilla/") || MARKERS.iter().any(|m| ua.contains(m))
}

/// `mobile`, `tablet` or `desktop`, from the user agent.
pub(crate) fn device_type(ua: &str) -> &'static str {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("ipad")
        || ua.contains("tablet")
        || (ua.contains("android") && !ua.contains("mobile"))
    {
        "tablet"
    } else if ua.contains("mobi")
        || ua.contains("iphone")
        || ua.contains("ipod")
        || ua.contains("android")
    {
        "mobile"
    } else {
        "desktop"
    }
}

/// The browser family, from the user agent (the order matters: most
/// browsers also claim to be Chrome or Safari).
pub(crate) fn browser(ua: &str) -> &'static str {
    let ua = ua.to_ascii_lowercase();
    if ua.contains("edg/") || ua.contains("edga/") || ua.contains("edgios/") {
        "Edge"
    } else if ua.contains("opr/") || ua.contains("opera") {
        "Opera"
    } else if ua.contains("samsungbrowser/") {
        "Samsung Internet"
    } else if ua.contains("firefox/") || ua.contains("fxios/") {
        "Firefox"
    } else if ua.contains("crios/") || ua.contains("chrome/") || ua.contains("chromium/") {
        "Chrome"
    } else if ua.contains("safari/") {
        "Safari"
    } else {
        "Other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
    const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1";
    const IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 18_6 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Mobile/15E148 Safari/604.1";
    const ANDROID_TABLET: &str = "Mozilla/5.0 (Linux; Android 14; SM-X710) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
    const PIXEL_FIREFOX: &str =
        "Mozilla/5.0 (Android 15; Mobile; rv:143.0) Gecko/143.0 Firefox/143.0";
    const EDGE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
    const SAMSUNG: &str = "Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/28.0 Chrome/130.0.0.0 Mobile Safari/537.36";

    fn beacon(route: &str) -> Beacon {
        Beacon {
            route: route.to_owned(),
            title: None,
            referrer_origin: None,
            utm_source: None,
            utm_medium: None,
            utm_campaign: None,
        }
    }

    #[test]
    fn routes_are_the_apps_pages() {
        for (route, path) in ROUTES {
            let v = validate(beacon(route), "usnewsmap.com").unwrap();
            assert_eq!((v.route, v.path), (route, path));
            assert_eq!(v.referrer, "direct");
        }
        for route in [
            "",
            "/",
            "/?q=gold",
            "home",
            "Search",
            "search?q=gold",
            "/datasets",
            "status",
        ] {
            assert!(validate(beacon(route), "usnewsmap.com").is_err(), "{route}");
        }
    }

    #[test]
    fn referrers_are_origins() {
        let r = |o: &str| {
            let mut b = beacon("search");
            b.referrer_origin = Some(o.to_owned());
            validate(b, "usnewsmap.com").map(|v| v.referrer)
        };
        assert_eq!(r("").unwrap(), "direct");
        assert_eq!(r("internal").unwrap(), "internal");
        assert_eq!(
            r("https://www.Google.com").unwrap(),
            "https://www.google.com"
        );
        assert_eq!(r("http://localhost:8080").unwrap(), "http://localhost:8080");
        assert_eq!(
            r("https://[2001:db8::1]:443").unwrap(),
            "https://[2001:db8::1]:443"
        );
        assert_eq!(r("https://usnewsmap.com").unwrap(), "internal");
        assert_eq!(r("https://www.usnewsmap.com").unwrap(), "internal");
        assert_eq!(
            r("https://notusnewsmap.com").unwrap(),
            "https://notusnewsmap.com"
        );
        for bad in [
            "https://example.com/",
            "https://example.com/search?q=gold",
            "https://example.com?q=gold",
            "https://user@example.com",
            "https://example.com#x",
            "ftp://example.com",
            "example.com",
            "https://",
            "https://exa mple.com",
            "https://example.com:",
            "https://example.com:123456",
            "https://[nope]",
            "https://[::1]x",
            "https://[::1]:",
            "external",
        ] {
            assert!(r(bad).is_err(), "{bad}");
        }
        assert!(r(&format!("https://{}.com", "a".repeat(260))).is_err());
    }

    #[test]
    fn titles_and_utm_are_capped() {
        let mut b = beacon("search");
        b.title = Some("  US News Map ".to_owned());
        b.utm_source = Some("NewsLetter".to_owned());
        b.utm_medium = Some("".to_owned());
        b.utm_campaign = Some("Fall 2026".to_owned());
        let v = validate(b, "usnewsmap.com").unwrap();
        assert_eq!(v.title.as_deref(), Some("US News Map"));
        assert_eq!(
            v.utm,
            [
                ("utm_source", "newsletter".to_owned()),
                ("utm_campaign", "fall 2026".to_owned())
            ]
        );
        let mut b = beacon("search");
        b.title = Some("x".repeat(MAX_TITLE + 1));
        assert!(validate(b, "usnewsmap.com").is_err());
        let mut b = beacon("search");
        b.utm_source = Some("x".repeat(MAX_UTM + 1));
        assert!(validate(b, "usnewsmap.com").is_err());
        let mut b = beacon("search");
        b.utm_campaign = Some("a\nb".to_owned());
        assert!(validate(b, "usnewsmap.com").is_err());
        let mut b = beacon("search");
        b.title = Some("a\u{7}b".to_owned());
        assert!(validate(b, "usnewsmap.com").is_err());
        // Leading and trailing ones too, which trimming would drop.
        let mut b = beacon("search");
        b.title = Some("\nUS News Map".to_owned());
        assert!(validate(b, "usnewsmap.com").is_err());
        let mut b = beacon("search");
        b.utm_source = Some("newsletter\t".to_owned());
        assert!(validate(b, "usnewsmap.com").is_err());
        let mut b = beacon("search");
        b.referrer_origin = Some("https://example.com\r\n".to_owned());
        assert!(validate(b, "usnewsmap.com").is_err());
    }

    #[test]
    fn bots() {
        for ua in [
            "",
            "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/140.0.0.0 Safari/537.36",
            "Mozilla/5.0 (Linux; Android 11; moto g power (2022)) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Mobile Safari/537.36 Chrome-Lighthouse",
            "facebookexternalhit/1.1",
            "curl/8.7.1",
            "python-requests/2.32",
            "Slackbot-LinkExpanding 1.0",
        ] {
            assert!(is_bot(ua), "{ua}");
        }
        for ua in [
            CHROME,
            IPHONE,
            IPAD,
            ANDROID_TABLET,
            PIXEL_FIREFOX,
            EDGE,
            SAMSUNG,
        ] {
            assert!(!is_bot(ua), "{ua}");
        }
    }

    #[test]
    fn devices_and_browsers() {
        let cases = [
            (CHROME, "desktop", "Chrome"),
            (IPHONE, "mobile", "Safari"),
            (IPAD, "tablet", "Safari"),
            (ANDROID_TABLET, "tablet", "Chrome"),
            (PIXEL_FIREFOX, "mobile", "Firefox"),
            (EDGE, "desktop", "Edge"),
            (SAMSUNG, "mobile", "Samsung Internet"),
        ];
        for (ua, device, family) in cases {
            assert_eq!((device_type(ua), browser(ua)), (device, family), "{ua}");
        }
    }
}
