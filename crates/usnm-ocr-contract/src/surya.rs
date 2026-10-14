//! File-based jobs for the Surya 2 test on BOINC (#516): one page per
//! workunit, read by Surya 2 (surya-ocr on a llama.cpp server) in up to three
//! input modes. The app has a CPU and a GPU version; the result records the
//! device every run used, so CPU against GPU agreement can be measured.
//!
//! Every way a run ends is an outcome, including a unit the host could not
//! run at all (`unit_error`): the result and its diagnostics still upload.
use crate::boinc::{is_sha256, sha256};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const VERSION: u32 = 1;
pub const MAX_JSON: usize = 16 * 1024;
pub const MAX_TEXT: usize = 512 * 1024;
/// The diagnostics file (`diag.txt`), whole.
pub const MAX_DIAG: usize = 64 * 1024;
pub const MAX_SCAN: usize = crate::boinc::MAX_SCAN;
/// Longest error text kept in a result.
pub const MAX_ERROR: usize = 1024;
/// The probe's acceptance floor for word-bag Dice between two hosts' texts
/// of one mode. Vision-language output can differ between devices, so the
/// threshold proper is measured on the probe's pairs (every compared pair is
/// logged) and set from them; this floor only rejects unrelated text.
pub const PROBE_MIN_DICE: f64 = 0.5;

/// How the page is given to Surya.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputMode {
    /// 1,280 px tiles overlapping by 256 px, as round 1 (#370) read it, with
    /// the overlap split at its midpoint so each word is kept once.
    Tiled,
    /// The whole page in one request: the model scales it to its budget.
    Whole,
    /// The whole page, first scaled so its long side is at most
    /// [`CAPPED_LONG_SIDE`] px.
    Capped,
}
pub const CAPPED_LONG_SIDE: u32 = 2048;
pub const MODES: [InputMode; 3] = [InputMode::Tiled, InputMode::Whole, InputMode::Capped];
impl InputMode {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tiled => "tiled",
            Self::Whole => "whole",
            Self::Capped => "capped",
        }
    }
    /// The output text's open name in the BOINC slot.
    pub fn text(self) -> &'static str {
        match self {
            Self::Tiled => "text-tiled.txt",
            Self::Whole => "text-whole.txt",
            Self::Capped => "text-capped.txt",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub bytes: u64,
    pub sha256: String,
    pub url: String,
}

/// The engine build a unit was staged for. The image carries the same
/// values in its environment and the host refuses a unit staged for another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    /// The worker image's registry digest (`sha256:...`): the operator's
    /// record of which app version the unit was staged for.
    pub image: String,
    /// The llama.cpp server image the worker is built on, by digest.
    pub llama: String,
    /// surya-ocr's version.
    pub surya: String,
    /// `sha256:` of the weights lock lines (path, URL, sha256) in the image.
    pub weights: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub v: u32,
    pub id: String,
    pub doc_id: String,
    /// The round 1 group the page came from (`serbian`, `worst-english`, ...).
    pub group: String,
    /// ISO 639-3.
    pub language: String,
    /// The full-size IIIF JPEG (`page.jpg` in the slot).
    pub input: Input,
    pub engine: Engine,
    /// Modes a host with a usable GPU runs, in order.
    pub gpu_modes: Vec<InputMode>,
    /// Modes a CPU-only host runs, in order (round 1 took about 40 minutes
    /// per tiled page on 8 cores, so fewer).
    pub cpu_modes: Vec<InputMode>,
    /// Deadline per mode on a GPU.
    pub gpu_timeout_secs: u32,
    /// Deadline per mode on a CPU.
    pub cpu_timeout_secs: u32,
}
impl Job {
    pub fn identity(&self) -> anyhow::Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.v,
            &self.doc_id,
            &self.group,
            &self.language,
            &self.input,
            &self.engine,
            &self.gpu_modes,
            &self.cpu_modes,
            self.gpu_timeout_secs,
            self.cpu_timeout_secs,
        ))?))
    }
    pub fn workunit(&self) -> String {
        format!("usnm_surya_{}", self.id)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported Surya job version");
        ensure!(
            is_sha256(&self.id) && self.id == self.identity()?,
            "job identity mismatch"
        );
        ensure!(
            self.input.bytes > 0 && self.input.bytes <= MAX_SCAN as u64,
            "invalid input size"
        );
        ensure!(is_sha256(&self.input.sha256), "invalid input checksum");
        ensure!(
            self.input.url.starts_with("https://tile.loc.gov/")
                && self.input.url.len() <= 512
                && !self
                    .input
                    .url
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace()),
            "invalid input URL"
        );
        for d in [&self.engine.image, &self.engine.llama, &self.engine.weights] {
            ensure!(
                d.strip_prefix("sha256:").is_some_and(is_sha256),
                "engine digests must be full sha256 digests"
            );
        }
        ensure!(
            !self.engine.surya.is_empty()
                && self.engine.surya.len() <= 32
                && self
                    .engine
                    .surya
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.'),
            "invalid surya version"
        );
        for modes in [&self.gpu_modes, &self.cpu_modes] {
            ensure!(
                !modes.is_empty() && modes.len() <= MODES.len(),
                "a job runs 1 to 3 modes"
            );
            let mut seen = modes.clone();
            seen.sort();
            seen.dedup();
            ensure!(seen.len() == modes.len(), "a mode is listed twice");
        }
        for t in [self.gpu_timeout_secs, self.cpu_timeout_secs] {
            ensure!((60..=14_400).contains(&t), "invalid deadline");
        }
        for (s, what) in [(&self.group, "group"), (&self.language, "language")] {
            ensure!(
                !s.is_empty()
                    && s.len() <= 32
                    && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "invalid {what}"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Surya exited 0 and wrote its text.
    Ok,
    /// The mode's deadline passed; the process group was killed.
    Timeout,
    /// Surya exited non-zero or was killed by a signal (an out-of-memory
    /// kill shows as signal 9).
    Failed,
    /// The GPU version ran, but no usable NVIDIA GPU was visible in the
    /// container, so nothing was read (the diagnostics say why).
    NoGpu,
}
impl Outcome {
    pub fn of(code: Option<i32>, timed_out: bool) -> Self {
        if timed_out {
            Self::Timeout
        } else if code == Some(0) {
            Self::Ok
        } else {
            Self::Failed
        }
    }
}

/// One mode's run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub code: Option<i32>,
    /// CPU seconds of the process group (Python and llama-server), summed
    /// over the processes seen while it ran.
    pub cpu_secs: Option<f64>,
    /// What llama-server ran the model on: `cuda` when it offloaded layers
    /// to a GPU, else `cpu`; `none` when nothing ran.
    pub device: String,
    pub engine_ms: u64,
    /// Highest GPU memory in use while the mode ran, less what was in use
    /// before it started (nvidia-smi, the whole GPU).
    pub gpu_mem_peak_mib: Option<u64>,
    /// llama-server's "offloaded N/M layers to GPU", when it logged one.
    pub gpu_layers: Option<String>,
    pub mode: InputMode,
    pub outcome: Outcome,
    /// Highest summed RSS of the process group, sampled every 0.5 s.
    pub peak_rss_kib: Option<u64>,
    pub signal: Option<i32>,
    pub text_bytes: u64,
    pub text_sha256: String,
    /// Tiles read (1 for a whole or capped page).
    pub tiles: Option<u32>,
}

/// What the host and the container looked like, from BOINC's init_data.xml
/// (only the fields named here; never the account or host identifiers),
/// the container runtime's marker file and nvidia-smi.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub boinc_client: Option<String>,
    pub cpu_threads: Option<u32>,
    pub cuda_version: Option<String>,
    pub gpu: Option<String>,
    pub gpu_compute_cap: Option<String>,
    pub gpu_device_num: Option<i32>,
    pub gpu_driver: Option<String>,
    pub gpu_mem_total_mib: Option<u64>,
    pub gpu_type: Option<String>,
    pub ncpus: Option<f64>,
    pub plan_class: Option<String>,
    pub rootless: Option<bool>,
    /// `podman-5.8.2`, `docker`, or none when not recognised.
    pub runtime: Option<String>,
    pub torch_cuda: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultManifest {
    pub v: u32,
    pub job_id: String,
    pub engine: Engine,
    pub host: Host,
    /// Set when the unit could not run at all (bad input, an engine that
    /// differs from the job): no runs, and the reason.
    pub unit_error: Option<String>,
    pub runs: Vec<Run>,
    pub diag_bytes: u64,
    pub diag_sha256: String,
    pub completed_at: String,
}
impl ResultManifest {
    /// Checks one host's result on its own: shape, sizes and checksums.
    pub fn check(&self, texts: &BTreeMap<InputMode, Vec<u8>>, diag: &[u8]) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported result version");
        ensure!(is_sha256(&self.job_id), "invalid job id");
        chrono::DateTime::parse_from_rfc3339(&self.completed_at)?;
        ensure!(diag.len() <= MAX_DIAG, "diagnostics over size limit");
        ensure!(
            self.diag_sha256 == sha256(diag) && self.diag_bytes == diag.len() as u64,
            "diagnostics checksum mismatch"
        );
        ensure!(
            self.unit_error
                .as_ref()
                .is_none_or(|e| e.len() <= MAX_ERROR),
            "error text too long"
        );
        ensure!(
            self.unit_error.is_none() != self.runs.is_empty(),
            "a result has runs or a unit error, not both or neither"
        );
        ensure!(self.runs.len() <= MODES.len(), "too many runs");
        let mut modes: Vec<InputMode> = self.runs.iter().map(|r| r.mode).collect();
        modes.sort();
        modes.dedup();
        ensure!(modes.len() == self.runs.len(), "a mode ran twice");
        for m in MODES {
            let text = texts.get(&m).map(Vec::as_slice).unwrap_or(b"");
            ensure!(text.len() <= MAX_TEXT, "text over size limit");
            ensure!(std::str::from_utf8(text).is_ok(), "text is not UTF-8");
            match self.runs.iter().find(|r| r.mode == m) {
                None => ensure!(text.is_empty(), "text for a mode that did not run"),
                Some(r) => {
                    ensure!(
                        r.text_sha256 == sha256(text) && r.text_bytes == text.len() as u64,
                        "returned text checksum mismatch"
                    );
                    ensure!(
                        r.outcome == Outcome::Ok || text.is_empty(),
                        "a failed run returned text"
                    );
                    let consistent = match r.outcome {
                        Outcome::Ok => r.code == Some(0) && r.signal.is_none(),
                        Outcome::Timeout | Outcome::NoGpu => r.code.is_none() && r.signal.is_none(),
                        Outcome::Failed => r.code != Some(0),
                    };
                    ensure!(consistent, "outcome does not match the exit status");
                    ensure!(
                        ["cpu", "cuda", "none"].contains(&r.device.as_str()),
                        "unknown device"
                    );
                    ensure!(
                        r.gpu_layers.as_ref().is_none_or(|s| s.len() <= 64),
                        "gpu layers text too long"
                    );
                }
            }
        }
        Ok(())
    }
}

/// One mode both results ran, with how far their texts agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeAgreement {
    pub dice: Option<f64>,
    pub mode: InputMode,
    pub outcomes: (Outcome, Outcome),
}

/// Compares two hosts' results for one unit. They agree when they are for
/// the same job and engine and, in every mode both ran, both read the page
/// with texts whose word-bag Dice is at least `min_dice`, or neither read it.
/// A mode only one of them ran (CPU hosts run fewer) is not compared.
pub fn agree(
    a: &ResultManifest,
    a_texts: &BTreeMap<InputMode, Vec<u8>>,
    b: &ResultManifest,
    b_texts: &BTreeMap<InputMode, Vec<u8>>,
    min_dice: f64,
) -> (Vec<ModeAgreement>, Result<(), String>) {
    let mut out = Vec::new();
    if a.job_id != b.job_id || a.engine != b.engine {
        return (out, Err("different job or engine".into()));
    }
    let mut verdict = Ok(());
    for ra in &a.runs {
        let Some(rb) = b.runs.iter().find(|r| r.mode == ra.mode) else {
            continue;
        };
        let both_ok = ra.outcome == Outcome::Ok && rb.outcome == Outcome::Ok;
        let dice = both_ok.then(|| {
            let ta = String::from_utf8_lossy(a_texts.get(&ra.mode).map_or(&[][..], |v| v));
            let tb = String::from_utf8_lossy(b_texts.get(&rb.mode).map_or(&[][..], |v| v));
            crate::assess::dice(&ta, &tb)
        });
        if verdict.is_ok() {
            if (ra.outcome == Outcome::Ok) != (rb.outcome == Outcome::Ok) {
                verdict = Err(format!(
                    "{}: outcomes {:?} and {:?}",
                    ra.mode.name(),
                    ra.outcome,
                    rb.outcome
                ));
            } else if let Some(d) = dice.filter(|d| *d < min_dice) {
                verdict = Err(format!("{}: text agreement {d:.4}", ra.mode.name()));
            }
        }
        out.push(ModeAgreement {
            dice,
            mode: ra.mode,
            outcomes: (ra.outcome, rb.outcome),
        });
    }
    if verdict.is_ok() {
        if a.unit_error.is_some() || b.unit_error.is_some() {
            // Two hosts that both could not run the unit agree on that; a
            // unit error never stands in for a read.
            if a.unit_error.is_none() || b.unit_error.is_none() {
                verdict = Err("one result could not run the unit".into());
            }
        } else if out.is_empty() {
            verdict = Err("no mode in common".into());
        }
    }
    (out, verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn engine() -> Engine {
        Engine {
            image: format!("sha256:{}", "a".repeat(64)),
            llama: format!("sha256:{}", "b".repeat(64)),
            surya: "0.22.1".into(),
            weights: format!("sha256:{}", "c".repeat(64)),
        }
    }

    fn job() -> Job {
        let mut j = Job {
            v: VERSION,
            id: String::new(),
            doc_id: "sn85055101_1917-04-03_ed-1_seq-3".into(),
            group: "serbian".into(),
            language: "srp".into(),
            input: Input {
                bytes: 10,
                sha256: "d".repeat(64),
                url: "https://tile.loc.gov/image-services/iiif/x/full/full/0/default.jpg".into(),
            },
            engine: engine(),
            gpu_modes: MODES.to_vec(),
            cpu_modes: vec![InputMode::Tiled],
            gpu_timeout_secs: 1800,
            cpu_timeout_secs: 5400,
        };
        j.id = j.identity().unwrap();
        j
    }

    fn result(
        runs: &[(InputMode, Outcome, &str)],
    ) -> (ResultManifest, BTreeMap<InputMode, Vec<u8>>) {
        let mut texts = BTreeMap::new();
        let runs = runs
            .iter()
            .map(|(m, o, t)| {
                texts.insert(*m, t.as_bytes().to_vec());
                Run {
                    code: (*o == Outcome::Ok)
                        .then_some(0)
                        .or((*o == Outcome::Failed).then_some(1)),
                    cpu_secs: None,
                    device: "cuda".into(),
                    engine_ms: 1,
                    gpu_mem_peak_mib: None,
                    gpu_layers: None,
                    mode: *m,
                    outcome: *o,
                    peak_rss_kib: None,
                    signal: None,
                    text_bytes: t.len() as u64,
                    text_sha256: sha256(t.as_bytes()),
                    tiles: Some(1),
                }
            })
            .collect();
        (
            ResultManifest {
                v: VERSION,
                job_id: job().id,
                engine: engine(),
                host: Host::default(),
                unit_error: None,
                runs,
                diag_bytes: 0,
                diag_sha256: sha256(b""),
                completed_at: "2026-10-14T00:00:00Z".into(),
            },
            texts,
        )
    }

    #[test]
    fn job_identity_and_limits() {
        let j = job();
        j.validate().unwrap();
        let mut bad = j.clone();
        bad.gpu_modes = vec![InputMode::Tiled, InputMode::Tiled];
        bad.id = bad.identity().unwrap();
        assert!(bad.validate().is_err());
        let mut tampered = j;
        tampered.language = "eng".into();
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn check_and_agree() {
        let (a, at) = result(&[
            (InputMode::Tiled, Outcome::Ok, "one two three four"),
            (InputMode::Whole, Outcome::Timeout, ""),
        ]);
        a.check(&at, b"").unwrap();
        let (b, bt) = result(&[(InputMode::Tiled, Outcome::Ok, "one two three five")]);
        let (modes, v) = agree(&a, &at, &b, &bt, 0.7);
        assert!(v.is_ok());
        assert_eq!(modes.len(), 1);
        assert_eq!(modes[0].dice, Some(0.75));
        let (_, v) = agree(&a, &at, &b, &bt, 0.8);
        assert!(v.is_err());
        // Results with no mode in common, or a result with no runs, prove nothing.
        let (d, dt) = result(&[(InputMode::Whole, Outcome::Ok, "one two three four")]);
        assert!(agree(&a, &at, &d, &dt, 0.5).1.is_err());
        let (e, et) = result(&[]);
        assert!(e.check(&et, b"").is_err());
        let (c, ct) = result(&[(InputMode::Tiled, Outcome::Failed, "")]);
        c.check(&ct, b"").unwrap();
        assert!(agree(&a, &at, &c, &ct, 0.5).1.is_err());
        // A text for a mode that did not run, or a wrong checksum, is refused.
        let mut extra = at.clone();
        extra.insert(InputMode::Capped, b"x".to_vec());
        assert!(a.check(&extra, b"").is_err());
        assert!(a.check(&at, b"diag").is_err());
    }
}
