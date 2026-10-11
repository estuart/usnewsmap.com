//! Language models: a language's function words, head words, real words and
//! the head words' OCR near-misses (`damage_v2.py`'s `LangModel`,
//! `build_model` and `Models`).

use std::collections::{BTreeSet, HashMap};
use std::sync::OnceLock;

use crate::lang;
use crate::page_language::text::{latin_char, str_is_alpha};
pub use crate::page_language::{script, scripts_of, WordMap, WordSet};

/// The most frequent words that count as function words.
pub const TOP_FUNCTION: usize = 100;
/// The most frequent words whose near-misses count as damage.
pub const TOP_HEAD: usize = 20;
/// The most frequent words that are real words, never near-misses.
pub const TOP_REAL: usize = 5000;
const LATIN_LETTERS: &str = "abcdefghijklmnopqrstuvwxyz";
/// What a speck or a split letter reads as: inserted, or two for one ("li" for "h").
const THIN_LETTERS: &str = "cfijlnrtv";
/// Close languages whose words are real words of each other's pages.
const RELATED: &[(&str, &[&str])] = &[
    ("nb", &["da"]),
    ("da", &["nb"]),
    ("cs", &["sk"]),
    ("sk", &["cs"]),
];
/// "paa" for "på", as Danish and Norwegian were printed until the 1900s.
const AA_SPELLING: &[&str] = &["da", "nb"];

/// A language's function words (top 100), its head words (top 20), their
/// OCR near-misses that aren't real words, and its real words (top 5,000).
#[derive(Debug)]
pub struct LangModel {
    /// The word list's code (`de`, `sh`, `yi`).
    pub code: &'static str,
    /// Function words: the top 100 words of 2 letters or more.
    pub top: WordSet,
    /// Head words: the top 20, most frequent first.
    pub head: Vec<String>,
    head_set: WordSet,
    /// Near-miss to the head word it is credited to (the simplest edit, the
    /// most frequent word).
    pub near: WordMap<String>,
    /// Real words: never near-misses.
    pub real: WordSet,
    /// The script of its words.
    pub script: &'static str,
}

impl LangModel {
    pub(crate) fn is_head(&self, w: &str) -> bool {
        self.head_set.contains(w)
    }
}

/// The strings one OCR edit of `kind` from `word`, in `damage_v2.py`'s
/// order: 0 a letter substituted (any of `letters`), 1 deleted, 2 inserted
/// (one of `inserted`), 3 read as two (two of `split`).
fn edits(
    word: &[char],
    kind: u8,
    letters: &[char],
    inserted: &[char],
    split: &[char],
    out: &mut Vec<String>,
) {
    let join = |parts: &[&[char]]| parts.iter().flat_map(|p| p.iter()).collect::<String>();
    match kind {
        0 => {
            for (i, &ch) in word.iter().enumerate() {
                for &c in letters {
                    if c != ch {
                        out.push(join(&[&word[..i], &[c], &word[i + 1..]]));
                    }
                }
            }
        }
        1 => {
            for i in 0..word.len() {
                out.push(join(&[&word[..i], &word[i + 1..]]));
            }
        }
        2 => {
            for i in 0..=word.len() {
                for &c in inserted {
                    out.push(join(&[&word[..i], &[c], &word[i..]]));
                }
            }
        }
        _ => {
            for i in 0..word.len() {
                for &a in split {
                    for &b in split {
                        out.push(join(&[&word[..i], &[a, b], &word[i + 1..]]));
                    }
                }
            }
        }
    }
}

/// `build_model`: a model from a language's most frequent words, most
/// frequent first; `also_real` are more words that aren't near-misses (a
/// closely related language's).
pub fn build_model(code: &'static str, ranked: &[&str], also_real: &[&str]) -> Option<LangModel> {
    let mut real: WordSet = ranked
        .iter()
        .take(TOP_REAL)
        .map(|w| (*w).to_owned())
        .collect();
    real.extend(also_real.iter().map(|w| (*w).to_owned()));
    let words: Vec<&str> = ranked
        .iter()
        .copied()
        .filter(|w| w.chars().count() >= 2 && str_is_alpha(w))
        .collect();
    let mut top: Vec<String> = words
        .iter()
        .take(TOP_FUNCTION)
        .map(|w| (*w).to_owned())
        .collect();
    let mut head: Vec<String> = words
        .iter()
        .take(TOP_HEAD)
        .map(|w| (*w).to_owned())
        .collect();
    if top.is_empty() {
        return None;
    }
    if AA_SPELLING.contains(&code) {
        let aa = |ws: &[String]| -> Vec<String> {
            ws.iter()
                .filter(|w| w.contains('å'))
                .map(|w| w.replace('å', "aa"))
                .collect()
        };
        let (more_top, more_head) = (aa(&top), aa(&head));
        top.extend(more_top);
        head.extend(more_head);
        let more_real: Vec<String> = real
            .iter()
            .filter(|w| w.contains('å'))
            .map(|w| w.replace('å', "aa"))
            .collect();
        real.extend(more_real);
    }
    let own: BTreeSet<char> = top.iter().flat_map(|w| w.chars()).collect();
    let (letters, inserted, split): (Vec<char>, Vec<char>, Vec<char>) =
        if own.iter().all(|&c| latin_char(c)) {
            let mut letters = own.clone();
            letters.extend(LATIN_LETTERS.chars());
            (
                letters.into_iter().collect(),
                THIN_LETTERS.chars().collect(),
                THIN_LETTERS.chars().collect(),
            )
        } else {
            let letters: Vec<char> = own.into_iter().collect();
            (letters.clone(), letters, Vec::new())
        };
    // A near-miss is credited to the word it is the simplest edit of.
    let mut near: WordMap<String> = WordMap::default();
    let mut buf = Vec::new();
    for kind in 0..4 {
        for base in &head {
            let chars: Vec<char> = base.chars().collect();
            buf.clear();
            edits(&chars, kind, &letters, &inserted, &split, &mut buf);
            for v in buf.drain(..) {
                if v.chars().count() >= 2 && !real.contains(&v) && !near.contains_key(&v) {
                    near.insert(v, base.clone());
                }
            }
        }
    }
    let script = script(&top[0]);
    Some(LangModel {
        code,
        head_set: head.iter().cloned().collect(),
        top: top.into_iter().collect(),
        head,
        near,
        real,
        script,
    })
}

fn ranked(code: &str) -> Vec<&'static str> {
    lang::wordlist(code).map_or_else(Vec::new, |w| w.lines().take(TOP_REAL).collect())
}

/// Every word list's model, built once.
fn models() -> &'static HashMap<&'static str, LangModel> {
    static MODELS: OnceLock<HashMap<&'static str, LangModel>> = OnceLock::new();
    MODELS.get_or_init(|| {
        lang::wordlist_codes()
            .into_iter()
            .filter_map(|code| {
                let also: Vec<&str> = RELATED
                    .iter()
                    .filter(|(c, _)| *c == code)
                    .flat_map(|(_, rel)| rel.iter().flat_map(|r| ranked(r)))
                    .collect();
                build_model(code, &ranked(code), &also).map(|m| (code, m))
            })
            .collect()
    })
}

/// `Models.get`: the model for a catalog language (MARC code), if it has a
/// word list.
pub fn model(marc: &str) -> Option<&'static LangModel> {
    lang::wordlist_code(marc).and_then(|code| models().get(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_near_misses() {
        let en = model("eng").unwrap();
        for (miss, word) in [
            ("tbe", "the"),
            ("tlie", "the"),
            ("thc", "the"),
            ("ihe", "the"),
            ("lhe", "the"),
            ("tne", "the"),
            ("aud", "and"),
            ("nnd", "and"),
            ("ot", "of"),
            ("iu", "in"),
        ] {
            assert_eq!(en.near.get(miss).map(String::as_str), Some(word), "{miss}");
        }
        for word in [
            "she", "an", "or", "he", "then", "hand", "they", "tho", "is", "on",
        ] {
            assert!(!en.near.contains_key(word), "{word}");
        }
        assert_eq!(en.script, "latin");
    }

    #[test]
    fn german_near_misses() {
        let de = model("ger").unwrap();
        for (miss, word) in [
            ("ift", "ist"),
            ("fich", "sich"),
            ("bie", "die"),
            ("unb", "und"),
            ("dcr", "der"),
        ] {
            assert_eq!(de.near.get(miss).map(String::as_str), Some(word), "{miss}");
        }
        assert!(["sie", "die"].contains(&de.near["fie"].as_str()));
        assert!(!de.near.contains_key("bcr") && !de.near.contains_key("beu"));
        for word in ["sie", "die", "ein", "den", "dem", "bei"] {
            assert!(!de.near.contains_key(word), "{word}");
        }
    }

    #[test]
    fn dano_norwegian_spellings_are_not_damage() {
        let nor = model("nor").unwrap();
        assert!(!nor.near.contains_key("af") && !nor.near.contains_key("aar"));
        assert!(nor.top.contains("paa"));
        assert_eq!(nor.near.get("fom").map(String::as_str), Some("som"));
    }

    #[test]
    fn scripts_of_the_models() {
        assert_eq!(model("srp").unwrap().script, "latin");
        assert_eq!(model("rus").unwrap().script, "cyrillic");
        assert_eq!(model("yid").unwrap().script, "hebrew");
        assert_eq!(model("gre").unwrap().script, "greek");
        assert!(std::ptr::eq(model("hrv").unwrap(), model("srp").unwrap()));
        assert!(model("chi").is_none() && model("cho").is_none());
        assert_eq!(script("ꭰ"), "cherokee");
        assert_eq!(script("1"), "latin");
        assert_eq!(scripts_of("srp"), ["latin", "cyrillic"]);
        assert_eq!(scripts_of("ger"), ["latin"]);
    }

    #[test]
    fn edits_in_order() {
        let mut got = Vec::new();
        let ab: Vec<char> = "ab".chars().collect();
        for kind in 0..4 {
            edits(&ab, kind, &['a', 'b', 'x'], &['i'], &['i', 'l'], &mut got);
        }
        for want in [
            "b", "a", "xb", "ax", "iab", "aib", "abi", "iib", "lib", "ail",
        ] {
            assert!(got.iter().any(|g| g == want), "{want}");
        }
        assert!(!got.iter().any(|g| g == "xab" || g == "ab"));
        let at = |s: &str| got.iter().position(|g| g == s).unwrap();
        assert!(at("xb") < at("b") && at("b") < at("iab") && at("iab") < at("lib"));
    }
}
