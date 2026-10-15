//! What stock `script_validator` runs for `usnm_qwen`.
//!
//! It passes a result's three files in output template order
//! (`readings.json`, `result.json`, `diag.txt`) and reads the exit status.
//! `check` looks at one result; with `--keep` it also copies the result
//! into that directory, named after its uploaded `result.json`. `compare`
//! looks at two results of one unit (per-crop word-bag Dice of at least
//! the threshold, both read or neither) and, with `--log`, appends what it
//! found to a JSON Lines file, so the threshold can be measured from every
//! compared pair.
use super::{DIAG_FILE, READINGS_FILE, RESULT_FILE};
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    io::Write as _,
    path::{Path, PathBuf},
};
use usnm_ocr_contract::qwen::{
    self, Readings, ResultManifest, MAX_DIAG, MAX_READINGS, MAX_RESULT_JSON,
};

pub struct Loaded {
    pub result: ResultManifest,
    pub readings: Readings,
    pub readings_bytes: Vec<u8>,
    pub diag: Vec<u8>,
}

pub fn load(files: &[PathBuf]) -> anyhow::Result<Loaded> {
    ensure!(
        files.len() == 3,
        "expected 3 output files ({READINGS_FILE}, {RESULT_FILE}, {DIAG_FILE}), got {}",
        files.len()
    );
    let readings_bytes = read(&files[0], MAX_READINGS)?;
    let result: ResultManifest =
        serde_json::from_slice(&read(&files[1], MAX_RESULT_JSON)?).context("result.json")?;
    let diag = read(&files[2], MAX_DIAG)?;
    let readings = result.check(&readings_bytes, &diag)?;
    Ok(Loaded {
        result,
        readings,
        readings_bytes,
        diag,
    })
}

pub fn check(files: &[PathBuf], keep: Option<&Path>) -> anyhow::Result<()> {
    let l = load(files)?;
    if let Some(dir) = keep {
        let name = crate::quality::validate::result_name(&files[1])?;
        let to = dir.join(&name);
        std::fs::create_dir_all(&to)?;
        atomic(&to.join(READINGS_FILE), &l.readings_bytes)?;
        atomic(
            &to.join(RESULT_FILE),
            &serde_json::to_vec_pretty(&l.result)?,
        )?;
        atomic(&to.join(DIAG_FILE), &l.diag)?;
    }
    Ok(())
}

pub fn compare(
    a: &[PathBuf],
    b: &[PathBuf],
    min_dice: f64,
    log: Option<&Path>,
) -> anyhow::Result<()> {
    let (la, lb) = (load(a)?, load(b)?);
    let (crops, verdict) =
        qwen::agree(&la.result, &la.readings, &lb.result, &lb.readings, min_dice);
    if let Some(log) = log {
        let line = serde_json::json!({
            "a": {"result": crate::quality::validate::result_name(&a[1])?, "device": la.result.device, "model": la.result.model, "gpu": la.result.host.gpu, "plan_class": la.result.host.plan_class},
            "b": {"result": crate::quality::validate::result_name(&b[1])?, "device": lb.result.device, "model": lb.result.model, "gpu": lb.result.host.gpu, "plan_class": lb.result.host.plan_class},
            "agree": verdict.is_ok(),
            "at": chrono::Utc::now().to_rfc3339(),
            "crops": crops,
            "job_id": la.result.job_id,
            "min_dice": min_dice,
            "outcomes": [la.result.outcome, lb.result.outcome],
            "reason": verdict.as_ref().err(),
        });
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)?;
        writeln!(f, "{line}")?;
    }
    verdict.map_err(|e| anyhow::anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use usnm_ocr_contract::{
        boinc::sha256,
        qwen::{Outcome, Reading, PROMPT_VERSION, VERSION},
    };

    fn write(dir: &Path, tag: &str, texts: &[(&str, &str)]) -> Vec<PathBuf> {
        let tmp = dir.join(format!("unit_{tag}"));
        let unit = dir.join(format!("q_{tag}"));
        crate::quality::work::tests::unit(&unit);
        let j = crate::qwen::stage::local_unit(
            &unit.join("page.jpg"),
            &unit.join("loc.xml"),
            &tmp,
            crate::qwen::stage::tests::engine(),
            1,
            4,
            600,
            4,
        )
        .unwrap();
        let readings = Readings {
            v: VERSION,
            job_id: j.id.clone(),
            device: "cuda".into(),
            diag: BTreeMap::new(),
            model: Some("qwen3-vl-8b-q4km".into()),
            outcome: Outcome::Ok,
            prompt_version: PROMPT_VERSION,
            readings: texts
                .iter()
                .map(|(id, t)| Reading {
                    id: id.to_string(),
                    ms: 1,
                    text: t.to_string(),
                    tokens: 3,
                })
                .collect(),
        };
        let rb = serde_json::to_vec(&readings).unwrap();
        let r = ResultManifest {
            v: VERSION,
            job_id: j.id,
            completed_at: "2026-10-14T00:00:00Z".into(),
            crops: j.crops.len() as u32,
            device: "cuda".into(),
            diag_bytes: 4,
            diag_sha256: sha256(b"diag"),
            engine: j.engine,
            engine_ms: 5,
            gpu_mem_peak_mib: None,
            host: Default::default(),
            load_ms: 1,
            model: Some("qwen3-vl-8b-q4km".into()),
            outcome: Outcome::Ok,
            peak_rss_kib: None,
            read: texts.len() as u32,
            readings_bytes: rb.len() as u64,
            readings_sha256: sha256(&rb),
            unit_error: None,
        };
        let files: Vec<PathBuf> = ["rd", "r", "d"]
            .iter()
            .map(|n| dir.join(format!("usnm_qwen_x_{tag}_{n}")))
            .collect();
        std::fs::write(&files[0], rb).unwrap();
        std::fs::write(&files[1], serde_json::to_vec(&r).unwrap()).unwrap();
        std::fs::write(&files[2], "diag").unwrap();
        files
    }

    #[test]
    fn keeps_results_and_logs_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(
            tmp.path(),
            "a",
            &[
                ("c01", "one two three four"),
                ("c02", "five six seven eight"),
            ],
        );
        let b = write(
            tmp.path(),
            "b",
            &[
                ("c01", "one two three five"),
                ("c02", "five six seven eight"),
            ],
        );
        let keep = tmp.path().join("seen");
        check(&a, Some(&keep)).unwrap();
        assert!(keep.join("usnm_qwen_x_a_r/readings.json").exists());
        let log = tmp.path().join("pairs.jsonl");
        compare(&a, &b, 0.7, Some(&log)).unwrap();
        assert!(compare(&a, &b, 0.9, Some(&log)).is_err());
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["crops"][0]["dice"], 0.75);
        assert_eq!(lines[1]["agree"], false);
        std::fs::write(&a[0], "tampered").unwrap();
        assert!(check(&a, None).is_err());
    }
}
