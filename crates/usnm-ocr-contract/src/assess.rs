//! File-based jobs for the JP2 against IIIF JPEG assessment (#456): one
//! page per BOINC workunit, both formats read by the same pinned engine.
//! The engine's exit is a recorded outcome, not an error: a JP2 that makes
//! Tesseract die with status 136 (SIGFPE, #455) is a result like any other.
use crate::boinc::{is_sha256, sha256, Engine};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_JSON: usize = 16 * 1024;
pub const MAX_TEXT: usize = 512 * 1024;
pub const MAX_SCAN: usize = crate::boinc::MAX_SCAN;
/// Word-bag Dice two hosts' texts of one image must reach to agree.
/// Tesseract with one thread is deterministic on one build, but the LSTM
/// can take different SIMD paths on different CPUs.
pub const MIN_TEXT_AGREEMENT: f64 = 0.98;

/// What a unit runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Both formats, to the end, at the production page timeout.
    Ocr,
    /// The JP2 only, with a short deadline: does the engine crash on it?
    Crash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Jp2,
    Jpeg,
}
impl Format {
    /// The input file's open name in the BOINC slot.
    pub fn input(self) -> &'static str {
        match self {
            Self::Jp2 => "page.jp2",
            Self::Jpeg => "page.jpg",
        }
    }
    /// The output text's open name in the BOINC slot.
    pub fn text(self) -> &'static str {
        match self {
            Self::Jp2 => "text-jp2.txt",
            Self::Jpeg => "text-jpeg.txt",
        }
    }
    /// score.py's name for the format.
    pub fn score_name(self) -> &'static str {
        match self {
            Self::Jp2 => "jp2",
            Self::Jpeg => "jpg",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

/// Where the page sits in the sample's strata, carried so results can be
/// weighted back to the corpus without the sample file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stratum {
    /// Weight for crash rates: stratum pages / all units drawn from it.
    pub crash_weight: f64,
    pub damage: String,
    pub decade: i32,
    pub lang: String,
    pub pages: usize,
    pub prefix: String,
    /// Weight for quality and speed: stratum pages / OCR units drawn.
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub v: u32,
    pub id: String,
    pub mode: Mode,
    pub doc_id: String,
    pub batch: String,
    pub batch_version: u16,
    /// ISO 639-3, as the production worker's message carries it.
    pub language: String,
    pub stratum: Stratum,
    /// Keyed by format; a crash unit holds the JP2 only.
    pub inputs: BTreeMap<Format, Input>,
    pub engine: Engine,
    /// The Tesseract image the worker image was built on (its
    /// `TESSERACT_IMAGE`), pinned by digest. The worker image carries it as
    /// `USNM_TESSERACT_IMAGE` and refuses a unit staged for another, so a
    /// change of decoder libraries inside the engine image is never mixed in.
    pub engine_base: String,
}
impl Job {
    pub fn identity(&self) -> anyhow::Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.v,
            self.mode,
            &self.doc_id,
            &self.batch,
            self.batch_version,
            &self.language,
            &self.inputs,
            &self.engine,
            &self.engine_base,
        ))?))
    }
    pub fn workunit(&self) -> String {
        format!("usnm_assess_{}", self.id)
    }
    pub fn formats(&self) -> Vec<Format> {
        self.inputs.keys().copied().collect()
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported assessment job version");
        ensure!(
            is_sha256(&self.id) && self.id == self.identity()?,
            "job identity mismatch"
        );
        let want: Vec<Format> = match self.mode {
            Mode::Ocr => vec![Format::Jp2, Format::Jpeg],
            Mode::Crash => vec![Format::Jp2],
        };
        ensure!(self.formats() == want, "inputs do not match the mode");
        for i in self.inputs.values() {
            ensure!(is_sha256(&i.sha256), "invalid input checksum");
            ensure!(
                i.bytes > 0 && i.bytes <= MAX_SCAN as u64,
                "invalid input size"
            );
            ensure!(
                i.url.starts_with("https://tile.loc.gov/")
                    && i.url.len() <= 512
                    && !i.url.chars().any(|c| c.is_control() || c.is_whitespace()),
                "invalid input URL"
            );
        }
        ensure!(
            self.engine
                .image
                .strip_prefix("sha256:")
                .is_some_and(is_sha256),
            "engine image must be a full digest"
        );
        ensure!(
            self.engine_base
                .rsplit_once("sha256:")
                .is_some_and(|(_, d)| is_sha256(d))
                && self.engine_base.len() <= 256
                && !self
                    .engine_base
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace()),
            "engine base must be pinned by full digest"
        );
        ensure!(
            self.engine.psm == 3 && self.engine.dpi == 300,
            "the assessment uses the production profile: PSM 3, DPI 300"
        );
        ensure!(
            (1..=3600).contains(&self.engine.timeout_secs),
            "invalid page deadline"
        );
        ensure!(
            !self.language.is_empty()
                && self.language.len() <= 8
                && self.language.bytes().all(|b| b.is_ascii_lowercase()),
            "invalid language"
        );
        Ok(())
    }
}

/// How one engine run ended. `Crash136` is exit status 136, or death by
/// signal 8 (SIGFPE) when the engine runs without a shell or container
/// runtime in between; production sees the first, a bare process the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Ok,
    Crash136,
    Timeout,
    Other,
}
impl Outcome {
    pub fn of(code: Option<i32>, signal: Option<i32>, timed_out: bool) -> Self {
        if timed_out {
            Self::Timeout
        } else if code == Some(0) {
            Self::Ok
        } else if code == Some(136) || signal == Some(8) {
            Self::Crash136
        } else {
            Self::Other
        }
    }
    /// For a crash unit, a run that reached its short deadline read the
    /// image; only the crash matters.
    pub fn readable(self) -> bool {
        matches!(self, Self::Ok | Self::Timeout)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub format: Format,
    pub outcome: Outcome,
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub engine_ms: u64,
    /// Highest VmHWM of the engine process, sampled while it ran.
    pub peak_rss_kib: Option<u64>,
    pub text_sha256: String,
    pub text_bytes: u64,
    /// The last bytes of the engine's stderr, for failures only.
    pub stderr_tail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultManifest {
    pub v: u32,
    pub job_id: String,
    pub mode: Mode,
    pub engine: Engine,
    pub runs: Vec<Run>,
    pub completed_at: String,
}
impl ResultManifest {
    /// Checks one host's result on its own: shape, sizes and checksums.
    /// `texts` holds the returned text files by format.
    pub fn check(&self, texts: &BTreeMap<Format, Vec<u8>>) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported result version");
        ensure!(is_sha256(&self.job_id), "invalid job id");
        chrono::DateTime::parse_from_rfc3339(&self.completed_at)?;
        let formats: Vec<Format> = self.runs.iter().map(|r| r.format).collect();
        let want: &[Format] = match self.mode {
            Mode::Ocr => &[Format::Jp2, Format::Jpeg],
            Mode::Crash => &[Format::Jp2],
        };
        ensure!(
            formats == want,
            "runs must be JP2 then JPEG for an OCR unit, JP2 alone for a crash unit"
        );
        for r in &self.runs {
            let text = texts.get(&r.format).map(Vec::as_slice).unwrap_or(b"");
            ensure!(text.len() <= MAX_TEXT, "text over size limit");
            ensure!(std::str::from_utf8(text).is_ok(), "text is not UTF-8");
            ensure!(
                r.text_sha256 == sha256(text) && r.text_bytes == text.len() as u64,
                "returned text checksum mismatch"
            );
            // The worker records a timeout with neither code nor signal.
            let consistent = if r.outcome == Outcome::Timeout {
                r.code.is_none() && r.signal.is_none()
            } else {
                r.outcome == Outcome::of(r.code, r.signal, false)
            };
            ensure!(consistent, "outcome does not match the exit status");
            ensure!(
                r.outcome == Outcome::Ok || text.is_empty(),
                "a failed run returned text"
            );
            ensure!(
                r.stderr_tail.as_ref().is_none_or(|s| s.len() <= 512),
                "stderr tail too long"
            );
        }
        Ok(())
    }
}

/// Words as score.py counts them, close enough for agreement between two
/// hosts: lower case, letters and digits only.
fn words(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for w in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        *out.entry(w.to_owned()).or_insert(0) += 1;
    }
    out
}

/// 2 * shared words / (words of both), 1.0 for two empty texts.
pub fn dice(a: &str, b: &str) -> f64 {
    let (wa, wb) = (words(a), words(b));
    let total: usize = wa.values().sum::<usize>() + wb.values().sum::<usize>();
    if total == 0 {
        return 1.0;
    }
    let shared: usize = wa
        .iter()
        .map(|(w, n)| (*n).min(*wb.get(w).unwrap_or(&0)))
        .sum();
    2.0 * shared as f64 / total as f64
}

/// Whether two hosts' results for one unit agree: same job and engine, the
/// same outcome per format (for a crash unit, crashed or not), and where
/// both read a format, texts that agree to [`MIN_TEXT_AGREEMENT`].
pub fn agree(
    a: &ResultManifest,
    a_texts: &BTreeMap<Format, Vec<u8>>,
    b: &ResultManifest,
    b_texts: &BTreeMap<Format, Vec<u8>>,
) -> Result<(), String> {
    if a.job_id != b.job_id || a.mode != b.mode || a.engine != b.engine {
        return Err("different job or engine".into());
    }
    if a.runs.len() != b.runs.len() {
        return Err("different runs".into());
    }
    let crash_unit = a.mode == Mode::Crash;
    for (ra, rb) in a.runs.iter().zip(&b.runs) {
        if ra.format != rb.format {
            return Err("different formats".into());
        }
        let same = if crash_unit {
            ra.outcome.readable() == rb.outcome.readable()
                && (ra.outcome.readable() || ra.outcome == rb.outcome)
        } else {
            ra.outcome == rb.outcome
        };
        if !same {
            return Err(format!(
                "{:?}: outcomes {:?} and {:?}",
                ra.format, ra.outcome, rb.outcome
            ));
        }
        if !crash_unit && ra.outcome == Outcome::Ok && ra.text_sha256 != rb.text_sha256 {
            let ta = String::from_utf8_lossy(a_texts.get(&ra.format).map_or(&[][..], |v| v));
            let tb = String::from_utf8_lossy(b_texts.get(&rb.format).map_or(&[][..], |v| v));
            let d = dice(&ta, &tb);
            if d < MIN_TEXT_AGREEMENT {
                return Err(format!("{:?}: text agreement {d:.4}", ra.format));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine {
            image: format!("sha256:{}", "a".repeat(64)),
            version: "5.5.0".into(),
            profile: "eng".into(),
            models: "sha256:0123456789ab".into(),
            psm: 3,
            dpi: 300,
            timeout_secs: 1200,
        }
    }
    fn run(format: Format, outcome: Outcome, text: &str) -> Run {
        let (code, signal) = match outcome {
            Outcome::Ok => (Some(0), None),
            Outcome::Crash136 => (None, Some(8)),
            Outcome::Timeout => (None, None),
            Outcome::Other => (Some(1), None),
        };
        Run {
            format,
            outcome,
            code,
            signal,
            engine_ms: 1,
            peak_rss_kib: Some(1),
            text_sha256: sha256(text.as_bytes()),
            text_bytes: text.len() as u64,
            stderr_tail: None,
        }
    }
    fn result(runs: Vec<Run>) -> ResultManifest {
        ResultManifest {
            v: VERSION,
            job_id: "b".repeat(64),
            mode: if runs.len() == 1 {
                Mode::Crash
            } else {
                Mode::Ocr
            },
            engine: engine(),
            runs,
            completed_at: "2026-10-13T00:00:00Z".into(),
        }
    }
    fn texts(jp2: &str, jpeg: &str) -> BTreeMap<Format, Vec<u8>> {
        BTreeMap::from([
            (Format::Jp2, jp2.as_bytes().to_vec()),
            (Format::Jpeg, jpeg.as_bytes().to_vec()),
        ])
    }

    #[test]
    fn outcome_from_status() {
        assert_eq!(Outcome::of(Some(0), None, false), Outcome::Ok);
        assert_eq!(Outcome::of(Some(136), None, false), Outcome::Crash136);
        assert_eq!(Outcome::of(None, Some(8), false), Outcome::Crash136);
        assert_eq!(Outcome::of(None, Some(9), true), Outcome::Timeout);
        assert_eq!(Outcome::of(Some(1), None, false), Outcome::Other);
    }

    #[test]
    fn crash_is_a_valid_result() {
        let r = result(vec![
            run(Format::Jp2, Outcome::Crash136, ""),
            run(Format::Jpeg, Outcome::Ok, "hello world"),
        ]);
        let t = texts("", "hello world");
        r.check(&t).unwrap();
        assert!(agree(&r, &t, &r.clone(), &t).is_ok());
    }

    #[test]
    fn hosts_must_agree_on_outcome_and_text() {
        let a = result(vec![
            run(Format::Jp2, Outcome::Ok, "the quick brown fox"),
            run(Format::Jpeg, Outcome::Ok, "the quick brown fox"),
        ]);
        let ta = texts("the quick brown fox", "the quick brown fox");
        let b = result(vec![
            run(Format::Jp2, Outcome::Crash136, ""),
            run(Format::Jpeg, Outcome::Ok, "the quick brown fox"),
        ]);
        let tb = texts("", "the quick brown fox");
        assert!(agree(&a, &ta, &b, &tb).is_err());
        let c = result(vec![
            run(Format::Jp2, Outcome::Ok, "the quick brown cat"),
            run(Format::Jpeg, Outcome::Ok, "the quick brown fox"),
        ]);
        let tc = texts("the quick brown cat", "the quick brown fox");
        assert!(agree(&a, &ta, &c, &tc).is_err());
    }

    #[test]
    fn crash_unit_counts_a_deadline_as_read() {
        let a = result(vec![run(Format::Jp2, Outcome::Timeout, "")]);
        let b = result(vec![run(Format::Jp2, Outcome::Ok, "")]);
        let t = BTreeMap::new();
        assert!(agree(&a, &t, &b, &t).is_ok());
        let c = result(vec![run(Format::Jp2, Outcome::Crash136, "")]);
        assert!(agree(&a, &t, &c, &t).is_err());
    }

    #[test]
    fn shape_must_match_mode() {
        let mut r = result(vec![run(Format::Jp2, Outcome::Ok, "")]);
        r.mode = Mode::Ocr;
        assert!(r.check(&BTreeMap::new()).is_err());
    }

    #[test]
    fn check_rejects_tampered_text() {
        let r = result(vec![run(Format::Jp2, Outcome::Ok, "abc")]);
        let t = BTreeMap::from([(Format::Jp2, b"abd".to_vec())]);
        assert!(r.check(&t).is_err());
    }

    #[test]
    fn job_identity_round_trip() {
        let mut j = Job {
            v: VERSION,
            id: String::new(),
            mode: Mode::Crash,
            doc_id: "sn00000001_1900-01-01_ed-1_seq-1".into(),
            batch: "test_batch".into(),
            batch_version: 1,
            language: "eng".into(),
            stratum: Stratum {
                crash_weight: 1.0,
                damage: "low".into(),
                decade: 1900,
                lang: "eng".into(),
                pages: 1,
                prefix: "test".into(),
                weight: 1.0,
            },
            inputs: BTreeMap::from([(
                Format::Jp2,
                Input {
                    url: "https://tile.loc.gov/x.jp2".into(),
                    sha256: "c".repeat(64),
                    bytes: 10,
                },
            )]),
            engine: engine(),
            engine_base: format!("sha256:{}", "f".repeat(64)),
        };
        j.id = j.identity().unwrap();
        j.validate().unwrap();
        j.mode = Mode::Ocr;
        assert!(j.validate().is_err());
    }
}
