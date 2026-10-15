//! Reads back what the validator kept: one row per result, with the page,
//! host, device, model, outcome, times and crops read, plus every compared
//! pair's Dice per crop.
use super::{units, JOB_FILE, RESULT_FILE};
use crate::read;
use std::{collections::BTreeMap, fmt::Write as _, path::Path};
use usnm_ocr_contract::qwen::{Job, ResultManifest, MAX_JOB_JSON, MAX_RESULT_JSON};

/// A Markdown report of the kept results and the logged pairs.
pub fn run(root: &Path, seen: &Path, pairs: &Path) -> anyhow::Result<String> {
    let mut pages: BTreeMap<String, (String, String)> = BTreeMap::new();
    if let Ok(dir) = std::fs::read_dir(units(root)) {
        for e in dir.filter_map(|e| e.ok()) {
            if let Ok(b) = read(&e.path().join(JOB_FILE), MAX_JOB_JSON) {
                if let Ok(j) = serde_json::from_slice::<Job>(&b) {
                    pages.insert(j.id.clone(), (j.doc_id, j.group));
                }
            }
        }
    }
    let mut out = String::from(
        "| Result | Page | Group | Client | Plan class | Runtime | GPU | Outcome | Device | Model | Load s | Read s | Crops read | Unit error |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut names: Vec<_> = std::fs::read_dir(seen)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    names.sort();
    for dir in names {
        let Ok(b) = read(&dir.join(RESULT_FILE), MAX_RESULT_JSON) else {
            continue;
        };
        let Ok(r) = serde_json::from_slice::<ResultManifest>(&b) else {
            continue;
        };
        let name = dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let (doc, group) = pages.get(&r.job_id).cloned().unwrap_or_default();
        let h = &r.host;
        let o = |s: &Option<String>| s.clone().unwrap_or_default();
        let _ = writeln!(
            out,
            "| {name} | {doc} | {group} | {} | {} | {} | {} | {:?} | {} | {} | {:.1} | {:.1} | {}/{} | {} |",
            o(&h.boinc_client),
            o(&h.plan_class),
            o(&h.runtime),
            o(&h.gpu),
            r.outcome,
            r.device,
            o(&r.model),
            r.load_ms as f64 / 1000.0,
            r.engine_ms as f64 / 1000.0,
            r.read,
            r.crops,
            o(&r.unit_error)
        );
    }
    out.push_str("\n| Job | Page | A | B | Agree | Crop | Dice |\n|---|---|---|---|---|---|---|\n");
    if let Ok(text) = std::fs::read_to_string(pairs) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let job = v["job_id"].as_str().unwrap_or("");
            let doc = pages.get(job).map(|p| p.0.as_str()).unwrap_or("");
            let crops = v["crops"].as_array().cloned().unwrap_or_default();
            if crops.is_empty() {
                let _ = writeln!(
                    out,
                    "| {}… | {doc} | {} | {} | {} | {} | n/a |",
                    job.get(..12).unwrap_or(job),
                    v["a"]["device"],
                    v["b"]["device"],
                    v["agree"],
                    v["reason"].as_str().unwrap_or("none compared"),
                );
            }
            for c in &crops {
                let _ = writeln!(
                    out,
                    "| {}… | {doc} | {} | {} | {} | {} | {} |",
                    job.get(..12).unwrap_or(job),
                    v["a"]["device"],
                    v["b"]["device"],
                    v["agree"],
                    c["id"].as_str().unwrap_or(""),
                    c["dice"]
                        .as_f64()
                        .map_or("n/a".into(), |d| format!("{d:.4}")),
                );
            }
        }
    }
    Ok(out)
}
