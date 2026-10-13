//! Reserve explicitly allocated BOINC pages using the same lock and native
//! requested-work ledger as enqueue/seed. Does not send Azure queue messages.
use super::{ledger, sync::Lease, LOCK};
use usnm_state::state::State;
use usnm_store::ObjectStore;

pub async fn reserve(
    state: &State,
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    row: &ledger::Queued,
    run: &str,
) -> anyhow::Result<()> {
    let owner = crate::owner_id();
    let lease = Lease::take(state, &owner).await?;
    let result = async {
        let version = state
            .current_version()
            .await?
            .ok_or_else(|| anyhow::anyhow!("no published index version"))?;
        let bytes = reference
            .get(&format!("{version}/batches.json"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("published batch list missing"))?;
        let batches: Vec<usnm_state::state::RunBatch> = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            batches
                .iter()
                .any(|b| b.batch == row.batch && b.curated.version == row.batch_version),
            "selected batch version is not published"
        );
        let titles = crate::ocr_damage::title_languages(reference, &version).await?;
        let key = usnm_core::ids::PageKey::from_doc_id(&row.doc_id)?;
        let langs = titles
            .get(&key.lccn)
            .ok_or_else(|| anyhow::anyhow!("selected title is not published"))?;
        anyhow::ensure!(
            !langs.iter().any(|l| l == "jpn"),
            "Japanese titles use the existing Japanese OCR pipeline"
        );
        let ledger = ledger::load(curated).await?;
        if let Some(old) = ledger.queued.get(&row.doc_id) {
            if old == row {
                return Ok(());
            } // Replay after a reservation response was lost.
            anyhow::bail!("page has prior OCR work; allocate a fresh pilot cohort");
        }
        lease.check()?;
        ledger::write_queued(curated, std::slice::from_ref(row), row.queued_at, run).await?;
        Ok(())
    }
    .await;
    drop(lease);
    let unlock = state.unlock(LOCK, &owner).await;
    result?;
    unlock
}
