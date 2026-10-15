//! Calibration on gold crops: the estimator's error where the true CER is
//! known.
//!
//! Takes gold-style `crops.jsonl` rows that have a transcription, one
//! hypothesis text per reader per crop (`--hyp ENGINE=FILE.jsonl`, the
//! spec `usnm-ocr-survey score` reads) and optional vision-language
//! readings, runs the hat per crop over the readers, and reports per
//! reader: true CER, estimated CER, bias (mean of estimated less true),
//! MAE, Pearson and Spearman correlation, and the same with the
//! vision-language reader excluded. Output: `calibration.csv`,
//! `calibration.md` and `by-crop.csv`.
//!
//! Crops `usnm-ocr-survey score` would leave out (no transcription, no
//! transcriber, too many unknowns) are left out here too. A crop enters
//! the hat only when at least three readers have a text for it; a reader
//! enters the hat with its text normalised as the pairwise metrics
//! normalise it, and its true CER is `score::cer_wer` against the
//! transcription.
use crate::atomic;
use anyhow::Context;
use std::{collections::BTreeMap, fmt::Write as _, path::PathBuf};
use usnm_ocr_quality::{align, hat, report::round6};
use usnm_ocr_survey::score;

/// The vision-language reader's name.
pub const VLM: &str = "vlm";

#[derive(Debug, Clone)]
pub struct Options {
    pub crops: PathBuf,
    /// `ENGINE=FILE.jsonl`, rows `{"crop_id", "text"}`.
    pub hyps: Vec<String>,
    /// Readings as the `usnm_qwen` app returns them, keyed by crop id
    /// (`{"crop_id", "text"}` rows), as one more reader named `vlm`.
    pub readings: Option<PathBuf>,
    pub out: PathBuf,
}

/// One reader's calibration row.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Row {
    pub reader: String,
    pub crops: usize,
    pub true_cer: f64,
    pub est_cer: f64,
    pub bias: f64,
    pub mae: f64,
    pub pearson: Option<f64>,
    pub spearman: Option<f64>,
    /// With the vision-language reader left out of the hat.
    pub without_vlm: bool,
}

/// One crop's estimate for one reader.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CropEstimate {
    pub crop_id: String,
    pub reader: String,
    pub readers: usize,
    pub true_cer: f64,
    pub est_cer: f64,
    pub est_cer_raw: f64,
    pub residual: f64,
    pub without_vlm: bool,
}

/// Pearson correlation of two equal-length series; none under two points
/// or with no variance.
pub fn pearson(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 2 {
        return None;
    }
    let n = x.len() as f64;
    let (mx, my) = (x.iter().sum::<f64>() / n, y.iter().sum::<f64>() / n);
    let (mut sxy, mut sxx, mut syy) = (0.0, 0.0, 0.0);
    for (a, b) in x.iter().zip(y) {
        sxy += (a - mx) * (b - my);
        sxx += (a - mx) * (a - mx);
        syy += (b - my) * (b - my);
    }
    (sxx > 0.0 && syy > 0.0).then(|| sxy / (sxx * syy).sqrt())
}

/// Ranks with ties averaged.
fn ranks(v: &[f64]) -> Vec<f64> {
    let mut idx: Vec<usize> = (0..v.len()).collect();
    idx.sort_by(|a, b| v[*a].total_cmp(&v[*b]));
    let mut r = vec![0.0; v.len()];
    let mut i = 0;
    while i < idx.len() {
        let mut j = i;
        while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
            j += 1;
        }
        let avg = (i + j) as f64 / 2.0 + 1.0;
        for k in i..=j {
            r[idx[k]] = avg;
        }
        i = j + 1;
    }
    r
}

/// Spearman correlation: Pearson over the ranks.
pub fn spearman(x: &[f64], y: &[f64]) -> Option<f64> {
    if x.len() != y.len() || x.len() < 2 {
        return None;
    }
    pearson(&ranks(x), &ranks(y))
}

/// Per-reader statistics from `(true, estimated)` pairs.
pub fn row(reader: &str, pairs: &[(f64, f64)], without_vlm: bool) -> Row {
    let n = pairs.len().max(1) as f64;
    let t: Vec<f64> = pairs.iter().map(|p| p.0).collect();
    let e: Vec<f64> = pairs.iter().map(|p| p.1).collect();
    Row {
        reader: reader.to_owned(),
        crops: pairs.len(),
        true_cer: t.iter().sum::<f64>() / n,
        est_cer: e.iter().sum::<f64>() / n,
        bias: pairs.iter().map(|(a, b)| b - a).sum::<f64>() / n,
        mae: pairs.iter().map(|(a, b)| (b - a).abs()).sum::<f64>() / n,
        pearson: pearson(&t, &e),
        spearman: spearman(&t, &e),
        without_vlm,
    }
}

/// The hat over `readers` (name, text) for one crop; estimates by name
/// when determined.
/// Estimates by reader name: `(clipped, raw)`.
type Estimates = BTreeMap<String, (f64, f64)>;

fn crop_hat(readers: &[(&str, &str)]) -> Option<(Estimates, f64)> {
    let mut pairs = Vec::new();
    for i in 0..readers.len() {
        for j in i + 1..readers.len() {
            pairs.push((i, j, align::pair(readers[i].1, readers[j].1).cer_sym));
        }
    }
    let h = hat::solve(readers.len(), &pairs);
    h.determined.then(|| {
        (
            readers
                .iter()
                .enumerate()
                .map(|(i, (n, _))| (n.to_string(), (h.estimates[i], h.raw[i])))
                .collect(),
            h.residual,
        )
    })
}

/// The calibration over gold rows and the readers' texts by crop id; the
/// readings, when given, are the reader `vlm`.
pub fn calibrate(
    rows: &[serde_json::Value],
    hyps: &score::Hyps,
    readings: Option<&std::collections::HashMap<String, String>>,
) -> (Vec<Row>, Vec<CropEstimate>) {
    let bad = score::invalid_crops(rows, false);
    let mut by_crop = Vec::new();
    // reader -> (true, est) pairs, with and without the vision-language reader.
    let mut with: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    let mut without: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    for r in rows {
        let Some(cid) = r.get("crop_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if bad.contains(cid.split(':').next().unwrap_or_default()) {
            continue;
        }
        let Some(truth) = r.get("transcription").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut readers: Vec<(&str, &str)> = hyps
            .iter()
            .filter_map(|(name, texts)| texts.get(cid).map(|t| (name.as_str(), t.as_str())))
            .collect();
        let true_cer = |hyp: &str| {
            let c = score::cer_wer(truth, hyp);
            if c.ref_chars == 0 {
                0.0
            } else {
                c.char_edits as f64 / c.ref_chars as f64
            }
        };
        let vlm = readings.and_then(|m| m.get(cid));
        let mut runs: Vec<(bool, Vec<(&str, &str)>)> = vec![(true, readers.clone())];
        if let Some(v) = vlm {
            readers.push((VLM, v.as_str()));
            runs.insert(0, (false, readers.clone()));
        }
        for (without_vlm, members) in runs {
            let Some((est, residual)) = crop_hat(&members) else {
                continue;
            };
            for (name, text) in &members {
                let t = true_cer(text);
                let (e, raw) = est[*name];
                by_crop.push(CropEstimate {
                    crop_id: cid.to_owned(),
                    reader: name.to_string(),
                    readers: members.len(),
                    true_cer: round6(t),
                    est_cer: round6(e),
                    est_cer_raw: round6(raw),
                    residual: round6(residual),
                    without_vlm,
                });
                let table = if without_vlm { &mut without } else { &mut with };
                table.entry(name.to_string()).or_default().push((t, e));
            }
        }
    }
    let mut out: Vec<Row> = with
        .iter()
        .map(|(n, p)| row(n, p, false))
        .chain(without.iter().map(|(n, p)| row(n, p, true)))
        .collect();
    // Without readings the two tables are the same run: keep one.
    if readings.is_none() {
        out.retain(|r| r.without_vlm);
    }
    (out, by_crop)
}

fn opt(x: Option<f64>) -> String {
    x.map_or("-".into(), |v| format!("{v:.3}"))
}

pub fn table(rows: &[Row]) -> String {
    let mut s = String::from(
        "| Reader | Hat | Crops | True CER | Est. CER | Bias | MAE | Pearson | Spearman |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {:.4} | {:.4} | {:+.4} | {:.4} | {} | {} |",
            r.reader,
            if r.without_vlm {
                "without vlm"
            } else {
                "with vlm"
            },
            r.crops,
            r.true_cer,
            r.est_cer,
            r.bias,
            r.mae,
            opt(r.pearson),
            opt(r.spearman)
        );
    }
    s
}

pub fn csv(rows: &[Row]) -> String {
    let mut s = String::from("reader,hat,crops,true_cer,est_cer,bias,mae,pearson,spearman\n");
    for r in rows {
        let _ = writeln!(
            s,
            "{},{},{},{:.6},{:.6},{:.6},{:.6},{},{}",
            r.reader,
            if r.without_vlm {
                "without_vlm"
            } else {
                "with_vlm"
            },
            r.crops,
            r.true_cer,
            r.est_cer,
            r.bias,
            r.mae,
            r.pearson.map_or(String::new(), |v| format!("{v:.6}")),
            r.spearman.map_or(String::new(), |v| format!("{v:.6}")),
        );
    }
    s
}

pub fn by_crop_csv(rows: &[CropEstimate]) -> String {
    let mut s = String::from("crop_id,reader,hat,readers,true_cer,est_cer,est_cer_raw,residual\n");
    for r in rows {
        let _ = writeln!(
            s,
            "{},{},{},{},{:.6},{:.6},{:.6},{:.6}",
            r.crop_id,
            r.reader,
            if r.without_vlm {
                "without_vlm"
            } else {
                "with_vlm"
            },
            r.readers,
            r.true_cer,
            r.est_cer,
            r.est_cer_raw,
            r.residual
        );
    }
    s
}

/// Readings as `{"crop_id", "text"}` rows.
fn read_readings(
    path: &std::path::Path,
) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let mut m = std::collections::HashMap::new();
    for r in score::read_jsonl(path)? {
        let cid = r
            .get("crop_id")
            .and_then(|v| v.as_str())
            .context("reading row without crop_id")?;
        let t = r
            .get("text")
            .and_then(|v| v.as_str())
            .with_context(|| format!("{cid}: text is not a string"))?;
        m.insert(cid.to_owned(), t.to_owned());
    }
    Ok(m)
}

pub fn run(opts: &Options) -> anyhow::Result<Vec<Row>> {
    anyhow::ensure!(!opts.hyps.is_empty(), "at least one --hyp");
    let rows = score::read_jsonl(&opts.crops)?;
    let hyps = score::load_hyps(&rows, &opts.hyps, false)?;
    let readings = opts.readings.as_deref().map(read_readings).transpose()?;
    let (table_rows, by_crop) = calibrate(&rows, &hyps, readings.as_ref());
    std::fs::create_dir_all(&opts.out)?;
    atomic(
        &opts.out.join("calibration.csv"),
        csv(&table_rows).as_bytes(),
    )?;
    atomic(
        &opts.out.join("calibration.md"),
        table(&table_rows).as_bytes(),
    )?;
    atomic(
        &opts.out.join("by-crop.csv"),
        by_crop_csv(&by_crop).as_bytes(),
    )?;
    Ok(table_rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn correlations_and_rows() {
        let x = [0.1, 0.2, 0.3, 0.4];
        let y = [0.12, 0.19, 0.33, 0.41];
        assert!(pearson(&x, &y).unwrap() > 0.99);
        assert_eq!(spearman(&x, &y), Some(1.0));
        assert_eq!(spearman(&x, &[0.4, 0.3, 0.2, 0.1]), Some(-1.0));
        assert_eq!(pearson(&x, &[1.0; 4]), None);
        assert_eq!(pearson(&[1.0], &[1.0]), None);
        assert_eq!(ranks(&[0.2, 0.1, 0.2]), vec![2.5, 1.0, 2.5]);
        let r = row("loc", &[(0.1, 0.12), (0.2, 0.18)], false);
        assert!((r.bias - 0.0).abs() < 1e-12);
        assert!((r.mae - 0.02).abs() < 1e-12);
        assert_eq!(r.crops, 2);
    }

    /// Synthetic crops: a truth, three readers with independent errors
    /// at known rates, a vision-language reading that is exact.
    fn synthetic(
        n: usize,
    ) -> (
        Vec<serde_json::Value>,
        score::Hyps,
        std::collections::HashMap<String, String>,
    ) {
        let words = [
            "the",
            "mayor",
            "of",
            "albany",
            "spoke",
            "with",
            "the",
            "council",
            "about",
            "bridges",
            "and",
            "roads",
            "across",
            "the",
            "river",
            "yesterday",
            "evening",
            "before",
            "a",
            "crowd",
        ];
        let mut rng = usnm_ocr_quality::crops::SplitMix(7);
        let mut rows = Vec::new();
        let mut hyps = score::Hyps::new();
        let mut readings = std::collections::HashMap::new();
        for c in 0..n {
            let cid = format!("c{c:03}");
            let truth = words
                .iter()
                .cycle()
                .skip(c)
                .take(15)
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            rows.push(json!({"crop_id": cid, "transcription": truth, "transcriber": "x"}));
            for (name, rate) in [("a", 2u64), ("b", 8), ("c", 20)] {
                let text: String = truth
                    .chars()
                    .map(|ch| {
                        if ch != ' ' && rng.below(100) < rate {
                            'x'
                        } else {
                            ch
                        }
                    })
                    .collect();
                hyps.entry(name.into())
                    .or_default()
                    .insert(cid.clone(), text);
            }
            readings.insert(cid.clone(), truth.clone());
        }
        (rows, hyps, readings)
    }

    #[test]
    fn calibrates_synthetic_readers() {
        let (rows, hyps, readings) = synthetic(40);
        let (table_rows, by_crop) = calibrate(&rows, &hyps, Some(&readings));
        assert_eq!(table_rows.len(), 7, "{table_rows:?}");
        let with: Vec<&Row> = table_rows.iter().filter(|r| !r.without_vlm).collect();
        let without: Vec<&Row> = table_rows.iter().filter(|r| r.without_vlm).collect();
        assert_eq!(with.len(), 4);
        assert_eq!(without.len(), 3);
        for r in &table_rows {
            assert_eq!(r.crops, 40);
            // Independent errors: the hat recovers each rate within a few
            // points, and ranks the readers right.
            assert!(r.mae < 0.05, "{r:?}");
        }
        let c = without.iter().find(|r| r.reader == "c").unwrap();
        let a = without.iter().find(|r| r.reader == "a").unwrap();
        assert!(c.est_cer > a.est_cer);
        assert!(c.true_cer > a.true_cer);
        assert!(c.pearson.unwrap() > 0.0);
        let v = with.iter().find(|r| r.reader == VLM).unwrap();
        assert!(v.true_cer < 1e-9);
        assert!(v.est_cer < 0.03, "{v:?}");
        assert_eq!(by_crop.len(), 40 * 7);
        assert!(table(&table_rows).contains("| c | without vlm | 40 |"));
        assert!(csv(&table_rows).starts_with("reader,hat,"));
        // Without readings there is one table, and a crop with two readers
        // is left out of it.
        let mut two = hyps.clone();
        two.remove("c");
        let (rows_only, _) = calibrate(&rows, &two, None);
        assert!(rows_only.is_empty());
        let (one, _) = calibrate(&rows, &hyps, None);
        assert_eq!(one.len(), 3);
        assert!(one.iter().all(|r| r.without_vlm));
    }

    #[test]
    fn runs_from_files() {
        let tmp = tempfile::tempdir().unwrap();
        let (rows, hyps, readings) = synthetic(5);
        let mut crops = String::new();
        for r in &rows {
            crops.push_str(&r.to_string());
            crops.push('\n');
        }
        std::fs::write(tmp.path().join("crops.jsonl"), crops).unwrap();
        let mut specs = Vec::new();
        for (name, texts) in &hyps {
            let p = tmp.path().join(format!("{name}.jsonl"));
            let mut s = String::new();
            for (cid, t) in texts {
                s.push_str(&json!({"crop_id": cid, "text": t}).to_string());
                s.push('\n');
            }
            std::fs::write(&p, s).unwrap();
            specs.push(format!("{name}={}", p.display()));
        }
        let mut s = String::new();
        for (cid, t) in &readings {
            s.push_str(&json!({"crop_id": cid, "text": t}).to_string());
            s.push('\n');
        }
        std::fs::write(tmp.path().join("readings.jsonl"), s).unwrap();
        let out = run(&Options {
            crops: tmp.path().join("crops.jsonl"),
            hyps: specs,
            readings: Some(tmp.path().join("readings.jsonl")),
            out: tmp.path().join("out"),
        })
        .unwrap();
        assert_eq!(out.len(), 7);
        for f in ["calibration.csv", "calibration.md", "by-crop.csv"] {
            assert!(tmp.path().join("out").join(f).exists());
        }
    }
}
