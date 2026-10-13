//! The JP2 against IIIF JPEG assessment (#456) as a BOINC app, `usnm_assess`.
//!
//! - [`sample`] draws a stratified random sample from the prefetch manifest.
//! - [`stage`] fetches each page's inputs from LoC, one request at a time,
//!   and writes one unit directory per page.
//! - [`submit`] hands staged units to stock `create_work`, two results each.
//! - [`work`] runs one unit on a volunteer host.
//! - [`validate`] is what stock `script_validator` calls.
//! - [`collect`] turns canonical results into the files the report's
//!   `score.py` and `aggregate.py` read.
use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod collect;
pub mod sample;
pub mod stage;
pub mod submit;
pub mod validate;
pub mod work;

/// The unit's request in the BOINC slot. Its presence makes `usnm-boinc work`
/// run an assessment unit, so one worker image serves both apps.
pub const JOB_FILE: &str = "assess.json";
pub const RESULT_FILE: &str = "result.json";
pub const APP: &str = "usnm_assess";

/// One page of the sample, as `sample` writes it and `stage` reads it.
/// Fields in alphabetical order, so the JSON keys are too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleRow {
    pub batch: String,
    pub batch_version: u16,
    /// Pages in the stratum / (OCR pages + crash pages drawn from it): the
    /// weight for crash rates, which every unit measures.
    pub crash_weight: f64,
    /// `low` (< 0.1), `mid` (< 0.5) or `high`: LoC's damage rate, binned.
    pub damage: String,
    pub damage_rate: f64,
    pub decade: i32,
    pub doc_id: String,
    /// ISO 639-3, as the production worker's message carries it.
    pub lang: String,
    pub mode: usnm_ocr_contract::assess::Mode,
    /// The batch name up to its first `_`: the awardee (`mdu`).
    pub prefix: String,
    /// Pages in the corpus stratum.
    pub stratum_pages: usize,
    /// Pages in the stratum / OCR pages drawn from it: the weight for
    /// quality and speed. On a crash row it equals `crash_weight`.
    pub weight: f64,
    pub year: i32,
}

/// Where a mode's units live under the staging root.
pub fn units(root: &Path, mode: usnm_ocr_contract::assess::Mode) -> PathBuf {
    root.join(match mode {
        usnm_ocr_contract::assess::Mode::Ocr => "ocr",
        usnm_ocr_contract::assess::Mode::Crash => "crash",
    })
}

pub fn read_sample(path: &Path) -> anyhow::Result<Vec<SampleRow>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {path:?}"))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).with_context(|| format!("sample line {}", i + 1)))
        .collect()
}

/// A doc id safe to use as a directory name.
pub fn check_doc_id(doc_id: &str) -> anyhow::Result<()> {
    ensure!(
        !doc_id.is_empty()
            && doc_id.len() <= 128
            && doc_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "invalid doc id {doc_id:?}"
    );
    Ok(())
}

/// A hold written when LoC refused or failed a request. Staging will not
/// start again until a person has looked and removed it.
pub fn hold_path(root: &Path) -> PathBuf {
    root.join("HOLD")
}
pub fn check_no_hold(root: &Path) -> anyhow::Result<()> {
    let hold = hold_path(root);
    if hold.exists() {
        bail!(
            "{} exists: LoC refused or failed a request ({}). Check before removing it.",
            hold.display(),
            std::fs::read_to_string(&hold).unwrap_or_default().trim()
        );
    }
    Ok(())
}
