//! Language codes for the OCR pipeline and the damage metric (12 §12.5
//! Step 4, #296).
//!
//! Three code systems meet here:
//!
//! - **MARC** (ISO 639-2/B): what LoC's title records and our catalog store
//!   (`ger`, `cze`, `srp`; see [`crate::names::LANGUAGES`]).
//! - **ISO 639-3**: queue messages and engine routing (`deu`, `ces`, `srp`).
//!   For these languages it is ISO 639-2/T, which differs from the MARC code
//!   only for the twenty or so languages with a bibliographic code.
//! - **Word lists**: the code of the damage metric's word list for the
//!   language, copied from `ja-ocr/damage_v2.py`'s `WORDFREQ` table (wordfreq's
//!   codes: `hrv` and `srp` are both `sh`, `nor` is `nb`, `tgl` is `fil`) and
//!   its `LOCAL_WORDLISTS` (Hawaiian `haw`, Yiddish `yi`, our own lists). A
//!   language with no list (Chinese, Korean, the indigenous languages, Latin,
//!   Welsh...) has none, as in `damage_v2.py`.
//!
//! With the cargo feature `damage`, [`wordlist`] returns a list's words,
//! embedded from `data/damage-words/` (`scripts/build-damage-words.py`
//! exports them from wordfreq 3.1.1; `data/README.md` has their
//! sources and licences).

/// One language in the three code systems.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    /// The catalog's MARC code.
    pub marc: &'static str,
    /// The ISO 639-3 code.
    pub iso3: &'static str,
    /// The damage metric's word list for it, if it has one.
    pub wordlist: Option<&'static str>,
    /// The list is our own (`ja-ocr/wordlists/`), not wordfreq's.
    pub local_list: bool,
}

const fn lang(marc: &'static str, iso3: &'static str, wordlist: Option<&'static str>) -> Language {
    Language {
        marc,
        iso3,
        wordlist,
        local_list: false,
    }
}

/// Every language the catalog lists ([`crate::names::LANGUAGES`]) and every
/// one `damage_v2.py` names, sorted by MARC code.
pub static LANGUAGES: &[Language] = &[
    lang("alb", "sqi", None),
    lang("ara", "ara", Some("ar")),
    lang("arm", "hye", None),
    lang("baq", "eus", None),
    lang("bel", "bel", None),
    lang("bul", "bul", Some("bg")),
    lang("chi", "zho", None),
    lang("cho", "cho", None),
    lang("chr", "chr", None),
    lang("cze", "ces", Some("cs")),
    lang("dak", "dak", None),
    lang("dan", "dan", Some("da")),
    lang("dut", "nld", Some("nl")),
    lang("eng", "eng", Some("en")),
    lang("est", "est", None),
    lang("fin", "fin", Some("fi")),
    lang("fre", "fra", Some("fr")),
    lang("ger", "deu", Some("de")),
    lang("gle", "gle", None),
    lang("gre", "ell", Some("el")),
    Language {
        local_list: true,
        ..lang("haw", "haw", Some("haw"))
    },
    lang("heb", "heb", Some("he")),
    lang("hrv", "hrv", Some("sh")),
    lang("hun", "hun", Some("hu")),
    lang("ice", "isl", Some("is")),
    lang("ita", "ita", Some("it")),
    lang("jpn", "jpn", None),
    lang("kor", "kor", None),
    lang("lad", "lad", None),
    lang("lat", "lat", None),
    lang("lav", "lav", Some("lv")),
    lang("lit", "lit", Some("lt")),
    lang("mac", "mkd", None),
    lang("mus", "mus", None),
    lang("nav", "nav", None),
    lang("nor", "nor", Some("nb")),
    lang("oji", "oji", None),
    lang("per", "fas", None),
    lang("pol", "pol", Some("pl")),
    lang("por", "por", Some("pt")),
    lang("rum", "ron", Some("ro")),
    lang("rus", "rus", Some("ru")),
    lang("slo", "slk", Some("sk")),
    lang("slv", "slv", Some("sl")),
    lang("spa", "spa", Some("es")),
    lang("srp", "srp", Some("sh")),
    lang("swe", "swe", Some("sv")),
    lang("tgl", "tgl", Some("fil")),
    lang("ukr", "ukr", Some("uk")),
    lang("wel", "cym", None),
    Language {
        local_list: true,
        ..lang("yid", "yid", Some("yi"))
    },
];

/// The language with this MARC code.
pub fn by_marc(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.marc == code)
}

/// The language with this ISO 639-3 code.
pub fn by_iso3(code: &str) -> Option<&'static Language> {
    LANGUAGES.iter().find(|l| l.iso3 == code)
}

/// A MARC code's ISO 639-3 code: `ger` to `deu`.
pub fn marc_to_iso3(code: &str) -> Option<&'static str> {
    by_marc(code).map(|l| l.iso3)
}

/// An ISO 639-3 code's MARC code: `deu` to `ger`.
pub fn iso3_to_marc(code: &str) -> Option<&'static str> {
    by_iso3(code).map(|l| l.marc)
}

/// The word list for a MARC code: `damage_v2.py`'s
/// `WORDFREQ.get(lang) or LOCAL_WORDLISTS.get(lang)`.
pub fn wordlist_code(marc: &str) -> Option<&'static str> {
    by_marc(marc).and_then(|l| l.wordlist)
}

/// Every word list code, once each, sorted.
pub fn wordlist_codes() -> Vec<&'static str> {
    let mut out: Vec<_> = LANGUAGES.iter().filter_map(|l| l.wordlist).collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The words of a word list, most frequent first, one per line: wordfreq
/// 3.1.1's `top_n_list(code, 5000)`, or our own list for `haw` and `yi`.
#[cfg(feature = "damage")]
pub fn wordlist(code: &str) -> Option<&'static str> {
    macro_rules! lists {
        ($($code:literal),* $(,)?) => {
            match code {
                $($code => Some(include_str!(concat!("../data/damage-words/", $code, ".txt"))),)*
                _ => None,
            }
        };
    }
    lists!(
        "ar", "bg", "cs", "da", "de", "el", "en", "es", "fi", "fil", "fr", "haw", "he", "hu", "is",
        "it", "lt", "lv", "nb", "nl", "pl", "pt", "ro", "ru", "sh", "sk", "sl", "sv", "uk", "yi",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn codes_round_trip() {
        for l in LANGUAGES {
            assert_eq!(marc_to_iso3(l.marc), Some(l.iso3), "{}", l.marc);
            assert_eq!(iso3_to_marc(l.iso3), Some(l.marc), "{}", l.iso3);
            assert_eq!(iso3_to_marc(marc_to_iso3(l.marc).unwrap()), Some(l.marc));
        }
        assert_eq!(marc_to_iso3("ger"), Some("deu"));
        assert_eq!(marc_to_iso3("cze"), Some("ces"));
        assert_eq!(marc_to_iso3("srp"), Some("srp"));
        assert_eq!(iso3_to_marc("fra"), Some("fre"));
        assert_eq!(marc_to_iso3("deu"), None, "an ISO code isn't a MARC code");
        assert_eq!(iso3_to_marc("ger"), None, "a MARC code isn't an ISO code");
    }

    #[test]
    fn codes_are_unique_sorted_and_well_formed() {
        let marcs: Vec<_> = LANGUAGES.iter().map(|l| l.marc).collect();
        let mut sorted = marcs.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(marcs, sorted, "sorted by MARC code, no duplicates");
        let isos: BTreeSet<_> = LANGUAGES.iter().map(|l| l.iso3).collect();
        assert_eq!(isos.len(), LANGUAGES.len(), "ISO 639-3 codes are unique");
        for l in LANGUAGES {
            for c in [l.marc, l.iso3] {
                assert!(
                    c.len() == 3 && c.bytes().all(|b| b.is_ascii_lowercase()),
                    "{c}"
                );
            }
        }
    }

    #[test]
    fn covers_the_catalog() {
        for (name, marc) in crate::names::LANGUAGES {
            assert!(by_marc(marc).is_some(), "{name} ({marc}) has no entry");
        }
    }

    /// `damage_v2.py`'s `WORDFREQ` and `LOCAL_WORDLISTS`, parsed from its source,
    /// so the two tables can't drift apart.
    fn damage_v2_tables() -> BTreeMap<String, String> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ja-ocr/damage_v2.py");
        let src = std::fs::read_to_string(path).expect("ja-ocr/damage_v2.py");
        let mut out = BTreeMap::new();
        for start in ["WORDFREQ = {", "LOCAL_WORDLISTS = {"] {
            let i = src.find(start).expect(start) + start.len();
            let body = &src[i..i + src[i..].find('}').unwrap()];
            for pair in body.split(',') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                let (k, v) = pair.split_once(':').unwrap();
                let unquote = |s: &str| s.trim().trim_matches('"').to_owned();
                assert!(out.insert(unquote(k), unquote(v)).is_none(), "{pair}");
            }
        }
        out
    }

    #[test]
    fn wordlists_match_damage_v2() {
        let ours: BTreeMap<String, String> = LANGUAGES
            .iter()
            .filter_map(|l| l.wordlist.map(|w| (l.marc.to_owned(), w.to_owned())))
            .collect();
        assert_eq!(ours, damage_v2_tables());
        assert_eq!(wordlist_code("hrv"), Some("sh"));
        assert_eq!(wordlist_code("srp"), Some("sh"));
        assert_eq!(wordlist_code("nor"), Some("nb"));
        assert_eq!(wordlist_code("tgl"), Some("fil"));
        assert_eq!(wordlist_code("yid"), Some("yi"));
        assert_eq!(wordlist_code("chi"), None);
        let local: Vec<_> = LANGUAGES
            .iter()
            .filter(|l| l.local_list)
            .map(|l| l.marc)
            .collect();
        assert_eq!(local, ["haw", "yid"]);
    }

    #[cfg(feature = "damage")]
    #[test]
    fn every_wordlist_is_embedded() {
        for code in wordlist_codes() {
            let words = wordlist(code).unwrap_or_else(|| panic!("no list for {code}"));
            let lines: Vec<_> = words.lines().collect();
            assert_eq!(lines.len(), 5000, "{code}");
            assert!(
                lines
                    .iter()
                    .all(|w| !w.is_empty() && !w.contains(char::is_whitespace)),
                "{code}"
            );
        }
        assert_eq!(wordlist("ja"), None);
        // Every file in the directory is one the table names.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/data/damage-words");
        let files: BTreeSet<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.unwrap().file_name().into_string().ok())
            .filter_map(|n| n.strip_suffix(".txt").map(str::to_owned))
            .collect();
        let codes: BTreeSet<String> = wordlist_codes().into_iter().map(str::to_owned).collect();
        assert_eq!(files, codes);
    }

    #[cfg(feature = "damage")]
    #[test]
    fn local_lists_are_ja_ocrs() {
        // damage_v2.py reads ja-ocr/wordlists/<code>.txt itself: the embedded
        // copies must be its first 5,000 words.
        for l in LANGUAGES.iter().filter(|l| l.local_list) {
            let code = l.wordlist.unwrap();
            let path = format!(
                "{}/../../ja-ocr/wordlists/{code}.txt",
                env!("CARGO_MANIFEST_DIR")
            );
            let src = std::fs::read_to_string(&path).expect("the local word list");
            let want: Vec<&str> = src
                .split('\n')
                .filter(|w| !w.is_empty())
                .take(5000)
                .collect();
            assert_eq!(
                wordlist(code).unwrap().lines().collect::<Vec<_>>(),
                want,
                "{code}"
            );
        }
    }

    #[cfg(feature = "damage")]
    #[test]
    fn lists_start_with_their_most_frequent_words() {
        assert_eq!(wordlist("en").unwrap().lines().next(), Some("the"));
        assert_eq!(
            wordlist("de").unwrap().lines().take(3).collect::<Vec<_>>(),
            ["die", "der", "und"]
        );
    }
}
