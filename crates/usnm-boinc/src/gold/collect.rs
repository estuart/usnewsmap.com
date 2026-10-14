//! Turn the gold units' canonical results into per-host report tables.
//!
//! Reads the unit directories under the campaign root and the spool that stock
//! `batch_collect_assimilator` writes (`<workunit>__file_gold-result.json`), checks each result
//! again against the bundle's expectations, and writes to `--out`:
//!
//! - `reports.jsonl`: one line per result, keyed by bundle version, manifest sha256 and engine
//!   (version, image, base), with the host id the client claimed, the per-source and overall
//!   CER and WER, and how many crops were scored and skipped;
//! - `crops.jsonl`: one line per scored crop with the same keys and its counts.
//!
//! Counts only: no page text and no transcription is in the results or in these files.
use super::{submit, validate};
use crate::gold::job::{Job, JOB_FILE, MAX_JSON, RESULT_FILE};
use crate::{atomic, read};
use anyhow::Context;
use serde_json::json;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub project: PathBuf,
    pub spool: PathBuf,
    pub out: PathBuf,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub collected: usize,
    pub pending: usize,
    pub failed: usize,
}

pub fn run(opts: &Options) -> anyhow::Result<Summary> {
    let mut summary = Summary::default();
    let (mut reports, mut crops) = (String::new(), String::new());
    let dir = submit::units_dir(&opts.root);
    let mut units: Vec<_> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("submitted").exists())
        .collect();
    units.sort();
    let expected_dir = validate::expected_dir(&opts.project);
    for unit in units {
        let job: Job = serde_json::from_slice(&read(&unit.join(JOB_FILE), MAX_JSON)?)?;
        let wu = job.workunit();
        if opts.spool.join(format!("{wu}_error")).exists()
            || opts.spool.join(format!("{wu}_no_output_files")).exists()
        {
            summary.failed += 1;
            continue;
        }
        let file = opts.spool.join(format!("{wu}__file_{RESULT_FILE}"));
        if !file.exists() {
            summary.pending += 1;
            continue;
        }
        let r = validate::load(&file)?;
        let e = validate::load_expected(&expected_dir, &r.manifest_sha256)?;
        validate::check_report(&r, &e).with_context(|| format!("canonical result {wu}"))?;
        let b = r.boinc.as_ref().context("not a BOINC report")?;
        anyhow::ensure!(
            b.job_id == job.id,
            "{wu}: result does not belong to its job"
        );
        let key = json!({
            "bundle_version": r.bundle_version,
            "engine_base": b.engine_base,
            "engine_image": b.engine_image,
            "engine_version": r.engine.version,
            "host_id": b.host_id,
            "manifest_sha256": r.manifest_sha256,
            "workunit": wu,
        });
        let mut row = key.clone();
        row["completed_at"] = json!(r.completed_at);
        row["crops"] = json!(r.crops.len());
        row["skipped"] = json!(r.skipped.len());
        row["summary"] = serde_json::to_value(&r.summary)?;
        reports.push_str(&serde_json::to_string(&row)?);
        reports.push('\n');
        for c in &r.crops {
            let mut row = key.clone();
            row["crop"] = serde_json::to_value(c)?;
            crops.push_str(&serde_json::to_string(&row)?);
            crops.push('\n');
        }
        summary.collected += 1;
    }
    std::fs::create_dir_all(&opts.out)?;
    atomic(&opts.out.join("reports.jsonl"), reports.as_bytes())?;
    atomic(&opts.out.join("crops.jsonl"), crops.as_bytes())?;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gold::{submit, validate::tests};

    #[test]
    fn collects_valid_results_and_counts_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let root = tmp.path().join("root");
        let spool = tmp.path().join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let (e, _) = tests::setup(&validate::expected_dir(&project));
        let p = submit::tests::prepared();
        let jobs: Vec<_> = (0..3)
            .map(|n| submit::job(&p, &submit::tests::engine(), n).unwrap())
            .collect();
        validate::add_jobs(&validate::expected_dir(&project), &p, &jobs).unwrap();
        let e =
            validate::load_expected(&validate::expected_dir(&project), &e.manifest_sha256).unwrap();
        for j in &jobs {
            let d = submit::units_dir(&root).join(format!("{:04}", j.unit));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(JOB_FILE), serde_json::to_vec(j).unwrap()).unwrap();
            std::fs::write(d.join("submitted"), j.workunit()).unwrap();
        }
        std::fs::write(
            spool.join(format!("{}__file_{RESULT_FILE}", jobs[0].workunit())),
            serde_json::to_vec(&tests::report(&e, &jobs[0].id)).unwrap(),
        )
        .unwrap();
        std::fs::write(spool.join(format!("{}_error", jobs[1].workunit())), "").unwrap();
        let out = tmp.path().join("out");
        let s = run(&Options {
            root,
            project,
            spool,
            out: out.clone(),
        })
        .unwrap();
        assert_eq!((s.collected, s.failed, s.pending), (1, 1, 1));
        let rows = std::fs::read_to_string(out.join("reports.jsonl")).unwrap();
        assert_eq!(rows.lines().count(), 1);
        assert!(rows.contains("\"host_id\":7"));
        assert_eq!(
            std::fs::read_to_string(out.join("crops.jsonl"))
                .unwrap()
                .lines()
                .count(),
            2
        );
    }
}
