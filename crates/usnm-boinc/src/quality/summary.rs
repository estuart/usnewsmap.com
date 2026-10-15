//! Reads back what the validator kept: one row per result, with the page,
//! host, outcome, time and each reader's estimate, plus every compared
//! pair's verdict.
use super::{units, JOB_FILE, RESULT_FILE};
use crate::read;
use std::{collections::BTreeMap, fmt::Write as _, path::Path};
use usnm_ocr_contract::quality::{Job, ResultManifest, MAX_JOB_JSON, MAX_RESULT_JSON};

fn opt(x: Option<f64>) -> String {
    x.map_or(String::new(), |v| format!("{v:.4}"))
}

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
        "| Result | Page | Group | Client | Plan class | Outcome | Seconds | Reader | Est. CER | Completeness | Findable content | Findable places | Unit error |\n|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
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
        let o = |s: &Option<String>| s.clone().unwrap_or_default();
        let head = format!(
            "| {name} | {doc} | {group} | {} | {} | {:?} | {:.1} |",
            o(&r.host.boinc_client),
            o(&r.host.plan_class),
            r.outcome,
            r.timings.total_ms as f64 / 1000.0
        );
        if r.readers.is_empty() {
            let _ = writeln!(out, "{head} | | | | | {} |", o(&r.unit_error));
        }
        for x in &r.readers {
            let _ = writeln!(
                out,
                "{head} {} | {} | {} | {} | {} | |",
                x.name,
                opt(x.est_cer),
                opt(x.completeness),
                opt(x.findable_content),
                opt(x.findable_places),
            );
        }
    }
    out.push_str("\n| Job | Page | A | B | Agree | Reason |\n|---|---|---|---|---|---|\n");
    if let Ok(text) = std::fs::read_to_string(pairs) {
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let job = v["job_id"].as_str().unwrap_or("");
            let doc = pages.get(job).map(|p| p.0.as_str()).unwrap_or("");
            let _ = writeln!(
                out,
                "| {}… | {doc} | {} | {} | {} | {} |",
                job.get(..12).unwrap_or(job),
                v["a"]["result"].as_str().unwrap_or(""),
                v["b"]["result"].as_str().unwrap_or(""),
                v["agree"],
                v["reason"].as_str().unwrap_or(""),
            );
        }
    }
    Ok(out)
}
