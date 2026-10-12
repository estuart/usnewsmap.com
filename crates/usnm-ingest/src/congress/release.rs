//! What a release with `--congress` (`USNM_CONGRESS`) builds from the
//! curated Congress units (#229 design §4, §7): the Congress index set
//! (`congress-{stamp}-{n}`), `congress.json` (the units it was built from),
//! `congress/baselines.json` (pages printed per series and day) and the
//! `current.json` block the API reads (`refdata::CongressIndexes`).
//!
//! The index is rebuilt from scratch when the units differ from the
//! published version's `congress.json`, or the version moves to another
//! analyzer; otherwise the new version keeps the published index and
//! carries its two files forward. No deltas: the whole set is small.

use std::collections::BTreeMap;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use usnm_core::time::date_from_day;
use usnm_store::ObjectStore;

use super::curated::{self, Counts};
use super::globe;

/// The snapshot file naming the units a version's Congress index holds.
pub const FILE: &str = "congress.json";

/// The Congress baselines: `{series: [[day, pages], …]}`, every page
/// printed, with text or not, as the newspaper baselines count (04 §4.5).
pub const BASELINES_FILE: &str = "congress/baselines.json";

/// One unit a version holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitRef {
    pub series: String,
    pub unit: String,
    pub attempt: u32,
    /// Its `counts.json`, which lists its parts with their sha256.
    pub counts: String,
    pub sha256: String,
    pub pages: u64,
    pub indexable: u64,
}

/// The finished units a release reads.
#[derive(Debug, Default)]
pub struct Listing {
    pub units: Vec<(UnitRef, Counts)>,
    /// Units left out: superseded by another unit (`globe::SUPERSEDED`) or
    /// by a later empty attempt of their own (`curated::Empty`: a Serial Set
    /// volume whose documents are now all left out), or of a series this
    /// code doesn't know. A unit with only empty attempts isn't listed.
    pub left_out: Vec<String>,
}

impl Listing {
    /// Each unit's newest finished attempt.
    pub async fn load(store: &dyn ObjectStore) -> anyhow::Result<Self> {
        let mut out = Listing::default();
        let empties = curated::empties(store).await?;
        for f in curated::finished(store).await? {
            let superseded = f.series == globe::SERIES
                && globe::SUPERSEDED.iter().any(|(id, _, _)| *id == f.unit);
            if superseded
                || curated::superseded_by_empty(&empties, &f)
                || !super::SERIES.contains(&f.series.as_str())
            {
                out.left_out.push(format!("{}/{}", f.series, f.unit));
                continue;
            }
            let (counts, sha256) = curated::read_counts(store, &f).await?;
            anyhow::ensure!(
                counts.series == f.series && counts.unit == f.unit && counts.attempt == f.attempt,
                "`{}` describes another unit",
                f.counts_path
            );
            out.units.push((
                UnitRef {
                    series: f.series,
                    unit: f.unit,
                    attempt: f.attempt,
                    counts: f.counts_path,
                    sha256,
                    pages: counts.pages,
                    indexable: counts.indexable,
                },
                counts,
            ));
        }
        Ok(out)
    }

    pub fn refs(&self) -> Vec<UnitRef> {
        self.units.iter().map(|(u, _)| u.clone()).collect()
    }

    /// Pages per series and day, from every unit's counts.
    pub fn baselines(&self) -> BTreeMap<String, Vec<(u32, u32)>> {
        let mut by: BTreeMap<String, BTreeMap<u32, u32>> = BTreeMap::new();
        for (u, c) in &self.units {
            let s = by.entry(u.series.clone()).or_default();
            for (day, pages) in &c.days {
                *s.entry(*day).or_default() += pages;
            }
        }
        by.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    }

    /// Pages that go into the index.
    pub fn indexable(&self) -> u64 {
        self.units.iter().map(|(u, _)| u.indexable).sum()
    }
}

/// `current.json`'s `congress` series list from the baselines: each series'
/// pages and first and last days.
pub fn series_block(baselines: &BTreeMap<String, Vec<(u32, u32)>>) -> Vec<Value> {
    baselines
        .iter()
        .filter_map(|(series, days)| {
            let (first, _) = days.first()?;
            let (last, _) = days.last()?;
            let pages: u64 = days.iter().map(|(_, p)| u64::from(*p)).sum();
            Some(json!({
                "series": series,
                "pages": pages,
                "from": date_from_day(*first).to_string(),
                "to": date_from_day(*last).to_string(),
            }))
        })
        .collect()
}

/// `congress.json` for a version built from `listing` into `index`.
pub fn record(listing: &Listing, index: &str, indexed: u64, common_grams: u32) -> Value {
    json!({
        "version": super::VERSION,
        "index": index,
        "indexed": indexed,
        "common_grams": common_grams,
        "pages": listing.units.iter().map(|(u, _)| u.pages).sum::<u64>(),
        "units": listing.refs(),
        "left_out": listing.left_out,
    })
}

/// Whether the published version's Congress set can be kept as it is: it
/// was built by this code's rule from exactly these units, with the
/// analyzer this version uses.
pub fn unchanged(published: &Value, listing: &Listing, common_grams: u32) -> anyhow::Result<bool> {
    let units: Vec<UnitRef> = serde_json::from_value(published["units"].clone())
        .context("congress.json has no unit list")?;
    Ok(published["version"] == json!(super::VERSION)
        && published["common_grams"] == json!(common_grams)
        && units == listing.refs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn series_spans_and_pages_from_the_baselines() {
        let mut b = BTreeMap::new();
        let day = |y, m, d| {
            usnm_core::time::day_number(chrono::NaiveDate::from_ymd_opt(y, m, d).unwrap())
        };
        b.insert(
            "globe".to_owned(),
            vec![(day(1838, 3, 28), 4), (day(1838, 7, 18), 2)],
        );
        b.insert("annals".to_owned(), vec![]);
        let s = series_block(&b);
        assert_eq!(
            s,
            [json!({"series": "globe", "pages": 6, "from": "1838-03-28", "to": "1838-07-18"})]
        );
    }
}
