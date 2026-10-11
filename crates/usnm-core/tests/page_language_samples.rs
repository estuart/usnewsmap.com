//! The page-language detector on text of known language (05 §5.5.8, #289):
//! `fixtures/page-language/samples.jsonl`, Wikipedia articles and Wikisource
//! texts in each catalog language with a word list, about 300 words each
//! (`scripts/page-language-samples.py`; `fixtures/page-language/README.md`).
//! The parity test checks the port against Python; this checks both against
//! the language a text is actually in.
//!
//! Each text is detected as its title's only language, and against every
//! other language with a word list, in both orders, as a two-language
//! title's page would be. Each must give the sample's own language, except
//! where the sample's `expect` records another result and why: a pair the
//! detector can't tell apart on that text. An `expect` entry the detector no
//! longer matches fails too, so any change in behaviour shows.
//!
//! Behind `#[ignore]`, so the `rust` CI job doesn't run it: the `page
//! language samples` job does, when the detector, its word lists or these
//! samples change (`cargo test -p usnm-core --test page_language_samples --
//! --ignored`).

use std::collections::BTreeMap;

use serde_json::Value;
use usnm_core::lang;
use usnm_core::page_language::{self, Decision, Detection};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../fixtures/page-language/samples.jsonl"
);

/// Languages with fewer than three samples of a source: Wikisource has too
/// few dated, public-domain prose texts in them (the fixture's README).
const FEWER: &[(&str, &str, usize)] = &[
    ("haw", "wikisource", 0),
    ("ice", "wikisource", 1),
    ("lav", "wikisource", 2),
    ("lit", "wikisource", 2),
    ("slo", "wikisource", 1),
    ("tgl", "wikisource", 1),
    ("yid", "wikisource", 1),
];

fn samples() -> Vec<Value> {
    let text = std::fs::read_to_string(FIXTURE).expect("the samples fixture");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("JSON"))
        .collect()
}

/// Every catalog language with a word list (MARC codes, sorted).
fn with_lists() -> Vec<&'static str> {
    lang::LANGUAGES
        .iter()
        .filter(|l| l.wordlist.is_some())
        .map(|l| l.marc)
        .collect()
}

/// A detection as the fixture writes an expected result: the language,
/// `und`, or `mixed` and its two languages.
fn result(d: &Detection) -> String {
    match d.decision {
        Decision::Detected => d.lang.clone(),
        Decision::Und => "und".to_owned(),
        Decision::Mixed => format!(
            "mixed {} {}",
            d.lang,
            d.mixed_with.as_deref().unwrap_or("?")
        ),
    }
}

fn str_of<'a>(s: &'a Value, key: &str) -> &'a str {
    s[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} in {}", s["title"]))
}

/// A sample's recorded exceptions: candidate languages, in order, to the
/// result expected for them.
fn expected(s: &Value) -> BTreeMap<Vec<String>, String> {
    let mut out = BTreeMap::new();
    for e in s["expect"].as_array().into_iter().flatten() {
        let langs: Vec<String> = e["languages"]
            .as_array()
            .expect("expect.languages")
            .iter()
            .map(|l| l.as_str().expect("a code").to_owned())
            .collect();
        assert!(
            e["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{}: an expect entry without a reason",
            s["title"]
        );
        let got = e["result"].as_str().expect("expect.result").to_owned();
        assert!(
            out.insert(langs.clone(), got).is_none(),
            "{}: {langs:?} expected twice",
            s["title"]
        );
    }
    out
}

#[test]
#[ignore = "ground truth: run with --ignored (CI's page language samples job)"]
fn the_fixture_covers_every_word_list() {
    let samples = samples();
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for s in &samples {
        let language = str_of(s, "language");
        let source = str_of(s, "source");
        assert_eq!(
            lang::wordlist_code(language),
            Some(str_of(s, "wordlist")),
            "{}",
            s["title"]
        );
        assert!(["wikipedia", "wikisource"].contains(&source), "{source}");
        let words = str_of(s, "text").split_whitespace().count();
        assert!(words >= 200, "{}: {words} words", s["title"]);
        if source == "wikisource" {
            let year = s["year"].as_u64().expect("year");
            assert!(year <= 1930, "{}: {year}, after 1930", s["title"]);
        }
        *counts.entry((language, source)).or_default() += 1;
    }
    for language in with_lists() {
        for source in ["wikipedia", "wikisource"] {
            let n = counts.get(&(language, source)).copied().unwrap_or(0);
            let want = FEWER
                .iter()
                .find(|f| (f.0, f.1) == (language, source))
                .map_or(3, |f| f.2);
            assert_eq!(n, want, "{language} {source}");
        }
    }
}

#[test]
#[ignore = "ground truth: run with --ignored (CI's page language samples job)"]
fn each_sample_is_found_in_its_language() {
    let samples = samples();
    let others = with_lists();
    let mut wrong = Vec::new();
    let mut checked = 0;
    for s in &samples {
        let language = str_of(s, "language");
        let words = page_language::text_words(str_of(s, "text")).words;
        let mut expect = expected(s);
        let mut candidates: Vec<Vec<&str>> = vec![vec![language]];
        for &other in others.iter().filter(|&&o| o != language) {
            candidates.push(vec![language, other]);
            candidates.push(vec![other, language]);
        }
        for langs in candidates {
            let key: Vec<String> = langs.iter().map(|l| (*l).to_owned()).collect();
            let want = expect.remove(&key).unwrap_or_else(|| language.to_owned());
            let got = result(&page_language::detect_words(&words, &langs));
            checked += 1;
            if got != want {
                wrong.push(format!(
                    "{language} {} {:?} as {langs:?}: {got}, expected {want}",
                    str_of(s, "source"),
                    str_of(s, "title")
                ));
            }
        }
        for (langs, want) in expect {
            wrong.push(format!(
                "{language} {:?}: expect {langs:?} -> {want} isn't a candidate list this test runs",
                str_of(s, "title")
            ));
        }
    }
    for w in &wrong {
        eprintln!("{w}");
    }
    assert!(wrong.is_empty(), "{} of {checked} detections", wrong.len());
}
