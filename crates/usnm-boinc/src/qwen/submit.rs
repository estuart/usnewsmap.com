//! Hand staged units to stock `create_work`, two results per unit, with the
//! assessment's write-ahead markers (`submitting`, then `submitted`).
use super::{units, APP, JOB_FILE, PAGE_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::path::PathBuf;
use tokio::process::Command;
use usnm_ocr_contract::{
    quality::MAX_SCAN,
    qwen::{Job, MAX_JOB_JSON},
};

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

/// Twelve crops through an 8B model at Q4: a few hundred prefill tokens
/// and a few hundred output tokens each, about 1e13 operations. UNVERIFIED
/// until the probe measures it; each crop has the unit's deadline anyway.
pub const FPOPS_EST: &str = "1e13";
pub const FPOPS_BOUND: &str = "1e16";
/// 6 GiB: the 8B model at Q4 needs about 7 GB of GPU memory and 4 GiB of
/// RAM; the 2B model at Q8 about 3 GiB of RAM (`ocr/boinc/qwen/`).
pub const MEMORY_BOUND: &str = "6442450944";

pub fn create_work_args(job: &Job) -> Vec<String> {
    let mut args: Vec<String> = [
        "--appname",
        APP,
        "--wu_name",
        &job.workunit(),
        "--wu_template",
        "templates/usnm_qwen_in",
        "--result_template",
        "templates/usnm_qwen_out",
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
        (format!("{}_qwen_json", job.id), JOB_FILE),
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
        let job: Job = serde_json::from_slice(&read(&unit.join(JOB_FILE), MAX_JOB_JSON)?)?;
        job.validate()?;
        let page = read(&unit.join(PAGE_FILE), MAX_SCAN)?;
        ensure!(
            job.input.matches(&page),
            "staged page changed for {}",
            job.doc_id
        );
        atomic(&unit.join("submitting"), job.workunit().as_bytes())?;
        for (physical, file) in physical_names(&job) {
            let limit = if file == JOB_FILE {
                MAX_JOB_JSON
            } else {
                MAX_SCAN
            };
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
        let tmp = tempfile::tempdir().unwrap();
        let unit = tmp.path().join("q");
        crate::quality::work::tests::unit(&unit);
        let j = super::super::stage::local_unit(
            &unit.join("page.jpg"),
            &unit.join("loc.xml"),
            &tmp.path().join("u"),
            super::super::stage::tests::engine(),
            1,
            4,
            600,
            4,
        )
        .unwrap();
        let names = physical_names(&j);
        assert_eq!(names[0].1, PAGE_FILE);
        assert_eq!(names[1].1, JOB_FILE);
        let args = create_work_args(&j);
        assert!(args.contains(&"templates/usnm_qwen_in".to_string()));
        assert!(args.ends_with(&[names[0].0.clone(), names[1].0.clone()]));
    }
}
