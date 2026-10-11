//! The damage metric's Rust port against `ja-ocr/damage_v2.py` (12 §12.5
//! Step 5): every page of `tests/data/damage-parity.jsonl.zst`, scored in
//! Python by `ja-ocr/damage_parity.py`, must score the same here.
#![cfg(feature = "damage")]

use sha2::{Digest, Sha256};
use usnm_core::damage::parity;

fn fixture() -> (parity::Header, Vec<parity::Record>) {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/damage-parity.jsonl.zst"
    );
    let bytes = std::fs::read(path).expect("the parity fixture");
    let text = String::from_utf8(zstd::decode_all(&bytes[..]).expect("zstd")).expect("UTF-8");
    parity::parse(&text).expect("a parity file")
}

fn sha256(path: &str) -> String {
    let digest = Sha256::digest(std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}")));
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

const RESCORE: &str = "port the change and score the fixture's pages again: \
    python3 ja-ocr/damage_parity.py --rescore crates/usnm-core/tests/data/damage-parity.jsonl.zst --out <new file>";

#[test]
fn fixture_was_scored_by_this_damage_v2_py() {
    let (header, _) = fixture();
    let ja_ocr = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ja-ocr");
    assert_eq!(
        header.damage_v2_sha256,
        sha256(&format!("{ja_ocr}/damage_v2.py")),
        "damage_v2.py changed since the fixture was built: {RESCORE}"
    );
    // The models also read our own word lists.
    assert_eq!(
        header.wordlists_sha256.keys().collect::<Vec<_>>(),
        ["haw", "yi"]
    );
    for (code, want) in &header.wordlists_sha256 {
        assert_eq!(
            want,
            &sha256(&format!("{ja_ocr}/wordlists/{code}.txt")),
            "ja-ocr/wordlists/{code}.txt changed since the fixture was built: {RESCORE}"
        );
    }
    assert_eq!(header.wordfreq, "3.1.1");
}

#[test]
fn every_page_agrees() {
    let (_, records) = fixture();
    assert!(records.len() >= 60, "{} pages", records.len());
    let report = parity::check(&records);
    for (doc_id, diff) in &report.differ {
        eprintln!("{doc_id}:\n  {}", diff.join("\n  "));
    }
    assert!(
        report.differ.is_empty(),
        "{} of {} pages differ",
        report.differ.len(),
        report.pages
    );
}

#[test]
fn fixture_covers_the_traps_and_the_decisions() {
    let (_, records) = fixture();
    let has = |f: &dyn Fn(&parity::Record) -> bool| records.iter().any(f);
    for d in ["detected", "mixed", "und"] {
        assert!(has(&|r| r.score.decision == d), "no {d} page");
    }
    for s in ["latin", "hebrew", "cyrillic", "greek"] {
        assert!(has(&|r| r.score.script == s), "no {s} page");
    }
    assert!(has(&|r| r.text.contains('ß')));
    assert!(has(&|r| r.text.contains('\u{1c}')));
    assert!(has(
        &|r| r.text.contains('\u{0301}') || r.text.contains('\u{0308}')
    ));
}
