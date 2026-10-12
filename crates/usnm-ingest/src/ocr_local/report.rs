//! `ocr-sync --report` (12 §12.5 Steps 8 and 9): rule v1 applied with the
//! scores `ocr-sync` stored, over all of `ocr-local`, without releasing.
//! The release scores both texts again; this is the pilot's estimate of the
//! gain before a rebuild. Written to `ocr-local/report/<run>.json`.
//!
//! For each page, the row with the lowest stored damage rate (ties: the
//! newest), weighed by rule v1 ([`super::rule`]).

use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::Serialize;
use usnm_store::ObjectStore;

use super::pages::{self, Scores, StoredRow};
use super::{report_path, run_id, ROOT};

pub use super::rule::MARGIN;

/// Why LoC's text stays, by [`super::rule::verdict`] on the stored scores,
/// without the title-language test (the report has no catalog).
pub fn verdict(ours: &Scores, loc: &Scores) -> Result<(), &'static str> {
    super::rule::verdict(ours, loc, None).map_err(super::rule::Keep::as_str)
}

/// The short engine key of 13 §13.8.1, `<family>/<version> <profile>`, or
/// the tag as it is for the older forms.
pub fn engine_key(tag: &str) -> String {
    let mut fields = tag.split(' ');
    let first = fields.next().unwrap_or_default();
    match fields.find_map(|f| f.strip_prefix("profile=")) {
        Some(p) => format!("{first} {p}"),
        None => tag.to_owned(),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub considered: u64,
    pub replaced: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub run: String,
    pub at: DateTime<Utc>,
    pub metric_version: u32,
    pub margin: f64,
    /// Stored scores, not scored again: an estimate.
    pub estimate: bool,
    pub parts: usize,
    pub rows: usize,
    pub pages: Tally,
    /// Pages where LoC's text stays, by why.
    pub kept: BTreeMap<String, u64>,
    pub by_language: BTreeMap<String, Tally>,
    pub by_engine: BTreeMap<String, Tally>,
    /// Pages by the node that read the chosen row.
    pub by_node: BTreeMap<String, Tally>,
    /// The stored damage scores of the pages both texts have a rate for.
    pub damage: Damage,
}

/// Quartiles of a set of values.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Quartiles {
    pub p25: f64,
    pub median: f64,
    pub p75: f64,
}

impl Quartiles {
    fn of(mut v: Vec<f64>) -> Option<Self> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(f64::total_cmp);
        // Linear interpolation between the closest ranks, so the median of
        // an even set is the mean of the middle pair, as `ocr_damage`'s.
        let q = |f: f64| {
            let x = (v.len() - 1) as f64 * f;
            let (lo, hi) = (x.floor() as usize, x.ceil() as usize);
            v[lo] + (v[hi] - v[lo]) * (x - lo as f64)
        };
        Some(Self {
            p25: q(0.25),
            median: q(0.5),
            p75: q(0.75),
        })
    }
}

/// Our damage against LoC's on the same pages (12 Step 11's pilot report).
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Damage {
    /// Pages where both texts have a damage rate.
    pub pages: u64,
    pub ours: Option<Quartiles>,
    pub loc: Option<Quartiles>,
    /// Our rate minus LoC's, per page.
    pub change: Option<Quartiles>,
    /// Pages where ours is lower.
    pub ours_lower: u64,
    /// Our head count over LoC's, per page with a LoC head count.
    pub head_ratio: Option<Quartiles>,
}

/// Choose each page's row and apply the rule.
pub fn tally(rows: Vec<StoredRow>, run: &str, parts: usize) -> Report {
    let n = rows.len();
    let mut best: HashMap<String, StoredRow> = HashMap::new();
    for r in rows {
        let rate = |s: &StoredRow| s.ours.damage_rate.unwrap_or(f64::INFINITY);
        match best.get(&r.doc_id) {
            Some(b) if rate(b) < rate(&r) || (rate(b) == rate(&r) && b.ocred_at >= r.ocred_at) => {}
            _ => {
                best.insert(r.doc_id.clone(), r);
            }
        }
    }
    let mut report = Report {
        run: run.to_owned(),
        at: Utc::now(),
        metric_version: usnm_core::damage::VERSION,
        margin: MARGIN,
        estimate: true,
        parts,
        rows: n,
        pages: Tally::default(),
        kept: BTreeMap::new(),
        by_language: BTreeMap::new(),
        by_engine: BTreeMap::new(),
        by_node: BTreeMap::new(),
        damage: Damage::default(),
    };
    let (mut ours, mut loc, mut change, mut head) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for r in best.values() {
        if let (Some(o), Some(l)) = (r.ours.damage_rate, r.loc.damage_rate) {
            ours.push(o);
            loc.push(l);
            change.push(o - l);
            report.damage.ours_lower += u64::from(o < l);
        }
        if let (Some(o), Some(l)) = (r.ours.head, r.loc.head) {
            if l > 0 {
                head.push(f64::from(o) / f64::from(l));
            }
        }
    }
    report.damage.pages = ours.len() as u64;
    report.damage.ours = Quartiles::of(ours);
    report.damage.loc = Quartiles::of(loc);
    report.damage.change = Quartiles::of(change);
    report.damage.head_ratio = Quartiles::of(head);
    for r in best.values() {
        let lang = r.ours.lang.clone().unwrap_or_else(|| "none".into());
        let engine = engine_key(&r.ocr_engine);
        let replaced = verdict(&r.ours, &r.loc);
        for t in [
            &mut report.pages,
            report.by_language.entry(lang).or_default(),
            report.by_engine.entry(engine).or_default(),
            report.by_node.entry(r.node.clone()).or_default(),
        ] {
            t.considered += 1;
            t.replaced += u64::from(replaced.is_ok());
        }
        if let Err(why) = replaced {
            *report.kept.entry(why.to_owned()).or_default() += 1;
        }
    }
    report
}

/// Read every merged part's scores and write the report.
pub async fn run(curated: &dyn ObjectStore) -> anyhow::Result<Report> {
    let run = run_id(Utc::now());
    let paths: Vec<String> = curated
        .list(&format!("{ROOT}/pages"))
        .await?
        .into_iter()
        .filter(|p| p.ends_with(".parquet"))
        .collect();
    let mut rows = Vec::new();
    for p in &paths {
        let Some(bytes) = curated.get(p).await? else {
            continue;
        };
        rows.extend(pages::read_scores(Bytes::from(bytes))?);
    }
    let report = tally(rows, &run, paths.len());
    let path = report_path(&run);
    curated
        .put_new(
            &path,
            serde_json::to_vec_pretty(&report)?,
            "application/json",
        )
        .await?;
    tracing::info!(
        path,
        pages = report.pages.considered,
        replaced = report.pages.replaced,
        kept = ?report.kept,
        by_node = ?report.by_node,
        damage = ?report.damage,
        "ocr-local report written"
    );
    Ok(report)
}

#[cfg(test)]
mod quartile_tests {
    use super::Quartiles;

    #[test]
    fn quartiles_of_a_small_set() {
        let q = Quartiles::of(vec![0.4, 0.1, 0.3, 0.2, 0.5]).unwrap();
        assert_eq!((q.p25, q.median, q.p75), (0.2, 0.3, 0.4));
        assert!(Quartiles::of(Vec::new()).is_none());
        let q = Quartiles::of(vec![0.1, 0.2]).unwrap();
        assert!((q.median - 0.15).abs() < 1e-12);
        let q = Quartiles::of(vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        assert_eq!((q.p25, q.median, q.p75), (1.75, 2.5, 3.25));
    }
}
