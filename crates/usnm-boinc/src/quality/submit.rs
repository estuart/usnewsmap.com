//! Hand staged units to stock `create_work`, two results per unit, with the
//! assessment's write-ahead markers (`submitting`, then `submitted`).
//!
//! `places.txt` is the same file for every unit of a campaign: its physical
//! name is its checksum, so the download tree holds it once and a host
//! downloads it once (the input template marks it sticky).
use super::{units, APP, JOB_FILE, PLACES_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::path::PathBuf;
use tokio::process::Command;
use usnm_ocr_contract::quality::{Job, MAX_ALTO, MAX_JOB_JSON, MAX_PLACES, MAX_SCAN, MAX_TEXTS};

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub project: PathBuf,
    pub max_units: usize,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub submitted: usize,
    pub uncertain: usize,
}

/// One page: a few alignments of some thousand words and one pass over a
/// downscaled image, well under a minute on one core. UNVERIFIED on
/// volunteers' hosts until the first units return.
pub const FPOPS_EST: &str = "1e12";
pub const FPOPS_BOUND: &str = "1e14";
/// 1.5 GiB, the `docker` plan class's container (`ocr/boinc/job.toml`).
pub const MEMORY_BOUND: &str = "1610612736";

pub fn create_work_args(job: &Job) -> Vec<String> {
    let mut args: Vec<String> = [
        "--appname",
        APP,
        "--wu_name",
        &job.workunit(),
        "--wu_template",
        "templates/usnm_quality_in",
        "--result_template",
        "templates/usnm_quality_out",
        "--min_quorum",
        "2",
        "--target_nresults",
        "2",
        "--max_error_results",
        "3",
        "--max_total_results",
        "4",
        "--max_success_results",
        "3",
        "--rsc_fpops_est",
        FPOPS_EST,
        "--rsc_fpops_bound",
        FPOPS_BOUND,
        "--rsc_memory_bound",
        MEMORY_BOUND,
        "--rsc_disk_bound",
        "1073741824",
        "--delay_bound",
        "259200",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(physical_names(job).into_iter().map(|(p, _)| p));
    args
}

/// Physical names in the download tree, in input template order, with the
/// unit file each comes from. The places file is named by its checksum,
/// shared across the campaign.
pub fn physical_names(job: &Job) -> Vec<(String, &'static str)> {
    vec![
        (format!("{}_page_jpg", job.id), super::PAGE_FILE),
        (format!("{}_loc_xml", job.id), super::ALTO_FILE),
        (format!("{}_texts_json", job.id), super::TEXTS_FILE),
        (
            format!("usnm_quality_places_{}", job.inputs.places.sha256),
            PLACES_FILE,
        ),
        (format!("{}_quality_json", job.id), JOB_FILE),
    ]
}

fn limit(file: &str) -> usize {
    match file {
        super::PAGE_FILE => MAX_SCAN,
        super::ALTO_FILE => MAX_ALTO,
        super::TEXTS_FILE => MAX_TEXTS,
        PLACES_FILE => MAX_PLACES,
        _ => MAX_JOB_JSON,
    }
}

pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    let project = opts.project.canonicalize()?;
    let dir = units(&opts.root);
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {dir:?}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join(JOB_FILE).exists())
        .map(|e| e.path())
        .collect();
    names.sort();
    let mut summary = Summary::default();
    for unit in names {
        if summary.submitted >= opts.max_units {
            break;
        }
        if unit.join("submitted").exists() {
            continue;
        }
        if unit.join("submitting").exists() {
            summary.uncertain += 1;
            continue;
        }
        let job: Job = serde_json::from_slice(&read(&unit.join(JOB_FILE), MAX_JOB_JSON)?)?;
        job.validate()?;
        for (file, input) in [
            (super::PAGE_FILE, &job.inputs.page),
            (super::ALTO_FILE, &job.inputs.alto),
            (super::TEXTS_FILE, &job.inputs.texts),
            (PLACES_FILE, &job.inputs.places),
        ] {
            ensure!(
                input.matches(&read(&unit.join(file), limit(file))?),
                "staged {file} changed for {}",
                job.doc_id
            );
        }
        atomic(&unit.join("submitting"), job.workunit().as_bytes())?;
        for (physical, file) in physical_names(&job) {
            crate::assess::submit::stage_file(
                &project,
                &physical,
                &read(&unit.join(file), limit(file))?,
            )
            .await?;
        }
        let status = Command::new(project.join("bin/create_work"))
            .current_dir(&project)
            .args(create_work_args(&job))
            .status()
            .await?;
        ensure!(
            status.success(),
            "create_work failed for {}; {} is uncertain until checked",
            job.workunit(),
            unit.display()
        );
        std::fs::rename(unit.join("submitting"), unit.join("submitted"))?;
        summary.submitted += 1;
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_inputs_in_template_order_with_a_shared_places_file() {
        let tmp = tempfile::tempdir().unwrap();
        let j = crate::quality::work::tests::unit(tmp.path());
        let names = physical_names(&j);
        let files: Vec<&str> = names.iter().map(|(_, f)| *f).collect();
        assert_eq!(files, super::super::inputs());
        assert!(names[3].0.starts_with("usnm_quality_places_"));
        assert!(!names[3].0.contains(&j.id));
        let args = create_work_args(&j);
        assert!(args.contains(&"templates/usnm_quality_in".to_string()));
        assert!(args.ends_with(&[names[3].0.clone(), names[4].0.clone()]));
    }
}
