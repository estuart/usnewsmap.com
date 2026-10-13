//! Reserve explicitly allocated BOINC pages using the same lock and native
//! requested-work ledger as enqueue/seed. Does not send Azure queue messages.
use std::collections::{HashMap, HashSet};

use super::{ledger, sync::Lease, LOCK};
use usnm_state::state::{RunBatch, State};
use usnm_store::ObjectStore;

/// Immutable published metadata shared by language routing and reservation.
/// Read before staging scans, then check its version again under the lease.
pub struct Snapshot {
    version: String,
    batches: Vec<RunBatch>,
    pub titles: HashMap<String, Vec<String>>,
}
impl Snapshot {
    pub async fn load(state: &State, reference: &dyn ObjectStore) -> anyhow::Result<Self> {
        let version = state
            .current_version()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no published index version"))?;
        anyhow::ensure!(
            usnm_store::is_safe_segment(&version),
            "invalid published version id"
        );
        let bytes = reference
            .get(&format!("{version}/batches.json"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("published batch list missing"))?;
        let batches = serde_json::from_slice(&bytes)?;
        #[derive(serde::Deserialize)]
        struct Title {
            lccn: String,
            #[serde(default)]
            languages: Vec<String>,
        }
        let bytes = reference
            .get(&format!("{version}/titles.json"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("published title list missing"))?;
        let published: Vec<Title> = serde_json::from_slice(&bytes)?;
        let mut titles = HashMap::new();
        for title in published {
            anyhow::ensure!(
                titles.insert(title.lccn, title.languages).is_none(),
                "duplicate published title"
            );
        }
        Ok(Self {
            version,
            batches,
            titles,
        })
    }
}

/// Validate every row before writing a single create-only cohort part. Exact
/// prior reservations are replays, including when publication of that part
/// succeeded but its response (or the local journal update) was lost.
pub async fn reserve(
    state: &State,
    curated: &dyn ObjectStore,
    snapshot: &Snapshot,
    rows: &[ledger::Queued],
    run: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !rows.is_empty() && rows.len() <= 100,
        "reserve 1 to 100 pages"
    );
    let owner = crate::owner_id();
    let lease = Lease::take(state, &owner).await?;
    let result = async {
        anyhow::ensure!(
            state.current_version().await?.as_deref() == Some(&snapshot.version),
            "published snapshot changed during export; repeat export"
        );
        let mut seen = HashSet::new();
        for row in rows {
            anyhow::ensure!(seen.insert(&row.doc_id), "duplicate cohort page");
            anyhow::ensure!(
                snapshot
                    .batches
                    .iter()
                    .any(|b| { b.batch == row.batch && b.curated.version == row.batch_version }),
                "selected batch version is not published"
            );
            let key = usnm_core::ids::PageKey::from_doc_id(&row.doc_id)?;
            let langs = snapshot
                .titles
                .get(&key.lccn)
                .ok_or_else(|| anyhow::anyhow!("selected title is not published"))?;
            anyhow::ensure!(
                !langs.iter().any(|l| l == "jpn"),
                "Japanese titles use the existing Japanese OCR pipeline"
            );
        }
        let ledger = ledger::load(curated).await?;
        let mut fresh = Vec::new();
        for row in rows {
            if let Some(old) = ledger.queued.get(&row.doc_id) {
                anyhow::ensure!(
                    old == row,
                    "page has prior OCR work; allocate a fresh pilot cohort"
                );
            } else {
                fresh.push(row.clone());
            }
        }
        if let Some(first) = fresh.first() {
            lease.check()?;
            ledger::write_queued(curated, &fresh, first.queued_at, run).await?;
        }
        Ok(())
    }
    .await;
    drop(lease);
    let unlock = state.unlock(LOCK, &owner).await;
    result?;
    unlock
}
