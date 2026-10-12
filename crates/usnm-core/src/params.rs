//! Search request parameters and their canonical form (06 §6.3.1).
//!
//! Canonicalization folds `mode`, `near` and `fuzzy` into the query AST, sorts
//! list values, makes `from`/`to`/`bucket` explicit, and renders keys in a fixed
//! order. Equivalent requests therefore share one cache key and one URL.

use chrono::NaiveDate;
use serde::Serialize;
use thiserror::Error;

use crate::query::{self, Mode, Node, QueryError};
use crate::text::Analyzers;
use crate::time::{day_number, BucketSpec, BucketUnit};

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("missing required parameter `{0}`")]
    Missing(&'static str),
    #[error("invalid value for `{name}`: {reason}")]
    Invalid { name: String, reason: String },
    #[error("unknown parameter `{0}`")]
    Unknown(String),
    #[error("parameter `{0}` given more than once")]
    Repeated(String),
    #[error(transparent)]
    Query(#[from] QueryError),
}

fn invalid(name: &str, reason: impl Into<String>) -> ParamError {
    ParamError::Invalid {
        name: name.to_owned(),
        reason: reason.into(),
    }
}

/// Filters every search endpoint accepts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Filters {
    pub from: NaiveDate,
    pub to: NaiveDate,
    /// USPS codes, uppercase, sorted.
    pub states: Vec<String>,
    /// LCCNs, sorted.
    pub lccns: Vec<String>,
    /// ISO 639-2 codes, lowercase, sorted.
    pub langs: Vec<String>,
    pub front_only: bool,
    /// Congress series to search (`globe`, `serialset`, `annals`,
    /// `register`), sorted; empty means all. Only with [`Source::Congress`]
    /// (#229).
    pub series: Vec<String>,
}

impl Filters {
    pub fn from_day(&self) -> u32 {
        day_number(self.from)
    }
    pub fn to_day(&self) -> u32 {
        day_number(self.to)
    }
}

/// Which corpus a search covers (#229): the newspapers (the default,
/// left out of canonical URLs), the debates and documents of Congress,
/// or both side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    News,
    Congress,
    All,
}

impl Source {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "news" => Some(Self::News),
            "congress" => Some(Self::Congress),
            "all" => Some(Self::All),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::News => "news",
            Self::Congress => "congress",
            Self::All => "all",
        }
    }
    /// Whether the search covers Congress pages.
    pub fn congress(self) -> bool {
        self != Self::News
    }
}

/// The Congress series a `series` filter can name (#229), sorted.
pub const CONGRESS_SERIES: &[&str] = &["annals", "globe", "register", "serialset"];

/// A validated, canonical search request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchRequest {
    pub query: Node,
    pub filters: Filters,
    /// The corpus searched; [`Source::News`] unless `source` says otherwise.
    pub source: Source,
    pub bucket: BucketUnit,
    /// `index_version` the client expects, if any.
    pub version: Option<String>,
}

/// Keys common to all search endpoints; endpoints may accept extra keys.
const COMMON_KEYS: &[&str] = &[
    "q", "mode", "near", "fuzzy", "from", "to", "state", "lccn", "lang", "front", "bucket", "v",
    "source", "series",
];

/// Raw query-string pairs with duplicate detection.
#[derive(Debug, Clone, Default)]
pub struct RawParams(Vec<(String, String)>);

impl RawParams {
    pub fn parse(query: &str) -> Result<Self, ParamError> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (k, v) in form_urlencoded::parse(query.as_bytes()) {
            if pairs.iter().any(|(pk, _)| *pk == k) {
                return Err(ParamError::Repeated(k.into_owned()));
            }
            pairs.push((k.into_owned(), v.into_owned()));
        }
        Ok(Self(pairs))
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    /// Reject keys outside `COMMON_KEYS` + `extra` (prevents cache-busting parameters).
    pub fn reject_unknown(&self, extra: &[&str]) -> Result<(), ParamError> {
        match self
            .0
            .iter()
            .find(|(k, _)| !COMMON_KEYS.contains(&k.as_str()) && !extra.contains(&k.as_str()))
        {
            Some((k, _)) => Err(ParamError::Unknown(k.clone())),
            None => Ok(()),
        }
    }

    /// The `lang` filter: catalog language codes, lowercased, sorted, without repeats.
    pub fn langs(&self) -> Result<Vec<String>, ParamError> {
        list(self.get("lang"), "lang", |s| {
            (s.len() == 3 && s.bytes().all(|b| b.is_ascii_alphabetic()))
                .then(|| s.to_ascii_lowercase())
        })
    }

    /// Reject every key except `allowed` (for endpoints without search parameters).
    pub fn reject_only(&self, allowed: &[&str]) -> Result<(), ParamError> {
        match self.0.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
            Some((k, _)) => Err(ParamError::Unknown(k.clone())),
            None => Ok(()),
        }
    }

    pub fn u8_in(&self, key: &str, max: u8) -> Result<Option<u8>, ParamError> {
        self.get(key)
            .map(|v| {
                v.parse::<u8>()
                    .ok()
                    .filter(|n| *n <= max)
                    .ok_or_else(|| invalid(key, format!("must be 0–{max}")))
            })
            .transpose()
    }
}

impl SearchRequest {
    /// Parse and validate. Dates are clamped to the corpus `bounds`, and the
    /// query's words are folded by the `analyzers` of the indexes it
    /// searches: the main indexes', or for a Japanese query the Japanese
    /// pages' (#139, #168).
    pub fn from_raw(
        raw: &RawParams,
        bounds: (NaiveDate, NaiveDate),
        analyzers: Analyzers,
    ) -> Result<Self, ParamError> {
        let q = raw.get("q").ok_or(ParamError::Missing("q"))?;
        let mode = raw
            .get("mode")
            .map(|m| {
                Mode::parse(m).ok_or_else(|| invalid("mode", "must be phrase, all, any or near"))
            })
            .transpose()?;
        let near = raw.u8_in("near", query::MAX_SLOP)?.unwrap_or(0);
        let fuzzy = raw.u8_in("fuzzy", query::MAX_FUZZY)?.unwrap_or(0);
        let query = parse_for(q, mode, near, fuzzy, analyzers)?;
        let (filters, source) = scope(raw, bounds)?;
        let bucket = match raw.get("bucket") {
            None | Some("auto") => BucketUnit::auto(filters.from, filters.to),
            Some(b) => BucketUnit::parse(b)
                .ok_or_else(|| invalid("bucket", "must be auto, year, month, week or day"))?,
        };
        Ok(Self {
            query,
            filters,
            source,
            bucket,
            version: raw.get("v").map(str::to_owned),
        })
    }

    pub fn bucket_spec(&self) -> BucketSpec {
        BucketSpec::new(self.bucket, self.filters.from, self.filters.to)
    }

    /// Canonical query string, without `v`. Used as the cache key and in URLs.
    pub fn canonical(&self) -> String {
        let f = &self.filters;
        let mut s = form_urlencoded::Serializer::new(String::new());
        s.append_pair("bucket", self.bucket.as_str());
        s.append_pair("from", &f.from.format("%Y-%m-%d").to_string());
        if f.front_only {
            s.append_pair("front", "true");
        }
        if !f.langs.is_empty() {
            s.append_pair("lang", &f.langs.join(","));
        }
        if !f.lccns.is_empty() {
            s.append_pair("lccn", &f.lccns.join(","));
        }
        s.append_pair("q", &self.query.to_string());
        if !f.series.is_empty() {
            s.append_pair("series", &f.series.join(","));
        }
        if self.source != Source::News {
            s.append_pair("source", self.source.as_str());
        }
        if !f.states.is_empty() {
            s.append_pair("state", &f.states.join(","));
        }
        s.append_pair("to", &f.to.format("%Y-%m-%d").to_string());
        s.finish()
    }

    /// Canonical query string pinned to `version`.
    pub fn canonical_with_version(&self, version: &str) -> String {
        let mut c = self.canonical();
        c.push_str("&v=");
        c.extend(form_urlencoded::byte_serialize(version.as_bytes()));
        c
    }
}

/// The filters and corpus of a request, with or without a query: dates
/// clamped to the corpus `bounds`, lists validated and sorted, and the
/// newspaper-only filters refused for Congress pages.
fn scope(raw: &RawParams, bounds: (NaiveDate, NaiveDate)) -> Result<(Filters, Source), ParamError> {
    let date = |key: &str, default: NaiveDate| -> Result<NaiveDate, ParamError> {
        raw.get(key)
            .map(|v| {
                NaiveDate::parse_from_str(v, "%Y-%m-%d")
                    .map_err(|_| invalid(key, "must be YYYY-MM-DD"))
            })
            .transpose()
            .map(|d| d.unwrap_or(default).clamp(bounds.0, bounds.1))
    };
    let from = date("from", bounds.0)?;
    let to = date("to", bounds.1)?;
    if from > to {
        return Err(invalid("from", "must not be after `to`"));
    }

    let states = list(raw.get("state"), "state", |s| {
        (s.len() == 2 && s.bytes().all(|b| b.is_ascii_alphabetic())).then(|| s.to_ascii_uppercase())
    })?;
    let lccns = list(raw.get("lccn"), "lccn", |s| {
        (!s.is_empty()
            && s.len() <= 16
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
        .then(|| s.to_owned())
    })?;
    let langs = raw.langs()?;
    let front_only = match raw.get("front") {
        None | Some("false") | Some("0") => false,
        Some("true") | Some("1") => true,
        Some(_) => return Err(invalid("front", "must be true or false")),
    };
    let source = match raw.get("source") {
        None => Source::News,
        Some(v) => {
            Source::parse(v).ok_or_else(|| invalid("source", "must be news, congress or all"))?
        }
    };
    let series = list(raw.get("series"), "series", |s| {
        CONGRESS_SERIES.contains(&s).then(|| s.to_owned())
    })?;
    if !series.is_empty() && !source.congress() {
        return Err(invalid("series", "needs source=congress or source=all"));
    }
    // Newspaper-only filters mean nothing for Congress pages, which have
    // no title and no front page.
    if source == Source::Congress {
        if !lccns.is_empty() {
            return Err(invalid(
                "lccn",
                "applies to newspapers, not source=congress",
            ));
        }
        if front_only {
            return Err(invalid(
                "front",
                "applies to newspapers, not source=congress",
            ));
        }
    }
    Ok((
        Filters {
            from,
            to,
            states,
            lccns,
            langs,
            front_only,
            series,
        },
        source,
    ))
}

/// The search parameters that make no sense without a query: refused when
/// pages are listed without one.
const QUERY_ONLY_KEYS: &[&str] = &["mode", "near", "fuzzy", "bucket"];

/// A request for pages without a query (#274): `/v1/hits` with `lccn` and
/// no `q`, which lists a title's pages by date. The filters are a search's,
/// newspapers only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowseRequest {
    pub filters: Filters,
    /// `index_version` the client expects, if any.
    pub version: Option<String>,
}

impl BrowseRequest {
    /// Parse and validate as [`SearchRequest::from_raw`] does, without `q`.
    /// `mode`, `near`, `fuzzy` and `bucket` need a query and are refused, as
    /// is a `source` other than the newspapers.
    pub fn from_raw(raw: &RawParams, bounds: (NaiveDate, NaiveDate)) -> Result<Self, ParamError> {
        if let Some(key) = QUERY_ONLY_KEYS.iter().find(|k| raw.get(k).is_some()) {
            return Err(invalid(key, "applies to a search with `q`"));
        }
        let (filters, source) = scope(raw, bounds)?;
        if source != Source::News {
            return Err(invalid(
                "source",
                "pages are listed without `q` for newspapers only",
            ));
        }
        Ok(Self {
            filters,
            version: raw.get("v").map(str::to_owned),
        })
    }

    /// Canonical query string, without `v`: the filters in a search's
    /// order, with no `q` or `bucket`, so it never names a search's
    /// response.
    pub fn canonical(&self) -> String {
        let f = &self.filters;
        let mut s = form_urlencoded::Serializer::new(String::new());
        s.append_pair("from", &f.from.format("%Y-%m-%d").to_string());
        if f.front_only {
            s.append_pair("front", "true");
        }
        if !f.langs.is_empty() {
            s.append_pair("lang", &f.langs.join(","));
        }
        if !f.lccns.is_empty() {
            s.append_pair("lccn", &f.lccns.join(","));
        }
        if !f.states.is_empty() {
            s.append_pair("state", &f.states.join(","));
        }
        s.append_pair("to", &f.to.format("%Y-%m-%d").to_string());
        s.finish()
    }
}

/// The query's AST, parsed with the analyzer of the indexes it searches: a
/// query that isn't Japanese under the main indexes' analyzer searches them;
/// otherwise it is parsed with the Japanese pages' analyzer as well, and a
/// query that is Japanese there searches them, with that parse (or its
/// error). With one analyzer for both, one parse.
fn parse_for(
    q: &str,
    mode: Option<Mode>,
    near: u8,
    fuzzy: u8,
    analyzers: Analyzers,
) -> Result<Node, QueryError> {
    let main = query::build(q, mode, near, fuzzy, analyzers.main);
    if analyzers.ja == analyzers.main || main.as_ref().is_ok_and(|n| !query::is_japanese(n)) {
        return main;
    }
    match query::build(q, mode, near, fuzzy, analyzers.ja) {
        Ok(ja) if query::is_japanese(&ja) => Ok(ja),
        // Japanese with the main analyzer, refused with the Japanese one.
        Err(e) if main.is_ok() => Err(e),
        _ => main,
    }
}

/// Parse a comma-separated list, validating and normalizing each item; sorted and deduplicated.
fn list(
    value: Option<&str>,
    name: &str,
    item: impl Fn(&str) -> Option<String>,
) -> Result<Vec<String>, ParamError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for part in value.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        out.push(item(part).ok_or_else(|| invalid(name, format!("`{part}` is not valid")))?);
    }
    if out.len() > 60 {
        return Err(invalid(name, "too many values"));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Analyzer;

    fn bounds() -> (NaiveDate, NaiveDate) {
        (
            NaiveDate::from_ymd_opt(1756, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(1963, 12, 31).unwrap(),
        )
    }

    fn req(q: &str) -> Result<SearchRequest, ParamError> {
        SearchRequest::from_raw(&RawParams::parse(q)?, bounds(), Analyzers::default())
    }

    #[test]
    fn source_defaults_to_newspapers_and_stays_out_of_canonical_urls() {
        let r = req("q=gold&from=1896-01-01&to=1896-12-31").unwrap();
        assert_eq!(r.source, Source::News);
        assert!(r.filters.series.is_empty());
        let explicit = req("q=gold&from=1896-01-01&to=1896-12-31&source=news").unwrap();
        assert_eq!(explicit.canonical(), r.canonical());
        assert!(!r.canonical().contains("source"));
    }

    #[test]
    fn congress_source_and_series_are_canonical() {
        let r = req(
            "q=gold&from=1850-01-01&to=1850-12-31&source=congress&series=serialset,globe,globe",
        )
        .unwrap();
        assert_eq!(r.source, Source::Congress);
        assert_eq!(r.filters.series, ["globe", "serialset"]);
        assert_eq!(
            r.canonical(),
            "bucket=week&from=1850-01-01&q=gold&series=globe%2Cserialset&source=congress&to=1850-12-31"
        );
        let all = req("q=gold&source=all").unwrap();
        assert_eq!(all.source, Source::All);
        assert!(all.canonical().contains("source=all"));
    }

    #[test]
    fn congress_params_refuse_what_does_not_apply() {
        let invalid = |q: &str| match req(q) {
            Err(ParamError::Invalid { name, .. }) => name,
            other => panic!("{q}: {other:?}"),
        };
        assert_eq!(invalid("q=gold&source=senate"), "source");
        assert_eq!(invalid("q=gold&source=congress&series=record"), "series");
        assert_eq!(invalid("q=gold&series=globe"), "series");
        assert_eq!(invalid("q=gold&source=congress&lccn=sn83030214"), "lccn");
        assert_eq!(invalid("q=gold&source=congress&front=true"), "front");
        // Under both, newspaper filters still apply to the newspaper half.
        assert!(req("q=gold&source=all&lccn=sn83030214&series=globe").is_ok());
    }

    fn req_with(q: &str, analyzers: Analyzers) -> SearchRequest {
        SearchRequest::from_raw(&RawParams::parse(q).unwrap(), bounds(), analyzers).unwrap()
    }

    #[test]
    fn folds_the_query_as_the_searched_indexes_were() {
        // `½` is one word with version 2, `1⁄2` with version 1 (#168).
        let v1 = Analyzers::all(Analyzer::V1);
        let v2 = Analyzers::all(Analyzer::V2);
        assert_eq!(req_with("q=%C2%BD", v2).query.to_string(), "½");
        assert_eq!(req_with("q=%C2%BD", v1).query.to_string(), "1\u{2044}2");
        // A Japanese query searches the Japanese pages, so it folds as they
        // were folded, whatever the main indexes' analyzer.
        let mixed = Analyzers {
            main: Analyzer::V2,
            ja: Analyzer::V1,
        };
        let ja = "q=%E5%B0%8F%E9%BA%A6+%C2%BD";
        assert_eq!(req_with(ja, mixed).query, req_with(ja, v1).query);
        assert_ne!(req_with(ja, v1).query, req_with(ja, v2).query);
        assert_eq!(
            req_with("q=%C2%BD", mixed).query,
            req_with("q=%C2%BD", v2).query
        );
        // A Japanese query the Japanese pages' analyzer refuses is refused,
        // whichever analyzer the main indexes have: version 1 has `1⁄2`
        // before the wildcard, which isn't one word.
        let wild = "q=%E6%9D%B1%E4%BA%AC+18461%C2%BD*";
        let raw = RawParams::parse(wild).unwrap();
        assert!(SearchRequest::from_raw(&raw, bounds(), mixed).is_err());
        assert!(SearchRequest::from_raw(&raw, bounds(), v1).is_err());
        let reversed = Analyzers {
            main: Analyzer::V1,
            ja: Analyzer::V2,
        };
        assert_eq!(req_with(wild, reversed).query, req_with(wild, v2).query);
    }

    #[test]
    fn equivalent_requests_share_a_canonical_form() {
        let a =
            req("q=Cross+of+Gold&mode=phrase&from=1896-06-01&to=1896-12-31&state=sc,GA").unwrap();
        let b = req("state=GA,SC&to=1896-12-31&from=1896-06-01&q=%22cross+of+gold%22&bucket=auto")
            .unwrap();
        assert_eq!(a.canonical(), b.canonical());
        assert_eq!(
            a.canonical(),
            "bucket=week&from=1896-06-01&q=%22cross+of+gold%22&state=GA%2CSC&to=1896-12-31"
        );
        assert_eq!(
            a.canonical_with_version("pages-v1"),
            format!("{}&v=pages-v1", a.canonical())
        );
    }

    #[test]
    fn defaults_and_clamping() {
        let r = req("q=scalawag&from=1600-01-01").unwrap();
        assert_eq!(r.filters.from, bounds().0);
        assert_eq!(r.filters.to, bounds().1);
        assert_eq!(r.bucket, BucketUnit::Year);
        assert!(!r.filters.front_only);
    }

    #[test]
    fn rejects_bad_parameters() {
        assert_eq!(req("mode=all").unwrap_err(), ParamError::Missing("q"));
        assert!(matches!(req("q=a1&q=b1"), Err(ParamError::Repeated(_))));
        assert!(matches!(
            req("q=gold&from=1900-01-01&to=1899-01-01"),
            Err(ParamError::Invalid { .. })
        ));
        assert!(matches!(
            req("q=gold&state=Georgia"),
            Err(ParamError::Invalid { .. })
        ));
        assert!(matches!(
            req("q=gold&fuzzy=3"),
            Err(ParamError::Invalid { .. })
        ));
        assert!(matches!(
            req("q=gold&bucket=decade"),
            Err(ParamError::Invalid { .. })
        ));
        assert!(matches!(req("q=text:gold"), Err(ParamError::Query(_))));
        let raw = RawParams::parse("q=gold&_=123").unwrap();
        assert_eq!(
            raw.reject_unknown(&[]).unwrap_err(),
            ParamError::Unknown("_".into())
        );
    }

    fn browse(q: &str) -> Result<BrowseRequest, ParamError> {
        BrowseRequest::from_raw(&RawParams::parse(q)?, bounds())
    }

    #[test]
    fn listing_pages_without_a_query_takes_the_filters_alone() {
        let r =
            browse("lccn=sn99000002&to=1900-01-01&from=1600-01-01&state=ga&v=pages-v2").unwrap();
        assert_eq!(r.filters.from, bounds().0);
        assert_eq!(r.filters.lccns, ["sn99000002"]);
        assert_eq!(r.version.as_deref(), Some("pages-v2"));
        // No `q` and no `bucket`: never a search's canonical URL.
        assert_eq!(
            r.canonical(),
            "from=1756-01-01&lccn=sn99000002&state=GA&to=1900-01-01"
        );
        assert_eq!(
            browse("lccn=sn99000002&source=news").unwrap().canonical(),
            browse("lccn=sn99000002").unwrap().canonical()
        );
        let invalid = |q: &str| match browse(q) {
            Err(ParamError::Invalid { name, .. }) => name,
            other => panic!("{q}: {other:?}"),
        };
        for key in ["mode=all", "near=3", "fuzzy=1", "bucket=year"] {
            assert_eq!(
                invalid(&format!("lccn=sn99000002&{key}")),
                key.split('=').next().unwrap()
            );
        }
        assert_eq!(invalid("lccn=sn99000002&source=all"), "source");
        assert_eq!(invalid("lccn=SN-1"), "lccn");
        assert_eq!(
            invalid("lccn=sn99000002&from=1900-01-01&to=1899-01-01"),
            "from"
        );
    }

    #[test]
    fn keeps_version() {
        assert_eq!(
            req("q=gold&v=pages-v2").unwrap().version.as_deref(),
            Some("pages-v2")
        );
    }
}
