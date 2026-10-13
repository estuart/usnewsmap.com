//! File-based transport for stock BOINC. Identity covers the selected page,
//! scan bytes and processing specification; no credentials or LoC text.
use crate::message::OcrMessage;
use anyhow::{bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: u32 = 1;
pub const MAX_JSON: usize = 16 * 1024;
pub const MAX_TEXT: usize = 512 * 1024;
pub const MAX_ALTO: usize = 32 * 1024 * 1024;
pub const MAX_SCAN: usize = 128 * 1024 * 1024;

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scan {
    pub url: String,
    pub iiif_base: String,
    pub sha256: String,
    pub bytes: u64,
    pub md5: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    /// Full digest of the released BOINC worker image, not the base image.
    pub image: String,
    pub version: String,
    pub profile: String,
    /// Same profile hash as usnm_core::ocr_engine (sha256:12 hex).
    pub models: String,
    pub psm: u8,
    pub dpi: u16,
    pub timeout_secs: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub v: u32,
    pub id: String,
    pub message: OcrMessage,
    pub batch_version: u16,
    pub scan: Scan,
    pub engine: Engine,
}
impl Job {
    pub fn identity(&self) -> anyhow::Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.v,
            &self.message.page,
            &self.message.batch,
            &self.message.language,
            self.message.reason,
            self.message.score,
            self.batch_version,
            &self.scan,
            &self.engine,
        ))?))
    }
    pub fn workunit(&self) -> String {
        format!("usnm_{}", self.id)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported BOINC job version");
        self.message.validate()?;
        ensure!(
            self.batch_version > 0 && self.batch_version <= 32767,
            "invalid batch version"
        );
        ensure!(
            is_sha256(&self.id) && self.id == self.identity()?,
            "job identity mismatch"
        );
        ensure!(is_sha256(&self.scan.sha256), "invalid scan checksum");
        ensure!(
            self.scan.bytes > 0 && self.scan.bytes <= MAX_SCAN as u64,
            "invalid scan size"
        );
        for url in [&self.scan.url, &self.scan.iiif_base] {
            ensure!(
                url.starts_with("https://tile.loc.gov/")
                    && url.len() <= 512
                    && !url.chars().any(|c| c.is_control() || c.is_whitespace()),
                "invalid scan URL"
            );
        }
        if let Some(md5) = &self.scan.md5 {
            ensure!(
                md5.len() == 32 && md5.bytes().all(|b| b.is_ascii_hexdigit()),
                "invalid scan md5"
            );
        }
        ensure!(
            self.engine
                .image
                .strip_prefix("sha256:")
                .is_some_and(is_sha256),
            "worker image must be a full digest"
        );
        ensure!(
            self.engine
                .models
                .strip_prefix("sha256:")
                .is_some_and(|s| s.len() == 12 && s.bytes().all(|c| c.is_ascii_hexdigit())),
            "invalid profile digest"
        );
        let tag = self.engine_tag()?;
        ensure!(tag.pinned(), "unpinned engine");
        ensure!(
            self.engine.psm == 3 && self.engine.dpi == 300,
            "pilot uses PSM 3 and DPI 300"
        );
        ensure!(
            (1..=3600).contains(&self.engine.timeout_secs),
            "invalid page deadline"
        );
        Ok(())
    }
    pub fn engine_tag(&self) -> anyhow::Result<usnm_core::ocr_engine::OcrEngineTag> {
        use usnm_core::ocr_engine::{Digest, Fields, OcrEngineTag};
        let image = self
            .engine
            .image
            .strip_prefix("sha256:")
            .ok_or_else(|| anyhow::anyhow!("invalid image digest"))?;
        Ok(OcrEngineTag::new(Fields {
            family: "tesseract",
            version: &self.engine.version,
            profile: &self.engine.profile,
            image: Digest::short(image)?,
            models: Digest::short(&self.engine.models)?,
            route: "boinc",
            route_version: 1,
            contract: 0,
        })?)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultManifest {
    pub v: u32,
    pub job_id: String,
    pub scan_sha256: String,
    pub engine: Engine,
    pub text_sha256: String,
    pub alto_sha256: String,
    pub elapsed_ms: u64,
    pub completed_at: String,
}
impl ResultManifest {
    pub fn matches(&self, job: &Job, text: &[u8], alto: &[u8]) -> anyhow::Result<()> {
        job.validate()?;
        chrono::DateTime::parse_from_rfc3339(&self.completed_at)?;
        ensure!(
            self.v == VERSION
                && self.job_id == job.id
                && self.scan_sha256 == job.scan.sha256
                && self.engine == job.engine,
            "returned job/provenance mismatch"
        );
        ensure!(
            text.len() <= MAX_TEXT && alto.len() <= MAX_ALTO,
            "result over size limit"
        );
        ensure!(
            self.text_sha256 == sha256(text) && self.alto_sha256 == sha256(alto),
            "returned output checksum mismatch"
        );
        if std::str::from_utf8(text).is_err() {
            bail!("text is not UTF-8");
        }
        Ok(())
    }
}
