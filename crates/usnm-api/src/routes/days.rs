//! `GET /v1/days` (06 §6.3.9): matching pages per day for a few places, so
//! the site can find their exact median and quartile dates. A follow-up to
//! the aggregate, not a search of its own: never in the search log.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{OriginalUri, State};
use axum::response::Response;
use serde::Serialize;
use usnm_core::params::RawParams;
use usnm_core::time::BucketUnit;
use usnm_search::plan::{self, PlannedDays};

use super::{cached, uses_fuzzy, with_timeout, Job};
use crate::error::ApiError;
use crate::{version, AppState};

/// Most places one request may ask for.
pub const MAX_PLACES: usize = 20;

#[derive(Serialize)]
struct DaysResponse {
    /// The version the counts come from, so the site can tell them from another snapshot's.
    index_version: String,
    places: Vec<PlaceOut>,
}

#[derive(Serialize)]
struct PlaceOut {
    id: String,
    /// Day numbers, as `places.first_day` in `/v1/aggregate`, ascending.
    days: Vec<u32>,
    /// Matching pages on each of those days.
    hits: Vec<u32>,
}

/// The `place` list: 1 to [`MAX_PLACES`] distinct ids, in the order given.
fn place_ids(value: Option<&str>) -> Result<Vec<String>, ApiError> {
    let value = value.ok_or_else(|| {
        ApiError::BadRequest(format!(
            "give `place`: 1 to {MAX_PLACES} place ids, separated by commas"
        ))
    })?;
    let mut ids: Vec<String> = Vec::new();
    for id in value.split(',').map(str::trim) {
        if id.is_empty() {
            return Err(ApiError::BadRequest("`place` has an empty id".into()));
        }
        if ids.iter().any(|seen| seen == id) {
            return Err(ApiError::BadRequest(format!(
                "`place` lists `{id}` more than once"
            )));
        }
        ids.push(id.to_owned());
    }
    if ids.len() > MAX_PLACES {
        return Err(ApiError::BadRequest(format!(
            "`place` takes at most {MAX_PLACES} place ids"
        )));
    }
    Ok(ids)
}

/// The search parameters of `/v1/aggregate` plus `place`. `bucket` is
/// accepted, as on the other search endpoints, but has no effect.
pub async fn days(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_unknown(&["place"])?;
    let snap = state.snapshot.load_full();
    let rd = &snap.refdata;
    let mut req = rd.search_request(&raw)?;
    // Always by day, so any `bucket` shares one cache key and one URL.
    req.bucket = BucketUnit::Day;
    if uses_fuzzy(&req.query) && !snap.backend.capabilities().fuzzy {
        return Err(ApiError::Unsupported(
            "OCR-tolerant (fuzzy) matching".into(),
        ));
    }
    let places = place_ids(raw.get("place"))?;
    // Congress pages sit at their own reserved place (#229).
    if let Some(p) = places.iter().find(|p| !rd.knows_place(p, req.source)) {
        return Err(ApiError::NotFound(format!("unknown place `{p}`")));
    }

    // Places stay in the order asked: the response lists them so.
    let canonical = {
        let mut canon = form_urlencoded::Serializer::new(req.canonical());
        canon.append_pair("place", &places.join(","));
        canon.finish()
    };
    let serving = rd.version().to_owned();
    let pinning = match version::check(req.version.as_deref(), &serving, uri.path(), &canonical) {
        Ok(p) => p,
        Err(redirect) => return Ok(*redirect),
    };
    // After a stale `v` is redirected, which tells the client the version
    // changed, and before any cache, so a cached Congress response is never
    // served once the version can't search Congress pages (#229).
    rd.check_source(&req)?;
    let key = rd.search_key("days", &canonical, &req.query);
    let st = state.clone();
    let compute = async move {
        let rd = &snap.refdata;
        let indexes = rd.index_set_for_request(&req)?;
        let t = Instant::now();
        let planned = with_timeout(
            &st,
            st.config.compute_cap,
            plan::days(
                snap.backend.as_ref(),
                &indexes,
                &req.query,
                &req.filters,
                &places,
                st.config.max_cells,
            ),
        )
        .await
        .and_then(|planned| match planned {
            PlannedDays::Complete(days) => Ok(days),
            PlannedDays::TooManyCells { upper_bound } => Err(ApiError::TooBroad(format!(
                "these places would have about {upper_bound} days with matching pages, \
                 more than the {} this endpoint returns",
                st.config.max_cells
            ))),
        });
        st.metrics.backend("days", t.elapsed(), &planned);
        Ok::<_, ApiError>(DaysResponse {
            index_version: rd.version().to_owned(),
            places: planned?
                .into_iter()
                .map(|p| PlaceOut {
                    id: p.place_id,
                    days: p.days,
                    hits: p.hits,
                })
                .collect(),
        })
    };
    cached(
        &state,
        Job::search("days", false, state.config.compute_cap),
        key,
        &pinning,
        &serving,
        uri.path(),
        &canonical,
        compute,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bad(value: Option<&str>) -> String {
        match place_ids(value) {
            Err(ApiError::BadRequest(msg)) => msg,
            other => panic!("{value:?}: {:?}", other.map_err(|e| format!("{e:?}"))),
        }
    }

    #[test]
    fn place_lists_keep_their_order() {
        assert_eq!(
            place_ids(Some("P00003, P00001")).unwrap(),
            ["P00003", "P00001"]
        );
        let twenty: Vec<String> = (1..=20).map(|i| format!("P{i:05}")).collect();
        assert_eq!(place_ids(Some(&twenty.join(","))).unwrap(), twenty);
    }

    #[test]
    fn place_lists_are_one_to_twenty_distinct_ids() {
        assert!(bad(None).contains("give `place`"));
        assert!(bad(Some("P00001,")).contains("empty id"));
        assert!(bad(Some("P00001,P00002,P00001")).contains("`P00001` more than once"));
        let many: Vec<String> = (1..=21).map(|i| format!("P{i:05}")).collect();
        assert!(bad(Some(&many.join(","))).contains("at most 20"));
    }
}
