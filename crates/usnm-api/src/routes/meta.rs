use std::sync::Arc;
use std::time::Duration;

use axum::extract::{OriginalUri, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use usnm_core::params::RawParams;
use usnm_core::query;

use super::{cached, Ctx, Job};
use crate::error::ApiError;
use crate::{version, AppState};

pub async fn readyz(State(state): State<Arc<AppState>>) -> Response {
    // Not ready until the first version's caches are warm (or the cap passes),
    // so the first visitors after a start don't wait on cold searches.
    if state.warming.load(std::sync::atomic::Ordering::Relaxed) {
        return (StatusCode::SERVICE_UNAVAILABLE, "warming up").into_response();
    }
    let backend = state.snapshot.load().backend.clone();
    match tokio::time::timeout(Duration::from_secs(2), backend.health()).await {
        Ok(Ok(())) => (StatusCode::OK, "ready").into_response(),
        Ok(Err(e)) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "search backend health check timed out",
        )
            .into_response(),
    }
}

/// Unversioned by design: it is how clients learn the current version.
pub async fn meta(State(state): State<Arc<AppState>>) -> Response {
    let snap = state.snapshot.load();
    let rd = &snap.refdata;
    let c = &rd.current;
    let body = json!({
        "index_version": c.index_version,
        "indexes": c.indexes,
        "bounds": c.bounds,
        "published_at": c.published_at,
        "synthetic": c.synthetic,
        "places": rd.places.len(),
        "titles": rd.titles.len(),
        "pages": rd.pages,
        // The Japanese pages we OCR (#139): searched by queries with Japanese words.
        "ja": c.ja,
        // Whether searches cover American Stories' text (05 §5.5.4): the
        // version has it and `USNM_AMERICAN_STORIES_SEARCH` leaves it on,
        // or it shares one field with LoC's (05 §5.5.7).
        "american_stories": rd.searches_american_stories(),
        // A searched field per text (1) or one for both (2), 05 §5.5.7.
        "text_layout": rd.text_layout().version(),
        // The Congress pages (#229), searched with `source=congress`: null
        // unless the version has a Congress set this API can search and
        // `USNM_CONGRESS_SEARCH` leaves it on. Every page sits at one
        // reserved place, Washington, DC, kept apart from DC's newspapers.
        "congress": rd.congress_index_set().and(c.congress.as_ref()).map(|cg| json!({
            "series": cg.series,
            "place": {
                "id": crate::refdata::CONGRESS_PLACE,
                "name": crate::refdata::CONGRESS_PLACE_NAME,
                "lat": 38.8899,
                "lon": -77.0091,
            },
        })),
        "languages": filter_languages(rd),
        "capabilities": snap.backend.capabilities(),
        "limits": {
            "max_query_chars": query::MAX_QUERY_CHARS,
            "max_terms": query::MAX_TERMS,
            "max_or_branches": query::MAX_OR_BRANCHES,
            "max_slop": query::MAX_SLOP,
            "max_fuzzy": query::MAX_FUZZY,
            "min_prefix_chars": query::MIN_PREFIX_CHARS
        }
    });
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    resp
}

/// The choices for the site's language filter: each catalog language the
/// `lang` parameter accepts, with its titles and (when the snapshot records
/// them) pages, most pages first. A title in several languages counts in each,
/// as a search filtered to any of them finds its pages. Languages without a
/// three-letter code, and titles with none recorded, can't be filtered on, so
/// they are left out (06 §6.3.2).
fn filter_languages(rd: &crate::refdata::RefData) -> Vec<serde_json::Value> {
    crate::status::assemble::by_language(rd)
        .rows
        .into_iter()
        .filter_map(|r| {
            let code = r.code?;
            (code.len() == 3 && code.bytes().all(|b| b.is_ascii_lowercase())).then(
                || json!({"code": code, "name": r.name, "titles": r.titles, "pages": r.pages}),
            )
        })
        .collect()
}

/// All places as GeoJSON (06 §6.3.2), versioned like the search endpoints.
pub async fn places(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let ctx = Ctx::serving(&state);
    places_in(&state, ctx, &uri).await
}

pub(crate) async fn places_in(
    state: &Arc<AppState>,
    ctx: Ctx,
    uri: &Uri,
) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_only(&["v"])?;
    let job = Job::reference("places", ctx.warm_up, ctx.timeout);
    let snap = ctx.snap;
    let serving = snap.refdata.version().to_owned();
    let pinning = match version::check(raw.get("v"), &serving, uri.path(), "") {
        Ok(p) => p,
        Err(redirect) => return Ok(*redirect),
    };
    // Reference data alone, like coverage: no American Stories mark.
    let key = format!("{serving}|places|");
    let compute = async move {
        let rd = &snap.refdata;
        let mut title_counts = std::collections::HashMap::<&str, usize>::new();
        // The languages the place's titles are printed in (catalog codes,
        // e.g. "eng", "ger"), with the titles that list each; a title in
        // several languages counts in each. The map's relative-rate view
        // names them for a place where any title isn't in English (doc 11,
        // 11.14).
        let mut languages =
            std::collections::HashMap::<&str, std::collections::BTreeMap<&str, usize>>::new();
        for t in rd.titles.values() {
            *title_counts.entry(t.place_id.as_str()).or_default() += 1;
            let place = languages.entry(t.place_id.as_str()).or_default();
            let own: std::collections::BTreeSet<&str> =
                t.languages.iter().map(String::as_str).collect();
            for l in own {
                *place.entry(l).or_default() += 1;
            }
        }
        let features: Vec<_> = rd
            .places
            .iter()
            .map(|p| {
                json!({
                    "type": "Feature",
                    "id": p.id,
                    "geometry": { "type": "Point", "coordinates": [p.lon, p.lat] },
                    "properties": {
                        "name": p.name,
                        "state": p.state,
                        "precision": p.precision,
                        "titles": title_counts.get(p.id.as_str()).copied().unwrap_or(0),
                        "languages": languages.get(p.id.as_str()).map(|l| l.keys().collect::<Vec<_>>()).unwrap_or_default(),
                        "language_titles": languages.get(p.id.as_str()).cloned().unwrap_or_default()
                    }
                })
            })
            .collect();
        Ok::<_, ApiError>(json!({
            "type": "FeatureCollection",
            "index_version": rd.version(),
            "features": features
        }))
    };
    cached(state, job, key, &pinning, &serving, uri.path(), "", compute).await
}
