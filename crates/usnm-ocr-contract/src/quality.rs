//! File-based jobs for the OCR quality estimate on BOINC (#522), the CPU
//! app `usnm_quality`: one page per workunit, with every text we hold of
//! it, and a result that says how the texts compare (the three-cornered
//! hat, completeness, findability), what the scan looks like, and each
//! text's confusion pairs. The computation is deterministic, so two hosts'
//! results compare equal after canonicalisation.
//!
//! Every way a run ends is an outcome, including a unit the host could not
//! run at all (`unit_error`): the result and its diagnostics still upload.
use crate::boinc::{is_sha256, sha256};
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Rounds to six decimals (half away from zero); non-finite values become 0.
pub fn round6(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    (x * 1e6).round() / 1e6
}

pub fn round6_opt(x: Option<f64>) -> Option<f64> {
    x.map(round6)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImageFeatures {
    pub bleed_through: f64,
    pub blur: f64,
    pub contrast: f64,
    /// Pixels of the page.
    pub height: u32,
    pub ink_density: f64,
    pub noise: f64,
    /// The integer factor the page was downscaled by.
    pub scale: u32,
    pub skew_deg: f64,
    /// The Otsu threshold on the grey copy.
    pub threshold: u8,
    pub width: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayoutFeatures {
    /// Text blocks.
    pub blocks: usize,
    /// Columns: text blocks clustered by x-centre.
    pub columns: usize,
    /// Median line height in pixels (an x-height proxy); none without lines.
    pub line_height_px: Option<f64>,
    pub lines: usize,
    /// Mean word confidence over the words that have one (LoC's WC).
    pub mean_word_conf: Option<f64>,
    pub words: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PageFeatures {
    pub image: Option<ImageFeatures>,
    pub layout: Option<LayoutFeatures>,
}

/// How two readings of one page differ. `a` is this reader, `b` the other;
/// `cer_a` is `a` as the hypothesis against `b` as the reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PairMetrics {
    pub cer_a: f64,
    pub cer_b: f64,
    /// Character edits over the mean of the two lengths: the symmetric
    /// disagreement the hat is solved on.
    pub cer_sym: f64,
    pub char_edits: usize,
    pub chars_a: usize,
    pub chars_b: usize,
    pub dice: f64,
    pub wer_a: f64,
    pub wer_b: f64,
    pub word_edits: usize,
    pub words_a: usize,
    pub words_b: usize,
}

impl PairMetrics {
    /// The same pair seen from the other reader.
    pub fn flipped(&self) -> Self {
        Self {
            cer_a: self.cer_b,
            cer_b: self.cer_a,
            cer_sym: self.cer_sym,
            char_edits: self.char_edits,
            chars_a: self.chars_b,
            chars_b: self.chars_a,
            dice: self.dice,
            wer_a: self.wer_b,
            wer_b: self.wer_a,
            word_edits: self.word_edits,
            words_a: self.words_b,
            words_b: self.words_a,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReaderReport {
    /// Characters of the normalised text.
    pub chars: usize,
    pub completeness: Option<f64>,
    /// Distinct consensus content words the reader has, over the total.
    pub content_found: usize,
    pub content_total: usize,
    /// The hat's estimate of this reader's CER, clipped at 0; none when
    /// the hat is underdetermined.
    pub est_cer: Option<f64>,
    pub est_cer_low_bound_note: String,
    /// The estimate before clipping.
    pub est_cer_raw: Option<f64>,
    pub findable_content: Option<f64>,
    pub findable_places: Option<f64>,
    pub name: String,
    /// Pairwise metrics against each other reader, by name; this reader
    /// is `a`.
    pub pairs: BTreeMap<String, PairMetrics>,
    pub places_found: usize,
    pub places_total: usize,
    pub precision: Option<f64>,
    pub source: String,
    /// Tokens of the text.
    pub words: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HatReport {
    pub determined: bool,
    pub n_readers: usize,
    pub pairs: usize,
    pub residual: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Legibility {
    /// What the estimate is based on: `page-hat` here (the readers of the
    /// whole page); the crop-level estimate with the vision-language
    /// reading is joined on the server.
    pub basis: String,
    /// `1 - min est_cer` over the readers, in 0..1; none when the hat is
    /// underdetermined.
    pub ceiling_est: Option<f64>,
    /// Consensus words on the page.
    pub consensus_words: usize,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfusionKind {
    /// Reference characters replaced by the reader's.
    Sub,
    /// Characters the reader has that the reference does not.
    Ins,
    /// Reference characters the reader dropped.
    Del,
}

/// One pair with how often it was seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfusionPair {
    pub count: usize,
    /// What the reference has (empty for an insertion).
    pub from: String,
    pub kind: ConfusionKind,
    /// What the reader has (empty for a deletion).
    pub to: String,
}

pub const VERSION: u32 = 1;
/// `quality.json`: the job holds the function-word list.
pub const MAX_JOB_JSON: usize = 64 * 1024;
/// `result.json`.
pub const MAX_RESULT_JSON: usize = 256 * 1024;
/// `confusions.json`.
pub const MAX_CONFUSIONS: usize = 256 * 1024;
/// `diag.txt`.
pub const MAX_DIAG: usize = 64 * 1024;
/// `texts.json`: every text we hold of the page, with regions.
pub const MAX_TEXTS: usize = 4 * 1024 * 1024;
/// `places.txt`: the campaign's place names, one per line.
pub const MAX_PLACES: usize = 8 * 1024 * 1024;
pub const MAX_ALTO: usize = crate::boinc::MAX_ALTO;
pub const MAX_SCAN: usize = crate::boinc::MAX_SCAN;
/// Longest error text kept in a result.
pub const MAX_ERROR: usize = 1024;
pub const MAX_READERS: usize = 8;
pub const MAX_FUNCTION_WORDS: usize = 2000;
/// One text in `texts.json`.
pub const MAX_TEXT: usize = 512 * 1024;
pub const MAX_CROPS: u32 = 64;

/// One input file: its size, checksum and where it came from (a URL, or a
/// short note such as `handoff:texts/<doc_id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub bytes: u64,
    pub sha256: String,
    pub source: String,
}
impl Input {
    pub fn of(bytes: &[u8], source: &str) -> Self {
        Self {
            bytes: bytes.len() as u64,
            sha256: sha256(bytes),
            source: source.to_owned(),
        }
    }
    pub fn validate(&self, max: usize, what: &str) -> anyhow::Result<()> {
        ensure!(
            self.bytes > 0 && self.bytes <= max as u64,
            "invalid {what} size"
        );
        ensure!(is_sha256(&self.sha256), "invalid {what} checksum");
        ensure!(
            !self.source.is_empty()
                && self.source.len() <= 512
                && !self
                    .source
                    .chars()
                    .any(|c| c.is_control() || c.is_whitespace()),
            "invalid {what} source"
        );
        Ok(())
    }
    pub fn matches(&self, bytes: &[u8]) -> bool {
        bytes.len() as u64 == self.bytes && sha256(bytes) == self.sha256
    }
}

/// The unit's inputs by their open names in the slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inputs {
    /// `loc.xml`.
    pub alto: Input,
    /// `page.jpg`.
    pub page: Input,
    /// `places.txt`, shared by every unit of the campaign.
    pub places: Input,
    /// `texts.json`.
    pub texts: Input,
}

/// The worker image a unit was staged for; the host refuses another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    /// The worker image's registry digest (`sha256:...`).
    pub image: String,
}

/// A text of the page in `texts.json`, by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderSpec {
    /// `loc`, `as`, `ours-tesseract`, ...: lower-case letters, digits and
    /// hyphens.
    pub name: String,
    /// `loc-alto`, `american-stories`, `ocr-out:<engine id>`, ...
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub v: u32,
    pub id: String,
    /// The seed of the crop selection the `usnm_qwen` unit of the same
    /// page uses, so both apps cut the same crops.
    pub crop_seed: u64,
    pub doc_id: String,
    pub engine: Engine,
    /// The language's function words, folded; tokens of four letters or
    /// more that are not in it are content words.
    pub function_words: Vec<String>,
    /// The campaign group the page came from.
    pub group: String,
    pub inputs: Inputs,
    pub k_crops: u32,
    /// ISO 639-3.
    pub language: String,
    /// The texts the unit compares, in the order they are reported.
    pub readers: Vec<ReaderSpec>,
}
impl Job {
    pub fn identity(&self) -> anyhow::Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.v,
            &self.doc_id,
            &self.language,
            &self.group,
            &self.inputs,
            &self.engine,
            &self.readers,
            &self.function_words,
            self.crop_seed,
            self.k_crops,
        ))?))
    }
    pub fn workunit(&self) -> String {
        format!("usnm_quality_{}", self.id)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported quality job version");
        ensure!(
            is_sha256(&self.id) && self.id == self.identity()?,
            "job identity mismatch"
        );
        self.inputs.page.validate(MAX_SCAN, "page")?;
        self.inputs.alto.validate(MAX_ALTO, "alto")?;
        self.inputs.texts.validate(MAX_TEXTS, "texts")?;
        self.inputs.places.validate(MAX_PLACES, "places")?;
        ensure!(
            self.engine
                .image
                .strip_prefix("sha256:")
                .is_some_and(is_sha256),
            "engine image must be a full sha256 digest"
        );
        ensure!(
            !self.readers.is_empty() && self.readers.len() <= MAX_READERS,
            "a job compares 1 to {MAX_READERS} readers"
        );
        let mut names: Vec<&str> = self.readers.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        ensure!(
            names.len() == self.readers.len(),
            "a reader is listed twice"
        );
        for r in &self.readers {
            check_name(&r.name, "reader name")?;
            ensure!(
                !r.source.is_empty()
                    && r.source.len() <= 128
                    && !r
                        .source
                        .chars()
                        .any(|c| c.is_control() || c.is_whitespace()),
                "invalid reader source"
            );
        }
        ensure!(
            self.function_words.len() <= MAX_FUNCTION_WORDS,
            "too many function words"
        );
        for w in &self.function_words {
            ensure!(
                !w.is_empty()
                    && w.len() <= 64
                    && !w.chars().any(|c| c.is_control() || c.is_whitespace()),
                "invalid function word"
            );
        }
        ensure!(
            (1..=MAX_CROPS).contains(&self.k_crops),
            "k_crops must be 1 to {MAX_CROPS}"
        );
        for (s, what) in [(&self.group, "group"), (&self.language, "language")] {
            check_name(s, what)?;
        }
        check_doc_id(&self.doc_id)?;
        Ok(())
    }
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

/// Lower-case ASCII letters, digits and hyphens, 1 to 32 bytes.
pub fn check_name(s: &str, what: &str) -> anyhow::Result<()> {
    ensure!(
        !s.is_empty()
            && s.len() <= 32
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
        "invalid {what} {s:?}"
    );
    Ok(())
}

/// A region of a reader's text that carries boxes (American Stories'
/// articles and headlines), in page pixels.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// `[x0, y0, x1, y1]`.
    #[serde(rename = "box")]
    pub rect: [u32; 4],
    pub kind: Option<String>,
    /// `legible`, `borderline` or `illegible`.
    pub legibility: Option<String>,
}

/// One text in `texts.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextReader {
    /// The reader has word boxes in an ALTO of its own (`ours.xml`).
    #[serde(default)]
    pub alto: bool,
    pub name: String,
    #[serde(default)]
    pub regions: Vec<Region>,
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageSize {
    pub height: u32,
    pub width: u32,
}

/// `texts.json`: the texts of the page that do not come from `loc.xml`
/// (LoC's words with boxes come from the ALTO on the host), plus LoC's as
/// text for convenience, and the page size the regions are in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Texts {
    pub page: Option<PageSize>,
    pub readers: Vec<TextReader>,
}
impl Texts {
    /// Checks the file against the job: every reader the job names is
    /// there once, with text within the limit.
    pub fn validate(&self, job: &Job) -> anyhow::Result<()> {
        ensure!(self.readers.len() <= MAX_READERS, "too many texts");
        for spec in &job.readers {
            let found: Vec<&TextReader> = self
                .readers
                .iter()
                .filter(|r| r.name == spec.name)
                .collect();
            ensure!(
                found.len() == 1,
                "texts.json must hold reader {:?} once",
                spec.name
            );
            let r = found[0];
            ensure!(
                r.source == spec.source,
                "reader {:?} source differs from the job",
                spec.name
            );
            ensure!(r.text.len() <= MAX_TEXT, "text over size limit");
            for g in &r.regions {
                ensure!(
                    g.rect[0] < g.rect[2] && g.rect[1] < g.rect[3],
                    "invalid region box"
                );
                ensure!(
                    g.legibility
                        .as_deref()
                        .is_none_or(|l| ["legible", "borderline", "illegible"].contains(&l)),
                    "invalid region legibility"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The computation ran and the record is complete.
    Ok,
    /// The computation failed part way (the reason is in the diagnostics);
    /// what was computed is in the record.
    Failed,
    /// The computation did not finish within the unit's deadline.
    Timeout,
    /// The unit could not run at all (bad input, an engine that differs
    /// from the job).
    UnitError,
}

/// What the host looked like, from BOINC's `init_data.xml` (only the
/// fields named here; never the account or host identifiers) and the
/// container runtime's marker file.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub boinc_client: Option<String>,
    pub cpu_threads: Option<u32>,
    pub ncpus: Option<f64>,
    pub plan_class: Option<String>,
    pub rootless: Option<bool>,
    pub runtime: Option<String>,
}

/// Where the time went, in milliseconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timings {
    /// CPU time of the process (user and system), when the host reports it.
    pub cpu_ms: Option<u64>,
    /// Decoding the page JPEG and the ALTO.
    pub decode_ms: u64,
    /// Image and layout features.
    pub features_ms: u64,
    /// The process's peak resident set (`VmHWM`), when the host reports it.
    pub peak_rss_kib: Option<u64>,
    /// Alignment, hat, consensus, findability, confusions.
    pub text_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultManifest {
    pub v: u32,
    pub job_id: String,
    pub completed_at: String,
    pub confusions_bytes: u64,
    pub confusions_sha256: String,
    pub diag_bytes: u64,
    pub diag_sha256: String,
    pub engine: Engine,
    pub hat: Option<HatReport>,
    pub host: Host,
    pub legibility: Option<Legibility>,
    pub outcome: Outcome,
    pub page: Option<PageFeatures>,
    pub readers: Vec<ReaderReport>,
    pub timings: Timings,
    /// Set with `unit_error`: the reason.
    pub unit_error: Option<String>,
}
impl ResultManifest {
    /// Checks one host's result on its own: shape, sizes and checksums.
    pub fn check(&self, confusions: &[u8], diag: &[u8]) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported result version");
        ensure!(is_sha256(&self.job_id), "invalid job id");
        chrono::DateTime::parse_from_rfc3339(&self.completed_at)?;
        ensure!(diag.len() <= MAX_DIAG, "diagnostics over size limit");
        ensure!(
            self.diag_sha256 == sha256(diag) && self.diag_bytes == diag.len() as u64,
            "diagnostics checksum mismatch"
        );
        ensure!(
            confusions.len() <= MAX_CONFUSIONS,
            "confusions over size limit"
        );
        ensure!(
            self.confusions_sha256 == sha256(confusions)
                && self.confusions_bytes == confusions.len() as u64,
            "confusions checksum mismatch"
        );
        let parsed: BTreeMap<String, Vec<ConfusionPair>> = serde_json::from_slice(confusions)?;
        ensure!(
            self.unit_error
                .as_ref()
                .is_none_or(|e| e.len() <= MAX_ERROR),
            "error text too long"
        );
        ensure!(
            (self.outcome == Outcome::UnitError) == self.unit_error.is_some(),
            "a unit error has a reason and nothing else does"
        );
        ensure!(self.readers.len() <= MAX_READERS, "too many readers");
        match self.outcome {
            Outcome::UnitError => ensure!(
                self.readers.is_empty()
                    && self.hat.is_none()
                    && self.page.is_none()
                    && parsed.is_empty(),
                "a unit error has no record"
            ),
            Outcome::Ok => {
                ensure!(
                    self.hat.is_some() && self.legibility.is_some() && self.page.is_some(),
                    "an ok result has the whole record"
                );
                for r in &self.readers {
                    ensure!(parsed.contains_key(&r.name), "confusions missing a reader");
                }
            }
            Outcome::Failed | Outcome::Timeout => {}
        }
        let mut names: Vec<&str> = self.readers.iter().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        ensure!(
            names.len() == self.readers.len(),
            "a reader is reported twice"
        );
        for r in &self.readers {
            ensure!(
                r.pairs.len() + 1 == self.readers.len(),
                "pairs do not match the readers"
            );
            ensure!(
                r.est_cer
                    .is_none_or(|e| e.is_finite() && (0.0..=10.0).contains(&e)),
                "estimate out of range"
            );
        }
        Ok(())
    }

    /// The part of the result two hosts must agree on: everything but the
    /// host, the timings, the time and the diagnostics.
    pub fn canonical(&self) -> serde_json::Value {
        canonicalise(serde_json::json!({
            "confusions_sha256": self.confusions_sha256,
            "engine": self.engine,
            "hat": self.hat,
            "job_id": self.job_id,
            "legibility": self.legibility,
            "outcome": self.outcome,
            "page": self.page,
            "readers": self.readers,
            "unit_error": self.unit_error,
            "v": self.v,
        }))
    }
}

/// Keys sorted (serde_json's map already is) and every float rounded to
/// six decimals.
pub fn canonicalise(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if !n.is_i64() && !n.is_u64() => {
                serde_json::Number::from_f64(round6(f)).map_or(Value::Null, Value::Number)
            }
            _ => Value::Number(n),
        },
        Value::Array(a) => Value::Array(a.into_iter().map(canonicalise).collect()),
        Value::Object(o) => {
            let sorted: BTreeMap<String, Value> =
                o.into_iter().map(|(k, v)| (k, canonicalise(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        other => other,
    }
}

/// Whether two hosts' results for one unit agree: same job and engine,
/// same outcome, and the canonical records equal.
pub fn agree(a: &ResultManifest, b: &ResultManifest) -> Result<(), String> {
    if a.job_id != b.job_id || a.engine != b.engine {
        return Err("different job or engine".into());
    }
    if a.outcome != b.outcome {
        return Err(format!("outcomes {:?} and {:?}", a.outcome, b.outcome));
    }
    let (ca, cb) = (a.canonical(), b.canonical());
    if ca != cb {
        let what = [
            "page",
            "readers",
            "hat",
            "legibility",
            "confusions_sha256",
            "unit_error",
        ]
        .iter()
        .find(|k| ca.get(k) != cb.get(k))
        .copied()
        .unwrap_or("record");
        return Err(format!("{what} differs"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn engine() -> Engine {
        Engine {
            image: format!("sha256:{}", "a".repeat(64)),
        }
    }

    pub(crate) fn job() -> Job {
        let mut j = Job {
            v: VERSION,
            id: String::new(),
            crop_seed: 522,
            doc_id: "sn85055101_1917-04-03_ed-1_seq-3".into(),
            engine: engine(),
            function_words: vec!["with".into(), "from".into()],
            group: "serbian".into(),
            inputs: Inputs {
                alto: Input::of(b"<alto/>", "https://tile.loc.gov/x/ocr.xml"),
                page: Input::of(b"jpeg", "https://tile.loc.gov/x/full/full/0/default.jpg"),
                places: Input::of(b"Albany\n", "campaign:places.txt"),
                texts: Input::of(b"{}", "handoff:texts/x.json"),
            },
            k_crops: 12,
            language: "srp".into(),
            readers: vec![
                ReaderSpec {
                    name: "loc".into(),
                    source: "loc-alto".into(),
                },
                ReaderSpec {
                    name: "as".into(),
                    source: "american-stories".into(),
                },
            ],
        };
        j.id = j.identity().unwrap();
        j
    }

    pub(crate) fn result(outcome: Outcome) -> (ResultManifest, Vec<u8>) {
        let confusions = if outcome == Outcome::UnitError {
            b"{}".to_vec()
        } else {
            br#"{"as":[],"loc":[]}"#.to_vec()
        };
        let pair = PairMetrics {
            cer_a: 0.0,
            cer_b: 0.0,
            cer_sym: 0.0,
            char_edits: 0,
            chars_a: 13,
            chars_b: 13,
            dice: 1.0,
            wer_a: 0.0,
            wer_b: 0.0,
            word_edits: 0,
            words_a: 3,
            words_b: 3,
        };
        let reader = |name: &str, other: &str| ReaderReport {
            chars: 13,
            completeness: Some(1.0),
            content_found: 1,
            content_total: 1,
            est_cer: None,
            est_cer_low_bound_note: "three-cornered hat".into(),
            est_cer_raw: None,
            findable_content: Some(1.0),
            findable_places: None,
            name: name.into(),
            pairs: BTreeMap::from([(other.into(), pair.clone())]),
            places_found: 0,
            places_total: 0,
            precision: Some(1.0),
            source: name.into(),
            words: 3,
        };
        let unit = outcome == Outcome::UnitError;
        (
            ResultManifest {
                v: VERSION,
                job_id: job().id,
                completed_at: "2026-10-14T00:00:00Z".into(),
                confusions_bytes: confusions.len() as u64,
                confusions_sha256: sha256(&confusions),
                diag_bytes: 0,
                diag_sha256: sha256(b""),
                engine: engine(),
                hat: (!unit).then_some(HatReport {
                    determined: false,
                    n_readers: 2,
                    pairs: 1,
                    residual: None,
                }),
                host: Host::default(),
                legibility: (!unit).then_some(Legibility {
                    basis: "page-hat".into(),
                    ceiling_est: None,
                    consensus_words: 3,
                }),
                outcome,
                page: (!unit).then_some(PageFeatures {
                    image: None,
                    layout: None,
                }),
                readers: if unit {
                    vec![]
                } else {
                    vec![reader("loc", "as"), reader("as", "loc")]
                },
                timings: Timings::default(),
                unit_error: unit.then(|| "checksum".into()),
            },
            confusions,
        )
    }

    #[test]
    fn job_identity_and_limits() {
        let j = job();
        j.validate().unwrap();
        let mut bad = j.clone();
        bad.readers.push(bad.readers[0].clone());
        bad.id = bad.identity().unwrap();
        assert!(bad.validate().is_err());
        let mut tampered = j.clone();
        tampered.language = "eng".into();
        assert!(tampered.validate().is_err());
        let mut k = j;
        k.k_crops = 0;
        k.id = k.identity().unwrap();
        assert!(k.validate().is_err());
    }

    #[test]
    fn texts_must_match_the_job() {
        let j = job();
        let t = Texts {
            page: Some(PageSize {
                height: 10,
                width: 10,
            }),
            readers: vec![
                TextReader {
                    alto: false,
                    name: "loc".into(),
                    regions: vec![],
                    source: "loc-alto".into(),
                    text: "a".into(),
                },
                TextReader {
                    alto: false,
                    name: "as".into(),
                    regions: vec![Region {
                        rect: [0, 0, 5, 5],
                        kind: Some("article".into()),
                        legibility: Some("legible".into()),
                    }],
                    source: "american-stories".into(),
                    text: "a".into(),
                },
            ],
        };
        t.validate(&j).unwrap();
        let mut missing = t.clone();
        missing.readers.pop();
        assert!(missing.validate(&j).is_err());
        let mut bad = t;
        bad.readers[1].regions[0].legibility = Some("fine".into());
        assert!(bad.validate(&j).is_err());
    }

    #[test]
    fn check_and_agree() {
        let (a, ca) = result(Outcome::Ok);
        a.check(&ca, b"").unwrap();
        assert!(a.check(&ca, b"diag").is_err());
        let (b, _) = result(Outcome::Ok);
        agree(&a, &b).unwrap();
        let mut host = b.clone();
        host.host.ncpus = Some(8.0);
        host.timings.total_ms = 99;
        host.completed_at = "2026-10-15T00:00:00Z".into();
        agree(&a, &host).unwrap();
        let mut other = b.clone();
        other.readers[0].chars += 1;
        assert_eq!(agree(&a, &other).unwrap_err(), "readers differs");
        let (u, cu) = result(Outcome::UnitError);
        u.check(&cu, b"").unwrap();
        assert!(agree(&a, &u).is_err());
        agree(&u, &u).unwrap();
        let mut wrong = u.clone();
        wrong.unit_error = None;
        assert!(wrong.check(&cu, b"").is_err());
    }

    #[test]
    fn canonical_rounds_floats_and_sorts_keys() {
        let v = canonicalise(
            serde_json::json!({"b": 0.12345678901, "a": [1, 2.0000004], "c": {"z": 1, "y": null}}),
        );
        assert_eq!(
            serde_json::to_string(&v).unwrap(),
            r#"{"a":[1,2.0],"b":0.123457,"c":{"y":null,"z":1}}"#
        );
    }
}
