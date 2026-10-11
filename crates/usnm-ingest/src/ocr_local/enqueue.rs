//! `usnm-ingest ocr-enqueue` and `ocr-requeue` (12 §12.5 Step 8): queue
//! chosen pages for the pilot before the seeder exists, and queue dead-
//! lettered pages again by their cause. The operator has no queue data
//! role, so both run as executions of the ingest job, as `id-usnm-ingest`.
//!
//! Each writes a ledger row for every message before it puts it, since a
//! message with no ledger row would have its part refused as not asked
//! for. If a put fails, the pages after it get a `not_sent` outcome, so
//! they aren't outstanding and can be queued again; the page whose put
//! failed may have gone out and stays outstanding (`--include-outstanding`,
//! or the seeder's timeout, Step 10). Outside a dry run both hold the
//! `ocr-sync` lock, as the seeder does, so its window count never races
//! them.

use super::ledger::{Outcome, NOT_SENT};
use super::sync::Lease;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context};
use bytes::Bytes;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use usnm_core::ids::PageKey;
use usnm_ocr_contract::message::{DeadLetter, OcrMessage, Reason};
use usnm_state::state::{RunBatch, State};
use usnm_store::queue::{Queue, BULK, PRIORITY};
use usnm_store::ObjectStore;

use super::ledger::{self, Ledger, Queued, DEAD_LETTERED};
use super::{run_id, ROOT};
use crate::ocr_damage::{self, StoredScore};

/// The most pages one `ocr-enqueue` or `ocr-requeue` queues.
pub const MAX_PAGES: usize = 10_000;

pub fn queue_for(reason: Reason) -> &'static str {
    match reason {
        Reason::User => PRIORITY,
        Reason::Bulk => BULK,
    }
}

/// What a run queued, or would have.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct QueueReport {
    pub queued: usize,
    /// Pages left out, by why.
    pub skipped: BTreeMap<String, usize>,
    pub ledger: Option<String>,
}

pub(super) fn skip(r: &mut QueueReport, why: &str, doc_id: &str) {
    tracing::info!(doc_id, why, "page not queued");
    *r.skipped.entry(why.to_owned()).or_default() += 1;
}

/// The message language for a page: its detected language in ISO 639-3,
/// or its title's first language for `und` (the seeder's rule, Step 10).
pub fn message_language(detected: Option<&str>, title_languages: &[String]) -> Option<String> {
    let iso = |marc: &str| usnm_core::lang::marc_to_iso3(marc).map(str::to_owned);
    detected
        .filter(|l| *l != usnm_core::damage::UNKNOWN)
        .and_then(iso)
        .or_else(|| title_languages.first().and_then(|l| iso(l)))
}

/// Record pages whose ledger rows are written but whose messages were
/// never put, so they aren't outstanding; returns how many.
async fn record_not_sent(
    curated: &dyn ObjectStore,
    run: &str,
    rest: &[(Queued, OcrMessage)],
) -> usize {
    let at = Utc::now();
    let rows: Vec<Outcome> = rest
        .iter()
        .map(|(q, _)| Outcome {
            doc_id: q.doc_id.clone(),
            outcome: NOT_SENT.to_owned(),
            cause: None,
            kind: None,
            source: run.to_owned(),
            at,
            run: run.to_owned(),
        })
        .collect();
    if let Err(w) = ledger::write_outcomes(curated, &rows, at, run).await {
        tracing::error!(
            error = %format!("{w:#}"),
            pages = rows.len(),
            "could not record the pages not sent; they stay outstanding until they time out"
        );
    }
    rows.len()
}

/// Send messages after writing their ledger rows, checking the lock (when
/// held) before the write and before each put.
pub(super) async fn send_all(
    curated: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    lease: Option<&Lease>,
    run: &str,
    now: DateTime<Utc>,
    batch: Vec<(Queued, OcrMessage)>,
    report: &mut QueueReport,
) -> anyhow::Result<()> {
    if let Some(l) = lease {
        l.check()?;
    }
    let rows: Vec<Queued> = batch.iter().map(|(q, _)| q.clone()).collect();
    report.ledger = ledger::write_queued(curated, &rows, now, run).await?;
    for (i, (q, m)) in batch.iter().enumerate() {
        if let Some(Err(e)) = lease.map(Lease::check) {
            let n = record_not_sent(curated, run, &batch[i..]).await;
            return Err(e.context(format!("{n} pages recorded as not sent")));
        }
        if let Err(e) = queue.send(queue_for(m.reason), &m.encode()).await {
            // The pages after this one never went out: not outstanding.
            let n = record_not_sent(curated, run, &batch[i + 1..]).await;
            return Err(e.context(format!(
                "queueing {} ({n} pages after it are recorded as not sent; this one's ledger row \
                 is written and may have gone out: --include-outstanding queues it again)",
                q.doc_id
            )));
        }
        report.queued += 1;
    }
    Ok(())
}

/// Find a page's copy in the published version: the first batch holding
/// its title whose page table (or curated text) has the page.
async fn find_page(
    curated: &dyn ObjectStore,
    batches: &[RunBatch],
    tables: &Option<HashMap<String, String>>,
    cache: &mut HashMap<String, HashMap<String, StoredScore>>,
    titles: &HashMap<String, Vec<String>>,
    key: &PageKey,
    only: Option<&str>,
) -> anyhow::Result<Option<StoredScore>> {
    let doc_id = key.doc_id();
    let wanted: HashSet<String> = [doc_id.clone()].into();
    for b in batches {
        if only.is_some_and(|o| o != b.batch) || !b.curated.lccns.contains(&key.lccn) {
            continue;
        }
        let found = match tables.as_ref().and_then(|t| t.get(&b.batch)) {
            Some(path) => {
                // Each table is read once per run, whatever the pages asked.
                if !cache.contains_key(&b.batch) {
                    let bytes = curated
                        .get(path)
                        .await?
                        .with_context(|| format!("damage table {path} not found"))?;
                    let all = ocr_damage::read_scores(Bytes::from(bytes), None)?;
                    cache.insert(b.batch.clone(), all);
                }
                &cache[&b.batch]
            }
            None => {
                let key = format!("curated:{}", b.batch);
                let scored = super::sync::score_from_curated(curated, b, titles, &wanted).await?;
                cache.entry(key.clone()).or_default().extend(scored);
                &cache[&key]
            }
        };
        if let Some(s) = found.get(&doc_id) {
            return Ok(Some(s.clone()));
        }
    }
    Ok(None)
}

/// `ocr-enqueue`'s options.
#[derive(Debug, Clone)]
pub struct EnqueueOptions {
    /// Page doc_ids (`sn83045462_1890-05-03_ed-1_seq-7`).
    pub pages: Vec<String>,
    pub reason: Reason,
    /// Recorded in the ledger as the plan.
    pub plan: String,
    /// Only look for the pages in this base batch.
    pub batch: Option<String>,
    /// Queue pages the ledger has outstanding too: to recover from a run
    /// that wrote its ledger rows but failed to put some messages.
    pub include_outstanding: bool,
    pub dry_run: bool,
}

/// Take the `ocr-sync` lock, unless it is a dry run.
async fn take(state: &State, owner: &str, dry_run: bool) -> anyhow::Result<Option<Lease>> {
    Ok(if dry_run {
        None
    } else {
        Some(Lease::take(state, owner).await?)
    })
}

/// Give the lock back, if held, and return the run's result.
async fn release<T>(
    state: &State,
    owner: &str,
    lease: Option<Lease>,
    result: anyhow::Result<T>,
) -> anyhow::Result<T> {
    let Some(lease) = lease else {
        return result;
    };
    drop(lease);
    let unlocked = state.unlock(super::LOCK, owner).await;
    let out = result?;
    unlocked?;
    Ok(out)
}

/// Queue chosen pages.
pub async fn enqueue(
    state: &State,
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    opts: &EnqueueOptions,
) -> anyhow::Result<QueueReport> {
    let owner = crate::owner_id();
    let lease = take(state, &owner, opts.dry_run).await?;
    let result = enqueue_locked(state, curated, reference, queue, opts, lease.as_ref()).await;
    release(state, &owner, lease, result).await
}

async fn enqueue_locked(
    state: &State,
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    opts: &EnqueueOptions,
    lease: Option<&Lease>,
) -> anyhow::Result<QueueReport> {
    if opts.pages.len() > MAX_PAGES {
        bail!(
            "{} pages; queue at most {MAX_PAGES} at a time",
            opts.pages.len()
        );
    }
    if !usnm_store::is_safe_segment(&opts.plan) {
        bail!("--plan must be letters, digits, `.`, `_` or `-`");
    }
    let version = state
        .current_version()
        .await?
        .context("no published version to find the pages in")?;
    let batches = ocr_damage::version_batches(reference, &version, &[]).await?;
    let titles = ocr_damage::title_languages(reference, &version).await?;
    let tables = match curated.get(&ocr_damage::versions_path(&version)).await? {
        Some(b) => {
            let list: ocr_damage::VersionList = serde_json::from_slice(&b)?;
            Some(
                list.batches
                    .into_iter()
                    .map(|b| (b.batch, b.table))
                    .collect(),
            )
        }
        None => None,
    };
    let ledger = ledger::load(curated).await?;
    let now = Utc::now();
    let run = run_id(now);
    let mut report = QueueReport::default();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut table_cache = HashMap::new();
    for doc_id in &opts.pages {
        if !seen.insert(doc_id.clone()) {
            continue;
        }
        let Ok(key) = PageKey::from_doc_id(doc_id) else {
            skip(&mut report, "bad_doc_id", doc_id);
            continue;
        };
        if !opts.include_outstanding && ledger.outstanding(doc_id).is_some() {
            skip(&mut report, "outstanding", doc_id);
            continue;
        }
        let langs = titles.get(&key.lccn).cloned().unwrap_or_default();
        if langs.iter().any(|l| l == "jpn") {
            skip(&mut report, "japanese_title", doc_id);
            continue;
        }
        let Some(found) = find_page(
            curated,
            &batches,
            &tables,
            &mut table_cache,
            &titles,
            &key,
            opts.batch.as_deref(),
        )
        .await?
        else {
            skip(&mut report, "not_published", doc_id);
            continue;
        };
        let Some(language) = message_language(found.lang.as_deref(), &langs) else {
            skip(&mut report, "no_language", doc_id);
            continue;
        };
        let msg = OcrMessage {
            v: 1,
            page: key.to_string(),
            batch: found.batch.clone(),
            language: language.clone(),
            reason: opts.reason,
            score: found.damage_rate.filter(|s| (0.0..=1.0).contains(s)),
            queued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        if let Err(e) = msg.validate() {
            skip(&mut report, e.kind(), doc_id);
            continue;
        }
        let attempt = ledger
            .attempts
            .get(&(doc_id.clone(), opts.plan.clone()))
            .copied()
            .unwrap_or(0)
            + 1;
        out.push((
            Queued {
                doc_id: doc_id.clone(),
                batch: found.batch,
                batch_version: found.batch_version,
                reason: opts.reason.as_str().to_owned(),
                language,
                plan: opts.plan.clone(),
                attempt,
                queued_at: now,
            },
            msg,
        ));
    }
    if opts.dry_run {
        report.queued = out.len();
        tracing::info!(would_queue = out.len(), skipped = ?report.skipped, "ocr-enqueue dry run");
        return Ok(report);
    }
    if !out.is_empty() {
        send_all(curated, queue, lease, &run, now, out, &mut report).await?;
    }
    tracing::info!(queued = report.queued, skipped = ?report.skipped, ledger = ?report.ledger, "ocr-enqueue finished");
    Ok(report)
}

/// `ocr-requeue`'s options.
#[derive(Debug, Clone)]
pub struct RequeueOptions {
    /// `dequeue_count`, `invalid`, `permanent` or `unroutable`.
    pub cause: String,
    pub kind: Option<String>,
    /// Only dead letters drained on or after this day (UTC).
    pub since: Option<NaiveDate>,
    pub plan: String,
    pub max_pages: usize,
    pub dry_run: bool,
}

/// Dead letters `ocr-sync` drained, newest last.
pub async fn read_dead_letters(
    curated: &dyn ObjectStore,
    since: Option<NaiveDate>,
) -> anyhow::Result<Vec<DeadLetter>> {
    let mut out = Vec::new();
    for path in curated.list(&format!("{ROOT}/deadletter")).await? {
        let day = path.split('/').nth(2).unwrap_or_default();
        if let (Some(since), Ok(d)) = (since, NaiveDate::parse_from_str(day, "%Y-%m-%d")) {
            if d < since {
                continue;
            }
        }
        let Some(body) = curated.get(&path).await? else {
            continue;
        };
        for line in String::from_utf8_lossy(&body).lines() {
            if let Ok(d) = serde_json::from_str::<DeadLetter>(line) {
                // Written from checked fields; checked again all the same.
                if d.validate().is_ok() {
                    out.push(d);
                }
            }
        }
    }
    Ok(out)
}

/// Queue dead-lettered pages again, by cause and kind.
pub async fn requeue(
    state: &State,
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    opts: &RequeueOptions,
) -> anyhow::Result<QueueReport> {
    let owner = crate::owner_id();
    let lease = take(state, &owner, opts.dry_run).await?;
    let result = requeue_locked(state, curated, reference, queue, opts, lease.as_ref()).await;
    release(state, &owner, lease, result).await
}

async fn requeue_locked(
    state: &State,
    curated: &dyn ObjectStore,
    reference: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    opts: &RequeueOptions,
    lease: Option<&Lease>,
) -> anyhow::Result<QueueReport> {
    const CAUSES: [&str; 4] = ["dequeue_count", "invalid", "permanent", "unroutable"];
    if !CAUSES.contains(&opts.cause.as_str()) {
        bail!("--cause must be one of {}", CAUSES.join(", "));
    }
    if !usnm_store::is_safe_segment(&opts.plan) {
        bail!("--plan must be letters, digits, `.`, `_` or `-`");
    }
    let max = opts.max_pages.min(MAX_PAGES);
    let version = state
        .current_version()
        .await?
        .context("no published version")?;
    let published: HashMap<String, u16> = ocr_damage::version_batches(reference, &version, &[])
        .await?
        .into_iter()
        .map(|b| (b.batch, b.curated.version))
        .collect();
    let ledger: Ledger = ledger::load(curated).await?;
    let letters = read_dead_letters(curated, opts.since).await?;
    let now = Utc::now();
    let run = run_id(now);
    let mut report = QueueReport::default();
    let mut latest: HashMap<String, OcrMessage> = HashMap::new();
    for d in letters {
        let cause = d.cause.map(|c| c.as_str());
        if cause != Some(opts.cause.as_str()) || (opts.kind.is_some() && d.kind != opts.kind) {
            continue;
        }
        let Some(m) = d.message else { continue };
        let Ok(key) = m.key() else { continue };
        latest.insert(key.doc_id(), m);
    }
    let mut out = Vec::new();
    let mut ids: Vec<String> = latest.keys().cloned().collect();
    ids.sort();
    for doc_id in ids {
        if out.len() >= max {
            skip(&mut report, "over_max_pages", &doc_id);
            continue;
        }
        let old = &latest[&doc_id];
        // Only pages whose latest result is still the dead letter.
        let last = ledger.outcomes.get(&doc_id);
        let queued_since = ledger
            .queued
            .get(&doc_id)
            .is_some_and(|q| last.is_none_or(|o| q.queued_at > o.at));
        if queued_since {
            skip(&mut report, "queued_since", &doc_id);
            continue;
        }
        if last.is_some_and(|o| o.outcome != DEAD_LETTERED) {
            skip(&mut report, "imported", &doc_id);
            continue;
        }
        // The page's latest dead letter must be the one asked for: one
        // requeued for `permanent` that then failed as `invalid` isn't
        // `permanent` any more.
        let matches = last.is_none_or(|o| {
            o.cause.as_deref() == Some(opts.cause.as_str())
                && (opts.kind.is_none() || o.kind == opts.kind)
        });
        if !matches {
            skip(&mut report, "latest_cause_differs", &doc_id);
            continue;
        }
        let Some(&batch_version) = published.get(&old.batch) else {
            skip(&mut report, "not_published", &doc_id);
            continue;
        };
        let msg = OcrMessage {
            queued_at: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ..old.clone()
        };
        if let Err(e) = msg.validate() {
            skip(&mut report, e.kind(), &doc_id);
            continue;
        }
        let attempt = ledger
            .attempts
            .get(&(doc_id.clone(), opts.plan.clone()))
            .copied()
            .unwrap_or(0)
            + 1;
        out.push((
            Queued {
                doc_id: doc_id.clone(),
                batch: msg.batch.clone(),
                batch_version,
                reason: msg.reason.as_str().to_owned(),
                language: msg.language.clone(),
                plan: opts.plan.clone(),
                attempt,
                queued_at: now,
            },
            msg,
        ));
    }
    if opts.dry_run {
        report.queued = out.len();
        tracing::info!(would_queue = out.len(), skipped = ?report.skipped, "ocr-requeue dry run");
        return Ok(report);
    }
    if !out.is_empty() {
        send_all(curated, queue, lease, &run, now, out, &mut report).await?;
    }
    tracing::info!(queued = report.queued, skipped = ?report.skipped, ledger = ?report.ledger, "ocr-requeue finished");
    Ok(report)
}
