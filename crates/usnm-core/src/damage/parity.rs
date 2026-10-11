//! Parity with `ja-ocr/damage_v2.py` (12 §12.5 Step 5): pages scored in
//! Python, read back and scored again here.
//!
//! A parity file is JSON lines, zstd-compressed by its writer
//! (`ja-ocr/damage_parity.py`). The first line is `{"header": {...}}`,
//! which holds the sha256 of `damage_v2.py` and of the local word lists;
//! each other line is one page
//! ([`Record`]): its text, its title's languages, `score_v2`'s output, the
//! damage counts `detect` keeps, and the forced-language score for each of
//! the title's languages and English. A page agrees when the language,
//! decision, second language and script are the same, the token, word,
//! head, near-miss and function-word counts are equal, and every rate is
//! within [`TOLERANCE`] (Python writes them at full precision).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{score, score_forced, Damage, ForcedScore, PageScore};
use crate::lang;

/// How far a rate may be from Python's.
pub const TOLERANCE: f64 = 1e-9;

/// The file's first line.
#[derive(Debug, Clone, Deserialize)]
pub struct Header {
    /// sha256 of the `damage_v2.py` that scored the pages.
    pub damage_v2_sha256: String,
    /// sha256 of the local word lists it read (`ja-ocr/wordlists/<code>.txt`), by code.
    #[serde(default)]
    pub wordlists_sha256: BTreeMap<String, String>,
    /// The wordfreq version its word lists came from.
    pub wordfreq: String,
    /// Where the text came from (`curated`, `loc.gov`, `synthetic`...).
    #[serde(default)]
    pub source: String,
    /// The number of pages that follow.
    pub pages: usize,
}

/// `score_v2`'s output for a page.
#[derive(Debug, Clone, Deserialize)]
pub struct PyScore {
    pub lang: String,
    pub decision: String,
    pub mixed_with: Option<String>,
    pub script: String,
    pub function_share: Option<f64>,
    pub damage_rate: Option<f64>,
    pub garbage_share: Option<f64>,
    pub chars: usize,
    pub tokens: usize,
    pub words: usize,
}

/// `detect`'s damage counts.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PyDamage {
    pub words: u64,
    pub function: u64,
    pub head: u64,
    pub near: u64,
}

/// `forced`'s output.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct PyForced {
    pub tokens: usize,
    pub words: u64,
    pub head: u64,
    pub near: u64,
    pub function: u64,
    pub function_share: Option<f64>,
    pub damage_rate: Option<f64>,
    pub known_words: u64,
}

/// One page, as Python scored it.
#[derive(Debug, Clone, Deserialize)]
pub struct Record {
    pub doc_id: String,
    /// The title's catalog languages (MARC codes).
    pub languages: Vec<String>,
    pub text: String,
    pub score: PyScore,
    pub damage: Option<PyDamage>,
    /// The forced-language score of each title language and English, by
    /// MARC code (Rust scores it by the ISO 639-3 code); null where the
    /// language has no word list.
    pub forced: BTreeMap<String, Option<PyForced>>,
}

#[derive(Deserialize)]
struct HeaderLine {
    header: Header,
}

/// A parity file that can't be read.
#[derive(Debug, thiserror::Error)]
pub enum ParityError {
    #[error("the first line isn't a header: {0}")]
    Header(serde_json::Error),
    #[error("line {line}: {source}")]
    Record {
        line: usize,
        source: serde_json::Error,
    },
    #[error("the header says {want} pages, the file has {got}")]
    Count { want: usize, got: usize },
}

/// Parse a parity file's lines (decompressed).
pub fn parse(text: &str) -> Result<(Header, Vec<Record>), ParityError> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: HeaderLine =
        serde_json::from_str(lines.next().unwrap_or("")).map_err(ParityError::Header)?;
    let records = lines
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).map_err(|source| ParityError::Record {
                line: i + 2,
                source,
            })
        })
        .collect::<Result<Vec<Record>, _>>()?;
    if records.len() != header.header.pages {
        return Err(ParityError::Count {
            want: header.header.pages,
            got: records.len(),
        });
    }
    Ok((header.header, records))
}

fn close(field: &str, py: Option<f64>, rs: Option<f64>, out: &mut Vec<String>) {
    let same = match (py, rs) {
        (None, None) => true,
        (Some(a), Some(b)) => (a - b).abs() <= TOLERANCE,
        _ => false,
    };
    if !same {
        out.push(format!("{field}: python {py:?}, rust {rs:?}"));
    }
}

fn equal<T: PartialEq + std::fmt::Debug>(field: &str, py: T, rs: T, out: &mut Vec<String>) {
    if py != rs {
        out.push(format!("{field}: python {py:?}, rust {rs:?}"));
    }
}

fn compare_score(py: &PyScore, pyd: Option<&PyDamage>, rs: &PageScore, out: &mut Vec<String>) {
    equal("lang", py.lang.as_str(), rs.lang.as_str(), out);
    equal("decision", py.decision.as_str(), rs.decision.as_str(), out);
    equal(
        "mixed_with",
        py.mixed_with.as_deref(),
        rs.mixed_with.as_deref(),
        out,
    );
    equal("script", py.script.as_str(), rs.script, out);
    equal("chars", py.chars, rs.chars, out);
    equal("tokens", py.tokens, rs.tokens, out);
    equal("words", py.words, rs.words, out);
    close("function_share", py.function_share, rs.function_share, out);
    close("damage_rate", py.damage_rate, rs.damage_rate, out);
    close("garbage_share", py.garbage_share, rs.garbage_share, out);
    let counts = |d: Damage| (d.words, d.function, d.head, d.near);
    equal(
        "damage (words, function, head, near)",
        pyd.map(|d| (d.words, d.function, d.head, d.near)),
        rs.damage.map(counts),
        out,
    );
}

fn compare_forced(
    marc: &str,
    py: Option<&PyForced>,
    rs: Option<&ForcedScore>,
    out: &mut Vec<String>,
) {
    match (py, rs) {
        (None, None) => {}
        (Some(py), Some(rs)) => {
            let d = rs.damage;
            equal(
                &format!("forced {marc} (tokens, words, function, head, near, known)"),
                (
                    py.tokens,
                    py.words,
                    py.function,
                    py.head,
                    py.near,
                    py.known_words,
                ),
                (
                    rs.tokens,
                    d.words,
                    d.function,
                    d.head,
                    d.near,
                    rs.known_words,
                ),
                out,
            );
            close(
                &format!("forced {marc} function_share"),
                py.function_share,
                rs.function_share,
                out,
            );
            close(
                &format!("forced {marc} damage_rate"),
                py.damage_rate,
                rs.damage_rate,
                out,
            );
        }
        (py, rs) => out.push(format!(
            "forced {marc}: python {}, rust {}",
            if py.is_some() { "scored" } else { "none" },
            if rs.is_some() { "scored" } else { "none" }
        )),
    }
}

/// Score one page here and compare: the differences, empty when it agrees.
pub fn compare(r: &Record) -> Vec<String> {
    let mut out = Vec::new();
    let languages: Vec<&str> = r.languages.iter().map(String::as_str).collect();
    compare_score(
        &r.score,
        r.damage.as_ref(),
        &score(&r.text, &languages),
        &mut out,
    );
    // Every title language and English has a forced score (null without a word list).
    for marc in languages.iter().copied().chain(std::iter::once("eng")) {
        if !r.forced.contains_key(marc) {
            out.push(format!("forced {marc}: missing from the file"));
        }
    }
    for (marc, py) in &r.forced {
        let rs = lang::marc_to_iso3(marc).and_then(|iso3| score_forced(&r.text, iso3));
        compare_forced(marc, py.as_ref(), rs.as_ref(), &mut out);
    }
    out
}

/// The result of checking every page of a file.
#[derive(Debug, Default)]
pub struct Report {
    pub pages: usize,
    pub agree: usize,
    /// Pages that differ: doc_id and what differs.
    pub differ: Vec<(String, Vec<String>)>,
}

/// Check every record.
pub fn check(records: &[Record]) -> Report {
    let mut report = Report::default();
    for r in records {
        report.pages += 1;
        let diff = compare(r);
        if diff.is_empty() {
            report.agree += 1;
        } else {
            report.differ.push((r.doc_id.clone(), diff));
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(forced: serde_json::Value) -> Record {
        let s = score("", &["cho"]);
        serde_json::from_value(serde_json::json!({
            "doc_id": "d1", "languages": ["cho"], "text": "",
            "score": {"lang": s.lang, "decision": s.decision.as_str(), "mixed_with": null, "script": s.script,
                      "function_share": null, "damage_rate": null, "garbage_share": null,
                      "chars": 0, "tokens": 0, "words": 0},
            "damage": null, "forced": forced,
        }))
        .unwrap()
    }

    #[test]
    fn every_forced_score_must_be_there() {
        let forced = score_forced("", "eng").unwrap();
        let eng = serde_json::json!({"tokens": 0, "words": 0, "head": 0, "near": 0, "function": 0,
                                     "function_share": null, "damage_rate": null, "known_words": 0});
        assert_eq!(forced.tokens, 0);
        assert!(compare(&record(serde_json::json!({"cho": null, "eng": eng}))).is_empty());
        let diff = compare(&record(serde_json::json!({"cho": null})));
        assert_eq!(diff, ["forced eng: missing from the file"]);
        assert!(serde_json::from_value::<Record>(serde_json::json!({"doc_id": "d"})).is_err());
    }
}
