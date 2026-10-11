//! A page's own language (05 §5.5.6, #289): a port of `quality.py`'s
//! `detect` (`ja-ocr/damage_v2.py`) and of the functions it calls, without
//! the damage half of the audit.
//!
//! A page's language is found among its title's catalog languages plus
//! English, by function words (each word list's 100 most frequent words of
//! 2 letters or more, embedded from `data/page-language-words.txt`): the
//! winner needs `MIN_SHARE` of the page's words, `PAGE_TYPES` different
//! function words, and `LEAD` times the runner-up's tokens among the
//! function words only one of the two has. A long page whose windows two
//! languages each win is `mixed`; so is one with a second script holding
//! `MIXED_SHARE` of its words in another title language. Words in a script
//! other than Latin take the title's language in that script. The language
//! report (`docs/reports/2026-10-language-analysis/report.md`) describes
//! the method; `tests/page_language_parity.rs` checks this port against
//! Python's results on real pages.
//!
//! [`page_languages`] turns the result into the languages the index records
//! for the page (05 §5.5.6), and [`set_key`] into the one keyword the
//! `lang=` filter's complement side matches (06 §6.3.1).
//!
//! Language codes in and out are the catalog's MARC codes.

mod lists;
pub(crate) mod text;

use std::collections::{BTreeMap, HashMap};

pub use lists::{function_words, script, scripts_of, FunctionWords, WordHasher, WordMap, WordSet};
pub use text::{text_words, TextWords};

/// The detector's version, and the index fields'. `current.json`'s
/// `page_language` names it; the API filters by page language only at its
/// own version. A change to the detector or its word lists bumps it.
pub const VERSION: u32 = 1;
/// The field of a page's languages (`array<text>`, raw, fast).
pub const FIELD: &str = "page_language";
/// The field of a page's languages as one keyword (`text`, raw, indexed only).
pub const SET_FIELD: &str = "page_language_set";
/// [`SET_FIELD`] for a page with no language.
pub const NO_LANGUAGE: &str = "none";
/// The language of a page no language clearly wins.
pub const UNKNOWN: &str = "und";
const ENGLISH: &str = "eng";

/// A page with fewer word tokens than this: und.
const MIN_WORDS: usize = 20;
/// The winner's function-word share must reach this,
const MIN_SHARE: f64 = 0.10;
/// with this many different function words (a page, a window),
const PAGE_TYPES: usize = 10;
const WINDOW_TYPES: usize = 5;
/// and this many times the runner-up's tokens among the function words only one of them has.
const LEAD: f64 = 2.0;
/// Word tokens per window for bilingual pages, on pages of `MIXED_MIN_WORDS` or more.
const WINDOW: usize = 50;
const MIXED_MIN_WORDS: usize = 4 * WINDOW;
/// A page's last, shorter window counts only with this many words.
const MIN_TAIL: usize = WINDOW / 2;
/// A second language winning this share of decided windows (and 2 or more) makes a page mixed.
const MIXED_SHARE: f64 = 0.2;

/// How a page's language was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// One language clearly wins.
    Detected,
    /// Two languages each win a share of the page.
    Mixed,
    /// No language clearly wins: too short, garbled, or no word list.
    Und,
}

impl Decision {
    /// As `damage_v2.py` writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Detected => "detected",
            Decision::Mixed => "mixed",
            Decision::Und => UNKNOWN,
        }
    }
}

/// A page's language (`damage_v2.py`'s `Language`, without the damage).
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// A catalog language code, or `und`.
    pub lang: String,
    pub decision: Decision,
    /// A mixed page's second language.
    pub mixed_with: Option<String>,
    /// The script of the words the language was found in.
    pub script: &'static str,
    /// The winner's function-word share (the best-fitting language's on an
    /// und page). Python rounds it to 4 places; this doesn't.
    pub share: Option<f64>,
    /// The runner-up (a mixed page's second language).
    pub runner_up: Option<String>,
    pub runner_up_share: Option<f64>,
}

impl Detection {
    fn und(script: &'static str) -> Self {
        Detection {
            lang: UNKNOWN.to_owned(),
            decision: Decision::Und,
            mixed_with: None,
            script,
            share: None,
            runner_up: None,
            runner_up_share: None,
        }
    }
}

/// The language of a page's text among its title's catalog languages (MARC
/// codes, in catalog order) plus English: `text_words`, then `detect`.
pub fn detect(text: &str, languages: &[&str]) -> Detection {
    detect_words(&text_words(text).words, languages)
}

/// `detect`: `words` are the page's word tokens ([`text_words`]), in order.
/// The script most of them are in picks the path; a title's language in
/// another script that covers `MIXED_SHARE` of the words makes the page
/// mixed.
pub fn detect_words(words: &[String], languages: &[&str]) -> Detection {
    if words.len() < MIN_WORDS {
        return Detection::und("latin");
    }
    let mut by_script: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    for w in words {
        by_script.entry(script(w)).or_default().push(w);
    }
    let main = by_script
        .iter()
        .max_by(|a, b| (a.1.len(), *a.0 == "latin", a.0).cmp(&(b.1.len(), *b.0 == "latin", b.0)))
        .map(|(s, _)| *s)
        .expect("a page of words has a script");
    let mut out = script_language(main, &by_script[main], languages, PAGE_TYPES);
    if out.decision != Decision::Detected {
        return out;
    }
    // The 20% share is the only size test for a second script's words; they
    // need as many different function words as a window does.
    let mut others: Vec<(&&str, &Vec<&str>)> = by_script.iter().collect();
    others.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));
    for (s, sw) in others {
        if *s != main && sw.len() as f64 >= MIXED_SHARE * words.len() as f64 {
            let other = script_language(s, sw, languages, WINDOW_TYPES);
            if other.decision == Decision::Detected && other.lang != out.lang {
                out.decision = Decision::Mixed;
                out.mixed_with = Some(other.lang);
                break;
            }
        }
    }
    out
}

fn script_language(
    s: &'static str,
    words: &[&str],
    languages: &[&str],
    min_types: usize,
) -> Detection {
    if s == "latin" {
        latin_language(words, languages, min_types)
    } else {
        other_script_language(s, words, languages, min_types)
    }
}

fn counted<'a>(words: impl IntoIterator<Item = &'a str>) -> HashMap<&'a str, usize> {
    let mut counts = HashMap::new();
    for w in words {
        *counts.entry(w).or_insert(0) += 1;
    }
    counts
}

struct Fit<'a> {
    lang: &'a str,
    words: &'static FunctionWords,
    /// Of the word tokens, the share that are its function words.
    share: f64,
    /// Distinct function words seen.
    types: usize,
}

fn fits<'a>(
    cands: &[(&'a str, &'static FunctionWords)],
    counts: &HashMap<&str, usize>,
    n: usize,
) -> Vec<Fit<'a>> {
    let mut out: Vec<Fit<'a>> = cands
        .iter()
        .map(|&(lang, words)| {
            let (mut hits, mut types) = (0usize, 0usize);
            for (w, c) in counts {
                if words.top.contains(*w) {
                    hits += c;
                    types += 1;
                }
            }
            let share = if n > 0 { hits as f64 / n as f64 } else { 0.0 };
            Fit {
                lang,
                words,
                share,
                types,
            }
        })
        .collect();
    out.sort_by(|a, b| b.share.total_cmp(&a.share).then_with(|| a.lang.cmp(b.lang)));
    out
}

/// The best-fitting language if it clearly wins: enough function words, of
/// enough kinds, and at least `LEAD` times the runner-up's tokens among the
/// function words only one of the two has.
fn winner<'a>(fs: &[Fit<'a>], counts: &HashMap<&str, usize>, min_types: usize) -> Option<&'a str> {
    let first = fs.first()?;
    if first.share < MIN_SHARE || first.types < min_types {
        return None;
    }
    if let Some(second) = fs.get(1) {
        let (a, b) = (&first.words.top, &second.words.top);
        let (mut only_a, mut only_b) = (0usize, 0usize);
        for (w, c) in counts {
            match (a.contains(*w), b.contains(*w)) {
                (true, false) => only_a += c,
                (false, true) => only_b += c,
                _ => {}
            }
        }
        if only_a == 0 || (only_a as f64) < LEAD * only_b as f64 {
            return None;
        }
    }
    Some(first.lang)
}

/// `windows_of`: a page of `MIXED_MIN_WORDS` or more cut into `WINDOW`-word
/// windows; a last, shorter one only if it has `MIN_TAIL` words.
fn windows_of<'a, 'b>(words: &'a [&'b str]) -> Vec<&'a [&'b str]> {
    if words.len() < MIXED_MIN_WORDS {
        return Vec::new();
    }
    let mut out: Vec<_> = words.chunks(WINDOW).collect();
    if out.last().is_some_and(|w| w.len() < MIN_TAIL) {
        out.pop();
    }
    out
}

/// Latin-script words: the title's languages with word lists, and English,
/// compete on function words.
fn latin_language(words: &[&str], languages: &[&str], min_types: usize) -> Detection {
    let mut out = Detection::und("latin");
    if words.is_empty() {
        return out;
    }
    let mut cands: Vec<(&str, &'static FunctionWords)> = Vec::new();
    for &lang in languages.iter().chain(std::iter::once(&ENGLISH)) {
        if !scripts_of(lang).contains(&"latin") {
            continue;
        }
        if let Some(m) = function_words(lang) {
            if m.script == "latin" && !cands.iter().any(|(_, c)| c.code == m.code) {
                cands.push((lang, m));
            }
        }
    }
    let counts = counted(words.iter().copied());
    let fs = fits(&cands, &counts, words.len());
    out.share = fs.first().map(|f| f.share);
    out.runner_up = fs.get(1).map(|f| f.lang.to_owned());
    out.runner_up_share = fs.get(1).map(|f| f.share);
    let share_of = |lang: &str| fs.iter().find(|f| f.lang == lang).map(|f| f.share);

    // Windows of the page, each given a language if one clearly wins in it:
    // two languages each winning a fair share of the windows make the page
    // mixed.
    let windows = windows_of(words);
    if !windows.is_empty() {
        let winners: Vec<Option<&str>> = windows
            .iter()
            .map(|win| {
                let c = counted(win.iter().copied());
                winner(&fits(&cands, &c, win.len()), &c, WINDOW_TYPES)
            })
            .collect();
        let votes = counted(winners.iter().flatten().copied());
        let decided: usize = votes.values().sum();
        let mut strong: Vec<(&str, usize)> = votes
            .iter()
            .filter(|(_, &v)| v >= 2 && v as f64 >= MIXED_SHARE * decided as f64)
            .map(|(l, v)| (*l, *v))
            .collect();
        strong.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        if strong.len() >= 2 {
            let (a, b) = (strong[0].0, strong[1].0);
            out.lang = a.to_owned();
            out.decision = Decision::Mixed;
            out.mixed_with = Some(b.to_owned());
            out.share = share_of(a);
            out.runner_up = Some(b.to_owned());
            out.runner_up_share = share_of(b);
            return out;
        }
    }
    if let Some(best) = winner(&fs, &counts, min_types) {
        out.lang = best.to_owned();
        out.decision = Decision::Detected;
    }
    out
}

/// Serbian Cyrillic in Latin letters (Vuk's Cyrillic and Gaj's Latin match
/// one to one), on case-folded words.
pub fn serbian_latin(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        let s = match c {
            'а' => "a",
            'б' => "b",
            'в' => "v",
            'г' => "g",
            'д' => "d",
            'ђ' => "đ",
            'е' => "e",
            'ж' => "ž",
            'з' => "z",
            'и' => "i",
            'ј' => "j",
            'к' => "k",
            'л' => "l",
            'љ' => "lj",
            'м' => "m",
            'н' => "n",
            'њ' => "nj",
            'о' => "o",
            'п' => "p",
            'р' => "r",
            'с' => "s",
            'т' => "t",
            'ћ' => "ć",
            'у' => "u",
            'ф' => "f",
            'х' => "h",
            'ц' => "c",
            'ч' => "č",
            'џ' => "dž",
            'ш' => "š",
            c => {
                out.push(c);
                continue;
            }
        };
        out.push_str(s);
    }
    out
}

/// Words in a script other than Latin: the title's language in that script;
/// if it lists several, the one with a word list that clearly fits best,
/// else the one without a list; else und.
fn other_script_language(
    s: &'static str,
    words: &[&str],
    languages: &[&str],
    min_types: usize,
) -> Detection {
    let langs: Vec<&str> = languages
        .iter()
        .copied()
        .filter(|l| scripts_of(l).contains(&s))
        .collect();
    if langs.is_empty() || words.is_empty() {
        return Detection::und(s);
    }
    // Serbian in Cyrillic is scored by its Latin-script list on the words
    // transliterated; they join the counts beside the Cyrillic ones.
    let serbian: Option<Vec<String>> = (s == "cyrillic" && langs.contains(&"srp"))
        .then(|| words.iter().map(|w| serbian_latin(w)).collect());
    let modelled: Vec<(&str, &'static FunctionWords)> = langs
        .iter()
        .filter_map(|&lang| {
            let m = function_words(lang)?;
            (m.script == s || (serbian.is_some() && lang == "srp")).then_some((lang, m))
        })
        .collect();
    let mut counts = counted(words.iter().copied());
    if let Some(serbian) = &serbian {
        for (w, c) in counted(serbian.iter().map(String::as_str)) {
            *counts.entry(w).or_insert(0) += c;
        }
    }
    let fs = fits(&modelled, &counts, words.len());
    let best = match winner(&fs, &counts, min_types) {
        Some(best) => best,
        None => {
            let unmodelled: Vec<&str> = langs
                .iter()
                .copied()
                .filter(|l| !modelled.iter().any(|(m, _)| m == l))
                .collect();
            if langs.len() == 1 {
                langs[0]
            } else if unmodelled.len() == 1 {
                unmodelled[0]
            } else {
                return Detection::und(s);
            }
        }
    };
    let rest = fs.iter().find(|f| f.lang != best);
    Detection {
        lang: best.to_owned(),
        decision: Decision::Detected,
        mixed_with: None,
        script: s,
        share: fs.iter().find(|f| f.lang == best).map(|f| f.share),
        runner_up: rest.map(|f| f.lang.to_owned()),
        runner_up_share: rest.map(|f| f.share),
    }
}

/// The languages the index records for a page (05 §5.5.6): the detected
/// language, or a mixed page's two; an und page, or one with no text of its
/// own (`None`), its title's languages; and every title language with no
/// word list whatever the result, since the detector can't see those (#251).
/// Sorted, without repeats.
pub fn page_languages(detection: Option<&Detection>, title_languages: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = match detection {
        Some(d) if d.decision == Decision::Detected => vec![d.lang.clone()],
        Some(d) if d.decision == Decision::Mixed => std::iter::once(d.lang.clone())
            .chain(d.mixed_with.clone())
            .collect(),
        _ => title_languages.iter().map(|l| (*l).to_owned()).collect(),
    };
    out.extend(
        title_languages
            .iter()
            .filter(|l| function_words(l).is_none())
            .map(|l| (*l).to_owned()),
    );
    out.sort_unstable();
    out.dedup();
    out
}

/// [`SET_FIELD`] for a page's languages ([`page_languages`], sorted): joined
/// by `_` (`eng`, `eng_ger`), or `none`.
pub fn set_key<S: AsRef<str>>(languages: &[S]) -> String {
    if languages.is_empty() {
        return NO_LANGUAGE.to_owned();
    }
    languages
        .iter()
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join("_")
}

/// The languages of a [`set_key`]: none for `none`.
pub fn set_languages(key: &str) -> impl Iterator<Item = &str> {
    key.split('_')
        .filter(|l| !l.is_empty() && *l != NO_LANGUAGE)
}

/// A language code the filter may put in a query: three lowercase letters.
pub fn is_code(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_lowercase())
}

#[cfg(test)]
mod tests;
