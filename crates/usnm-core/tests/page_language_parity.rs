//! The page-language detector's Rust port against `ja-ocr/damage_v2.py`'s
//! `detect` (05 §5.5.6, #289): every page of the damage metric's parity
//! fixture (`tests/data/damage-parity.jsonl.zst`: pages from loc.gov's text
//! service and made-up pages for the known traps between Python's string
//! handling and Rust's, scored in Python by `ja-ocr/damage_parity.py`) must
//! get the same language, decision, second language, script, runner-up and
//! shares here.

use serde_json::Value;
use sha2::{Digest, Sha256};
use usnm_core::page_language::{self, Decision};

/// Python rounds the shares to 4 places.
const SHARE_TOLERANCE: f64 = 0.5e-4 + 1e-12;

fn fixture() -> (Value, Vec<Value>) {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/damage-parity.jsonl.zst"
    );
    let bytes = std::fs::read(path).expect("the parity fixture");
    let text = String::from_utf8(zstd::decode_all(&bytes[..]).expect("zstd")).expect("UTF-8");
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: Value = serde_json::from_str(lines.next().expect("a header")).expect("JSON");
    let records = lines
        .map(|l| serde_json::from_str(l).expect("JSON"))
        .collect();
    (header["header"].clone(), records)
}

fn sha256(path: &str) -> String {
    let digest = Sha256::digest(std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}")));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn languages(r: &Value) -> Vec<&str> {
    r["languages"]
        .as_array()
        .expect("languages")
        .iter()
        .map(|l| l.as_str().expect("a code"))
        .collect()
}

#[test]
fn fixture_was_scored_by_this_damage_v2_py() {
    let (header, records) = fixture();
    let ja_ocr = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ja-ocr");
    assert_eq!(
        header["damage_v2_sha256"].as_str(),
        Some(sha256(&format!("{ja_ocr}/damage_v2.py")).as_str()),
        "damage_v2.py changed since the fixture was scored: python3 ja-ocr/damage_parity.py --rescore"
    );
    assert_eq!(header["wordfreq"].as_str(), Some("3.1.1"));
    assert_eq!(header["pages"].as_u64(), Some(records.len() as u64));
}

/// Differences between a page's detection here and Python's.
fn differences(r: &Value) -> Vec<String> {
    let py = &r["score"];
    let got = page_language::detect(r["text"].as_str().expect("text"), &languages(r));
    let mut out = Vec::new();
    let mut same = |what: &str, ours: Value, theirs: &Value| {
        if &ours != theirs {
            out.push(format!("{what}: {ours} here, {theirs} in Python"));
        }
    };
    same("lang", got.lang.clone().into(), &py["lang"]);
    same("decision", got.decision.as_str().into(), &py["decision"]);
    same(
        "mixed_with",
        got.mixed_with.clone().into(),
        &py["mixed_with"],
    );
    same("script", got.script.into(), &py["script"]);
    same("runner_up", got.runner_up.clone().into(), &py["runner_up"]);
    for (what, ours, theirs) in [
        ("share", got.share, &py["share"]),
        (
            "runner_up_share",
            got.runner_up_share,
            &py["runner_up_share"],
        ),
    ] {
        let agree = match (ours, theirs.as_f64()) {
            (None, None) => theirs.is_null(),
            (Some(a), Some(b)) => (a - b).abs() <= SHARE_TOLERANCE,
            _ => false,
        };
        if !agree {
            out.push(format!("{what}: {ours:?} here, {theirs} in Python"));
        }
    }
    out
}

#[test]
fn every_page_agrees() {
    let (_, records) = fixture();
    assert!(records.len() >= 60, "{} pages", records.len());
    let mut differ = 0;
    for r in &records {
        let diff = differences(r);
        if !diff.is_empty() {
            differ += 1;
            eprintln!("{}:\n  {}", r["doc_id"], diff.join("\n  "));
        }
    }
    assert_eq!(differ, 0, "{differ} of {} pages differ", records.len());
}

#[test]
fn fixture_covers_the_decisions_and_scripts() {
    let (_, records) = fixture();
    let found: Vec<(Decision, &str)> = records
        .iter()
        .map(|r| {
            let d = page_language::detect(r["text"].as_str().unwrap(), &languages(r));
            (d.decision, d.script)
        })
        .collect();
    for d in [Decision::Detected, Decision::Mixed, Decision::Und] {
        assert!(found.iter().any(|f| f.0 == d), "no {d:?} page");
    }
    for s in ["latin", "hebrew", "cyrillic", "greek"] {
        assert!(found.iter().any(|f| f.1 == s), "no {s} page");
    }
}

/// The detector's time per page and per 1,000 words on the fixture (05
/// §5.5.6's cost): `cargo test --release -p usnm-core --test
/// page_language_parity -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement, not a check"]
fn time_per_page() {
    let (_, records) = fixture();
    let pages: Vec<(&str, Vec<&str>)> = records
        .iter()
        .map(|r| (r["text"].as_str().unwrap(), languages(r)))
        .collect();
    let words: usize = pages
        .iter()
        .map(|(t, _)| page_language::text_words(t).words.len())
        .sum();
    let rounds = 20;
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        for (text, langs) in &pages {
            std::hint::black_box(page_language::detect(text, langs));
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let n = (rounds * pages.len()) as f64;
    eprintln!(
        "{} pages, {words} words: {:.0} µs a page, {:.0} µs per 1,000 words",
        pages.len(),
        secs / n * 1e6,
        secs / (rounds * words) as f64 * 1e9
    );
}
