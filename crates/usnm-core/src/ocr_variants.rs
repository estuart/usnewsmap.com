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

/// Whether `token` is one of `word`'s [`variants`], without building them
/// all when it can't be.
pub fn is_variant(word: &str, token: &str) -> bool {
    token == word
        || (token.len().abs_diff(word.len()) <= 1 && variants(word).iter().any(|v| v == token))
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
        assert!(is_variant("gold", "golcl"));
        assert!(!is_variant("gold", "goicl"));
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
