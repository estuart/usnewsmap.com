//! The OCR damage metric, version 2 (12 §12.5 Step 5, #296): a port of
//! `ja-ocr/damage_v2.py`'s `score_v2` and of the forced-language score the
//! OCR engine survey uses.
//!
//! A page's language is found among its title's catalog languages plus
//! English, by function words (each language's 100 most frequent words of
//! 2 letters or more); a page is then scored against it. The damage rate is
//! near-misses / (near-misses + occurrences of the language's 20 most
//! frequent words), where a near-miss is one OCR edit from one of those 20
//! and not a real word (in the top 5,000 of the language, or of the other
//! candidate languages). `quality.py`'s docstring and the language report
//! (`docs/reports/2026-10-language-analysis/report.md`) define the metric
//! in full. `parity` checks this port against `damage_v2.py`'s numbers.
//!
//! Language codes in and out are the catalog's MARC codes, as in
//! `quality.py`; [`score_forced`] takes ISO 639-3 (`usnm_core::lang`).

mod model;
pub mod parity;
mod text;

use std::collections::{BTreeMap, HashMap};

pub use model::{model, script, scripts_of, LangModel};
pub use text::{text_words, TextWords};

use crate::lang;

/// The metric's version, in the tables it writes.
pub const VERSION: u32 = 2;
/// A page above this damage rate is damaged.
pub const DAMAGED: f64 = 0.1;
/// A page above this damage rate is badly damaged.
pub const BADLY_DAMAGED: f64 = 0.25;
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
/// Languages with words of no vowel (Czech "smrt"): the no-vowel rule would flag real words.
const SYLLABIC_CONSONANTS: &[&str] = &["cze", "slo", "hrv", "srp", "slv"];

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

/// Counts of a page's words against a language (`damage_v2.py`'s `Damage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Damage {
    /// Word tokens scored.
    pub words: u64,
    /// Of those, function words (the top 100).
    pub function: u64,
    /// Head words (the top 20).
    pub head: u64,
    /// Near-misses of the head words.
    pub near: u64,
}

impl Damage {
    /// Count `words` against `model`; a near-miss that is a real word in one
    /// of `others` doesn't count.
    fn add<'a>(
        &mut self,
        model: &LangModel,
        words: impl IntoIterator<Item = &'a str>,
        others: &[&LangModel],
    ) {
        for w in words {
            self.words += 1;
            if model.top.contains(w) {
                self.function += 1;
                if model.is_head(w) {
                    self.head += 1;
                }
            } else if model.near.contains_key(w) && !others.iter().any(|o| o.real.contains(w)) {
                self.near += 1;
            }
        }
    }

    fn merge(&mut self, other: &Damage) {
        self.words += other.words;
        self.function += other.function;
        self.head += other.head;
        self.near += other.near;
    }

    /// The share of the words that are function words.
    pub fn function_share(&self) -> Option<f64> {
        (self.words > 0).then(|| self.function as f64 / self.words as f64)
    }

    /// near / (near + head).
    pub fn damage_rate(&self) -> Option<f64> {
        let n = self.near + self.head;
        (n > 0).then(|| self.near as f64 / n as f64)
    }
}

fn damage<'a>(
    model: &LangModel,
    words: impl IntoIterator<Item = &'a str>,
    others: &[&LangModel],
) -> Damage {
    let mut d = Damage::default();
    d.add(model, words, others);
    d
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
    model: &'static LangModel,
    /// Of the word tokens, the share in the language's top 100.
    share: f64,
    /// Distinct top-100 words seen.
    types: usize,
}

fn fits<'a>(
    cands: &[(&'a str, &'static LangModel)],
    counts: &HashMap<&str, usize>,
    n: usize,
) -> Vec<Fit<'a>> {
    let mut out: Vec<Fit<'a>> = cands
        .iter()
        .map(|&(lang, model)| {
            let (mut hits, mut types) = (0usize, 0usize);
            for (w, c) in counts {
                if model.top.contains(*w) {
                    hits += c;
                    types += 1;
                }
            }
            let share = if n > 0 { hits as f64 / n as f64 } else { 0.0 };
            Fit {
                lang,
                model,
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
        let (a, b) = (&first.model.top, &second.model.top);
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

/// A page's language and its damage against it (`damage_v2.py`'s `Language`).
#[derive(Debug, Clone, PartialEq)]
pub struct Language {
    /// A catalog language code, or `und`.
    pub lang: String,
    pub decision: Decision,
    pub mixed_with: Option<String>,
    pub script: &'static str,
    /// Against the detected language(s); None if it has no word list.
    pub damage: Option<Damage>,
}

impl Language {
    fn und(script: &'static str) -> Self {
        Language {
            lang: UNKNOWN.to_owned(),
            decision: Decision::Und,
            mixed_with: None,
            script,
            damage: None,
        }
    }
}

/// `detect`: the page's language among its title's catalog languages (plus
/// English), and its damage against it. `words` are the page's word tokens
/// ([`text_words`]), in order.
pub fn detect(words: &[String], languages: &[&str]) -> Language {
    if words.len() < MIN_WORDS {
        return Language::und("latin");
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
                if let Some(d) = other.damage {
                    out.damage.get_or_insert_with(Damage::default).merge(&d);
                }
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
) -> Language {
    if s == "latin" {
        latin_language(words, languages, min_types)
    } else {
        other_script_language(s, words, languages, min_types)
    }
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
fn latin_language(words: &[&str], languages: &[&str], min_types: usize) -> Language {
    let mut out = Language::und("latin");
    if words.is_empty() {
        return out;
    }
    let mut cands: Vec<(&str, &'static LangModel)> = Vec::new();
    for &lang in languages.iter().chain(std::iter::once(&ENGLISH)) {
        if !scripts_of(lang).contains(&"latin") {
            continue;
        }
        if let Some(m) = model(lang) {
            if m.script == "latin" && !cands.iter().any(|(_, c)| c.code == m.code) {
                cands.push((lang, m));
            }
        }
    }
    let counts = counted(words.iter().copied());
    let fs = fits(&cands, &counts, words.len());
    let model_of = |lang: &str| {
        cands
            .iter()
            .find(|(l, _)| *l == lang)
            .map(|(_, m)| *m)
            .expect("a candidate")
    };
    let others = |lang: &str| -> Vec<&LangModel> {
        cands
            .iter()
            .filter(|(l, _)| *l != lang)
            .map(|(_, m)| *m)
            .collect()
    };

    // Windows of the page, each given a language if one clearly wins in it:
    // two languages each winning a fair share of the windows make the page
    // mixed. Each window is then scored against its own language, and
    // windows no language wins aren't scored.
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
            let mut d = Damage::default();
            for (win, w) in windows.iter().zip(&winners) {
                if let Some(w) = w {
                    d.add(model_of(w), win.iter().copied(), &others(w));
                }
            }
            out.damage = Some(d);
            return out;
        }
    }
    if let Some(best) = winner(&fs, &counts, min_types) {
        out.lang = best.to_owned();
        out.decision = Decision::Detected;
        out.damage = Some(damage(model_of(best), words.iter().copied(), &others(best)));
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
) -> Language {
    let langs: Vec<&str> = languages
        .iter()
        .copied()
        .filter(|l| scripts_of(l).contains(&s))
        .collect();
    if langs.is_empty() || words.is_empty() {
        return Language::und(s);
    }
    // Serbian in Cyrillic is scored by its Latin-script list on the words
    // transliterated; they join the counts beside the Cyrillic ones.
    let serbian: Option<Vec<String>> = (s == "cyrillic" && langs.contains(&"srp"))
        .then(|| words.iter().map(|w| serbian_latin(w)).collect());
    let modelled: Vec<(&str, &'static LangModel)> = langs
        .iter()
        .filter_map(|&lang| {
            let m = model(lang)?;
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
                return Language::und(s);
            }
        }
    };
    let rest: Vec<&LangModel> = fs
        .iter()
        .filter(|f| f.lang != best)
        .map(|f| f.model)
        .collect();
    let m = modelled
        .iter()
        .rev()
        .find(|(l, _)| *l == best)
        .map(|(_, m)| *m);
    let damage = m.map(|m| match (&serbian, best) {
        (Some(serbian), "srp") => damage(m, serbian.iter().map(String::as_str), &rest),
        _ => damage(m, words.iter().copied(), &rest),
    });
    Language {
        lang: best.to_owned(),
        decision: Decision::Detected,
        mixed_with: None,
        script: s,
        damage,
    }
}

/// A page's language and scores (`damage_v2.py`'s `score_v2`, with the
/// damage counts `detect` keeps).
#[derive(Debug, Clone, PartialEq)]
pub struct PageScore {
    /// The detected language (a catalog code), or `und`.
    pub lang: String,
    pub decision: Decision,
    pub mixed_with: Option<String>,
    pub script: &'static str,
    pub function_share: Option<f64>,
    pub damage_rate: Option<f64>,
    pub garbage_share: Option<f64>,
    /// Characters (code points) of the text.
    pub chars: usize,
    /// Whitespace tokens.
    pub tokens: usize,
    /// Word tokens.
    pub words: usize,
    /// The counts behind the rates; None when no language was scored.
    pub damage: Option<Damage>,
}

/// Score a page's text against its title's catalog languages (MARC codes)
/// plus English: `score_v2`.
pub fn score(text: &str, languages: &[&str]) -> PageScore {
    let tw = text_words(text);
    let lang = detect(&tw.words, languages);
    let first = languages.first().copied().unwrap_or(UNKNOWN);
    let g_lang = if lang.lang != UNKNOWN {
        lang.lang.as_str()
    } else {
        first
    };
    let g = if SYLLABIC_CONSONANTS.contains(&g_lang) {
        tw.garbage_syllabic
    } else {
        tw.garbage
    };
    PageScore {
        function_share: lang.damage.and_then(|d| d.function_share()),
        damage_rate: lang.damage.and_then(|d| d.damage_rate()),
        garbage_share: (tw.tokens > 0).then(|| g as f64 / tw.tokens as f64),
        chars: text.chars().count(),
        tokens: tw.tokens,
        words: tw.words.len(),
        damage: lang.damage,
        lang: lang.lang,
        decision: lang.decision,
        mixed_with: lang.mixed_with,
        script: lang.script,
    }
}

/// A page's damage against one given language, with no detection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ForcedScore {
    /// Whitespace tokens.
    pub tokens: usize,
    pub damage: Damage,
    pub function_share: Option<f64>,
    pub damage_rate: Option<f64>,
    /// Word tokens in the language's top 5,000.
    pub known_words: u64,
}

/// Score a page against one language given as ISO 639-3 (`deu`), with no
/// detection: `damage_v2.py`'s `forced`, what the OCR engine survey scores.
/// Serbian is transliterated to Latin letters; on a page not scored as
/// English, a near-miss that is a real English word doesn't count. None if
/// the code is unknown or the language has no word list.
pub fn score_forced(text: &str, iso3: &str) -> Option<ForcedScore> {
    let marc = lang::iso3_to_marc(iso3)?;
    let m = model(marc)?;
    let tw = text_words(text);
    let words: Vec<String> = if marc == "srp" {
        tw.words.iter().map(|w| serbian_latin(w)).collect()
    } else {
        tw.words
    };
    let others: Vec<&LangModel> = if marc == ENGLISH {
        Vec::new()
    } else {
        model(ENGLISH).into_iter().collect()
    };
    let d = damage(m, words.iter().map(String::as_str), &others);
    Some(ForcedScore {
        tokens: tw.tokens,
        damage: d,
        function_share: d.function_share(),
        damage_rate: d.damage_rate(),
        known_words: words.iter().filter(|w| m.real.contains(w.as_str())).count() as u64,
    })
}

#[cfg(test)]
mod tests;
