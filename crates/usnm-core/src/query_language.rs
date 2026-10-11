//! The language a query's words are in, for its relative rate's baseline
//! (#237, 06 §6.3.3, 11 §11.2).
//!
//! An English word rarely matches a page printed in Yiddish, French or
//! Croatian, so dividing an English search's matches by every page makes
//! places with other-language papers read low. Without a `lang` filter the
//! API divides by the pages of titles in the query's language instead, when
//! it can tell what that is:
//!
//! - **Japanese**: a query with a Japanese word that isn't excluded
//!   ([`crate::query::is_japanese`]) searches only the Japanese pages (#139).
//! - **English**: each alternative the query matches (each side of an `OR`)
//!   has an English word ([`ENGLISH_WORDS`]), and its other words are
//!   English, shared ([`SHARED_WORDS`]) or digits. A phrase or an `AND`
//!   matches only pages with all its words, so one English word in it is
//!   enough when the rest are words English uses too: "John Brown"'s pages
//!   must have "brown" ("john" is shared). Excluded words (`-`, `NOT`) don't
//!   count. A prefix (`influ*`) or wildcard (`presi?ent`) term leaves the
//!   language unknown, since it matches words the lists can't vouch for; a
//!   fuzzy term (`railroad~1`) or OCR term (`railroad~ocr`, its OCR
//!   misspellings) is judged by its word. `2 OF (a b c)` is read as
//!   `a OR b OR c`: English only when each of them is.
//! - Otherwise the language is unknown and the baseline stays every page.
//!
//! The English words are at least as frequent in English as in each other
//! Latin-script language of the corpus, by wordfreq, folded as the index
//! folds a page; the shared words are English words another such language
//! uses up to 10 times as often (`scripts/build-english-words.py` builds
//! both). So a search the rule calls English finds mostly English pages,
//! and a word German, Spanish, French or Italian pages use as much
//! ("november", "influenza") leaves the baseline at every page. The rule
//! reads only the query, so the same request always gets the same baseline
//! and cached responses stay correct; a change to the lists bumps
//! `RESPONSE_FORMAT` in the API.

use std::sync::OnceLock;

use crate::query::{is_japanese, Node};

/// Folded words that make a query English, sorted, one per line.
pub const ENGLISH_WORDS: &str = include_str!("../data/english-words.txt");

/// Folded English words that other languages use as much, up to 10 times as
/// often: allowed beside an English word, never enough alone.
pub const SHARED_WORDS: &str = include_str!("../data/english-shared-words.txt");

/// Why the baseline counts the pages it does (06 §6.3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineWhy {
    /// The request's `lang` filter.
    Filter,
    /// The query's language (Japanese, or English by the word list).
    QueryLanguage,
    /// No filter and no language to go by: every page.
    All,
}

impl BaselineWhy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filter => "filter",
            Self::QueryLanguage => "query_language",
            Self::All => "all",
        }
    }
}

fn lines(text: &'static str) -> Vec<&'static str> {
    text.lines().filter(|l| !l.is_empty()).collect()
}

fn english_words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| lines(ENGLISH_WORDS))
}

fn shared_words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| lines(SHARED_WORDS))
}

/// What a word, or a part of the query, says about the query's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reading {
    /// Matches mostly English pages.
    English,
    /// Neither English nor foreign on its own: a shared word, digits, an
    /// excluded part, or a mix of these.
    Neutral,
    /// Can't be read as English: another language's word, a word the lists
    /// don't know, a prefix or a wildcard.
    Other,
}

/// How the lists read one folded word.
fn word(w: &str) -> Reading {
    if w.bytes().all(|b| b.is_ascii_digit()) || shared_words().binary_search(&w).is_ok() {
        Reading::Neutral
    } else if english_words().binary_search(&w).is_ok() {
        Reading::English
    } else {
        Reading::Other
    }
}

/// Every part must hold (a phrase or `AND`): any `Other` part makes it
/// `Other`, else one `English` part makes it `English`.
fn all_of(parts: impl Iterator<Item = Reading>) -> Reading {
    let mut out = Reading::Neutral;
    for r in parts {
        match r {
            Reading::Other => return Reading::Other,
            Reading::English => out = Reading::English,
            Reading::Neutral => {}
        }
    }
    out
}

/// Any part may hold (`OR`): `English` only when every part is.
fn any_of(parts: impl Iterator<Item = Reading>) -> Reading {
    let mut out = Reading::English;
    for r in parts {
        match r {
            Reading::Other => return Reading::Other,
            Reading::Neutral => out = Reading::Neutral,
            Reading::English => {}
        }
    }
    out
}

fn read(node: &Node) -> Reading {
    match node {
        Node::Term(t) if t.prefix || t.wildcard => Reading::Other,
        Node::Term(t) => word(&t.text),
        Node::Phrase { terms, .. } => all_of(terms.iter().map(|t| word(t))),
        Node::And(c) => all_of(c.iter().map(read)),
        Node::Or(c) => any_of(c.iter().map(read)),
        // Any `min` of its parts may hold: read as an `OR` of them.
        Node::AtLeast { of, .. } => any_of(of.iter().map(read)),
        Node::Not(_) => Reading::Neutral,
    }
}

/// The catalog language code of the query's words: `jpn` for a Japanese
/// query, `eng` for one the word lists read as English, `None` when the
/// rule can't tell (see the module docs).
pub fn query_language(node: &Node) -> Option<&'static str> {
    if is_japanese(node) {
        return Some("jpn");
    }
    (read(node) == Reading::English).then_some("eng")
}

/// The title languages a search's baseline counts and why: its `lang`
/// filter when it has one, else [`query_language`] when that is known,
/// else every page (no languages).
pub fn baseline_languages(node: &Node, filter: &[String]) -> (Vec<String>, BaselineWhy) {
    if !filter.is_empty() {
        return (filter.to_vec(), BaselineWhy::Filter);
    }
    match query_language(node) {
        Some(lang) => (vec![lang.to_owned()], BaselineWhy::QueryLanguage),
        None => (Vec::new(), BaselineWhy::All),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse;

    fn lang(q: &str) -> Option<&'static str> {
        query_language(&parse(q).unwrap())
    }

    #[test]
    fn the_lists_are_sorted_folded_unique_and_apart() {
        for words in [english_words(), shared_words()] {
            assert!(words.len() > 10_000, "{}", words.len());
            assert!(words.windows(2).all(|w| w[0] < w[1]));
            assert!(words
                .iter()
                .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase())));
        }
        assert!(shared_words()
            .iter()
            .all(|w| english_words().binary_search(w).is_err()));
    }

    #[test]
    fn english_words_make_an_english_query() {
        for q in [
            "railroad",
            "cotton",
            "korea",
            "\"cross of gold\"",
            "railroad AND strike",
            "baseball OR football",
            "railroad~1",
            "\"yellow fever\" 1878",
        ] {
            assert_eq!(lang(q), Some("eng"), "{q}");
        }
    }

    #[test]
    fn shared_words_need_an_english_word_beside_them() {
        // "civil", "november" and "john" are as common in Spanish, German
        // or Hungarian text: alone they leave the baseline at every page,
        // next to an English word in a phrase or AND they don't.
        assert_eq!(lang("civil"), None);
        assert_eq!(lang("november"), None);
        assert_eq!(lang("\"civil rights\""), Some("eng"));
        assert_eq!(lang("\"John Brown\""), Some("eng"));
        assert_eq!(lang("john AND railroad"), Some("eng"));
        // Each side of an OR must be English.
        assert_eq!(lang("john OR railroad"), None);
        assert_eq!(lang("railroad OR 1896"), None);
    }

    #[test]
    fn other_languages_words_leave_it_unknown() {
        for q in [
            "influenza",
            "\"guerra civil\"",
            "\"der krieg\"",
            "krieg AND in",
            "railroad AND eisenbahn",
            "korea OR corea",
            "zeleznice",
            "1896",
        ] {
            assert_eq!(lang(q), None, "{q}");
        }
    }

    #[test]
    fn prefixes_and_wildcards_leave_it_unknown() {
        assert_eq!(lang("railr*"), None);
        assert_eq!(lang("presi?ent"), None);
        assert_eq!(lang("railroad AND cotto*"), None);
    }

    #[test]
    fn ocr_terms_by_their_word_and_at_least_n_as_an_or() {
        assert_eq!(lang("railroad~ocr"), Some("eng"));
        assert_eq!(lang("influenza~ocr"), None);
        assert_eq!(lang("2 OF (railroad cotton strike)"), Some("eng"));
        assert_eq!(lang("2 OF (railroad cotton john)"), None);
        assert_eq!(lang("2 OF (railroad cotton eisenbahn)"), None);
    }

    #[test]
    fn excluded_words_are_ignored() {
        assert_eq!(lang("railroad -eisenbahn"), Some("eng"));
        assert_eq!(lang("civil -railroad"), None);
    }

    #[test]
    fn other_scripts_are_unknown_and_japanese_is_japanese() {
        assert_eq!(lang("קארעא"), None);
        assert_eq!(lang("Корея"), None);
        assert_eq!(lang("戦争"), Some("jpn"));
    }

    #[test]
    fn a_filter_wins_then_the_query_language_then_every_page() {
        let q = parse("railroad").unwrap();
        assert_eq!(
            baseline_languages(&q, &["ger".to_owned()]),
            (vec!["ger".to_owned()], BaselineWhy::Filter)
        );
        assert_eq!(
            baseline_languages(&q, &[]),
            (vec!["eng".to_owned()], BaselineWhy::QueryLanguage)
        );
        assert_eq!(
            baseline_languages(&parse("influenza").unwrap(), &[]),
            (Vec::new(), BaselineWhy::All)
        );
    }
}
