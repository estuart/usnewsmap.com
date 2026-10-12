//! Search backends for US News Map (05 §5.4).
//!
//! The API depends only on [`SearchBackend`]. Implementations:
//! - [`quickwit::QuickwitBackend`]: the production engine (ADR-0001).
//! - [`memory::MemoryBackend`]: a brute-force in-memory engine used for local
//!   development, tests, and as the correctness oracle for count checks.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use usnm_core::params::Filters;
use usnm_core::query::Node;
use usnm_core::text::Analyzer;
use usnm_core::text_layout::TextLayout;
use usnm_core::time::BucketSpec;

pub use language::{LanguageMatch, PageLanguageSets, PageLanguageSide};

pub mod cache_metrics;
pub mod language;
pub mod memory;
pub mod plan;
pub mod quickwit;
pub mod snippet;
pub mod thread_metrics;

/// One indexed page, as stored in the engine (05 §5.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageDoc {
    pub doc_id: String,
    pub day: u32,
    pub ym: u32,
    pub year: u16,
    pub place_id: String,
    pub place_shard: u8,
    pub lccn: String,
    pub state: String,
    #[serde(default)]
    pub language: Vec<String>,
    /// The page's own languages (05 §5.5.8), on a version with them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub page_language: Vec<String>,
    /// The same as one keyword (`eng_ger`, `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_language_set: Option<String>,
    pub front_page: bool,
    pub edition: u16,
    pub seq: u16,
    /// Same-day order for hits: `title ordinal << 32 | edition << 16 | seq`.
    /// Numeric because Quickwit 0.9 can't sort on text fields (05 §5.5).
    pub sort_key: u64,
    /// The batch this copy of the page came from. The fixtures don't have it.
    #[serde(default)]
    pub batch: String,
    /// The indexed text. For a Japanese page (#139), the tokens of `printed`
    /// joined by spaces ([`usnm_core::ja::index_text`]).
    pub text: String,
    /// A Japanese page's text as printed, for snippets (the index holds
    /// folded tokens). `None` on other pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printed: Option<String>,
    /// Who made the text when it isn't LoC's OCR (`usnm-ndlocr-lite`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_source: Option<String>,
    /// The engine and version that made it (`ndlocr-lite 636d1cf`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ocr_engine: Option<String>,
    /// American Stories' text of the page (05 §5.5.4), when it has one.
    /// Searched only when the index set says so
    /// ([`IndexSet::american_stories`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_as: Option<String>,
    /// A Congress page's fields (#229), in the Congress index set only
    /// (`infra/quickwit/congress-index.yaml`). `None` on newspaper pages.
    #[serde(flatten)]
    pub congress: CongressFields,
}

/// What a Congress page carries beyond a newspaper page's fields (#229):
/// which series it is from and what to cite and link. Every field is
/// optional and absent on newspaper pages.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CongressFields {
    /// `globe`, `serialset`, `annals` or `register`
    /// ([`usnm_core::params::CONGRESS_SERIES`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    /// The document's title (Serial Set) or the volume's label (debates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// As printed: "H. Doc. 16-42", "Cong. Globe, 25th Cong., 2nd Sess."
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub congress: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// `house`, `senate` or `joint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chamber: Option<String>,
    /// The page or column numbers as printed ("272", "1041-1042").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub printed_page: Option<String>,
    /// GovInfo package (Serial Set), UNT item or Congress.gov volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_id: Option<String>,
    /// The page in its source's viewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

impl CongressFields {
    /// Whether this is a Congress page.
    pub fn is_congress(&self) -> bool {
        self.series.is_some()
    }
}

/// The sealed indexes a published `index_version` names (08 §8.4.1), and
/// the copies of duplicated pages in them that searches must not see: pages
/// that ship in two batches, whose extra copy an earlier index already held
/// (04 §4.7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSet {
    ids: Vec<String>,
    /// The analyzer every index was built with (`usnm_core::text`, #168):
    /// queries are parsed with it, and a Japanese page's snippets fold its
    /// printed text with it. Pages' `text` is `usnm_text`'s whatever the
    /// version, so the memory backend and the other snippets read it as
    /// Quickwit does.
    analyzer: Analyzer,
    /// Every index has the `text_cg` field at a `usnm_core::common_grams`
    /// version the API supports, the one of [`IndexSet::analyzer`]
    /// (05 §5.5.3).
    common_grams: bool,
    /// Every index has American Stories' text at the API's
    /// `usnm_core::american_stories::VERSION` (05 §5.5.4), and searches
    /// cover it: in `text_as` and `text_as_cg`, or in the one searched
    /// field of [`TextLayout::Single`].
    american_stories: bool,
    /// Which fields the texts are searched in (05 §5.5.7, #283): a field
    /// per text, or one for both.
    text_layout: TextLayout,
    /// Every index has the `decade` field at the API's
    /// `usnm_core::decade::VERSION`, and the decades its pages span (05
    /// §5.5.5): a date-limited search names its decades, so Quickwit skips
    /// the splits of the others.
    decades: Option<RangeInclusive<u16>>,
    /// Every index has the page languages at the API's
    /// `usnm_core::page_language::VERSION`, and the version's sets of them
    /// with their pages (05 §5.5.8): `lang=` matches page languages,
    /// rendered as the smaller side, and pages are counted by them.
    page_languages: Option<Arc<PageLanguageSets>>,
    /// Batch → ids of its documents to hide.
    hidden: Arc<BTreeMap<String, BTreeSet<String>>>,
}

impl IndexSet {
    pub fn new(ids: Vec<String>) -> Self {
        Self {
            ids,
            analyzer: Analyzer::LATEST,
            common_grams: false,
            american_stories: false,
            text_layout: TextLayout::Separate,
            decades: None,
            page_languages: None,
            hidden: Arc::default(),
        }
    }

    /// The analyzer the indexes were built with ([`Analyzer::LATEST`] unless
    /// set).
    pub fn with_analyzer(mut self, analyzer: Analyzer) -> Self {
        self.analyzer = analyzer;
        self
    }

    pub fn analyzer(&self) -> Analyzer {
        self.analyzer
    }

    /// Whether phrases may search `text_cg` (05 §5.5.3).
    pub fn with_common_grams(mut self, on: bool) -> Self {
        self.common_grams = on;
        self
    }

    pub fn common_grams(&self) -> bool {
        self.common_grams
    }

    /// Whether queries also search American Stories' text (05 §5.5.4).
    pub fn with_american_stories(mut self, on: bool) -> Self {
        self.american_stories = on;
        self
    }

    pub fn american_stories(&self) -> bool {
        self.american_stories
    }

    /// The indexes' text fields (05 §5.5.7).
    pub fn with_text_layout(mut self, layout: TextLayout) -> Self {
        self.text_layout = layout;
        self
    }

    pub fn text_layout(&self) -> TextLayout {
        self.text_layout
    }

    /// Whether the aggregate can count the pages only American Stories'
    /// text matches (05 §5.5.4): searches cover it, in fields of its own.
    /// With one field for both texts, LoC's alone can't be searched.
    pub fn counts_american_stories_only(&self) -> bool {
        self.american_stories && self.text_layout == TextLayout::Separate
    }

    /// Whether searches name their decades, and the decades the version's
    /// pages span (05 §5.5.5).
    pub fn with_decades(mut self, span: Option<RangeInclusive<u16>>) -> Self {
        self.decades = span;
        self
    }

    pub fn decades(&self) -> Option<&RangeInclusive<u16>> {
        self.decades.as_ref()
    }

    /// Whether `lang=` matches page languages, and the version's sets of
    /// them (05 §5.5.8).
    pub fn with_page_languages(mut self, sets: Option<Arc<PageLanguageSets>>) -> Self {
        self.page_languages = sets;
        self
    }

    pub fn page_languages(&self) -> Option<&PageLanguageSets> {
        self.page_languages.as_deref()
    }

    /// How `langs` (a `lang=` filter) matches pages; `None` without one.
    pub fn language_match(&self, langs: &[String]) -> Option<LanguageMatch> {
        if langs.is_empty() {
            return None;
        }
        Some(match &self.page_languages {
            Some(sets) => sets.matching(langs),
            None => LanguageMatch::Title(langs.to_vec()),
        })
    }

    /// The field matching pages are counted by language in: their own
    /// languages on a version with them, their title's otherwise.
    pub fn language_field(&self) -> &'static str {
        if self.page_languages.is_some() {
            usnm_core::page_language::FIELD
        } else {
            "language"
        }
    }

    /// Hide each `(doc_id, batch)`: that batch's copy of the page.
    pub fn hiding(mut self, copies: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut hidden: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (doc_id, batch) in copies {
            hidden.entry(batch).or_default().insert(doc_id);
        }
        self.hidden = Arc::new(hidden);
        self
    }

    pub fn ids(&self) -> &[String] {
        &self.ids
    }

    /// The documents to hide, by batch.
    pub fn hidden(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.hidden
    }

    /// Whether `batch`'s copy of `doc_id` is hidden.
    pub fn hides(&self, doc_id: &str, batch: &str) -> bool {
        self.hidden.get(batch).is_some_and(|d| d.contains(doc_id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlaceSummary {
    pub place_id: String,
    pub hits: u64,
    pub first_day: u32,
    pub last_day: u32,
}

/// Matching pages for one value of a field: a title's LCCN or a language code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KeyCount {
    pub key: String,
    pub hits: u64,
}

/// Result of the summary call: national series, per-place totals, and first
/// and last appearance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub total_hits: u64,
    /// Earliest and latest matching day; `None` when nothing matches.
    pub first_day: Option<u32>,
    pub last_day: Option<u32>,
    /// One entry per bucket of the request's [`BucketSpec`].
    pub series: Vec<u64>,
    /// One entry per place with matches, by place id, so both backends agree.
    pub places: Vec<PlaceSummary>,
    /// Matching pages per newspaper (LCCN), most first, ties by LCCN (#121).
    pub papers: Vec<KeyCount>,
    /// Matching pages per title language, most first, ties by code. A page
    /// of a paper catalogued in several languages counts in each (#121).
    pub languages: Vec<KeyCount>,
    /// Days with at least one matching page (#127). Quickwit's count is a
    /// HyperLogLog estimate, close but not always exact.
    pub days: u64,
}

/// Sort counts most first, ties by key, so both backends agree.
pub fn rank(mut counts: Vec<KeyCount>) -> Vec<KeyCount> {
    counts.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.key.cmp(&b.key)));
    counts
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CubeCell {
    pub place_id: String,
    pub bucket: u32,
    pub hits: u32,
}

/// Order of a hit list. By date, pages on the same day keep the order of
/// `sort_key` (title, edition, page), reversed for newest first.
/// `Relevant` puts the pages that mention the query most first (#126): the
/// engine's score, which with `fieldnorms: false` on `text` ignores page
/// length. Quickwit weighs rare words per split, so it is "most mentions
/// first", not an exact ranking. Ties go oldest first. Quickwit 0.9 sorts by at
/// most two fields, so same-score pages on the same day come in the index's
/// order: stable between requests, but not by `sort_key` as here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HitSort {
    #[default]
    Oldest,
    Newest,
    Relevant,
}

impl HitSort {
    pub fn as_str(self) -> &'static str {
        match self {
            HitSort::Oldest => "oldest",
            HitSort::Newest => "newest",
            HitSort::Relevant => "relevant",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "oldest" => Some(HitSort::Oldest),
            "newest" => Some(HitSort::Newest),
            "relevant" => Some(HitSort::Relevant),
            _ => None,
        }
    }
}

/// [`SearchBackend::browse`]'s refusal of [`HitSort::Relevant`]: with no
/// query, no page mentions anything more than another.
pub fn browse_order(sort: HitSort) -> Result<(), SearchError> {
    match sort {
        HitSort::Relevant => Err(SearchError::Unsupported(
            "most mentions first without a query".into(),
        )),
        HitSort::Oldest | HitSort::Newest => Ok(()),
    }
}

/// Which hits to return. With neither `place_id` nor `lccn`, every match.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HitsQuery {
    pub place_id: Option<String>,
    pub lccn: Option<String>,
    pub sort: HitSort,
    pub offset: usize,
    pub limit: usize,
    /// Also count the days the selected pages appeared on (#127): for the
    /// first page of a list a visitor asked for, not the internal lookups.
    pub days: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub doc_id: String,
    pub day: u32,
    pub lccn: String,
    pub place_id: String,
    pub edition: u16,
    pub seq: u16,
    pub front_page: bool,
    /// HTML-escaped text with matches wrapped in `<mark>`.
    pub snippets: Vec<String>,
    /// Who made the page's text when it isn't LoC's OCR (`usnm-ndlocr-lite`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_source: Option<String>,
    /// The engine and version that made it (`ndlocr-lite 636d1cf`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_engine: Option<String>,
    /// Which text the snippets come from when it isn't LoC's:
    /// [`SNIPPETS_FROM_AMERICAN_STORIES`] when the query matched only in
    /// American Stories' text (05 §5.5.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet_source: Option<&'static str>,
    /// Which of the page's texts the query matches (05 §5.5.4):
    /// [`MATCHED_IN_LOC`], [`SNIPPETS_FROM_AMERICAN_STORIES`] or both, from
    /// [`snippet::matched_in`]. Only when the search covers American
    /// Stories' text, and not on Japanese pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_in: Option<Vec<&'static str>>,
    /// A Congress page's series, title, citation and link (#229); empty,
    /// and left out of the JSON, on newspaper pages.
    #[serde(flatten)]
    pub congress: CongressFields,
}

/// [`Hit::snippet_source`] for snippets from American Stories' text, and
/// that text in [`Hit::matched_in`].
pub const SNIPPETS_FROM_AMERICAN_STORIES: &str = "american_stories";

/// LoC's text (`text`) in [`Hit::matched_in`].
pub const MATCHED_IN_LOC: &str = "loc";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HitsPage {
    pub total: u64,
    pub hits: Vec<Hit>,
    /// Days with at least one of the selected pages (#127), when the query
    /// asked for them (`HitsQuery::days`); an estimate from Quickwit.
    pub days: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub fuzzy: bool,
    pub max_slop: u8,
    pub nested_aggregations: bool,
}

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("not supported by this search backend: {0}")]
    Unsupported(String),
    #[error("search backend timed out")]
    Timeout,
    #[error("search backend error: {0}")]
    Backend(String),
    /// The backend refused the request itself (a Quickwit 4xx): unlike
    /// `Backend`, trying again won't help.
    #[error("search backend rejected the request: {0}")]
    Rejected(String),
}

#[async_trait]
pub trait SearchBackend: Send + Sync {
    fn capabilities(&self) -> Capabilities;

    async fn summary(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
    ) -> Result<Summary, SearchError>;

    /// Cube cells for places whose `place_shard` is in `shards`.
    async fn cube(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
        shards: &[u8],
    ) -> Result<Vec<CubeCell>, SearchError>;

    /// Cube cells for the places in `places` only, whatever their shard
    /// (`/v1/days`). Callers pass validated place ids. A backend without it
    /// refuses.
    async fn place_cube(
        &self,
        _indexes: &IndexSet,
        _query: &Node,
        _filters: &Filters,
        _spec: &BucketSpec,
        _places: &[String],
    ) -> Result<Vec<CubeCell>, SearchError> {
        Err(SearchError::Unsupported(
            "matching pages per place and day".into(),
        ))
    }

    /// Hits by date in `page.sort` order, then by `sort_key` (title, edition,
    /// page), with snippets.
    async fn hits(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        page: &HitsQuery,
    ) -> Result<HitsPage, SearchError>;

    /// Pages without a query (#274): every page `filters` and `page`'s
    /// place or title select, by date in `page.sort` order and then by
    /// `sort_key`, as [`Self::hits`] lists them. Each hit's snippet is the
    /// opening of its text ([`snippet::opening_snippet`]); none has
    /// `snippet_source` or `matched_in`. With no query there is no
    /// relevance, so [`HitSort::Relevant`] is refused. A backend without it
    /// refuses.
    async fn browse(
        &self,
        _indexes: &IndexSet,
        _filters: &Filters,
        _page: &HitsQuery,
    ) -> Result<HitsPage, SearchError> {
        Err(SearchError::Unsupported(
            "listing pages without a query".into(),
        ))
    }

    /// How many of the pages a search of both texts finds match the query
    /// in American Stories' text but not in LoC's (05 §5.5.4): the hits
    /// whose [`Hit::matched_in`] is American Stories' text alone. One
    /// count-only request. A backend without it refuses, and so does every
    /// backend on a set with one field for both texts (05 §5.5.7), where
    /// LoC's alone can't be searched.
    async fn american_stories_only(
        &self,
        _indexes: &IndexSet,
        _query: &Node,
        _filters: &Filters,
    ) -> Result<u64, SearchError> {
        Err(SearchError::Unsupported(
            "counting the pages only American Stories' text matches".into(),
        ))
    }

    async fn health(&self) -> Result<(), SearchError>;

    /// Make every index in the set searchable before a version goes live, or
    /// fail so the loader keeps serving the previous version.
    async fn prepare(&self, _indexes: &IndexSet) -> Result<(), SearchError> {
        Ok(())
    }
}

/// Escape text for HTML and wrap highlighted ranges in `<mark>`.
pub fn mark_html(segments: &[(bool, &str)]) -> String {
    let mut out = String::new();
    for (marked, text) in segments {
        if *marked {
            out.push_str("<mark>");
        }
        for c in text.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&#39;"),
                c => out.push(c),
            }
        }
        if *marked {
            out.push_str("</mark>");
        }
    }
    out
}

/// Snippets of a Japanese page (#139) from its printed text, around the
/// query's positive words and phrases, HTML-escaped with `<mark>`. Folding is
/// one character for one, so the marks land on the text as printed. An exact
/// phrase is marked as a whole; a NEAR phrase's words are marked one by one;
/// a prefix, wildcard or fuzzy term marks each printed word it matches.
///
/// The page's Latin runs are folded by `analyzer`, the one the Japanese
/// index was built with (#168).
pub fn ja_snippets(printed: &str, query: &Node, analyzer: Analyzer) -> Vec<String> {
    const CONTEXT: usize = 40;
    let tokens = usnm_core::ja::tokenize(printed, analyzer);
    let mut phrases: Vec<Vec<String>> = Vec::new();
    let mut stack = vec![query];
    while let Some(node) = stack.pop() {
        match node {
            // Prefix, wildcard and `~ocr` (a Latin word in a Japanese query)
            // and fuzzy (memory backend only) terms: mark each printed word
            // they match.
            Node::Term(t) if !t.is_exact() => {
                for tok in tokens.iter().filter(|tok| memory::term_matches(t, tok)) {
                    phrases.push(vec![tok.clone()]);
                }
            }
            Node::Term(t) => phrases.push(vec![t.text.clone()]),
            Node::Phrase { terms, slop: 0 } => phrases.push(terms.clone()),
            Node::Phrase { terms, .. } => phrases.extend(terms.iter().map(|t| vec![t.clone()])),
            Node::And(c) | Node::Or(c) | Node::AtLeast { of: c, .. } => stack.extend(c),
            Node::Not(_) => {}
        }
    }
    phrases.sort();
    phrases.dedup();
    usnm_core::ja::snippets(printed, &phrases, CONTEXT, 1, analyzer)
        .into_iter()
        .map(|pieces| {
            let borrowed: Vec<(bool, &str)> =
                pieces.iter().map(|(m, s)| (*m, s.as_str())).collect();
            mark_html(&borrowed)
        })
        .collect()
}
