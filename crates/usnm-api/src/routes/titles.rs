//! `GET /v1/titles` and `GET /v1/titles/{lccn}` (06 §6.3.11): the newspapers
//! in the published version and a summary of each, from the reference
//! snapshot alone, for the site's newspaper picker (07 §7.13).

use std::sync::Arc;

use axum::extract::{OriginalUri, Path, State};
use axum::http::Uri;
use axum::response::Response;
use serde::Serialize;
use usnm_core::params::RawParams;

use super::{cached, Ctx, Job};
use crate::error::ApiError;
use crate::refdata::{RefData, Title};
use crate::{version, AppState};

#[derive(Serialize)]
struct TitlesResponse {
    index_version: String,
    titles: Vec<TitleOut>,
}

#[derive(Serialize)]
struct TitleResponse {
    index_version: String,
    #[serde(flatten)]
    title: TitleOut,
    links: Links,
}

/// One newspaper: its catalog record joined with its place and its pages.
#[derive(Serialize)]
struct TitleOut {
    lccn: String,
    title: String,
    place_id: String,
    /// The place's name, as `/v1/places` has it.
    place: Option<String>,
    state: String,
    first_year: Option<u16>,
    last_year: Option<u16>,
    /// Pages in the version; `null` for a snapshot without `title_pages.json`.
    pages: Option<u64>,
    languages: Vec<String>,
}

#[derive(Serialize)]
struct Links {
    /// The newspaper's page at the Library of Congress, where its issues can
    /// be browsed by date.
    loc: String,
}

fn title_out(rd: &RefData, t: &Title) -> TitleOut {
    TitleOut {
        lccn: t.lccn.clone(),
        title: t.name.clone(),
        place_id: t.place_id.clone(),
        place: rd.place(&t.place_id).map(|p| p.name.clone()),
        state: t.state.clone(),
        first_year: t.first_year,
        last_year: t.last_year,
        pages: rd
            .title_pages
            .as_ref()
            .map(|p| p.get(&t.lccn).copied().unwrap_or(0)),
        languages: t.languages.clone(),
    }
}

/// Every newspaper in the version, by LCCN, versioned like `/v1/places`.
pub async fn titles(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let ctx = Ctx::serving(&state);
    titles_in(&state, ctx, &uri).await
}

async fn titles_in(state: &Arc<AppState>, ctx: Ctx, uri: &Uri) -> Result<Response, ApiError> {
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_only(&["v"])?;
    let job = Job::reference("titles", ctx.warm_up, ctx.timeout);
    let snap = ctx.snap;
    let serving = snap.refdata.version().to_owned();
    let pinning = match version::check(raw.get("v"), &serving, uri.path(), "") {
        Ok(p) => p,
        Err(redirect) => return Ok(*redirect),
    };
    // Reference data alone, like places: no American Stories mark.
    let key = format!("{serving}|titles|");
    let compute = async move {
        let rd = &snap.refdata;
        let mut titles: Vec<&Title> = rd.titles.values().collect();
        titles.sort_unstable_by(|a, b| a.lccn.cmp(&b.lccn));
        Ok::<_, ApiError>(TitlesResponse {
            index_version: rd.version().to_owned(),
            titles: titles.into_iter().map(|t| title_out(rd, t)).collect(),
        })
    };
    cached(state, job, key, &pinning, &serving, uri.path(), "", compute).await
}

/// One newspaper, with its page at the Library of Congress.
pub async fn title(
    State(state): State<Arc<AppState>>,
    Path(lccn): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Response, ApiError> {
    let ctx = Ctx::serving(&state);
    // The `lccn` parameter's rule (06 §6.3.1). Checked first, so the
    // redirect below only ever names an LCCN.
    let shaped = !lccn.is_empty()
        && lccn.len() <= 16
        && lccn
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if !shaped {
        return Err(ApiError::NotFound(format!("unknown title `{lccn}`")));
    }
    let raw = RawParams::parse(uri.query().unwrap_or(""))?;
    raw.reject_only(&["v"])?;
    let job = Job::reference("titles", ctx.warm_up, ctx.timeout);
    let snap = ctx.snap;
    let serving = snap.refdata.version().to_owned();
    // A title of an older version may be gone from this one: redirect first,
    // so the client learns of the new version rather than a 404.
    let pinning = match version::check(raw.get("v"), &serving, uri.path(), "") {
        Ok(p) => p,
        Err(redirect) => return Ok(*redirect),
    };
    if !snap.refdata.titles.contains_key(&lccn) {
        return Err(ApiError::NotFound(format!("unknown title `{lccn}`")));
    }
    let key = format!("{serving}|title|{lccn}");
    let compute = async move {
        let rd = &snap.refdata;
        let t = &rd.titles[&lccn];
        Ok::<_, ApiError>(TitleResponse {
            index_version: rd.version().to_owned(),
            title: title_out(rd, t),
            links: Links {
                loc: format!("https://www.loc.gov/item/{}/", t.lccn),
            },
        })
    };
    cached(
        &state,
        job,
        key,
        &pinning,
        &serving,
        uri.path(),
        "",
        compute,
    )
    .await
}
