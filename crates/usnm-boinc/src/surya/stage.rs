//! Fetch each page's full-size IIIF JPEG from LoC and write its unit.
//!
//! Politeness as for the assessment (`assess::stage`): one request at a
//! time and at least `gap` (5 s or more) between any two requests, whichever
//! server they go to; no retries; a 403, 429 or 5xx, or a network failure,
//! writes a hold and stops the run. A 404 marks the page missing.
//!
//! Two requests per page: the issue's item JSON on loc.gov (for the page's
//! IIIF base), then the JPEG on tile.loc.gov.
use super::{check_no_hold, check_word, units, Page, JOB_FILE, PAGE_FILE};
use crate::atomic;
use anyhow::{ensure, Context};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::{
    boinc::{is_sha256, sha256},
    surya::{Engine, Input, InputMode, Job, MAX_SCAN, VERSION},
};
use usnm_ocr_worker::loc::{FetchError, ImageSize, LocClient, LocConfig};

pub const MIN_GAP: Duration = Duration::from_secs(5);

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
}

#[derive(Debug, Default)]
pub struct Summary {
    pub staged: usize,
    pub missing: usize,
    pub already: usize,
}

/// Waits until `gap` has passed since the last request of the run.
struct Gap {
    gap: Duration,
    last: Option<Instant>,
}
impl Gap {
    async fn wait(&mut self) {
        if let Some(last) = self.last {
            let next = last + self.gap;
            let now = Instant::now();
            if next > now {
                tokio::time::sleep(next - now).await;
            }
        }
    }
    fn done(&mut self) {
        self.last = Some(Instant::now());
    }
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
    let loc = LocClient::new(LocConfig {
        api_gap: opts.gap,
        tile_gap: opts.gap,
        storage_gap: opts.gap,
        storage_own_budget: false,
        attempts: 1,
        // Nothing more is sent after a 5xx: the run holds and stops.
        item_probes: false,
        ..LocConfig::default()
    })?;
    let mut gap = Gap {
        gap: opts.gap,
        last: None,
    };
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
        match page(&loc, &mut gap, p, &unit, opts).await {
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

async fn page(
    loc: &LocClient,
    gap: &mut Gap,
    p: &Page,
    unit: &Path,
    opts: &Options,
) -> anyhow::Result<bool> {
    let key = PageKey::from_doc_id(&p.doc_id)?;
    gap.wait().await;
    let base = loc.iiif_base_of(&key).await;
    gap.done();
    let base = match base {
        Ok(b) => b,
        Err(FetchError::Permanent(m)) => return missing(unit, &m),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
    gap.wait().await;
    let jpg = loc.iiif_scan(base, ImageSize::Full).await;
    gap.done();
    let jpg = match jpg {
        Ok(s) => s,
        Err(FetchError::Permanent(m)) => return missing(unit, &m),
        Err(e) => return Err(anyhow::anyhow!("{e}")),
    };
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
