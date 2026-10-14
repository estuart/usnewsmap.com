//! Host trust from the gold check (#468): a `usnm_ocr` result is published only when the host
//! that returned it has a valid gold report for the same engine image with overall CER at or
//! below a threshold.
//!
//! - `usnm-boinc gold trust` reads one or more `reports.jsonl` files (from `gold collect`) and
//!   writes `trust.json`: per engine image, the host ids that passed.
//! - `usnm-boinc collect --trust trust.json --host-id N` checks the result's host before it
//!   publishes. An untrusted or unknown host's result is held back: its spool files and a reason
//!   go to `<root>/quarantine/<job>/`, nothing is published, and the job stays as it was, so a
//!   later collect (after the host passes a gold unit) can publish it.
//!
//! The host id is the client's own claim, so this is a quality signal for trusted volunteers,
//! not an identity check.
use crate::{atomic, load, Stage};
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use usnm_store::ObjectStore;

pub const TRUST_VERSION: u32 = 1;
pub const DEFAULT_MAX_CER: f64 = 0.10;
const MAX_REPORTS: u64 = 64 << 20;

/// `trust.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    /// Engine image digest to the host ids with a passing gold report for it.
    pub hosts: BTreeMap<String, BTreeSet<u64>>,
    pub max_cer: f64,
    pub v: u32,
}

#[derive(Deserialize)]
struct Row {
    engine_image: String,
    host_id: Option<u64>,
    summary: Vec<SummaryRow>,
}
#[derive(Deserialize)]
struct SummaryRow {
    cer: Option<f64>,
    source: String,
}

impl Trust {
    /// Builds the list from `gold collect`'s `reports.jsonl` files. Every row there is a valid
    /// canonical result (collect checked it again); rows with no host id or no overall CER are
    /// skipped.
    pub fn build(reports: &[PathBuf], max_cer: f64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (0.0..=1.0).contains(&max_cer),
            "max CER must be between 0 and 1"
        );
        let mut hosts: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        for path in reports {
            let bytes = crate::read(path, MAX_REPORTS as usize)
                .with_context(|| format!("reading {}", path.display()))?;
            for (n, line) in std::str::from_utf8(&bytes)?.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                let row: Row = serde_json::from_str(line)
                    .with_context(|| format!("{}:{}", path.display(), n + 1))?;
                let overall = row.summary.iter().find(|s| s.source == "all");
                if let (Some(host), Some(cer)) = (row.host_id, overall.and_then(|s| s.cer)) {
                    if cer <= max_cer {
                        hosts.entry(row.engine_image).or_default().insert(host);
                    }
                }
            }
        }
        Ok(Self {
            hosts,
            max_cer,
            v: TRUST_VERSION,
        })
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let t: Self = serde_json::from_slice(&crate::read(path, MAX_REPORTS as usize)?)
            .with_context(|| format!("reading {}", path.display()))?;
        anyhow::ensure!(t.v == TRUST_VERSION, "unsupported trust list version");
        Ok(t)
    }

    /// `None` when the host is trusted for this image; otherwise why not.
    pub fn refuse(&self, engine_image: &str, host_id: Option<u64>) -> Option<String> {
        let Some(host) = host_id else {
            return Some("no host id for this result".into());
        };
        if self
            .hosts
            .get(engine_image)
            .is_some_and(|h| h.contains(&host))
        {
            None
        } else {
            Some(format!(
                "host {host} has no gold report for {engine_image} with CER <= {}",
                self.max_cer
            ))
        }
    }
}

pub fn quarantine_dir(root: &Path, id: &str) -> PathBuf {
    root.join("quarantine").join(id)
}

/// `usnm-boinc collect` with the trust check in front of [`crate::collect`].
pub async fn collect(
    root: &Path,
    spool: &Path,
    id: &str,
    trust: &Trust,
    host_id: Option<u64>,
    curated: &dyn ObjectStore,
    out: &dyn ObjectStore,
) -> anyhow::Result<()> {
    let record = load(root, id)?;
    let wu = record.job.workunit();
    // A terminal failure has no canonical result (so no host id): let collect mark it failed.
    let terminal = spool.join(format!("{wu}_error")).exists()
        || spool.join(format!("{wu}_no_output_files")).exists();
    if record.stage != Stage::Published && !terminal {
        if let Some(reason) = trust.refuse(&record.job.engine.image, host_id) {
            let q = quarantine_dir(root, id);
            std::fs::create_dir_all(&q)?;
            for logical in ["result.json", "text.txt", "layout.xml"] {
                let f = spool.join(format!("{wu}__file_{logical}"));
                if f.is_file() {
                    std::fs::copy(&f, q.join(logical))?;
                }
            }
            atomic(
                &q.join("reason.json"),
                &serde_json::to_vec_pretty(&serde_json::json!({
                    "at": chrono::Utc::now().to_rfc3339(),
                    "engine_image": record.job.engine.image,
                    "host_id": host_id,
                    "job": id,
                    "reason": reason,
                    "workunit": wu,
                }))?,
            )?;
            bail!("held back, not published: {reason}");
        }
    }
    crate::collect(root, spool, id, curated, out).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(image: &str, host: Option<u64>, cer: Option<f64>) -> String {
        serde_json::json!({
            "bundle_version": 1,
            "engine_image": image,
            "host_id": host,
            "summary": [
                {"cer": cer, "crops": 2, "ref_chars": 10, "source": "all", "wer": cer},
                {"cer": 0.0, "crops": 1, "ref_chars": 5, "source": "bln600", "wer": 0.0},
            ],
        })
        .to_string()
    }

    #[test]
    fn builds_from_reports_and_refuses_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.jsonl");
        let b = tmp.path().join("b.jsonl");
        std::fs::write(
            &a,
            [
                row("sha256:aa", Some(1), Some(0.05)),
                row("sha256:aa", Some(2), Some(0.30)),
                row("sha256:aa", None, Some(0.01)),
                row("sha256:aa", Some(3), None),
            ]
            .join("\n"),
        )
        .unwrap();
        std::fs::write(&b, row("sha256:bb", Some(2), Some(0.10))).unwrap();
        let t = Trust::build(&[a, b], DEFAULT_MAX_CER).unwrap();
        assert_eq!(t.hosts["sha256:aa"], BTreeSet::from([1]));
        assert_eq!(t.hosts["sha256:bb"], BTreeSet::from([2]));
        assert!(t.refuse("sha256:aa", Some(1)).is_none());
        assert!(t.refuse("sha256:aa", Some(2)).is_some());
        assert!(t.refuse("sha256:bb", Some(1)).is_some());
        assert!(t.refuse("sha256:cc", Some(1)).is_some());
        assert!(t.refuse("sha256:aa", None).is_some());
        let p = tmp.path().join("trust.json");
        std::fs::write(&p, serde_json::to_vec(&t).unwrap()).unwrap();
        assert_eq!(Trust::load(&p).unwrap(), t);
        assert!(Trust::build(&[], 2.0).is_err());
    }
}
