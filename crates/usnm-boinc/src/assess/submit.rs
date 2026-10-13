//! Hand staged units to stock `create_work`, two results per unit.
//!
//! Write-ahead, like the pilot's submit: `submitting` before `create_work`,
//! `submitted` after. A unit left at `submitting` (the run died, or
//! `create_work` failed) is skipped until a person checks the server for
//! its workunit name and either removes `submitting` (absent) or renames
//! it to `submitted` (present).
use super::{units, APP, JOB_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use usnm_ocr_contract::assess::{Job, Mode, MAX_JSON};

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub project: PathBuf,
    pub mode: Mode,
    pub max_units: usize,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub submitted: usize,
    pub uncertain: usize,
}

/// The `create_work` arguments that make the unit: redundancy 2 with
/// room for a third and fourth result when the first two disagree.
pub fn create_work_args(job: &Job) -> Vec<String> {
    let (template, fpops) = match job.mode {
        Mode::Ocr => ("templates/usnm_assess_in", "2e13"),
        Mode::Crash => ("templates/usnm_assess_crash_in", "2e11"),
    };
    let mut args: Vec<String> = [
        "--appname",
        APP,
        "--wu_name",
        &job.workunit(),
        "--wu_template",
        template,
        "--result_template",
        "templates/usnm_assess_out",
        "--min_quorum",
        "2",
        "--target_nresults",
        "2",
        "--max_error_results",
        "4",
        "--max_total_results",
        "6",
        "--max_success_results",
        "4",
        "--rsc_fpops_est",
        fpops,
        "--rsc_fpops_bound",
        "1e15",
        "--rsc_memory_bound",
        "1610612736",
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

/// Physical file names in the download tree, in input template order,
/// with the unit file each one comes from.
pub fn physical_names(job: &Job) -> Vec<(String, &'static str)> {
    let mut out: Vec<(String, &'static str)> = job
        .formats()
        .into_iter()
        .map(|f| {
            (
                format!("{}_{}", job.id, f.input().replace('.', "_")),
                f.input(),
            )
        })
        .collect();
    out.push((format!("{}_assess_json", job.id), JOB_FILE));
    out
}

async fn stage_file(project: &Path, physical: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let output = Command::new(project.join("bin/dir_hier_path"))
        .arg(physical)
        .current_dir(project)
        .output()
        .await?;
    ensure!(output.status.success(), "dir_hier_path failed");
    let path = String::from_utf8(output.stdout)?;
    let path = Path::new(path.trim());
    ensure!(!path.as_os_str().is_empty(), "empty staging path");
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        project.join(path)
    };
    atomic(&path, bytes)?;
    #[cfg(unix)]
    {
        // Apache serves the download tree through the BOINC group.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664))?;
    }
    Ok(())
}

pub async fn run(opts: &Options) -> anyhow::Result<Summary> {
    let project = opts.project.canonicalize()?;
    let dir = units(&opts.root, opts.mode);
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
        for f in job.formats() {
            let i = &job.inputs[&f];
            let b = read(&unit.join(f.input()), usnm_ocr_contract::assess::MAX_SCAN)?;
            ensure!(
                b.len() as u64 == i.bytes && usnm_ocr_contract::boinc::sha256(&b) == i.sha256,
                "staged {:?} changed for {}",
                f,
                job.doc_id
            );
        }
        atomic(&unit.join("submitting"), job.workunit().as_bytes())?;
        for (physical, file) in physical_names(&job) {
            let limit = if file == JOB_FILE {
                MAX_JSON
            } else {
                usnm_ocr_contract::assess::MAX_SCAN
            };
            stage_file(&project, &physical, &read(&unit.join(file), limit)?).await?;
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
    use usnm_ocr_contract::assess::Format;
    #[test]
    fn crash_units_send_the_jp2_only() {
        let j = super::super::work::tests::job(Mode::Crash);
        let names = physical_names(&j);
        assert_eq!(names.len(), 2);
        assert!(names[0].0.ends_with("_page_jp2"));
        assert!(create_work_args(&j).contains(&"templates/usnm_assess_crash_in".to_string()));
        let o = super::super::work::tests::job(Mode::Ocr);
        assert_eq!(physical_names(&o)[1].1, Format::Jpeg.input());
    }
}
