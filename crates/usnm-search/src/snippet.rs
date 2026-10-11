//! Snippets of a page's stored text (#126), the same for every backend:
//! up to [`MAX_FRAGMENTS`] fragments around the query's matches, HTML-escaped
//! with only `<mark>` as markup. Built in the API from the text Quickwit
//! already returns with each hit, so its single short fragment per field
//! (which its REST API can't lengthen) no longer decides what readers see.

use usnm_core::query::{Node, Term};
use usnm_core::text::{fold, index_tokens, REMOVE_LONG_BYTES, USNM_TEXT};

use crate::memory::{as_analyzed, eval, term_matches, word_matches};
use crate::{mark_html, MATCHED_IN_LOC, SNIPPETS_FROM_AMERICAN_STORIES};

/// Fragments per page.
pub const MAX_FRAGMENTS: usize = 3;
/// Characters of context on each side of a match, about a line of print.
pub const CONTEXT: usize = 80;

/// One word as the analyzer counts positions: its folded token, or `None`
/// for an over-long OCR run the analyzer drops but still counts as a
/// position (255 bytes or more, as `text` and `text_as` index them,
/// [`index_tokens`], #286), and its character range in the text.
struct Word {
    token: Option<String>,
    start: usize,
    end: usize,
}

/// The text's words as the `usnm_text` analyzer splits and folds them, with
/// positions.
fn words(chars: &[char]) -> Vec<Word> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_alphanumeric() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        let raw: String = chars[start..i].iter().collect();
        // A dropped run keeps its position, so a phrase can't match across it.
        out.push(Word {
            token: (raw.len() < REMOVE_LONG_BYTES).then(|| fold(&raw, USNM_TEXT)),
            start,
            end: i,
        });
    }
    out
}

/// What to mark: a word matching a term, or an exact phrase as a whole.
enum Pattern {
    Term(Term),
    Phrase(Vec<String>),
}

fn patterns(query: &Node) -> Vec<Pattern> {
    let mut out = Vec::new();
    let mut stack = vec![query];
    while let Some(node) = stack.pop() {
        match node {
            Node::Term(t) => out.push(Pattern::Term(t.clone())),
            Node::Phrase { terms, slop: 0 } => out.push(Pattern::Phrase(terms.clone())),
            // A NEAR phrase's words can be far apart: mark them one by one.
            Node::Phrase { terms, .. } => out.extend(terms.iter().map(|t| {
                Pattern::Term(Term {
                    text: t.clone(),
                    fuzzy: 0,
                    prefix: false,
                    wildcard: false,
                })
            })),
            Node::And(c) | Node::Or(c) => stack.extend(c),
            Node::Not(_) => {}
        }
    }
    out
}

/// Character ranges of the matches, sorted and merged.
fn matches(words: &[Word], patterns: &[Pattern]) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    for p in patterns {
        match p {
            Pattern::Term(t) => hits.extend(
                words
                    .iter()
                    .filter(|w| w.token.as_deref().is_some_and(|tok| term_matches(t, tok)))
                    .map(|w| (w.start, w.end)),
            ),
            Pattern::Phrase(terms) if !terms.is_empty() => {
                for run in words.windows(terms.len()) {
                    if run
                        .iter()
                        .zip(terms)
                        .all(|(w, t)| w.token.as_deref().is_some_and(|tok| word_matches(t, tok)))
                    {
                        hits.push((run[0].start, run[terms.len() - 1].end));
                    }
                }
            }
            Pattern::Phrase(_) => {}
        }
    }
    hits.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in hits {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// Move `at` back to the start of the word it falls in.
fn word_start(chars: &[char], mut at: usize) -> usize {
    while at > 0
        && at < chars.len()
        && chars[at - 1].is_alphanumeric()
        && chars[at].is_alphanumeric()
    {
        at -= 1;
    }
    at
}

/// Move `at` forward to the end of the word it falls in.
fn word_end(chars: &[char], mut at: usize) -> usize {
    while at > 0
        && at < chars.len()
        && chars[at - 1].is_alphanumeric()
        && chars[at].is_alphanumeric()
    {
        at += 1;
    }
    at
}

/// Up to [`MAX_FRAGMENTS`] fragments of `text` around the matches of the
/// query's positive words and phrases: exact phrases marked as a whole, a
/// NEAR phrase's words one by one, prefix, wildcard and fuzzy terms on each
/// word they match. Matches closer together than the context share a
/// fragment, and fragments never overlap. Line breaks (columns) become spaces; a fragment
/// that doesn't reach the text's start or end is marked with `…`. The words
/// are matched as the engine matches them: the text's as `usnm_text` reads
/// them, the query's analyzed again ([`as_analyzed`]), so with any analyzer
/// version the marks land on the words the engine found (#168).
pub fn text_snippets(text: &str, query: &Node) -> Vec<String> {
    let chars: Vec<char> = text
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let hits = matches(&words(&chars), &patterns(&as_analyzed(query)));
    let mut out = Vec::new();
    let mut covered = 0;
    let mut k = 0;
    while k < hits.len() && out.len() < MAX_FRAGMENTS {
        let (s, _) = hits[k];
        let from = word_start(&chars, s.saturating_sub(CONTEXT).max(covered)).max(covered);
        let mut pieces: Vec<(bool, String)> = Vec::new();
        if from > 0 {
            pieces.push((false, "… ".to_owned()));
        }
        let mut at = from;
        // Take every match that starts within the context of the last one.
        while k < hits.len() && hits[k].0 <= at.max(s) + CONTEXT {
            let (hs, he) = hits[k];
            if hs > at {
                pieces.push((false, chars[at..hs].iter().collect()));
            }
            pieces.push((true, chars[hs..he].iter().collect()));
            at = he;
            k += 1;
        }
        let to = word_end(&chars, (at + CONTEXT).min(chars.len()));
        if to > at {
            pieces.push((false, chars[at..to].iter().collect()));
        }
        if to < chars.len() {
            pieces.push((false, " …".to_owned()));
        }
        covered = to;
        // Collapse the spaces OCR leaves between columns.
        let pieces: Vec<(bool, String)> = pieces
            .into_iter()
            .map(|(m, s)| (m, collapse_spaces(&s)))
            .collect();
        let borrowed: Vec<(bool, &str)> = pieces.iter().map(|(m, s)| (*m, s.as_str())).collect();
        out.push(mark_html(&borrowed));
    }
    out
}

/// A page's snippets and, when they aren't from LoC's text, where they come
/// from (05 §5.5.4): LoC's `text` first; when the query marks nothing there
/// and the search covers American Stories' text (`american_stories`), that
/// text's, from [`SNIPPETS_FROM_AMERICAN_STORIES`]. Both backends use it.
pub fn page_snippets(
    text: &str,
    text_as: Option<&str>,
    query: &Node,
    american_stories: bool,
) -> (Vec<String>, Option<&'static str>) {
    let loc = text_snippets(text, query);
    match text_as {
        Some(text_as) if loc.is_empty() && american_stories => {
            let other = text_snippets(text_as, query);
            if other.is_empty() {
                (loc, None)
            } else {
                (other, Some(SNIPPETS_FROM_AMERICAN_STORIES))
            }
        }
        _ => (loc, None),
    }
}

/// Which of a page's texts the query matches (05 §5.5.4), from the texts
/// the hit already carries for its snippets: [`MATCHED_IN_LOC`] when LoC's
/// `text` matches the whole query on its own, [`SNIPPETS_FROM_AMERICAN_STORIES`]
/// when American Stories' text does. A page found only by taking a word
/// from each text (`a b` with `a` in one and `b` in the other) lists both.
/// Evaluated as the memory backend evaluates a query, so a page listed
/// without `loc` is one a search of LoC's text alone doesn't find: the
/// pages `/v1/aggregate` counts in `total.american_stories_only`. Both
/// backends use it. The texts are read as `text` and `text_as` index them
/// ([`index_tokens`]), where that count is made.
pub fn matched_in(text: &str, text_as: Option<&str>, query: &Node) -> Vec<&'static str> {
    let Some(text_as) = text_as else {
        return vec![MATCHED_IN_LOC];
    };
    let query = as_analyzed(query);
    let loc = eval(&query, &index_tokens(text, USNM_TEXT));
    let american = eval(&query, &index_tokens(text_as, USNM_TEXT));
    match (loc, american) {
        (true, false) => vec![MATCHED_IN_LOC],
        (false, true) => vec![SNIPPETS_FROM_AMERICAN_STORIES],
        _ => vec![MATCHED_IN_LOC, SNIPPETS_FROM_AMERICAN_STORIES],
    }
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for c in s.chars() {
        if c == ' ' {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_core::query::parse;

    fn snip(text: &str, q: &str) -> Vec<String> {
        text_snippets(text, &parse(q).unwrap())
    }

    #[test]
    fn marks_words_and_escapes_everything_else() {
        assert_eq!(
            snip("a <b> cross of Gold, & more", "gold"),
            ["a &lt;b&gt; cross of <mark>Gold</mark>, &amp; more"]
        );
    }

    #[test]
    fn marks_an_exact_phrase_as_a_whole_and_not_its_words_alone() {
        assert_eq!(
            snip(
                "gold here, and a cross\nof  gold there",
                "\"cross of gold\""
            ),
            ["gold here, and a <mark>cross of gold</mark> there"]
        );
    }

    #[test]
    fn marks_near_words_prefixes_and_fuzzy_matches_but_not_excluded_words() {
        assert_eq!(
            snip(
                "Bryan spoke; crucify mankind upon silver",
                "\"bryan mankind\"~5 cruci* mankimd~1 -silver"
            ),
            ["<mark>Bryan</mark> spoke; <mark>crucify</mark> <mark>mankind</mark> upon silver"]
        );
    }

    #[test]
    fn marks_each_word_a_wildcard_matches() {
        assert_eq!(
            snip(
                "The President and the Presidency, the Presidents; presi",
                "presi?ent*"
            ),
            ["The <mark>President</mark> and the Presidency, the <mark>Presidents</mark>; presi"]
        );
        assert_eq!(
            snip("Washington, Washinton, Washi. ton", "washi*ton"),
            ["<mark>Washington</mark>, <mark>Washinton</mark>, Washi. ton"]
        );
    }

    #[test]
    fn marks_what_the_engine_finds_for_a_version_1_query() {
        // With analyzer version 1, `½` is `1⁄2`, which the engine reads as
        // the phrase "1 2" (#168): the mark lands on `1/2`.
        let q = usnm_core::query::parse_with("½", usnm_core::text::Analyzer::V1).unwrap();
        assert_eq!(
            text_snippets("Oats closed 1/2 lower", &q),
            ["Oats closed <mark>1/2</mark> lower"]
        );
        assert!(text_snippets("Wheat closed ½ higher", &q).is_empty());
    }

    #[test]
    fn folds_like_the_index() {
        assert_eq!(
            snip("the Ñoño café", "nono cafe"),
            ["the <mark>Ñoño</mark> <mark>café</mark>"]
        );
    }

    #[test]
    fn gives_up_to_three_fragments_with_ellipses_on_word_boundaries() {
        let filler = |n: usize| vec!["word"; n].join(" ");
        let text = [
            filler(40),
            "gold".into(),
            filler(60),
            "gold".into(),
            filler(60),
            "gold".into(),
            filler(60),
            "gold".into(),
            filler(10),
        ]
        .join(" ");
        let s = snip(&text, "gold");
        assert_eq!(s.len(), MAX_FRAGMENTS);
        for f in &s {
            assert_eq!(f.matches("<mark>gold</mark>").count(), 1, "{f}");
            assert!(f.starts_with("… word") && f.ends_with("word …"), "{f}");
        }
    }

    #[test]
    fn a_dropped_ocr_run_breaks_a_phrase() {
        // A run of 41 to 254 bytes is a word of `text`, and one of 255 or
        // more is dropped; both keep their positions (#286).
        for run in ["x".repeat(41), "x".repeat(254), "x".repeat(255)] {
            assert!(snip(&format!("cross {run} of gold"), "\"cross of gold\"").is_empty());
            assert_eq!(
                snip(&format!("{run} gold"), "gold"),
                [format!("{run} <mark>gold</mark>")]
            );
        }
        // A prefix reaches the first, as on `text`.
        let (kept, dropped) = ("y".repeat(254), "z".repeat(255));
        assert_eq!(
            snip(&format!("{kept} {dropped}"), "yyyyy* zzzzz*"),
            [format!("<mark>{kept}</mark> {dropped}")]
        );
    }

    #[test]
    fn nearby_matches_share_a_fragment() {
        let s = snip("gold and silver and gold again", "gold silver");
        assert_eq!(
            s,
            ["<mark>gold</mark> and <mark>silver</mark> and <mark>gold</mark> again"]
        );
    }

    #[test]
    fn no_match_no_fragment() {
        assert!(snip("nothing here", "gold").is_empty());
        assert!(snip("gold", "silver -gold").is_empty());
    }

    #[test]
    fn american_stories_text_only_when_loc_text_marks_nothing() {
        let q = parse("bryan").unwrap();
        let loc = "the boy orator spoke";
        let american = "BRYAN AT CHICAGO the boy orator spoke";
        // Off: LoC's text only.
        assert_eq!(
            page_snippets(loc, Some(american), &q, false),
            (Vec::new(), None)
        );
        assert_eq!(
            page_snippets(loc, Some(american), &q, true),
            (
                vec!["<mark>BRYAN</mark> AT CHICAGO the boy orator spoke".to_owned()],
                Some(SNIPPETS_FROM_AMERICAN_STORIES)
            )
        );
        // A match in LoC's text keeps its snippets.
        let q = parse("orator").unwrap();
        assert_eq!(
            page_snippets(loc, Some(american), &q, true),
            (vec!["the boy <mark>orator</mark> spoke".to_owned()], None)
        );
        // Nothing in either text, or no second text.
        let q = parse("silver").unwrap();
        assert_eq!(
            page_snippets(loc, Some(american), &q, true),
            (Vec::new(), None)
        );
        assert_eq!(page_snippets(loc, None, &q, true), (Vec::new(), None));
    }

    #[test]
    fn matched_in_names_the_texts_that_match_the_whole_query() {
        let m = |q: &str, loc: &str, american: Option<&str>| {
            matched_in(loc, american, &parse(q).unwrap())
        };
        let loc = "a cross of goid, said the boy orator";
        let american = "BRYAN AT CHICAGO a cross of gold, said the boy oratar";
        assert_eq!(m("cross", loc, Some(american)), ["loc", "american_stories"]);
        assert_eq!(m("orator", loc, Some(american)), ["loc"]);
        assert_eq!(m("gold", loc, Some(american)), ["american_stories"]);
        assert_eq!(
            m(r#""cross of gold""#, loc, Some(american)),
            ["american_stories"]
        );
        // Every word of the query in one text: `boy` is in both, `gold` in one.
        assert_eq!(m("boy gold", loc, Some(american)), ["american_stories"]);
        // A word from each text: neither matches alone, so both are listed.
        assert_eq!(
            m("bryan orator", loc, Some(american)),
            ["loc", "american_stories"]
        );
        // An empty LoC text (a page only American Stories has text for).
        assert_eq!(m("gold", "", Some(american)), ["american_stories"]);
        // No American Stories text: LoC's.
        assert_eq!(m("orator", loc, None), ["loc"]);
    }
}
