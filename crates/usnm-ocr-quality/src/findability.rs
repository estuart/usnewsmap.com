//! Findability: the site is a search index over places, so beyond a
//! character error rate each text gets the share of the consensus reading's
//! distinct content words, and separately its place names, that it
//! contains. A content word is a token of at least four letters that is
//! not in the language's function-word list given in the job; place names
//! come from the campaign's `places.txt`, one name per line.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Letters a token needs to count as a content word.
pub const MIN_LETTERS: usize = 4;

/// Whether a folded token is a content word.
pub fn is_content(token: &str, function_words: &BTreeSet<String>) -> bool {
    token.chars().filter(|c| c.is_alphabetic()).count() >= MIN_LETTERS
        && !function_words.contains(token)
}

/// The distinct content words of a token sequence.
pub fn content_words(tokens: &[String], function_words: &BTreeSet<String>) -> BTreeSet<String> {
    tokens
        .iter()
        .filter(|t| is_content(t, function_words))
        .cloned()
        .collect()
}

/// The place names of a campaign, tokenised as the readers' texts are
/// ([`crate::consensus::tokens`], folded), with an index on the first
/// token so a page's tokens can be scanned once.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Places {
    names: Vec<Vec<String>>,
    first: BTreeMap<String, Vec<usize>>,
}

impl Places {
    /// One name per line; blank lines and `#` comments skipped; names
    /// that tokenise to nothing dropped; duplicates (after folding) merged.
    pub fn parse(text: &str) -> Places {
        let mut set: BTreeSet<Vec<String>> = BTreeSet::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let toks: Vec<String> = crate::consensus::tokens(line)
                .iter()
                .map(|t| crate::consensus::fold(t))
                .collect();
            if !toks.is_empty() {
                set.insert(toks);
            }
        }
        let names: Vec<Vec<String>> = set.into_iter().collect();
        let mut first: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, n) in names.iter().enumerate() {
            first.entry(n[0].clone()).or_default().push(i);
        }
        Places { names, first }
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The names whose token sequence appears consecutively in `tokens`
    /// (folded), as the names joined by single spaces.
    pub fn found(&self, tokens: &[String]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for (p, t) in tokens.iter().enumerate() {
            let Some(cands) = self.first.get(t) else {
                continue;
            };
            for &c in cands {
                let name = &self.names[c];
                if tokens.len() >= p + name.len() && tokens[p..p + name.len()] == name[..] {
                    out.insert(name.join(" "));
                }
            }
        }
        out
    }
}

/// How findable the consensus reading's words are in one reader's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Findability {
    /// Distinct content words in the consensus.
    pub content_total: usize,
    /// Of those, the ones the reader's text has.
    pub content_found: usize,
    /// `content_found / content_total`; none when the consensus has no
    /// content words.
    pub content: Option<f64>,
    /// Distinct place names in the consensus.
    pub places_total: usize,
    pub places_found: usize,
    pub places: Option<f64>,
}

/// `consensus` and `reader` are folded token sequences.
pub fn of(
    consensus: &[String],
    reader: &[String],
    function_words: &BTreeSet<String>,
    places: &Places,
) -> Findability {
    let want = content_words(consensus, function_words);
    let have: BTreeSet<&String> = reader.iter().collect();
    let content_found = want.iter().filter(|w| have.contains(w)).count();
    let want_places = places.found(consensus);
    let have_places = places.found(reader);
    let places_found = want_places.intersection(&have_places).count();
    Findability {
        content_total: want.len(),
        content_found,
        content: (!want.is_empty()).then(|| content_found as f64 / want.len() as f64),
        places_total: want_places.len(),
        places_found,
        places: (!want_places.is_empty()).then(|| places_found as f64 / want_places.len() as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        crate::consensus::tokens(s)
            .iter()
            .map(|t| crate::consensus::fold(t))
            .collect()
    }

    #[test]
    fn content_words_need_four_letters_and_no_function_word() {
        let fw: BTreeSet<String> = ["with", "that"].iter().map(|s| s.to_string()).collect();
        assert!(is_content("mayor", &fw));
        assert!(!is_content("with", &fw));
        assert!(!is_content("the", &fw));
        assert!(!is_content("1876", &fw));
        assert!(!is_content("a1b2", &fw));
        assert_eq!(
            content_words(&toks("the mayor of that town, 1876"), &fw),
            ["mayor", "town"].iter().map(|s| s.to_string()).collect()
        );
    }

    #[test]
    fn places_match_token_sequences() {
        let p = Places::parse("New York\nAlbany\n# a comment\n\nalbany\nSt. Louis\n");
        assert_eq!(p.len(), 3);
        let found = p.found(&toks("left Albany for New York and St Louis."));
        assert_eq!(
            found,
            ["albany", "new york", "st louis"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
        assert!(p.found(&toks("New Jersey")).is_empty());
    }

    #[test]
    fn rates() {
        let fw = BTreeSet::new();
        let p = Places::parse("Albany\nTroy\n");
        let cons = toks("mayor spoke at Albany and Troy about bridges");
        let reader = toks("mayor spoke at Albany and Tray about bridgcs");
        let f = of(&cons, &reader, &fw, &p);
        assert_eq!((f.content_total, f.content_found), (6, 4));
        assert_eq!((f.places_total, f.places_found), (2, 1));
        assert_eq!(f.places, Some(0.5));
        let none = of(&toks("a b"), &reader, &fw, &p);
        assert_eq!(none.content, None);
        assert_eq!(none.places, None);
    }
}
