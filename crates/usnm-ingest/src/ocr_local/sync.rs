//! `usnm-ingest ocr-sync` (12 §12.5 Step 8): one pass over the exchange
//! account, under the Cosmos lock `ops/ocr-sync` so overlapping runs never
//! import twice.
//!
//! 1. With `--from-quarantine --reason <r>`, put that reason's rejects back.
//! 2. Delete sources a stopped run already consumed (its import record says
//!    so), without importing them again.
//! 3. Drain `ocr-deadletter`: valid dead letters to
//!    `ocr-local/deadletter/<day>/<run>.jsonl`, re-encoded, with an outcome
//!    for each page; the rest to quarantine. Then delete them from the queue.
//! 4. Check each part in `ocr-out`, oldest first, up to `--max-parts`, in
//!    imports of `--chunk` parts. For each import: (1) the ALTO of each part
//!    that passes, to `ocr-local/alto/`, as it is checked; (2) one merged
//!    part per title; (3) the outcomes and the import record; (4) delete the
//!    sources with If-Match on the ETags read.
//! 5. With a seed plan, top `ocr-bulk` up ([`super::seed`], Step 10).
//! 6. Write `status/ocr-local.json` and the metrics.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use serde::{Deserialize, Serialize};
use tokio::time::Instant;
use usnm_ocr_contract::message::DeadLetter;
use usnm_overlay::OverlayRow;
use usnm_state::state::{RunBatch, State};
use usnm_store::queue::{Queue, Received, BULK, DEADLETTER, PRIORITY};
use usnm_store::{ObjectInfo, ObjectStore};

use super::checks::{self, Context as CheckContext, PartName, Reject};
use super::ledger::{self, Outcome, DEAD_LETTERED, IMPORTED};
use super::pages::{self, Imported, Scores};
use super::quarantine::{self, Item};
use super::{alto_path, deadletter_path, import_path, pages_path, run_id, ROOT, STATUS_PATH};
use crate::ocr_damage::{self, StoredScore};

/// How long the lock lasts without renewal, and how often it is renewed.
const LOCK_TTL: chrono::Duration = chrono::Duration::minutes(15);
const LOCK_RENEW: Duration = Duration::from_secs(5 * 60);
/// How long a received dead letter stays hidden while it is written.
const DEAD_LETTER_HIDE: Duration = Duration::from_secs(600);
/// How far back import records are read to finish a stopped run's deletes.
const RECOVER_DAYS: i64 = 3;
/// Page tables kept in memory at once.
const TABLE_CACHE: usize = 32;

/// What to do.
#[derive(Debug, Clone)]
pub struct Options {
    /// The most parts checked in one run.
    pub max_parts: usize,
    /// Parts per import (each import writes its own merged parts, record
    /// and deletes, so a stopped run loses at most one import's work).
    pub chunk: usize,
    /// The most dead letters drained in one run.
    pub max_dead_letters: usize,
    /// Put the rejects with this reason back first.
    pub from_quarantine: Option<String>,
    /// Stop starting imports after this.
    pub deadline: Option<Instant>,
    /// The seeder's settings; off while its plan is empty.
    pub seed: super::seed::Options,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_parts: 2000,
            chunk: 200,
            max_dead_letters: 5000,
            from_quarantine: None,
            deadline: None,
            seed: super::seed::Options::default(),
        }
    }
}

/// The stores a run reads and writes.
#[derive(Debug, Clone)]
pub struct Stores {
    pub curated: Arc<dyn ObjectStore>,
    pub reference: Arc<dyn ObjectStore>,
    /// The exchange account's `ocr-out`.
    pub out: Arc<dyn ObjectStore>,
    /// The exchange account's `ocr-quarantine`.
    pub quarantine: Arc<dyn ObjectStore>,
    /// The exchange account's queues.
    pub queue: Arc<dyn Queue>,
}

/// A run's counts.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub imported: u64,
    /// Parts rejected, by reason.
    pub rejected: BTreeMap<String, u64>,
    /// Dead letters drained, by `cause` or `cause/kind` (`v1` for v1).
    pub dead_letters: BTreeMap<String, u64>,
    /// Dead letters that failed their checks, by reason.
    pub dead_letters_rejected: BTreeMap<String, u64>,
    /// Sources a stopped run had consumed, deleted now.
    pub cleaned: u64,
    /// Parts written again since they were listed; left for the next run.
    pub changed: u64,
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.imported += other.imported;
        for (map, from) in [
            (&mut self.rejected, &other.rejected),
            (&mut self.dead_letters, &other.dead_letters),
            (
                &mut self.dead_letters_rejected,
                &other.dead_letters_rejected,
            ),
        ] {
            for (k, v) in from {
                *map.entry(k.clone()).or_default() += v;
            }
        }
        self.cleaned += other.cleaned;
        self.changed += other.changed;
    }
}

/// `status/ocr-local.json`.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub updated_at: DateTime<Utc>,
    pub run: String,
    /// The published index version the checks ran against.
    pub index_version: String,
    pub last_run: Counts,
    pub totals: Counts,
    /// Approximate message counts, hidden ones included.
    pub queues: BTreeMap<String, u64>,
    /// Pages queued with no result yet, by the ledger.
    pub outstanding: u64,
    /// Parts left in `ocr-out` after the run, and the oldest one's age.
    pub parts_left: u64,
    pub oldest_age_s: u64,
    /// The run's seeding, when a plan is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<super::seed::SeedReport>,
}

/// One source the run consumed (imported, or moved to quarantine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumed {
    pub path: String,
    pub etag: String,
    /// `imported` or `quarantined`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `ocr-local/import/<run>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportRecord {
    pub run: String,
    pub at: DateTime<Utc>,
    pub index_version: String,
    pub sources: Vec<Consumed>,
    pub pages: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcomes: Option<String>,
}

/// The lock, renewed in the background until dropped.
pub(super) struct Lease {
    lost: Arc<AtomicBool>,
    renewer: tokio::task::JoinHandle<()>,
}

impl Lease {
    pub(super) async fn take(state: &State, owner: &str) -> anyhow::Result<Self> {
        state.lock(super::LOCK, owner, LOCK_TTL).await?;
        let lost = Arc::new(AtomicBool::new(false));
        let (state, owner, flag) = (state.clone(), owner.to_owned(), lost.clone());
        let renewer = tokio::spawn(async move {
            loop {
                tokio::time::sleep(LOCK_RENEW).await;
                if let Err(e) = state.lock(super::LOCK, &owner, LOCK_TTL).await {
                    tracing::error!(error = %format!("{e:#}"), "ocr-sync lock renewal failed");
                    flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
        });
        Ok(Self { lost, renewer })
    }

    pub(super) fn check(&self) -> anyhow::Result<()> {
        if self.lost.load(Ordering::SeqCst) {
            bail!("the ocr-sync lock could not be renewed; stopping before the next write");
        }
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.renewer.abort();
    }
}

/// Run one pass under the lock.
pub async fn run(
    state: &State,
    owner: &str,
    stores: &Stores,
    opts: &Options,
) -> anyhow::Result<Status> {
    let lease = Lease::take(state, owner).await?;
    let result = run_locked(state, stores, opts, &lease).await;
    drop(lease);
    let unlocked = state.unlock(super::LOCK, owner).await;
    let status = result?;
    unlocked?;
    Ok(status)
}

fn secs_since(t: Option<SystemTime>, now: SystemTime) -> u64 {
    t.and_then(|t| now.duration_since(t).ok())
        .map_or(0, |d| d.as_secs())
}

fn to_utc(t: Option<SystemTime>) -> Option<DateTime<Utc>> {
    t.map(DateTime::<Utc>::from)
}

/// The published version's batches, titles and the ledger.
async fn context(
    state: &State,
    stores: &Stores,
) -> anyhow::Result<(String, HashMap<String, RunBatch>, CheckContext)> {
    let version = state
        .current_version()
        .await?
        .context("no published version: nothing to check node output against")?;
    let batches = ocr_damage::version_batches(stores.reference.as_ref(), &version, &[]).await?;
    let titles = ocr_damage::title_languages(stores.reference.as_ref(), &version).await?;
    let ledger = ledger::load(stores.curated.as_ref()).await?;
    let published = batches
        .iter()
        .map(|b| (b.batch.clone(), b.curated.version))
        .collect();
    let by_name = batches.into_iter().map(|b| (b.batch.clone(), b)).collect();
    Ok((
        version,
        by_name,
        CheckContext {
            published,
            titles,
            ledger,
        },
    ))
}

/// Sources recent import records list as consumed.
async fn consumed_before(
    curated: &dyn ObjectStore,
    now: DateTime<Utc>,
) -> anyhow::Result<HashSet<(String, String)>> {
    let since = (now - chrono::Duration::days(RECOVER_DAYS))
        .format("%Y%m%d")
        .to_string();
    let mut out = HashSet::new();
    for path in curated.list(&format!("{ROOT}/import")).await? {
        let name = path.rsplit('/').next().unwrap_or_default();
        if name.get(..8).is_none_or(|d| d < since.as_str()) {
            continue;
        }
        let Some(body) = curated.get(&path).await? else {
            continue;
        };
        let record: ImportRecord =
            serde_json::from_slice(&body).with_context(|| format!("reading {path}"))?;
        for s in record.sources {
            out.insert((s.path, s.etag));
        }
    }
    Ok(out)
}

/// LoC's scores for pages, from Step 6's tables for the published version,
/// or from the curated text for batches the tables lack.
struct LocScores {
    tables: Option<HashMap<String, String>>,
    cache: HashMap<String, HashMap<String, StoredScore>>,
}

impl LocScores {
    async fn new(curated: &dyn ObjectStore, version: &str) -> anyhow::Result<Self> {
        let tables = match curated.get(&ocr_damage::versions_path(version)).await? {
            Some(body) => {
                let list: ocr_damage::VersionList = serde_json::from_slice(&body)
                    .context("reading the damage tables' version list")?;
                Some(
                    list.batches
                        .into_iter()
                        .map(|b| (b.batch, b.table))
                        .collect(),
                )
            }
            None => None,
        };
        Ok(Self {
            tables,
            cache: HashMap::new(),
        })
    }

    async fn get(
        &mut self,
        curated: &dyn ObjectStore,
        batch: &RunBatch,
        titles: &HashMap<String, Vec<String>>,
        doc_ids: &HashSet<String>,
    ) -> anyhow::Result<HashMap<String, Scores>> {
        let table_path = self
            .tables
            .as_ref()
            .and_then(|t| t.get(&batch.batch))
            .cloned();
        if let Some(path) = table_path {
            // A page table covers the whole batch: read it once.
            if !self.cache.contains_key(&batch.batch) {
                if self.cache.len() >= TABLE_CACHE {
                    self.cache.clear();
                }
                let bytes = curated
                    .get(&path)
                    .await?
                    .with_context(|| format!("damage table {path} not found"))?;
                let scores = ocr_damage::read_scores(Bytes::from(bytes), None)?;
                self.cache.insert(batch.batch.clone(), scores);
            }
        } else {
            // From the curated text, only the pages asked for: score those
            // not scored yet and add them.
            let have = self.cache.entry(batch.batch.clone()).or_default();
            let missing: HashSet<String> = doc_ids
                .iter()
                .filter(|d| !have.contains_key(*d))
                .cloned()
                .collect();
            if !missing.is_empty() {
                let found = score_from_curated(curated, batch, titles, &missing).await?;
                self.cache
                    .get_mut(&batch.batch)
                    .expect("inserted above")
                    .extend(found);
            }
        }
        let table = &self.cache[&batch.batch];
        Ok(doc_ids
            .iter()
            .map(|d| {
                let s = table.get(d).map(|s| Scores {
                    lang: s.lang.clone(),
                    decision: Some(s.decision.clone()),
                    damage_rate: s.damage_rate,
                    function_share: s.function_share,
                    head: Some(s.head),
                });
                (d.clone(), s.unwrap_or_default())
            })
            .collect())
    }
}

/// Score the wanted pages' LoC text from a batch's curated parts.
pub(super) async fn score_from_curated(
    curated: &dyn ObjectStore,
    batch: &RunBatch,
    titles: &HashMap<String, Vec<String>>,
    doc_ids: &HashSet<String>,
) -> anyhow::Result<HashMap<String, StoredScore>> {
    let lccns: HashSet<&str> = doc_ids.iter().filter_map(|d| d.split('_').next()).collect();
    let mut out = HashMap::new();
    for part in &batch.curated.parts {
        let bytes = curated
            .get(part)
            .await?
            .with_context(|| format!("curated part {part} not found"))?;
        let wanted = doc_ids.clone();
        let titles = titles.clone();
        let lccns: HashSet<String> = lccns.iter().map(|s| (*s).to_owned()).collect();
        let found =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ocr_damage::PageRow>> {
                let mut rows = Vec::new();
                crate::curated::read_part(Bytes::from(bytes), true, |row| {
                    if lccns.contains(&row.key.lccn) && wanted.contains(&row.key.doc_id()) {
                        let langs: Vec<&str> = titles
                            .get(&row.key.lccn)
                            .map(|l| l.iter().map(String::as_str).collect())
                            .unwrap_or_default();
                        rows.push(ocr_damage::score_row(&row, &langs));
                    }
                    Ok(())
                })?;
                Ok(rows)
            })
            .await??;
        for r in found {
            out.insert(
                r.doc_id.clone(),
                StoredScore {
                    batch: r.batch,
                    batch_version: r.batch_version,
                    lang: r.lang,
                    decision: r.decision.to_owned(),
                    damage_rate: r.damage_rate,
                    function_share: r.function_share,
                    head: r.head,
                },
            );
        }
    }
    Ok(out)
}

/// Counts dead letters by cause (and kind, for `invalid`).
fn dead_letter_key(d: &DeadLetter) -> String {
    match (d.cause, &d.kind) {
        (None, _) => "v1".into(),
        (Some(c), Some(k)) => format!("{}/{k}", c.as_str()),
        (Some(c), None) => c.as_str().into(),
    }
}

/// Dead letters received, written and deleted at a time: well inside
/// [`DEAD_LETTER_HIDE`], so none comes back while the run holds it.
const DEAD_LETTER_BATCH: usize = 100;

async fn drain_dead_letters(
    stores: &Stores,
    run: &str,
    max: usize,
    counts: &mut Counts,
    written: &mut Vec<Outcome>,
    lease: &Lease,
) -> anyhow::Result<()> {
    let mut drained = 0;
    let mut n = 0;
    while drained < max {
        let limit = DEAD_LETTER_BATCH.min(max - drained);
        let tag = format!("{run}-dl{n:03}");
        let got = drain_batch(stores, &tag, limit, counts, written, lease).await?;
        drained += got;
        n += 1;
        if got < limit {
            break;
        }
    }
    Ok(())
}

/// Receive up to `max` dead letters, write them, then delete them; the
/// number received.
async fn drain_batch(
    stores: &Stores,
    run: &str,
    max: usize,
    counts: &mut Counts,
    written: &mut Vec<Outcome>,
    lease: &Lease,
) -> anyhow::Result<usize> {
    let mut valid: Vec<(Received, DeadLetter)> = Vec::new();
    let mut rejected: Vec<Received> = Vec::new();
    while valid.len() + rejected.len() < max {
        let Some(msg) = stores.queue.receive(DEADLETTER, DEAD_LETTER_HIDE).await? else {
            break;
        };
        match checks::check_dead_letter(&msg.text) {
            Ok(d) => valid.push((msg, d)),
            Err(r) => {
                let code = r.code();
                quarantine::put(
                    stores.quarantine.as_ref(),
                    run,
                    Item {
                        reason: code.clone(),
                        kind: quarantine::DEAD_LETTER,
                        source: msg.id.clone(),
                        etag: None,
                        size: msg.text.len() as u64,
                        created: None,
                        content: Some(msg.text.clone().into_bytes()),
                    },
                )
                .await?;
                tracing::warn!(message_id = %msg.id, reason = %code, "dead letter quarantined");
                *counts.dead_letters_rejected.entry(code).or_default() += 1;
                rejected.push(msg);
            }
        }
    }
    if !valid.is_empty() {
        lease.check()?;
        let now = Utc::now();
        let mut lines = String::new();
        let mut outcomes = Vec::new();
        for (msg, d) in &valid {
            lines.push_str(&serde_json::to_string(d)?);
            lines.push('\n');
            *counts.dead_letters.entry(dead_letter_key(d)).or_default() += 1;
            if let Some(m) = &d.message {
                // The message passed its checks, so its page parses.
                if let Ok(key) = m.key() {
                    outcomes.push(Outcome {
                        doc_id: key.doc_id(),
                        outcome: DEAD_LETTERED.into(),
                        cause: d.cause.map(|c| c.as_str().to_owned()),
                        kind: d.kind.clone(),
                        source: msg.id.clone(),
                        at: now,
                        run: run.to_owned(),
                    });
                }
            }
        }
        let path = deadletter_path(now, run);
        if !stores
            .curated
            .put_new(&path, lines.into_bytes(), "application/x-ndjson")
            .await?
        {
            bail!("{path} already exists");
        }
        ledger::write_outcomes(stores.curated.as_ref(), &outcomes, now, run).await?;
        written.extend(outcomes);
        tracing::info!(path, dead_letters = valid.len(), "dead letters written");
    }
    for msg in valid.iter().map(|(m, _)| m).chain(rejected.iter()) {
        if let Err(e) = stores.queue.delete(msg).await {
            tracing::warn!(message_id = %msg.id, error = %format!("{e:#}"), "dead letter not deleted; it comes back and is drained again");
        }
    }
    Ok(valid.len() + rejected.len())
}

/// The run.
async fn run_locked(
    state: &State,
    stores: &Stores,
    opts: &Options,
    lease: &Lease,
) -> anyhow::Result<Status> {
    let started = Utc::now();
    let run = run_id(started);
    let metrics = crate::telemetry::metrics();
    let mut counts = Counts::default();

    if let Some(reason) = &opts.from_quarantine {
        let r = quarantine::replay(
            stores.quarantine.as_ref(),
            stores.out.as_ref(),
            &stores.queue,
            reason,
        )
        .await?;
        tracing::info!(reason, ?r, "quarantine replayed");
    }

    let (version, batches, ctx) = context(state, stores).await?;

    // Parts first, then ALTO: a node writes a page's ALTO before its part,
    // so every part listed has its ALTO in the second listing.
    let mut parts = stores.out.list_info("pages").await?;
    let altos = stores.out.list_info("alto").await?;

    // A stopped run's consumed sources: delete without importing again.
    let consumed = consumed_before(stores.curated.as_ref(), started).await?;
    let mut gone: HashSet<String> = HashSet::new();
    for o in parts.iter().chain(altos.iter()) {
        if consumed.contains(&(o.path.clone(), o.etag.clone())) {
            lease.check()?;
            if stores.out.delete_if_match(&o.path, &o.etag).await? {
                counts.cleaned += 1;
            }
            gone.insert(o.path.clone());
        }
    }
    parts.retain(|p| !gone.contains(&p.path));
    let alto_by_path: HashMap<String, ObjectInfo> = altos
        .into_iter()
        .filter(|a| !gone.contains(&a.path))
        .map(|a| (a.path.clone(), a))
        .collect();
    // An ALTO's node and stamp, so a rejected part's ALTO goes with it.
    let alto_by_run: HashMap<(String, String), String> = alto_by_path
        .keys()
        .filter_map(|p| {
            let name = p.strip_prefix("alto/")?.strip_suffix(".xml")?;
            let mut it = name.rsplitn(3, '.');
            let (stamp, node) = (it.next()?, it.next()?);
            Some(((node.to_owned(), stamp.to_owned()), p.clone()))
        })
        .collect();

    // Outcomes this run writes, for the status's outstanding count.
    let mut written: Vec<Outcome> = Vec::new();
    drain_dead_letters(
        stores,
        &run,
        opts.max_dead_letters,
        &mut counts,
        &mut written,
        lease,
    )
    .await?;

    // Oldest first, cheapest first.
    parts.sort_by(|a, b| (a.created, a.size, &a.path).cmp(&(b.created, b.size, &b.path)));
    let total_parts = parts.len();
    let mut taken: HashSet<String> = HashSet::new();
    let mut loc = LocScores::new(stores.curated.as_ref(), &version).await?;
    for (n, chunk) in parts
        .iter()
        .take(opts.max_parts)
        .collect::<Vec<_>>()
        .chunks(opts.chunk.max(1))
        .enumerate()
    {
        if opts.deadline.is_some_and(|d| Instant::now() >= d) {
            tracing::info!("max runtime reached; the rest waits for the next run");
            break;
        }
        let import_run = format!("{run}-{n:04}");
        import(
            stores,
            &import_run,
            &version,
            &batches,
            &ctx,
            &mut loc,
            chunk,
            &alto_by_path,
            &alto_by_run,
            &mut counts,
            &mut taken,
            &mut written,
            lease,
        )
        .await?;
    }

    // Seeding, after the imports so the ledger has their outcomes.
    let seeded = if !opts.seed.on() {
        None
    } else if opts.deadline.is_some_and(|d| Instant::now() >= d) {
        tracing::info!("max runtime reached; seeding waits for the next run");
        None
    } else {
        let seed_opts = super::seed::Options {
            deadline: opts.deadline,
            dry_run: false,
            ..opts.seed.clone()
        };
        let seed_stores = super::seed::Stores {
            curated: stores.curated.as_ref(),
            reference: stores.reference.as_ref(),
            queue: &stores.queue,
            out: Some(stores.out.as_ref()),
        };
        Some(super::seed::seed(state, &seed_stores, &seed_opts, Some(lease)).await)
    };

    // Status and metrics.
    let now = SystemTime::now();
    let left: Vec<&ObjectInfo> = parts.iter().filter(|p| !taken.contains(&p.path)).collect();
    let oldest = left.iter().filter_map(|p| p.created).min();
    let oldest_age_s = secs_since(oldest, now);
    metrics.ocr_out_oldest_age.record(oldest_age_s, &[]);
    let mut queues = BTreeMap::new();
    for q in [PRIORITY, BULK, DEADLETTER] {
        match stores.queue.approximate_count(q).await {
            Ok(n) => {
                queues.insert(q.to_owned(), n);
            }
            Err(e) => tracing::warn!(queue = q, error = %format!("{e:#}"), "queue depth unknown"),
        }
    }
    let previous: Option<Status> = match stores.reference.get(STATUS_PATH).await? {
        Some(b) => serde_json::from_slice(&b).ok(),
        None => None,
    };
    let mut totals = previous.map(|p| p.totals).unwrap_or_default();
    totals.add(&counts);
    let status = Status {
        updated_at: Utc::now(),
        run: run.clone(),
        index_version: version,
        last_run: counts,
        totals,
        queues,
        outstanding: {
            // Seeding may have written queued rows, even when it failed:
            // read the ledger again.
            let reread = match seeded {
                Some(_) => match ledger::load(stores.curated.as_ref()).await {
                    Ok(l) => Some(l),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "ledger not read again after seeding");
                        None
                    }
                },
                None => None,
            };
            let after = reread.unwrap_or_else(|| {
                let mut after = ctx.ledger.clone();
                for o in written {
                    after.add_outcome(o);
                }
                after
            });
            after.outstanding_count() as u64
        },
        parts_left: left.len() as u64,
        oldest_age_s,
        seed: seeded.as_ref().and_then(|r| r.as_ref().ok()).cloned(),
    };
    stores
        .reference
        .put(
            STATUS_PATH,
            serde_json::to_vec_pretty(&status)?,
            "application/json",
        )
        .await?;
    let c = &status.last_run;
    tracing::info!(
        run,
        imported = c.imported,
        rejected = c.rejected.values().sum::<u64>(),
        dead_letters = c.dead_letters.values().sum::<u64>(),
        dead_letters_rejected = c.dead_letters_rejected.values().sum::<u64>(),
        cleaned = c.cleaned,
        changed = c.changed,
        parts_seen = total_parts,
        parts_left = status.parts_left,
        oldest_age_s,
        "ocr sync finished"
    );
    // A failed seeding fails the run, once the imports' status is written.
    if let Some(Err(e)) = seeded {
        return Err(e.context("seeding ocr-bulk"));
    }
    Ok(status)
}

/// One import: check `chunk`, write what passed, record, delete.
#[allow(clippy::too_many_arguments)]
async fn import(
    stores: &Stores,
    run: &str,
    version: &str,
    batches: &HashMap<String, RunBatch>,
    ctx: &CheckContext,
    loc: &mut LocScores,
    chunk: &[&ObjectInfo],
    alto_by_path: &HashMap<String, ObjectInfo>,
    alto_by_run: &HashMap<(String, String), String>,
    counts: &mut Counts,
    taken: &mut HashSet<String>,
    written_outcomes: &mut Vec<Outcome>,
    lease: &Lease,
) -> anyhow::Result<()> {
    let metrics = crate::telemetry::metrics();
    let mut sources: Vec<Consumed> = Vec::new();
    let mut accepted: Vec<(Imported, String)> = Vec::new();
    for part in chunk {
        let outcome = check_one(stores, ctx, part, alto_by_path, alto_by_run).await?;
        match outcome {
            Checked::Changed => {
                counts.changed += 1;
            }
            Checked::Rejected {
                reason,
                part: bytes,
                alto,
            } => {
                let code = reason.code();
                lease.check()?;
                quarantine_part(stores, run, part, bytes, &code, alto.as_ref()).await?;
                sources.push(Consumed {
                    path: part.path.clone(),
                    etag: part.etag.clone(),
                    outcome: "quarantined".into(),
                    reason: Some(code.clone()),
                });
                if let Some((a, _)) = &alto {
                    sources.push(Consumed {
                        path: a.path.clone(),
                        etag: a.etag.clone(),
                        outcome: "quarantined".into(),
                        reason: Some(code.clone()),
                    });
                }
                tracing::warn!(part = %part.path, reason = %code, "part quarantined");
                metrics.ocr_sync_parts.add(
                    1,
                    &[
                        KeyValue::new("outcome", "rejected"),
                        KeyValue::new("reason", code.clone()),
                    ],
                );
                *counts.rejected.entry(code).or_default() += 1;
                taken.insert(part.path.clone());
            }
            Checked::Passed {
                name,
                row,
                alto,
                alto_bytes,
            } => {
                lease.check()?;
                // (1) The ALTO, the bytes that were checked.
                let doc_id = row.key.doc_id();
                let dest = alto_path(&doc_id, &name.node, &name.stamp);
                if !stores
                    .curated
                    .put_new(&dest, alto_bytes.clone(), "application/xml")
                    .await?
                {
                    // An earlier run wrote it and stopped before its record.
                    // The node may have rewritten the source since: only the
                    // same bytes are the same import.
                    let existing = stores.curated.get(&dest).await?;
                    if existing.as_deref() != Some(alto_bytes.as_slice()) {
                        bail!("{dest} exists with other bytes than the checked source's");
                    }
                }
                let queued = ctx
                    .ledger
                    .outstanding(&doc_id)
                    .expect("check 3 found it outstanding");
                let overlay = OverlayRow {
                    key: row.key.clone(),
                    batch: row.batch.clone(),
                    ocr_source: row.ocr_source.clone(),
                    ocr_engine: row.ocr_engine.clone(),
                    loc_text: row.loc_text.clone(),
                    text: row.text.clone().unwrap_or_default(),
                    image_url: row.image_url.clone(),
                    // The server's time, not the node's.
                    ocred_at: to_utc(part.created).unwrap_or_else(Utc::now),
                };
                let languages = ctx.title_languages(&row.key.lccn);
                let ours = Scores::of(&overlay.text, &languages);
                accepted.push((
                    Imported {
                        row: overlay,
                        batch_version: ctx
                            .published
                            .get(&row.batch)
                            .copied()
                            .unwrap_or(queued.batch_version),
                        source_part: part.path.clone(),
                        node: name.node.clone(),
                        reason: row.loc_text.clone(),
                        ours,
                        loc: Scores::default(),
                    },
                    dest,
                ));
                sources.push(Consumed {
                    path: part.path.clone(),
                    etag: part.etag.clone(),
                    outcome: IMPORTED.into(),
                    reason: None,
                });
                sources.push(Consumed {
                    path: alto.path.clone(),
                    etag: alto.etag.clone(),
                    outcome: IMPORTED.into(),
                    reason: None,
                });
                metrics.ocr_sync_parts.add(
                    1,
                    &[
                        KeyValue::new("outcome", "imported"),
                        KeyValue::new("reason", ""),
                    ],
                );
                counts.imported += 1;
                taken.insert(part.path.clone());
            }
        }
    }
    if sources.is_empty() {
        return Ok(());
    }

    // LoC's scores, by batch.
    let mut by_batch: HashMap<String, HashSet<String>> = HashMap::new();
    for (i, _) in &accepted {
        by_batch
            .entry(i.row.batch.clone())
            .or_default()
            .insert(i.row.key.doc_id());
    }
    let mut loc_scores: HashMap<String, Scores> = HashMap::new();
    for (batch, ids) in &by_batch {
        if let Some(b) = batches.get(batch) {
            loc_scores.extend(
                loc.get(stores.curated.as_ref(), b, &ctx.titles, ids)
                    .await?,
            );
        }
    }
    for (i, _) in &mut accepted {
        if let Some(s) = loc_scores.remove(&i.row.key.doc_id()) {
            i.loc = s;
        }
    }

    // (2) One merged part per title.
    lease.check()?;
    let mut by_title: BTreeMap<String, Vec<Imported>> = BTreeMap::new();
    for (i, _) in &accepted {
        by_title
            .entry(i.row.key.lccn.clone())
            .or_default()
            .push(i.clone());
    }
    let mut written = Vec::new();
    for (lccn, rows) in &by_title {
        let path = pages_path(lccn, run);
        if !stores
            .curated
            .put_new(
                &path,
                pages::encode(rows)?,
                "application/vnd.apache.parquet",
            )
            .await?
        {
            bail!("{path} already exists");
        }
        written.push(path);
    }

    // (3) Outcomes and the import record.
    let now = Utc::now();
    let outcomes: Vec<Outcome> = accepted
        .iter()
        .map(|(i, _)| Outcome {
            doc_id: i.row.key.doc_id(),
            outcome: IMPORTED.into(),
            cause: None,
            kind: None,
            source: i.source_part.clone(),
            at: now,
            run: run.to_owned(),
        })
        .collect();
    let outcomes_path =
        ledger::write_outcomes(stores.curated.as_ref(), &outcomes, now, run).await?;
    written_outcomes.extend(outcomes);
    let record = ImportRecord {
        run: run.to_owned(),
        at: now,
        index_version: version.to_owned(),
        sources: sources.clone(),
        pages: written.clone(),
        outcomes: outcomes_path,
    };
    let record_path = import_path(run);
    if !stores
        .curated
        .put_new(
            &record_path,
            serde_json::to_vec_pretty(&record)?,
            "application/json",
        )
        .await?
    {
        bail!("{record_path} already exists");
    }
    tracing::info!(
        run,
        imported = accepted.len(),
        sources = sources.len(),
        titles = written.len(),
        "import written"
    );

    // (4) Delete the sources: only the bytes that were read.
    lease.check()?;
    for s in &sources {
        if !stores.out.delete_if_match(&s.path, &s.etag).await? {
            tracing::warn!(path = %s.path, "source changed or gone before its delete; the next run checks it again");
        }
    }
    Ok(())
}

/// What the checks found for one part.
enum Checked {
    /// Written again (or deleted) since the listing.
    Changed,
    Rejected {
        reason: Reject,
        /// The part's bytes, if they were read.
        part: Option<Vec<u8>>,
        /// The part's ALTO, if one is there, with its bytes if they were read.
        alto: Option<(ObjectInfo, Option<Vec<u8>>)>,
    },
    Passed {
        name: PartName,
        row: Box<usnm_overlay::NodeRow>,
        alto: ObjectInfo,
        alto_bytes: Vec<u8>,
    },
}

/// Keep a rejected part, and its ALTO if there is one, in quarantine:
/// the bytes that were checked, or for a source not read yet, its bytes if
/// within the size limit.
async fn quarantine_part(
    stores: &Stores,
    run: &str,
    part: &ObjectInfo,
    bytes: Option<Vec<u8>>,
    reason: &str,
    alto: Option<&(ObjectInfo, Option<Vec<u8>>)>,
) -> anyhow::Result<()> {
    let content = match bytes {
        Some(b) => Some(b),
        None if part.size <= checks::MAX_PART_BYTES => {
            stores
                .out
                .get_if_match(&part.path, &part.etag, checks::MAX_PART_BYTES)
                .await?
        }
        None => None,
    };
    quarantine::put(
        stores.quarantine.as_ref(),
        run,
        Item {
            reason: reason.to_owned(),
            kind: quarantine::PART,
            source: part.path.clone(),
            etag: Some(part.etag.clone()),
            size: part.size,
            created: to_utc(part.created),
            content,
        },
    )
    .await?;
    if let Some((a, bytes)) = alto {
        let content = match bytes {
            Some(b) => Some(b.clone()),
            None if a.size <= checks::MAX_ALTO_BYTES => {
                stores
                    .out
                    .get_if_match(&a.path, &a.etag, checks::MAX_ALTO_BYTES)
                    .await?
            }
            None => None,
        };
        quarantine::put(
            stores.quarantine.as_ref(),
            run,
            Item {
                reason: reason.to_owned(),
                kind: quarantine::ALTO,
                source: a.path.clone(),
                etag: Some(a.etag.clone()),
                size: a.size,
                created: to_utc(a.created),
                content,
            },
        )
        .await?;
    }
    Ok(())
}

/// Checks 1 to 6 on one part and its ALTO.
async fn check_one(
    stores: &Stores,
    ctx: &CheckContext,
    part: &ObjectInfo,
    alto_by_path: &HashMap<String, ObjectInfo>,
    alto_by_run: &HashMap<(String, String), String>,
) -> anyhow::Result<Checked> {
    let reject =
        |reason: Reject, part: Option<Vec<u8>>, alto: Option<(ObjectInfo, Option<Vec<u8>>)>| {
            Ok(Checked::Rejected { reason, part, alto })
        };
    // 1: name and size, before anything is read.
    let Some(name) = checks::parse_part_name(&part.path) else {
        return reject(Reject::Name, None, None);
    };
    let partner = alto_by_run
        .get(&(name.node.clone(), name.stamp.clone()))
        .and_then(|p| alto_by_path.get(p))
        .map(|a| (a.clone(), None));
    if part.size > checks::MAX_PART_BYTES {
        return reject(Reject::PartTooLarge, None, partner);
    }
    let Some(bytes) = stores
        .out
        .get_if_match(&part.path, &part.etag, checks::MAX_PART_BYTES)
        .await?
    else {
        return Ok(Checked::Changed);
    };
    // 2 to 5.
    let row = match checks::check_part(&name, bytes.clone(), ctx) {
        Ok(r) => r,
        Err(r) => return reject(r, Some(bytes), partner),
    };
    // 2: its ALTO, by the row's doc_id and the part's node and stamp.
    let want = checks::source_alto_path(&row.key.doc_id(), &name);
    let Some(alto) = alto_by_path.get(&want) else {
        return reject(Reject::AltoMissing, Some(bytes), partner);
    };
    if alto.size > checks::MAX_ALTO_BYTES {
        return reject(Reject::AltoTooLarge, Some(bytes), partner);
    }
    let Some(alto_bytes) = stores
        .out
        .get_if_match(&alto.path, &alto.etag, checks::MAX_ALTO_BYTES)
        .await?
    else {
        return Ok(Checked::Changed);
    };
    // 6: the ALTO itself. (7, the runs record, comes once the worker writes
    // one, 13 §13.8.3.)
    if let Err(r) = checks::check_alto(&alto_bytes) {
        return reject(r, Some(bytes), Some((alto.clone(), Some(alto_bytes))));
    }
    Ok(Checked::Passed {
        name,
        row: Box::new(row),
        alto: alto.clone(),
        alto_bytes,
    })
}
