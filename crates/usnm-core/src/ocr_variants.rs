//! The spellings OCR commonly gives a word, for `word~ocr` (06 §6.4, #124).
//!
//! Quickwit 0.9.1 has no fuzzy matching (05 §5.5.1), so an OCR-tolerant
//! search names the misreadings it accepts instead: the word and each
//! spelling one confusion in [`CONFUSIONS`] away from it, sent to the engine
//! as one term set (`text:IN [...]`). The table is fixed and short, so the
//! same word always has the same spellings (and a search one cache key),
//! and the set stays small: a word of the English list has at most 16.

use std::collections::BTreeSet;

/// Pairs of letter groups 19th-century newspaper OCR reads one as the
/// other, each applied both ways: `h` as `li` and `li` as `h`, `m` as `rn`,
/// `e` as `c`, `th` as `tb`, a long `s` as `f`, `n` as `u`, `i` as `l`, `d`
/// as `cl` and `w` as `vv`.
pub const CONFUSIONS: [(&str, &str); 9] = [
    ("h", "li"),
    ("m", "rn"),
    ("e", "c"),
    ("th", "tb"),
    ("s", "f"),
    ("n", "u"),
    ("i", "l"),
    ("d", "cl"),
    ("w", "vv"),
];

/// The most spellings a `~ocr` word may have, itself included. No word of
/// the English word list has more than 16; a longer one is refused.
pub const MAX_OCR_VARIANTS: usize = 32;

/// `word` and each spelling one confusion away from it, sorted: one
/// occurrence of one side of a pair in [`CONFUSIONS`] replaced by the
/// other side. Two misreadings in one word aren't covered; that would
/// square the set's size.
pub fn variants(word: &str) -> Vec<String> {
    let mut out = BTreeSet::from([word.to_owned()]);
    for (a, b) in CONFUSIONS {
        for (from, to) in [(a, b), (b, a)] {
            for (i, _) in word.match_indices(from) {
                out.insert(format!("{}{to}{}", &word[..i], &word[i + from.len()..]));
            }
        }
    }
    out.into_iter().collect()
}

/// Whether `token` is one of `word`'s [`variants`]: `word` itself, or
/// `word` with one occurrence of one side of a pair in [`CONFUSIONS`]
/// replaced by the other. Checked in place, without building the variants:
/// it runs on every word of every page a `~ocr` search reads.
pub fn is_variant(word: &str, token: &str) -> bool {
    if token == word {
        return true;
    }
    if token.len().abs_diff(word.len()) > 1 {
        return false;
    }
    let (w, t) = (word.as_bytes(), token.as_bytes());
    CONFUSIONS
        .iter()
        .flat_map(|&(a, b)| [(a, b), (b, a)])
        .filter(|(from, to)| w.len() + to.len() == t.len() + from.len())
        .any(|(from, to)| {
            word.match_indices(from).any(|(i, _)| {
                t[..i] == w[..i]
                    && &t[i..i + to.len()] == to.as_bytes()
                    && t[i + to.len()..] == w[i + from.len()..]
            })
        })
}

/// Whether `word` is one index token that a term set can hold as typed:
/// letters and digits only. [`Analyzer::V1`](crate::text::Analyzer::V1)
/// can fold one word into several (`ﷺ` into four Arabic words) or into
/// text with a separator (`½` into `1⁄2`), which the engine's term-set
/// syntax would split into several members, and no page token equals.
/// Its [`variants`] only swap ASCII letters, so they are one token too.
pub fn is_one_token(word: &str) -> bool {
    !word.is_empty() && word.chars().all(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_confusion_away_both_ways() {
        assert_eq!(
            variants("president"),
            [
                "prcsident",
                "prefident",
                "presiclent",
                "presidcnt",
                "president",
                "presideut",
                "presldent",
            ]
        );
        assert_eq!(variants("the"), ["tbe", "thc", "the", "tlie"]);
        // Each direction: `li` and `rn` read back as `h` and `m`.
        assert!(variants("tlie").contains(&"the".to_owned()));
        assert!(variants("rnoney").contains(&"money".to_owned()));
        assert!(variants("money").contains(&"rnoney".to_owned()));
        // A long s read as f, anywhere in the word.
        let congress = variants("congress");
        assert!(congress.contains(&"congrefs".to_owned()));
        assert!(congress.contains(&"congresf".to_owned()));
        // One misreading at a time.
        assert!(!congress.contains(&"congreff".to_owned()));
        assert_eq!(variants("xyz"), ["xyz"]);
    }

    #[test]
    fn is_variant_agrees_with_variants() {
        for w in ["president", "washington", "gold", "convention", "the"] {
            for v in variants(w) {
                assert!(is_variant(w, &v), "{w} {v}");
            }
            assert!(!is_variant(w, "silver"));
        }
        assert!(is_variant("gold", "goid"));
        assert!(!is_variant("gold", "gol"));
        assert!(!is_variant("gold", "golcll"));
        assert!(!is_variant("the", "tlic"));
        assert!(is_variant("gold", "golcl"));
        assert!(!is_variant("gold", "goicl"));
    }

    /// The direct check accepts exactly the generated set: every variant,
    /// and no token one edit of the table's letters away that isn't one.
    #[test]
    fn is_variant_is_exactly_the_variant_set() {
        let mut words: Vec<String> = [
            "president",
            "the",
            "money",
            "rnoney",
            "tlie",
            "congress",
            "gold",
            "golcl",
            "washington",
            "vvest",
            "thth",
            "lili",
            "hh",
            "x",
            "ſo",
            "café",
            "½",
        ]
        .map(String::from)
        .to_vec();
        // Every side of every pair, alone and inside a word.
        for (a, b) in CONFUSIONS {
            for s in [a, b] {
                words.push(s.into());
                words.push(format!("a{s}a"));
            }
        }
        let letters: Vec<String> = CONFUSIONS
            .iter()
            .flat_map(|(a, b)| [*a, *b])
            .chain(["a", "o", "x", ""])
            .map(String::from)
            .collect();
        for w in &words {
            let set: BTreeSet<String> = variants(w).into_iter().collect();
            for v in &set {
                assert!(is_variant(w, v), "{w} {v}");
            }
            // Candidates: each byte position's span replaced by each side
            // of each pair (or nothing), plus each variant's own variants.
            let mut candidates: BTreeSet<String> = set.iter().flat_map(|v| variants(v)).collect();
            for i in 0..=w.len() {
                for j in i..=(i + 2).min(w.len()) {
                    let (Some(pre), Some(post)) = (w.get(..i), w.get(j..)) else {
                        continue;
                    };
                    for l in &letters {
                        candidates.insert(format!("{pre}{l}{post}"));
                    }
                }
            }
            for c in &candidates {
                assert_eq!(is_variant(w, c), set.contains(c), "{w} {c}");
            }
        }
        for w in crate::query_language::ENGLISH_WORDS.lines().take(2000) {
            let set = variants(w);
            for v in set.iter().flat_map(|v| variants(v)) {
                assert_eq!(is_variant(w, &v), set.contains(&v), "{w} {v}");
            }
        }
    }

    #[test]
    fn one_token_is_letters_and_digits() {
        assert!(is_one_token("president"));
        assert!(is_one_token("1896"));
        assert!(is_one_token("½"));
        assert!(!is_one_token(""));
        assert!(!is_one_token("1⁄2"));
        assert!(!is_one_token("a b"));
        assert!(!is_one_token("a]"));
        use crate::text::{fold, Analyzer};
        assert!(!is_one_token(&fold("ﷺ", Analyzer::V1)));
        assert!(is_one_token(&fold("ﷺ", Analyzer::V2)));
    }

    #[test]
    fn english_words_stay_under_the_limit() {
        let most = crate::query_language::ENGLISH_WORDS
            .lines()
            .map(|w| variants(w).len())
            .max()
            .unwrap();
        assert_eq!(most, 16);
        assert!(most <= MAX_OCR_VARIANTS);
    }
}
