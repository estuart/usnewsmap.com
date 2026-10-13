//! CER and WER against the gold transcriptions: the same rows, numbers and files as `gold.py score`.

use crate::{distance, text};
use anyhow::{bail, Context};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;

/// Crops with more unknown characters than this fail validation.
pub const MAX_UNKNOWN_SHARE: f64 = 0.02;

/// Edit counts for one hypothesis against one transcription.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Counts {
    pub ref_chars: usize,
    pub char_edits: usize,
    pub ref_words: usize,
    pub word_edits: usize,
}

/// gold.py's `cer_wer`: the transcription is normalized as truth, the hypothesis as plain text.
pub fn cer_wer(truth: &str, hyp: &str) -> Counts {
    let t = text::normalize(truth, true);
    let h = text::normalize(hyp, false);
    let (tw, hw) = (text::py_split(&t), text::py_split(&h));
    Counts {
        ref_chars: t.chars().count(),
        char_edits: distance::chars(&t, &h),
        ref_words: tw.len(),
        word_edits: distance::words(&tw, &hw),
    }
}

/// Reads a JSON Lines file the way gold.py's `read_jsonl` does: blank lines are skipped.
pub fn read_jsonl(path: &Path) -> anyhow::Result<Vec<Value>> {
    let s = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    s.lines()
        .filter(|l| !text::py_strip(l).is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{}: row {}", path.display(), i + 1))
        })
        .collect()
}

/// Python truthiness of a JSON value.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn str_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => "?".into(),
    }
}

/// The engines' hint texts stored in a crops.jsonl row (`hints.texts`), sorted by engine.
fn hint_texts(row: &Value) -> Vec<(String, &Value)> {
    let mut v: Vec<(String, &Value)> = row
        .get("hints")
        .and_then(|h| h.get("texts"))
        .and_then(Value::as_object)
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v)).collect())
        .unwrap_or_default();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// The crop ids gold.py's `validate_rows` reports an error for (without a manifest); `score` leaves
/// them out. Errors are keyed by the text before the first `:`, as gold.py splits them.
pub fn invalid_crops(rows: &[Value], require_review: bool) -> BTreeSet<String> {
    let mut bad = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for r in rows {
        let cid = str_of(r.get("crop_id"));
        let key = cid.split(':').next().unwrap_or_default().to_string();
        if !seen.insert(cid.clone()) {
            bad.insert(key.clone());
        }
        let t = match r.get("transcription") {
            Some(Value::String(t)) if !text::py_strip(t).is_empty() => t,
            _ => {
                bad.insert(key);
                continue;
            }
        };
        let mut err = !truthy(r.get("transcriber"));
        if require_review
            && (!truthy(r.get("reviewer")) || r.get("reviewer") == r.get("transcriber"))
        {
            err = true;
        }
        if text::drop_guesses(t)
            .replace(text::UNKNOWN, "")
            .contains("[?")
            || t.contains(text::SKIPPED)
            || text::unknown_share(t) > MAX_UNKNOWN_SHARE
        {
            err = true;
        }
        if !truthy(r.get("hint_copy_reviewed")) {
            let nt = text::normalize(t, true);
            for (_, h) in hint_texts(r) {
                if let Value::String(h) = h {
                    if !h.is_empty() && text::normalize(h, false) == nt {
                        err = true;
                    }
                }
            }
        }
        if err {
            bad.insert(key);
        }
    }
    bad
}

/// One scored crop: a row of cer-by-crop.csv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CropScore {
    pub engine: String,
    pub crop_id: String,
    pub group: String,
    pub counts: Counts,
}

/// One row of cer-summary.csv; `cer` and `wer` are rounded to 4 places as gold.py's `round`.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub engine: String,
    pub group: String,
    pub crops: usize,
    pub ref_chars: usize,
    pub cer: Option<f64>,
    pub wer: Option<f64>,
}

/// The result of a score run.
pub struct Scored {
    pub rows: usize,
    pub used: usize,
    pub by_crop: Vec<CropScore>,
    pub summary: Vec<Summary>,
}

/// Python's `round(x, 4)`: the correctly rounded decimal, ties to even on the exact binary value.
pub fn round4(x: f64) -> f64 {
    format!("{x:.4}").parse().unwrap_or(x)
}

/// Python's `repr` of a float (finite, as CER and WER are).
pub fn py_float(x: f64) -> String {
    let s = format!("{x}");
    if s.contains(['.', 'e', 'E', 'i', 'N']) {
        s
    } else {
        format!("{s}.0")
    }
}

/// Hypotheses per engine: crop id to text.
pub type Hyps = BTreeMap<String, HashMap<String, String>>;

/// Reads `--hyp ENGINE=FILE.jsonl` files (rows `{"crop_id", "text"}`) and, with `hints`, the
/// engines' texts stored in the crops rows, as gold.py does.
pub fn load_hyps(rows: &[Value], specs: &[String], hints: bool) -> anyhow::Result<Hyps> {
    let mut hyps = Hyps::new();
    for spec in specs {
        let Some((name, path)) = spec.split_once('=').filter(|(_, p)| !p.is_empty()) else {
            bail!("--hyp wants ENGINE=FILE.jsonl, got {spec:?}");
        };
        let mut m = HashMap::new();
        for r in read_jsonl(Path::new(path))? {
            let cid = r
                .get("crop_id")
                .and_then(Value::as_str)
                .context("hypothesis row without crop_id")?;
            let t = r
                .get("text")
                .and_then(Value::as_str)
                .with_context(|| format!("{cid}: text is not a string"))?;
            m.insert(cid.to_string(), t.to_string());
        }
        hyps.insert(name.to_string(), m);
    }
    if hints {
        for r in rows {
            let cid = str_of(r.get("crop_id"));
            for (engine, t) in hint_texts(r) {
                let t = t
                    .as_str()
                    .with_context(|| format!("{cid}: hint {engine:?} is not a string"))?;
                hyps.entry(engine)
                    .or_default()
                    .insert(cid.clone(), t.to_string());
            }
        }
    }
    Ok(hyps)
}

/// gold.py's `cmd_score`, without the printing.
pub fn score(rows: &[Value], hyps: &Hyps, require_review: bool) -> anyhow::Result<Scored> {
    let bad = invalid_crops(rows, require_review);
    let used: Vec<&Value> = rows
        .iter()
        .filter(|r| !bad.contains(&str_of(r.get("crop_id"))))
        .collect();
    let group_of = |r: &Value| -> anyhow::Result<String> {
        r.get("group")
            .and_then(Value::as_str)
            .map(str::to_string)
            .with_context(|| format!("{}: no group", str_of(r.get("crop_id"))))
    };
    let mut by_crop = Vec::new();
    for (engine, texts) in hyps {
        for r in &used {
            let cid = str_of(r.get("crop_id"));
            if let Some(h) = texts.get(&cid) {
                let t = r
                    .get("transcription")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                by_crop.push(CropScore {
                    engine: engine.clone(),
                    crop_id: cid,
                    group: group_of(r)?,
                    counts: cer_wer(t, h),
                });
            }
        }
    }
    let groups: BTreeSet<String> = used
        .iter()
        .map(|r| group_of(r))
        .collect::<anyhow::Result<_>>()?;
    Ok(Scored {
        rows: rows.len(),
        used: used.len(),
        summary: summarize(&by_crop, hyps.keys(), &groups),
        by_crop,
    })
}

/// Per engine: all crops, then each group, as gold.py orders them.
pub fn summarize<'a>(
    by_crop: &[CropScore],
    engines: impl IntoIterator<Item = &'a String>,
    groups: &BTreeSet<String>,
) -> Vec<Summary> {
    let mut out = Vec::new();
    for engine in engines {
        for group in std::iter::once(None).chain(groups.iter().map(Some)) {
            let rs: Vec<&CropScore> = by_crop
                .iter()
                .filter(|x| &x.engine == engine && group.is_none_or(|g| &x.group == g))
                .collect();
            if rs.is_empty() {
                continue;
            }
            let c: usize = rs.iter().map(|x| x.counts.ref_chars).sum();
            let w: usize = rs.iter().map(|x| x.counts.ref_words).sum();
            let ce: usize = rs.iter().map(|x| x.counts.char_edits).sum();
            let we: usize = rs.iter().map(|x| x.counts.word_edits).sum();
            out.push(Summary {
                engine: engine.clone(),
                // gold.py writes `group or "all"`, so an empty group name also reads "all".
                group: group
                    .filter(|g| !g.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "all".into()),
                crops: rs.len(),
                ref_chars: c,
                cer: (c > 0).then(|| round4(ce as f64 / c as f64)),
                wer: (w > 0).then(|| round4(we as f64 / w as f64)),
            });
        }
    }
    out
}

/// One CSV field as Python's `csv` module writes it with the default dialect.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_row(fields: &[String]) -> String {
    let mut s = fields
        .iter()
        .map(|f| csv_field(f))
        .collect::<Vec<_>>()
        .join(",");
    s.push_str("\r\n");
    s
}

fn opt(x: Option<f64>) -> String {
    x.map(py_float).unwrap_or_default()
}

/// cer-by-crop.csv, byte for byte as gold.py writes it.
pub fn by_crop_csv(rows: &[CropScore]) -> String {
    let mut s = csv_row(
        &[
            "engine",
            "crop_id",
            "group",
            "ref_chars",
            "char_edits",
            "ref_words",
            "word_edits",
        ]
        .map(String::from),
    );
    for r in rows {
        s.push_str(&csv_row(&[
            r.engine.clone(),
            r.crop_id.clone(),
            r.group.clone(),
            r.counts.ref_chars.to_string(),
            r.counts.char_edits.to_string(),
            r.counts.ref_words.to_string(),
            r.counts.word_edits.to_string(),
        ]));
    }
    s
}

/// cer-summary.csv, byte for byte as gold.py writes it.
pub fn summary_csv(rows: &[Summary]) -> String {
    let mut s = csv_row(&["engine", "group", "crops", "ref_chars", "cer", "wer"].map(String::from));
    for r in rows {
        s.push_str(&csv_row(&[
            r.engine.clone(),
            r.group.clone(),
            r.crops.to_string(),
            r.ref_chars.to_string(),
            opt(r.cer),
            opt(r.wer),
        ]));
    }
    s
}

/// The Markdown table gold.py prints.
pub fn table(rows: &[Summary]) -> String {
    let mut s = String::from(
        "| engine | group | crops | reference chars | CER | WER |\n|---|---|---|---|---|---|\n",
    );
    let py = |x: Option<f64>| x.map(py_float).unwrap_or_else(|| "None".into());
    for r in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {} | {} | {} |",
            r.engine,
            r.group,
            r.crops,
            r.ref_chars,
            py(r.cer),
            py(r.wer)
        );
    }
    s
}

/// Writes the two CSV files into `out`, as gold.py does (a file is written only if it has rows).
pub fn write_csvs(out: &Path, s: &Scored) -> anyhow::Result<()> {
    std::fs::create_dir_all(out)?;
    if !s.by_crop.is_empty() {
        std::fs::write(out.join("cer-by-crop.csv"), by_crop_csv(&s.by_crop))?;
    }
    if !s.summary.is_empty() {
        std::fs::write(out.join("cer-summary.csv"), summary_csv(&s.summary))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_and_prints_like_python() {
        assert_eq!(py_float(round4(1.0 / 32.0)), "0.0312");
        assert_eq!(py_float(round4(3.0 / 32.0)), "0.0938");
        assert_eq!(py_float(round4(0.0)), "0.0");
        assert_eq!(py_float(round4(2.0)), "2.0");
        assert_eq!(py_float(round4(1.0 / 3.0)), "0.3333");
    }

    #[test]
    fn counts() {
        let c = cer_wer("The cat [?s]at.", "The cat sat");
        assert_eq!(
            c,
            Counts {
                ref_chars: 12,
                char_edits: 1,
                ref_words: 3,
                word_edits: 1
            }
        );
    }
}
