//! Brute-force in-memory backend. It evaluates the AST exactly against
//! tokenized text, so it doubles as the oracle for engine count checks
//! (05 §5.8) and as the backend for local development on the fixture corpus.

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use usnm_core::params::Filters;
use usnm_core::query::{wildcard_matches, Node, Term};
use usnm_core::text::{fold, tokenize, Analyzer, USNM_TEXT};
use usnm_core::time::BucketSpec;

use crate::snippet::{matched_in, page_snippets};
use crate::{
    ja_snippets, rank, Capabilities, CubeCell, Hit, HitSort, HitsPage, HitsQuery, IndexSet,
    KeyCount, PageDoc, PlaceSummary, SearchBackend, SearchError, Summary,
};

struct Indexed {
    doc: PageDoc,
    /// The page's words as Quickwit indexes them: `usnm_text`'s tokens of
    /// `text`, whatever analyzer version built the index (#168), or a
    /// Japanese page's tokens as written (`whitespace`).
    tokens: Vec<String>,
    /// American Stories' text's tokens (05 §5.5.4), when the page has it.
    as_tokens: Option<Vec<String>>,
}

impl Indexed {
    /// The token streams a search reads: LoC's text, and American Stories'
    /// when the index set searches it.
    fn texts(&self, american_stories: bool) -> Vec<&[String]> {
        let mut out = vec![self.tokens.as_slice()];
        if american_stories {
            out.extend(self.as_tokens.as_deref());
        }
        out
    }

    /// The query as the engine reads it on this page: analyzed again by
    /// `usnm_text` ([`as_analyzed`]) on a main-index page, as it is on a
    /// Japanese page, whose `whitespace` tokenizer keeps each term whole.
    fn reads<'q>(&self, query: &'q Node, analyzed: &'q Node) -> &'q Node {
        if self.doc.printed.is_some() {
            query
        } else {
            analyzed
        }
    }
}

/// `query` as Quickwit reads it on `text` and `text_as`, whose `usnm_text`
/// analyzer analyzes a query's words again: an exact term or a phrase's
/// words become the words the analyzer makes of them. A term the API folded
/// into several words (`1⁄2` with `Analyzer::V1`, #168) is then the phrase
/// of them ("1 2"), and one the analyzer drops matches nothing. For a query
/// parsed with the latest analyzer this changes nothing.
///
/// A word `Analyzer::V1` folded into words with spaces between them (`ﷺ`)
/// stays whole: a phrase requires it through the pairs, where it is the
/// page's one character, folded and run together ([`word_matches`]).
pub fn as_analyzed(query: &Node) -> Node {
    let words = |t: &str| {
        if t.contains(char::is_whitespace) {
            vec![t.to_owned()]
        } else {
            tokenize(t, USNM_TEXT)
        }
    };
    match query {
        Node::Term(t) if !t.prefix && !t.wildcard && t.fuzzy == 0 => {
            let w = words(&t.text);
            if w.len() == 1 && w[0] == t.text {
                query.clone()
            } else {
                Node::Phrase { terms: w, slop: 0 }
            }
        }
        Node::Term(_) => query.clone(),
        Node::Phrase { terms, slop } => Node::Phrase {
            terms: terms.iter().flat_map(|t| words(t)).collect(),
            slop: *slop,
        },
        Node::And(c) => Node::And(c.iter().map(as_analyzed).collect()),
        Node::Or(c) => Node::Or(c.iter().map(as_analyzed).collect()),
        Node::Not(n) => Node::Not(Box::new(as_analyzed(n))),
    }
}

/// Documents grouped by index id, so explicit index sets behave like the real engine.
#[derive(Default)]
pub struct MemoryBackend {
    indexes: HashMap<String, Vec<Indexed>>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_index(&mut self, index_id: &str, docs: impl IntoIterator<Item = PageDoc>) {
        let entry = self.indexes.entry(index_id.to_owned()).or_default();
        entry.extend(docs.into_iter().map(|doc| Indexed {
            // A Japanese page's text is already its tokens (Quickwit's
            // `whitespace` tokenizer, pages-ja-index.yaml); folding them again
            // would strip dakuten (が → か).
            tokens: if doc.printed.is_some() {
                doc.text.split_whitespace().map(str::to_owned).collect()
            } else {
                tokenize(&doc.text, USNM_TEXT)
            },
            as_tokens: doc.text_as.as_deref().map(|t| tokenize(t, USNM_TEXT)),
            doc,
        }));
    }

    fn matching<'a>(
        &'a self,
        indexes: &'a IndexSet,
        query: &'a Node,
        filters: &'a Filters,
    ) -> Result<impl Iterator<Item = &'a Indexed> + 'a, SearchError> {
        let mut selected = Vec::new();
        for id in indexes.ids() {
            let docs = self
                .indexes
                .get(id)
                .ok_or_else(|| SearchError::Backend(format!("index `{id}` not found")))?;
            selected.push(docs);
        }
        let (from, to) = (filters.from_day(), filters.to_day());
        let analyzed = as_analyzed(query);
        Ok(selected.into_iter().flatten().filter(move |d| {
            let doc = &d.doc;
            doc.day >= from
                && doc.day <= to
                && (filters.states.is_empty() || filters.states.contains(&doc.state))
                && (filters.lccns.is_empty() || filters.lccns.contains(&doc.lccn))
                && (filters.langs.is_empty()
                    || doc.language.iter().any(|l| filters.langs.contains(l)))
                && (!filters.front_only || doc.front_page)
                && (filters.series.is_empty()
                    || doc
                        .congress
                        .series
                        .as_ref()
                        .is_some_and(|s| filters.series.contains(s)))
                && !indexes.hides(&doc.doc_id, &doc.batch)
                && eval_texts(
                    d.reads(query, &analyzed),
                    &d.texts(indexes.american_stories()),
                )
        }))
    }
}

#[async_trait]
impl SearchBackend for MemoryBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            fuzzy: true,
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
        let mut series = vec![0u64; spec.len()];
        let mut places: BTreeMap<&str, (u64, u32, u32)> = BTreeMap::new();
        let mut total = 0;
        let mut span: Option<(u32, u32)> = None;
        let mut days = std::collections::BTreeSet::new();
        let mut papers: BTreeMap<&str, u64> = BTreeMap::new();
        let mut languages: BTreeMap<&str, u64> = BTreeMap::new();
        for d in self.matching(indexes, query, filters)? {
            let day = d.doc.day;
            total += 1;
            days.insert(day);
            *papers.entry(&d.doc.lccn).or_default() += 1;
            for l in &d.doc.language {
                *languages.entry(l).or_default() += 1;
            }
            series[spec.index_of_day(day)] += 1;
            let e = places.entry(&d.doc.place_id).or_insert((0, u32::MAX, 0));
            e.0 += 1;
            e.1 = e.1.min(day);
            e.2 = e.2.max(day);
            span = Some(span.map_or((day, day), |(lo, hi)| (lo.min(day), hi.max(day))));
        }
        Ok(Summary {
            total_hits: total,
            first_day: span.map(|s| s.0),
            last_day: span.map(|s| s.1),
            series,
            places: places
                .into_iter()
                .map(|(id, (hits, first_day, last_day))| PlaceSummary {
                    place_id: id.to_owned(),
                    hits,
                    first_day,
                    last_day,
                })
                .collect(),
            papers: counts(papers),
            languages: counts(languages),
            days: days.len() as u64,
        })
    }

    async fn cube(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
        shards: &[u8],
    ) -> Result<Vec<CubeCell>, SearchError> {
        Ok(cells(
            self.matching(indexes, query, filters)?
                .filter(|d| shards.contains(&d.doc.place_shard)),
            spec,
        ))
    }

    async fn place_cube(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        spec: &BucketSpec,
        places: &[String],
    ) -> Result<Vec<CubeCell>, SearchError> {
        Ok(cells(
            self.matching(indexes, query, filters)?
                .filter(|d| places.contains(&d.doc.place_id)),
            spec,
        ))
    }

    async fn hits(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
        page: &HitsQuery,
    ) -> Result<HitsPage, SearchError> {
        let analyzed = as_analyzed(query);
        let mut docs: Vec<(usize, &PageDoc)> = self
            .matching(indexes, query, filters)?
            .filter(|d| page.place_id.as_ref().is_none_or(|p| &d.doc.place_id == p))
            .filter(|d| page.lccn.as_ref().is_none_or(|l| &d.doc.lccn == l))
            .map(|d| {
                let mentions: usize = d
                    .texts(indexes.american_stories())
                    .into_iter()
                    .map(|t| mentions(d.reads(query, &analyzed), t))
                    .sum();
                (mentions, &d.doc)
            })
            .collect();
        docs.sort_by(|(ma, a), (mb, b)| {
            let order = (a.day, a.sort_key, &a.doc_id).cmp(&(b.day, b.sort_key, &b.doc_id));
            match page.sort {
                HitSort::Oldest => order,
                HitSort::Newest => order.reverse(),
                HitSort::Relevant => mb.cmp(ma).then(order),
            }
        });
        let days = page.days.then(|| {
            docs.iter()
                .map(|(_, d)| d.day)
                .collect::<std::collections::BTreeSet<_>>()
                .len() as u64
        });
        Ok(HitsPage {
            total: docs.len() as u64,
            days,
            hits: docs
                .into_iter()
                .skip(page.offset)
                .take(page.limit)
                .map(|(_, d)| {
                    let (snippets, snippet_source) = match &d.printed {
                        Some(printed) => (ja_snippets(printed, query, indexes.analyzer()), None),
                        None => page_snippets(
                            &d.text,
                            d.text_as.as_deref(),
                            query,
                            indexes.american_stories(),
                        ),
                    };
                    let matched_in = (indexes.american_stories() && d.printed.is_none())
                        .then(|| matched_in(&d.text, d.text_as.as_deref(), query));
                    Hit {
                        doc_id: d.doc_id.clone(),
                        day: d.day,
                        lccn: d.lccn.clone(),
                        place_id: d.place_id.clone(),
                        edition: d.edition,
                        seq: d.seq,
                        front_page: d.front_page,
                        snippets,
                        snippet_source,
                        matched_in,
                        ocr_source: d.ocr_source.clone(),
                        ocr_engine: d.ocr_engine.clone(),
                        congress: d.congress.clone(),
                    }
                })
                .collect(),
        })
    }

    async fn american_stories_only(
        &self,
        indexes: &IndexSet,
        query: &Node,
        filters: &Filters,
    ) -> Result<u64, SearchError> {
        let both = indexes.clone().with_american_stories(true);
        let analyzed = as_analyzed(query);
        let only = self
            .matching(&both, query, filters)?
            .filter(|d| {
                let q = d.reads(query, &analyzed);
                !eval(q, &d.tokens) && d.as_tokens.as_ref().is_some_and(|t| eval(q, t))
            })
            .count();
        Ok(only as u64)
    }

    async fn health(&self) -> Result<(), SearchError> {
        Ok(())
    }
}

/// Matching pages per place and bucket, by place then bucket.
fn cells<'a>(docs: impl Iterator<Item = &'a Indexed>, spec: &BucketSpec) -> Vec<CubeCell> {
    let mut cells: BTreeMap<(&str, u32), u32> = BTreeMap::new();
    for d in docs {
        *cells
            .entry((&d.doc.place_id, spec.index_of_day(d.doc.day) as u32))
            .or_default() += 1;
    }
    cells
        .into_iter()
        .map(|((place, bucket), hits)| CubeCell {
            place_id: place.to_owned(),
            bucket,
            hits,
        })
        .collect()
}

/// Evaluate the AST against a document's token stream.
pub fn eval(node: &Node, tokens: &[String]) -> bool {
    eval_texts(node, &[tokens])
}

/// Evaluate the AST against a document's texts (05 §5.5.4): a term or
/// phrase matches when it matches in any one of them (a phrase within one
/// text), and the operators combine those as usual, so `NOT x` excludes a
/// page with `x` in any text.
pub fn eval_texts(node: &Node, texts: &[&[String]]) -> bool {
    match node {
        Node::Term(t) => texts
            .iter()
            .any(|tokens| tokens.iter().any(|tok| term_matches(t, tok))),
        Node::Phrase { terms, slop } => texts
            .iter()
            .any(|tokens| phrase_matches(terms, *slop, tokens)),
        Node::And(c) => c.iter().all(|n| eval_texts(n, texts)),
        Node::Or(c) => c.iter().any(|n| eval_texts(n, texts)),
        Node::Not(n) => !eval_texts(n, texts),
    }
}

/// Whether a term matches one indexed word.
pub(crate) fn term_matches(t: &Term, token: &str) -> bool {
    if t.prefix {
        token.starts_with(&t.text)
    } else if t.wildcard {
        wildcard_matches(&t.text, token)
    } else if t.fuzzy > 0 {
        levenshtein_within(&t.text, token, usize::from(t.fuzzy))
    } else {
        word_matches(&t.text, token)
    }
}

/// Whether a query's word matches a page's word, as `usnm_text` has it. A
/// word with spaces in it is what `Analyzer::V1` folds one character into
/// (`ﷺ`, four Arabic words); the pairs of an index built with it hold that
/// character as those words run together, so it matches the character (#168).
pub(crate) fn word_matches(word: &str, token: &str) -> bool {
    if word.contains(char::is_whitespace) {
        fold(token, Analyzer::V1) == word
    } else {
        token == word
    }
}

/// Ordered phrase match where the total number of extra words between
/// consecutive terms is at most `slop`.
fn phrase_matches(terms: &[String], slop: u8, tokens: &[String]) -> bool {
    phrase_starts(terms, slop, tokens).next().is_some()
}

/// Where in `tokens` the phrase starts a match, within `slop` extra words.
fn phrase_starts<'a>(
    terms: &'a [String],
    slop: u8,
    tokens: &'a [String],
) -> impl Iterator<Item = usize> + 'a {
    fn from(terms: &[String], tokens: &[String], pos: usize, budget: usize) -> bool {
        let Some((first, rest)) = terms.split_first() else {
            return true;
        };
        let end = (pos + budget + 1).min(tokens.len());
        (pos..end).any(|i| {
            word_matches(first, &tokens[i]) && from(rest, tokens, i + 1, budget - (i - pos))
        })
    }
    tokens.iter().enumerate().filter_map(move |(i, t)| {
        let (first, rest) = terms.split_first()?;
        (word_matches(first, t) && from(rest, tokens, i + 1, usize::from(slop))).then_some(i)
    })
}

pub(crate) fn levenshtein_within(a: &str, b: &str, max: usize) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return false;
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()] <= max
}

/// A ~25-word window around the first highlighted word, HTML-escaped with `<mark>`.
fn counts(m: BTreeMap<&str, u64>) -> Vec<KeyCount> {
    rank(
        m.into_iter()
            .map(|(k, hits)| KeyCount {
                key: k.to_owned(),
                hits,
            })
            .collect(),
    )
}

/// How often the page mentions the query's positive parts: each word a term
/// matches, and each place an exact or NEAR phrase matches as a phrase (not
/// its words one by one, so the `of` in "cross of gold" counts only there).
/// The reference for "most mentions first" (`HitSort::Relevant`).
fn mentions(node: &Node, tokens: &[String]) -> usize {
    match node {
        Node::Term(t) => tokens.iter().filter(|tok| term_matches(t, tok)).count(),
        Node::Phrase { terms, slop } => phrase_starts(terms, *slop, tokens).count(),
        Node::And(c) | Node::Or(c) => c.iter().map(|n| mentions(n, tokens)).sum(),
        Node::Not(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_core::query::parse;

    fn toks(s: &str) -> Vec<String> {
        tokenize(s, USNM_TEXT)
    }

    #[test]
    fn evaluates_terms_phrases_and_operators() {
        let t = toks("You shall not crucify mankind upon a cross of gold said Bryan");
        assert!(eval(&parse(r#""cross of gold""#).unwrap(), &t));
        assert!(!eval(&parse(r#""gold of cross""#).unwrap(), &t));
        assert!(eval(&parse(r#""crucify gold"~5"#).unwrap(), &t));
        assert!(!eval(&parse(r#""crucify gold"~4"#).unwrap(), &t));
        assert!(eval(&parse("bryan -silver").unwrap(), &t));
        assert!(!eval(&parse("bryan -gold").unwrap(), &t));
        assert!(eval(&parse("silver OR mankind").unwrap(), &t));
        assert!(eval(&parse("cruci*").unwrap(), &t));
        assert!(eval(&parse("mankimd~1").unwrap(), &t));
        assert!(!eval(&parse("mankxxd~1").unwrap(), &t));
        // Wildcards match whole words (#124).
        assert!(eval(&parse("cruci?y").unwrap(), &t));
        assert!(eval(&parse("manki*d").unwrap(), &t));
        assert!(!eval(&parse("manki??d").unwrap(), &t));
        assert!(!eval(&parse("cross*ld").unwrap(), &t));
    }

    #[test]
    fn mentions_count_matching_words_and_whole_phrases() {
        let q = parse("gold cruci* -silver").unwrap();
        assert_eq!(
            mentions(&q, &toks("gold, gold and crucify; silver gold")),
            4
        );
        assert_eq!(mentions(&q, &toks("silver")), 0);
        // A phrase counts where it occurs, not each of its words.
        let phrase = parse(r#""cross of gold""#).unwrap();
        assert_eq!(
            mentions(
                &phrase,
                &toks("cross of gold, of this and of that, cross of gold")
            ),
            2
        );
        let near = parse(r#""crucify gold"~5"#).unwrap();
        assert_eq!(
            mentions(&near, &toks("crucify mankind upon a cross of gold; gold")),
            1
        );
    }
}

#[cfg(test)]
mod shard_tests {
    use super::*;
    use chrono::NaiveDate;
    use usnm_core::params::Filters;
    use usnm_core::time::{BucketSpec, BucketUnit};

    fn doc(i: u32, place: u8) -> PageDoc {
        PageDoc {
            printed: None,
            ocr_source: None,
            ocr_engine: None,
            text_as: None,
            congress: Default::default(),
            doc_id: format!("sn99{i:06}_1896-07-10_ed-1_seq-1"),
            day: 71_000 + i,
            ym: 1896 * 12 + 6,
            year: 1896,
            place_id: format!("P{place:05}"),
            place_shard: place % 8,
            lccn: format!("sn99{place:06}"),
            state: "GA".into(),
            language: vec!["eng".into()],
            front_page: true,
            edition: 1,
            seq: 1,
            sort_key: u64::from(place) << 32 | 1 << 16 | 1,
            batch: "b".into(),
            text: "a cross of gold".into(),
        }
    }

    #[tokio::test]
    async fn sharded_cube_equals_unsharded_cube() {
        let mut b = MemoryBackend::new();
        b.add_index("i", (0..200).map(|i| doc(i, (i % 13) as u8)));
        let idx = IndexSet::new(vec!["i".into()]);
        let q = usnm_core::query::parse("gold").unwrap();
        let from = usnm_core::time::date_from_day(71_000);
        let to = usnm_core::time::date_from_day(71_300);
        let f = Filters {
            from,
            to,
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let spec = BucketSpec::new(BucketUnit::Week, from, to);
        let all = b
            .cube(&idx, &q, &f, &spec, &(0..8).collect::<Vec<_>>())
            .await
            .unwrap();
        for n in 1..=8u8 {
            let mut merged = Vec::new();
            for k in 0..n {
                merged.extend(
                    b.cube(&idx, &q, &f, &spec, &usnm_core::cube::shards_for(k, n))
                        .await
                        .unwrap(),
                );
            }
            merged.sort_by(|a, b| (&a.place_id, a.bucket).cmp(&(&b.place_id, b.bucket)));
            assert_eq!(merged, all, "n = {n}");
        }
        let _ = NaiveDate::MIN;
    }

    /// A page matches when the query matches in either text, once (05 §5.5.4),
    /// and only when the index set searches American Stories' text.
    #[tokio::test]
    async fn american_stories_text_matches_a_page_once_when_searched() {
        let mut both = doc(1, 1);
        both.text = "a cross of goid".into();
        both.text_as = Some("BRYAN SPEAKS a cross of gold".into());
        let mut b = MemoryBackend::new();
        b.add_index("i", [both, doc(2, 1)]);
        let off = IndexSet::new(vec!["i".into()]);
        let on = off.clone().with_american_stories(true);
        let (from, to) = (
            usnm_core::time::date_from_day(71_000),
            usnm_core::time::date_from_day(71_300),
        );
        let f = Filters {
            from,
            to,
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let spec = BucketSpec::new(BucketUnit::Year, from, to);
        let total = |set: &IndexSet, q: &str| {
            let (b, f, spec, set) = (&b, &f, &spec, set.clone());
            let q = usnm_core::query::parse(q).unwrap();
            async move { b.summary(&set, &q, f, spec).await.unwrap().total_hits }
        };
        // Doc 1 has the word in American Stories' text only; doc 2 in LoC's.
        assert_eq!(total(&off, "gold").await, 1);
        assert_eq!(total(&on, "gold").await, 2);
        assert_eq!(total(&on, r#""cross of gold""#).await, 2);
        assert_eq!(total(&on, "bryan").await, 1);
        assert_eq!(total(&off, "bryan").await, 0);
        // A term in each text, both in one page: still one page.
        assert_eq!(total(&on, "bryan goid").await, 1);
        // A phrase can't span the two texts.
        assert_eq!(total(&on, r#""speaks a cross of goid""#).await, 0);
        // NOT excludes a page with the word in either text.
        assert_eq!(total(&on, "cross -bryan").await, 1);
        assert_eq!(total(&off, "cross -bryan").await, 2);
        let page = HitsQuery {
            limit: 10,
            ..HitsQuery::default()
        };
        let q = usnm_core::query::parse("bryan").unwrap();
        let hits = b.hits(&on, &q, &f, &page).await.unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(
            hits.hits[0].snippets,
            ["<mark>BRYAN</mark> SPEAKS a cross of gold"]
        );
        assert_eq!(
            hits.hits[0].snippet_source,
            Some(crate::SNIPPETS_FROM_AMERICAN_STORIES)
        );
        assert_eq!(hits.hits[0].matched_in, Some(vec!["american_stories"]));
        let q = usnm_core::query::parse("cross").unwrap();
        let hits = b.hits(&on, &q, &f, &page).await.unwrap();
        assert!(hits.hits.iter().all(|h| h.snippet_source.is_none()));
        // Doc 1 has `cross` in both texts, doc 2 (no American Stories text) in LoC's.
        let matched: Vec<_> = hits.hits.iter().map(|h| h.matched_in.clone()).collect();
        assert_eq!(
            matched,
            [Some(vec!["loc", "american_stories"]), Some(vec!["loc"])]
        );
        // Not when the search covers LoC's text alone.
        let hits = b.hits(&off, &q, &f, &page).await.unwrap();
        assert!(hits.hits.iter().all(|h| h.matched_in.is_none()));
        // Doc 1 is found only through American Stories' text.
        let gold = usnm_core::query::parse("gold").unwrap();
        assert_eq!(b.american_stories_only(&on, &gold, &f).await.unwrap(), 1);
        assert_eq!(b.american_stories_only(&on, &q, &f).await.unwrap(), 0);
    }

    /// Pages are read as Quickwit's `usnm_text` reads them whatever the
    /// analyzer version, and query terms analyzed again as it does (#168):
    /// `½` is the word `½` with version 2, and with version 1 it is `1⁄2`,
    /// which the engine reads as the phrase "1 2", so it finds the page that
    /// prints `1/2`.
    #[tokio::test]
    async fn queries_are_read_as_the_engine_reads_them() {
        use usnm_core::text::Analyzer;
        let mut half = doc(1, 1);
        half.text = "Wheat closed ½ higher".into();
        let mut slash = doc(2, 1);
        slash.text = "Oats closed 1/2 lower".into();
        let mut b = MemoryBackend::new();
        b.add_index("i", [half, slash]);
        let (from, to) = (
            usnm_core::time::date_from_day(71_000),
            usnm_core::time::date_from_day(71_300),
        );
        let f = Filters {
            from,
            to,
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let page = HitsQuery {
            limit: 10,
            ..HitsQuery::default()
        };
        let ids = |a: Analyzer, q: &str| {
            let (b, f, page) = (&b, &f, &page);
            let set = IndexSet::new(vec!["i".into()]).with_analyzer(a);
            let q = usnm_core::query::parse_with(q, a).unwrap();
            async move {
                let hits = b.hits(&set, &q, f, page).await.unwrap();
                hits.hits.into_iter().map(|h| h.doc_id).collect::<Vec<_>>()
            }
        };
        let (half, slash) = (doc(1, 1).doc_id, doc(2, 1).doc_id);
        assert_eq!(ids(Analyzer::V2, "½").await, std::slice::from_ref(&half));
        assert_eq!(ids(Analyzer::V1, "½").await, std::slice::from_ref(&slash));
        assert_eq!(
            ids(Analyzer::V1, r#""closed ½ lower""#).await,
            std::slice::from_ref(&slash)
        );
        for a in Analyzer::ALL {
            assert_eq!(ids(a, "1/2").await, std::slice::from_ref(&slash), "{a:?}");
            assert_eq!(ids(a, "wheat").await, std::slice::from_ref(&half), "{a:?}");
        }
        // Version 2 marks `½` in the snippet.
        let set = IndexSet::new(vec!["i".into()]);
        let q = usnm_core::query::parse("½").unwrap();
        let hits = b.hits(&set, &q, &f, &page).await.unwrap();
        assert_eq!(
            hits.hits[0].snippets,
            ["Wheat closed <mark>½</mark> higher"]
        );
    }

    #[tokio::test]
    async fn characters_that_fold_to_japanese_are_read_as_the_engine_reads_them() {
        // Quickwit 0.9.1's `usnm_text` keeps `㊀` as it is, and `a㊀` as one
        // token: the term `a㊀` finds only the page with `a㊀`, `a一` only
        // the one with `a一`, `㊀` only the one with `㊀` (#241).
        use usnm_core::text::Analyzer;
        let texts = [
            "Lot ㊀ sold",
            "Item a㊀ held",
            "Item a一 held",
            "Plain 一 here",
        ];
        let docs: Vec<PageDoc> = (1..)
            .zip(texts)
            .map(|(i, t)| PageDoc {
                text: t.into(),
                ..doc(i, 1)
            })
            .collect();
        let id = |i: u32| doc(i, 1).doc_id;
        let mut b = MemoryBackend::new();
        b.add_index("i", docs);
        let f = Filters {
            from: usnm_core::time::date_from_day(71_000),
            to: usnm_core::time::date_from_day(71_300),
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let page = HitsQuery {
            limit: 10,
            ..HitsQuery::default()
        };
        let hits = |a: Analyzer, q: &str| {
            let (b, f, page) = (&b, &f, &page);
            let set = IndexSet::new(vec!["i".into()]).with_analyzer(a);
            let q = usnm_core::query::parse_with(q, a).unwrap();
            async move { b.hits(&set, &q, f, page).await.unwrap().hits }
        };
        let ids = |h: Vec<crate::Hit>| h.into_iter().map(|h| h.doc_id).collect::<Vec<_>>();
        // Version 2 keeps them, as the index has them.
        assert_eq!(ids(hits(Analyzer::V2, "㊀").await), [id(1)]);
        let a = hits(Analyzer::V2, "a㊀").await;
        assert_eq!(a[0].snippets, ["Item <mark>a㊀</mark> held"]);
        assert_eq!(ids(a), [id(2)]);
        // Version 1 folds them to Japanese characters, which the engine
        // finds only where they are printed: `a㊀` is the term `a一`.
        assert_eq!(ids(hits(Analyzer::V1, "a㊀").await), [id(3)]);
        assert_eq!(ids(hits(Analyzer::V1, "㊀").await), [id(4)]);
    }

    #[test]
    fn a_version_1_word_with_spaces_matches_its_character() {
        // `ﷺ` folds to four words with `Analyzer::V1`; the pairs of an index
        // built with it hold the character, so a phrase finds it (#168).
        let q = usnm_core::query::parse_with("\"of \u{fdfa} gold\"", Analyzer::V1).unwrap();
        let q = as_analyzed(&q);
        assert!(eval(&q, &tokenize("cross of \u{fdfa} gold", USNM_TEXT)));
        assert!(!eval(&q, &tokenize("cross of gold", USNM_TEXT)));
    }

    #[test]
    fn analyzing_again_changes_nothing_for_the_latest_analyzer() {
        for q in [
            r#""cross of gold" -bryan (silver OR freed*) "gold silver"~3"#,
            r#"½ "wheat 61¼" Æsop ﷺ presi?ent 東京"#,
            r#"㊀ "lot a㊀" abcd〸* 一"#,
        ] {
            let n = usnm_core::query::parse(q).unwrap();
            assert_eq!(as_analyzed(&n), n, "{q}");
        }
    }

    #[tokio::test]
    async fn hidden_copies_are_not_counted_or_listed() {
        let mut copy = doc(1, 1);
        copy.batch = "other".into();
        let mut b = MemoryBackend::new();
        b.add_index("base", [doc(1, 1), doc(2, 1)]);
        b.add_index("delta", [copy]);
        let both = IndexSet::new(vec!["base".into(), "delta".into()]);
        let hidden = both.clone().hiding([(doc(1, 1).doc_id, "b".to_owned())]);
        let q = usnm_core::query::parse("gold").unwrap();
        let (from, to) = (
            usnm_core::time::date_from_day(71_000),
            usnm_core::time::date_from_day(71_300),
        );
        let f = Filters {
            from,
            to,
            states: vec![],
            lccns: vec![],
            langs: vec![],
            front_only: false,
            series: Vec::new(),
        };
        let spec = BucketSpec::new(BucketUnit::Year, from, to);
        let total = |set: IndexSet| {
            let (b, q, f, spec) = (&b, &q, &f, &spec);
            async move { b.summary(&set, q, f, spec).await.unwrap().total_hits }
        };
        assert_eq!(total(both.clone()).await, 3);
        assert_eq!(total(hidden.clone()).await, 2);
        let page = HitsQuery {
            limit: 10,
            ..HitsQuery::default()
        };
        let hits = b.hits(&hidden, &q, &f, &page).await.unwrap();
        assert_eq!(hits.total, 2);
        assert_eq!(
            hits.hits
                .iter()
                .filter(|h| h.doc_id == doc(1, 1).doc_id)
                .count(),
            1
        );
    }
}
