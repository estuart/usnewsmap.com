//! The volunteer gold check (#457): score this host's engine on the public gold bundle and report
//! the scores with the bundle version, so results from different hosts and engine builds compare.
//!
//! Two steps, because the BOINC job container has no network (docs/boinc.md):
//! - `gold-stage` (network): verifies the bundle and downloads the original files it refers to
//!   (the BLN600 zip, Library of Congress page images) into a cache, checking each sha256. The
//!   project never ships them: BLN600 is No-Derivatives, and the page images come from LoC.
//! - `gold-run` (offline): verifies the bundle again, checks the cache, cuts the crops locally,
//!   runs Tesseract on each, scores CER/WER as gold.py does, and writes a report of counts only.
//!   The report holds no page text and no transcription.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use usnm_ocr_survey::{bundle, score};
use usnm_ocr_worker::engine::{self, Engine, Runner, Tesseract, TesseractOptions};

pub mod collect;
pub mod job;
pub mod prepare;
pub mod submit;
pub mod validate;
pub mod work;

/// The report's format version.
pub const REPORT_VERSION: u32 = 1;

/// Opens a bundle directory, or unpacks a bundle tar into `work`, and verifies it.
pub fn open(bundle_path: &Path, work: &Path) -> anyhow::Result<(PathBuf, bundle::Manifest)> {
    let dir = if bundle_path.is_file() {
        let dest = work.join("bundle");
        if dest.exists() {
            std::fs::remove_dir_all(&dest)?;
        }
        std::fs::create_dir_all(&dest)?;
        bundle::unpack(bundle_path, &dest)?
    } else {
        bundle_path.to_path_buf()
    };
    let m = bundle::verify(&dir).context("bundle verification failed")?;
    Ok((dir, m))
}

pub async fn stage(
    bundle_path: &Path,
    cache: &Path,
    work: &Path,
) -> anyhow::Result<bundle::Staged> {
    let (_, m) = open(bundle_path, work)?;
    bundle::stage(&m, cache, Duration::from_secs(1)).await
}

pub struct RunOptions {
    pub bundle: PathBuf,
    pub cache: PathBuf,
    pub work: PathBuf,
    pub tesseract: PathBuf,
    pub psm: u8,
    pub dpi: u32,
    pub timeout_secs: u32,
    /// Run at most this many crops (for a quick check); all when none.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineInfo {
    pub dpi: u32,
    pub name: String,
    pub psm: u8,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CropRun {
    #[serde(flatten)]
    pub result: bundle::CropResult,
    pub elapsed_ms: u64,
    pub profile: String,
}

/// gold-result.json. Counts only: no page text, no transcription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// Set when the report came from a BOINC unit; absent from a hand run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boinc: Option<BoincInfo>,
    pub bundle_version: u32,
    pub completed_at: String,
    pub crops: Vec<CropRun>,
    pub engine: EngineInfo,
    /// The `--limit` the run had; a limited run is partial and not comparable with a full one.
    pub limit: Option<usize>,
    pub manifest_sha256: String,
    pub report_version: u32,
    pub skipped: Vec<bundle::Skipped>,
    pub summary: Vec<bundle::SourceSummary>,
}

/// Who ran a BOINC unit, as the slot says. The host id is the client's own claim (from the
/// slot's `init_data.xml`), not proof of identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoincInfo {
    pub engine_base: String,
    pub engine_image: String,
    pub host_id: Option<u64>,
    pub job_id: String,
}

pub async fn run(opts: &RunOptions) -> anyhow::Result<Report> {
    std::fs::create_dir_all(&opts.work)?;
    let (dir, m) = open(&opts.bundle, &opts.work)?;
    let crops_dir = opts.work.join("crops");
    let (ready, mut skipped) = bundle::materialize(&m, &dir, &opts.cache, &crops_dir, opts.limit)?;
    let tess = Tesseract::detect(TesseractOptions {
        runner: Runner::Host(opts.tesseract.clone()),
        psm: opts.psm,
        dpi: opts.dpi,
        threads: 1,
        timeout: Duration::from_secs(opts.timeout_secs.into()),
    })
    .await?;
    let mut crops = Vec::new();
    for r in ready {
        let Some(profile) = engine::route(&r.lang, tess.installed()) else {
            skipped.push(bundle::Skipped {
                crop_id: r.crop_id,
                reason: format!("no installed model for {}", r.lang),
            });
            continue;
        };
        let scratch = tempfile::tempdir()?;
        let started = Instant::now();
        match tess.ocr(&r.image, &r.lang, scratch.path()).await {
            Ok(out) => crops.push(CropRun {
                result: bundle::CropResult {
                    counts: score::cer_wer(&r.transcription, &out.text),
                    crop_id: r.crop_id,
                    source: r.source,
                },
                elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                profile,
            }),
            Err(e) => skipped.push(bundle::Skipped {
                crop_id: r.crop_id,
                reason: format!("engine: {e}"),
            }),
        }
    }
    ensure!(
        !crops.is_empty(),
        "no crop could be run: {} skipped",
        skipped.len()
    );
    let results: Vec<_> = crops.iter().map(|c| c.result.clone()).collect();
    Ok(Report {
        boinc: None,
        bundle_version: m.bundle_version,
        completed_at: chrono::Utc::now().to_rfc3339(),
        summary: bundle::summarize(&results),
        crops,
        engine: EngineInfo {
            dpi: opts.dpi,
            name: tess.source().to_string(),
            psm: opts.psm,
            version: tess.version().to_string(),
        },
        limit: opts.limit,
        manifest_sha256: m.digest()?,
        report_version: REPORT_VERSION,
        skipped,
    })
}
