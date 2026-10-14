//! What stock `script_validator` runs for `usnm_surya`.
//!
//! It passes a result's five files in output template order
//! (`text-tiled.txt`, `text-whole.txt`, `text-capped.txt`, `result.json`,
//! `diag.txt`) and reads the exit status. `check` looks at one result; with
//! `--keep` it also copies the result into that directory, named after its
//! uploaded `result.json`, so every host's timings and diagnostics are kept,
//! not just the canonical result's. `compare` looks at two results of one
//! unit and, with `--log`, appends what it found to a JSON Lines file, so
//! the cross-host threshold can be measured from every compared pair.
use super::DIAG_FILE;
use crate::{atomic, read};
use anyhow::{ensure, Context};
use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
};
use usnm_ocr_contract::surya::{
    self, InputMode, ResultManifest, MAX_DIAG, MAX_JSON, MAX_TEXT, MODES,
};

pub struct Loaded {
    pub result: ResultManifest,
    pub texts: BTreeMap<InputMode, Vec<u8>>,
    pub diag: Vec<u8>,
}

pub fn load(files: &[PathBuf]) -> anyhow::Result<Loaded> {
    ensure!(
        files.len() == 5,
        "expected 5 output files (three texts, result.json, {DIAG_FILE}), got {}",
        files.len()
    );
    let mut texts = BTreeMap::new();
    for (m, f) in MODES.iter().zip(files) {
        texts.insert(*m, read(f, MAX_TEXT)?);
    }
    let result: ResultManifest =
        serde_json::from_slice(&read(&files[3], MAX_JSON)?).context("result.json")?;
    let diag = read(&files[4], MAX_DIAG)?;
    result.check(&texts, &diag)?;
    Ok(Loaded {
        result,
        texts,
        diag,
    })
}

/// A name for the result from its uploaded `result.json` (BOINC names it
/// `<result name>_<n>`): letters, digits, `_` and `-` only.
fn result_name(file: &Path) -> anyhow::Result<String> {
    let n = file
        .file_name()
        .and_then(|n| n.to_str())
        .context("result file name")?;
    ensure!(
        !n.is_empty()
            && n.len() <= 200
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "unexpected result file name {n:?}"
    );
    Ok(n.to_owned())
}

pub fn check(files: &[PathBuf], keep: Option<&Path>) -> anyhow::Result<()> {
    let l = load(files)?;
    if let Some(dir) = keep {
        let name = result_name(&files[3])?;
        let to = dir.join(&name);
        std::fs::create_dir_all(&to)?;
        for (m, t) in &l.texts {
            atomic(&to.join(m.text()), t)?;
        }
        atomic(
            &to.join("result.json"),
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
    let (modes, verdict) = surya::agree(&la.result, &la.texts, &lb.result, &lb.texts, min_dice);
    if let Some(log) = log {
        let line = serde_json::json!({
            "a": {"result": result_name(&a[3])?, "device": devices(&la.result), "gpu": la.result.host.gpu, "plan_class": la.result.host.plan_class},
            "b": {"result": result_name(&b[3])?, "device": devices(&lb.result), "gpu": lb.result.host.gpu, "plan_class": lb.result.host.plan_class},
            "agree": verdict.is_ok(),
            "at": chrono::Utc::now().to_rfc3339(),
            "job_id": la.result.job_id,
            "min_dice": min_dice,
            "modes": modes,
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

/// The devices a result's runs used, in run order.
fn devices(r: &ResultManifest) -> Vec<String> {
    r.runs
        .iter()
        .map(|x| format!("{}:{}", x.mode.name(), x.device))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_ocr_contract::{
        boinc::sha256,
        surya::{Host, Outcome, Run, VERSION},
    };

    fn write(dir: &Path, tag: &str, device: &str, text: &str) -> Vec<PathBuf> {
        let j = super::super::work::tests::job();
        let r = ResultManifest {
            v: VERSION,
            job_id: j.id,
            engine: j.engine,
            host: Host::default(),
            unit_error: None,
            runs: vec![Run {
                code: Some(0),
                cpu_secs: Some(1.0),
                device: device.into(),
                engine_ms: 5,
                gpu_mem_peak_mib: None,
                gpu_layers: None,
                mode: InputMode::Tiled,
                outcome: Outcome::Ok,
                peak_rss_kib: None,
                signal: None,
                text_bytes: text.len() as u64,
                text_sha256: sha256(text.as_bytes()),
                tiles: Some(12),
            }],
            diag_bytes: 4,
            diag_sha256: sha256(b"diag"),
            completed_at: "2026-10-14T00:00:00Z".into(),
        };
        let files: Vec<PathBuf> = ["t", "w", "c", "r", "d"]
            .iter()
            .map(|n| dir.join(format!("usnm_surya_x_{tag}_{n}")))
            .collect();
        std::fs::write(&files[0], text).unwrap();
        std::fs::write(&files[1], "").unwrap();
        std::fs::write(&files[2], "").unwrap();
        std::fs::write(&files[3], serde_json::to_vec(&r).unwrap()).unwrap();
        std::fs::write(&files[4], "diag").unwrap();
        files
    }

    #[test]
    fn keeps_results_and_logs_pairs() {
        let tmp = tempfile::tempdir().unwrap();
        let a = write(tmp.path(), "a", "cuda", "one two three four");
        let b = write(tmp.path(), "b", "cpu", "one two three five");
        let keep = tmp.path().join("seen");
        check(&a, Some(&keep)).unwrap();
        assert!(keep.join("usnm_surya_x_a_r/result.json").exists());
        let log = tmp.path().join("pairs.jsonl");
        compare(&a, &b, 0.7, Some(&log)).unwrap();
        assert!(compare(&a, &b, 0.9, Some(&log)).is_err());
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["modes"][0]["dice"], 0.75);
        assert_eq!(lines[1]["agree"], false);
        std::fs::write(&a[0], "tampered").unwrap();
        assert!(check(&a, None).is_err());
    }
}
