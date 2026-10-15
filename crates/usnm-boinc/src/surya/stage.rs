//! Get each page's full-size IIIF JPEG and write its unit.
//!
//! The scan comes from [`crate::scans`]: the scan archive first, then
//! tile.loc.gov through the page's batch (bag manifest and METS on the
//! storage service, then the IIIF JPEG), never www.loc.gov's item API.
//! Politeness as for the assessment (`assess::stage`): one LoC request at a
//! time and at least `gap` (5 s or more) between any two; no retries; a
//! 403, 429 or 5xx, a network failure, or an HTML page in place of a file
//! writes a hold and stops the run. A page no source has is marked missing.
use super::{check_no_hold, check_word, units, Page, JOB_FILE, PAGE_FILE};
use crate::{atomic, scans};
use anyhow::{ensure, Context};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::{
    boinc::{is_sha256, sha256},
    surya::{Engine, Input, InputMode, Job, MAX_SCAN, VERSION},
};

pub const MIN_GAP: Duration = scans::MIN_GAP;

#[derive(Debug, Clone)]
pub struct Options {
    pub pages: Vec<Page>,
    pub root: PathBuf,
    pub engine: Engine,
    pub gpu_modes: Vec<InputMode>,
    pub cpu_modes: Vec<InputMode>,
    pub gpu_timeout_secs: u32,
    pub cpu_timeout_secs: u32,
    pub gap: Duration,
    pub max_pages: usize,
    /// The scan archive's container URL; None: LoC only.
    pub scan_archive: Option<String>,
    /// Where batch manifests and METS are kept between runs.
    pub batch_cache: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub staged: usize,
    pub missing: usize,
    pub already: usize,
}

pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    ensure!(opts.gap >= MIN_GAP, "--gap-secs must be at least 5");
    ensure!(
        opts.engine
            .image
            .strip_prefix("sha256:")
            .is_some_and(is_sha256),
        "--engine-image must be the worker image's full sha256 digest"
    );
    check_no_hold(&opts.root)?;
    let dir = units(&opts.root);
    std::fs::create_dir_all(&dir)?;
    let src = scans::Sources::new(
        scans::loc_client(opts.gap, opts.batch_cache.clone())?,
        scans::archive(opts.scan_archive.as_deref())?,
        MAX_SCAN,
    );
    let mut summary = Summary::default();
    for p in &opts.pages {
        if summary.staged + summary.missing >= opts.max_pages {
            break;
        }
        check_word(&p.group, "group")?;
        check_word(&p.lang, "language")?;
        let unit = dir.join(&p.doc_id);
        if unit.join(JOB_FILE).exists() || unit.join("missing").exists() {
            summary.already += 1;
            continue;
        }
        std::fs::create_dir_all(&unit)?;
        match page(&src, p, &unit, opts).await {
            Ok(true) => summary.staged += 1,
            Ok(false) => summary.missing += 1,
            Err(e) => {
                atomic(
                    &crate::assess::hold_path(&opts.root),
                    format!("{}: {e:#}\n", p.doc_id).as_bytes(),
                )?;
                return Err(e.context(format!("stopped at {}", p.doc_id)));
            }
        }
    }
    Ok(summary)
}

fn missing(unit: &Path, why: &str) -> anyhow::Result<bool> {
    atomic(&unit.join("missing"), format!("{why}\n").as_bytes())?;
    Ok(false)
}

async fn page(src: &scans::Sources, p: &Page, unit: &Path, opts: &Options) -> anyhow::Result<bool> {
    let key = PageKey::from_doc_id(&p.doc_id)?;
    let jpg = match src.jpeg(&key, &p.batches).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return missing(
                unit,
                "not in the scan archive, and none of its batches places it on tile.loc.gov",
            )
        }
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
    eprintln!(
        "{}: {} bytes from {}",
        p.doc_id,
        jpg.bytes.len(),
        if jpg.from_archive {
            "the scan archive"
        } else {
            "tile.loc.gov"
        }
    );
    ensure!(jpg.bytes.len() <= MAX_SCAN, "JPEG over size limit");
    atomic(&unit.join(PAGE_FILE), &jpg.bytes)?;
    let mut job = Job {
        v: VERSION,
        id: String::new(),
        doc_id: p.doc_id.clone(),
        group: p.group.clone(),
        language: p.lang.clone(),
        input: Input {
            bytes: jpg.bytes.len() as u64,
            sha256: sha256(&jpg.bytes),
            url: jpg.url.clone(),
        },
        engine: opts.engine.clone(),
        gpu_modes: opts.gpu_modes.clone(),
        cpu_modes: opts.cpu_modes.clone(),
        gpu_timeout_secs: opts.gpu_timeout_secs,
        cpu_timeout_secs: opts.cpu_timeout_secs,
    };
    job.id = job.identity()?;
    job.validate().context("staged job")?;
    atomic(&unit.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
    Ok(true)
}

/// Writes a unit from a local image, for checking a host or an image
/// without LoC or BOINC: `dir` gets `page.jpg` and `surya.json`, and
/// `usnm-boinc work --dir <dir>` then runs it. The URL is a placeholder.
pub fn local_unit(
    image: &Path,
    dir: &Path,
    engine: Engine,
    modes: Vec<InputMode>,
    timeout_secs: u32,
) -> anyhow::Result<Job> {
    let bytes = crate::read(image, MAX_SCAN)?;
    std::fs::create_dir_all(dir)?;
    atomic(&dir.join(PAGE_FILE), &bytes)?;
    let mut job = Job {
        v: VERSION,
        id: String::new(),
        doc_id: "local".into(),
        group: "local".into(),
        language: "und".into(),
        input: Input {
            bytes: bytes.len() as u64,
            sha256: sha256(&bytes),
            url: "https://tile.loc.gov/local".into(),
        },
        engine,
        gpu_modes: modes.clone(),
        cpu_modes: modes,
        gpu_timeout_secs: timeout_secs,
        cpu_timeout_secs: timeout_secs,
    };
    job.id = job.identity()?;
    job.validate()?;
    atomic(&dir.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
    Ok(job)
}
