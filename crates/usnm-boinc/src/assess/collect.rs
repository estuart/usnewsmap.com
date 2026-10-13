//! Turn canonical results into the inputs of the #435 report's scripts.
//!
//! Reads the unit directories under the staging root and the spool that
//! stock `batch_collect_assimilator` writes
//! (`<workunit>__file_<open name>`), and writes to `--out`:
//!
//! - `sample.jsonl`: OCR units, one line each, numbered from 1 (`n`)
//! - `<n>/loc.xml`, `<n>/out-jp2/page.txt`, `<n>/out-jpg/page.txt`
//! - `results.jsonl`: one line per OCR run, the fields `score.py` and
//!   `aggregate.py` read from `run_ocr.py`'s output
//! - `crash.jsonl`: the JP2 outcome of every unit (OCR and crash) that
//!   crashed or read, in `crash_scan.py`'s shape
//! - `nonresponse.jsonl`: sampled pages with no result (LoC had no file,
//!   BOINC gave up, or still pending), with their strata
//! - `units.jsonl`: every unit with its stratum and weights, and the outcome,
//!   time and memory per format, for corpus estimates
//!
//! Then `score.py --sample sample.jsonl --dir <out> --results results.jsonl`
//! and `aggregate.py --scores ... --results results.jsonl --crash crash.jsonl`
//! run unchanged.
use super::{units, validate, SampleRow, JOB_FILE};
use crate::{atomic, read};
use anyhow::Context;
use serde_json::json;
use std::path::{Path, PathBuf};
use usnm_ocr_contract::assess::{Format, Job, Mode, Outcome, MAX_JSON};

#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub spool: PathBuf,
    pub out: PathBuf,
}

#[derive(Debug, Default)]
pub struct Summary {
    pub ocr: usize,
    pub crash: usize,
    pub pending: usize,
    pub failed: usize,
    /// Units whose JP2 run ended neither in a crash nor a read.
    pub inconclusive: usize,
}

fn rc(o: Outcome, code: Option<i32>) -> i32 {
    match o {
        Outcome::Ok => 0,
        Outcome::Crash136 => 136,
        Outcome::Timeout => 124,
        Outcome::Other => code.filter(|c| *c != 0).unwrap_or(1),
    }
}

fn verdict(o: Outcome) -> &'static str {
    match o {
        Outcome::Crash136 => "crash_136",
        Outcome::Ok => "ok",
        Outcome::Timeout => "timeout",
        Outcome::Other => "other",
    }
}

fn spool_files(spool: &Path, workunit: &str) -> Vec<PathBuf> {
    ["text-jp2.txt", "text-jpeg.txt", "result.json"]
        .iter()
        .map(|n| spool.join(format!("{workunit}__file_{n}")))
        .collect()
}

/// The stratum a unit was drawn from.
fn stratum_key(s: &usnm_ocr_contract::assess::Stratum) -> (String, i32, String, String) {
    (s.lang.clone(), s.decade, s.prefix.clone(), s.damage.clone())
}

fn nonresponse(row: &SampleRow, status: &str) -> serde_json::Value {
    json!({
        "damage": row.damage, "decade": row.decade, "doc_id": row.doc_id, "lang": row.lang,
        "mode": row.mode, "prefix": row.prefix, "status": status,
    })
}

pub fn run(opts: &Options) -> anyhow::Result<Summary> {
    let mut summary = Summary::default();
    let (mut sample, mut results, mut crash, mut missing) =
        (String::new(), String::new(), String::new(), String::new());
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut n = 0usize;
    // Numbers move as pending units complete: start from an empty tree so
    // no file of an earlier run is scored against another page.
    if opts.out.exists() {
        for e in std::fs::read_dir(&opts.out)? {
            let p = e?.path();
            let numbered = p
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.parse::<usize>().is_ok());
            if numbered && p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            }
        }
    }
    for mode in [Mode::Ocr, Mode::Crash] {
        let dir = units(&opts.root, mode);
        if !dir.exists() {
            continue;
        }
        let mut unit_dirs: Vec<_> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.join("submitted").exists() || p.join("missing").exists())
            .collect();
        unit_dirs.sort();
        for unit in unit_dirs {
            if unit.join("missing").exists() {
                if let Ok(b) = read(&unit.join("sample.json"), MAX_JSON) {
                    let row: SampleRow = serde_json::from_slice(&b)?;
                    missing.push_str(&serde_json::to_string(&nonresponse(&row, "missing"))?);
                    missing.push('\n');
                }
                continue;
            }
            let job: Job = serde_json::from_slice(&read(&unit.join(JOB_FILE), MAX_JSON)?)?;
            let row: SampleRow =
                serde_json::from_slice(&read(&unit.join("sample.json"), MAX_JSON)?)?;
            let wu = job.workunit();
            if opts.spool.join(format!("{wu}_error")).exists()
                || opts.spool.join(format!("{wu}_no_output_files")).exists()
            {
                summary.failed += 1;
                missing.push_str(&serde_json::to_string(&nonresponse(&row, "failed"))?);
                missing.push('\n');
                continue;
            }
            let files = spool_files(&opts.spool, &wu);
            if !files.iter().all(|f| f.exists()) {
                summary.pending += 1;
                missing.push_str(&serde_json::to_string(&nonresponse(&row, "pending"))?);
                missing.push('\n');
                continue;
            }
            let got = validate::load(&files).with_context(|| format!("canonical result {wu}"))?;
            if got.result.job_id != job.id
                || got.result.mode != job.mode
                || got.result.engine != job.engine
            {
                anyhow::bail!("{wu}: result does not belong to its job");
            }
            n += 1;
            let jp2 = &got.result.runs[0];
            // aggregate.py counts every verdict but crash_136 as read; an
            // `other` exit (an out-of-memory kill, say) shows neither, so it
            // stays out of crash.jsonl and is counted in units.jsonl only.
            if jp2.outcome == Outcome::Other {
                summary.inconclusive += 1;
            } else {
                crash.push_str(&serde_json::to_string(&json!({
                    "n": n, "doc_id": job.doc_id, "lang": job.language, "decade": row.decade,
                    "batch": job.batch, "bytes": job.inputs[&Format::Jp2].bytes,
                    "wall": jp2.engine_ms as f64 / 1000.0, "rc": rc(jp2.outcome, jp2.code),
                    "verdict": verdict(jp2.outcome), "tail": jp2.stderr_tail,
                }))?);
                crash.push('\n');
            }
            all.push(json!({
                "n": n, "doc_id": job.doc_id, "mode": job.mode, "stratum": job.stratum,
                "engine": job.engine, "runs": got.result.runs,
            }));
            if mode == Mode::Crash {
                summary.crash += 1;
                continue;
            }
            summary.ocr += 1;
            let mut s = serde_json::to_value(&row)?;
            s["n"] = json!(n);
            // score.py and aggregate.py read `damage` as LoC's rate.
            s["damage"] = json!(row.damage_rate);
            s["damage_bin"] = json!(row.damage);
            sample.push_str(&serde_json::to_string(&s)?);
            sample.push('\n');
            let page = opts.out.join(n.to_string());
            if unit.join("loc.xml").exists() {
                atomic(&page.join("loc.xml"), &std::fs::read(unit.join("loc.xml"))?)?;
            }
            for r in &got.result.runs {
                atomic(
                    &page
                        .join(format!("out-{}", r.format.score_name()))
                        .join("page.txt"),
                    &got.texts[&r.format],
                )?;
                let secs = r.engine_ms as f64 / 1000.0;
                results.push_str(&serde_json::to_string(&json!({
                    "n": n, "fmt": r.format.score_name(), "rc": rc(r.outcome, r.code),
                    "chars": String::from_utf8_lossy(&got.texts[&r.format]).chars().count(),
                    "status": verdict(r.outcome), "wall": secs, "secs": secs,
                    "stderr": r.stderr_tail, "node": "boinc", "load_before": null,
                    "image_bytes": job.inputs[&r.format].bytes, "peak_rss_kib": r.peak_rss_kib,
                }))?);
                results.push('\n');
            }
        }
    }
    // Weights over the units that came back, so pages LoC did not have and
    // units BOINC gave up on do not leave their strata under-represented.
    let mut back: std::collections::BTreeMap<_, (usize, usize)> = Default::default();
    for u in &all {
        let s: usnm_ocr_contract::assess::Stratum = serde_json::from_value(u["stratum"].clone())?;
        let e = back.entry(stratum_key(&s)).or_default();
        e.1 += 1;
        if u["mode"] == json!(Mode::Ocr) {
            e.0 += 1;
        }
    }
    let mut units = String::new();
    for u in &mut all {
        let s: usnm_ocr_contract::assess::Stratum = serde_json::from_value(u["stratum"].clone())?;
        let (o, c) = back[&stratum_key(&s)];
        let pages = s.pages as f64;
        u["realized_crash_weight"] = json!(pages / c as f64);
        u["realized_weight"] = if o > 0 {
            json!(pages / o as f64)
        } else {
            json!(null)
        };
        units.push_str(&serde_json::to_string(u)?);
        units.push('\n');
    }
    std::fs::create_dir_all(&opts.out)?;
    for (name, text) in [
        ("nonresponse.jsonl", &missing),
        ("sample.jsonl", &sample),
        ("results.jsonl", &results),
        ("crash.jsonl", &crash),
        ("units.jsonl", &units),
    ] {
        atomic(&opts.out.join(name), text.as_bytes())?;
    }
    Ok(summary)
}
