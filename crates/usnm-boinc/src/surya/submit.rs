//! Hand staged units to stock `create_work`, two results per unit, with the
//! assessment's write-ahead markers (`submitting`, then `submitted`).
use super::{units, APP, JOB_FILE, PAGE_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::path::PathBuf;
use tokio::process::Command;
use usnm_ocr_contract::surya::{Job, MAX_JSON, MAX_SCAN};

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

/// Round 1 (#370) took a median 18,753 CPU-seconds per tiled page. At a few
/// GFLOPS per core that is about 1e14 operations; the bound leaves room for
/// slow hosts, and each mode has its own deadline inside the unit anyway.
pub const FPOPS_EST: &str = "1e14";
pub const FPOPS_BOUND: &str = "2e16";
/// 8 GiB. Round 1 peaked at 11.8 GB with 8 threads on the whole node; the
/// CPU version's container is capped at 12 GiB and the GPU version's at
/// 8 GiB (`ocr/boinc/surya/`). BOINC compares this with the share of the
/// host's memory the volunteer allows, so 12 GiB would keep most 16 GB
/// GPU hosts out of the probe. Revisit with the probe's peaks.
pub const MEMORY_BOUND: &str = "8589934592";

pub fn create_work_args(job: &Job) -> Vec<String> {
    let mut args: Vec<String> = [
        "--appname",
        APP,
        "--wu_name",
        &job.workunit(),
        "--wu_template",
        "templates/usnm_surya_in",
        "--result_template",
        "templates/usnm_surya_out",
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
        "2147483648",
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
/// unit file each comes from.
pub fn physical_names(job: &Job) -> Vec<(String, &'static str)> {
    vec![
        (format!("{}_page_jpg", job.id), PAGE_FILE),
        (format!("{}_surya_json", job.id), JOB_FILE),
    ]
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
        let job: Job = serde_json::from_slice(&read(&unit.join(JOB_FILE), MAX_JSON)?)?;
        job.validate()?;
        let page = read(&unit.join(PAGE_FILE), MAX_SCAN)?;
        ensure!(
            page.len() as u64 == job.input.bytes
                && usnm_ocr_contract::boinc::sha256(&page) == job.input.sha256,
            "staged page changed for {}",
            job.doc_id
        );
        atomic(&unit.join("submitting"), job.workunit().as_bytes())?;
        for (physical, file) in physical_names(&job) {
            let limit = if file == JOB_FILE { MAX_JSON } else { MAX_SCAN };
            crate::assess::submit::stage_file(&project, &physical, &read(&unit.join(file), limit)?)
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
    fn two_inputs_in_template_order() {
        let j = super::super::work::tests::job();
        let names = physical_names(&j);
        assert_eq!(names[0].1, PAGE_FILE);
        assert_eq!(names[1].1, JOB_FILE);
        let args = create_work_args(&j);
        assert!(args.contains(&"templates/usnm_surya_in".to_string()));
        assert!(args.ends_with(&[names[0].0.clone(), names[1].0.clone()]));
    }
}
