//! `GET /v1/aggregate` (06 §6.3.3): totals, the first and last matching page,
//! national series, per-place totals with first and last appearance, and the
//! sparse place × bucket cube.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Uri};
use axum::response::Response;
use serde::Serialize;
use usnm_core::cube::{Cell, SparseCube};
use usnm_core::params::{RawParams, SearchRequest};
use usnm_core::query::highlight_terms;
use usnm_core::query_language::{baseline_languages, BaselineWhy};
use usnm_core::time::{BucketSpec, BucketUnit};
use usnm_search::plan::{self, Planned};

use super::hits::Item;
use super::{cached_body, mount_prefix, uses_fuzzy, with_timeout, Ctx, Job};
use crate::error::ApiError;
use crate::searchlog::{self, Admission, SearchLog};
use crate::{version, AppState};

#[derive(Serialize)]
struct AggregateResponse {
    index_version: String,
    synthetic: bool,
    query: QueryEcho,
    bucket: BucketEcho,
    total: Totals,
    series: Series,
    places: Places,
    papers: Papers,
    languages: Languages,
    /// Which pages `series.baseline`, `total.baseline_pages` and
    /// `cube.baseline_ref` count; `null` when they're `null` (#237).
    baseline: Option<BaselineOut>,
    cube: CubeOut,
    /// True when buckets were coarsened to keep the cube under the cell cap.
    coarsened: bool,
    timing_ms: Timing,
}

#[derive(Serialize)]
struct QueryEcho {
    canonical: String,
    ast: String,
}

#[derive(Serialize)]
struct BucketEcho {
    unit: BucketUnit,
    from: String,
    to: String,
    count: usize,
}

/// Most newspapers listed in `papers`; `total.papers` counts them all.
pub const MAX_PAPERS_LISTED: usize = 500;

#[derive(Serialize)]
struct Totals {
    hits: u64,
    places: usize,
    /// Newspapers with at least one matching page (#121).
    papers: usize,
    /// Days with at least one matching page (#127); an estimate from
    /// Quickwit on the full index.
    days: u64,
    /// Pages published in scope; `null` when filters make the baseline inexact.
    baseline_pages: Option<u64>,
    /// Earliest and latest matching day; `null` when nothing matches.
    first_day: Option<u32>,
    last_day: Option<u32>,
    /// The pages on those days that sort first and last in `/v1/hits`.
    first: Option<Item>,
    last: Option<Item>,
    /// Matching pages that match in American Stories' text but not in
    /// LoC's (05 §5.5.4): the hits whose `matched_in` is
    /// `["american_stories"]`. Only when the version searches American
    /// Stories' text.
    #[serde(skip_serializing_if = "Option::is_none")]
    american_stories_only: Option<u64>,
}

#[derive(Serialize)]
struct Series {
    hits: Vec<u64>,
    /// Pages published per bucket; `null` when `lccn` or `front` filters are set,
    /// because baselines are kept per place and day (and per title language), and
    /// when `lang` is set on a version published before baselines were kept per language.
    baseline: Option<Vec<u64>>,
}

#[derive(Serialize)]
struct Places {
    id: Vec<String>,
    hits: Vec<u64>,
    first_day: Vec<u32>,
    last_day: Vec<u32>,
}

/// Matching pages per newspaper, most first (ties by LCCN), at most
/// [`MAX_PAPERS_LISTED`]; name and place from the catalog (#121).
#[derive(Serialize)]
struct Papers {
    lccn: Vec<String>,
    hits: Vec<u64>,
    title: Vec<Option<String>>,
    place_id: Vec<Option<String>>,
}

/// Matching pages per title language, most first. A page of a paper
/// catalogued in several languages counts in each, so these can add up to
/// more than `total.hits` (#121).
#[derive(Serialize)]
struct Languages {
    code: Vec<String>,
    hits: Vec<u64>,
}

/// The pages a search is compared with (#237): those of titles that list
/// any of `languages`, or every page when it's empty, and why. The hits are
/// never filtered by it: without `lang` they count every matching page,
/// including the few in titles catalogued in other languages.
#[derive(Serialize)]
struct BaselineOut {
    languages: Vec<String>,
    /// `filter` (the `lang` parameter), `query_language` (the query's words
    /// are Japanese, or English by `usnm_core::query_language`) or `all`
    /// (neither: every page).
    why: &'static str,
}

#[derive(Serialize)]
struct CubeOut {
    #[serde(flatten)]
    cells: SparseCube,
    calls: u8,
    baseline_ref: Option<String>,
}

#[derive(Serialize)]
struct Timing {
    backend: u128,
    total: u128,
}

/// The response caches' key for the search for `query` (canonical
/// parameters `canonical`) on `rd`'s version.
pub(crate) fn cache_key(
    rd: &crate::refdata::RefData,
    canonical: &str,
    query: &usnm_core::query::Node,
) -> String {
    rd.search_key("aggregate", canonical, query)
}

/// A visitor's search, and whether it goes in the search log. Every search
/// on the site requests this endpoint once, so this is where it's counted
/// (06 §6.8).
pub async fn aggregate(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let ctx = Ctx::serving(&state);
    let log = state
        .search_log
        .as_deref()
        .map(|log| (log, searchlog::admit(&headers, &state.config.site_host)));
    aggregate_in(&state, ctx, &uri, log).await
}

/// `log` is `None` for the warm-up, which is never recorded.
pub(crate) async fn aggregate_in(
    state: &Arc<AppState>,
    ctx: Ctx,
    uri: &Uri,
    log: Option<(&SearchLog, Admission)>,
) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_unknown(&["format"])?;
    if raw.get("format").is_some_and(|f| f != "json") {
        return Err(ApiError::Unsupported("format other than json".into()));
    }
    let snap = &ctx.snap;
    let req = snap.refdata.search_request(&raw)?;
    if uses_fuzzy(&req.query) && !snap.backend.capabilities().fuzzy {
        return Err(ApiError::Unsupported(
            "OCR-tolerant (fuzzy) matching".into(),
        ));
    }
    let serving = snap.refdata.version().to_owned();
    let canonical = req.canonical();
    let pinning = match version::check(req.version.as_deref(), &serving, uri.path(), &canonical) {
        Ok(p) => p,
        Err(redirect) => return Ok(*redirect),
    };
    // After a stale `v` is redirected, which tells the client the version
    // changed, and before any cache, so a cached Congress response is never
    // served once the version can't search Congress pages (#229).
    snap.refdata.check_source(&req)?;
    let key = cache_key(&snap.refdata, &canonical, &req.query);
    let prefix = mount_prefix(uri.path(), "/aggregate").to_owned();
    let job = Job::search("aggregate", ctx.warm_up, ctx.timeout);
    let search = match log {
        Some((_, Admission::Record)) => Some(searchlog::Search::new(&raw, &req, &serving)),
        _ => None,
    };
    let fut = compute(state.clone(), ctx, req, canonical.clone(), prefix);
    let (resp, body) = cached_body(
        state,
        job,
        key,
        &pinning,
        &serving,
        uri.path(),
        &canonical,
        fut,
    )
    .await?;
    // Still computing (`202`): nothing counts yet. The client asks again, and
    // the request that gets the results is the one that counts.
    let Some(body) = body else {
        return Ok(resp);
    };
    // Only a search that got its results counts, whichever cache served it.
    match (log, search) {
        (Some((log, _)), Some(search)) => {
            log.record(searchlog::Entry::new(chrono::Utc::now(), search, body))
        }
        (Some((log, Admission::Excluded(reason))), _) => log.excluded(reason),
        _ => {}
    }
    Ok(resp)
}

fn coarser(unit: BucketUnit) -> Option<BucketUnit> {
    match unit {
        BucketUnit::Day => Some(BucketUnit::Week),
        BucketUnit::Week => Some(BucketUnit::Month),
        BucketUnit::Month => Some(BucketUnit::Year),
        BucketUnit::Year => None,
    }
}

async fn compute(
    state: Arc<AppState>,
    ctx: Ctx,
    req: SearchRequest,
    canonical: String,
    prefix: String,
) -> Result<AggregateResponse, ApiError> {
    let started = Instant::now();
    let snap = &ctx.snap;
    let rd = &snap.refdata;
    let indexes = rd.index_set_for_request(&req)?;
    let mut spec = req.bucket_spec();
    let mut coarsened = false;
    if let Some((term, delay)) = &state.config.fixture_slow {
        // End-to-end tests: stand in for a cold search (memory backend only).
        if req.query.to_string().to_lowercase().contains(term.as_str()) {
            tokio::time::sleep(*delay).await;
        }
    }
    let t = Instant::now();
    // The planner checks places-with-hits × buckets before issuing the cube,
    // so an oversized cube is coarsened here instead of failing in the engine.
    let agg = async {
        loop {
            let planned = with_timeout(
                &state,
                ctx.timeout,
                plan::aggregate(
                    snap.backend.as_ref(),
                    &indexes,
                    &req.query,
                    &req.filters,
                    &spec,
                    state.config.max_cells,
                ),
            )
            .await?;
            match planned {
                Planned::Complete(agg) => break Ok(*agg),
                Planned::TooManyCells { upper_bound } => match coarser(spec.unit) {
                    Some(unit) => {
                        spec = BucketSpec::new(unit, spec.from, spec.to);
                        coarsened = true;
                    }
                    None => {
                        return Err(ApiError::TooBroad(format!(
                            "this search would produce about {upper_bound} map cells even by year"
                        )))
                    }
                },
            }
        }
    }
    .await;
    if !ctx.warm_up {
        state.metrics.backend("aggregate", t.elapsed(), &agg);
    }
    let agg = agg?;
    let backend_ms = t.elapsed().as_millis();

    let mut place_ids: Vec<&str> = agg
        .summary
        .places
        .iter()
        .map(|p| p.place_id.as_str())
        .collect();
    place_ids.sort_unstable();
    let index_of = |id: &str| place_ids.binary_search(&id).ok().map(|i| i as u32);
    let by_id: std::collections::HashMap<&str, &usnm_search::PlaceSummary> = agg
        .summary
        .places
        .iter()
        .map(|p| (p.place_id.as_str(), p))
        .collect();
    let cells: Vec<Cell> = agg
        .cells
        .iter()
        .filter_map(|c| {
            Some(Cell {
                place: index_of(&c.place_id)?,
                bucket: c.bucket,
                hits: c.hits,
            })
        })
        .collect();

    let highlight = highlight_terms(&req.query).join(" ");
    let f = &req.filters;
    // Without `lang`, the baseline is the pages in the query's language when
    // the query says what that is (#237): a Japanese query searches only
    // pages of titles that list Japanese (#139), and an English one finds
    // mostly English pages. The hits stay unfiltered: a `lang=eng` filter
    // makes cold searches about 40% slower and prunes nothing.
    let (mut langs, mut why) = baseline_languages(&req.query, &f.langs);
    if why == BaselineWhy::QueryLanguage && langs == ["eng"] && !rd.has_baselines_for(&langs) {
        // A version from before pages were counted per language: every page,
        // as before, rather than no baseline at all.
        (langs, why) = (Vec::new(), BaselineWhy::All);
    }
    // Congress pages have baselines of their own, per series and day (#229
    // design §4), which come with the first Congress data; until then a
    // Congress search answers with page counts only.
    let congress = req.source.congress();
    let baseline_exact =
        !congress && f.lccns.is_empty() && !f.front_only && rd.has_baselines_for(&langs);
    let baseline = baseline_exact.then(|| rd.national_baseline(&spec, &f.states, &langs));
    let baseline_ref = baseline_exact.then(|| {
        let mut s = form_urlencoded::Serializer::new(String::new());
        s.append_pair("bucket", spec.unit.as_str());
        s.append_pair("from", &spec.from.to_string());
        if !langs.is_empty() {
            s.append_pair("lang", &langs.join(","));
        }
        if !f.states.is_empty() {
            s.append_pair("state", &f.states.join(","));
        }
        s.append_pair("to", &spec.to.to_string());
        s.append_pair("v", rd.version());
        // Same mount as this request (`/v1` or `/api/v1`), so the link stays on the API.
        format!("{prefix}/coverage?{}", s.finish())
    });

    Ok(AggregateResponse {
        index_version: rd.version().to_owned(),
        synthetic: rd.current.synthetic,
        query: QueryEcho {
            canonical,
            ast: req.query.to_string(),
        },
        bucket: BucketEcho {
            unit: spec.unit,
            from: spec.from.to_string(),
            to: spec.to.to_string(),
            count: spec.len(),
        },
        total: Totals {
            hits: agg.summary.total_hits,
            places: place_ids.len(),
            papers: if congress {
                0
            } else {
                agg.summary.papers.len()
            },
            days: agg.summary.days,
            baseline_pages: baseline.as_ref().map(|b| b.iter().sum()),
            first_day: agg.summary.first_day,
            last_day: agg.summary.last_day,
            first: agg.first.map(|h| Item::new(h, rd, &highlight)),
            last: agg.last.map(|h| Item::new(h, rd, &highlight)),
            american_stories_only: agg.american_stories_only,
        },
        series: Series {
            hits: agg.summary.series.clone(),
            baseline,
        },
        places: Places {
            hits: place_ids.iter().map(|id| by_id[id].hits).collect(),
            first_day: place_ids.iter().map(|id| by_id[id].first_day).collect(),
            last_day: place_ids.iter().map(|id| by_id[id].last_day).collect(),
            id: place_ids.iter().map(|s| (*s).to_owned()).collect(),
        },
        papers: {
            // Congress pages belong to no newspaper (their `lccn` is empty).
            let papers: &[_] = if congress { &[] } else { &agg.summary.papers };
            let listed = &papers[..papers.len().min(MAX_PAPERS_LISTED)];
            let title = |lccn: &str| rd.titles.get(lccn);
            Papers {
                lccn: listed.iter().map(|p| p.key.clone()).collect(),
                hits: listed.iter().map(|p| p.hits).collect(),
                title: listed
                    .iter()
                    .map(|p| title(&p.key).map(|t| t.name.clone()))
                    .collect(),
                place_id: listed
                    .iter()
                    .map(|p| title(&p.key).map(|t| t.place_id.clone()))
                    .collect(),
            }
        },
        baseline: baseline_exact.then(|| BaselineOut {
            languages: langs,
            why: why.as_str(),
        }),
        languages: Languages {
            code: agg
                .summary
                .languages
                .iter()
                .map(|l| l.key.clone())
                .collect(),
            hits: agg.summary.languages.iter().map(|l| l.hits).collect(),
        },
        cube: CubeOut {
            cells: SparseCube::from_cells(cells),
            calls: agg.cube_calls,
            baseline_ref,
        },
        coarsened,
        timing_ms: Timing {
            backend: backend_ms,
            total: started.elapsed().as_millis(),
        },
    })
}
