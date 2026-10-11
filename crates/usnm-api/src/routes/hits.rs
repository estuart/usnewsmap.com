//! `GET /v1/hits` (06 §6.3.4): pages for one place or one title (`lccn`),
//! sorted by date (`sort=oldest`, the default, or `newest`) or by how often
//! they mention the query (`relevant`, #126), with snippets.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{OriginalUri, State};
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use usnm_core::ids::PageKey;
use usnm_core::params::RawParams;
use usnm_core::query::highlight_terms;
use usnm_core::time::date_from_day;
use usnm_search::{Hit, HitSort, HitsQuery};

use super::{cached, uses_fuzzy, with_timeout, Job};
use crate::error::ApiError;
use crate::refdata::{RefData, CONGRESS_PLACE, CONGRESS_PLACE_NAME};
use crate::{version, AppState};

pub const MAX_LIMIT: usize = 50;
/// Deepest page reachable by offset pagination.
pub const MAX_OFFSET: usize = 10_000;

#[derive(Serialize, Deserialize)]
struct Cursor {
    o: usize,
}

#[derive(Serialize)]
struct HitsResponse {
    index_version: String,
    synthetic: bool,
    place: Option<PlaceOut>,
    title: Option<TitleOut>,
    total: u64,
    /// Days with at least one of these pages (#127), on the first page of a
    /// list only; an estimate from Quickwit on the full index.
    #[serde(skip_serializing_if = "Option::is_none")]
    days: Option<u64>,
    items: Vec<Item>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct PlaceOut {
    id: String,
    name: String,
    state: String,
}

#[derive(Serialize)]
struct TitleOut {
    lccn: String,
    name: String,
}

/// One page, as `/v1/hits` lists it and `/v1/aggregate` names the first and last.
#[derive(Serialize)]
pub(crate) struct Item {
    doc_id: String,
    date: String,
    lccn: String,
    title: Option<String>,
    place_id: String,
    edition: u16,
    seq: u16,
    front_page: bool,
    snippets: Vec<String>,
    /// Where the snippets come from when it isn't LoC's text:
    /// `american_stories` when the query matched only in American Stories'
    /// text (05 §5.5.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    snippet_source: Option<&'static str>,
    /// Which of the page's texts the query matches: `["loc"]`,
    /// `["american_stories"]` or both (05 §5.5.4). Only when the version
    /// searches American Stories' text, and not on Japanese pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    matched_in: Option<Vec<&'static str>>,
    /// When the text is our own OCR, not LoC's: the Japanese index's pages
    /// (#139), and the main index's pages with our OCR's Latin text (#203).
    #[serde(skip_serializing_if = "Option::is_none")]
    ocr: Option<Ocr>,
    /// A Congress page's series and citation (#229). Its `title` is the
    /// document's or volume's, its `lccn` empty, and its viewer link the
    /// page in its source (GovInfo, UNT or Congress.gov).
    #[serde(skip_serializing_if = "Option::is_none")]
    congress: Option<CongressOut>,
    links: Links,
}

#[derive(Serialize)]
struct CongressOut {
    series: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    citation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    congress: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    chamber: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    printed_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    package_id: Option<String>,
    /// Who made the page's text: `govinfo`, `unt`, our OCR
    /// (`usnm-tesseract`) or `fixture`. Not `ocr`, which the site shows as
    /// our Japanese OCR (#229).
    #[serde(skip_serializing_if = "Option::is_none")]
    text_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text_engine: Option<String>,
}

/// Who made a page's text when it isn't LoC's OCR.
#[derive(Serialize)]
struct Ocr {
    source: String,
    engine: Option<String>,
}

#[derive(Serialize)]
struct Links {
    viewer: Option<String>,
}

impl Item {
    /// `highlight` is the query's highlight terms, for the LoC viewer link.
    pub(crate) fn new(h: Hit, rd: &RefData, highlight: &str) -> Self {
        if let Some(series) = h.congress.series.clone() {
            let c = h.congress;
            return Item {
                date: date_from_day(h.day).to_string(),
                title: c.title,
                // Its source's viewer can't highlight our words either.
                links: Links { viewer: c.link },
                // Who made its text goes with the citation (#229).
                ocr: None,
                congress: Some(CongressOut {
                    series,
                    citation: c.citation,
                    congress: c.congress,
                    session: c.session,
                    chamber: c.chamber,
                    printed_page: c.printed_page,
                    package_id: c.package_id,
                    text_source: h.ocr_source,
                    text_engine: h.ocr_engine,
                }),
                doc_id: h.doc_id,
                lccn: h.lccn,
                place_id: h.place_id,
                edition: h.edition,
                seq: h.seq,
                front_page: h.front_page,
                snippets: h.snippets,
                snippet_source: h.snippet_source,
                matched_in: h.matched_in,
            };
        }
        // A Japanese page says so itself; a main-index page with our Latin
        // text is in the snapshot's list.
        let ocr = match h.ocr_source {
            Some(source) => Some(Ocr {
                source,
                engine: h.ocr_engine,
            }),
            None => rd.ja_latin.get(&h.doc_id).map(|p| Ocr {
                source: p.ocr_source.clone(),
                engine: Some(p.ocr_engine.clone()),
            }),
        };
        Item {
            date: date_from_day(h.day).to_string(),
            title: rd.titles.get(&h.lccn).map(|t| t.name.clone()),
            links: Links {
                // LoC has no text for a page we OCR'd, so its viewer can't
                // highlight the words: link the page without them.
                viewer: PageKey::from_doc_id(&h.doc_id)
                    .ok()
                    .map(|k| k.viewer_url(ocr.is_none().then_some(highlight))),
            },
            ocr,
            congress: None,
            doc_id: h.doc_id,
            lccn: h.lccn,
            place_id: h.place_id,
            edition: h.edition,
            seq: h.seq,
            front_page: h.front_page,
            snippets: h.snippets,
            snippet_source: h.snippet_source,
            matched_in: h.matched_in,
        }
    }
}

/// Select hits by `place=`, or by title with a single `lccn=` (the same
/// parameter that filters the other endpoints). With `place`, `lccn` stays a filter.
pub async fn hits(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_unknown(&["place", "cursor", "limit", "sort"])?;
    let snap = state.snapshot.load_full();
    let rd = &snap.refdata;
    let req = rd.search_request(&raw)?;
    if uses_fuzzy(&req.query) && !snap.backend.capabilities().fuzzy {
        return Err(ApiError::Unsupported(
            "OCR-tolerant (fuzzy) matching".into(),
        ));
    }
    let place = raw.get("place").map(str::to_owned);
    let title = match (&place, req.filters.lccns.as_slice()) {
        (Some(p), _) => {
            // Congress pages sit at their own reserved place (#229).
            if !rd.knows_place(p, req.source) {
                return Err(ApiError::NotFound(format!("unknown place `{p}`")));
            }
            None
        }
        (None, [lccn]) => {
            if !rd.titles.contains_key(lccn) {
                return Err(ApiError::NotFound(format!("unknown title `{lccn}`")));
            }
            Some(lccn.clone())
        }
        (None, _) => {
            return Err(ApiError::BadRequest(
                "give `place`, or a single `lccn` to list one title's pages".into(),
            ))
        }
    };
    let limit = match raw.get("limit") {
        None => MAX_LIMIT,
        Some(l) => l
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| ApiError::BadRequest(format!("`limit` must be 1–{MAX_LIMIT}")))?,
    };
    let sort = match raw.get("sort") {
        None => HitSort::default(),
        Some(s) => HitSort::parse(s).ok_or_else(|| {
            ApiError::BadRequest("`sort` must be `oldest`, `newest` or `relevant`".into())
        })?,
    };
    let offset = match raw.get("cursor") {
        None => 0,
        Some(c) => URL_SAFE_NO_PAD
            .decode(c)
            .ok()
            .and_then(|b| serde_json::from_slice::<Cursor>(&b).ok())
            .map(|c| c.o)
            .filter(|o| *o <= MAX_OFFSET)
            .ok_or_else(|| ApiError::BadRequest("invalid `cursor`".into()))?,
    };

    let canonical = {
        let mut canon = form_urlencoded::Serializer::new(req.canonical());
        if let Some(c) = raw.get("cursor") {
            canon.append_pair("cursor", c);
        }
        canon.append_pair("limit", &limit.to_string());
        if let Some(p) = &place {
            canon.append_pair("place", p);
        }
        // Only when not the default, so `sort=oldest` and no `sort` share a key.
        if sort != HitSort::default() {
            canon.append_pair("sort", sort.as_str());
        }
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
    let key = rd.search_key("hits", &canonical, &req.query);
    let st = state.clone();
    let snap2 = snap.clone();
    let compute = async move {
        let rd = &snap2.refdata;
        let page = HitsQuery {
            place_id: place.clone(),
            lccn: title.clone(),
            sort,
            offset,
            limit,
            // The first page says on how many days the pages appeared (#127).
            days: offset == 0,
        };
        let t = Instant::now();
        let result = with_timeout(
            &st,
            st.config.compute_cap,
            snap2.backend.hits(
                &rd.index_set_for_request(&req)?,
                &req.query,
                &req.filters,
                &page,
            ),
        )
        .await;
        st.metrics.backend("hits", t.elapsed(), &result);
        let result = result?;
        let highlight = highlight_terms(&req.query).join(" ");
        let next = offset + result.hits.len();
        let next_cursor = (next < result.total as usize && next <= MAX_OFFSET).then(|| {
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor { o: next }).unwrap_or_default())
        });
        Ok::<_, ApiError>(HitsResponse {
            index_version: rd.version().to_owned(),
            synthetic: rd.current.synthetic,
            place: place.as_deref().and_then(|p| match rd.place(p) {
                Some(p) => Some(PlaceOut {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    state: p.state.clone(),
                }),
                // The Congress place isn't in the newspapers' catalog (#229).
                None if req.source.congress() && p == CONGRESS_PLACE => Some(PlaceOut {
                    id: CONGRESS_PLACE.to_owned(),
                    name: CONGRESS_PLACE_NAME.to_owned(),
                    state: "DC".to_owned(),
                }),
                None => None,
            }),
            title: title
                .as_deref()
                .and_then(|t| rd.titles.get(t))
                .map(|t| TitleOut {
                    lccn: t.lccn.clone(),
                    name: t.name.clone(),
                }),
            total: result.total,
            days: result.days,
            items: result
                .hits
                .into_iter()
                .map(|h| Item::new(h, rd, &highlight))
                .collect(),
            next_cursor,
        })
    };
    cached(
        &state,
        Job::search("hits", false, state.config.compute_cap),
        key,
        &pinning,
        &serving,
        uri.path(),
        &canonical,
        compute,
    )
    .await
}
