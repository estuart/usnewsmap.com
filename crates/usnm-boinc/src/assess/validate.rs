//! What stock `script_validator` runs for `usnm_assess`.
//!
//! `script_validator` passes a result's output files in output template
//! order (`text-jp2.txt`, `text-jpeg.txt`, `result.json`) and reads the
//! exit status: 0 for a valid result or an equivalent pair. `check` looks at
//! one result alone; `compare` at two results of the same unit. An engine
//! crash is a valid result; two hosts must agree on it.
use super::RESULT_FILE;
use crate::read;
use anyhow::{ensure, Context};
use std::{collections::BTreeMap, path::PathBuf};
use usnm_ocr_contract::assess::{self, Format, ResultManifest, MAX_JSON, MAX_TEXT};

pub struct Loaded {
    pub result: ResultManifest,
    pub texts: BTreeMap<Format, Vec<u8>>,
}

/// Reads and checks one result from its three files.
pub fn load(files: &[PathBuf]) -> anyhow::Result<Loaded> {
    ensure!(
        files.len() == 3,
        "expected 3 output files (text-jp2.txt, text-jpeg.txt, {RESULT_FILE}), got {}",
        files.len()
    );
    let texts = BTreeMap::from([
        (Format::Jp2, read(&files[0], MAX_TEXT)?),
        (Format::Jpeg, read(&files[1], MAX_TEXT)?),
    ]);
    let result: ResultManifest =
        serde_json::from_slice(&read(&files[2], MAX_JSON)?).context("result.json")?;
    result.check(&texts)?;
    if result.mode == assess::Mode::Crash {
        ensure!(
            texts[&Format::Jpeg].is_empty(),
            "crash unit returned JPEG text"
        );
    }
    Ok(Loaded { result, texts })
}

pub fn check(files: &[PathBuf]) -> anyhow::Result<()> {
    load(files).map(|_| ())
}

pub fn compare(a: &[PathBuf], b: &[PathBuf]) -> anyhow::Result<()> {
    let (a, b) = (load(a)?, load(b)?);
    assess::agree(&a.result, &a.texts, &b.result, &b.texts).map_err(|e| anyhow::anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_ocr_contract::{
        assess::{Outcome, Run, VERSION},
        boinc::sha256,
    };

    fn write(dir: &std::path::Path, tag: &str, jp2: Outcome, text: &str) -> Vec<PathBuf> {
        let run = |format, outcome, t: &str| Run {
            format,
            outcome,
            code: if outcome == Outcome::Ok {
                Some(0)
            } else {
                Some(136)
            },
            signal: None,
            engine_ms: 5,
            peak_rss_kib: None,
            text_sha256: sha256(t.as_bytes()),
            text_bytes: t.len() as u64,
            stderr_tail: None,
        };
        let jp2_text = if jp2 == Outcome::Ok { text } else { "" };
        let r = ResultManifest {
            v: VERSION,
            job_id: "e".repeat(64),
            mode: assess::Mode::Ocr,
            engine: crate::assess::work::tests::job(assess::Mode::Ocr).engine,
            runs: vec![
                run(Format::Jp2, jp2, jp2_text),
                run(Format::Jpeg, Outcome::Ok, text),
            ],
            completed_at: "2026-10-13T00:00:00Z".into(),
        };
        let files = ["jp2", "jpeg", "json"].map(|n| dir.join(format!("{tag}_{n}")));
        std::fs::write(&files[0], jp2_text).unwrap();
        std::fs::write(&files[1], text).unwrap();
        std::fs::write(&files[2], serde_json::to_vec(&r).unwrap()).unwrap();
        files.to_vec()
    }

    #[test]
    fn crash_results_validate_and_must_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a", Outcome::Crash136, "some words here");
        let b = write(tmp.path(), "b", Outcome::Crash136, "some words here");
        let c = write(tmp.path(), "c", Outcome::Ok, "some words here");
        check(&a).unwrap();
        compare(&a, &b).unwrap();
        assert!(compare(&a, &c).is_err());
        std::fs::write(&a[1], "tampered").unwrap();
        assert!(check(&a).is_err());
    }
}
