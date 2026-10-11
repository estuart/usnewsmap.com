//! The release's side of our own OCR (12 §12.5 Step 9, `--ocr-local`):
//! which of `ocr-local`'s readings replace LoC's text in the index.
//!
//! - **Loading.** The release lists `ocr-local/pages/` once ([`Listing`]).
//!   As it reaches a batch it loads the parts of that batch's titles
//!   ([`Listing::load`]), so memory holds one batch's titles; a title in
//!   several batches is read once for each. For each page it takes the row
//!   with the lowest stored damage rate (ties: the newest) from the stored
//!   scores alone, then reads the text of the chosen rows only.
//! - **Revocations.** `ocr-local/revoked/<stamp>.json` files, create-only,
//!   each naming a node, an engine, a time window or an import run, with a
//!   reason and a reference ([`Revocation`]). Rows they match are left out
//!   before a page's row is chosen.
//! - **Rule.** [`super::rule`] on both texts scored again.
//! - **Records.** [`Tally`] for the version's `ocr_local.json`; the pages
//!   replaced for its `ocr_local_pages.bin` (`usnm_state::ocr_local`).
//!
//! Logs carry page ids, paths and counts, never text.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::{bail, Context};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use usnm_store::ObjectStore;

use super::pages::{self, Scores, StoredRow};
use super::report::{engine_key, Tally as Count};
use super::rule::{self, Keep};
use super::ROOT;
use crate::source::hex;

/// Where revocations are.
pub fn revoked_prefix() -> String {
    format!("{ROOT}/revoked")
}

/// Every merged part in `ocr-local/pages/`, by title.
#[derive(Debug, Default, Clone)]
pub struct Listing {
    by_title: BTreeMap<String, Vec<String>>,
}

impl Listing {
    /// List `ocr-local/pages/` once. Paths that aren't
    /// `ocr-local/pages/<lccn>/<run>.parquet` are left out.
    pub async fn list(curated: &dyn ObjectStore) -> anyhow::Result<Self> {
        let prefix = format!("{ROOT}/pages");
        let mut by_title: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for path in curated.list(&prefix).await? {
            let Some(rest) = path.strip_prefix(&format!("{prefix}/")) else {
                continue;
            };
            let Some((lccn, file)) = rest.split_once('/') else {
                continue;
            };
            if file.contains('/') || !file.ends_with(".parquet") {
                continue;
            }
            by_title.entry(lccn.to_owned()).or_default().push(path);
        }
        for paths in by_title.values_mut() {
            paths.sort();
        }
        Ok(Self { by_title })
    }

    pub fn parts(&self) -> usize {
        self.by_title.values().map(Vec::len).sum()
    }

    pub fn titles(&self) -> usize {
        self.by_title.len()
    }

    /// Whether any part is for one of `lccns`.
    pub fn has_any<'a>(&self, mut lccns: impl Iterator<Item = &'a str>) -> bool {
        lccns.any(|l| self.by_title.contains_key(l))
    }

    /// Load the pages of `lccns` for the release: per page, the chosen row
    /// with its text, after `revoked`.
    pub async fn load<'a>(
        &self,
        curated: &dyn ObjectStore,
        lccns: impl Iterator<Item = &'a str>,
        revoked: &[Revocation],
    ) -> anyhow::Result<Loaded> {
        let mut out = Loaded::default();
        let lccns: BTreeSet<&str> = lccns.collect();
        for lccn in lccns {
            let Some(paths) = self.by_title.get(lccn) else {
                continue;
            };
            // The stored scores alone choose each page's row: (row, part,
            // the row's place in the part).
            let mut best: HashMap<String, (StoredRow, usize, usize)> = HashMap::new();
            let mut withdrawn: HashSet<String> = HashSet::new();
            let mut bodies = Vec::with_capacity(paths.len());
            for (i, path) in paths.iter().enumerate() {
                let bytes = Bytes::from(
                    curated
                        .get(path)
                        .await?
                        .with_context(|| format!("`{path}` vanished"))?,
                );
                out.parts.push((path.clone(), bytes.len() as u64));
                let run = run_of(path);
                let rows = pages::read_scores(bytes.clone()).with_context(|| path.clone())?;
                for (r, row) in rows.into_iter().enumerate() {
                    anyhow::ensure!(row.lccn == lccn, "`{path}` holds a page of `{}`", row.lccn);
                    if revoked.iter().any(|v| v.matches(&row, run)) {
                        withdrawn.insert(row.doc_id.clone());
                        continue;
                    }
                    let take = match best.get(&row.doc_id) {
                        None => true,
                        Some((b, j, q)) => prefer((&row, i, r), (b, *j, *q)),
                    };
                    if take {
                        best.insert(row.doc_id.clone(), (row, i, r));
                    }
                }
                bodies.push(bytes);
            }
            // Then the chosen rows' text, by their place in their part (a
            // part can hold a page twice), reading only that column.
            let mut wanted: HashMap<usize, HashSet<usize>> = HashMap::new();
            for (_, i, r) in best.values() {
                wanted.entry(*i).or_default().insert(*r);
            }
            let mut texts: HashMap<(usize, usize), (String, String)> = HashMap::new();
            for (i, rows) in wanted {
                for (r, found) in
                    pages::read_texts(bodies[i].clone(), &rows).with_context(|| paths[i].clone())?
                {
                    texts.insert((i, r), found);
                }
            }
            drop(bodies);
            for (id, (row, i, r)) in best {
                let (doc_id, text) = texts
                    .remove(&(i, r))
                    .with_context(|| format!("`{id}` has scores but no text"))?;
                anyhow::ensure!(doc_id == id, "`{}` row {r} changed between reads", paths[i]);
                withdrawn.remove(&id);
                out.pages.insert(id, Candidate { row, text });
            }
            out.revoked.extend(withdrawn);
        }
        Ok(out)
    }

    /// How many pages of `ocr-local` would replace LoC's text in
    /// `published`, the batches the version keeps from its published
    /// indexes, by rule v1 on the scores `ocr-sync` stored: an estimate,
    /// for timing a full rebuild. Reads no text.
    pub async fn estimate(
        &self,
        curated: &dyn ObjectStore,
        published: &BTreeSet<&str>,
        title_languages: impl Fn(&str) -> Vec<String>,
        revoked: &[Revocation],
    ) -> anyhow::Result<u64> {
        let mut n = 0u64;
        for (lccn, paths) in &self.by_title {
            let langs = title_languages(lccn);
            let langs: Vec<&str> = langs.iter().map(String::as_str).collect();
            let mut best: HashMap<String, (StoredRow, usize, usize)> = HashMap::new();
            for (i, path) in paths.iter().enumerate() {
                let Some(bytes) = curated.get(path).await? else {
                    continue;
                };
                let run = run_of(path);
                let rows = pages::read_scores(Bytes::from(bytes)).with_context(|| path.clone())?;
                for (r, row) in rows.into_iter().enumerate() {
                    if !published.contains(row.batch.as_str())
                        || revoked.iter().any(|r| r.matches(&row, run))
                    {
                        continue;
                    }
                    let take = best
                        .get(&row.doc_id)
                        .is_none_or(|(b, j, q)| prefer((&row, i, r), (b, *j, *q)));
                    if take {
                        best.insert(row.doc_id.clone(), (row, i, r));
                    }
                }
            }
            n += best
                .values()
                .filter(|(r, _, _)| rule::verdict(&r.ours, &r.loc, Some(&langs)).is_ok())
                .count() as u64;
        }
        Ok(n)
    }
}

/// The import run a merged part came from: its file name's stem.
fn run_of(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|f| f.strip_suffix(".parquet"))
        .unwrap_or_default()
}

/// Whether row `a` (the `r`th row of the `i`th part in path order) is
/// chosen over `b`: the lower stored damage rate, then the newer, then the
/// later part, then the later row.
fn prefer(a: (&StoredRow, usize, usize), b: (&StoredRow, usize, usize)) -> bool {
    let rate = |r: &StoredRow| r.ours.damage_rate.unwrap_or(f64::INFINITY);
    let (ra, rb) = (rate(a.0), rate(b.0));
    if ra != rb {
        return ra < rb;
    }
    (a.0.ocred_at, a.1, a.2) > (b.0.ocred_at, b.1, b.2)
}

/// One page's chosen reading.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub row: StoredRow,
    pub text: String,
}

impl Candidate {
    pub fn engine(&self) -> usnm_state::ocr_local::Engine {
        usnm_state::ocr_local::Engine {
            source: self.row.ocr_source.clone(),
            engine: engine_key(&self.row.ocr_engine),
        }
    }
}

/// One batch's titles' pages.
#[derive(Debug, Default)]
pub struct Loaded {
    pub pages: HashMap<String, Candidate>,
    /// Pages whose every row a revocation matched.
    pub revoked: HashSet<String>,
    /// The parts read, with their sizes.
    pub parts: Vec<(String, u64)>,
}

/// A create-only `ocr-local/revoked/<stamp>.json`: the rows it names are
/// left out of every release from then on. Each selector given must match
/// (at least one is required); a page whose rows are all withdrawn keeps
/// LoC's text. Published indexes keep our text until the next full rebuild.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    pub reason: String,
    /// An issue, an incident, a report.
    pub reference: String,
    /// The node that read the rows (the merged parts' `node`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The engine: its full tag, or its short key (`<family>/<version> <profile>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// The `ocr-sync` import run (`ocr-local/pages/<lccn>/<run>.parquet`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    /// Read at or after this time (`ocred_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// Read before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub path: String,
}

impl Revocation {
    pub fn matches(&self, row: &StoredRow, run: &str) -> bool {
        let at = super::from_micros(row.ocred_at);
        self.node.as_ref().is_none_or(|n| *n == row.node)
            && self
                .engine
                .as_ref()
                .is_none_or(|e| *e == row.ocr_engine || *e == engine_key(&row.ocr_engine))
            && self.run.as_ref().is_none_or(|r| r == run)
            && self.from.is_none_or(|f| at >= f)
            && self.to.is_none_or(|t| at < t)
    }

    fn check(&self) -> anyhow::Result<()> {
        if self.node.is_none()
            && self.engine.is_none()
            && self.run.is_none()
            && self.from.is_none()
            && self.to.is_none()
        {
            bail!("names no node, engine, run or time");
        }
        if self.reason.trim().is_empty() || self.reference.trim().is_empty() {
            bail!("has no reason or reference");
        }
        for (name, v) in [
            ("node", &self.node),
            ("engine", &self.engine),
            ("run", &self.run),
        ] {
            if v.as_ref().is_some_and(|v| v.trim().is_empty()) {
                bail!("has an empty `{name}`, which matches nothing");
            }
        }
        if let (Some(from), Some(to)) = (self.from, self.to) {
            if from >= to {
                bail!("has `from` at or after `to`, which matches nothing");
            }
        }
        Ok(())
    }
}

/// Every revocation. One that can't be read stops the release: a
/// withdrawn node's text mustn't go live because its file has a typo.
pub async fn revocations(curated: &dyn ObjectStore) -> anyhow::Result<Vec<Revocation>> {
    let prefix = revoked_prefix();
    let mut out = Vec::new();
    let mut paths = curated.list(&prefix).await?;
    paths.sort();
    for path in paths.into_iter().filter(|p| p.ends_with(".json")) {
        let bytes = curated
            .get(&path)
            .await?
            .with_context(|| format!("`{path}` vanished"))?;
        let mut r: Revocation =
            serde_json::from_slice(&bytes).with_context(|| format!("revocation `{path}`"))?;
        r.check().with_context(|| format!("revocation `{path}`"))?;
        r.path = path;
        out.push(r);
    }
    Ok(out)
}

/// What the release did with `ocr-local`, for `ocr_local.json`.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct Tally {
    /// Pages of the batches this release indexes, on the copy the version
    /// keeps, with a reading of ours.
    pub considered: u64,
    pub replaced: u64,
    /// Pages whose LoC text stays, by why: the rule's reasons, `revoked`
    /// and `japanese_ocr` (the Japanese overlay has the page).
    pub kept: BTreeMap<String, u64>,
    /// The pages the rule weighed, by our text's language and by engine.
    pub by_language: BTreeMap<String, Count>,
    pub by_engine: BTreeMap<String, Count>,
    /// The parts read, with each title's read once per batch holding it.
    #[serde(skip)]
    pub parts: BTreeMap<String, u64>,
}

impl Tally {
    pub fn keep(&mut self, why: &str) {
        self.considered += 1;
        *self.kept.entry(why.to_owned()).or_default() += 1;
    }

    /// A page the rule weighed.
    pub fn weighed(&mut self, c: &Candidate, ours: &Scores, verdict: Result<(), Keep>) {
        self.considered += 1;
        let ok = verdict.is_ok();
        self.replaced += u64::from(ok);
        if let Err(k) = verdict {
            *self.kept.entry(k.as_str().to_owned()).or_default() += 1;
        }
        let lang = ours.lang.clone().unwrap_or_else(|| "none".into());
        for t in [
            self.by_language.entry(lang).or_default(),
            self.by_engine
                .entry(engine_key(&c.row.ocr_engine))
                .or_default(),
        ] {
            t.considered += 1;
            t.replaced += u64::from(ok);
        }
    }

    pub fn add_parts(&mut self, parts: &[(String, u64)]) {
        for (p, b) in parts {
            self.parts.insert(p.clone(), *b);
        }
    }

    /// The parts summary: count, bytes and the sha256 of the sorted path
    /// list (one path a line), for the manifest and `ocr_local.json`.
    pub fn parts_summary(&self) -> Value {
        let mut h = Sha256::new();
        for p in self.parts.keys() {
            h.update(p.as_bytes());
            h.update(b"\n");
        }
        json!({
            "count": self.parts.len(),
            "bytes": self.parts.values().sum::<u64>(),
            "paths_sha256": hex(&h.finalize()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rate: Option<f64>, at: i64) -> StoredRow {
        StoredRow {
            doc_id: "sn1_1900-01-01_ed-1_seq-1".into(),
            lccn: "sn1".into(),
            batch: "batch_a".into(),
            ocr_source: "usnm-tesseract".into(),
            ocr_engine: "tesseract/5.5.1 profile=eng".into(),
            node: "node1".into(),
            metric_version: usnm_core::damage::VERSION,
            ocred_at: at,
            ours: Scores {
                damage_rate: rate,
                ..Scores::default()
            },
            loc: Scores::default(),
        }
    }

    #[test]
    fn the_lowest_stored_rate_wins_then_the_newest() {
        assert!(prefer(
            (&row(Some(0.1), 1), 0, 0),
            (&row(Some(0.2), 9), 1, 0)
        ));
        assert!(!prefer(
            (&row(Some(0.2), 9), 1, 0),
            (&row(Some(0.1), 1), 0, 0)
        ));
        // Ties: the newest, then the later part.
        assert!(prefer(
            (&row(Some(0.1), 2), 0, 0),
            (&row(Some(0.1), 1), 1, 0)
        ));
        assert!(prefer(
            (&row(Some(0.1), 1), 1, 0),
            (&row(Some(0.1), 1), 0, 0)
        ));
        // An unscored row loses to any scored one.
        assert!(prefer((&row(Some(0.9), 0), 0, 0), (&row(None, 9), 1, 0)));
    }

    #[test]
    fn a_revocation_matches_every_selector_it_gives() {
        let r = |v: Value| -> Revocation { serde_json::from_value(v).unwrap() };
        let base = json!({"reason": "bad node", "reference": "#296"});
        let with = |k: &str, v: Value| {
            let mut b = base.clone();
            b[k] = v;
            r(b)
        };
        let x = row(Some(0.1), 1_000_000);
        assert!(with("node", "node1".into()).matches(&x, "run1"));
        assert!(!with("node", "node2".into()).matches(&x, "run1"));
        assert!(with("engine", "tesseract/5.5.1 eng".into()).matches(&x, "run1"));
        assert!(with("engine", "tesseract/5.5.1 profile=eng".into()).matches(&x, "run1"));
        assert!(!with("engine", "kraken/5 eng".into()).matches(&x, "run1"));
        assert!(with("run", "run1".into()).matches(&x, "run1"));
        assert!(!with("run", "run1".into()).matches(&x, "run2"));
        assert!(with("from", "1970-01-01T00:00:01Z".into()).matches(&x, "r"));
        assert!(!with("to", "1970-01-01T00:00:01Z".into()).matches(&x, "r"));
        let mut both = with("node", "node1".into());
        both.run = Some("other".into());
        assert!(!both.matches(&x, "run1"), "every selector must match");
        assert!(r(base.clone()).check().is_err(), "a selector is required");
        assert!(with("node", "node1".into()).check().is_ok());
        assert!(
            with("node", " ".into()).check().is_err(),
            "an empty selector"
        );
        let mut window = with("from", "2026-10-02T00:00:00Z".into());
        window.to = Some("2026-10-01T00:00:00Z".parse().unwrap());
        assert!(window.check().is_err(), "a reversed window");
        window.to = window.from;
        assert!(window.check().is_err(), "an empty window");
        assert!(serde_json::from_value::<Revocation>(json!({
            "reason": "x", "reference": "y", "nodes": "z"
        }))
        .is_err());
    }

    /// A part `ocr-sync` scored with another metric version: its stored
    /// scores read as none, so they rank last and estimate nothing.
    #[test]
    fn scores_of_another_metric_version_are_not_compared() {
        use std::sync::Arc;
        let text = "The city council met on Tuesday and voted for the new school on the hill \
            and the mayor said the railroad would bring more trade to the town and its farms.";
        let imported = pages::Imported {
            row: usnm_overlay::OverlayRow {
                key: usnm_core::ids::PageKey::new(
                    "sn1",
                    chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
                    1,
                    1,
                )
                .unwrap(),
                batch: "batch_a".into(),
                ocr_source: "usnm-tesseract".into(),
                ocr_engine: "tesseract/5.5.1 profile=eng".into(),
                loc_text: "bulk".into(),
                text: text.into(),
                image_url: "https://tile.loc.gov/x".into(),
                ocred_at: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            },
            batch_version: 1,
            source_part: "pages/x.parquet".into(),
            node: "node1".into(),
            reason: "bulk".into(),
            ours: Scores::of(text, &["eng"]),
            loc: Scores::of("", &["eng"]),
        };
        let bytes = Bytes::from(pages::encode(&[imported]).unwrap());
        let now = pages::read_scores(bytes.clone()).unwrap();
        assert_eq!(now[0].metric_version, usnm_core::damage::VERSION);
        assert!(now[0].ours.damage_rate.is_some());
        // The same part as an older metric wrote it.
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(bytes)
            .unwrap()
            .build()
            .unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let i = batch.schema().index_of("metric_version").unwrap();
        let mut cols = batch.columns().to_vec();
        cols[i] = Arc::new(arrow_array::Int32Array::from(vec![1]));
        let old = arrow_array::RecordBatch::try_new(batch.schema(), cols).unwrap();
        let mut w = parquet::arrow::ArrowWriter::try_new(Vec::new(), old.schema(), None).unwrap();
        w.write(&old).unwrap();
        let rows = pages::read_scores(Bytes::from(w.into_inner().unwrap())).unwrap();
        assert_eq!(rows[0].metric_version, 1);
        assert_eq!(rows[0].ours, Scores::default());
        assert!(
            prefer((&now[0], 0, 0), (&rows[0], 1, 0)),
            "the current metric's row wins"
        );
        assert!(rule::verdict(&rows[0].ours, &rows[0].loc, None).is_err());
    }

    #[test]
    fn names_the_import_run_of_a_part() {
        assert_eq!(
            run_of("ocr-local/pages/sn1/20261011T000000Z-ab.parquet"),
            "20261011T000000Z-ab"
        );
    }
}
