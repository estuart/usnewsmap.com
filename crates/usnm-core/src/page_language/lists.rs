//! Scripts, and each word list's function words
//! (`data/page-language-words.txt`, built by
//! `scripts/build-page-language-words.py` from `ja-ocr/damage_v2.py`'s
//! models).

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::OnceLock;

use super::text::latin_char;
use crate::lang;

/// The embedded word lists: one line per list, `code script word...`.
const WORDS: &str = include_str!("../../data/page-language-words.txt");

/// Scripts other than Latin, by code point.
const SCRIPT_RANGES: &[(&str, u32, u32)] = &[
    ("hebrew", 0x0590, 0x05FF),
    ("hebrew", 0xFB1D, 0xFB4F),
    ("cyrillic", 0x0400, 0x052F),
    ("greek", 0x0370, 0x03FF),
    ("greek", 0x1F00, 0x1FFF),
    ("arabic", 0x0600, 0x06FF),
    ("arabic", 0x0750, 0x077F),
    ("arabic", 0xFB50, 0xFDFF),
    ("arabic", 0xFE70, 0xFEFF),
    ("cherokee", 0x13A0, 0x13FF),
    ("cherokee", 0xAB70, 0xABBF),
    ("cjk", 0x3040, 0x30FF),
    ("cjk", 0x3400, 0x9FFF),
    ("cjk", 0xAC00, 0xD7AF),
];

/// Catalog languages written in a script other than Latin. Serbian is either.
const LANGUAGE_SCRIPTS: &[(&str, &[&str])] = &[
    ("yid", &["hebrew"]),
    ("heb", &["hebrew"]),
    ("lad", &["hebrew"]),
    ("rus", &["cyrillic"]),
    ("ukr", &["cyrillic"]),
    ("bul", &["cyrillic"]),
    ("bel", &["cyrillic"]),
    ("mac", &["cyrillic"]),
    ("srp", &["latin", "cyrillic"]),
    ("gre", &["greek"]),
    ("ara", &["arabic"]),
    ("per", &["arabic"]),
    ("chr", &["cherokee"]),
    ("chi", &["cjk"]),
    ("kor", &["cjk"]),
    ("jpn", &["cjk"]),
];

/// The script of a word, by its first character: `latin`, `hebrew`,
/// `cyrillic`, `greek`, `arabic`, `cherokee`, `cjk` or `other`.
pub fn script(word: &str) -> &'static str {
    let Some(c) = word.chars().next() else {
        return "other";
    };
    if latin_char(c) {
        return "latin";
    }
    let o = c as u32;
    SCRIPT_RANGES
        .iter()
        .find(|(_, lo, hi)| (*lo..=*hi).contains(&o))
        .map_or("other", |(name, _, _)| name)
}

/// The scripts a catalog language (MARC code) is written in.
pub fn scripts_of(marc: &str) -> &'static [&'static str] {
    LANGUAGE_SCRIPTS
        .iter()
        .find(|(l, _)| *l == marc)
        .map_or(&["latin"], |(_, s)| s)
}

/// FxHash (rustc's): fast for short keys. Only the fixed word sets use it;
/// maps keyed by a page's words keep the standard hasher, so text can't be
/// chosen to collide.
#[derive(Debug, Default, Clone, Copy)]
pub struct WordHasher(u64);

impl WordHasher {
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

impl Hasher for WordHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(b));
        }
    }

    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }

    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// A fixed set of words.
pub type WordSet = HashSet<String, BuildHasherDefault<WordHasher>>;
/// A fixed map from words.
pub type WordMap<V> = HashMap<String, V, BuildHasherDefault<WordHasher>>;

/// A word list's function words: its 100 most frequent words of 2 letters
/// or more (and for Danish and Norwegian their `aa` spellings).
#[derive(Debug)]
pub struct FunctionWords {
    /// The word list's code (`de`, `sh`, `yi`).
    pub code: &'static str,
    /// The script of its words.
    pub script: &'static str,
    /// The words.
    pub top: WordSet,
}

/// Every embedded list, by code, read once.
fn lists() -> &'static HashMap<&'static str, FunctionWords> {
    static LISTS: OnceLock<HashMap<&'static str, FunctionWords>> = OnceLock::new();
    LISTS.get_or_init(|| parse(WORDS))
}

fn parse(text: &'static str) -> HashMap<&'static str, FunctionWords> {
    text.lines()
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.split(' ');
            let code = parts.next()?;
            let script = parts.next()?;
            let top: WordSet = parts.map(str::to_owned).collect();
            Some((code, FunctionWords { code, script, top }))
        })
        .collect()
}

/// `Models.get`, for the function words only: the list for a catalog
/// language (MARC code), if it has one.
pub fn function_words(marc: &str) -> Option<&'static FunctionWords> {
    lang::wordlist_code(marc).and_then(|code| lists().get(code))
}

#[cfg(test)]
mod tests {
    use super::super::text::str_is_alpha;
    use super::*;

    #[test]
    fn every_word_list_is_embedded_once() {
        let mut codes: Vec<&str> = lists().keys().copied().collect();
        codes.sort_unstable();
        assert_eq!(codes, lang::wordlist_codes());
        assert_eq!(codes.len(), 30);
        for l in lists().values() {
            assert!(
                ["latin", "hebrew", "cyrillic", "greek", "arabic"].contains(&l.script),
                "{}: {}",
                l.code,
                l.script
            );
            let aa = l.top.iter().filter(|w| w.contains("aa")).count();
            assert!(
                (100..=100 + aa).contains(&l.top.len()),
                "{}: {} words",
                l.code,
                l.top.len()
            );
            for w in &l.top {
                assert!(str_is_alpha(w) && w.chars().count() >= 2, "{w}");
            }
        }
    }

    #[test]
    fn languages_share_a_list() {
        assert!(std::ptr::eq(
            function_words("hrv").unwrap(),
            function_words("srp").unwrap()
        ));
        assert_eq!(function_words("srp").unwrap().script, "latin");
        assert_eq!(function_words("yid").unwrap().script, "hebrew");
        assert!(function_words("nor").unwrap().top.contains("paa"));
        assert!(function_words("dan").unwrap().top.contains("paa"));
        assert!(function_words("eng").unwrap().top.contains("the"));
        for marc in ["chi", "jpn", "cho", "chr", "dak", "lat", "zzz"] {
            assert!(function_words(marc).is_none(), "{marc}");
        }
    }

    #[test]
    fn scripts() {
        assert_eq!(script("ꭰ"), "cherokee");
        assert_eq!(script("1"), "latin");
        assert_eq!(script("ẞ"), "latin");
        assert_eq!(script("日本"), "cjk");
        assert_eq!(script("ა"), "other");
        assert_eq!(script(""), "other");
        assert_eq!(scripts_of("srp"), ["latin", "cyrillic"]);
        assert_eq!(scripts_of("ger"), ["latin"]);
        assert_eq!(scripts_of("jpn"), ["cjk"]);
    }

    /// The same words as the damage metric's models (which read wordfreq's
    /// top 5,000 themselves), so the two ports can't drift apart.
    #[cfg(feature = "damage")]
    #[test]
    fn the_damage_models_have_the_same_function_words() {
        for code in lang::wordlist_codes() {
            let marc = lang::LANGUAGES
                .iter()
                .find(|l| l.wordlist == Some(code))
                .unwrap()
                .marc;
            let ours = function_words(marc).unwrap();
            let theirs = crate::damage::model(marc).unwrap();
            assert_eq!(ours.top, theirs.top, "{code}");
            assert_eq!(ours.script, theirs.script, "{code}");
        }
    }
}
