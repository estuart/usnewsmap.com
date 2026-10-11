//! Published version pointer and its reference snapshot (04 §4.3).
//!
//! `current.json` names the sealed index set and the reference snapshot that
//! were published together; both are immutable, so the API can hot-swap to a
//! new version atomically. The same layout is read from a Blob container (via
//! the managed identity) or a local directory, and every reference file is
//! checked against the snapshot's `manifest.json` before it is used.

use std::collections::HashMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use usnm_core::params::{ParamError, RawParams, SearchRequest, Source};
use usnm_core::query::Node;
use usnm_core::text::{Analyzer, Analyzers};
use usnm_core::time::{day_number, BucketSpec};
use usnm_search::IndexSet;
use usnm_state::state::{
    JaLatin, JaLatinPage, LanguageBaselines, LanguageSet, DUPLICATES_FILE, JA_LATIN_FILE,
    LANGUAGE_BASELINES_FILE, TITLE_PAGES_FILE,
};
use usnm_store::{is_safe_segment, ObjectStore};

/// Appended to a search's cache key when `USNM_AMERICAN_STORIES_SEARCH`
/// leaves out the version's American Stories text ([`RefData::search_key`]).
/// A canonical query is form-encoded, so it never contains `|`.
pub const AMERICAN_STORIES_OFF_KEY: &str = "|american_stories=off";

/// The reference files the API loads from each snapshot.
const FILES: [&str; 3] = ["places.json", "titles.json", "baselines.json"];

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Bounds {
    pub from: NaiveDate,
    pub to: NaiveDate,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Current {
    pub index_version: String,
    pub indexes: Vec<String>,
    /// Reference snapshot directory name.
    pub reference: String,
    pub bounds: Bounds,
    pub published_at: String,
    #[serde(default)]
    pub synthetic: bool,
    /// The Japanese pages' index (#139, 04 §4.8), when the version has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ja: Option<JaIndexes>,
    /// The `usnm_core::common_grams` version every index was built with
    /// (05 §5.5.3), which names their analyzer (#168); absent for versions
    /// without `text_cg`, whose analyzer is version 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_grams: Option<u32>,
    /// The `usnm_core::american_stories::VERSION` every index was built
    /// with (05 §5.5.4); absent for versions without `text_as`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub american_stories: Option<u32>,
    /// The `usnm_core::decade::VERSION` every index's `decade` field was
    /// written with (05 §5.5.5): the base is partitioned by decade, and its
    /// deltas tag their splits with theirs. Absent for versions without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decades: Option<u32>,
    /// The Congress index set (#229), when the version has one: the
    /// Congressional Globe, the Serial Set, the Annals and the Register,
    /// searched only with `source=congress`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub congress: Option<CongressIndexes>,
}

/// `current.json`'s `congress`: the Congress pages' index set (#229).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CongressIndexes {
    pub indexes: Vec<String>,
    /// The `usnm_core::common_grams` version the set was built with. A
    /// Congress search parses its query once, with the main indexes'
    /// analyzer, so the set is searched only when this matches the main
    /// indexes' `common_grams` (the release rebuilds it with them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub common_grams: Option<u32>,
    /// Each series the set holds, with its pages and dates.
    #[serde(default)]
    pub series: Vec<CongressSeries>,
}

/// One Congress series in a version: `globe`, `serialset`, `annals` or
/// `register` (`usnm_core::params::CONGRESS_SERIES`).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct CongressSeries {
    pub series: String,
    pub pages: u64,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

/// The reserved place every Congress page sits at for now (#229 design §3):
/// Washington, DC, apart from DC's newspapers' place.
pub const CONGRESS_PLACE: &str = "PC001";

/// The Congress place's name in `/v1/meta` and `/v1/hits`, which the site
/// shows as "{name}, {state}": "Congress, DC" (#229).
pub const CONGRESS_PLACE_NAME: &str = "Congress";

/// `current.json`'s `ja`: the index of the Japanese pages we OCR ourselves.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct JaIndexes {
    pub indexes: Vec<String>,
    /// The `usnm_core::ja` fold version the index was built with, which
    /// names its analyzer (#168).
    pub fold: u32,
    #[serde(default)]
    pub pages: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Place {
    pub id: String,
    pub ordinal: u32,
    pub name: String,
    pub state: String,
    pub lat: f64,
    pub lon: f64,
    pub precision: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Title {
    pub lccn: String,
    pub name: String,
    pub place_id: String,
    pub state: String,
    #[serde(default)]
    pub languages: Vec<String>,
}

/// Everything the API needs in memory for one published version.
#[derive(Debug)]
pub struct RefData {
    pub current: Current,
    pub places: Vec<Place>,
    pub place_index: HashMap<String, usize>,
    pub titles: HashMap<String, Title>,
    /// Pages published per place, as sorted `(day, pages)`.
    pub baselines: HashMap<String, Vec<(u32, u32)>>,
    /// Pages published per place and day for each set of title languages,
    /// from the snapshot's `language_baselines.json`; `None` for snapshots
    /// written before releases recorded it, which have no exact baselines
    /// under a language filter.
    pub language_baselines: Option<Vec<LanguageSet>>,
    /// Every page in the published version (the sum of the baselines).
    pub pages: u64,
    /// Pages published per place (each place's baselines summed).
    pub place_pages: HashMap<String, u64>,
    /// Pages published per title, from the snapshot's `title_pages.json`;
    /// `None` for snapshots written before releases recorded it.
    pub title_pages: Option<HashMap<String, u64>>,
    /// The batches (and their versions) the version was built from, from the
    /// snapshot manifest; `None` for snapshots that don't record them.
    pub published_batches: Option<HashMap<String, u16>>,
    /// Copies of pages that also ship in another batch, left out of `pages`
    /// (04 §4.7); `None` for snapshots that don't record them.
    pub duplicate_pages: Option<u64>,
    /// Copies of duplicated pages that an index of the version holds but
    /// searches must not see, from the snapshot's `duplicates.json`.
    pub hidden: Vec<HiddenCopy>,
    /// Pages whose main-index document holds our own OCR's Latin-script
    /// text, not LoC's (#203), from the snapshot's `ja_latin.json`: their
    /// hits are marked as our OCR. Empty for snapshots without it.
    pub ja_latin: HashMap<String, JaLatinPage>,
    /// Whether searches cover American Stories' text when the version has
    /// it (`USNM_AMERICAN_STORIES_SEARCH`, 05 §5.5.4). True when loaded; the
    /// API sets it from its configuration before the snapshot serves.
    pub american_stories_search: bool,
    /// Whether `source=congress` searches the version's Congress index set
    /// (`USNM_CONGRESS_SEARCH`, #229). True when loaded; the API sets it
    /// from its configuration before the snapshot serves.
    pub congress_search: bool,
}

/// One copy of a page to hide: the document `doc_id` from `batch`.
#[derive(Debug, Clone, Deserialize)]
pub struct HiddenCopy {
    pub doc_id: String,
    pub batch: String,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    /// The published version this snapshot was built for.
    index_version: String,
    files: Vec<ManifestFile>,
    #[serde(default)]
    built_from: Option<BuiltFrom>,
    #[serde(default)]
    duplicate_pages: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct BuiltFrom {
    batches: Vec<BuiltFromBatch>,
}

#[derive(Debug, Deserialize)]
struct BuiltFromBatch {
    batch: String,
    version: u16,
}

#[derive(Debug, Deserialize)]
struct ManifestFile {
    path: String,
    sha256: String,
    bytes: u64,
}

impl RefData {
    /// Load whatever `current.json` names now.
    pub async fn load(store: &dyn ObjectStore) -> Result<Self, String> {
        let current = read_current(store).await?;
        Self::load_for(store, current).await
    }

    /// Load the reference snapshot `current` names, verifying each file
    /// against the snapshot manifest.
    pub async fn load_for(store: &dyn ObjectStore, current: Current) -> Result<Self, String> {
        let dir = &current.reference;
        let manifest: Manifest = parse(
            &format!("{dir}/manifest.json"),
            &fetch(store, &format!("{dir}/manifest.json")).await?,
        )?;
        // Each version publishes its own snapshot (04 §4.4), so the manifest
        // must name the version `current.json` pairs it with.
        if manifest.index_version != current.index_version {
            return Err(format!(
                "{dir}/manifest.json is for `{}`, not `{}`",
                manifest.index_version, current.index_version
            ));
        }
        let mut raw = Vec::with_capacity(FILES.len());
        for name in FILES {
            let entry = manifest
                .files
                .iter()
                .find(|f| f.path == name)
                .ok_or_else(|| format!("{dir}/manifest.json does not list {name}"))?;
            raw.push(fetch_checked(store, dir, entry).await?);
        }
        // Optional: only snapshots whose manifest lists it have it.
        let optional = |name: &str| manifest.files.iter().find(|f| f.path == name);
        let title_pages = match optional(TITLE_PAGES_FILE) {
            Some(entry) => Some(fetch_checked(store, dir, entry).await?),
            None => None,
        };
        let hidden = match optional(DUPLICATES_FILE) {
            Some(entry) => {
                let (path, bytes) = fetch_checked(store, dir, entry).await?;
                parse(&path, &bytes)?
            }
            None => Vec::new(),
        };
        let language_baselines = match optional(LANGUAGE_BASELINES_FILE) {
            Some(entry) => Some(fetch_checked(store, dir, entry).await?),
            None => None,
        };
        let ja_latin = match optional(JA_LATIN_FILE) {
            Some(entry) => Some(fetch_checked(store, dir, entry).await?),
            None => None,
        };
        let published_batches = manifest.built_from.map(|b| {
            b.batches
                .into_iter()
                .map(|b| (b.batch, b.version))
                .collect()
        });
        // Parsing large snapshots is CPU-bound; keep it off the async workers.
        let mut refdata = tokio::task::spawn_blocking(move || {
            let mut rd = Self::build(current, &raw)?;
            if let Some((path, bytes)) = title_pages {
                rd.title_pages = Some(parse(&path, &bytes)?);
            }
            if let Some((path, bytes)) = language_baselines {
                let mut lb: LanguageBaselines = parse(&path, &bytes)?;
                for series in lb.sets.iter_mut().flat_map(|s| s.baselines.values_mut()) {
                    series.sort_unstable();
                }
                rd.language_baselines = Some(lb.sets);
            }
            if let Some((path, bytes)) = ja_latin {
                let latin: JaLatin = parse(&path, &bytes)?;
                rd.ja_latin = latin.pages.into_iter().collect();
            }
            Ok::<_, String>(rd)
        })
        .await
        .map_err(|e| e.to_string())??;
        refdata.published_batches = published_batches;
        refdata.duplicate_pages = manifest.duplicate_pages;
        // A copy of ours that the version hides isn't what its hits show.
        if !refdata.ja_latin.is_empty() {
            let hidden_ours: std::collections::HashSet<(&str, &str)> = hidden
                .iter()
                .filter(|h: &&HiddenCopy| refdata.ja_latin.contains_key(&h.doc_id))
                .map(|h| (h.doc_id.as_str(), h.batch.as_str()))
                .collect();
            refdata
                .ja_latin
                .retain(|doc_id, p| !hidden_ours.contains(&(doc_id.as_str(), p.batch.as_str())));
        }
        refdata.hidden = hidden;
        Ok(refdata)
    }

    fn build(current: Current, raw: &[(String, Vec<u8>)]) -> Result<Self, String> {
        let [(pp, places), (tp, titles), (bp, baselines)] = raw else {
            return Err("reference snapshot is incomplete".into());
        };
        let places: Vec<Place> = parse(pp, places)?;
        let titles: Vec<Title> = parse(tp, titles)?;
        let mut baselines: HashMap<String, Vec<(u32, u32)>> = parse(bp, baselines)?;
        for series in baselines.values_mut() {
            series.sort_unstable();
        }
        let place_index = places
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        let place_pages: HashMap<String, u64> = baselines
            .iter()
            .map(|(id, series)| (id.clone(), series.iter().map(|&(_, n)| u64::from(n)).sum()))
            .collect();
        let pages = place_pages.values().sum();
        // Queries are folded as each index was built (#168): only a version
        // newer than this API's can't be.
        if let Some(v) = current
            .common_grams
            .filter(|v| usnm_core::common_grams::analyzer(*v).is_none())
        {
            tracing::warn!(
                index_common_grams = v,
                api_common_grams = usnm_core::common_grams::VERSION,
                "the indexes were built with common-word pairs this API doesn't know; phrases \
                 search `text`, and words this API folds differently may miss"
            );
        }
        if let Some(ja) = current
            .ja
            .as_ref()
            .filter(|ja| usnm_core::ja::analyzer(ja.fold).is_none())
        {
            tracing::warn!(
                index_fold = ja.fold,
                api_fold = usnm_core::ja::FOLD_VERSION,
                "the Japanese index was folded differently from this API; Japanese searches may miss"
            );
        }
        if current
            .congress
            .as_ref()
            .is_some_and(|c| c.series.is_empty())
        {
            tracing::warn!(
                "the Congress index set lists no series, so no dates; Congress searches answer 422"
            );
        }
        Ok(Self {
            current,
            places,
            place_index,
            titles: titles.into_iter().map(|t| (t.lccn.clone(), t)).collect(),
            baselines,
            language_baselines: None,
            pages,
            place_pages,
            title_pages: None,
            published_batches: None,
            duplicate_pages: None,
            hidden: Vec::new(),
            ja_latin: HashMap::new(),
            american_stories_search: true,
            congress_search: true,
        })
    }

    pub fn version(&self) -> &str {
        &self.current.index_version
    }

    /// The analyzers the version's indexes were built with, from
    /// `common_grams` and `ja.fold` (#168): version 1 where they're absent
    /// (built before either existed), and this API's latest for a version it
    /// doesn't know.
    pub fn analyzers(&self) -> Analyzers {
        let main = match self.current.common_grams {
            None => Analyzer::V1,
            Some(v) => usnm_core::common_grams::analyzer(v).unwrap_or(Analyzer::LATEST),
        };
        let ja = self.current.ja.as_ref().map_or(main, |ja| {
            usnm_core::ja::analyzer(ja.fold).unwrap_or(Analyzer::LATEST)
        });
        Analyzers { main, ja }
    }

    /// A search request's parameters, parsed for this version: dates within
    /// its bounds, words folded as its indexes were.
    pub fn search_request(&self, raw: &RawParams) -> Result<SearchRequest, ParamError> {
        SearchRequest::from_raw(raw, self.bounds_for(raw.get("source")), self.analyzers())
    }

    /// The dates a search's `from` and `to` are clamped to (#229): the
    /// newspapers' bounds, the Congress series' for `source=congress`
    /// (1789 to 1994 in full, past the newspapers at both ends), and the
    /// span of both for `source=all`. An unknown `source` gets the
    /// newspapers' bounds and is refused when the request is parsed.
    pub fn bounds_for(&self, source: Option<&str>) -> (NaiveDate, NaiveDate) {
        let news = self.bounds();
        let Some(congress) = self.congress_bounds() else {
            return news;
        };
        match source {
            Some("congress") => congress,
            Some("all") => (news.0.min(congress.0), news.1.max(congress.1)),
            _ => news,
        }
    }

    /// The earliest and latest dates of the version's Congress series.
    pub fn congress_bounds(&self) -> Option<(NaiveDate, NaiveDate)> {
        let series = &self.current.congress.as_ref()?.series;
        let from = series.iter().map(|s| s.from).min()?;
        let to = series.iter().map(|s| s.to).max()?;
        Some((from, to))
    }

    /// Whether the version was built with American Stories' text at this
    /// API's version: only such a version has `text_as` (05 §5.5.4).
    pub fn has_american_stories(&self) -> bool {
        self.current.american_stories == Some(usnm_core::american_stories::VERSION)
    }

    /// Whether searches cover American Stories' text: the version has it and
    /// `USNM_AMERICAN_STORIES_SEARCH` doesn't switch it off.
    pub fn searches_american_stories(&self) -> bool {
        self.has_american_stories() && self.american_stories_search
    }

    /// Whether the setting leaves out the American Stories' text the version
    /// has. Searches then answer as if it had none, and their cache keys say
    /// so ([`Self::search_key`]).
    pub fn american_stories_switched_off(&self) -> bool {
        self.has_american_stories() && !self.american_stories_search
    }

    /// The response caches' key (in process and, hashed, in Blob, 06 §6.5)
    /// for a search on `endpoint` for `query`, whose canonical parameters
    /// are `canonical`: `{index_version}|{endpoint}|{canonical}`, with
    /// [`AMERICAN_STORIES_OFF_KEY`] appended when American Stories' text is
    /// switched off and `query` searches the main indexes, so responses
    /// computed without it and with it never stand in for each other. A
    /// Japanese query searches the Japanese pages, which have no American
    /// Stories text, so its key never changes. With the setting on the key
    /// is the same as before the setting existed, and responses cached then
    /// still serve.
    pub fn search_key(&self, endpoint: &str, canonical: &str, query: &Node) -> String {
        let mut key = format!("{}|{endpoint}|{canonical}", self.version());
        if self.american_stories_switched_off() && !usnm_core::query::is_japanese(query) {
            key.push_str(AMERICAN_STORIES_OFF_KEY);
        }
        key
    }

    pub fn index_set(&self) -> IndexSet {
        // Phrases search `text_cg` when every index has it at a version this
        // API knows: the query's words are then folded with that version's
        // analyzer, as the pairs were (#168). A version without the pairs,
        // or with a newer one, keeps them in `text` (05 §5.5.3).
        let grams = self
            .current
            .common_grams
            .and_then(usnm_core::common_grams::analyzer)
            .is_some();
        let american = self.searches_american_stories();
        // Date-limited searches name their decades, so Quickwit skips the
        // other decades' splits, only on a version laid out with this API's
        // decades (05 §5.5.5): another version has no `decade` field, or
        // other buckets.
        let decades = (self.current.decades == Some(usnm_core::decade::VERSION)).then(|| {
            usnm_core::decade::of_date(self.current.bounds.from)
                ..=usnm_core::decade::of_date(self.current.bounds.to)
        });
        IndexSet::new(self.current.indexes.clone())
            .with_analyzer(self.analyzers().main)
            .with_common_grams(grams)
            .with_american_stories(american)
            .with_decades(decades)
            .hiding(
                self.hidden
                    .iter()
                    .map(|h| (h.doc_id.clone(), h.batch.clone())),
            )
    }

    /// The Japanese pages' index set, when the version has one.
    pub fn ja_index_set(&self) -> Option<IndexSet> {
        self.current
            .ja
            .as_ref()
            .map(|j| IndexSet::new(j.indexes.clone()).with_analyzer(self.analyzers().ja))
    }

    /// The Congress index set (#229), when the version has one built with
    /// the main indexes' analyzer, with the series that date its searches,
    /// and `USNM_CONGRESS_SEARCH` leaves it on.
    pub fn congress_index_set(&self) -> Option<IndexSet> {
        let c = self.current.congress.as_ref()?;
        // Without series a Congress search has no dates of its own
        // ([`Self::congress_bounds`]) and would be clamped to the
        // newspapers': refused instead.
        if !self.congress_search
            || c.series.is_empty()
            || c.common_grams != self.current.common_grams
        {
            return None;
        }
        let grams = c
            .common_grams
            .and_then(usnm_core::common_grams::analyzer)
            .is_some();
        Some(
            IndexSet::new(c.indexes.clone())
                .with_analyzer(self.analyzers().main)
                .with_common_grams(grams),
        )
    }

    /// The indexes a request searches (#229): the newspapers' for
    /// `source=news` (main or Japanese by the query, [`Self::index_set_for`]),
    /// the Congress set for `source=congress`. Both at once (`source=all`)
    /// answers 422 until the API returns two result blocks.
    pub fn index_set_for_request(
        &self,
        req: &usnm_core::params::SearchRequest,
    ) -> Result<IndexSet, crate::error::ApiError> {
        use crate::error::ApiError::Unsupported;
        match req.source {
            Source::News => self
                .index_set_for(&req.query)
                .ok_or_else(crate::routes::no_japanese),
            Source::Congress if usnm_core::query::is_japanese(&req.query) => {
                Err(Unsupported("Japanese text search of Congress pages".into()))
            }
            Source::Congress => self
                .congress_index_set()
                .ok_or_else(|| Unsupported("Searching Congress pages".into())),
            Source::All => Err(Unsupported(
                "Searching newspapers and Congress together".into(),
            )),
        }
    }

    /// Refuse a request this version can't search before any cache is asked
    /// (#229): a Congress response cached while `USNM_CONGRESS_SEARCH` was on
    /// isn't served once it's off, and a refused search takes no flight,
    /// Blob read or search slot. A newspaper search passes without building
    /// anything; its index set is still chosen in the computation.
    pub fn check_source(&self, req: &SearchRequest) -> Result<(), crate::error::ApiError> {
        if req.source == Source::News {
            return Ok(());
        }
        self.index_set_for_request(req).map(|_| ())
    }

    /// The indexes a query searches: a query with a Japanese word searches
    /// the Japanese pages (#139), any other the main indexes. `None` for a
    /// Japanese query on a version without Japanese pages.
    pub fn index_set_for(&self, query: &usnm_core::query::Node) -> Option<IndexSet> {
        if usnm_core::query::is_japanese(query) {
            self.ja_index_set()
        } else {
            Some(self.index_set())
        }
    }

    pub fn bounds(&self) -> (NaiveDate, NaiveDate) {
        (self.current.bounds.from, self.current.bounds.to)
    }

    pub fn place(&self, id: &str) -> Option<&Place> {
        self.place_index.get(id).map(|&i| &self.places[i])
    }

    /// Whether a search on `source` can name the place `id`: a newspaper
    /// place, or the reserved Congress place, which isn't in the
    /// newspapers' catalog, when the search covers Congress pages (#229).
    pub fn knows_place(&self, id: &str, source: Source) -> bool {
        self.place(id).is_some() || (source.congress() && id == CONGRESS_PLACE)
    }

    /// Whether the baselines are exact under a language filter: always with
    /// none, otherwise only when the snapshot has them per language.
    pub fn has_baselines_for(&self, langs: &[String]) -> bool {
        langs.is_empty() || self.language_baselines.is_some()
    }

    /// Pages published per bucket for one place: all of them, or only those of
    /// titles that list any of `langs`. Check [`Self::has_baselines_for`]
    /// first: without per-language counts a language filter gets zeros.
    pub fn place_baseline(&self, place_id: &str, spec: &BucketSpec, langs: &[String]) -> Vec<u64> {
        let mut out = vec![0u64; spec.len()];
        if langs.is_empty() {
            if let Some(series) = self.baselines.get(place_id) {
                add_series(&mut out, series, spec);
            }
        } else {
            for set in self.language_sets(langs) {
                if let Some(series) = set.baselines.get(place_id) {
                    add_series(&mut out, series, spec);
                }
            }
        }
        out
    }

    /// National pages published per bucket, restricted to `states` when
    /// non-empty and to titles that list any of `langs` when that is.
    pub fn national_baseline(
        &self,
        spec: &BucketSpec,
        states: &[String],
        langs: &[String],
    ) -> Vec<u64> {
        let mut out = vec![0u64; spec.len()];
        let in_scope = |id: &str| {
            states.is_empty() || self.place(id).is_some_and(|p| states.contains(&p.state))
        };
        if langs.is_empty() {
            for (_, series) in self.baselines.iter().filter(|(id, _)| in_scope(id)) {
                add_series(&mut out, series, spec);
            }
        } else {
            for set in self.language_sets(langs) {
                for (_, series) in set.baselines.iter().filter(|(id, _)| in_scope(id)) {
                    add_series(&mut out, series, spec);
                }
            }
        }
        out
    }

    /// The sets of title languages that share one of `langs`; each page is in
    /// one set, so summing them counts every matching page once.
    fn language_sets<'a>(&'a self, langs: &'a [String]) -> impl Iterator<Item = &'a LanguageSet> {
        self.language_baselines
            .iter()
            .flatten()
            .filter(move |set| set.languages.iter().any(|l| langs.contains(l)))
    }
}

/// Add a place's sorted `(day, pages)` series that fall inside `spec` to `out`.
fn add_series(out: &mut [u64], series: &[(u32, u32)], spec: &BucketSpec) {
    let (from, to) = (day_number(spec.from), day_number(spec.to));
    let start = series.partition_point(|(d, _)| *d < from);
    for &(day, pages) in series[start..].iter().take_while(|(d, _)| *d <= to) {
        out[spec.index_of_day(day)] += u64::from(pages);
    }
}

/// Read only the version pointer. Its ids become object paths and cache
/// prefixes, so each must be a single safe path segment.
pub async fn read_current(store: &dyn ObjectStore) -> Result<Current, String> {
    let current: Current = parse("current.json", &fetch(store, "current.json").await?)?;
    let ja = current.ja.iter().flat_map(|j| &j.indexes);
    let congress = current.congress.iter().flat_map(|c| &c.indexes);
    for id in [&current.index_version, &current.reference]
        .into_iter()
        .chain(&current.indexes)
        .chain(ja)
        .chain(congress)
    {
        if !is_safe_segment(id) {
            return Err(format!("current.json: `{id}` is not a valid id"));
        }
    }
    if current.indexes.is_empty() {
        return Err("current.json lists no indexes".into());
    }
    if current.ja.as_ref().is_some_and(|j| j.indexes.is_empty()) {
        return Err("current.json names a Japanese index set with no indexes".into());
    }
    if let Some(c) = &current.congress {
        if c.indexes.is_empty() {
            return Err("current.json names a Congress index set with no indexes".into());
        }
        if let Some(s) = c
            .series
            .iter()
            .find(|s| !usnm_core::params::CONGRESS_SERIES.contains(&s.series.as_str()))
        {
            return Err(format!(
                "current.json: unknown Congress series `{}`",
                s.series
            ));
        }
        // Requests clamp their dates to these spans, which needs from ≤ to.
        if let Some(s) = c.series.iter().find(|s| s.from > s.to) {
            return Err(format!(
                "current.json: Congress series `{}` ends before it starts",
                s.series
            ));
        }
    }
    Ok(current)
}

/// `{dir}/{entry.path}`, checked against its manifest entry.
async fn fetch_checked(
    store: &dyn ObjectStore,
    dir: &str,
    entry: &ManifestFile,
) -> Result<(String, Vec<u8>), String> {
    let path = format!("{dir}/{}", entry.path);
    let bytes = fetch(store, &path).await?;
    let digest = hex(&Sha256::digest(&bytes));
    if bytes.len() as u64 != entry.bytes || !digest.eq_ignore_ascii_case(&entry.sha256) {
        return Err(format!("{path} does not match its manifest entry"));
    }
    Ok((path, bytes))
}

async fn fetch(store: &dyn ObjectStore, path: &str) -> Result<Vec<u8>, String> {
    store
        .get(path)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{path} not found in {store:?}"))
}

fn parse<T: for<'de> Deserialize<'de>>(path: &str, bytes: &[u8]) -> Result<T, String> {
    serde_json::from_slice(bytes).map_err(|e| format!("{path}: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
