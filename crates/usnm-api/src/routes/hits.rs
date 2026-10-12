//! `GET /v1/hits` (06 §6.3.4): pages for one place or one title (`lccn`),
//! sorted by date (`sort=oldest`, the default, or `newest`) or by how often
//! they mention the query (`relevant`, #126), with snippets. With
//! `USNM_BROWSE_PAGES` on, one title's pages without a query (#274).

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{OriginalUri, State};
use axum::http::Uri;
use axum::response::Response;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use usnm_core::ids::PageKey;
use usnm_core::params::{BrowseRequest, RawParams};
use usnm_core::query::highlight_terms;
use usnm_core::time::date_from_day;
use usnm_search::{BrowseKey, Hit, HitSort, HitsPage, HitsQuery};

use super::{cached, uses_fuzzy, with_timeout, Job};
use crate::error::ApiError;
use crate::refdata::{RefData, CONGRESS_PLACE, CONGRESS_PLACE_NAME};
use crate::{version, AppState};

pub const MAX_LIMIT: usize = 50;
/// A search's cursor: the offset of its next page, at most
/// `Config::max_offset` (10,000).
#[derive(Serialize, Deserialize)]
struct Cursor {
    o: usize,
}

/// A list of a title's pages without `q` (#274) resumes after the last page
/// listed rather than at an offset, so it reaches every page however many
/// the title has: `d` and `k` are that page's [`BrowseKey`] (day and
/// `sort_key`), and `s` the order it was listed in, which a request must
/// repeat. It carries no count: anyone can write a cursor, so a later
/// page's `total` is what the backend counts after the key, never a number
/// the client sent. A search's cursor (`{"o":…}`)
/// isn't one, nor is this a search's.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowseCursor {
    d: u32,
    k: u64,
    s: String,
}

impl BrowseCursor {
    /// The cursor after `last`, listed in `sort` order.
    fn encode(last: BrowseKey, sort: HitSort) -> String {
        let c = BrowseCursor {
            d: last.day,
            k: last.sort_key,
            s: sort.as_str().to_owned(),
        };
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&c).unwrap_or_default())
    }

    /// The key to resume after; 400 for anything that isn't a browse cursor
    /// of `sort` order.
    fn decode(c: &str, sort: HitSort) -> Result<BrowseKey, ApiError> {
        URL_SAFE_NO_PAD
            .decode(c)
            .ok()
            .and_then(|b| serde_json::from_slice::<BrowseCursor>(&b).ok())
            .filter(|c| c.s == sort.as_str())
            .map(|c| BrowseKey {
                day: c.d,
                sort_key: c.k,
            })
            .ok_or_else(|| ApiError::BadRequest("invalid `cursor`".into()))
    }
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
    /// (#139), the main index's pages with our OCR's Latin text (#203), and
    /// those with our OCR in place of LoC's (12 §12.5 Step 9).
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
        // text, or with our own OCR in place of LoC's (12 §12.5 Step 9), is
        // in the snapshot's lists.
        let ocr = match h.ocr_source {
            Some(source) => Some(Ocr {
                source,
                engine: h.ocr_engine,
            }),
            None => rd
                .ja_latin
                .get(&h.doc_id)
                .map(|p| Ocr {
                    source: p.ocr_source.clone(),
                    engine: Some(p.ocr_engine.clone()),
                })
                .or_else(|| {
                    let e = rd.ocr_local.get(&h.doc_id)?;
                    let engine = rd.ocr_local.engine(e)?;
                    Some(Ocr {
                        source: engine.source.clone(),
                        engine: Some(engine.engine.clone()),
                    })
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
/// Without `q`, and with `USNM_BROWSE_PAGES` on, one title's pages ([`browse`]).
pub async fn hits(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_unknown(&["place", "cursor", "limit", "sort"])?;
    if raw.get("q").is_none() && state.config.browse_pages {
        return browse(state, &uri, &raw).await;
    }
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
            known_title(rd, lccn)?;
            Some(lccn.clone())
        }
        (None, _) => {
            return Err(ApiError::BadRequest(
                "give `place`, or a single `lccn` to list one title's pages".into(),
            ))
        }
    };
    let PageParams {
        limit,
        sort,
        offset,
    } = PageParams::parse(&raw, state.config.max_offset)?;

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
        let place = place.as_deref().and_then(|p| match rd.place(p) {
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
        });
        let next_cursor = offset_cursor(offset, &result, st.config.max_offset);
        Ok::<_, ApiError>(respond(
            rd,
            place,
            title.as_deref(),
            result,
            next_cursor,
            &highlight,
        ))
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

/// A title the version has, or 404.
fn known_title(rd: &RefData, lccn: &str) -> Result<(), ApiError> {
    if rd.titles.contains_key(lccn) {
        Ok(())
    } else {
        Err(ApiError::NotFound(format!("unknown title `{lccn}`")))
    }
}

/// Which part of a list to answer, in which order.
struct PageParams {
    limit: usize,
    sort: HitSort,
    offset: usize,
}

/// `limit` and `sort`, for a search or a browse list.
fn limit_and_sort(raw: &RawParams) -> Result<(usize, HitSort), ApiError> {
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
    Ok((limit, sort))
}

impl PageParams {
    /// A search's: its cursor is an offset, at most `max_offset`.
    fn parse(raw: &RawParams, max_offset: usize) -> Result<Self, ApiError> {
        let (limit, sort) = limit_and_sort(raw)?;
        let offset = match raw.get("cursor") {
            None => 0,
            Some(c) => URL_SAFE_NO_PAD
                .decode(c)
                .ok()
                .and_then(|b| serde_json::from_slice::<Cursor>(&b).ok())
                .map(|c| c.o)
                .filter(|o| *o <= max_offset)
                .ok_or_else(|| ApiError::BadRequest("invalid `cursor`".into()))?,
        };
        Ok(Self {
            limit,
            sort,
            offset,
        })
    }
}

/// A search's next cursor after `result`, its page from `offset`: none at
/// the end, or past `max_offset`.
fn offset_cursor(offset: usize, result: &HitsPage, max_offset: usize) -> Option<String> {
    let next = offset + result.hits.len();
    (next < result.total as usize && next <= max_offset).then(|| {
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Cursor { o: next }).unwrap_or_default())
    })
}

/// The response for a page of `result`, with `next_cursor`.
fn respond(
    rd: &RefData,
    place: Option<PlaceOut>,
    title: Option<&str>,
    result: HitsPage,
    next_cursor: Option<String>,
    highlight: &str,
) -> HitsResponse {
    HitsResponse {
        index_version: rd.version().to_owned(),
        synthetic: rd.current.synthetic,
        place,
        title: title.and_then(|t| rd.titles.get(t)).map(|t| TitleOut {
            lccn: t.lccn.clone(),
            name: t.name.clone(),
        }),
        total: result.total,
        days: result.days,
        items: result
            .hits
            .into_iter()
            .map(|h| Item::new(h, rd, highlight))
            .collect(),
        next_cursor,
    }
}

/// One title's pages without a query (#274, `USNM_BROWSE_PAGES`): every
/// page of the single `lccn` the other filters select, by date (`sort` is
/// `oldest` or `newest`), each with the opening of its text as its snippet.
/// The response is a search's, with no `place`. Newspaper pages only, from
/// the main indexes, which hold every page of a title (a Japanese page's
/// copy in the Japanese index is searched by Japanese words only).
async fn browse(state: Arc<AppState>, uri: &Uri, raw: &RawParams) -> Result<Response, ApiError> {
    let snap = state.snapshot.load_full();
    let rd = &snap.refdata;
    let req = BrowseRequest::from_raw(raw, rd.bounds_for(None))?;
    if raw.get("place").is_some() {
        return Err(ApiError::BadRequest(
            "without `q`, give a single `lccn` and no `place` to list one title's pages".into(),
        ));
    }
    let title = match req.filters.lccns.as_slice() {
        [lccn] => lccn.clone(),
        _ => {
            return Err(ApiError::BadRequest(
                "without `q`, give a single `lccn` to list one title's pages".into(),
            ))
        }
    };
    let (limit, sort) = limit_and_sort(raw)?;
    if sort == HitSort::Relevant {
        return Err(ApiError::BadRequest(
            "`sort=relevant` needs `q`: without one, pages are listed by date (`oldest` or `newest`)"
                .into(),
        ));
    }
    // Not an offset: the last page's key, so no depth is out of reach.
    let after = raw
        .get("cursor")
        .map(|c| BrowseCursor::decode(c, sort))
        .transpose()?;
    // A search's canonical URL always has `q`; this one never does, so the
    // two never share a cache key.
    let canonical = {
        let mut canon = form_urlencoded::Serializer::new(req.canonical());
        if let Some(c) = raw.get("cursor") {
            canon.append_pair("cursor", c);
        }
        canon.append_pair("limit", &limit.to_string());
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
    // After a stale `v` is redirected: a title of an older version may be
    // gone from this one, and the client learns of the new version from the
    // redirect rather than a 404 at the old URL (as `/v1/titles/{lccn}`).
    known_title(rd, &title)?;
    // No query, so no American Stories mark: the list is the same whether
    // or not searches cover that text.
    let key = format!("{serving}|hits|{canonical}");
    let st = state.clone();
    let snap2 = snap.clone();
    let compute = async move {
        let rd = &snap2.refdata;
        let page = HitsQuery {
            place_id: None,
            lccn: Some(title.clone()),
            sort,
            offset: 0,
            limit,
            days: after.is_none(),
        };
        let t = Instant::now();
        let result = with_timeout(
            &st,
            st.config.compute_cap,
            snap2
                .backend
                .browse(&rd.index_set(), &req.filters, &page, after),
        )
        .await;
        st.metrics.backend("hits", t.elapsed(), &result);
        let result = result?;
        // `total` is the backend's count from the key on: every page on the
        // first request, those not yet listed on a later one.
        let next_cursor = result
            .hits
            .last()
            .filter(|_| (result.hits.len() as u64) < result.total)
            .map(|last| BrowseCursor::encode(BrowseKey::of(last), sort));
        // No words to highlight: the viewer links have no `q`.
        Ok::<_, ApiError>(respond(rd, None, Some(&title), result, next_cursor, ""))
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
