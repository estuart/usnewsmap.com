//! Get each sampled page's inputs and write its unit directory.
//!
//! Scans come from [`crate::scans`]: the scan archive first, then LoC's
//! storage service and image server through the page's batch. Nothing goes
//! to www.loc.gov's item API. LoC's ALTO is only on the storage service.
//!
//! Politeness, as for the #435 report's `fetch.py`: one request at a time
//! (every await finishes before the next request), at least `gap` apart on
//! the storage service and the image server, which share one pacer here,
//! and no retries. A 403, 429 or 5xx, a network failure, or an HTML page in
//! place of a file (a bot challenge) writes a hold
//! and stops the run; a later run refuses to start until a person removes
//! it. A 404 marks the page missing and moves on.
//!
//! A unit directory holds `page.jp2` (checked against the bag's md5),
//! `page.jpg` (OCR units), `loc.xml` (LoC's ALTO, kept on the server for
//! scoring; never sent to hosts) and `assess.json`, written last.
use super::{check_doc_id, check_no_hold, hold_path, units, SampleRow, JOB_FILE};
use crate::{atomic, scans};
use anyhow::{ensure, Context};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::{
    assess::{Format, Input, Job, Mode, Stratum, MAX_SCAN, VERSION},
    boinc::{self, sha256},
};
use usnm_ocr_worker::{engine, loc::FetchError};

pub const MIN_GAP: Duration = scans::MIN_GAP;
const MAX_ALTO: usize = 64 << 20;

#[derive(Debug, Clone)]
pub struct Options {
    pub sample: PathBuf,
    pub root: PathBuf,
    pub mode: Mode,
    pub engine_image: String,
    pub engine_base: String,
    pub engine_version: String,
    pub timeout_secs: u32,
    pub gap: Duration,
    pub max_pages: usize,
    pub batch_cache: Option<PathBuf>,
    /// The scan archive's container URL; None: LoC only.
    pub scan_archive: Option<String>,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub staged: usize,
    pub missing: usize,
    pub already: usize,
}

/// A failure that should stop the run (a block, a server error, the
/// network), as opposed to a page LoC does not have.
fn stop(e: FetchError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    ensure!(opts.gap >= MIN_GAP, "--gap-secs must be at least 5");
    ensure!(
        opts.engine_image
            .strip_prefix("sha256:")
            .is_some_and(boinc::is_sha256),
        "--engine-image must be the worker image's full sha256 digest"
    );
    check_no_hold(&opts.root)?;
    let rows: Vec<SampleRow> = super::read_sample(&opts.sample)?
        .into_iter()
        .filter(|r| r.mode == opts.mode)
        .collect();
    let dir = units(&opts.root, opts.mode);
    std::fs::create_dir_all(&dir)?;
    let installed: BTreeSet<String> = engine::MODELS_LOCK
        .lines()
        .filter(|s| !s.starts_with('#'))
        .filter_map(|s| s.split_whitespace().next())
        .filter_map(|s| s.strip_prefix("tessdata/")?.strip_suffix(".traineddata"))
        .map(str::to_owned)
        .collect();
    let src = scans::Sources::new(
        scans::loc_client(opts.gap, opts.batch_cache.clone())?,
        scans::archive(opts.scan_archive.as_deref())?,
        MAX_SCAN,
    );
    let mut summary = Summary::default();
    for row in rows {
        if summary.staged + summary.missing >= opts.max_pages {
            break;
        }
        check_doc_id(&row.doc_id)?;
        let unit = dir.join(&row.doc_id);
        if unit.join(JOB_FILE).exists() || unit.join("missing").exists() {
            summary.already += 1;
            continue;
        }
        std::fs::create_dir_all(&unit)?;
        match page(&src, &row, &unit, opts, &installed).await {
            Ok(true) => summary.staged += 1,
            Ok(false) => summary.missing += 1,
            Err(e) => {
                atomic(
                    &hold_path(&opts.root),
                    format!("{}: {e:#}\n", row.doc_id).as_bytes(),
                )?;
                return Err(e.context(format!("stopped at {}", row.doc_id)));
            }
        }
    }
    Ok(summary)
}

fn missing(unit: &std::path::Path, why: &str) -> anyhow::Result<bool> {
    atomic(&unit.join("missing"), format!("{why}\n").as_bytes())?;
    Ok(false)
}

async fn page(
    src: &scans::Sources,
    row: &SampleRow,
    unit: &std::path::Path,
    opts: &Options,
    installed: &BTreeSet<String>,
) -> anyhow::Result<bool> {
    // First, so a page LoC does not have still shows in the nonresponse list.
    atomic(&unit.join("sample.json"), &serde_json::to_vec(row)?)?;
    let key = PageKey::from_doc_id(&row.doc_id)?;
    let profile = engine::route(&row.lang, installed).context("no locked model for language")?;
    let (resolved, jp2) = match src.jp2(&key, &row.batch, row.batch_version).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            return missing(
                unit,
                "the batch does not place this page, or LoC has no JP2",
            )
        }
        Err(FetchError::Permanent(m)) => return missing(unit, &m),
        Err(e) => return Err(stop(e)),
    };
    let mut inputs = BTreeMap::new();
    ensure!(jp2.bytes.len() <= MAX_SCAN, "JP2 over size limit");
    atomic(&unit.join(Format::Jp2.input()), &jp2.bytes)?;
    inputs.insert(
        Format::Jp2,
        Input {
            url: jp2.url.clone(),
            sha256: sha256(&jp2.bytes),
            bytes: jp2.bytes.len() as u64,
        },
    );
    if opts.mode == Mode::Ocr {
        let jpg = match src.jpeg_of(&key, &row.batch, &resolved.iiif_base).await {
            Ok(Some(s)) => s,
            Ok(None) => return missing(unit, "LoC has no IIIF JPEG for this page"),
            Err(FetchError::Permanent(m)) => return missing(unit, &m),
            Err(e) => return Err(stop(e)),
        };
        ensure!(jpg.bytes.len() <= MAX_SCAN, "JPEG over size limit");
        atomic(&unit.join(Format::Jpeg.input()), &jpg.bytes)?;
        inputs.insert(
            Format::Jpeg,
            Input {
                url: jpg.url.clone(),
                sha256: sha256(&jpg.bytes),
                bytes: jpg.bytes.len() as u64,
            },
        );
        let alto_url = resolved
            .url
            .strip_suffix(".jp2")
            .map(|s| format!("{s}.xml"))
            .context("JP2 URL does not end in .jp2")?;
        match src.loc.storage_file(&alto_url, &key.lccn, MAX_ALTO).await {
            Ok(Some(b)) => atomic(&unit.join("loc.xml"), &b)?,
            // Scored pages need it; the unit still runs for crash and speed.
            Ok(None) | Err(FetchError::Permanent(_)) => {
                atomic(&unit.join("loc.xml.missing"), b"404\n")?
            }
            Err(e) => return Err(stop(e)),
        }
    }
    let mut job = Job {
        v: VERSION,
        id: String::new(),
        mode: opts.mode,
        doc_id: row.doc_id.clone(),
        batch: row.batch.clone(),
        batch_version: row.batch_version,
        language: row.lang.clone(),
        stratum: Stratum {
            crash_weight: row.crash_weight,
            damage: row.damage.clone(),
            decade: row.decade,
            lang: row.lang.clone(),
            pages: row.stratum_pages,
            prefix: row.prefix.clone(),
            weight: row.weight,
        },
        inputs,
        engine: boinc::Engine {
            image: opts.engine_image.clone(),
            version: opts.engine_version.clone(),
            profile: profile.clone(),
            models: engine::models_digest(engine::MODELS_LOCK, &profile)
                .context("unlocked model")?
                .to_string(),
            psm: 3,
            dpi: 300,
            timeout_secs: opts.timeout_secs,
        },
        engine_base: opts.engine_base.clone(),
    };
    job.id = job.identity()?;
    job.validate()?;
    atomic(&unit.join(JOB_FILE), &serde_json::to_vec_pretty(&job)?)?;
    Ok(true)
}
