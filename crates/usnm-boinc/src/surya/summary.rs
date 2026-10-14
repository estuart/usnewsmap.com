//! Reads back what the validator kept: one row per result and mode, with
//! the page, host, device, time and memory, plus every compared pair's Dice.
use super::{units, JOB_FILE, RESULT_FILE};
use crate::read;
use std::{collections::BTreeMap, fmt::Write as _, path::Path};
use usnm_ocr_contract::surya::{Job, ResultManifest, MAX_JSON};

/// A Markdown report of the kept results and the logged pairs.
pub fn run(root: &Path, seen: &Path, pairs: &Path) -> anyhow::Result<String> {
    let mut pages: BTreeMap<String, (String, String)> = BTreeMap::new();
    if let Ok(dir) = std::fs::read_dir(units(root)) {
        for e in dir.filter_map(|e| e.ok()) {
            if let Ok(b) = read(&e.path().join(JOB_FILE), MAX_JSON) {
                if let Ok(j) = serde_json::from_slice::<Job>(&b) {
                    pages.insert(j.id.clone(), (j.doc_id, j.group));
                }
            }
        }
    }
    let mut out = String::from(
        "| Result | Page | Group | Client | Plan class | Runtime | GPU | Mode | Outcome | Device | GPU layers | Seconds | CPU s | Peak RSS MiB | GPU MiB | Tiles | Unit error |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
    );
    let mut names: Vec<_> = std::fs::read_dir(seen)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    names.sort();
    for dir in names {
        let Ok(b) = read(&dir.join(RESULT_FILE), MAX_JSON) else {
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
        let head = format!(
            "| {name} | {doc} | {group} | {} | {} | {} | {} |",
            o(&h.boinc_client),
            o(&h.plan_class),
            o(&h.runtime),
            o(&h.gpu)
        );
        if r.runs.is_empty() {
            let _ = writeln!(out, "{head} | | | | | | | | | | {} |", o(&r.unit_error));
        }
        for x in &r.runs {
            let _ = writeln!(
                out,
                "{head} {} | {:?} | {} | {} | {:.0} | {:.0} | {} | {} | {} | |",
                x.mode.name(),
                x.outcome,
                x.device,
                o(&x.gpu_layers),
                x.engine_ms as f64 / 1000.0,
                x.cpu_secs.unwrap_or(0.0),
                x.peak_rss_kib
                    .map_or(String::new(), |k| (k / 1024).to_string()),
                x.gpu_mem_peak_mib.map_or(String::new(), |m| m.to_string()),
                x.tiles.map_or(String::new(), |t| t.to_string()),
            );
        }
    }
    out.push_str("\n| Job | Page | A | B | Agree | Mode | Dice |\n|---|---|---|---|---|---|---|\n");
    if let Ok(text) = std::fs::read_to_string(pairs) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let job = v["job_id"].as_str().unwrap_or("");
            let doc = pages.get(job).map(|p| p.0.as_str()).unwrap_or("");
            let modes = v["modes"].as_array().cloned().unwrap_or_default();
            if modes.is_empty() {
                // Two unit errors, no mode in common, or different jobs.
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
            for m in &modes {
                let _ = writeln!(
                    out,
                    "| {}… | {doc} | {} | {} | {} | {} | {} |",
                    job.get(..12).unwrap_or(job),
                    v["a"]["device"],
                    v["b"]["device"],
                    v["agree"],
                    m["mode"].as_str().unwrap_or(""),
                    m["dice"]
                        .as_f64()
                        .map_or("n/a".into(), |d| format!("{d:.4}")),
                );
            }
        }
    }
    Ok(out)
}
