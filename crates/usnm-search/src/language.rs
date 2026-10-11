//! How `lang=` matches pages (06 §6.3.1): by the languages their title
//! lists, or, on a version with page languages at the API's
//! `usnm_core::page_language::VERSION`, by each page's own (05 §5.5.8,
//! #289).
//!
//! A page-language filter is rendered as the smaller side. The match side
//! names the languages (`page_language:IN [L]`); the complement names every
//! set of page languages that has none of them, and `none`
//! (`NOT page_language_set:IN [...]`). Both select the same pages, given
//! each set's pages from the version's page-language baselines: the
//! complement is on the set field so that a page in English and French
//! still matches `lang=eng`, which `NOT page_language:IN [fre]` would drop.
//! For `lang=eng` the complement is a few percent of the pages, so each
//! split warms short posting lists instead of one that covers nearly every
//! page.

use usnm_core::page_language::{is_code, set_key, NO_LANGUAGE};

/// A `lang=` filter as a backend applies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageMatch {
    /// Pages of titles that list any of the languages: `language:IN [...]`.
    Title(Vec<String>),
    /// Pages written in any of them: `page_language:IN [...]`.
    Page(Vec<String>),
    /// The same pages, as those in none of these sets of page languages
    /// ([`usnm_core::page_language::set_key`]s, `none` among them):
    /// `NOT page_language_set:IN [...]`.
    NotPageSets(Vec<String>),
}

/// Which side a page-language filter is rendered as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageLanguageSide {
    /// The side with fewer pages.
    #[default]
    Smaller,
    /// Always the match side (`USNM_PAGE_LANGUAGE_FILTER=match`): for
    /// measuring the complement against it, or turning it off.
    Match,
}

/// A version's sets of page languages, each with its pages (from its
/// page-language baselines), which pick a filter's side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageLanguageSets {
    /// Sorted languages and pages, one entry per set.
    sets: Vec<(Vec<String>, u64)>,
    side: PageLanguageSide,
}

impl PageLanguageSets {
    pub fn new(sets: impl IntoIterator<Item = (Vec<String>, u64)>) -> Self {
        let mut sets: Vec<(Vec<String>, u64)> = sets.into_iter().collect();
        sets.sort();
        Self {
            sets,
            side: PageLanguageSide::Smaller,
        }
    }

    /// Render filters on `side`.
    pub fn with_side(mut self, side: PageLanguageSide) -> Self {
        self.side = side;
        self
    }

    pub fn side(&self) -> PageLanguageSide {
        self.side
    }

    /// The sets and their pages.
    pub fn sets(&self) -> &[(Vec<String>, u64)] {
        &self.sets
    }

    /// Pages written in `lang`: those of every set that has it.
    pub fn pages_in(&self, lang: &str) -> u64 {
        self.sets
            .iter()
            .filter(|(s, _)| s.iter().any(|l| l == lang))
            .map(|(_, n)| n)
            .sum()
    }

    /// `lang=langs` on the smaller side (or the match side when told to,
    /// or when a set has a code that can't go in a query).
    pub fn matching(&self, langs: &[String]) -> LanguageMatch {
        let (mut matching, mut rest) = (0u64, 0u64);
        let mut others = Vec::new();
        for (set, pages) in &self.sets {
            if set.iter().any(|l| langs.contains(l)) {
                matching += pages;
            } else {
                rest += pages;
                others.push(set);
            }
        }
        let quotable = others.iter().all(|s| s.iter().all(|l| is_code(l)));
        if self.side == PageLanguageSide::Smaller && rest < matching && quotable {
            let mut keys: Vec<String> = others.iter().map(|s| set_key(s)).collect();
            keys.push(NO_LANGUAGE.to_owned());
            LanguageMatch::NotPageSets(keys)
        } else {
            LanguageMatch::Page(langs.to_vec())
        }
    }
}

impl LanguageMatch {
    /// Whether a page whose title lists `title` and whose own languages are
    /// `page` (as one set, `page_set`) matches: the memory backend's test,
    /// the same as Quickwit's.
    pub fn matches(&self, title: &[String], page: &[String], page_set: Option<&str>) -> bool {
        match self {
            LanguageMatch::Title(langs) => title.iter().any(|l| langs.contains(l)),
            LanguageMatch::Page(langs) => page.iter().any(|l| langs.contains(l)),
            LanguageMatch::NotPageSets(sets) => {
                !page_set.is_some_and(|s| sets.iter().any(|x| x == s))
            }
        }
    }

    /// The Quickwit clause.
    pub fn clause(&self) -> String {
        match self {
            LanguageMatch::Title(langs) => format!("language:IN [{}]", langs.join(" ")),
            LanguageMatch::Page(langs) => format!(
                "{}:IN [{}]",
                usnm_core::page_language::FIELD,
                langs.join(" ")
            ),
            LanguageMatch::NotPageSets(sets) => format!(
                "NOT {}:IN [{}]",
                usnm_core::page_language::SET_FIELD,
                sets.join(" ")
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn sets() -> PageLanguageSets {
        PageLanguageSets::new([
            (langs(&["eng"]), 900),
            (langs(&["eng", "fre"]), 30),
            (langs(&["fre"]), 40),
            (langs(&["ger"]), 25),
            (langs(&["dak", "eng"]), 5),
        ])
    }

    #[test]
    fn the_smaller_side() {
        let s = sets();
        assert_eq!(s.pages_in("eng"), 935);
        assert_eq!(s.pages_in("fre"), 70);
        // English: the complement (65 pages) is smaller.
        let eng = s.matching(&langs(&["eng"]));
        assert_eq!(
            eng,
            LanguageMatch::NotPageSets(langs(&["fre", "ger", "none"]))
        );
        assert_eq!(eng.clause(), "NOT page_language_set:IN [fre ger none]");
        // German: the match side.
        let ger = s.matching(&langs(&["ger"]));
        assert_eq!(ger, LanguageMatch::Page(langs(&["ger"])));
        assert_eq!(ger.clause(), "page_language:IN [ger]");
        // Told to, always the match side.
        let forced = sets().with_side(PageLanguageSide::Match);
        assert_eq!(
            forced.matching(&langs(&["eng"])),
            LanguageMatch::Page(langs(&["eng"]))
        );
        assert_eq!(
            LanguageMatch::Title(langs(&["eng", "ger"])).clause(),
            "language:IN [eng ger]"
        );
    }

    #[test]
    fn a_set_with_an_odd_code_is_never_quoted() {
        let s = PageLanguageSets::new([(langs(&["eng"]), 900), (langs(&["x-1"]), 1)]);
        assert_eq!(
            s.matching(&langs(&["eng"])),
            LanguageMatch::Page(langs(&["eng"]))
        );
    }

    #[test]
    fn both_sides_select_the_same_pages() {
        let s = sets();
        let pages: Vec<(Vec<String>, Option<String>)> = s
            .sets()
            .iter()
            .map(|(l, _)| (l.clone(), Some(set_key(l))))
            .chain([(Vec::new(), Some(NO_LANGUAGE.to_owned()))])
            .collect();
        for want in [&["eng"][..], &["fre"], &["ger", "fre"], &["dak"], &["spa"]] {
            let want = langs(want);
            let a = LanguageMatch::Page(want.clone());
            let b = match s.matching(&want) {
                LanguageMatch::NotPageSets(sets) => LanguageMatch::NotPageSets(sets),
                // Render the complement anyway.
                _ => {
                    let mut keys: Vec<String> = s
                        .sets()
                        .iter()
                        .filter(|(l, _)| !l.iter().any(|x| want.contains(x)))
                        .map(|(l, _)| set_key(l))
                        .collect();
                    keys.push(NO_LANGUAGE.to_owned());
                    LanguageMatch::NotPageSets(keys)
                }
            };
            for (page, set) in &pages {
                assert_eq!(
                    a.matches(&[], page, set.as_deref()),
                    b.matches(&[], page, set.as_deref()),
                    "{want:?} {page:?}"
                );
            }
        }
    }
}
