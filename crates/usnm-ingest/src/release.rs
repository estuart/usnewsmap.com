//! The `index` stage (04 §4.4, 08 §8.4.1): build a new sealed index and the
//! reference snapshot that goes with it, then publish both by writing
//! `current.json` last.
//!
//! - **Incremental:** batches curated since the published version go into a
//!   new delta index; the version lists base + deltas + the new delta.
//! - **Full** (compaction): every curated batch goes into a new base index.
//!   Also forced when there is no published version or it already has 8
//!   deltas. Replacements (new batch versions) and catalog changes to
//!   published titles, a move to another place included, take effect only
//!   here (04 §4.7); a published place's corrected name or point rides the
//!   next delta (04 §4.6).
//!
//! A Quickwit index is merged into a few large splits and closed before it
//! is published (`crate::merges`); the version's manifest records the
//! splits of each of its indexes.
//!
//! A curated batch whose titles aren't all in the catalog yet waits for a
//! later release (it counts as new until it's published): `titles-sync` is
//! paced and stops on LoC's rate limit, and a backlog of titles shouldn't
//! hold up the batches that are ready.
//!
//! The Cosmos `ops/quickwit-writer` lock makes this the only writer. Indexes
//! are never rewritten: a failed run leaves an unpublished index for the
//! janitor, and the retry builds a new one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::field::Empty;
use tracing::Instrument;
use usnm_core::text::{Analyzer, TextStatus};
use usnm_core::text_layout::TextLayout;
use usnm_core::time::{date_from_day, day_number, ym_number};
use usnm_store::ObjectStore;

use crate::activity::Reporter;
use crate::american_stories;
use crate::build_info;
use crate::catalog::{Catalog, Place, Title};
use crate::curated::{read_part, CuratedRow};
use crate::decade_order::DecadeOrder;
use crate::dedup::{self, Plan};
use crate::merges::{self, IndexLayout};
use crate::ocr_ja;
use crate::ocr_local;
use crate::progress::{self, Progress};
use crate::sink::{Decades, IndexSink, MainLayout};
use crate::source::hex;
use crate::state::{
    Curated, IndexRun, JaLatin, JaLatinPage, LanguageBaselines, LanguageSet, RunBatch, RunStatus,
    State, Step, DUPLICATES_FILE, JA_LATIN_FILE, LANGUAGE_BASELINES_FILE, RUN_BATCHES_FILE,
    TITLE_PAGES_FILE,
};

pub use crate::state::{MAX_DELTAS, WRITER_LOCK};

pub struct Release {
    pub state: State,
    pub curated: Arc<dyn ObjectStore>,
    pub reference: Arc<dyn ObjectStore>,
    pub owner: String,
    pub full: bool,
    /// Mark the version as synthetic demo data (the fixture batches); the
    /// site then says so and doesn't link to loc.gov.
    pub synthetic: bool,
    /// Names versions (`pages-v{date}-{n}`). `published_at` is stamped when
    /// the version pointer is written, which can be many hours later.
    pub now: DateTime<Utc>,
    /// Why titles-sync stopped before trying every title, if it did. A delta
    /// goes ahead without the batches whose titles are missing; a full base
    /// (asked for, or forced) doesn't, since it would replace the published
    /// one without them.
    pub titles_left: Option<String>,
    /// Index American Stories' text with LoC's (#218, 04 §4.9): `text_as` on
    /// the pages it covers, and the pages only it has text for. The version
    /// then says so in `current.json` (`american_stories`), and a published
    /// version built without it is rebuilt in full first. Off: nothing is
    /// read, and the documents and `current.json` are as before.
    pub american_stories: bool,
    /// Give the pages LoC ships without text (`loc_text = missing`) a
    /// main-index document with the Latin-script text our Japanese OCR read
    /// on them (#203, 04 §4.8), so English searches reach their English ads
    /// and sections. The pages are in the baselines already (#139). The
    /// release's new main index (a delta or a base) takes the pages no main
    /// index of the version holds yet; a delta keeps the earlier ones. Off:
    /// no new ones are written, and a full release drops them.
    pub ja_latin: bool,
    /// Lay a full base out by decade (05 §5.5.5, #123): every split holds
    /// one decade's pages, sent a decade at a time (`crate::decade_order`),
    /// and `current.json` says so (`decades`), so date-limited searches skip
    /// the other decades' splits. Only a full base takes it: a delta follows
    /// the published version (its pages get the `decade` field and its
    /// splits the decade tags when that version has them), whatever this
    /// says. Off: the documents, the index config and `current.json` are as
    /// before.
    pub partition_decade: bool,
    /// Search a full base's texts in one field (05 §5.5.7, #283): `text_all`
    /// and `text_all_cg` hold LoC's and American Stories' words with a gap
    /// between them, `text` and `text_as` are stored only, and
    /// `current.json` says so (`text_layout`). Only a full base takes it: a
    /// delta follows the published version, whatever this says. Off: the
    /// documents, the index config and `current.json` are as before.
    pub single_text_field: bool,
    /// Weigh our own OCR from `ocr-local` against LoC's text (12 §12.5 Step
    /// 9): on the pages of the batches this release indexes, ours replaces
    /// LoC's where rule v1 prefers it (`crate::ocr_local::rule`), and the
    /// version records it in `ocr_local.json` and `ocr_local_pages.bin`.
    /// Pages of published batches take it at the next full rebuild. Off:
    /// nothing is read and the documents are as before; a delta still
    /// carries the published version's `ocr_local_pages.bin` forward.
    pub ocr_local: bool,
}

/// What the release reads of `ocr-local` (`Release::ocr_local`).
struct LocalRun {
    listing: ocr_local::replace::Listing,
    revoked: Vec<ocr_local::replace::Revocation>,
    /// Pages the Japanese overlay has, or whose document is its Latin text:
    /// that overlay wins.
    ja: HashSet<String>,
    /// Pages of published batches ours would replace (stored scores).
    estimate: Option<u64>,
}

/// What a version records of our OCR from `ocr-local`.
#[derive(Debug, Default)]
struct LocalFiles {
    /// `ocr_local_pages.bin`, when the version holds such pages.
    pages: Option<Vec<u8>>,
    /// `ocr_local.json`, with the setting.
    record: Option<Value>,
    /// The manifest's `built_from.ocr_local`: the parts summary.
    summary: Option<Value>,
}

/// The pages this release's main index gave our text: (doc_id, engine,
/// the batch of the copy that holds it).
type LocalReplaced = Vec<(String, usnm_state::ocr_local::Engine, String)>;

/// A full release refused to start because titles-sync left titles unfetched
/// (`Release::titles_left`): nothing was published, and the next execution
/// continues the sync.
#[derive(Debug)]
pub struct TitlesLeft(pub String);

impl std::fmt::Display for TitlesLeft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TitlesLeft {}

/// The version pointer was written, so the new version is live, but
/// recording the publish in the pipeline state failed afterwards.
#[derive(Debug)]
pub struct PublishedUnrecorded(pub String);

impl std::fmt::Display for PublishedUnrecorded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PublishedUnrecorded {}

/// The batches whose titles are all in `catalog`. The others are logged and
/// left for a later release.
fn catalogued(batches: Vec<RunBatch>, catalog: &Catalog) -> Vec<RunBatch> {
    let is_ready = |b: &RunBatch| b.curated.lccns.iter().all(|l| catalog.title(l).is_some());
    let mut ready = Vec::new();
    let mut waiting = Vec::new();
    for b in batches {
        if is_ready(&b) {
            ready.push(b);
        } else {
            waiting.push(b);
        }
    }
    if !waiting.is_empty() {
        let titles: BTreeSet<&str> = waiting
            .iter()
            .flat_map(|b| &b.curated.lccns)
            .filter(|lccn| catalog.title(lccn).is_none())
            .map(String::as_str)
            .collect();
        tracing::warn!(
            batches = waiting.len(),
            titles = titles.len(),
            first = ?waiting.iter().take(5).map(|b| b.batch.as_str()).collect::<Vec<_>>(),
            "batches wait for titles missing from the catalog (run titles-sync)"
        );
    }
    ready
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub index_version: String,
    pub indexes: Vec<String>,
    pub full: bool,
    pub docs: u64,
    pub pages: u64,
}

/// How a release writes its main-index documents. Every main index of a
/// version is written the same way: a delta as the indexes it adds to were.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocFormat {
    /// The page's decade partition too (05 §5.5.5).
    pub decade: bool,
    /// The analyzer that folds the common-word pairs (05 §5.5.3, #168), and
    /// the words of `text_all`.
    pub analyzer: Analyzer,
    /// Which fields the texts are searched in (05 §5.5.7).
    pub text: TextLayout,
}

/// One engine document for a curated page (05 §5.5), with American
/// Stories' text when it has some (05 §5.5.4). A page LoC has no usable
/// text for is a document only with American Stories' text; its `text` is
/// then empty.
pub fn page_doc(
    row: &CuratedRow,
    title: &Title,
    place: &Place,
    text_as: Option<&str>,
    format: DocFormat,
) -> Value {
    main_doc(
        &row.key,
        &row.batch,
        row.text.as_deref().unwrap_or_default(),
        title,
        place,
        text_as,
        format,
    )
}

/// A main-index document for page `k` of `batch` with `text` as its text:
/// [`page_doc`]'s, and the Latin-script text of our Japanese OCR on a page
/// LoC ships without text (`ocr_ja::latin_doc`).
pub fn main_doc(
    k: &usnm_core::ids::PageKey,
    batch: &str,
    text: &str,
    title: &Title,
    place: &Place,
    text_as: Option<&str>,
    format: DocFormat,
) -> Value {
    let mut doc = json!({
        "doc_id": k.doc_id(),
        "day": day_number(k.date),
        "ym": ym_number(k.date),
        "year": k.date.year(),
        "place_id": place.id,
        "place_shard": place.ordinal % 8,
        "lccn": k.lccn,
        "state": title.state,
        "language": title.languages,
        "front_page": k.seq == 1,
        "edition": k.edition,
        "seq": k.seq,
        // Hits order within a day: title, then edition, then page.
        "sort_key": (u64::from(title.ordinal) << 32) | (u64::from(k.edition) << 16) | u64::from(k.seq),
        "date": k.date.to_string(),
        "batch": batch,
        "text": text,
    });
    match format.text {
        TextLayout::Separate => {
            doc["text_cg"] = usnm_core::common_grams::index_text(text, format.analyzer).into();
            if let Some(t) = text_as {
                doc["text_as_cg"] = usnm_core::common_grams::index_text(t, format.analyzer).into();
            }
        }
        // One searched field for both texts; `text` and `text_as` are
        // stored for snippets (05 §5.5.7).
        TextLayout::Single => {
            let (words, pairs) =
                usnm_core::text_layout::index_fields(text, text_as, format.analyzer);
            doc[usnm_core::text_layout::FIELD] = words.into();
            doc[usnm_core::text_layout::PAIRS_FIELD] = pairs.into();
        }
    }
    if let Some(t) = text_as {
        doc["text_as"] = t.into();
    }
    if format.decade {
        doc["decade"] = usnm_core::decade::of_date(k.date).into();
    }
    doc
}

/// The pages of our OCR the snapshot accounts for (#139, #203).
struct Latin<'a> {
    /// Of the overlay's pages that curation never had, and of the pages the
    /// version's main indexes hold with our Latin text, the ones some batch
    /// of the version has in its curated parts.
    curated_keys: &'a BTreeMap<String, CuratedCopies>,
    /// Pages this release's main index added with our Latin text.
    added: u64,
    /// Pages an earlier index holds with our Latin text, now hidden.
    hidden: u64,
    /// Every page the version's main indexes hold with our Latin text.
    version: &'a JaLatin,
}

/// A page's copies in the curated parts of the version's batches.
#[derive(Debug, Default)]
struct CuratedCopies {
    /// Some copy has LoC's text (`ok`).
    with_text: bool,
    /// The batches with a copy without it (`empty`, `short`).
    without_text: BTreeSet<String>,
}

/// What the published `current.json` says about the version's indexes.
struct PublishedPointer {
    backend: String,
    /// `common_grams`: the pairs every index of the version has.
    grams: Option<u32>,
    /// `american_stories`: every index of the version has American Stories' text.
    american_stories: Option<u32>,
    /// `decades`: every index of the version has the `decade` field (05 §5.5.5).
    decades: Option<u32>,
    /// `text_layout`: the text fields of every main index (05 §5.5.7).
    text_layout: Option<u32>,
}

/// How long the writer lock lasts without renewal, and how often it's renewed.
const LEASE_TTL: Duration = Duration::minutes(30);
const LEASE_RENEW: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// A held writer lock, renewed in the background until it is released.
/// If a renewal fails, the release stops at its next checkpoint and never
/// publishes (08 §8.4.1).
pub struct WriterLease {
    lost: Arc<AtomicBool>,
    renewer: tokio::task::JoinHandle<()>,
}

impl WriterLease {
    /// Fails once a renewal has failed: the holder may no longer hold the
    /// lock, so it must stop before its next write.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.lost.load(Ordering::SeqCst) {
            bail!("the writer lock could not be renewed; stopping without publishing");
        }
        Ok(())
    }

    /// Resolves once a renewal has failed. Long waits (the merges, up to
    /// 4 hours by default) race against it, so a writer that may no longer hold the
    /// lock stops within a second instead of at its next checkpoint, long
    /// before the lock expires and another writer could take it.
    pub(crate) async fn lost(&self) {
        while !self.lost.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    /// A lease with no lock behind it, lost once `lost` is set.
    #[cfg(test)]
    pub(crate) fn for_test(lost: Arc<AtomicBool>) -> Self {
        Self {
            lost,
            renewer: tokio::spawn(async {}),
        }
    }
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        self.renewer.abort();
    }
}

/// Take the writer lock as `owner` and keep renewing it until
/// [`unlock_writer`]. Whatever changes the indexes Quickwit holds takes it
/// first: a release, and `prune` (`crate::prune`), so neither runs beside
/// the other.
pub async fn lock_writer(state: &State, owner: &str) -> anyhow::Result<WriterLease> {
    state.lock(WRITER_LOCK, owner, LEASE_TTL).await?;
    let lost = Arc::new(AtomicBool::new(false));
    let (state, owner, flag) = (state.clone(), owner.to_owned(), lost.clone());
    let renewer = tokio::spawn(async move {
        loop {
            tokio::time::sleep(LEASE_RENEW).await;
            if let Err(e) = state.lock(WRITER_LOCK, &owner, LEASE_TTL).await {
                tracing::error!(error = %format!("{e:#}"), "writer lock renewal failed");
                flag.store(true, Ordering::SeqCst);
                break;
            }
        }
    });
    Ok(WriterLease { lost, renewer })
}

/// Stop renewing `lease` and release the writer lock `owner` holds.
pub async fn unlock_writer(state: &State, owner: &str, lease: WriterLease) -> anyhow::Result<()> {
    drop(lease);
    state.unlock(WRITER_LOCK, owner).await
}

impl Release {
    /// Take the writer lock and keep renewing it. Callers that run a
    /// Quickwit writer node take it before the node starts and release it
    /// after the node stops, so no two writers ever overlap.
    pub async fn lock(&self) -> anyhow::Result<WriterLease> {
        lock_writer(&self.state, &self.owner).await
    }

    pub async fn unlock(&self, lease: WriterLease) -> anyhow::Result<()> {
        unlock_writer(&self.state, &self.owner, lease).await
    }

    /// Build and publish a new version, or `None` if there is nothing new.
    pub async fn run(&self, sink: &mut dyn IndexSink) -> anyhow::Result<Option<Published>> {
        let lease = self.lock().await?;
        let result = self.run_held(sink, &lease).await;
        let unlocked = self.unlock(lease).await;
        let published = result?;
        unlocked?;
        Ok(published)
    }

    /// Confirm this owner still holds the lock (and extend it).
    async fn confirm(&self, lease: &WriterLease) -> anyhow::Result<()> {
        lease.check()?;
        self.state.lock(WRITER_LOCK, &self.owner, LEASE_TTL).await
    }

    /// [`Release::run`] for a caller that already holds the writer lock.
    /// Runs in a `release` span (a request in Application Insights).
    pub async fn run_held(
        &self,
        sink: &mut dyn IndexSink,
        lease: &WriterLease,
    ) -> anyhow::Result<Option<Published>> {
        self.run_reporting(sink, lease, &Reporter::off()).await
    }

    /// [`Release::run_held`], recording its steps (indexing, merging,
    /// publishing) with `report` for the status page.
    pub async fn run_reporting(
        &self,
        sink: &mut dyn IndexSink,
        lease: &WriterLease,
        report: &Reporter,
    ) -> anyhow::Result<Option<Published>> {
        let span = tracing::info_span!(
            "release",
            otel.kind = "consumer",
            version = Empty,
            full = Empty,
            batches = Empty,
            docs = Empty,
            otel.status_code = Empty,
            otel.status_description = Empty,
        );
        let result = self
            .run_in_span(sink, lease, &span, report)
            .instrument(span.clone())
            .await;
        if let Err(e) = &result {
            span.record("otel.status_code", "ERROR");
            span.record("otel.status_description", format!("{e:#}").as_str());
        }
        result
    }

    async fn run_in_span(
        &self,
        sink: &mut dyn IndexSink,
        lease: &WriterLease,
        span: &tracing::Span,
        report: &Reporter,
    ) -> anyhow::Result<Option<Published>> {
        self.confirm(lease).await?;
        let current = Catalog::load(self.reference.as_ref()).await?;
        let (previous, pointer) = match self.published_run().await? {
            Some((run, pointer)) => (Some(run), Some(pointer)),
            None => (None, None),
        };
        let previous_backend = pointer.as_ref().map(|p| p.backend.as_str());
        let previous_grams = pointer.as_ref().and_then(|p| p.grams);
        let previous_as = pointer.as_ref().and_then(|p| p.american_stories);
        let previous_decades = pointer.as_ref().and_then(|p| p.decades);
        let previous_layout = pointer.as_ref().map(|p| p.text_layout);
        // Every batch's last committed curation, whatever its current status.
        let curated: BTreeMap<String, Curated> = self
            .state
            .batches(&[])
            .await?
            .into_iter()
            .filter_map(|(b, _)| b.curated.map(|c| (b.batch, c)))
            .collect();

        // A delta only makes sense on top of indexes in the same engine.
        let switching = previous_backend.is_some_and(|b| b != sink.backend());
        if switching {
            tracing::info!(
                to = sink.backend(),
                "search backend changed; building a full base"
            );
        }
        // Every index of a version has its pairs, and folds its words, with
        // one analyzer (05 §5.5.3, #168): a delta is built with the
        // published version's, and only a full base moves to the latest.
        // A version without pairs, or with ones this code doesn't know,
        // can't take a delta: rebuild every index.
        let published_analyzer = previous_grams.and_then(usnm_core::common_grams::analyzer);
        let regramming = previous.is_some() && published_analyzer.is_none();
        if regramming {
            tracing::info!(
                from = ?previous_grams,
                to = usnm_core::common_grams::VERSION,
                "the published version has no common-word pairs this release can add to; \
                 building a full base"
            );
        }
        // The same for American Stories' text (05 §5.5.4): a version says it
        // has the text only when every index in it does, so the first
        // release with it, or with another version of it, rebuilds them all.
        // Without the setting the text is neither written nor searched,
        // whatever the indexes hold.
        let adding_stories = self.american_stories
            && previous.is_some()
            && previous_as != Some(usnm_core::american_stories::VERSION);
        if adding_stories {
            tracing::info!(
                from = ?previous_as,
                to = usnm_core::american_stories::VERSION,
                "American Stories' text is new to the published version; building a full base"
            );
        }
        // The text fields (05 §5.5.7): a delta is laid out as the published
        // version is, so a layout this release doesn't know can't take one.
        let published_layout = previous_layout.map(TextLayout::from_version);
        let relayouting = matches!(published_layout, Some(None));
        if relayouting {
            tracing::info!(
                from = ?previous_layout.flatten(),
                "the published version's text fields are laid out in a way this release \
                 doesn't know; building a full base"
            );
        }
        // With one field for both texts, American Stories' text can't be
        // left out of the indexes a delta keeps, as it is from a field of
        // its own: a release without it rebuilds them all.
        let dropping_stories = !self.american_stories
            && previous_as.is_some()
            && published_layout == Some(Some(TextLayout::Single));
        if dropping_stories {
            tracing::info!(
                "the published version searches American Stories' text in one field with \
                 LoC's, and this release leaves it out; building a full base"
            );
        }
        let full = self.full
            || switching
            || regramming
            || adding_stories
            || relayouting
            || dropping_stories
            || previous
                .as_ref()
                .is_none_or(|p| p.indexes.len() > MAX_DELTAS);
        let decades = self.decade_layout(full, previous_decades);
        let text_layout = self.text_layout(full, published_layout.flatten());
        let analyzer = match published_analyzer {
            Some(a) if !full => a,
            _ => Analyzer::LATEST,
        };
        if analyzer != Analyzer::LATEST {
            tracing::info!(
                common_grams = usnm_core::common_grams::version(analyzer),
                latest = usnm_core::common_grams::VERSION,
                "a delta folds its words as the published version's indexes do; a full \
                 rebuild (USNM_INGEST_FULL) moves the version to the latest analyzer"
            );
        }
        if let Some(why) = &self.titles_left {
            if full {
                return Err(TitlesLeft(format!(
                    "{why}. A full release now would leave out every batch whose title is \
                     missing, so nothing was released: start the job again"
                ))
                .into());
            }
            tracing::warn!("{why}; releasing with the catalog as it is");
        }
        // An incremental release with no new batches still publishes when our
        // Japanese OCR has changed (04 §4.8): same main indexes, a new
        // Japanese index and snapshot. With `--ja-latin` and Latin text of
        // ours that no main index holds yet, a delta of just those pages.
        let mut nothing_new = false;
        let (scope, version_batches, catalog, mut indexes) = if full {
            let all: Vec<RunBatch> = curated
                .into_iter()
                .map(|(batch, curated)| RunBatch { batch, curated })
                .collect();
            let all = catalogued(all, &current);
            (all.clone(), all, current, Vec::new())
        } else {
            let prev = previous
                .as_ref()
                .expect("incremental has a previous version");
            let prev_batches = self.run_batches(prev).await?;
            let published: HashMap<&str, &Curated> = prev_batches
                .iter()
                .map(|b| (b.batch.as_str(), &b.curated))
                .collect();
            let mut new = Vec::new();
            for (batch, c) in &curated {
                match published.get(batch.as_str()) {
                    None => new.push(RunBatch {
                        batch: batch.clone(),
                        curated: c.clone(),
                    }),
                    Some(p) if *p != c => {
                        tracing::info!(batch, "re-curated batch waits for the next full release")
                    }
                    Some(_) => {}
                }
            }
            let published_catalog = self.load_snapshot_catalog(&prev.index_version).await?;
            // Published titles the catalog now puts in another place (a
            // merge, an alias, 04 §4.6) stay where they were published: a
            // full release on the E4 profile moves them, since one on the
            // Consumption profile runs out of memory (#172).
            let moved = Catalog::regrouped(&published_catalog, &current);
            if !moved.is_empty() {
                let n = moved.len();
                tracing::warn!(
                    titles = n,
                    first = ?moved.iter().take(10).collect::<Vec<_>>(),
                    "{n} published titles' place changes wait for the next full release \
                     (USNM_INGEST_FULL, run on the E4 profile: docs/operations.md, Full rebuild)"
                );
            }
            let catalog = Catalog::carried_forward(&published_catalog, &current)?;
            let new = catalogued(new, &catalog);
            nothing_new = new.is_empty();
            let mut all = prev_batches;
            all.extend(new.iter().cloned());
            (new, all, catalog, prev.indexes.clone())
        };
        if scope.is_empty() && !nothing_new {
            tracing::info!("no curated batches; nothing to release");
            return Ok(None);
        }
        // Our own OCR of the Japanese pages (04 §4.8): its own index, rebuilt
        // at every release from the pages of the version's batches.
        let batch_names: BTreeSet<&str> =
            version_batches.iter().map(|b| b.batch.as_str()).collect();
        let overlay = ocr_ja::load(self.curated.as_ref(), &batch_names, &catalog).await?;
        if !overlay.parts.is_empty() {
            tracing::info!(
                parts = overlay.parts.len(),
                pages = overlay.pages.len(),
                skipped = overlay.skipped,
                "Japanese OCR overlay"
            );
        }
        // Of the pages of our OCR that curation never had, and of the pages
        // a main index of the version holds with our OCR's Latin text
        // (#203), the ones some batch of the version has in its curated
        // parts: those are counted, and indexed, from LoC's copy.
        let latin_before = match &previous {
            Some(prev) if !full => self.previous_latin(&prev.index_version).await?,
            _ => JaLatin::default(),
        };
        // With no newly curated batches, our Latin text of pages LoC ships
        // without text that no main index of the version holds yet still
        // gets a main index of its own (a delta of just those pages), so it
        // doesn't wait for LoC's next batch (#203). Pages with none of it
        // can skip reading the curated parts below, and so can a run whose
        // published version already weighed every page of this overlay by
        // this rule: those its main indexes don't hold were left out for a
        // copy of LoC's, and with no new batch they still are.
        let latin_pending = nothing_new
            && self.ja_latin
            && overlay.pages.iter().any(|p| {
                p.missing_from_curation()
                    && !latin_before.pages.contains_key(&p.key.doc_id())
                    && p.latin().is_some()
            })
            && !self.latin_settled(previous.as_ref(), &overlay).await?;
        // Decided before the duplicate-page plan, which reads every batch's
        // counts: a run with nothing to release returns without it.
        if nothing_new
            && !latin_pending
            && !self.overlay_releasable(previous.as_ref(), &overlay).await?
        {
            return Ok(None);
        }
        let curated_keys = {
            let mut wanted: BTreeSet<String> = overlay
                .pages
                .iter()
                .filter(|p| p.missing_from_curation())
                .map(|p| p.key.doc_id())
                .collect();
            wanted.extend(latin_before.pages.keys().cloned());
            self.curated_keys(&version_batches, &wanted).await?
        };
        // The Latin text this release's main index adds: the pages no main
        // index of the version holds yet. A copy with LoC's text supersedes
        // ours; one without doesn't, and gets no document (`latin_owned`),
        // unless an index the version keeps may hold it as a document
        // already: with American Stories' text, a copy without LoC's is one
        // when that text covers it.
        let scope_names: BTreeSet<&str> = scope.iter().map(|b| b.batch.as_str()).collect();
        let latin_new: Vec<(&ocr_ja::JaPage, String)> = if self.ja_latin {
            overlay
                .pages
                .iter()
                .filter(|p| {
                    let id = p.key.doc_id();
                    let copies = curated_keys.get(&id);
                    p.missing_from_curation()
                        && !copies.is_some_and(|c| c.with_text)
                        && !(self.american_stories
                            && copies.is_some_and(|c| {
                                c.without_text
                                    .iter()
                                    .any(|b| !scope_names.contains(b.as_str()))
                            }))
                        && !latin_before.pages.contains_key(&id)
                })
                .filter_map(|p| p.latin().map(|t| (p, t)))
                .collect()
        } else {
            Vec::new()
        };
        // An incremental release with no new batches and no such pages
        // publishes only a new Japanese index and snapshot, on the same
        // main indexes.
        let overlay_only = nothing_new && latin_new.is_empty();
        // Every page was weighed and none added (each left out for a copy
        // of LoC's): an overlay-only release, once, so its `ocr_ja.json`
        // records `latin.rule` and later runs skip the curated parts. The
        // run records the Japanese index as the index it wrote, so there
        // must be one.
        if overlay_only && latin_pending {
            if !overlay.pages.iter().any(ocr_ja::JaPage::indexable) {
                tracing::info!(
                    "no newly curated batches and no Latin text of ours to add, and the \
                     Japanese OCR has no page to index; nothing to release"
                );
                return Ok(None);
            }
            tracing::info!(
                "no newly curated batches and no Latin text of ours to add; releasing the \
                 Japanese OCR on the same indexes, which records that every page was weighed"
            );
        }
        if nothing_new && !overlay_only {
            tracing::info!(
                pages = latin_new.len(),
                "no newly curated batches; releasing our OCR's Latin text of pages LoC ships \
                 without text in a delta of its own. Our text in place of LoC's waits for a \
                 release that indexes the page's batch"
            );
        }
        // Pages curation had that our OCR read Latin text on: a batch this
        // release indexes takes ours in place of LoC's text where ours reads
        // more words (#203). A release with no new batches indexes none.
        let latin_curated: HashMap<String, (&ocr_ja::JaPage, String)> =
            if self.ja_latin && !nothing_new {
                overlay
                    .pages
                    .iter()
                    .filter(|p| !p.missing_from_curation())
                    .filter_map(|p| p.latin().map(|t| (p.key.doc_id(), (p, t))))
                    .collect()
            } else {
                HashMap::new()
            };

        // Pages that ship in more than one batch: which copy the version
        // keeps. The batches outside the scope are in indexes it keeps.
        let new: BTreeSet<&str> = scope.iter().map(|b| b.batch.as_str()).collect();
        let indexed: BTreeSet<&str> = version_batches
            .iter()
            .map(|b| b.batch.as_str())
            .filter(|b| !new.contains(b))
            .collect();
        let previously_hidden = match &previous {
            Some(prev) if !full => self.previously_hidden(&prev.index_version).await?,
            _ => Some(BTreeSet::new()),
        };
        let mut dedup = dedup::plan(
            self.curated.as_ref(),
            &version_batches,
            &indexed,
            &previously_hidden,
            self.american_stories,
        )
        .await?;
        // A page an earlier index holds with our Latin text (a delta can't
        // take it out) that a copy with LoC's text now wins over: hidden, so
        // the page counts once, from LoC's copy. That is a page curation
        // never had that some batch of the version now has with text, or a
        // copy given our text in place of LoC's that another batch's copy
        // with text wins over. A copy without LoC's text doesn't take over:
        // ours stays searchable, and the release writes no document for the
        // other (`latin_kept`), so the page is still one document.
        let (latin_hidden, latin_kept): (Vec<_>, Vec<_>) =
            latin_before.pages.iter().partition(|(id, p)| {
                if p.loc_text == "missing" {
                    curated_keys.get(*id).is_some_and(|c| c.with_text)
                } else {
                    usnm_core::ids::PageKey::from_doc_id(id).is_ok_and(|key| {
                        !dedup.keeps(&key, &p.batch) && dedup.kept_has_text(&key) == Some(true)
                    })
                }
            });
        let latin_hidden: Vec<dedup::Hidden> = latin_hidden
            .into_iter()
            .map(|(id, p)| dedup::Hidden {
                doc_id: id.clone(),
                batch: p.batch.clone(),
            })
            .collect();
        // Our copies that stay searchable, by page, with their batch.
        let latin_kept: HashMap<String, String> = latin_kept
            .into_iter()
            .map(|(id, p)| (id.clone(), p.batch.clone()))
            .collect();
        // The pages whose document is ours, in an index the version keeps or
        // in this release's: a copy of one without LoC's text gets none.
        let latin_owned: HashMap<String, String> = latin_kept
            .iter()
            .map(|(id, b)| (id.clone(), b.clone()))
            .chain(
                latin_new
                    .iter()
                    .map(|(p, _)| (p.key.doc_id(), p.batch.clone())),
            )
            .collect();
        // With American Stories' text on, the plan hides a losing copy
        // without LoC's text too: not ours, which stays the page's document.
        dedup
            .hidden
            .retain(|h| latin_kept.get(&h.doc_id) != Some(&h.batch));
        if !latin_hidden.is_empty() {
            tracing::warn!(
                pages = latin_hidden.len(),
                "pages indexed with our OCR's Latin text now have LoC's; hiding our copies"
            );
            dedup.hidden.extend(latin_hidden.iter().cloned());
            dedup.hidden.sort();
            dedup.hidden.dedup();
        }
        // What the version's main indexes hold with our Latin text: the
        // pages kept from the indexes it keeps, this release's pages that
        // curation never had, and (after the build) the pages it gave our
        // text in place of LoC's.
        let mut latin_after = latin_before;
        for (p, _) in &latin_new {
            latin_after.pages.insert(
                p.key.doc_id(),
                JaLatinPage {
                    batch: p.batch.clone(),
                    loc_text: p.loc_text.clone(),
                    ocr_source: p.ocr_source.clone(),
                    ocr_engine: p.ocr_engine.clone(),
                },
            );
        }
        if dedup.duplicate_pages > 0 {
            tracing::warn!(
                pages = dedup.duplicate_pages,
                title_days = dedup.shared_days,
                hidden = dedup.hidden.len(),
                pairs = ?dedup.pairs.iter().take(10).collect::<Vec<_>>(),
                "pages ship in more than one batch; keeping one copy of each"
            );
        }

        // Our own OCR (12 §12.5 Step 9): listed once, loaded batch by batch
        // as the build reaches each.
        let local = if self.ocr_local && !overlay_only {
            let listing = ocr_local::replace::Listing::list(self.curated.as_ref()).await?;
            let revoked = ocr_local::replace::revocations(self.curated.as_ref()).await?;
            // Pages of published batches only take ours at a full rebuild:
            // how many would, by the scores `ocr-sync` stored.
            let estimate = if indexed.is_empty() || listing.parts() == 0 {
                None
            } else {
                let n = listing
                    .estimate(
                        self.curated.as_ref(),
                        &indexed,
                        |lccn| {
                            catalog
                                .title(lccn)
                                .map(|t| t.languages.clone())
                                .unwrap_or_default()
                        },
                        &revoked,
                    )
                    .await?;
                tracing::info!(
                    pages = n,
                    "pages of ocr-local that would replace LoC's text in published batches, by \
                     the scores ocr-sync stored: a full rebuild (USNM_INGEST_FULL) applies them"
                );
                Some(n)
            };
            tracing::info!(
                parts = listing.parts(),
                titles = listing.titles(),
                revocations = revoked.len(),
                "our OCR in ocr-local"
            );
            let ja: HashSet<String> = overlay
                .pages
                .iter()
                .map(|p| p.key.doc_id())
                .chain(latin_owned.keys().cloned())
                .collect();
            Some(LocalRun {
                listing,
                revoked,
                ja,
                estimate,
            })
        } else {
            None
        };

        // American Stories' text, listed once: the parts of the years the
        // writer has finished. With the setting on and nothing written yet,
        // nothing is built: a version without the text mustn't say it has it.
        let stories = if self.american_stories && !overlay_only {
            american_stories::check_written(self.curated.as_ref()).await?;
            let source = american_stories::Source::list(self.curated.as_ref()).await?;
            tracing::info!(
                years = source.years.len(),
                first = source.years.first(),
                last = source.years.last(),
                parts = source.parts(),
                unfinished_parts = source.unfinished_parts,
                "American Stories' text"
            );
            Some(source)
        } else {
            None
        };

        let (version, index_id) = self.next_names(full).await?;
        // An overlay-only release adds no main index: the run's new index is the Japanese one.
        let new_index = if overlay_only {
            ocr_ja::index_id(&version)
        } else {
            indexes.push(index_id.clone());
            index_id.clone()
        };
        // What this run builds, for the version's build record (#161): the
        // same predicate as `build_ja_index` for the Japanese index.
        let mut built = build_info::Built {
            full,
            main_index: !overlay_only,
            ja_index: overlay.pages.iter().any(|p| p.indexable()),
            engine: sink.engine().await,
            writer: crate::sink::WriterTuning::from_env()?,
            american_stories: self.american_stories,
            ja_latin: self.ja_latin,
            ocr_local: local.is_some(),
            decades,
            text_layout,
            analyzer,
        };
        let mut run = IndexRun {
            id: version.clone(),
            index_version: version.clone(),
            full,
            indexes: indexes.clone(),
            new_index: new_index.clone(),
            batch_count: Some(version_batches.len() as u64),
            batch_list: None,
            batches: None,
            status: RunStatus::Building,
            docs: 0,
            pages: version_batches.iter().map(|b| b.curated.pages).sum::<u64>()
                - dedup.duplicate_pages,
            duplicate_pages: dedup.duplicate_pages,
            started_at: Utc::now(),
            published_at: None,
            previous_version: previous.as_ref().map(|p| p.index_version.clone()),
            last_error: None,
            failed_at: None,
            build: Some(build_info::summary(&built)),
        };
        if !self.state.create_run(&run).await? {
            bail!("index run `{version}` already exists");
        }
        let (_, mut etag) = self.state.run(&version).await?.context("run vanished")?;
        span.record("version", version.as_str());
        span.record("full", full);
        span.record("batches", scope.len());
        tracing::info!(%version, index = %new_index, full, batches = scope.len(), overlay_only, decades = ?decades, text_layout = text_layout.version(), "building index");

        report.version(&version);
        report.step(Step::Indexing).await;
        let outcome = async {
            let (docs, stories_record, replaced, (local_tally, local_replaced)) = if overlay_only {
                (0, None, Vec::new(), Default::default())
            } else {
                self.build_index(
                    lease,
                    sink,
                    &version,
                    &index_id,
                    &scope,
                    &catalog,
                    &dedup,
                    stories.as_ref(),
                    &latin_new,
                    &latin_curated,
                    &latin_owned,
                    local.as_ref(),
                    MainLayout {
                        decades,
                        text: text_layout,
                    },
                    analyzer,
                    report,
                )
                .await?
            };
            let added_latin = (latin_new.len() + replaced.len()) as u64;
            latin_after.pages.extend(replaced);
            let stories_record = stories.as_ref().zip(stories_record.as_ref());
            // The pages the version holds with our OCR's text.
            let local_files = self
                .ocr_local_files(
                    previous.as_ref().filter(|_| !full),
                    &version_batches,
                    &dedup.hidden,
                    &local_replaced,
                    local.as_ref().map(|l| (l, &local_tally)),
                )
                .await?;
            let ja = self
                .build_ja_index(lease, sink, &version, &overlay, &catalog, analyzer)
                .await?;
            // What it built in the end: the Japanese index can come out empty.
            built.ja_index = ja.is_some();
            report.step(Step::Publishing).await;
            // What every index of the version is made of, for the log and
            // the manifest: how many splits a cold search opens (05 §5.5.1).
            let mut laid_out = indexes.clone();
            laid_out.extend(ja.iter().map(|(id, _)| id.clone()));
            let layout = sink.layout(&laid_out).await?;
            for l in &layout {
                l.log();
            }
            let (bounds, added) = self
                .write_snapshot(
                    &version,
                    &version_batches,
                    &catalog,
                    &dedup,
                    &layout,
                    &overlay,
                    ja.as_ref(),
                    &Latin {
                        curated_keys: &curated_keys,
                        added: added_latin,
                        hidden: latin_hidden.len() as u64,
                        version: &latin_after,
                    },
                    stories_record,
                    &local_files,
                    &build_info::record(&built),
                    analyzer,
                )
                .await?;
            Ok::<_, anyhow::Error>((docs, bounds, ja, added))
        }
        .await;
        let (docs, (from, to), ja, added) = match outcome {
            Ok(v) => v,
            Err(e) => {
                run.status = RunStatus::Failed;
                run.failed_at = Some(Utc::now());
                run.last_error = Some(format!("{e:#}").chars().take(2000).collect());
                let _ = self.state.update_run(&run, &etag).await;
                return Err(e);
            }
        };
        run.docs = docs;
        // What it built in the end, as the manifest has it.
        run.build = Some(build_info::summary(&built));
        // Pages of our Japanese OCR that curation never had are in the snapshot's counts.
        run.pages += added;
        run.batch_list = Some(format!("{version}/{RUN_BATCHES_FILE}"));
        span.record("docs", docs);
        etag = self.state.update_run(&run, &etag).await?;

        // Publish: the version pointer is the last write (04 §4.7), and only
        // while this release still holds the writer lock.
        self.confirm(lease).await?;
        let published_at = Utc::now();
        let mut pointer = json!({
            "index_version": version,
            "backend": sink.backend(),
            "indexes": indexes,
            "reference": version,
            "bounds": {"from": from.to_string(), "to": to.to_string()},
            "published_at": published_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "previous_version": run.previous_version,
            "synthetic": self.synthetic,
            // Every index in the version has `text_cg` at this version, the
            // one of its analyzer (a delta keeps the published version's,
            // above), which the API parses queries with (#168).
            "common_grams": usnm_core::common_grams::version(analyzer),
        });
        // Every index in the version has American Stories' text: this one was
        // built with it, and so was the published version it adds to (or the
        // release built a full base, above). An overlay-only release keeps
        // the published main indexes, which have it for the same reason.
        if self.american_stories {
            pointer["american_stories"] = json!(usnm_core::american_stories::VERSION);
        }
        // Every main index of the version has the `decade` field at this
        // version: a partitioned base this run built, or one the published
        // version has, with deltas tagged like this one (05 §5.5.5).
        if decades.on() {
            pointer["decades"] = json!(usnm_core::decade::VERSION);
        }
        // Every main index of the version has one searched field for both
        // texts: a base this run built with it, or one the published version
        // has, with deltas written like it (05 §5.5.7). Absent: a field per
        // text, as before the setting.
        if text_layout != TextLayout::Separate {
            pointer["text_layout"] = json!(text_layout.version());
        }
        // The Japanese pages' index (#139), folded with the same analyzer
        // as the main indexes. An API without Japanese search ignores the
        // field; one with it folds Japanese queries as `fold` says.
        if let Some((id, pages)) = &ja {
            pointer["ja"] = json!({
                "indexes": [id],
                "fold": usnm_core::ja::fold_version(analyzer),
                "pages": pages,
            });
        }
        self.reference
            .put(
                "current.json",
                serde_json::to_vec_pretty(&pointer)?,
                "application/json",
            )
            .await?;
        // The version is live from here on: a failure to record it says so
        // (the next release repairs the record, `published_run`).
        run.status = RunStatus::Published;
        run.published_at = Some(published_at);
        let recorded = async {
            self.state.update_run(&run, &etag).await?;
            self.state.set_current_version(&version).await
        }
        .await;
        if let Err(e) = recorded {
            return Err(PublishedUnrecorded(format!(
                "`{version}` is live, but recording the publish failed: {e:#}"
            ))
            .into());
        }
        tracing::info!(%version, docs, "published");
        Ok(Some(Published {
            index_version: version,
            indexes,
            full,
            docs,
            pages: run.pages,
        }))
    }

    /// The published version's run. `current.json` is the source of truth:
    /// if a previous release crashed after writing it but before recording
    /// the publish in Cosmos, the run and `ops/current` are repaired here.
    async fn published_run(&self) -> anyhow::Result<Option<(IndexRun, PublishedPointer)>> {
        let Some(bytes) = self.reference.get("current.json").await? else {
            return Ok(None);
        };
        let pointer: Value = serde_json::from_slice(&bytes).context("current.json")?;
        let version = pointer["index_version"]
            .as_str()
            .context("current.json has no index_version")?;
        let (mut run, etag) = self.state.run(version).await?.with_context(|| {
            format!("current.json names `{version}`, which has no index run in the pipeline state")
        })?;
        if run.status != RunStatus::Published {
            tracing::warn!(version, "recording a publish that was interrupted");
            run.status = RunStatus::Published;
            // The pointer's own time, so the run and current.json agree
            // however long after the crash this runs.
            let pointer_at = pointer["published_at"]
                .as_str()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc));
            run.published_at = pointer_at.or(run.published_at).or_else(|| Some(Utc::now()));
            self.state.update_run(&run, &etag).await?;
        }
        if self.state.current_version().await?.as_deref() != Some(version) {
            self.state.set_current_version(version).await?;
        }
        let feature = |key: &str| pointer[key].as_u64().and_then(|v| u32::try_from(v).ok());
        let published = PublishedPointer {
            backend: pointer["backend"].as_str().unwrap_or("memory").to_owned(),
            grams: feature("common_grams"),
            american_stories: feature("american_stories"),
            decades: feature("decades"),
            text_layout: feature("text_layout"),
        };
        Ok(Some((run, published)))
    }

    /// How this release's main index lays its pages out by decade (05
    /// §5.5.5): a full base as `partition_decade` says; a delta (or an
    /// overlay-only release, which keeps the main indexes) as the published
    /// version has it, so every main index of the new version has the
    /// `decade` field or none does. A delta on a version laid out by decade
    /// tags its splits instead of partitioning them: it is small, a few
    /// splits at most, and partitions would cut those into one per decade,
    /// which every full-range search (the site's default) would open.
    fn decade_layout(&self, full: bool, published: Option<u32>) -> Decades {
        if full {
            return if self.partition_decade {
                Decades::Partitioned
            } else {
                Decades::Off
            };
        }
        match published {
            Some(v) if v == usnm_core::decade::VERSION => Decades::Tagged,
            Some(v) => {
                tracing::warn!(
                    published = v,
                    api = usnm_core::decade::VERSION,
                    "the published version is laid out by another version of the decades; this \
                     delta has no decade field, so searches skip no splits until the next full release"
                );
                Decades::Off
            }
            None => {
                if self.partition_decade {
                    tracing::info!(
                        "the published version isn't laid out by decade: --partition-decade \
                         takes effect at the next full release"
                    );
                }
                Decades::Off
            }
        }
    }

    /// The text fields of this release's main index (05 §5.5.7): a full
    /// base's as `single_text_field` says; a delta's (or an overlay-only
    /// release's, which keeps the main indexes) as the published version
    /// has them, so every main index of a version has the same fields.
    /// `published` is `None` for a version this code doesn't know, which
    /// takes a full base (`relayouting`).
    fn text_layout(&self, full: bool, published: Option<TextLayout>) -> TextLayout {
        if full {
            return if self.single_text_field {
                TextLayout::Single
            } else {
                TextLayout::Separate
            };
        }
        let layout = published.unwrap_or_default();
        if self.single_text_field && layout == TextLayout::Separate {
            tracing::info!(
                "the published version searches a field per text: --single-text-field \
                 takes effect at the next full release"
            );
        }
        layout
    }

    /// `pages-v{date}-{n}` and its new index, `pages-{base|delta}-{date}-{n}`.
    async fn next_names(&self, full: bool) -> anyhow::Result<(String, String)> {
        let stamp = self.now.format("%Y%m%d");
        for n in 1..1000 {
            let version = format!("pages-v{stamp}-{n}");
            if self.state.run(&version).await?.is_none() {
                let kind = if full { "base" } else { "delta" };
                return Ok((version, format!("pages-{kind}-{stamp}-{n}")));
            }
        }
        bail!("too many index runs today")
    }

    /// The batches a published version was built from. Runs written before
    /// the list moved to the reference snapshot carry it inline; newer runs
    /// point at the snapshot's `batches.json`, which is checked against its
    /// manifest entry like the API checks the files it loads.
    async fn run_batches(&self, run: &IndexRun) -> anyhow::Result<Vec<RunBatch>> {
        if let Some(batches) = &run.batches {
            return Ok(batches.clone());
        }
        let path = run.batch_list.as_deref().with_context(|| {
            format!(
                "index run `{}` has neither a batch list nor inline batches",
                run.index_version
            )
        })?;
        let (dir, name) = path
            .rsplit_once('/')
            .with_context(|| format!("batch list path `{path}` has no directory"))?;
        let manifest_path = format!("{dir}/manifest.json");
        let manifest: Value = serde_json::from_slice(
            &self
                .reference
                .get(&manifest_path)
                .await?
                .with_context(|| format!("`{manifest_path}` is missing"))?,
        )
        .context(manifest_path.clone())?;
        let entry = manifest["files"]
            .as_array()
            .and_then(|files| files.iter().find(|f| f["path"] == name))
            .with_context(|| format!("`{manifest_path}` does not list `{name}`"))?;
        let bytes = self
            .reference
            .get(path)
            .await?
            .with_context(|| format!("`{path}` is missing"))?;
        let digest = hex(&Sha256::digest(&bytes));
        if entry["bytes"].as_u64() != Some(bytes.len() as u64)
            || !entry["sha256"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(&digest))
        {
            bail!("`{path}` does not match its manifest entry");
        }
        let batches: Vec<RunBatch> = serde_json::from_slice(&bytes).context(path.to_owned())?;
        if let Some(n) = run.batch_count {
            anyhow::ensure!(
                batches.len() as u64 == n,
                "`{path}` lists {} batches; the run recorded {n}",
                batches.len()
            );
        }
        Ok(batches)
    }

    /// The copies `version` hid (its `duplicates.json`), or `None` when its
    /// snapshot predates the file: its indexes then hold every copy.
    async fn previously_hidden(&self, version: &str) -> anyhow::Result<dedup::PreviouslyHidden> {
        let path = format!("{version}/{DUPLICATES_FILE}");
        let Some(bytes) = self.reference.get(&path).await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&bytes).context(path)?))
    }

    async fn load_snapshot_catalog(&self, version: &str) -> anyhow::Result<Catalog> {
        let get = |name: &'static str| async move {
            let path = format!("{version}/{name}");
            let bytes = self
                .reference
                .get(&path)
                .await?
                .with_context(|| format!("`{path}` is missing"))?;
            Ok::<_, anyhow::Error>((path, bytes))
        };
        let (tp, titles) = get("titles.json").await?;
        let (pp, places) = get("places.json").await?;
        Catalog::new(
            serde_json::from_slice(&titles).context(tp)?,
            serde_json::from_slice(&places).context(pp)?,
        )
    }

    // The lease, sink and reporter are the release's, threaded through.
    #[allow(clippy::too_many_arguments)]
    async fn build_index(
        &self,
        lease: &WriterLease,
        sink: &mut dyn IndexSink,
        version: &str,
        index_id: &str,
        scope: &[RunBatch],
        catalog: &Catalog,
        dedup: &Plan,
        stories: Option<&american_stories::Source>,
        latin: &[(&ocr_ja::JaPage, String)],
        latin_curated: &HashMap<String, (&ocr_ja::JaPage, String)>,
        latin_owned: &HashMap<String, String>,
        local: Option<&LocalRun>,
        layout: MainLayout,
        analyzer: Analyzer,
        report: &Reporter,
    ) -> anyhow::Result<(
        u64,
        Option<american_stories::Record>,
        Vec<(String, JaLatinPage)>,
        (ocr_local::replace::Tally, LocalReplaced),
    )> {
        // Every title must resolve before anything is written.
        let mut missing = BTreeSet::new();
        for b in scope {
            for lccn in &b.curated.lccns {
                if catalog.title(lccn).is_none() {
                    missing.insert(lccn.clone());
                }
            }
        }
        if !missing.is_empty() {
            bail!("titles missing from the catalog: {missing:?}");
        }
        sink.create(index_id, layout).await?;
        let format = DocFormat {
            decade: layout.decades.on(),
            analyzer,
            text: layout.text,
        };
        // A partitioned base's pages go out a decade at a time, through
        // files on the writer's scratch disk (`crate::decade_order`).
        let mut order = match layout.decades {
            Decades::Partitioned => {
                let work = sink
                    .stats()
                    .work_dir
                    .clone()
                    .unwrap_or_else(std::env::temp_dir);
                let chunk = merges::template_setting("split_num_docs_target").unwrap_or(30_000);
                Some(DecadeOrder::new(&work, chunk)?)
            }
            _ => None,
        };
        // A line every 30 s and after each batch (the saved query
        // `release-progress` and the stall alert read them). Pages with
        // only American Stories' text join the expected count as they're
        // found: only their batch's parts say which pages LoC has no text for.
        let expected = scope
            .iter()
            .map(|b| {
                b.curated
                    .ok_pages
                    .saturating_sub(dedup.skipped_docs(&b.batch))
            })
            .sum();
        let progress = Progress::new(sink.stats(), expected);
        progress.snapshot().log(None);
        progress::report(&self.state, version, &progress.snapshot()).await;
        let _ticker = progress.every(progress::INTERVAL);
        // The same counts for the status page, in their own `ops` item: the
        // run item's ETag stays the release's alone.
        let _reporter =
            progress.report_every(progress::INTERVAL, self.state.clone(), version.to_owned());
        let mut docs = 0u64;
        let mut record = stories.map(|_| american_stories::Record::default());
        // The pages given our Latin text in place of LoC's (#203).
        let mut replaced: Vec<(String, JaLatinPage)> = Vec::new();
        // Our OCR from `ocr-local` weighed and given in place of LoC's (Step 9).
        let mut local_tally = ocr_local::replace::Tally::default();
        let mut local_replaced: LocalReplaced = Vec::new();
        for b in scope {
            // This batch's titles' readings of ours, dropped after the batch.
            let mine = match local {
                Some(l)
                    if l.listing
                        .has_any(b.curated.lccns.iter().map(String::as_str)) =>
                {
                    lease.check()?;
                    let loaded = l
                        .listing
                        .load(
                            self.curated.as_ref(),
                            b.curated.lccns.iter().map(String::as_str),
                            &l.revoked,
                        )
                        .await?;
                    tracing::info!(
                        batch = %b.batch,
                        pages = loaded.pages.len(),
                        revoked = loaded.revoked.len(),
                        parts = loaded.parts.len(),
                        "our OCR for the batch's titles"
                    );
                    local_tally.add_parts(&loaded.parts);
                    loaded
                }
                _ => ocr_local::replace::Loaded::default(),
            };
            // This batch's American Stories texts, dropped after the batch.
            let texts = match stories {
                Some(source) => {
                    lease.check()?;
                    let counts = dedup::load_counts(self.curated.as_ref(), b).await?;
                    let loaded = source.load(self.curated.as_ref(), &counts).await?;
                    tracing::info!(
                        batch = %b.batch,
                        pages = loaded.texts.len(),
                        text_mb = loaded.text_bytes / (1024 * 1024),
                        parts = loaded.parts.len(),
                        empty = loaded.empty,
                        "American Stories' text for the batch"
                    );
                    if let Some(r) = record.as_mut() {
                        r.add(&loaded);
                    }
                    loaded.texts
                }
                None => HashMap::new(),
            };
            for path in &b.curated.parts {
                lease.check()?;
                let bytes = self
                    .curated
                    .get(path)
                    .await?
                    .with_context(|| format!("curated part `{path}` is missing"))?;
                let mut part_docs = Vec::new();
                let (mut with_as, mut only_as, mut without_locs) = (0u64, 0u64, 0u64);
                read_part(bytes.into(), true, |row| {
                    if !dedup.keeps(&row.key, &b.batch) {
                        return Ok(());
                    }
                    let doc_id = row.key.doc_id();
                    let text_as = if texts.is_empty() {
                        None
                    } else {
                        texts.get(&doc_id).map(String::as_str)
                    };
                    // Our reading of the page, where it reads more words, on
                    // the copy the version keeps (whichever batch's copy we read).
                    let ours = latin_curated
                        .get(&doc_id)
                        .filter(|(_, t)| ocr_ja::better_than_locs(t, row.text.as_deref()));
                    let ok = row.status == TextStatus::Ok;
                    let title = catalog.title(&row.key.lccn).context("title")?;
                    // Our own OCR of the page, where rule v1 prefers it to
                    // LoC's text (Step 9). The Japanese overlay wins.
                    let mut local_text: Option<&str> = None;
                    if let Some(c) = mine.pages.get(&doc_id) {
                        if let (Some(l), None) = (local, ours) {
                            if l.ja.contains(&doc_id) {
                                local_tally.keep("japanese_ocr");
                            } else {
                                let langs: Vec<&str> =
                                    title.languages.iter().map(String::as_str).collect();
                                let (scores, _, verdict) =
                                    ocr_local::rule::judge(&c.text, row.text.as_deref(), &langs);
                                local_tally.weighed(c, &scores, verdict);
                                if verdict.is_ok() {
                                    local_text = Some(c.text.as_str());
                                    local_replaced.push((
                                        doc_id.clone(),
                                        c.engine(),
                                        b.batch.clone(),
                                    ));
                                }
                            }
                        } else {
                            local_tally.keep("japanese_ocr");
                        }
                    } else if mine.revoked.contains(&doc_id) {
                        local_tally.keep("revoked");
                    }
                    if !ok && text_as.is_none() && ours.is_none() && local_text.is_none() {
                        return Ok(());
                    }
                    // A copy without LoC's text of a page whose document is
                    // ours (an earlier index's, or a page LoC's archive had
                    // no text for in another batch): ours stays the page's.
                    if !ok
                        && latin_owned
                            .get(&doc_id)
                            .is_some_and(|ours| *ours != b.batch)
                    {
                        return Ok(());
                    }
                    with_as += u64::from(text_as.is_some());
                    only_as += u64::from(!ok && ours.is_none() && local_text.is_none());
                    without_locs += u64::from(!ok);
                    let place = catalog.place(&title.place_id).context("place")?;
                    part_docs.push(match ours {
                        Some((p, text)) => {
                            replaced.push((
                                doc_id,
                                JaLatinPage {
                                    // The document's batch, which hidden copies name.
                                    batch: b.batch.clone(),
                                    loc_text: p.loc_text.clone(),
                                    ocr_source: p.ocr_source.clone(),
                                    ocr_engine: p.ocr_engine.clone(),
                                },
                            ));
                            main_doc(&row.key, &row.batch, text, title, place, text_as, format)
                        }
                        None => match local_text {
                            Some(text) => {
                                main_doc(&row.key, &row.batch, text, title, place, text_as, format)
                            }
                            None => page_doc(&row, title, place, text_as, format),
                        },
                    });
                    Ok(())
                })
                .with_context(|| path.clone())?;
                // Pages LoC has no usable text for are documents with another's.
                progress.add_expected(without_locs);
                if let Some(r) = record.as_mut() {
                    r.docs += with_as;
                    r.only += only_as;
                }
                for d in &part_docs {
                    match order.as_mut() {
                        Some(o) => o.add(sink, d).await?,
                        None => sink.add(d).await?,
                    }
                }
                docs += part_docs.len() as u64;
            }
            progress.snapshot().log(Some(&b.batch));
        }
        if let Some(r) = &record {
            tracing::info!(
                docs_with_text = r.docs,
                only_american_stories = r.only,
                "American Stories' text indexed"
            );
        }
        // The pages LoC ships without text that our OCR read Latin text on
        // (#203): after the batches, each with its own batch's name.
        if !latin.is_empty() {
            progress.add_expected(latin.len() as u64);
            for (p, text) in latin {
                lease.check()?;
                let title = catalog.title(&p.key.lccn).context("title")?;
                let place = catalog.place(&title.place_id).context("place")?;
                // With the version's decade field, and a decade at a time
                // in a partitioned base, like LoC's pages.
                let d = ocr_ja::latin_doc(p, text, title, place, format);
                match order.as_mut() {
                    Some(o) => o.add(sink, &d).await?,
                    None => sink.add(&d).await?,
                }
            }
            docs += latin.len() as u64;
        }
        if !latin.is_empty() || !replaced.is_empty() {
            tracing::info!(
                missing = latin.len(),
                in_place_of_locs = replaced.len(),
                "indexed our Japanese OCR's Latin text"
            );
        }
        if let Some(o) = order.as_mut() {
            lease.check()?;
            let s = drain(o, sink, lease).await?;
            tracing::info!(
                runs = s.runs,
                pages = s.docs,
                peak_held_pages = s.peak_docs,
                peak_disk_mb = s.peak_bytes / (1024 * 1024),
                "sent the pages a decade at a time"
            );
        }
        drop(order);
        // The last commit and the merge wait, given up as soon as the lock is.
        report.step(Step::Merging).await;
        tokio::select! {
            r = sink.finish(docs) => r?,
            () = lease.lost() => lease.check()?,
        }
        if local.is_some() {
            tracing::info!(
                considered = local_tally.considered,
                replaced = local_tally.replaced,
                kept = ?local_tally.kept,
                "weighed our OCR against LoC's text"
            );
        }
        progress.snapshot().log(None);
        progress::report(&self.state, version, &progress.snapshot()).await;
        Ok((docs, record, replaced, (local_tally, local_replaced)))
    }

    /// Whether a release with no newly curated batches and no Latin text
    /// to add publishes the overlay on the published main indexes (04
    /// §4.8): when our Japanese OCR has changed and has a page to index.
    /// Logs why not.
    async fn overlay_releasable(
        &self,
        previous: Option<&IndexRun>,
        overlay: &ocr_ja::Overlay,
    ) -> anyhow::Result<bool> {
        let prev = previous.expect("incremental has a previous version");
        if !self.overlay_changed(&prev.index_version, overlay).await? {
            tracing::info!(
                "no newly curated batches with catalogued titles, and no new Japanese OCR; \
                 nothing to release"
            );
            return Ok(false);
        }
        // The run records the Japanese index as the index it wrote, so
        // there must be one: OCR that changed without a page to index waits.
        if !overlay.pages.iter().any(ocr_ja::JaPage::indexable) {
            tracing::info!("the Japanese OCR changed but has no page to index; nothing to release");
            return Ok(false);
        }
        tracing::info!(
            "no newly curated batches; releasing the new Japanese OCR on the same indexes"
        );
        Ok(true)
    }

    /// Whether the published version weighed every page of `overlay` for
    /// our Latin text: it was built from the same overlay parts with
    /// `--ja-latin` at this rule (its `ocr_ja.json` has `latin.rule`). A
    /// release with the setting weighs them all; one before #203's
    /// follow-up, overlay-only, weighed none and recorded no rule.
    async fn latin_settled(
        &self,
        previous: Option<&IndexRun>,
        overlay: &ocr_ja::Overlay,
    ) -> anyhow::Result<bool> {
        let prev = previous.expect("incremental has a previous version");
        let path = format!("{}/{}", prev.index_version, ocr_ja::OCR_JA_FILE);
        let Some(bytes) = self.reference.get(&path).await? else {
            return Ok(false);
        };
        let record: Value = serde_json::from_slice(&bytes).context(path)?;
        Ok(record["latin"]["rule"] == json!(ocr_ja::LATIN_VERSION)
            && record["parts"] == serde_json::to_value(&overlay.parts)?)
    }

    /// Whether the overlay's parts differ from the ones `version` was built
    /// from (its `ocr_ja.json`; none if it has none).
    async fn overlay_changed(
        &self,
        version: &str,
        overlay: &ocr_ja::Overlay,
    ) -> anyhow::Result<bool> {
        let before = match self
            .reference
            .get(&format!("{version}/{}", ocr_ja::OCR_JA_FILE))
            .await?
        {
            Some(bytes) => serde_json::from_slice::<Value>(&bytes)?["parts"].clone(),
            None => json!([]),
        };
        Ok(serde_json::to_value(&overlay.parts)? != before)
    }

    /// Build the Japanese pages' index for `version` from the overlay's pages
    /// with text, or nothing when there are none. Its name and page count.
    async fn build_ja_index(
        &self,
        lease: &WriterLease,
        sink: &mut dyn IndexSink,
        version: &str,
        overlay: &ocr_ja::Overlay,
        catalog: &Catalog,
        analyzer: Analyzer,
    ) -> anyhow::Result<Option<(String, u64)>> {
        let mut docs = Vec::new();
        for p in overlay.pages.iter().filter(|p| p.indexable()) {
            let title = catalog.title(&p.key.lccn).context("title")?;
            let place = catalog.place(&title.place_id).context("place")?;
            docs.push(ocr_ja::ja_doc(p, title, place, analyzer));
        }
        if docs.is_empty() {
            return Ok(None);
        }
        let id = ocr_ja::index_id(version);
        sink.create_with(&id, ocr_ja::JA_TEMPLATE).await?;
        for d in &docs {
            lease.check()?;
            sink.add(d).await?;
        }
        let n = docs.len() as u64;
        tokio::select! {
            r = sink.finish(n) => r?,
            () = lease.lost() => lease.check()?,
        }
        tracing::info!(index = %id, docs = n, "built the Japanese pages' index");
        Ok(Some((id, n)))
    }

    /// Write `{version}/` (titles, places, baselines, pages per title, the
    /// batch list, the hidden duplicate copies, manifest last) and return the
    /// version's date bounds. A page in more than one batch counts once.
    /// The manifest also records `layout`, the splits of each index, when
    /// the engine has splits.
    #[allow(clippy::too_many_arguments)]
    async fn write_snapshot(
        &self,
        version: &str,
        batches: &[RunBatch],
        catalog: &Catalog,
        dedup: &Plan,
        layout: &[IndexLayout],
        overlay: &ocr_ja::Overlay,
        ja: Option<&(String, u64)>,
        latin: &Latin<'_>,
        stories: Option<(&american_stories::Source, &american_stories::Record)>,
        local: &LocalFiles,
        build: &Value,
        analyzer: Analyzer,
    ) -> anyhow::Result<((NaiveDate, NaiveDate), u64)> {
        let mut baselines: BTreeMap<String, BTreeMap<u32, u32>> = BTreeMap::new();
        // Every page counted once, by its title: the same pages as the
        // baselines, which sum them by place instead.
        let mut title_pages: BTreeMap<String, u64> = BTreeMap::new();
        // The same pages again, by the languages their title lists.
        let mut by_language: BTreeMap<Vec<String>, BTreeMap<String, BTreeMap<u32, u32>>> =
            BTreeMap::new();
        let (mut first, mut last) = (u32::MAX, u32::MIN);
        for b in batches {
            for (lccn, days) in dedup::load_counts(self.curated.as_ref(), b).await? {
                let title = catalog
                    .title(&lccn)
                    .with_context(|| format!("title `{lccn}` is missing from the catalog"))?;
                let series = baselines.entry(title.place_id.clone()).or_default();
                let mut language_series = language_set(title).map(|set| {
                    by_language
                        .entry(set)
                        .or_default()
                        .entry(title.place_id.clone())
                        .or_default()
                });
                let total = title_pages.entry(lccn).or_default();
                for (day, pages) in days {
                    *series.entry(day).or_default() += pages;
                    if let Some(s) = language_series.as_mut() {
                        *s.entry(day).or_default() += pages;
                    }
                    *total += u64::from(pages);
                    first = first.min(day);
                    last = last.max(day);
                }
            }
        }
        if first > last {
            bail!("the version has no pages");
        }
        for (lccn, day, extra) in dedup.excess() {
            let title = catalog.title(lccn).context("title")?;
            let pages = baselines
                .get_mut(&title.place_id)
                .and_then(|s| s.get_mut(&day))
                .context("a duplicated page outside the baselines")?;
            *pages -= extra;
            *title_pages.get_mut(lccn).context("title pages")? -= u64::from(extra);
            if let Some(set) = language_set(title) {
                *by_language
                    .get_mut(&set)
                    .and_then(|places| places.get_mut(&title.place_id))
                    .and_then(|s| s.get_mut(&day))
                    .context("a duplicated page outside the language baselines")? -= extra;
            }
        }
        // Our OCR of pages curation never had (no ocr.txt in LoC's archive):
        // they are in no counts.json, so they join the baselines here. A page
        // missing from one batch's archive can have text in another batch of
        // the version, which counted it already: those are left out. A page
        // with a main-index document of our Latin text (#203) is one of these,
        // counted here and nowhere else.
        let mut added = 0u64;
        for p in overlay.pages.iter().filter(|p| {
            p.missing_from_curation() && !latin.curated_keys.contains_key(&p.key.doc_id())
        }) {
            let title = catalog.title(&p.key.lccn).context("title")?;
            let day = day_number(p.key.date);
            *baselines
                .entry(title.place_id.clone())
                .or_default()
                .entry(day)
                .or_default() += 1;
            *title_pages.entry(p.key.lccn.clone()).or_default() += 1;
            if let Some(set) = language_set(title) {
                *by_language
                    .entry(set)
                    .or_default()
                    .entry(title.place_id.clone())
                    .or_default()
                    .entry(day)
                    .or_default() += 1;
            }
            first = first.min(day);
            last = last.max(day);
            added += 1;
        }
        // Only titles and places with pages in this version.
        let titles: Vec<&Title> = catalog
            .titles
            .iter()
            .filter(|t| title_pages.contains_key(&t.lccn))
            .collect();
        let place_ids: BTreeSet<&str> = titles.iter().map(|t| t.place_id.as_str()).collect();
        let places: Vec<&Place> = catalog
            .places
            .iter()
            .filter(|p| place_ids.contains(p.id.as_str()))
            .collect();
        let baselines: BTreeMap<String, Vec<(u32, u32)>> = baselines
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect();
        let language_baselines = LanguageBaselines {
            sets: by_language
                .into_iter()
                .map(|(languages, places)| LanguageSet {
                    languages,
                    baselines: places
                        .into_iter()
                        .map(|(k, v)| (k, v.into_iter().collect()))
                        .collect(),
                })
                .collect(),
        };

        let mut snapshot_files: Vec<(&str, Vec<u8>)> = vec![
            ("titles.json", serde_json::to_vec(&titles)?),
            ("places.json", serde_json::to_vec(&places)?),
            ("baselines.json", serde_json::to_vec(&baselines)?),
            (TITLE_PAGES_FILE, serde_json::to_vec(&title_pages)?),
            (
                LANGUAGE_BASELINES_FILE,
                serde_json::to_vec(&language_baselines)?,
            ),
            (RUN_BATCHES_FILE, serde_json::to_vec(batches)?),
            (DUPLICATES_FILE, serde_json::to_vec(&dedup.hidden)?),
        ];
        // Which overlay parts the Japanese pages came from (only when there are any,
        // so a version without them has the same files as before).
        if !overlay.parts.is_empty() {
            let mut record = json!({
                "fold": usnm_core::ja::fold_version(analyzer),
                "index": ja.map(|(id, _)| id),
                "indexed": ja.map_or(0, |(_, n)| *n),
                "pages": overlay.pages.len(),
                "added_to_baselines": added,
                // Pages with a main-index document of our Latin text (#203):
                // added by this release, in the version, and hidden.
                "latin": {
                    "added": latin.added,
                    "pages": latin.version.pages.len(),
                    "hidden": latin.hidden,
                },
                "skipped": overlay.skipped,
                // What the OCR found: Japanese text, near-blank pages, mostly Latin.
                "kinds": ocr_ja::kinds(&overlay.pages),
                "parts": overlay.parts,
            });
            // With the setting, every page of the overlay was weighed for
            // our Latin text by this rule (`latin_settled`).
            if self.ja_latin {
                record["latin"]["rule"] = json!(ocr_ja::LATIN_VERSION);
            }
            snapshot_files.push((ocr_ja::OCR_JA_FILE, serde_json::to_vec(&record)?));
        }
        // The pages the main indexes hold with our Latin text, for the API to
        // mark their hits as our OCR (only when there are any).
        if !latin.version.pages.is_empty() {
            snapshot_files.push((JA_LATIN_FILE, serde_json::to_vec(latin.version)?));
        }
        // Which American Stories parts the new main index was built from
        // (only when it was built with them). The list can be long, so the
        // manifest, which the API reads, has only its summary.
        if let Some((source, record)) = stories {
            snapshot_files.push((
                american_stories::FILE,
                serde_json::to_vec(&record.file(source))?,
            ));
        }
        // Our OCR from `ocr-local` (Step 9): the pages the main indexes hold
        // with it, for the API, and with the setting, how this run weighed it.
        if let Some(body) = &local.pages {
            snapshot_files.push((usnm_state::ocr_local::OCR_LOCAL_PAGES_FILE, body.clone()));
        }
        if let Some(record) = &local.record {
            snapshot_files.push((
                usnm_state::ocr_local::OCR_LOCAL_FILE,
                serde_json::to_vec(record)?,
            ));
        }
        let mut files = Vec::new();
        for (name, body) in snapshot_files {
            files.push(json!({
                "path": name,
                "sha256": hex(&Sha256::digest(&body)),
                "bytes": body.len(),
            }));
            self.put_new(&format!("{version}/{name}"), body).await?;
        }
        let mut manifest = json!({
            "index_version": version,
            "files": files,
            "duplicate_pages": dedup.duplicate_pages,
            "built_from": {
                "batches": batches.iter().map(|b| json!({
                    "batch": b.batch, "version": b.curated.version, "counts": b.curated.counts,
                })).collect::<Vec<_>>(),
            },
        });
        if !layout.is_empty() {
            manifest["indexes"] = serde_json::to_value(layout)?;
        }
        if !overlay.parts.is_empty() {
            manifest["built_from"]["ocr_ja"] = serde_json::to_value(&overlay.parts)?;
        }
        if let Some((source, record)) = stories {
            manifest["built_from"]["american_stories"] = record.summary(source);
        }
        if let Some(summary) = &local.summary {
            manifest["built_from"]["ocr_local"] = summary.clone();
        }
        // What built it (#161), with the index templates in full.
        manifest["build"] = build.clone();
        self.put_new(
            &format!("{version}/manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )
        .await?;
        Ok(((date_from_day(first), date_from_day(last)), added))
    }

    /// The doc ids in `wanted` that some batch of the version has in its
    /// curated parts, with which of those copies have LoC's text. Only the
    /// parts of batches holding these pages' titles are read, without
    /// their text.
    async fn curated_keys(
        &self,
        batches: &[RunBatch],
        wanted: &BTreeSet<String>,
    ) -> anyhow::Result<BTreeMap<String, CuratedCopies>> {
        // A doc id starts with its title's LCCN (`sn83025517_1945-01-01_ed-1_seq-4`).
        let lccns: BTreeSet<&str> = wanted
            .iter()
            .filter_map(|id| id.split_once('_').map(|(lccn, _)| lccn))
            .collect();
        let mut found = BTreeMap::new();
        if wanted.is_empty() {
            return Ok(found);
        }
        for b in batches
            .iter()
            .filter(|b| b.curated.lccns.iter().any(|l| lccns.contains(l.as_str())))
        {
            for path in &b.curated.parts {
                let bytes = self
                    .curated
                    .get(path)
                    .await?
                    .with_context(|| format!("curated part `{path}` is missing"))?;
                read_part(bytes.into(), false, |row| {
                    let id = row.key.doc_id();
                    if wanted.contains(&id) {
                        let c: &mut CuratedCopies = found.entry(id).or_default();
                        if row.status == TextStatus::Ok {
                            c.with_text = true;
                        } else {
                            c.without_text.insert(b.batch.clone());
                        }
                    }
                    Ok(())
                })
                .with_context(|| path.clone())?;
            }
        }
        Ok(found)
    }

    /// The version's [`LocalFiles`]: the published version's pages carried
    /// forward on a delta (`previous`), less the copies the version hides,
    /// plus `replaced`; and with the setting, the record of this run.
    async fn ocr_local_files(
        &self,
        previous: Option<&IndexRun>,
        batches: &[RunBatch],
        hidden: &[dedup::Hidden],
        replaced: &LocalReplaced,
        run: Option<(&LocalRun, &ocr_local::replace::Tally)>,
    ) -> anyhow::Result<LocalFiles> {
        use usnm_state::ocr_local::{Builder, OcrLocalPages, OCR_LOCAL_PAGES_FILE};
        let index: HashMap<&str, u16> = batches
            .iter()
            .enumerate()
            .map(|(i, b)| Ok((b.batch.as_str(), u16::try_from(i)?)))
            .collect::<Result<_, std::num::TryFromIntError>>()
            .context("more than 65,536 batches")?;
        // A delta's batch list starts with its published version's, in the
        // same order, so the published indexes still hold.
        let mut pages = match previous {
            Some(prev) => {
                let path = format!("{}/{OCR_LOCAL_PAGES_FILE}", prev.index_version);
                match self.reference.get(&path).await? {
                    Some(bytes) => {
                        let before = OcrLocalPages::decode(&bytes, batches.len())
                            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
                        Builder::from_pages(&before, |e| Some(e.batch))
                            .map_err(|e| anyhow::anyhow!("{path}: {e}"))?
                    }
                    None => Builder::default(),
                }
            }
            None => Builder::default(),
        };
        let carried = pages.len();
        // A copy the version hides isn't what its hits show.
        let mut dropped = 0u64;
        for h in hidden {
            if pages.batch_of(&h.doc_id).is_some()
                && pages.batch_of(&h.doc_id) == index.get(h.batch.as_str()).copied()
            {
                pages.remove(&h.doc_id);
                dropped += 1;
            }
        }
        for (doc_id, engine, batch) in replaced {
            let i = *index
                .get(batch.as_str())
                .with_context(|| format!("`{batch}` isn't in the version"))?;
            pages
                .add(doc_id, engine.clone(), i)
                .map_err(|e| anyhow::anyhow!(e))?;
        }
        if dropped > 0 {
            tracing::warn!(
                pages = dropped,
                "pages indexed with our OCR's text now have another copy; dropping them"
            );
        }
        let pages = pages.finish();
        let mut files = LocalFiles {
            pages: (!pages.is_empty())
                .then(|| pages.encode())
                .transpose()
                .map_err(|e| anyhow::anyhow!(e))?,
            ..LocalFiles::default()
        };
        if let Some((run, tally)) = run {
            let summary = tally.parts_summary();
            let mut record = ocr_local::rule::record();
            record["parts"] = summary.clone();
            record["pages"] = serde_json::to_value(tally)?;
            record["version"] = json!({
                "pages": pages.len(),
                "carried": carried,
                "dropped": dropped,
                "added": replaced.len(),
            });
            record["revocations"] = run
                .revoked
                .iter()
                .map(|r| json!({"path": r.path, "reason": r.reason, "reference": r.reference}))
                .collect();
            // Of published batches, by stored scores (`Listing::estimate`).
            record["estimate_published"] = json!(run.estimate);
            files.record = Some(record);
            files.summary = Some(summary);
        }
        Ok(files)
    }

    /// The pages `version`'s main indexes hold with our Latin text (its
    /// `ja_latin.json`; none if it has none).
    async fn previous_latin(&self, version: &str) -> anyhow::Result<JaLatin> {
        let path = format!("{version}/{JA_LATIN_FILE}");
        match self.reference.get(&path).await? {
            Some(bytes) => serde_json::from_slice(&bytes).context(path),
            None => Ok(JaLatin::default()),
        }
    }

    async fn put_new(&self, path: &str, body: Vec<u8>) -> anyhow::Result<()> {
        // Every snapshot file is JSON but `ocr_local_pages.bin`.
        let content_type = if path.ends_with(".bin") {
            "application/octet-stream"
        } else {
            "application/json"
        };
        if !self.reference.put_new(path, body, content_type).await? {
            bail!("`{path}` already exists; reference snapshots are immutable");
        }
        Ok(())
    }
}

/// Send the pages `order` still holds, given up as soon as the writer lock
/// is: up to 15 decades of 30,000 pages can take longer than the lock lasts
/// without renewal.
async fn drain(
    order: &mut DecadeOrder,
    sink: &mut dyn IndexSink,
    lease: &WriterLease,
) -> anyhow::Result<crate::decade_order::OrderStats> {
    tokio::select! {
        r = order.finish(sink) => r,
        () = lease.lost() => {
            lease.check()?;
            bail!("the writer lock was lost while sending the held pages")
        }
    }
}

/// The languages a title lists, sorted and without repeats; `None` when it
/// lists none (no language filter matches its pages).
fn language_set(title: &Title) -> Option<Vec<String>> {
    let set: BTreeSet<&str> = title
        .languages
        .iter()
        .map(String::as_str)
        .filter(|l| !l.is_empty())
        .collect();
    (!set.is_empty()).then(|| set.into_iter().map(str::to_owned).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_group_by_their_sorted_language_set() {
        let mut t = Title {
            lccn: "sn1".into(),
            name: "sn1".into(),
            ordinal: 1,
            place_id: "P1".into(),
            state: "IL".into(),
            languages: vec!["ger".into(), "eng".into(), "ger".into(), String::new()],
            extra: BTreeMap::new(),
        };
        assert_eq!(
            language_set(&t),
            Some(vec!["eng".to_owned(), "ger".to_owned()])
        );
        t.languages = vec![];
        assert_eq!(language_set(&t), None);
    }

    /// A sink that never answers, like a writer that stopped taking pages.
    struct Stuck;

    #[async_trait::async_trait]
    impl IndexSink for Stuck {
        async fn create(&mut self, _: &str, _: crate::sink::MainLayout) -> anyhow::Result<()> {
            Ok(())
        }
        async fn add(&mut self, _: &Value) -> anyhow::Result<()> {
            std::future::pending().await
        }
        async fn finish(&mut self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn backend(&self) -> &'static str {
            "stuck"
        }
    }

    #[tokio::test]
    async fn a_lost_lease_stops_sending_the_held_pages() {
        let dir = tempfile::tempdir().unwrap();
        let mut order = DecadeOrder::new(dir.path(), 1000).unwrap();
        let mut sink = Stuck;
        order
            .add(&mut sink, &json!({"doc_id": "a", "decade": 1890}))
            .await
            .unwrap();
        let lost = Arc::new(AtomicBool::new(false));
        let lease = WriterLease {
            lost: lost.clone(),
            renewer: tokio::spawn(async {}),
        };
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            lost.store(true, Ordering::SeqCst);
        });
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            drain(&mut order, &mut sink, &lease),
        )
        .await
        .expect("the lost lease ends the drain")
        .unwrap_err()
        .to_string();
        assert!(err.contains("could not be renewed"), "{err}");
    }

    #[tokio::test]
    async fn a_lost_lease_ends_a_wait() {
        let lost = Arc::new(AtomicBool::new(false));
        let lease = WriterLease {
            lost: lost.clone(),
            renewer: tokio::spawn(async {}),
        };
        let flag = lost.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            flag.store(true, Ordering::SeqCst);
        });
        let r: anyhow::Result<()> = tokio::select! {
            () = std::future::pending::<()>() => Ok(()),
            () = lease.lost() => lease.check(),
        };
        assert!(r.unwrap_err().to_string().contains("could not be renewed"));
    }
}
