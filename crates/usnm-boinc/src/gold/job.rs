//! File-based jobs for the volunteer gold check (#457, #462): one workunit scores the host's
//! engine on the public gold bundle and returns counts only (no page text, no transcription).
//!
//! The workunit's inputs are the job (`gold.json`), the bundle tar, and the original files the
//! bundle names (the BLN600 zip, Library of Congress page images). The BOINC client downloads
//! the originals from their publishers, not from the project server, and keeps them, and the
//! tar, as sticky files, so a host fetches each once. The run step has no network.
use anyhow::ensure;
use serde::{Deserialize, Serialize};
use usnm_ocr_contract::boinc::{is_sha256, sha256};

pub const VERSION: u32 = 1;
/// The job's open name in the slot. Its presence makes `usnm-boinc work` run a gold unit.
pub const JOB_FILE: &str = "gold.json";
/// The bundle tar's open name in the slot.
pub const BUNDLE_FILE: &str = "bundle.tar";
/// The report's open name in the slot; the one output file.
pub const RESULT_FILE: &str = "gold-result.json";
pub const MAX_JSON: usize = 64 * 1024;
/// The output template's limit on the report.
pub const MAX_RESULT: usize = 256 * 1024;
/// The largest bundle tar (`usnm_ocr_survey::bundle::MAX_BUNDLE`).
pub const MAX_BUNDLE: u64 = 256 << 20;
/// The largest original file (`usnm_ocr_survey::bundle::MAX_REMOTE`).
pub const MAX_REMOTE: u64 = 256 << 20;

/// A file of the unit, by size and sha256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub bytes: u64,
    /// The open name in the slot.
    pub name: String,
    pub sha256: String,
}

/// The engine build the unit was made for. Language profiles are chosen per crop by the
/// worker's router, from the models the image has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Engine {
    /// The Tesseract image the worker image was built on (its `TESSERACT_IMAGE`).
    pub base: String,
    pub dpi: u16,
    /// Full digest of the released BOINC worker image: the operator's record.
    pub image: String,
    pub psm: u8,
    /// Per crop.
    pub timeout_secs: u32,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub bundle: File,
    pub bundle_version: u32,
    pub engine: Engine,
    pub id: String,
    pub manifest_sha256: String,
    /// The original files, as the bundle's cache names them (`<sha256>.zip`, `<sha256>.jpg`).
    pub remotes: Vec<File>,
    /// Which of the campaign's units this is, so units of one campaign differ.
    pub unit: u32,
    pub v: u32,
}

/// The cache name of an original file, as `usnm_ocr_survey::bundle::cache_path` writes it.
pub fn remote_name_ok(name: &str) -> bool {
    name.split_once('.')
        .is_some_and(|(h, ext)| is_sha256(h) && matches!(ext, "zip" | "jpg"))
}

fn pinned(s: &str) -> bool {
    s.rsplit_once("sha256:").is_some_and(|(_, d)| is_sha256(d))
        && s.len() <= 256
        && !s.chars().any(|c| c.is_control() || c.is_whitespace())
}

impl Job {
    pub fn identity(&self) -> anyhow::Result<String> {
        Ok(sha256(&serde_json::to_vec(&(
            self.v,
            self.unit,
            self.bundle_version,
            &self.manifest_sha256,
            &self.bundle,
            &self.remotes,
            &self.engine,
        ))?))
    }
    pub fn workunit(&self) -> String {
        format!("usnm_gold_{}", self.id)
    }
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.v == VERSION, "unsupported gold job version");
        ensure!(
            is_sha256(&self.id) && self.id == self.identity()?,
            "job identity mismatch"
        );
        ensure!(self.bundle_version > 0, "invalid bundle version");
        ensure!(
            is_sha256(&self.manifest_sha256),
            "invalid manifest checksum"
        );
        ensure!(
            self.bundle.name == BUNDLE_FILE
                && is_sha256(&self.bundle.sha256)
                && (1..=MAX_BUNDLE).contains(&self.bundle.bytes),
            "invalid bundle file"
        );
        ensure!(self.remotes.len() <= 1000, "too many original files");
        let mut names = std::collections::BTreeSet::new();
        for r in &self.remotes {
            ensure!(
                remote_name_ok(&r.name)
                    && is_sha256(&r.sha256)
                    && r.name.starts_with(&r.sha256)
                    && (1..=MAX_REMOTE).contains(&r.bytes),
                "invalid original file {:?}",
                r.name
            );
            ensure!(names.insert(&r.name), "duplicate original file {}", r.name);
        }
        ensure!(
            self.engine
                .image
                .strip_prefix("sha256:")
                .is_some_and(is_sha256),
            "engine image must be a full digest"
        );
        ensure!(
            pinned(&self.engine.base),
            "engine base must be pinned by full digest"
        );
        ensure!(
            self.engine.psm == 3 && self.engine.dpi == 300,
            "the gold check uses the production profile: PSM 3, DPI 300"
        );
        ensure!(
            (1..=3600).contains(&self.engine.timeout_secs),
            "invalid crop deadline"
        );
        ensure!(
            !self.engine.version.is_empty()
                && self.engine.version.len() <= 32
                && self
                    .engine
                    .version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
            "invalid engine version"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn job() -> Job {
        let mut j = Job {
            bundle: File {
                bytes: 10,
                name: BUNDLE_FILE.into(),
                sha256: "a".repeat(64),
            },
            bundle_version: 1,
            engine: Engine {
                base: format!("docker.io/x@sha256:{}", "b".repeat(64)),
                dpi: 300,
                image: format!("sha256:{}", "c".repeat(64)),
                psm: 3,
                timeout_secs: 300,
                version: "5.5.0".into(),
            },
            id: String::new(),
            manifest_sha256: "d".repeat(64),
            remotes: vec![File {
                bytes: 5,
                name: format!("{}.zip", "e".repeat(64)),
                sha256: "e".repeat(64),
            }],
            unit: 0,
            v: VERSION,
        };
        j.id = j.identity().unwrap();
        j
    }

    #[test]
    fn identity_and_names() {
        let j = job();
        j.validate().unwrap();
        let mut k = j.clone();
        k.unit = 1;
        assert_ne!(k.identity().unwrap(), j.id);
        assert!(k.validate().is_err());
        let mut bad = j.clone();
        bad.remotes[0].name = "../x.zip".into();
        bad.id = bad.identity().unwrap();
        assert!(bad.validate().is_err());
        assert!(!remote_name_ok(&format!("{}.png", "e".repeat(64))));
        for sha in [
            "e".repeat(63),
            "e".repeat(65),
            "E".repeat(64),
            "g".repeat(64),
            String::new(),
        ] {
            let mut bad = j.clone();
            bad.remotes[0].name = format!("{}.zip", "e".repeat(64));
            bad.remotes[0].sha256 = sha.clone();
            bad.id = bad.identity().unwrap();
            assert!(bad.validate().is_err(), "remote sha256 {sha:?}");
        }
    }
}
