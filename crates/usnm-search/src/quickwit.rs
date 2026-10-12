//! Quickwit backend (ADR-0001): translates the AST into Quickwit's query
//! language and aggregation requests, and queries an explicit set of sealed
//! indexes (08 §8.4.1).
//!
//! Behaviour confirmed against Quickwit 0.9.1 in Spike S-2 is marked `S-2`
//! (05 §5.5.1); `tests/quickwit_parity.rs` checks it against the reference
//! backend.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use usnm_core::params::Filters;
use usnm_core::query::Node;
use usnm_core::text_layout::TextLayout;
use usnm_core::time::BucketSpec;

use crate::cache_metrics::{self, CacheReport};
use crate::snippet::{matched_in, opening_snippet, page_snippets};
use crate::thread_metrics::{self, ThreadReport};
use crate::{
    ja_snippets, rank, Capabilities, CubeCell, Hit, HitSort, HitsPage, HitsQuery, IndexSet,
    KeyCount, PlaceSummary, SearchBackend, SearchError, Summary,
};

/// Upper bound on places in one terms aggregation.
const MAX_PLACES: u32 = 5_000;
/// Upper bound on titles: every title (4,693 on loc.gov in October 2026)
/// fits, so the newspaper count is exact (#121).
pub const MAX_PAPERS: u32 = 10_000;
/// Upper bound on title languages (about 50 in the catalog).
const MAX_LANGUAGES: u32 = 200;
/// Limit on reading the searcher's metrics, which it renders from memory.
const METRICS_TIMEOUT: Duration = Duration::from_secs(5);

/// What one read of the searcher's metrics found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearcherMetrics {
    pub caches: CacheReport,
    pub threads: ThreadReport,
}

impl SearcherMetrics {
    /// Both reports from one Prometheus text.
    pub fn parse(text: &str) -> Self {
        Self {
            caches: cache_metrics::parse(text),
            threads: thread_metrics::parse(text),
        }
    }
}

pub struct QuickwitBackend {
    base_url: String,
    client: reqwest::Client,
    /// Limit on a search request, if different from the client's.
    search_timeout: Option<Duration>,
}

impl QuickwitBackend {
    /// `timeout` limits every request: searches, index lookups and health checks.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, SearchError> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| SearchError::Backend(e.to_string()))?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client,
            search_timeout: None,
        })
    }

    /// Give searches their own limit, leaving index lookups and health
    /// checks on the client's.
    pub fn with_search_timeout(mut self, timeout: Duration) -> Self {
        self.search_timeout = Some(timeout);
        self
    }

    /// The searcher's cache, thread pool and runtime metrics, from its
    /// Prometheus endpoint (`GET /metrics`). Never waits longer than
    /// `METRICS_TIMEOUT`.
    pub async fn searcher_metrics(&self) -> Result<SearcherMetrics, SearchError> {
        let resp = self
            .client
            .get(format!("{}/metrics", self.base_url))
            .timeout(METRICS_TIMEOUT)
            .send()
            .await
            .map_err(map_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SearchError::Backend(format!(
                "quickwit /metrics returned {status}"
            )));
        }
        Ok(SearcherMetrics::parse(&resp.text().await.map_err(map_err)?))
    }

    async fn search(
        &self,
        indexes: &IndexSet,
        body: &Value,
    ) -> Result<SearchResponse, SearchError> {
        if indexes.ids().is_empty() {
            return Err(SearchError::Backend(
                "no indexes in the published version".into(),
            ));
        }
        // The query string sent, whose words a failure's cause leaves out.
        let sent = body
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let url = format!(
            "{}/api/v1/{}/search",
            self.base_url,
            indexes.ids().join(",")
        );
        let mut req = self.client.post(url).json(body);
        if let Some(t) = self.search_timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(map_err)?;
        let status = resp.status();
        if !status.is_success() {
            // Never include the response body: Quickwit may echo the query, and
            // query text must not reach logs (09 §9.4.2). Its cause, classified
            // from the body, is safe to log.
            // 408 is Quickwit cancelling a search at its own
            // `searcher.request_timeout_secs`: a timeout, not a refusal.
            if status == reqwest::StatusCode::REQUEST_TIMEOUT {
                return Err(SearchError::Timeout);
            }
            // A failure reading it is the real cause, with its own chain.
            let body = resp.text().await.map_err(map_err)?;
            // With failed splits not allowed (the default), Quickwit answers a
            // split's failure this way, naming the split in the body.
            let msg = format!(
                "quickwit returned {status} ({}){}",
                cause(&body, &sent),
                splits_named(&body, &sent)
            );
            return Err(if status.is_client_error() {
                SearchError::Rejected(msg)
            } else {
                SearchError::Backend(msg)
            });
        }
        let parsed: SearchResponse = resp.json().await.map_err(map_err)?;
        check_complete(&parsed, &sent)?;
        Ok(parsed)
    }
}

fn map_err(e: reqwest::Error) -> SearchError {
    if e.is_timeout() {
        return SearchError::Timeout;
    }
    // The whole chain (e.g. "connection refused" or "connection closed before
    // message completed"), which says whether the sidecar went away. It holds
    // the URL (index names) and transport errors, never the query, which is
    // in the request body.
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    let kind = if e.is_connect() {
        "connect"
    } else if e.is_decode() {
        "decode"
    } else if e.is_body() {
        "body"
    } else {
        "request"
    };
    SearchError::Backend(format!("quickwit {kind} error: {msg}"))
}

/// What a Quickwit error message is about, by keyword, so its cause can be
/// logged without its text (which may echo the query, 09 §9.4.2). The
/// message is compared word by word, leaving out the words of `query` (the
/// Quickwit query string sent), so an echoed query can't decide the
/// category: a search for "memory" that fails on storage is `storage`. A
/// failure whose only telling word is also in the query is `other`.
pub fn cause(message: &str, query: &str) -> &'static str {
    let echoed: std::collections::HashSet<String> = words(query).collect();
    let w: Vec<String> = words(message).filter(|w| !echoed.contains(w)).collect();
    let any = |of: &[&str]| w.iter().any(|x| of.contains(&x.as_str()));
    let pair = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    if any(&["memory"]) {
        "memory_limit"
    } else if any(&["bucket", "buckets"]) {
        "bucket_limit"
    } else if any(&["timeout", "deadline", "elapsed"]) || pair("timed", "out") {
        "timeout"
    } else if pair("too", "many")
        || pair("rate", "limit")
        || any(&[
            "concurrent",
            "concurrency",
            "throttled",
            "throttling",
            "503",
            "429",
        ])
    {
        "overloaded"
    } else if any(&[
        "storage",
        "azure",
        "blob",
        "io",
        "connection",
        "http",
        "network",
    ]) {
        "storage"
    } else if any(&[
        "split",
        "splits",
        "footer",
        "hotcache",
        "corrupt",
        "corrupted",
    ]) {
        "split"
    } else if w.is_empty() {
        "empty"
    } else {
        "other"
    }
}

/// `; splits A, B` for up to 5 split ids named in a Quickwit message, or
/// nothing. Split ids are ULIDs: 26 characters of Crockford base32 in upper
/// case, starting 0-7, with digits and letters. A word of `query` (the
/// query string sent, which the message may echo) is never one, in any
/// case, so no search text is logged.
fn splits_named(message: &str, query: &str) -> String {
    let echoed: std::collections::HashSet<String> = words(query).collect();
    let crockford = |c: char| c.is_ascii_digit() || (c.is_ascii_uppercase() && !"ILOU".contains(c));
    let mut ids: Vec<&str> = Vec::new();
    for w in message.split(|c: char| !c.is_ascii_alphanumeric()) {
        if w.len() == 26
            && w.starts_with(|c: char| ('0'..='7').contains(&c))
            && w.chars().all(crockford)
            && !echoed.contains(&w.to_ascii_lowercase())
            && w.chars().any(|c| c.is_ascii_digit())
            && w.chars().any(|c| c.is_ascii_uppercase())
            && !ids.contains(&w)
        {
            ids.push(w);
        }
    }
    if ids.is_empty() {
        String::new()
    } else {
        format!("; splits {}", ids[..ids.len().min(5)].join(", "))
    }
}

/// Lowercase ASCII alphanumeric words.
fn words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
}

// ------------------------------------------------------------ translation

/// Quickwit query-language string for the AST. Terms are folded alphanumerics
/// from our own parser, with at most the wildcards `?` and `*`, so they never
/// need escaping. With `grams` (the indexes have `text_cg`, 05 §5.5.3), an
/// exact phrase holding a common word searches `text_cg`, which never reads
/// the common word's positions. Each of its other words is also required in
/// `text`: that changes no match (a page with the phrase has the words) and
/// gives the snippets on `text` their highlights. A word that folds to
/// several (`ﷺ`) is left out of those: unquoted it would be several terms,
/// and the pairs already require it.
///
/// With `american_stories` (the indexes have `text_as` and `text_as_cg`,
/// 05 §5.5.4), each term or phrase matches in either text: `(text:… OR
/// text_as:…)`. A phrase stays within one text, and `NOT x` excludes a page
/// with `x` in either. Without it, the string is exactly LoC's text alone.
/// This is [`TextLayout::Separate`]'s rendering; [`query_for`] picks the
/// one of a set's layout.
pub fn query_string(
    node: &Node,
    grams: bool,
    american_stories: bool,
) -> Result<String, SearchError> {
    if american_stories {
        render(node, grams, &[&LOC, &AMERICAN_STORIES])
    } else {
        render(node, grams, &[&LOC])
    }
}

/// The query string for `indexes`, as their text layout has the texts
/// (05 §5.5.7): [`query_string`] on a field per text, and on one field for
/// both, each term or phrase once in `text_all` (and `text_all_cg`). The
/// release puts a gap between the texts there, so a phrase still can't
/// span them, and `NOT x` still excludes a page with `x` in either.
pub fn query_for(node: &Node, indexes: &IndexSet) -> Result<String, SearchError> {
    match indexes.text_layout() {
        TextLayout::Separate => {
            query_string(node, indexes.common_grams(), indexes.american_stories())
        }
        TextLayout::Single => render(node, indexes.common_grams(), &[&BOTH]),
    }
}

/// The query with each term or phrase in any of `texts`.
fn render(node: &Node, grams: bool, texts: &[&TextFields]) -> Result<String, SearchError> {
    Ok(match node {
        Node::And(c) => format!("({})", join(c, " AND ", grams, texts)?),
        Node::Or(c) => format!("({})", join(c, " OR ", grams, texts)?),
        Node::AtLeast { min, of } => at_least(usize::from(*min), of, grams, texts)?,
        Node::Not(n) => format!("NOT {}", render(n, grams, texts)?),
        leaf => {
            let leaves = texts
                .iter()
                .map(|f| leaf_string(leaf, grams, f))
                .collect::<Result<Vec<_>, _>>()?;
            match leaves.as_slice() {
                [one] => one.clone(),
                _ => format!("({})", leaves.join(" OR ")),
            }
        }
    })
}

/// `N OF (...)` (#124): an `OR` of each `min` of the items `AND`ed. Quickwit
/// 0.9.1's query language has no "at least N"; its Elasticsearch-compatible
/// API has (`bool` with `minimum_should_match`) and counts the same pages on
/// the fixtures, but this way hits, aggregations and every text layout keep
/// one request shape. At most `C(6, 3) = 20` groups
/// ([`usnm_core::query::MAX_AT_LEAST_ITEMS`]).
fn at_least(
    min: usize,
    of: &[Node],
    grams: bool,
    texts: &[&TextFields],
) -> Result<String, SearchError> {
    let items = of
        .iter()
        .map(|n| render(n, grams, texts))
        .collect::<Result<Vec<_>, _>>()?;
    let mut groups = Vec::new();
    let mut pick: Vec<usize> = (0..min).collect();
    loop {
        let parts: Vec<&str> = pick.iter().map(|&i| items[i].as_str()).collect();
        groups.push(format!("({})", parts.join(" AND ")));
        // The next `min` indexes in lexicographic order, if any.
        let Some(i) = (0..min).rev().find(|&i| pick[i] < items.len() - min + i) else {
            break;
        };
        pick[i] += 1;
        for j in i + 1..min {
            pick[j] = pick[j - 1] + 1;
        }
    }
    Ok(format!("({})", groups.join(" OR ")))
}

/// The pages a search of both texts finds that match the query in American
/// Stories' text but not in LoC's (05 §5.5.4): the search in both texts,
/// `AND NOT` the same in LoC's text alone, `AND` the same in American
/// Stories' text alone. All three, because with `NOT` in the query a page
/// can match in one text and still be left out by a word in the other.
/// The hits these are list `matched_in: ["american_stories"]`.
pub fn american_stories_only_string(node: &Node, grams: bool) -> Result<String, SearchError> {
    let both = render(node, grams, &[&LOC, &AMERICAN_STORIES])?;
    let loc = render(node, grams, &[&LOC])?;
    let american = render(node, grams, &[&AMERICAN_STORIES])?;
    Ok(format!("({both}) AND NOT ({loc}) AND ({american})"))
}

/// A page's text field and its common-word pairs.
struct TextFields {
    text: &'static str,
    pairs: &'static str,
}

/// LoC's OCR (05 §5.5).
const LOC: TextFields = TextFields {
    text: "text",
    pairs: "text_cg",
};

/// American Stories' (05 §5.5.4).
const AMERICAN_STORIES: TextFields = TextFields {
    text: "text_as",
    pairs: "text_as_cg",
};

/// Both texts in one field, with a gap between them (05 §5.5.7).
const BOTH: TextFields = TextFields {
    text: usnm_core::text_layout::FIELD,
    pairs: usnm_core::text_layout::PAIRS_FIELD,
};

/// A term or phrase in one text's fields.
fn leaf_string(node: &Node, grams: bool, f: &TextFields) -> Result<String, SearchError> {
    let text = f.text;
    Ok(match node {
        Node::Term(t) if t.fuzzy > 0 => {
            // S-2: Quickwit 0.9's query language has no fuzzy terms (`~n` on a
            // term silently matches nothing), so refuse rather than miscount.
            return Err(SearchError::Unsupported(
                "fuzzy (OCR-tolerant) matching".into(),
            ));
        }
        Node::Term(t) if t.prefix => format!("{text}:{}*", t.text),
        // `word~ocr` (#124): the word and its OCR spellings as one term set,
        // which the engine looks up in the term dictionary like a word.
        // Members are split at whitespace and not analyzed, so each must be
        // one index token as it stands. The parser refuses a word that isn't
        // (`ﷺ` folded by `Analyzer::V1`); this never sends one split into
        // several members.
        Node::Term(t) if t.ocr => {
            let variants = usnm_core::ocr_variants::variants(&t.text);
            if !variants
                .iter()
                .all(|v| usnm_core::ocr_variants::is_one_token(v))
            {
                return Err(SearchError::Unsupported(
                    "~ocr on a word the index holds as several words".into(),
                ));
            }
            format!("{text}:IN [{}]", variants.join(" "))
        }
        // With `?` or `*` in it (#124), an unquoted word is a wildcard query
        // in Quickwit 0.9, matched against the index's words.
        Node::Term(t) => format!("{text}:{}", t.text),
        Node::Phrase { terms, slop } if *slop > 0 => {
            format!("{text}:\"{}\"~{slop}", terms.join(" "))
        }
        Node::Phrase { terms, .. } => match grams
            .then(|| usnm_core::common_grams::query_terms(terms))
            .flatten()
        {
            Some(pairs) => {
                let mut parts = vec![format!("{}:\"{}\"", f.pairs, pairs.join(" "))];
                let mut seen = std::collections::BTreeSet::new();
                parts.extend(
                    terms
                        .iter()
                        .filter(|t| !usnm_core::common_grams::is_common(t))
                        // A word `Analyzer::V1` folds to several (`ﷺ`) is
                        // only required through the pairs.
                        .filter(|t| !t.contains(char::is_whitespace) && seen.insert(*t))
                        .map(|w| format!("{text}:{w}")),
                );
                format!("({})", parts.join(" AND "))
            }
            None => format!("{text}:\"{}\"", terms.join(" ")),
        },
        Node::And(_) | Node::Or(_) | Node::AtLeast { .. } | Node::Not(_) => {
            unreachable!("query_string renders operators")
        }
    })
}

fn join(
    children: &[Node],
    sep: &str,
    grams: bool,
    texts: &[&TextFields],
) -> Result<String, SearchError> {
    Ok(children
        .iter()
        .map(|c| render(c, grams, texts))
        .collect::<Result<Vec<_>, _>>()?
        .join(sep))
}

/// Filter clauses (values are validated by `usnm_core::params`), `lang=` as
/// `indexes` match it (title or page languages, 05 §5.5.8).
pub fn filter_clauses(filters: &Filters, indexes: &IndexSet) -> Vec<String> {
    let mut out = vec![format!(
        "day:[{} TO {}]",
        filters.from_day(),
        filters.to_day()
    )];
    let set = |field: &str, values: &[String]| format!("{field}:IN [{}]", values.join(" "));
    if !filters.states.is_empty() {
        out.push(set("state", &filters.states));
    }
    if !filters.lccns.is_empty() {
        out.push(set("lccn", &filters.lccns));
    }
    if let Some(m) = indexes.language_match(&filters.langs) {
        out.push(m.clause());
    }
    if filters.front_only {
        out.push("front_page:true".into());
    }
    // Only Congress pages have a series (#229); usnm_core::params refuses
    // `series` on a newspaper search.
    if !filters.series.is_empty() {
        out.push(set("series", &filters.series));
    }
    out
}

/// The decades a date-limited search needs, on a version laid out by
/// decade (05 §5.5.5): Quickwit then skips the other decades' splits before
/// opening them (tag pruning). The `day` filter stays the exact one. Never a
/// timestamp or `date:` range: Quickwit 0.9.1 matches nothing before 1972
/// with those (#158).
pub fn decade_clause(filters: &Filters, indexes: &IndexSet) -> Option<String> {
    usnm_core::decade::clause(filters.from, filters.to, indexes.decades()?)
}

/// Clauses that hide the copies of duplicated pages `indexes` names, one per
/// batch: that batch's documents with those ids (04 §4.7). Doc ids and batch
/// names are our own (letters, digits, `_` and `-`), so they need no escaping.
pub fn hidden_clauses(indexes: &IndexSet) -> Vec<String> {
    indexes
        .hidden()
        .iter()
        .map(|(batch, ids)| {
            let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
            format!("NOT (batch:{batch} AND doc_id:IN [{}])", ids.join(" "))
        })
        .collect()
}

fn full_query(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    extra: Vec<String>,
) -> Result<String, SearchError> {
    let query = query_for(node, indexes)?;
    Ok(scoped(query, filters, indexes, extra))
}

/// `query` with the filters and the hidden copies' clauses.
fn scoped(query: String, filters: &Filters, indexes: &IndexSet, extra: Vec<String>) -> String {
    let mut parts = vec![query];
    parts.extend(scope(filters, indexes, extra));
    parts.join(" AND ")
}

/// The filters' clauses, the decades', the hidden copies' and `extra`:
/// never empty, as the filters always name the days.
fn scope(filters: &Filters, indexes: &IndexSet, extra: Vec<String>) -> Vec<String> {
    let mut parts = filter_clauses(filters, indexes);
    parts.extend(decade_clause(filters, indexes));
    parts.extend(hidden_clauses(indexes));
    parts.extend(extra);
    parts
}

fn histogram(spec: &BucketSpec, min_doc_count: u64) -> Value {
    let h = spec.histogram_field();
    json!({
        "histogram": {
            "field": h.field,
            "interval": h.interval,
            "offset": h.origin % h.interval,
            "min_doc_count": min_doc_count
        }
    })
}

pub fn summary_request(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    spec: &BucketSpec,
) -> Result<Value, SearchError> {
    Ok(json!({
        "query": full_query(node, filters, indexes, Vec::new())?,
        "max_hits": 0,
        "aggs": {
            "series": histogram(spec, 1),
            "first": { "min": { "field": "day" } },
            "last": { "max": { "field": "day" } },
            "places": {
                "terms": { "field": "place_id", "size": MAX_PLACES },
                "aggs": {
                    "first": { "min": { "field": "day" } },
                    "last": { "max": { "field": "day" } }
                }
            },
            "days": { "cardinality": { "field": "day" } },
            "papers": { "terms": { "field": "lccn", "size": MAX_PAPERS } },
            "languages": { "terms": { "field": indexes.language_field(), "size": MAX_LANGUAGES } }
        }
    }))
}

pub fn cube_request(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    spec: &BucketSpec,
    shards: &[u8],
) -> Result<Value, SearchError> {
    let mut extra = Vec::new();
    if shards.len() < usize::from(usnm_core::cube::PLACE_SHARDS) {
        let list: Vec<String> = shards.iter().map(u8::to_string).collect();
        extra.push(format!("place_shard:IN [{}]", list.join(" ")));
    }
    cube_body(node, filters, indexes, spec, extra, MAX_PLACES)
}

/// The cube for the places in `places` only (`/v1/days`). Place ids are
/// our own (letters, digits, `_` and `-`), so they need no escaping; any
/// other id is refused rather than put in the query.
pub fn place_cube_request(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    spec: &BucketSpec,
    places: &[String],
) -> Result<Value, SearchError> {
    let plain = |p: &String| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    if places.is_empty() || !places.iter().all(plain) {
        return Err(SearchError::Rejected(
            "place ids must be letters, digits, `_` or `-`".into(),
        ));
    }
    let extra = vec![format!("place_id:IN [{}]", places.join(" "))];
    let size = u32::try_from(places.len()).unwrap_or(MAX_PLACES);
    cube_body(node, filters, indexes, spec, extra, size)
}

fn cube_body(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    spec: &BucketSpec,
    extra: Vec<String>,
    places: u32,
) -> Result<Value, SearchError> {
    Ok(json!({
        "query": full_query(node, filters, indexes, extra)?,
        "max_hits": 0,
        "aggs": {
            "places": {
                "terms": { "field": "place_id", "size": places },
                "aggs": { "t": histogram(spec, 1) }
            }
        }
    }))
}

pub fn hits_request(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
    page: &HitsQuery,
) -> Result<Value, SearchError> {
    let query = full_query(node, filters, indexes, selection(page))?;
    Ok(page_request(query, page))
}

/// The place or title a page list is limited to.
fn selection(page: &HitsQuery) -> Vec<String> {
    let mut extra = Vec::new();
    if let Some(p) = &page.place_id {
        extra.push(format!("place_id:{p}"));
    }
    if let Some(l) = &page.lccn {
        extra.push(format!("lccn:{l}"));
    }
    extra
}

/// Pages without a query (#274): the filters' clauses alone, with no text
/// term, in date order. Quickwit then has no posting list of a word to
/// skip splits by: it reads the `lccn` (or `place_id`) and `day` postings
/// of every split of the version, and skips splits only as any search
/// limited to some decades does (05 §5.5.5).
/// `Relevant` is refused, as by every backend.
pub fn browse_request(
    filters: &Filters,
    indexes: &IndexSet,
    page: &HitsQuery,
) -> Result<Value, SearchError> {
    crate::browse_order(page.sort)?;
    let query = scope(filters, indexes, selection(page)).join(" AND ");
    Ok(page_request(query, page))
}

/// A page of a hit list for `query`, with the days counted when asked.
fn page_request(query: String, page: &HitsQuery) -> Value {
    let mut req = json!({
        "query": query,
        "max_hits": page.limit,
        "start_offset": page.offset,
        "sort_by": sort_by(page.sort)
    });
    // On how many days the pages appeared (#127), when asked: the first page
    // of a visitor's list, not the aggregate's first and last page lookups.
    if page.days {
        req["aggs"] = json!({ "days": { "cardinality": { "field": "day" } } });
    }
    req
}

/// How many pages only American Stories' text finds
/// ([`american_stories_only_string`]): no hits, no aggregations, no sort.
/// Quickwit still counts every match (`num_hits`). Refused with one field
/// for both texts (05 §5.5.7): LoC's alone isn't indexed there.
pub fn american_stories_only_request(
    node: &Node,
    filters: &Filters,
    indexes: &IndexSet,
) -> Result<Value, SearchError> {
    if indexes.text_layout() == TextLayout::Single {
        return Err(SearchError::Unsupported(
            "counting the pages only American Stories' text matches, on these indexes".into(),
        ));
    }
    let query = american_stories_only_string(node, indexes.common_grams())?;
    Ok(json!({
        "query": scoped(query, filters, indexes, Vec::new()),
        "max_hits": 0
    }))
}

/// By day, then title, edition and page (`sort_key`). Quickwit 0.9 can't sort
/// on text fields, and a leading `-` means ascending (S-2). `_score` sorts
/// highest first; with `fieldnorms: false` on `text` it follows how often a
/// page mentions the words (#126), and ties go oldest first. A third field
/// (`-sort_key`) is refused: "sort by field must be up to 2 fields".
fn sort_by(sort: HitSort) -> &'static str {
    match sort {
        HitSort::Oldest => "-day,-sort_key",
        HitSort::Newest => "day,sort_key",
        HitSort::Relevant => "_score,-day",
    }
}

// ------------------------------------------------------------ responses

#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    pub num_hits: u64,
    #[serde(default)]
    pub hits: Vec<StoredHit>,
    #[serde(default)]
    pub aggregations: Option<Value>,
    /// Partial failures (e.g. splits that could not be searched). A 200 with
    /// any errors is incomplete and must not be served or cached.
    #[serde(default)]
    pub errors: Vec<Value>,
}

/// The fields a hit carries: only those stored in the index (05 §5.5).
/// `day` and `front_page` are not stored; they are derived from `date` and `seq`.
#[derive(Debug, Deserialize)]
pub struct StoredHit {
    pub doc_id: String,
    /// Quickwit renders datetimes as RFC 3339 (`1896-07-10T00:00:00Z`) by default.
    pub date: String,
    pub place_id: String,
    pub lccn: String,
    pub edition: u16,
    pub seq: u16,
    /// The page's stored text, for its snippets (#126).
    #[serde(default)]
    pub text: Option<String>,
    /// A Japanese page's printed text (pages-ja-index.yaml), for its snippets.
    #[serde(default)]
    pub printed: Option<String>,
    /// American Stories' text (05 §5.5.4), for snippets when the query
    /// matched only there.
    #[serde(default)]
    pub text_as: Option<String>,
    #[serde(default)]
    pub ocr_source: Option<String>,
    #[serde(default)]
    pub ocr_engine: Option<String>,
    /// A Congress page's stored fields (congress-index.yaml, #229).
    #[serde(flatten)]
    pub congress: crate::CongressFields,
}

impl StoredHit {
    fn day(&self) -> Option<u32> {
        let ymd = self.date.get(..10)?;
        let date = chrono::NaiveDate::parse_from_str(ymd, "%Y-%m-%d").ok()?;
        Some(usnm_core::time::day_number(date))
    }
}

/// Reject responses that report partial failures, without echoing their
/// content: the error names how many, what they were about ([`cause`]) and
/// up to 5 of the failed splits, whose ids are opaque.
pub fn check_complete(resp: &SearchResponse, query: &str) -> Result<(), SearchError> {
    if resp.errors.is_empty() {
        return Ok(());
    }
    let mut causes = std::collections::BTreeMap::<&str, usize>::new();
    let mut texts = Vec::new();
    for e in &resp.errors {
        // Quickwit 0.9's REST response gives each error as a string.
        let text = e.as_str().map_or_else(|| e.to_string(), str::to_owned);
        *causes.entry(cause(&text, query)).or_default() += 1;
        texts.push(text);
    }
    let causes: Vec<String> = causes.iter().map(|(c, n)| format!("{c}×{n}")).collect();
    let msg = format!(
        "quickwit reported {} partial failure(s): {}{}",
        resp.errors.len(),
        causes.join(", "),
        splits_named(&texts.join(" "), query)
    );
    Err(SearchError::Backend(msg))
}

#[derive(Debug, Deserialize)]
struct Buckets<B> {
    buckets: Vec<B>,
}

#[derive(Debug, Deserialize)]
struct NumBucket {
    key: f64,
    doc_count: u64,
}

/// A `min` or `max` metric; `value` is null when no document matched.
#[derive(Debug, Deserialize)]
struct MetricValue {
    value: Option<f64>,
}

impl MetricValue {
    fn day(m: Option<Self>) -> Option<u32> {
        m.and_then(|m| m.value).map(|v| v as u32)
    }
}

#[derive(Debug, Deserialize)]
struct PlaceBucket {
    key: String,
    doc_count: u64,
    first: Option<MetricValue>,
    last: Option<MetricValue>,
    t: Option<Buckets<NumBucket>>,
}

fn agg<T: for<'de> Deserialize<'de>>(resp: &SearchResponse, name: &str) -> Result<T, SearchError> {
    let v = resp
        .aggregations
        .as_ref()
        .and_then(|a| a.get(name))
        .ok_or_else(|| SearchError::Backend(format!("missing aggregation `{name}`")))?;
    serde_json::from_value(v.clone())
        .map_err(|e| SearchError::Backend(format!("aggregation `{name}`: {e}")))
}

pub fn parse_summary(resp: &SearchResponse, spec: &BucketSpec) -> Result<Summary, SearchError> {
    let mut series = vec![0u64; spec.len()];
    for b in agg::<Buckets<NumBucket>>(resp, "series")?.buckets {
        if let Some(i) = spec.index_of_key(b.key as u32) {
            series[i] += b.doc_count;
        }
    }
    // Every place bucket has at least one page, so both days must be there.
    let mut places: Vec<PlaceSummary> = agg::<Buckets<PlaceBucket>>(resp, "places")?
        .buckets
        .into_iter()
        .map(|b| {
            let missing =
                || SearchError::Backend("place bucket without its first or last day".into());
            Ok(PlaceSummary {
                first_day: MetricValue::day(b.first).ok_or_else(missing)?,
                last_day: MetricValue::day(b.last).ok_or_else(missing)?,
                place_id: b.key,
                hits: b.doc_count,
            })
        })
        .collect::<Result<_, SearchError>>()?;
    // By id, as the memory backend gives them: Quickwit orders tied buckets
    // differently from one split layout to another (#293).
    places.sort_by(|a, b| a.place_id.cmp(&b.place_id));
    // Null when nothing matched; otherwise both days must be there.
    let day = |name| -> Result<Option<u32>, SearchError> {
        if resp.num_hits == 0 {
            return Ok(None);
        }
        MetricValue::day(Some(agg::<MetricValue>(resp, name)?))
            .map(Some)
            .ok_or_else(|| SearchError::Backend(format!("aggregation `{name}` has no day")))
    };
    Ok(Summary {
        total_hits: resp.num_hits,
        first_day: day("first")?,
        last_day: day("last")?,
        series,
        places,
        papers: key_counts(resp, "papers")?,
        languages: key_counts(resp, "languages")?,
        // Always requested: missing with matches is an incomplete response.
        days: match distinct(resp)? {
            Some(d) => d,
            None if resp.num_hits == 0 => 0,
            None => return Err(SearchError::Backend("missing aggregation `days`".into())),
        },
    })
}

#[derive(Debug, Deserialize)]
struct KeyBucket {
    key: String,
    doc_count: u64,
}

/// A terms aggregation's buckets, ranked as the memory backend ranks them.
fn key_counts(resp: &SearchResponse, name: &str) -> Result<Vec<KeyCount>, SearchError> {
    Ok(rank(
        agg::<Buckets<KeyBucket>>(resp, name)?
            .buckets
            .into_iter()
            .map(|b| KeyCount {
                key: b.key,
                hits: b.doc_count,
            })
            .collect(),
    ))
}

/// The `days` cardinality, when the request asked for it. Quickwit's
/// HyperLogLog gives a float; nothing matched is 0.
fn distinct(resp: &SearchResponse) -> Result<Option<u64>, SearchError> {
    if resp
        .aggregations
        .as_ref()
        .and_then(|a| a.get("days"))
        .is_none()
    {
        return Ok(None);
    }
    if resp.num_hits == 0 {
        return Ok(Some(0));
    }
    let m = agg::<MetricValue>(resp, "days")?;
    m.value
        .map(|v| Some(v.round() as u64))
        .ok_or_else(|| SearchError::Backend("aggregation `days` has no value".into()))
}

pub fn parse_cube(resp: &SearchResponse, spec: &BucketSpec) -> Result<Vec<CubeCell>, SearchError> {
    let mut cells = Vec::new();
    for place in agg::<Buckets<PlaceBucket>>(resp, "places")?.buckets {
        for b in place.t.map(|t| t.buckets).unwrap_or_default() {
            if let Some(i) = spec.index_of_key(b.key as u32) {
                cells.push(CubeCell {
                    place_id: place.key.clone(),
                    bucket: i as u32,
                    hits: b.doc_count as u32,
                });
            }
        }
    }
    Ok(cells)
}

/// `indexes`: whether the search covered American Stories' text, whose
/// snippets stand in when LoC's text has no match (05 §5.5.4), and the
/// analyzer that folded a Japanese page's text, for its snippets (#168).
pub fn parse_hits(
    resp: SearchResponse,
    query: &Node,
    indexes: &IndexSet,
) -> Result<HitsPage, SearchError> {
    hits_page(resp, Some(query), indexes)
}

/// The pages of a [`browse_request`] (#274): each with the opening of its
/// text as its snippet ([`opening_snippet`]), and no `snippet_source` or
/// `matched_in`.
pub fn parse_browse(resp: SearchResponse, indexes: &IndexSet) -> Result<HitsPage, SearchError> {
    hits_page(resp, None, indexes)
}

/// The hits of `resp`, with snippets of `query`, or without one the
/// opening of each page's text.
fn hits_page(
    resp: SearchResponse,
    query: Option<&Node>,
    indexes: &IndexSet,
) -> Result<HitsPage, SearchError> {
    let american_stories = indexes.american_stories();
    let analyzer = indexes.analyzer();
    let layout = indexes.text_layout();
    let days = distinct(&resp)?;
    let mut hits = Vec::with_capacity(resp.hits.len());
    for d in resp.hits {
        let day = d
            .day()
            .ok_or_else(|| SearchError::Backend("hit has an unparseable date".into()))?;
        // Built here from the stored text, as the memory backend does (#126).
        // A Japanese page's come from its printed text: its indexed text is
        // folded tokens with spaces between them.
        let text = d.text.as_deref().unwrap_or_default();
        let (snippets, snippet_source, matched_in) = match query {
            None => (
                opening_snippet(d.printed.as_deref().unwrap_or(text)),
                None,
                None,
            ),
            Some(query) => {
                let (snippets, snippet_source) = match &d.printed {
                    Some(printed) => (ja_snippets(printed, query, analyzer), None),
                    None => {
                        page_snippets(text, d.text_as.as_deref(), query, american_stories, layout)
                    }
                };
                // Which texts match, from the same stored texts: no extra query.
                let matched_in = (american_stories && d.printed.is_none())
                    .then(|| matched_in(text, d.text_as.as_deref(), query, layout));
                (snippets, snippet_source, matched_in)
            }
        };
        // A Congress unit's first page is no front page (#229), as its
        // document says and the memory backend reads.
        let front_page = d.seq == 1 && !d.congress.is_congress();
        hits.push(Hit {
            snippets,
            snippet_source,
            matched_in,
            ocr_source: d.ocr_source,
            ocr_engine: d.ocr_engine,
            congress: d.congress,
            front_page,
            day,
            doc_id: d.doc_id,
            lccn: d.lccn,
            place_id: d.place_id,
            edition: d.edition,
            seq: d.seq,
        });
    }
    Ok(HitsPage {
        total: resp.num_hits,
        days,
        hits,
    })
}

#[async_trait]
impl SearchBackend for QuickwitBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            fuzzy: false,
            max_slop: usnm_core::query::MAX_SLOP,
            nested_aggregations: true,
        }
    }

    async fn summary(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
    ) -> Result<Summary, SearchError> {
        let resp = self
            .search(indexes, &summary_request(query, filters, indexes, spec)?)
            .await?;
        parse_summary(&resp, spec)
    }

    async fn cube(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
        shards: &[u8],
    ) -> Result<Vec<CubeCell>, SearchError> {
        let resp = self
            .search(
                indexes,
                &cube_request(query, filters, indexes, spec, shards)?,
            )
            .await?;
        parse_cube(&resp, spec)
    }

    async fn place_cube(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
        places: &[String],
    ) -> Result<Vec<CubeCell>, SearchError> {
        let resp = self
            .search(
                indexes,
                &place_cube_request(query, filters, indexes, spec, places)?,
            )
            .await?;
        parse_cube(&resp, spec)
    }

    async fn hits(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        page: &HitsQuery,
    ) -> Result<HitsPage, SearchError> {
        let resp = self
            .search(indexes, &hits_request(query, filters, indexes, page)?)
            .await?;
        parse_hits(resp, query, indexes)
    }

    async fn browse(
        &self,
        indexes: &IndexSet,
        filters: &Filters,
        page: &HitsQuery,
    ) -> Result<HitsPage, SearchError> {
        let resp = self
            .search(indexes, &browse_request(filters, indexes, page)?)
            .await?;
        parse_browse(resp, indexes)
    }

    async fn american_stories_only(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
    ) -> Result<u64, SearchError> {
        let resp = self
            .search(
                indexes,
                &american_stories_only_request(query, filters, indexes)?,
            )
            .await?;
        Ok(resp.num_hits)
    }

    /// A read-only searcher reads the metastore manifest once at start, so it
    /// can't find indexes created later by name in a search. Looking an index
    /// up by id loads it from storage (and then it's polled like the rest), so
    /// each index is looked up before its version is served (S-2, 08 §8.4.1).
    async fn prepare(&self, indexes: &IndexSet) -> Result<(), SearchError> {
        for id in indexes.ids() {
            let url = format!("{}/api/v1/indexes/{id}", self.base_url);
            let resp = self.client.get(url).send().await.map_err(map_err)?;
            if !resp.status().is_success() {
                return Err(SearchError::Backend(format!(
                    "index `{id}` is not available: quickwit returned {}",
                    resp.status()
                )));
            }
        }
        Ok(())
    }

    async fn health(&self) -> Result<(), SearchError> {
        let url = format!("{}/health/readyz", self.base_url);
        let resp = self.client.get(url).send().await.map_err(map_err)?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(SearchError::Backend(format!(
                "quickwit readyz returned {}",
                resp.status()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use usnm_core::query::parse;
    use usnm_core::text::Analyzer;
    use usnm_core::time::BucketUnit;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn series_filter_is_a_term_set_and_absent_by_default() {
        let mut f = filters();
        let set = IndexSet::new(vec!["congress".into()]);
        assert!(!filter_clauses(&f, &set)
            .iter()
            .any(|c| c.starts_with("series")));
        f.front_only = false;
        f.series = vec!["globe".into(), "serialset".into()];
        assert_eq!(
            filter_clauses(&f, &set).last().map(String::as_str),
            Some("series:IN [globe serialset]")
        );
    }

    fn filters() -> Filters {
        Filters {
            from: d("1896-06-01"),
            to: d("1896-12-31"),
            states: vec!["GA".into(), "SC".into()],
            lccns: vec![],
            langs: vec![],
            front_only: true,
            series: Vec::new(),
        }
    }

    fn none() -> IndexSet {
        IndexSet::new(vec!["i".into()])
    }

    /// A set that searches American Stories' text or not.
    fn stories(on: bool) -> IndexSet {
        none().with_american_stories(on)
    }

    /// Answers every request with `status` and an empty JSON object.
    async fn stub(status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0; 16 * 1024];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\n\
                     content-length: 2\r\nconnection: close\r\n\r\n{{}}"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        url
    }

    #[tokio::test]
    async fn quickwit_cancelling_a_search_at_its_timeout_is_a_timeout() {
        let qw = QuickwitBackend::new(&stub(408).await, Duration::from_secs(5)).unwrap();
        let err = qw
            .search(&IndexSet::new(vec!["i".into()]), &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::Timeout), "{err}");
    }

    #[tokio::test]
    async fn a_refused_request_is_rejected_and_a_server_error_is_not() {
        for (status, rejected) in [(400, true), (422, true), (500, false), (503, false)] {
            let qw = QuickwitBackend::new(&stub(status).await, Duration::from_secs(5)).unwrap();
            let err = qw
                .search(&IndexSet::new(vec!["i".into()]), &serde_json::json!({}))
                .await
                .unwrap_err();
            assert_eq!(
                matches!(err, SearchError::Rejected(_)),
                rejected,
                "{status}: {err}"
            );
        }
    }

    #[test]
    fn translates_ast_to_query_language() {
        let q = parse(r#""cross of gold" -bryan (silver OR freed*) "gold silver"~3"#).unwrap();
        assert_eq!(
            query_string(&q, false, false).unwrap(),
            r#"(text:"cross of gold" AND text:"gold silver"~3 AND NOT text:bryan AND (text:freed* OR text:silver))"#
        );
        assert!(matches!(
            query_string(&parse("gold~1").unwrap(), false, false),
            Err(SearchError::Unsupported(_))
        ));
        // Wildcards inside a word go as typed, folded (#124).
        assert_eq!(
            query_string(&parse("Presi?ent washi*TON").unwrap(), false, false).unwrap(),
            "(text:presi?ent AND text:washi*ton)"
        );
        // ...in either text when the version has American Stories' (#218).
        assert_eq!(
            query_string(&parse("presi?ent").unwrap(), false, true).unwrap(),
            "(text:presi?ent OR text_as:presi?ent)"
        );
        // A word's OCR spellings are one term set (#124).
        assert_eq!(
            query_string(&parse("Gold~ocr").unwrap(), false, false).unwrap(),
            "text:IN [goid golcl gold]"
        );
        assert_eq!(
            query_string(&parse("gold~ocr").unwrap(), false, true).unwrap(),
            "(text:IN [goid golcl gold] OR text_as:IN [goid golcl gold])"
        );
        // A word that isn't one index token (`ﷺ` or `½` folded by
        // `Analyzer::V1`) is never sent split into several members; the
        // parser refuses it, and a term built without the parser is refused
        // here.
        for text in [
            usnm_core::text::fold("ﷺ", Analyzer::V1),
            usnm_core::text::fold("½", Analyzer::V1),
            "gold]".into(),
        ] {
            let node = Node::Term(usnm_core::query::Term {
                text: text.clone(),
                fuzzy: 0,
                prefix: false,
                wildcard: false,
                ocr: true,
            });
            for american_stories in [false, true] {
                assert!(
                    matches!(
                        query_string(&node, false, american_stories),
                        Err(SearchError::Unsupported(_))
                    ),
                    "{text}"
                );
            }
        }
        // At least N: each N of them, ANDed, ORed.
        assert_eq!(
            query_string(&parse("2 OF (gold silver bryan)").unwrap(), false, false).unwrap(),
            "((text:bryan AND text:gold) OR (text:bryan AND text:silver) \
             OR (text:gold AND text:silver))"
        );
        let q = parse("3 OF (a1 b1 c1 d1 e1 f1)").unwrap();
        let s = query_string(&q, false, false).unwrap();
        assert_eq!(s.matches(" OR (").count() + 1, 20);
        assert!(s.starts_with("((text:a1 AND text:b1 AND text:c1) OR "));
        assert!(s.ends_with(" OR (text:d1 AND text:e1 AND text:f1))"));
        assert_eq!(
            query_string(&parse("-2 OF (a1 b1 c1) gold").unwrap(), false, true).unwrap(),
            "(NOT (((text:a1 OR text_as:a1) AND (text:b1 OR text_as:b1)) \
             OR ((text:a1 OR text_as:a1) AND (text:c1 OR text_as:c1)) \
             OR ((text:b1 OR text_as:b1) AND (text:c1 OR text_as:c1))) \
             AND (text:gold OR text_as:gold))"
        );
    }

    #[test]
    fn phrases_with_common_words_search_the_pairs_field() {
        let q = parse(r#""cross of gold" -bryan "gold silver"~3 "yellow fever""#).unwrap();
        assert_eq!(
            query_string(&q, true, false).unwrap(),
            "((text_cg:\"cross of_gold gold\" AND text:cross AND text:gold) \
             AND text:\"gold silver\"~3 AND text:\"yellow fever\" AND NOT text:bryan)"
        );
        // A repeated word is required once.
        assert_eq!(
            query_string(&parse(r#""the gold of the cross""#).unwrap(), true, false).unwrap(),
            "(text_cg:\"the_gold gold of_the the_cross cross\" AND text:gold AND text:cross)"
        );
        // A character that decomposes into several words (`ﷺ`) or with a
        // separator (`½`) is one word, as the analyzer has it (#168).
        assert_eq!(
            query_string(&parse("\"of \u{fdfa} gold\"").unwrap(), true, false).unwrap(),
            "(text_cg:\"of_\u{fdfa} \u{fdfa} gold\" AND text:\u{fdfa} AND text:gold)"
        );
        assert_eq!(
            query_string(&parse(r#""½ higher at 61¼""#).unwrap(), true, false).unwrap(),
            "(text_cg:\"½ higher at_61¼ 61¼\" AND text:½ AND text:higher AND text:61¼)"
        );
        // On an index built with version 1 the query folds them as its pairs
        // have them: `ﷺ` as its words run together, required only through
        // the pairs, and `½` as `1⁄2`.
        let v1 = |q: &str| usnm_core::query::parse_with(q, Analyzer::V1).unwrap();
        let s = query_string(&v1("\"of \u{fdfa} gold\""), true, false).unwrap();
        assert!(s.starts_with("(text_cg:\"of_"), "{s}");
        assert!(s.ends_with(" AND text:gold)"), "{s}");
        assert_eq!(s.matches(" AND ").count(), 1, "{s}");
        assert_eq!(
            query_string(&v1(r#""½ higher at 61¼""#), true, false).unwrap(),
            "(text_cg:\"1\u{2044}2 higher at_611\u{2044}4 611\u{2044}4\" AND text:1\u{2044}2 \
             AND text:higher AND text:611\u{2044}4)"
        );
        // Without the field, or for a phrase ending in a common word: `text`.
        assert_eq!(
            query_string(&parse(r#""cross of gold""#).unwrap(), false, false).unwrap(),
            r#"text:"cross of gold""#
        );
        assert_eq!(
            query_string(&parse(r#""remember the""#).unwrap(), true, false).unwrap(),
            r#"text:"remember the""#
        );
        let set = IndexSet::new(vec!["i".into()]).with_common_grams(true);
        let r = summary_request(
            &parse(r#""cross of gold""#).unwrap(),
            &filters(),
            &set,
            &BucketSpec::new(BucketUnit::Year, d("1896-01-01"), d("1896-12-31")),
        )
        .unwrap();
        assert!(r["query"].as_str().unwrap().starts_with("(text_cg:"), "{r}");
    }

    #[test]
    fn american_stories_text_is_searched_leaf_by_leaf() {
        let qs = |q: &str, grams| query_string(&parse(q).unwrap(), grams, true).unwrap();
        assert_eq!(qs("gold", false), "(text:gold OR text_as:gold)");
        assert_eq!(qs("silve*", false), "(text:silve* OR text_as:silve*)");
        assert_eq!(
            qs(r#""gold silver"~3"#, true),
            r#"(text:"gold silver"~3 OR text_as:"gold silver"~3)"#
        );
        assert_eq!(
            qs(r#""cross of gold""#, false),
            r#"(text:"cross of gold" OR text_as:"cross of gold")"#
        );
        assert_eq!(
            qs(r#""yellow fever""#, true),
            r#"(text:"yellow fever" OR text_as:"yellow fever")"#
        );
        // An exact phrase through the pairs stays within one text.
        assert_eq!(
            qs(r#""cross of gold""#, true),
            "((text_cg:\"cross of_gold gold\" AND text:cross AND text:gold) \
             OR (text_as_cg:\"cross of_gold gold\" AND text_as:cross AND text_as:gold))"
        );
        // Operators are unchanged; NOT excludes a page with the word in either text.
        assert_eq!(
            qs("bryan -silver (free OR orator)", false),
            "(NOT (text:silver OR text_as:silver) AND (text:bryan OR text_as:bryan) \
             AND ((text:free OR text_as:free) OR (text:orator OR text_as:orator)))"
        );
        assert!(matches!(
            query_string(&parse("gold~1").unwrap(), false, true),
            Err(SearchError::Unsupported(_))
        ));
        // Every request takes the flag from the index set.
        let set = IndexSet::new(vec!["i".into()]).with_american_stories(true);
        let r = summary_request(
            &parse("gold").unwrap(),
            &filters(),
            &set,
            &BucketSpec::new(BucketUnit::Year, d("1896-01-01"), d("1896-12-31")),
        )
        .unwrap();
        assert!(
            r["query"]
                .as_str()
                .unwrap()
                .starts_with("(text:gold OR text_as:gold) AND "),
            "{r}"
        );
    }

    /// One field for both texts (05 §5.5.7): each term or phrase once, in
    /// `text_all` and `text_all_cg`, never in a field of one text.
    #[test]
    fn one_field_for_both_texts_is_searched_once() {
        let single = |grams: bool| {
            none()
                .with_american_stories(true)
                .with_common_grams(grams)
                .with_text_layout(TextLayout::Single)
        };
        let qs = |q: &str, grams| query_for(&parse(q).unwrap(), &single(grams)).unwrap();
        assert_eq!(qs("gold", true), "text_all:gold");
        assert_eq!(qs("silve*", true), "text_all:silve*");
        assert_eq!(qs("presi?ent", true), "text_all:presi?ent");
        assert_eq!(
            qs(r#""gold silver"~3"#, true),
            r#"text_all:"gold silver"~3"#
        );
        assert_eq!(qs(r#""yellow fever""#, true), r#"text_all:"yellow fever""#);
        assert_eq!(
            qs(r#""cross of gold""#, true),
            "(text_all_cg:\"cross of_gold gold\" AND text_all:cross AND text_all:gold)"
        );
        // Without pairs it knows (a newer `common_grams`): the phrase itself.
        assert_eq!(
            qs(r#""cross of gold""#, false),
            r#"text_all:"cross of gold""#
        );
        assert_eq!(
            qs("bryan -silver (free OR orator)", true),
            "(NOT text_all:silver AND text_all:bryan AND (text_all:free OR text_all:orator))"
        );
        assert!(matches!(
            query_for(&parse("gold~1").unwrap(), &single(true)),
            Err(SearchError::Unsupported(_))
        ));
        // Whether the set says American Stories' text is there or not.
        assert_eq!(
            query_for(
                &parse("gold").unwrap(),
                &none().with_text_layout(TextLayout::Single)
            )
            .unwrap(),
            "text_all:gold"
        );
        // Every request takes it from the set, and none names a field of
        // one text.
        let q = parse(r#""cross of gold" bryan"#).unwrap();
        let spec = BucketSpec::new(BucketUnit::Year, d("1896-01-01"), d("1896-12-31"));
        for r in [
            summary_request(&q, &filters(), &single(true), &spec).unwrap(),
            cube_request(&q, &filters(), &single(true), &spec, &[0, 1]).unwrap(),
            place_cube_request(&q, &filters(), &single(true), &spec, &["P1".into()]).unwrap(),
            hits_request(&q, &filters(), &single(true), &HitsQuery::default()).unwrap(),
        ] {
            let s = r["query"].as_str().unwrap();
            assert!(s.starts_with("((text_all_cg:"), "{s}");
            for field in ["text:", "text_cg:", "text_as:", "text_as_cg:"] {
                assert!(!s.contains(&format!(" {field}")), "{s}");
                assert!(!s.contains(&format!("({field}")), "{s}");
            }
        }
        // A field per text is as before.
        assert_eq!(
            query_for(&parse("gold").unwrap(), &stories(true)).unwrap(),
            "(text:gold OR text_as:gold)"
        );
        assert_eq!(
            query_for(&parse("gold").unwrap(), &stories(false)).unwrap(),
            "text:gold"
        );
        // The count of pages only American Stories' text matches needs
        // LoC's text on its own: refused.
        assert!(matches!(
            american_stories_only_request(&q, &filters(), &single(true)),
            Err(SearchError::Unsupported(_))
        ));
    }

    #[test]
    fn builds_summary_and_sharded_cube_requests() {
        let q = parse("gold").unwrap();
        let spec = BucketSpec::new(BucketUnit::Week, d("1896-06-01"), d("1896-12-31"));
        let origin = spec.histogram_field().origin;
        let s = summary_request(&q, &filters(), &none(), &spec).unwrap();
        assert_eq!(
            s["query"],
            format!(
                "text:gold AND day:[{} TO {}] AND state:IN [GA SC] AND front_page:true",
                filters().from_day(),
                filters().to_day()
            )
        );
        assert_eq!(s["aggs"]["series"]["histogram"]["interval"], 7);
        assert_eq!(s["aggs"]["series"]["histogram"]["offset"], origin % 7);
        assert_eq!(s["aggs"]["first"]["min"]["field"], "day");
        assert_eq!(s["aggs"]["last"]["max"]["field"], "day");
        assert_eq!(s["aggs"]["places"]["aggs"]["first"]["min"]["field"], "day");
        assert_eq!(s["aggs"]["places"]["aggs"]["last"]["max"]["field"], "day");
        let c = cube_request(&q, &filters(), &none(), &spec, &[0, 5]).unwrap();
        assert!(c["query"]
            .as_str()
            .unwrap()
            .ends_with("AND place_shard:IN [0 5]"));
        let all = cube_request(&q, &filters(), &none(), &spec, &[0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        assert!(!all["query"].as_str().unwrap().contains("place_shard"));
    }

    #[test]
    fn a_version_laid_out_by_decade_names_the_searchs_decades() {
        let q = parse("gold").unwrap();
        let spec = BucketSpec::new(BucketUnit::Week, d("1896-06-01"), d("1896-12-31"));
        let set = IndexSet::new(vec!["i".into()]).with_decades(Some(1820..=1960));
        // After the exact `day` filter, the decade for the splits to open,
        // in every request: summaries, cubes, hits and the count.
        let want = format!(
            "text:gold AND day:[{} TO {}] AND state:IN [GA SC] AND front_page:true \
             AND decade:IN [1890]",
            filters().from_day(),
            filters().to_day()
        );
        let s = summary_request(&q, &filters(), &set, &spec).unwrap();
        assert_eq!(s["query"], want);
        let c = cube_request(&q, &filters(), &set, &spec, &[0, 5]).unwrap();
        assert!(c["query"]
            .as_str()
            .unwrap()
            .contains("AND decade:IN [1890] AND place_shard"));
        let h = hits_request(&q, &filters(), &set, &HitsQuery::default()).unwrap();
        assert!(h["query"]
            .as_str()
            .unwrap()
            .ends_with("AND decade:IN [1890]"));
        let both = set.clone().with_american_stories(true);
        let n = american_stories_only_request(&q, &filters(), &both).unwrap();
        assert!(n["query"]
            .as_str()
            .unwrap()
            .ends_with("AND decade:IN [1890]"));
        // A search over every decade of the version skips nothing: no clause.
        let mut all = filters();
        all.from = d("1700-01-01");
        all.to = d("1963-12-31");
        let s = summary_request(&q, &all, &set, &spec).unwrap();
        assert!(!s["query"].as_str().unwrap().contains("decade"));
        // A version without the field never gets the clause, and no search
        // ever filters on `date` or sends timestamps (#158).
        let s = summary_request(&q, &filters(), &none(), &spec).unwrap();
        assert!(!s["query"].as_str().unwrap().contains("decade"));
        for r in [
            summary_request(&q, &filters(), &set, &spec).unwrap(),
            hits_request(&q, &filters(), &set, &HitsQuery::default()).unwrap(),
        ] {
            assert!(r.get("start_timestamp").is_none() && r.get("end_timestamp").is_none());
            assert!(!r["query"].as_str().unwrap().contains("date:"));
        }
    }

    #[test]
    fn builds_place_cube_requests_by_day() {
        let q = parse("gold").unwrap();
        let spec = BucketSpec::new(BucketUnit::Day, d("1896-06-01"), d("1896-12-31"));
        let places = vec!["P00003".to_owned(), "P00001".to_owned()];
        let r = place_cube_request(&q, &filters(), &none(), &spec, &places).unwrap();
        assert_eq!(
            r["query"],
            format!(
                "text:gold AND day:[{} TO {}] AND state:IN [GA SC] AND front_page:true \
                 AND place_id:IN [P00003 P00001]",
                filters().from_day(),
                filters().to_day()
            )
        );
        assert_eq!(r["max_hits"], 0);
        let places_agg = &r["aggs"]["places"];
        assert_eq!(places_agg["terms"]["field"], "place_id");
        assert_eq!(places_agg["terms"]["size"], 2);
        let h = &places_agg["aggs"]["t"]["histogram"];
        assert_eq!(h["field"], "day");
        assert_eq!(h["interval"], 1);
        assert_eq!(h["min_doc_count"], 1);
        assert_eq!(h["offset"], 0);
        // Keys come back as day numbers: bucket = day - from.
        let from = usnm_core::time::day_number(d("1896-06-01"));
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 3,
            "aggregations": { "places": { "buckets": [
                {"key": "P00003", "doc_count": 3,
                 "t": {"buckets": [{"key": f64::from(from + 2), "doc_count": 1},
                                   {"key": f64::from(from + 9), "doc_count": 2}]}}
            ]}}
        }))
        .unwrap();
        let cells = parse_cube(&resp, &spec).unwrap();
        assert_eq!(
            cells.iter().map(|c| (c.bucket, c.hits)).collect::<Vec<_>>(),
            [(2, 1), (9, 2)]
        );
        // Anything but a plain id never reaches the query.
        for bad in [vec![], vec!["P1 OR x".to_owned()], vec![String::new()]] {
            assert!(matches!(
                place_cube_request(&q, &filters(), &none(), &spec, &bad),
                Err(SearchError::Rejected(_))
            ));
        }
    }

    #[test]
    fn hits_sort_by_day_then_sort_key() {
        let page = HitsQuery {
            place_id: Some("P00001".into()),
            lccn: None,
            sort: HitSort::Oldest,
            offset: 50,
            limit: 25,
            days: false,
        };
        let r = hits_request(&parse("gold").unwrap(), &filters(), &none(), &page).unwrap();
        // A leading `-` is ascending in Quickwit 0.9 (S-2): oldest first.
        assert_eq!(r["sort_by"], "-day,-sort_key");
        let newest = HitsQuery {
            sort: HitSort::Newest,
            ..page.clone()
        };
        let r2 = hits_request(&parse("gold").unwrap(), &filters(), &none(), &newest).unwrap();
        assert_eq!(r2["sort_by"], "day,sort_key");
        let relevant = HitsQuery {
            sort: HitSort::Relevant,
            ..page.clone()
        };
        let r3 = hits_request(&parse("gold").unwrap(), &filters(), &none(), &relevant).unwrap();
        assert_eq!(r3["sort_by"], "_score,-day");
        // Snippets are built from the stored text, not asked of Quickwit (#126).
        assert!(r.get("snippet_fields").is_none());
        // Distinct days (#127) only when asked, even on a first page.
        assert!(r.get("aggs").is_none());
        let first = HitsQuery {
            offset: 0,
            ..page.clone()
        };
        let r4 = hits_request(&parse("gold").unwrap(), &filters(), &none(), &first).unwrap();
        assert!(r4.get("aggs").is_none());
        let counted = HitsQuery {
            days: true,
            ..first.clone()
        };
        let r5 = hits_request(&parse("gold").unwrap(), &filters(), &none(), &counted).unwrap();
        assert_eq!(r5["aggs"]["days"]["cardinality"]["field"], "day");
        assert_eq!(r["start_offset"], 50);
        assert!(r["query"]
            .as_str()
            .unwrap()
            .ends_with("AND place_id:P00001"));
    }

    #[test]
    fn decodes_hits_from_stored_fields_only() {
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 2,
            "hits": [
                {"doc_id": "sn99000001_1896-07-10_ed-1_seq-1", "date": "1896-07-10T00:00:00Z",
                 "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 1,
                 "text": "a <gold> b"},
                {"doc_id": "sn99000001_1896-07-10_ed-1_seq-3", "date": "1896-07-10T00:00:00Z",
                 "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 3}
            ]
        }))
        .unwrap();
        let page = parse_hits(
            resp,
            &usnm_core::query::parse("gold").unwrap(),
            &stories(false),
        )
        .unwrap();
        assert_eq!(
            page.hits[0].day,
            usnm_core::time::day_number(d("1896-07-10"))
        );
        assert!(page.hits[0].front_page);
        assert!(!page.hits[1].front_page);
        assert_eq!(page.hits[0].snippets, vec!["a &lt;<mark>gold</mark>&gt; b"]);
        assert!(page.hits[1].snippets.is_empty());
        // Without the `days` aggregation (a later page), no count.
        assert_eq!(page.days, None);
        let first: SearchResponse = serde_json::from_value(json!({
            "num_hits": 3, "hits": [], "aggregations": { "days": { "value": 2.0 } }
        }))
        .unwrap();
        let q = usnm_core::query::parse("gold").unwrap();
        assert_eq!(
            parse_hits(first, &q, &stories(false)).unwrap().days,
            Some(2)
        );
    }

    /// A title's pages without a query (#274): the filters alone, in date
    /// order, never by relevance, and each page's opening as its snippet.
    #[test]
    fn browsing_sends_the_filters_alone_and_decodes_openings() {
        let page = HitsQuery {
            place_id: None,
            lccn: Some("sn99000001".into()),
            sort: HitSort::Newest,
            offset: 0,
            limit: 20,
            days: true,
        };
        let mut f = filters();
        f.lccns = vec!["sn99000001".into()];
        let r = browse_request(&f, &stories(true), &page).unwrap();
        assert_eq!(
            r["query"],
            format!(
                "day:[{} TO {}] AND state:IN [GA SC] AND lccn:IN [sn99000001] \
                 AND front_page:true AND lccn:sn99000001",
                f.from_day(),
                f.to_day()
            )
        );
        assert_eq!(r["sort_by"], "day,sort_key");
        assert_eq!(r["max_hits"], 20);
        assert_eq!(r["aggs"]["days"]["cardinality"]["field"], "day");
        let relevant = HitsQuery {
            sort: HitSort::Relevant,
            ..page.clone()
        };
        assert!(matches!(
            browse_request(&f, &none(), &relevant),
            Err(SearchError::Unsupported(_))
        ));
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 2,
            "hits": [
                {"doc_id": "sn99000001_1896-07-10_ed-1_seq-1", "date": "1896-07-10T00:00:00Z",
                 "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 1,
                 "text": "THE <BEE>\nOmaha", "text_as": "other"},
                {"doc_id": "sn99000001_1896-07-10_ed-1_seq-2", "date": "1896-07-10T00:00:00Z",
                 "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 2}
            ]
        }))
        .unwrap();
        let page = parse_browse(resp, &stories(true)).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.hits[0].snippets, vec!["THE &lt;BEE&gt; Omaha"]);
        assert!(page.hits[1].snippets.is_empty());
        assert!(page
            .hits
            .iter()
            .all(|h| h.matched_in.is_none() && h.snippet_source.is_none()));
    }

    /// A Congress page's stored fields come back with it, and the first page
    /// of its unit is no front page (#229).
    #[test]
    fn decodes_congress_hits() {
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 1,
            "hits": [
                {"doc_id": "fixture-globe-0001_p1", "date": "1850-01-07T00:00:00Z",
                 "place_id": "PC001", "lccn": "", "edition": 1, "seq": 1,
                 "text": "the gold of california", "series": "globe",
                 "title": "Fixture Globe", "citation": "Cong. Globe p. 1", "congress": 31,
                 "link": "https://example.org/p1", "ocr_source": "unt"}
            ]
        }))
        .unwrap();
        let q = usnm_core::query::parse("gold").unwrap();
        let hit = &parse_hits(resp, &q, &stories(false)).unwrap().hits[0];
        assert!(!hit.front_page);
        assert_eq!((hit.seq, hit.place_id.as_str()), (1, "PC001"));
        assert_eq!(hit.congress.series.as_deref(), Some("globe"));
        assert_eq!(hit.congress.congress, Some(31));
        assert_eq!(hit.congress.link.as_deref(), Some("https://example.org/p1"));
        assert_eq!(hit.ocr_source.as_deref(), Some("unt"));
        assert_eq!(hit.snippets, vec!["the <mark>gold</mark> of california"]);
    }

    /// A hit says which texts the query matches only when the search covers
    /// American Stories' text (05 §5.5.4), from the texts it already has.
    #[test]
    fn hits_say_which_texts_match_when_both_are_searched() {
        let resp = || -> SearchResponse {
            serde_json::from_value(json!({
                "num_hits": 3,
                "hits": [
                    {"doc_id": "sn99000001_1896-07-10_ed-1_seq-1", "date": "1896-07-10T00:00:00Z",
                     "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 1,
                     "text": "a cross of goid", "text_as": "a cross of gold"},
                    {"doc_id": "sn99000001_1896-07-10_ed-1_seq-2", "date": "1896-07-10T00:00:00Z",
                     "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 2,
                     "text": "gold", "text_as": "gold"},
                    {"doc_id": "sn99000001_1896-07-10_ed-1_seq-3", "date": "1896-07-10T00:00:00Z",
                     "place_id": "P00001", "lccn": "sn99000001", "edition": 1, "seq": 3,
                     "text": "gold"}
                ]
            }))
            .unwrap()
        };
        let q = usnm_core::query::parse("gold").unwrap();
        let on = parse_hits(resp(), &q, &stories(true)).unwrap();
        let matched: Vec<_> = on.hits.iter().map(|h| h.matched_in.clone()).collect();
        assert_eq!(
            matched,
            [
                Some(vec!["american_stories"]),
                Some(vec!["loc", "american_stories"]),
                Some(vec!["loc"]),
            ]
        );
        let off = parse_hits(resp(), &q, &stories(false)).unwrap();
        assert!(off.hits.iter().all(|h| h.matched_in.is_none()));
    }

    #[test]
    fn american_stories_only_asks_for_a_count_of_one_text_but_not_the_other() {
        let q = parse("gold -bryan").unwrap();
        let both = none().with_american_stories(true);
        let r = american_stories_only_request(&q, &filters(), &both).unwrap();
        assert_eq!(r["max_hits"], 0);
        assert!(r.get("aggs").is_none() && r.get("sort_by").is_none());
        assert!(r["query"].as_str().unwrap().starts_with(
            "((NOT (text:bryan OR text_as:bryan) AND (text:gold OR text_as:gold))) \
             AND NOT ((NOT text:bryan AND text:gold)) \
             AND ((NOT text_as:bryan AND text_as:gold)) AND day:["
        ));
        // Exact phrases through the pairs, one text at a time.
        let phrase = parse(r#""cross of gold""#).unwrap();
        assert_eq!(
            american_stories_only_string(&phrase, true).unwrap(),
            "(((text_cg:\"cross of_gold gold\" AND text:cross AND text:gold) \
             OR (text_as_cg:\"cross of_gold gold\" AND text_as:cross AND text_as:gold))) \
             AND NOT ((text_cg:\"cross of_gold gold\" AND text:cross AND text:gold)) \
             AND ((text_as_cg:\"cross of_gold gold\" AND text_as:cross AND text_as:gold))"
        );
    }

    #[test]
    fn rejects_partial_failures_without_echoing_them() {
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 3,
            "errors": [{"split_id": "x", "error": "failed on query text:secret"}]
        }))
        .unwrap();
        let err = check_complete(&resp, "text:secret")
            .unwrap_err()
            .to_string();
        assert!(err.contains("1 partial failure"));
        assert!(!err.contains("secret"));
    }

    #[test]
    fn partial_failures_say_what_they_were_about() {
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 3,
            "errors": [
                "split 01JA9XQ3V5Z8M2K7T4R6N1P0WB: aggregation memory limit exceeded on query text:secret",
                "split 01JA9XQ3V5Z8M2K7T4R6N1P0WC: Aggregation Memory Limit exceeded",
                "request timed out for text:SECRETSECRETSECRETSECRET12",
                "storage error: Azure blob read failed for text:secret"
            ]
        }))
        .unwrap();
        let err = check_complete(&resp, "text:secret OR text:SECRETSECRETSECRETSECRET12")
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "search backend error: quickwit reported 4 partial failure(s): \
             memory_limit×2, storage×1, timeout×1; \
             splits 01JA9XQ3V5Z8M2K7T4R6N1P0WB, 01JA9XQ3V5Z8M2K7T4R6N1P0WC"
        );
        assert!(!err.to_lowercase().contains("secret"), "{err}");
    }

    #[test]
    fn split_ids_are_named_only_when_they_look_like_ulids() {
        assert_eq!(
            splits_named(
                "failed splits: [01JA9XQ3V5Z8M2K7T4R6N1P0WB, 01JA9XQ3V5Z8M2K7T4R6N1P0WB]",
                ""
            ),
            "; splits 01JA9XQ3V5Z8M2K7T4R6N1P0WB"
        );
        // Too short, lower case, a letter Crockford leaves out, or not 0-7 first.
        assert_eq!(
            splits_named(
                "01JA9X 01ja9xq3v5z8m2k7t4r6n1p0wb 01JA9XQ3V5Z8M2K7T4R6N1P0WI 91JA9XQ3V5Z8M2K7T4R6N1P0WB",
                ""
            ),
            ""
        );
        // A word of the query is never named, whatever its case.
        assert_eq!(
            splits_named(
                "no match for 01JA9XQ3V5Z8M2K7T4R6N1P0WB",
                "text:01ja9xq3v5z8m2k7t4r6n1p0wb"
            ),
            ""
        );
    }

    #[test]
    fn causes_are_classified_by_keyword() {
        assert_eq!(
            cause("Aggregation memory limit of 768MB exceeded", ""),
            "memory_limit"
        );
        assert_eq!(
            cause("too many buckets: 210000 > 200000", ""),
            "bucket_limit"
        );
        assert_eq!(cause("deadline exceeded", ""), "timeout");
        assert_eq!(cause("Too many requests", ""), "overloaded");
        assert_eq!(cause("failed to fetch footer", ""), "split");
        assert_eq!(cause("Io error: broken pipe", ""), "storage");
        assert_eq!(cause("", ""), "empty");
        assert_eq!(cause("something else", ""), "other");
        assert_eq!(cause("timed out after 30s", ""), "timeout");
    }

    #[test]
    fn an_echoed_query_does_not_decide_the_cause() {
        // A search for "memory" that failed on storage is a storage failure.
        assert_eq!(
            cause(
                "Azure blob read failed for query text:memory",
                "text:memory"
            ),
            "storage"
        );
        assert_eq!(
            cause("failed for text:\"bucket http\"", "text:\"bucket http\""),
            "other"
        );
        // Words of the query that aren't echoed still count.
        assert_eq!(
            cause("aggregation memory limit exceeded", "text:gold"),
            "memory_limit"
        );
    }

    #[test]
    fn parses_aggregation_responses() {
        let spec = BucketSpec::new(BucketUnit::Year, d("1895-01-01"), d("1897-12-31"));
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 7,
            "aggregations": {
                "series": { "buckets": [ {"key": 1895.0, "doc_count": 2}, {"key": 1896.0, "doc_count": 5} ] },
                "first": {"value": 71000.0},
                "last": {"value": 71250.0},
                "places": { "buckets": [
                    {"key": "P00001", "doc_count": 4, "first": {"value": 71000.0}, "last": {"value": 71200.0},
                     "t": {"buckets": [{"key": 1895.0, "doc_count": 1}, {"key": 1896.0, "doc_count": 3}]}},
                    {"key": "P00002", "doc_count": 3, "first": {"value": 71100.0}, "last": {"value": 71250.0},
                     "t": {"buckets": [{"key": 1896.0, "doc_count": 3}]}}
                ]},
                "papers": { "buckets": [ {"key": "sn2", "doc_count": 3}, {"key": "sn1", "doc_count": 4},
                                         {"key": "sn3", "doc_count": 3} ] },
                "languages": { "buckets": [ {"key": "eng", "doc_count": 7}, {"key": "ger", "doc_count": 2} ] },
                "days": { "value": 5.0 }
            }
        }))
        .unwrap();
        let s = parse_summary(&resp, &spec).unwrap();
        assert_eq!(s.series, vec![2, 5, 0]);
        assert_eq!(s.places[0].first_day, 71000);
        assert_eq!(s.places[0].last_day, 71200);
        assert_eq!((s.first_day, s.last_day), (Some(71000), Some(71250)));
        // Most first, ties by key, whatever order Quickwit gives them in.
        let keys = |c: &[KeyCount]| {
            c.iter()
                .map(|k| (k.key.clone(), k.hits))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys(&s.papers),
            [("sn1".into(), 4), ("sn2".into(), 3), ("sn3".into(), 3)]
        );
        assert_eq!(keys(&s.languages), [("eng".into(), 7), ("ger".into(), 2)]);
        assert_eq!(s.days, 5);
        // Without `days`, a summary with matches is incomplete, not 0 days.
        let mut partial = resp.aggregations.clone().unwrap();
        partial.as_object_mut().unwrap().remove("days");
        let incomplete: SearchResponse =
            serde_json::from_value(json!({ "num_hits": 7, "aggregations": partial })).unwrap();
        assert!(matches!(
            parse_summary(&incomplete, &spec),
            Err(SearchError::Backend(_))
        ));
        let cells = parse_cube(&resp, &spec).unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(
            cells[1],
            CubeCell {
                place_id: "P00001".into(),
                bucket: 1,
                hits: 3
            }
        );
    }

    #[test]
    fn places_come_by_id_whatever_order_quickwit_gives_them_in() {
        // Two layouts of one index gave these tied buckets in opposite orders (#293).
        let spec = BucketSpec::new(BucketUnit::Year, d("1895-01-01"), d("1897-12-31"));
        let place = |key: &str, n: u64| {
            let (first, last) = (json!({"value": 71000.0}), json!({"value": 71200.0}));
            json!({"key": key, "doc_count": n, "first": first, "last": last})
        };
        let summary = |places: Vec<Value>| {
            let resp: SearchResponse = serde_json::from_value(json!({
                "num_hits": 128,
                "aggregations": {
                    "series": { "buckets": [] },
                    "first": {"value": 71000.0},
                    "last": {"value": 71200.0},
                    "places": { "buckets": places },
                    "papers": { "buckets": [] },
                    "languages": { "buckets": [] },
                    "days": { "value": 3.0 }
                }
            }))
            .unwrap();
            parse_summary(&resp, &spec).unwrap()
        };
        let a = summary(vec![
            place("P00009", 16),
            place("P00003", 56),
            place("P00004", 56),
        ]);
        let b = summary(vec![
            place("P00004", 56),
            place("P00003", 56),
            place("P00009", 16),
        ]);
        assert_eq!(a, b);
        let ids: Vec<&str> = a.places.iter().map(|p| p.place_id.as_str()).collect();
        assert_eq!(ids, ["P00003", "P00004", "P00009"]);
    }

    #[test]
    fn a_summary_with_no_matches_has_no_first_or_last_day() {
        let spec = BucketSpec::new(BucketUnit::Year, d("1895-01-01"), d("1897-12-31"));
        let resp: SearchResponse = serde_json::from_value(json!({
            "num_hits": 0,
            "aggregations": {
                "series": { "buckets": [] },
                "first": {"value": null},
                "last": {"value": null},
                "places": { "buckets": [] },
                "papers": { "buckets": [] },
                "languages": { "buckets": [] }
            }
        }))
        .unwrap();
        let s = parse_summary(&resp, &spec).unwrap();
        assert_eq!((s.first_day, s.last_day), (None, None));
    }

    #[test]
    fn a_summary_with_matches_needs_its_first_and_last_day() {
        let spec = BucketSpec::new(BucketUnit::Year, d("1895-01-01"), d("1897-12-31"));
        for (first, last) in [
            (
                Some(json!({"value": null})),
                Some(json!({"value": 71200.0})),
            ),
            (None, Some(json!({"value": 71200.0}))),
            (
                Some(json!({"value": 71000.0})),
                Some(json!({"value": "late"})),
            ),
        ] {
            let mut aggs = json!({
                "series": { "buckets": [] }, "places": { "buckets": [] },
                "papers": { "buckets": [] }, "languages": { "buckets": [] }
            });
            if let Some(f) = first {
                aggs["first"] = f;
            }
            if let Some(l) = last {
                aggs["last"] = l;
            }
            let resp: SearchResponse =
                serde_json::from_value(json!({ "num_hits": 4, "aggregations": aggs })).unwrap();
            assert!(matches!(
                parse_summary(&resp, &spec),
                Err(SearchError::Backend(_))
            ));
        }
    }

    #[test]
    fn a_place_without_its_days_is_an_error_not_day_zero() {
        let spec = BucketSpec::new(BucketUnit::Year, d("1895-01-01"), d("1897-12-31"));
        for place in [
            json!({"key": "P00001", "doc_count": 4, "first": {"value": 71000.0}}),
            json!({"key": "P00001", "doc_count": 4, "first": {"value": null}, "last": {"value": 71200.0}}),
        ] {
            let resp: SearchResponse = serde_json::from_value(json!({
                "num_hits": 4,
                "aggregations": {
                    "series": { "buckets": [] },
                    "first": {"value": 71000.0},
                    "last": {"value": 71200.0},
                    "places": { "buckets": [place] },
                    "papers": { "buckets": [] },
                    "languages": { "buckets": [] }
                }
            }))
            .unwrap();
            assert!(matches!(
                parse_summary(&resp, &spec),
                Err(SearchError::Backend(_))
            ));
        }
    }
}
