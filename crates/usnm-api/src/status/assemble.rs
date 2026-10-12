//! The status document's sections, built from the published reference data
//! and a [`Summary`] of the pipeline state. Pure functions of their inputs,
//! so they are tested without a database.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, DurationRound, Utc};
use serde::Serialize;
use usnm_core::names;
use usnm_state::state::{BatchStatus, RunStatus, MAX_DELTAS};
use usnm_state::summary::{BatchSummary, RunSummary, Summary};

use super::sanitize::{sanitize, short_id};
use crate::refdata::RefData;

/// Hours of curation history in the throughput chart.
pub const HISTORY_HOURS: i64 = 48;
/// Hours the curation rate (and so the ETA) is averaged over.
pub const RATE_WINDOW_HOURS: i64 = 12;
const RECENT: usize = 20;
const MAX_LISTED: usize = 100;
const MAX_RUNS: usize = 20;

/// A section that needs the pipeline state: its data, or why there is none.
#[derive(Debug, Clone, Serialize)]
pub struct Section<T> {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
}

impl<T> Section<T> {
    pub fn of(data: T) -> Self {
        Self {
            available: true,
            reason: None,
            data: Some(data),
        }
    }

    pub fn unavailable(reason: &str) -> Self {
        Self {
            available: false,
            reason: Some(reason.to_owned()),
            data: None,
        }
    }
}

/// The version the API is serving, from its reference data (always available).
#[derive(Debug, Clone, Serialize)]
pub struct Published {
    pub index_version: String,
    pub published_at: String,
    pub synthetic: bool,
    pub pages: u64,
    pub titles: usize,
    pub places: usize,
    pub bounds: crate::refdata::Bounds,
    /// Base index first, then its deltas.
    pub indexes: Vec<String>,
    pub deltas: usize,
    /// Deltas a version may carry; the release after that many is a full rebuild.
    pub max_deltas: usize,
    pub next_release_full: bool,
    /// Batches the version was built from, when its snapshot records them.
    pub batches: Option<usize>,
    /// Copies of pages that also ship in another batch, counted once in
    /// `pages` (04 §4.7); `None` when the snapshot doesn't record them.
    pub duplicate_pages: Option<u64>,
    /// Pages by state or territory, most first.
    pub by_state: Vec<StatePages>,
    pub by_language: ByLanguage,
    /// The Japanese pages we OCR (#139), when the version has their index.
    pub ja: Option<crate::refdata::JaIndexes>,
    /// Pages by year of publication, oldest first; years without pages are
    /// left out.
    pub by_year: Vec<YearPages>,
    /// The texts searched besides LoC's OCR, as the version holds them.
    pub sources: Sources,
}

/// One year's published pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct YearPages {
    pub year: i32,
    pub pages: u64,
}

/// The texts the version searches besides LoC's own OCR, each `None` when
/// the version has none of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sources {
    /// American Stories' text (04 §4.9).
    pub american_stories: Option<AmericanStories>,
    /// Our OCR's Latin-script text of Japanese titles' pages, in the main
    /// index (#203, 04 §4.8).
    pub ja_latin: Option<JaLatinPages>,
    /// Our own OCR's text in place of LoC's (12 §12.5 Step 9).
    pub ocr_local: Option<OcrLocalSummary>,
}

/// American Stories' text in the version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AmericanStories {
    /// Searches cover it (`USNM_AMERICAN_STORIES_SEARCH` leaves it on, or
    /// the version shares one field with LoC's text).
    pub searched: bool,
    /// Pages with its text, and of those pages with no text of LoC's, when
    /// the release that made the version built its only main index (no
    /// deltas): the manifest records the index it built, so a delta's
    /// count leaves out its base. `None` otherwise.
    pub pages: Option<u64>,
    pub only_pages: Option<u64>,
    /// Years of its text the release could read, under the same condition.
    pub years: Option<u64>,
}

/// Pages whose main-index document holds our OCR's Latin-script text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JaLatinPages {
    pub pages: u64,
    /// Why our OCR read them (`missing`, `empty`, `short`, `garbled`,
    /// `mixed`), most pages first.
    pub by_reason: Vec<ReasonPages>,
    /// By newspaper, most pages first.
    pub by_title: Vec<TitleOcrPages>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReasonPages {
    pub reason: String,
    pub pages: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TitleOcrPages {
    pub lccn: String,
    /// The catalog's name; the LCCN when the catalog lacks it.
    pub name: String,
    pub pages: u64,
}

/// Pages whose main-index document holds our own OCR's text in place of
/// LoC's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OcrLocalSummary {
    pub pages: u64,
    /// By engine, most pages first.
    pub by_engine: Vec<EnginePages>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnginePages {
    /// `ocr_source`, e.g. `usnm-tesseract`.
    pub source: String,
    /// The short engine key.
    pub engine: String,
    pub pages: u64,
}

/// One state's (or territory's) share of the published pages.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatePages {
    /// The places' postal code, e.g. `IL`.
    pub state: String,
    /// LoC's name for it, or the code if it isn't a known state.
    pub name: String,
    pub places: usize,
    pub titles: usize,
    pub pages: u64,
    /// Percent of every page in the version, to one decimal.
    pub percent: f64,
}

/// Pages by language. A title that lists several languages counts in each
/// one's row (as a search filtered to any of them finds its pages), so the
/// rows add up to more than the version's pages when there are such titles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ByLanguage {
    /// The snapshot records pages per title. Older snapshots don't: then
    /// only the newspaper counts are known.
    pub pages_known: bool,
    /// Titles that list more than one language, and their pages.
    pub multilingual_titles: usize,
    pub multilingual_pages: Option<u64>,
    /// Most pages first (most titles, when pages aren't known).
    pub rows: Vec<LanguagePages>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LanguagePages {
    /// The catalog's code (MARC, e.g. `eng`, or LoC's own name for a language
    /// without one); `None` for titles that list no language.
    pub code: Option<String>,
    /// LoC's name for it, e.g. "English"; "Not recorded" when `code` is `None`.
    pub name: String,
    pub titles: usize,
    pub pages: Option<u64>,
    pub percent: Option<f64>,
}

/// Published pages summed by each place's state (its baselines).
pub fn by_state(rd: &RefData) -> Vec<StatePages> {
    let mut states: BTreeMap<&str, StatePages> = BTreeMap::new();
    for p in &rd.places {
        let r = states
            .entry(p.state.as_str())
            .or_insert_with(|| StatePages {
                state: p.state.clone(),
                name: names::state_name(&p.state).unwrap_or(&p.state).to_owned(),
                places: 0,
                titles: 0,
                pages: 0,
                percent: 0.0,
            });
        r.places += 1;
        r.pages += rd.place_pages.get(&p.id).copied().unwrap_or(0);
    }
    for t in rd.titles.values() {
        if let Some(r) = rd
            .place(&t.place_id)
            .and_then(|p| states.get_mut(p.state.as_str()))
        {
            r.titles += 1;
        }
    }
    let mut rows: Vec<StatePages> = states.into_values().collect();
    for r in &mut rows {
        r.percent = percent(r.pages, rd.pages);
    }
    rows.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.name.cmp(&b.name)));
    rows
}

const NOT_RECORDED: &str = "Not recorded";

/// Published pages (when the snapshot has them per title) and titles by language.
pub fn by_language(rd: &RefData) -> ByLanguage {
    let pages_of = |lccn: &str| {
        rd.title_pages
            .as_ref()
            .map(|tp| tp.get(lccn).copied().unwrap_or(0))
    };
    let mut langs: BTreeMap<Option<&str>, (usize, u64)> = BTreeMap::new();
    let (mut multilingual_titles, mut multilingual_pages) = (0, 0);
    for t in rd.titles.values() {
        let codes: BTreeSet<&str> = t
            .languages
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect();
        let pages = pages_of(&t.lccn).unwrap_or(0);
        if codes.len() > 1 {
            multilingual_titles += 1;
            multilingual_pages += pages;
        }
        let keys: Vec<Option<&str>> = if codes.is_empty() {
            vec![None]
        } else {
            codes.into_iter().map(Some).collect()
        };
        for k in keys {
            let e = langs.entry(k).or_default();
            e.0 += 1;
            e.1 += pages;
        }
    }
    let known = rd.title_pages.is_some();
    let mut rows: Vec<LanguagePages> = langs
        .into_iter()
        .map(|(code, (titles, pages))| LanguagePages {
            name: code.map_or_else(|| NOT_RECORDED.to_owned(), names::language_name),
            code: code.map(str::to_owned),
            titles,
            pages: known.then_some(pages),
            percent: known.then(|| percent(pages, rd.pages)),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.pages
            .cmp(&a.pages)
            .then_with(|| b.titles.cmp(&a.titles))
            .then_with(|| a.name.cmp(&b.name))
    });
    ByLanguage {
        pages_known: known,
        multilingual_titles,
        multilingual_pages: known.then_some(multilingual_pages),
        rows,
    }
}

pub fn published(rd: &RefData) -> Published {
    let c = &rd.current;
    let deltas = c.indexes.len().saturating_sub(1);
    Published {
        index_version: c.index_version.clone(),
        published_at: c.published_at.clone(),
        synthetic: c.synthetic,
        pages: rd.pages,
        titles: rd.titles.len(),
        places: rd.places.len(),
        bounds: c.bounds.clone(),
        indexes: c.indexes.clone(),
        deltas,
        max_deltas: MAX_DELTAS,
        next_release_full: deltas >= MAX_DELTAS,
        batches: rd.published_batches.as_ref().map(HashMap::len),
        duplicate_pages: rd.duplicate_pages,
        by_state: by_state(rd),
        by_language: by_language(rd),
        ja: c.ja.clone(),
        by_year: rd
            .year_pages
            .iter()
            .map(|&(year, pages)| YearPages { year, pages })
            .collect(),
        sources: sources(rd),
    }
}

/// The texts the version searches besides LoC's, from its reference data.
pub fn sources(rd: &RefData) -> Sources {
    let american_stories = rd.has_american_stories().then(|| {
        // The manifest counts the main index its release built: the whole
        // version's only when that is its one main index.
        let whole = rd
            .american_stories_built
            .as_ref()
            .filter(|_| rd.current.indexes.len() == 1);
        AmericanStories {
            searched: rd.searches_american_stories(),
            pages: whole.map(|b| b.docs),
            only_pages: whole.map(|b| b.only_american_stories),
            years: whole.map(|b| b.years),
        }
    });
    let ja_latin = (!rd.ja_latin.is_empty()).then(|| {
        let mut reasons: BTreeMap<&str, u64> = BTreeMap::new();
        let mut titles: BTreeMap<&str, u64> = BTreeMap::new();
        for (doc_id, p) in &rd.ja_latin {
            *reasons.entry(p.loc_text.as_str()).or_default() += 1;
            let lccn = doc_id.split('_').next().unwrap_or(doc_id);
            *titles.entry(lccn).or_default() += 1;
        }
        let mut by_reason: Vec<ReasonPages> = reasons
            .into_iter()
            .map(|(r, pages)| ReasonPages {
                reason: r.to_owned(),
                pages,
            })
            .collect();
        by_reason.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.reason.cmp(&b.reason)));
        let mut by_title: Vec<TitleOcrPages> = titles
            .into_iter()
            .map(|(lccn, pages)| TitleOcrPages {
                lccn: lccn.to_owned(),
                name: rd
                    .titles
                    .get(lccn)
                    .map_or_else(|| lccn.to_owned(), |t| t.name.clone()),
                pages,
            })
            .collect();
        by_title.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.lccn.cmp(&b.lccn)));
        JaLatinPages {
            pages: rd.ja_latin.len() as u64,
            by_reason,
            by_title,
        }
    });
    let ocr_local = (!rd.ocr_local.is_empty()).then(|| {
        let mut per = vec![0u64; rd.ocr_local.engines.len()];
        for e in &rd.ocr_local.entries {
            if let Some(n) = per.get_mut(usize::from(e.engine)) {
                *n += 1;
            }
        }
        let mut by_engine: Vec<EnginePages> = rd
            .ocr_local
            .engines
            .iter()
            .zip(per)
            .filter(|(_, pages)| *pages > 0)
            .map(|(e, pages)| EnginePages {
                source: e.source.clone(),
                engine: e.engine.clone(),
                pages,
            })
            .collect();
        by_engine.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.engine.cmp(&b.engine)));
        OcrLocalSummary {
            pages: rd.ocr_local.len() as u64,
            by_engine,
        }
    });
    Sources {
        american_stories,
        ja_latin,
        ocr_local,
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ByStatus {
    pub queued: u64,
    pub downloading: u64,
    pub curated: u64,
    pub failed: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HourBin {
    pub start: DateTime<Utc>,
    pub batches: u64,
    pub pages: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Throughput {
    /// Batches curated per hour, oldest first, ending with the current hour.
    pub hours: Vec<HourBin>,
    pub rate_window_hours: i64,
    /// Batches curated per hour over the last `rate_window_hours`.
    pub rate_per_hour: f64,
    /// Batches still to curate (queued or downloading).
    pub remaining: u64,
    /// When the remaining batches finish at the current rate.
    pub eta: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocPacer {
    /// The next free bulk-download slot, shared by every worker.
    pub next_slot: Option<DateTime<Utc>>,
    /// Downloads are held until then (LoC answered 429).
    pub blocked_until: Option<DateTime<Utc>>,
    pub throttled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InProgress {
    pub batch: String,
    pub version: u16,
    /// A short, opaque id for the worker holding the lease.
    pub worker: Option<String>,
    /// When the worker claimed it.
    pub since: DateTime<Utc>,
    pub lease_until: Option<DateTime<Utc>>,
    /// The worker stopped renewing it; another worker may claim it.
    pub lease_expired: bool,
    pub attempts: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Recent {
    pub batch: String,
    pub version: u16,
    pub pages: u64,
    pub curated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Failed {
    pub batch: String,
    pub version: u16,
    pub attempts: u32,
    /// Sanitized and shortened.
    pub error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Backfill {
    /// Batches listed by LoC and recorded by the pipeline.
    pub total: u64,
    pub by_status: ByStatus,
    /// Downloading with a live lease.
    pub in_progress: u64,
    /// Downloading with an expired lease (the worker stopped).
    pub stale_leases: u64,
    /// Queued again after a failed attempt.
    pub retrying: u64,
    pub percent: f64,
    /// Pages in curated batches, and those with usable text (the indexed ones).
    pub pages: u64,
    pub ok_pages: u64,
    /// Batches by the LoC version being curated (`_ver01`, `_ver02`, …).
    pub versions: BTreeMap<String, u64>,
    /// A newer version is waiting to replace a curated one.
    pub newer_versions_pending: u64,
    pub throughput: Throughput,
    pub loc: LocPacer,
    pub in_progress_batches: Vec<InProgress>,
    pub recent: Vec<Recent>,
    pub failed_batches: Vec<Failed>,
    /// The lists above are cut to their first entries.
    pub listed_limit: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Writer {
    /// A release holds the single-writer lock.
    pub held: bool,
    pub holder: Option<String>,
    pub until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseProgress {
    pub index_version: String,
    pub docs_sent: u64,
    pub docs_expected: u64,
    pub percent: f64,
    pub mb_sent: f64,
    pub updated_at: DateTime<Utc>,
}

/// A run's state as the status page shows it. The pipeline records only
/// building, published and failed; a release that was stopped (a job stopped
/// by hand, a replica killed) never records an end, so a run left `building`
/// that isn't the one holding the writer is shown as stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Building,
    Published,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub index_version: String,
    pub full: bool,
    pub status: RunState,
    pub new_index: String,
    pub indexes: usize,
    pub batches: Option<u64>,
    pub docs: u64,
    pub pages: u64,
    pub duplicate_pages: u64,
    pub started_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
    pub duration_secs: Option<i64>,
    pub previous_version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Indexing {
    /// The published version as the pipeline recorded it (`ops/current`).
    pub current_version: Option<String>,
    pub writer: Writer,
    /// The release being built, with its progress, if one is running.
    pub release: Option<ReleaseProgress>,
    pub last_published_at: Option<DateTime<Utc>>,
    /// Failed runs among the recent ones, whenever they happened.
    pub failed_runs: u64,
    /// Failed runs that started after the last successful publish: the
    /// ones not yet superseded by a version that did publish.
    pub failed_since_last_publish: u64,
    /// Most recent first.
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Catalog {
    pub titles: usize,
    pub places: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TitlesPipeline {
    /// Titles with pages in curated batches.
    pub curated_titles: usize,
    /// Of those, titles the catalog doesn't have yet (they wait for titles-sync).
    pub awaiting_sync: Option<usize>,
    /// Curated batches with at least one such title: releases skip them.
    pub batches_waiting_for_titles: Option<usize>,
    /// Curated batches (at their curated version) not in the published version.
    pub unpublished_batches: Option<usize>,
    /// Batches the next release can take: unpublished ones whose titles are
    /// all catalogued, plus re-curated ones when it will be a full release.
    pub ready_for_release: Option<usize>,
    /// Published batches curated again at a newer version: only a full release takes them.
    pub recurated_awaiting_full: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Titles {
    /// `catalog/titles.json` and `places.json` (maintained by titles-sync and geocode).
    pub catalog: Section<Catalog>,
    /// Titles and places in the published version.
    pub published_titles: usize,
    pub published_places: usize,
    pub pipeline: Section<TitlesPipeline>,
}

/// Titles in the catalog (for the sync counts).
pub struct CatalogTitles {
    pub lccns: HashSet<String>,
    pub places: usize,
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round1(100.0 * part as f64 / whole as f64)
    }
}

pub fn backfill(at: DateTime<Utc>, s: &Summary) -> Backfill {
    let mut by = ByStatus::default();
    let mut versions = BTreeMap::new();
    let (mut in_progress, mut stale, mut retrying, mut newer) = (0, 0, 0, 0);
    let (mut pages, mut ok_pages) = (0, 0);
    let mut active = Vec::new();
    let mut failed = Vec::new();
    for b in &s.batches {
        *versions.entry(format!("{:02}", b.version)).or_insert(0) += 1;
        if b.curated_version.is_some_and(|v| v < b.version) {
            newer += 1;
        }
        match b.status {
            BatchStatus::Queued => {
                by.queued += 1;
                if b.attempts > 0 {
                    retrying += 1;
                }
            }
            BatchStatus::Downloading => {
                by.downloading += 1;
                let expired = b.lease.as_ref().is_none_or(|l| l.until <= at);
                if expired {
                    stale += 1;
                } else {
                    in_progress += 1;
                }
                active.push(InProgress {
                    batch: b.batch.clone(),
                    version: b.version,
                    worker: b.lease.as_ref().map(|l| short_id(&l.owner)),
                    since: b.updated_at,
                    lease_until: b.lease.as_ref().map(|l| l.until),
                    lease_expired: expired,
                    attempts: b.attempts,
                });
            }
            BatchStatus::Curated => {
                by.curated += 1;
                pages += b.curated_pages.unwrap_or(0);
                ok_pages += b.curated_ok_pages.unwrap_or(0);
            }
            BatchStatus::Failed => {
                by.failed += 1;
                failed.push(Failed {
                    batch: b.batch.clone(),
                    version: b.version,
                    attempts: b.attempts,
                    error: b.last_error.as_deref().map(sanitize),
                    updated_at: b.updated_at,
                });
            }
        }
    }
    // Every batch's last committed curation, whatever its status now (a
    // curated batch may be queued again for a newer version).
    let mut curated: Vec<(&BatchSummary, DateTime<Utc>)> = s
        .batches
        .iter()
        .filter_map(|b| b.curated_time().map(|t| (b, t)))
        .collect();
    let this_hour = at.duration_trunc(Duration::hours(1)).unwrap_or(at);
    let first_hour = this_hour - Duration::hours(HISTORY_HOURS - 1);
    let mut hours: Vec<HourBin> = (0..HISTORY_HOURS)
        .map(|i| HourBin {
            start: first_hour + Duration::hours(i),
            batches: 0,
            pages: 0,
        })
        .collect();
    let window_start = at - Duration::hours(RATE_WINDOW_HOURS);
    let mut in_window = 0u64;
    for &(b, t) in &curated {
        if t >= first_hour && t <= at {
            let i = ((t - first_hour).num_hours()).clamp(0, HISTORY_HOURS - 1) as usize;
            hours[i].batches += 1;
            hours[i].pages += b.curated_pages.unwrap_or(0);
        }
        if t > window_start && t <= at {
            in_window += 1;
        }
    }
    let rate = in_window as f64 / RATE_WINDOW_HOURS as f64;
    let remaining = by.queued + by.downloading;
    let eta = (rate > 0.0 && remaining > 0)
        .then(|| at + Duration::seconds((remaining as f64 / rate * 3600.0) as i64));

    curated.sort_by(|(a, x), (b, y)| y.cmp(x).then(a.batch.cmp(&b.batch)));
    let recent = curated
        .iter()
        .take(RECENT)
        .map(|&(b, t)| Recent {
            batch: b.batch.clone(),
            version: b.curated_version.unwrap_or(b.version),
            pages: b.curated_pages.unwrap_or(0),
            curated_at: t,
        })
        .collect();
    active.sort_by(|a, b| a.since.cmp(&b.since).then(a.batch.cmp(&b.batch)));
    active.truncate(MAX_LISTED);
    failed.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.batch.cmp(&b.batch)));
    failed.truncate(MAX_LISTED);

    let blocked = s.ops.pacer_blocked_until.filter(|u| *u > at);
    Backfill {
        total: s.batches.len() as u64,
        percent: percent(by.curated, s.batches.len() as u64),
        by_status: by,
        in_progress,
        stale_leases: stale,
        retrying,
        pages,
        ok_pages,
        versions,
        newer_versions_pending: newer,
        throughput: Throughput {
            hours,
            rate_window_hours: RATE_WINDOW_HOURS,
            rate_per_hour: round1(rate),
            remaining,
            eta,
        },
        loc: LocPacer {
            next_slot: s.ops.pacer_next.filter(|n| *n > at),
            blocked_until: blocked,
            throttled: blocked.is_some(),
        },
        in_progress_batches: active,
        recent,
        failed_batches: failed,
        listed_limit: MAX_LISTED,
    }
}

pub fn indexing(at: DateTime<Utc>, s: &Summary) -> Indexing {
    let writer = s
        .ops
        .writer
        .as_ref()
        .filter(|w| !w.owner.is_empty() && w.until > at);
    let mut runs: Vec<&RunSummary> = s.runs.iter().collect();
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    // Only the newest run can be the one running: an older run left
    // `building` by a crashed release stays that way.
    let running = runs
        .first()
        .filter(|r| r.status == RunStatus::Building)
        .map(|r| r.index_version.as_str());
    let release = s
        .ops
        .release_progress
        .as_ref()
        .filter(|p| writer.is_some() && running == Some(p.index_version.as_str()))
        .map(|p| ReleaseProgress {
            index_version: p.index_version.clone(),
            docs_sent: p.docs_sent,
            docs_expected: p.docs_expected,
            percent: percent(p.docs_sent, p.docs_expected),
            mb_sent: p.mb_sent,
            updated_at: p.updated_at,
        });
    let last_published_at = runs.iter().filter_map(|r| r.published_at).max();
    Indexing {
        current_version: s.ops.current_version.clone(),
        writer: Writer {
            held: writer.is_some(),
            holder: writer.map(|w| short_id(&w.owner)),
            until: writer.map(|w| w.until),
        },
        release,
        last_published_at,
        failed_runs: runs
            .iter()
            .filter(|r| r.status == RunStatus::Failed)
            .count() as u64,
        failed_since_last_publish: runs
            .iter()
            .filter(|r| r.status == RunStatus::Failed)
            .filter(|r| last_published_at.is_none_or(|p| r.started_at > p))
            .count() as u64,
        runs: runs
            .iter()
            .take(MAX_RUNS)
            .map(|r| Run {
                index_version: r.index_version.clone(),
                full: r.full,
                status: match r.status {
                    RunStatus::Published => RunState::Published,
                    RunStatus::Failed => RunState::Failed,
                    // Building only while it is the newest run and a writer
                    // holds the lock; otherwise nothing is building it.
                    RunStatus::Building
                        if writer.is_some() && running == Some(r.index_version.as_str()) =>
                    {
                        RunState::Building
                    }
                    RunStatus::Building => RunState::Stopped,
                },
                new_index: r.new_index.clone(),
                indexes: r.indexes.len(),
                batches: r.batches(),
                docs: r.docs,
                pages: r.pages,
                duplicate_pages: r.duplicate_pages,
                started_at: r.started_at,
                published_at: r.published_at,
                duration_secs: r.published_at.map(|p| (p - r.started_at).num_seconds()),
                previous_version: r.previous_version.clone(),
                error: r.last_error.as_deref().map(sanitize),
            })
            .collect(),
    }
}

pub fn titles(
    rd: &RefData,
    catalog: Option<&CatalogTitles>,
    catalog_reason: &str,
    pipeline: Option<&Summary>,
    pipeline_reason: &str,
) -> Titles {
    let pipeline = match pipeline {
        None => Section::unavailable(pipeline_reason),
        Some(s) => Section::of(titles_pipeline(rd, catalog, s)),
    };
    Titles {
        catalog: match catalog {
            Some(c) => Section::of(Catalog {
                titles: c.lccns.len(),
                places: c.places,
            }),
            None => Section::unavailable(catalog_reason),
        },
        published_titles: rd.titles.len(),
        published_places: rd.places.len(),
        pipeline,
    }
}

fn titles_pipeline(rd: &RefData, catalog: Option<&CatalogTitles>, s: &Summary) -> TitlesPipeline {
    let curated: Vec<&BatchSummary> = s
        .batches
        .iter()
        .filter(|b| b.curated_version.is_some())
        .collect();
    let curated_titles: BTreeSet<&str> = curated
        .iter()
        .flat_map(|b| b.curated_lccns.iter().map(String::as_str))
        .collect();
    let missing =
        |b: &BatchSummary, c: &CatalogTitles| b.curated_lccns.iter().any(|l| !c.lccns.contains(l));
    let awaiting_sync = catalog.map(|c| {
        curated_titles
            .iter()
            .filter(|l| !c.lccns.contains(**l))
            .count()
    });
    let waiting = catalog.map(|c| curated.iter().filter(|b| missing(b, c)).count());
    let published = rd.published_batches.as_ref();
    let next_full = rd.current.indexes.len() > MAX_DELTAS;
    let unpublished: Option<Vec<&&BatchSummary>> = published.map(|p| {
        curated
            .iter()
            .filter(|b| !p.contains_key(&b.batch))
            .collect()
    });
    TitlesPipeline {
        curated_titles: curated_titles.len(),
        awaiting_sync,
        batches_waiting_for_titles: waiting,
        // A full release (after MAX_DELTAS deltas) also takes re-curated batches.
        ready_for_release: match (published, catalog) {
            (Some(p), Some(c)) => Some(
                curated
                    .iter()
                    .filter(|b| match p.get(&b.batch) {
                        None => true,
                        Some(v) => next_full && Some(*v) != b.curated_version,
                    })
                    .filter(|b| !missing(b, c))
                    .count(),
            ),
            _ => None,
        },
        unpublished_batches: unpublished.as_ref().map(Vec::len),
        recurated_awaiting_full: published.map(|p| {
            curated
                .iter()
                .filter(|b| {
                    p.get(&b.batch)
                        .is_some_and(|v| Some(*v) != b.curated_version)
                })
                .count()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use usnm_state::summary::{BatchSummary, LockSummary, RunSummary};

    fn at() -> DateTime<Utc> {
        "2026-09-29T12:30:00Z".parse().unwrap()
    }

    fn batch(v: Value) -> BatchSummary {
        serde_json::from_value(v).unwrap()
    }

    fn curated(name: &str, hours_ago: i64, pages: u64, lccns: &[&str]) -> BatchSummary {
        batch(json!({
            "batch": name, "version": 1, "status": "curated", "attempts": 1,
            "updated_at": at() - Duration::minutes(hours_ago * 60 + 1),
            "curated_version": 1, "curated_pages": pages, "curated_ok_pages": pages - 1,
            "curated_lccns": lccns,
        }))
    }

    fn summary() -> Summary {
        Summary {
            batches: vec![
                curated("batch_a", 0, 10, &["sn1"]),
                curated("batch_b", 1, 20, &["sn1", "sn2"]),
                curated("batch_c", 30, 5, &["sn3"]),
                curated("batch_old", 100, 7, &["sn1"]),
                batch(json!({"batch": "batch_q", "version": 1, "status": "queued",
                         "updated_at": at()})),
                batch(
                    json!({"batch": "batch_r", "version": 2, "status": "queued", "attempts": 2,
                         "last_error": "GET https://chroniclingamerica.loc.gov/x returned 503",
                         "updated_at": at(), "curated_version": 1, "curated_pages": 3,
                         "curated_at": at() - Duration::hours(30),
                         "curated_lccns": ["sn1"]}),
                ),
                batch(
                    json!({"batch": "batch_d", "version": 1, "status": "downloading",
                         "attempts": 1, "updated_at": at() - Duration::minutes(5),
                         "lease": {"owner": "caj-usnm-backfill-x-12-0a1b2c3d",
                                   "until": at() + Duration::hours(1)}}),
                ),
                batch(
                    json!({"batch": "batch_e", "version": 1, "status": "downloading",
                         "attempts": 3, "updated_at": at() - Duration::hours(5),
                         "lease": {"owner": "w-1-deadbeef", "until": at() - Duration::hours(1)}}),
                ),
                batch(
                    json!({"batch": "batch_f", "version": 1, "status": "failed", "attempts": 5,
                         "last_error": "put https://stusnmdata.blob.core.windows.net/curated/x: 403 from 192.0.2.4",
                         "updated_at": at() - Duration::hours(2)}),
                ),
            ],
            ..Summary::default()
        }
    }

    #[test]
    fn backfill_counts_rates_and_lists() {
        let mut s = summary();
        s.ops.pacer_blocked_until = Some(at() + Duration::minutes(40));
        s.ops.pacer_next = Some(at() - Duration::minutes(1));
        let b = backfill(at(), &s);
        assert_eq!(b.total, 9);
        assert_eq!(
            (
                b.by_status.queued,
                b.by_status.downloading,
                b.by_status.curated,
                b.by_status.failed
            ),
            (2, 2, 4, 1)
        );
        assert_eq!((b.in_progress, b.stale_leases, b.retrying), (1, 1, 1));
        assert_eq!(b.percent, 44.4);
        assert_eq!((b.pages, b.ok_pages), (42, 38));
        assert_eq!(b.versions.get("01"), Some(&8));
        assert_eq!(b.versions.get("02"), Some(&1));
        assert_eq!(b.newer_versions_pending, 1);
        // 48 hourly bins ending with this hour; batch_old is older than that.
        let t = &b.throughput;
        assert_eq!(t.hours.len(), 48);
        assert_eq!(
            t.hours[47].start,
            "2026-09-29T12:00:00Z".parse::<DateTime<Utc>>().unwrap()
        );
        // batch_r's curation (queued again for a newer version) still counts.
        assert_eq!(t.hours.iter().map(|h| h.batches).sum::<u64>(), 4);
        assert_eq!(t.hours.iter().map(|h| h.pages).sum::<u64>(), 38);
        // Two batches in the last 12 hours: 2/12 per hour, 4 remaining → 24 h.
        assert_eq!(t.rate_per_hour, 0.2);
        assert_eq!(t.remaining, 4);
        assert_eq!(t.eta, Some(at() + Duration::hours(24)));
        assert!(b.loc.throttled);
        assert_eq!(b.loc.next_slot, None);
        // Workers appear only as short ids; the stopped one is flagged.
        let ids: Vec<_> = b
            .in_progress_batches
            .iter()
            .map(|a| (a.batch.as_str(), a.worker.as_deref(), a.lease_expired))
            .collect();
        assert_eq!(
            ids,
            [
                ("batch_e", Some("adbeef"), true),
                ("batch_d", Some("1b2c3d"), false)
            ]
        );
        assert_eq!(b.recent[0].batch, "batch_a");
        assert_eq!(b.recent.len(), 5);
        let err = b.failed_batches[0].error.as_deref().unwrap();
        assert_eq!(err, "put [url]: 403 from [ip]");
        let text = serde_json::to_string(&b).unwrap();
        assert!(!text.contains("caj-usnm"), "{text}");
        assert!(!text.contains("blob.core"), "{text}");
    }

    #[test]
    fn no_rate_means_no_eta() {
        let mut s = summary();
        s.batches.retain(|b| b.status != BatchStatus::Curated);
        let b = backfill(at(), &s);
        assert_eq!((b.throughput.rate_per_hour, b.throughput.eta), (0.0, None));
        assert_eq!(b.percent, 0.0);
        assert!(!b.loc.throttled);
    }

    fn run(v: Value) -> RunSummary {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn indexing_runs_writer_and_live_progress() {
        let mut s = Summary {
            runs: vec![
                run(
                    json!({"index_version": "v1", "full": true, "indexes": ["b1"], "new_index": "b1",
                       "status": "published", "docs": 9, "pages": 10, "batches_len": 2,
                       "started_at": "2026-09-28T10:00:00Z", "published_at": "2026-09-28T11:00:00Z"}),
                ),
                run(
                    json!({"index_version": "v2", "full": false, "indexes": ["b1", "d1"], "new_index": "d1",
                       "status": "failed", "started_at": "2026-09-29T01:00:00Z",
                       "last_error": "lock `quickwit-writer` is held by `ca-x-1-0a1b2c3d` until later"}),
                ),
                run(
                    json!({"index_version": "v3", "full": false, "indexes": ["b1", "d2"], "new_index": "d2",
                       "status": "building", "batch_count": 3, "started_at": "2026-09-29T12:00:00Z"}),
                ),
            ],
            ..Summary::default()
        };
        s.ops.current_version = Some("v1".into());
        s.ops.writer = Some(LockSummary {
            owner: "caj-usnm-ingest-9-0badf00d".into(),
            until: at() + Duration::minutes(20),
        });
        s.ops.release_progress = Some(usnm_state::state::ReleaseProgress {
            index_version: "v3".into(),
            docs_sent: 250,
            docs_expected: 1000,
            mb_sent: 12.5,
            updated_at: at(),
        });
        let i = indexing(at(), &s);
        assert_eq!(
            i.runs
                .iter()
                .map(|r| r.index_version.as_str())
                .collect::<Vec<_>>(),
            ["v3", "v2", "v1"]
        );
        assert_eq!(i.runs[2].duration_secs, Some(3600));
        // An older run's inline list length, or a newer run's batch_count.
        assert_eq!(i.runs[2].batches, Some(2));
        assert_eq!(i.runs[0].batches, Some(3));
        assert_eq!(
            i.runs[1].error.as_deref(),
            Some("lock `quickwit-writer` is held by `[worker]` until later")
        );
        assert_eq!(i.failed_runs, 1);
        // v2 failed after v1 published, and nothing has published since.
        assert_eq!(i.failed_since_last_publish, 1);
        assert!(i.writer.held);
        assert_eq!(i.writer.holder.as_deref(), Some("adf00d"));
        assert_eq!(i.release.as_ref().unwrap().percent, 25.0);
        assert_eq!(
            i.last_published_at,
            Some("2026-09-28T11:00:00Z".parse().unwrap())
        );

        // An older run left `building` by a crash isn't the running one.
        let mut crashed = s.clone();
        crashed.runs.push(run(
            json!({"index_version": "v4", "full": false, "indexes": ["b1", "d3"],
            "new_index": "d3", "status": "failed", "started_at": "2026-09-29T12:20:00Z"}),
        ));
        assert!(indexing(at(), &crashed).release.is_none());

        // A run left `building` under a newer one was stopped; so is the newest
        // once no writer holds the lock.
        let mut stopped = s.clone();
        stopped.runs.push(run(
            json!({"index_version": "v0", "full": true, "indexes": ["b0"], "new_index": "b0",
            "status": "building", "started_at": "2026-09-27T12:00:00Z"}),
        ));
        let i = indexing(at(), &stopped);
        let state =
            |i: &Indexing, v: &str| i.runs.iter().find(|r| r.index_version == v).unwrap().status;
        assert_eq!(state(&i, "v0"), RunState::Stopped);
        assert_eq!(
            state(&i, &i.runs[0].index_version.clone()),
            RunState::Building
        );
        stopped.ops.writer.as_mut().unwrap().until = at() - Duration::minutes(1);
        let i = indexing(at(), &stopped);
        assert_eq!(
            state(&i, &i.runs[0].index_version.clone()),
            RunState::Stopped
        );

        // A version published after the failure supersedes it.
        let mut recovered = s.clone();
        recovered.runs.push(run(
            json!({"index_version": "v5", "full": false, "indexes": ["b1", "d4"], "new_index": "d4",
            "status": "published", "started_at": "2026-09-29T02:00:00Z",
            "published_at": "2026-09-29T02:30:00Z"}),
        ));
        let r = indexing(at(), &recovered);
        assert_eq!((r.failed_runs, r.failed_since_last_publish), (1, 0));

        // Progress from a release that is no longer running isn't shown.
        s.ops.writer.as_mut().unwrap().until = at() - Duration::minutes(1);
        let i = indexing(at(), &s);
        assert!(!i.writer.held && i.release.is_none() && i.writer.holder.is_none());
    }

    async fn fixture_refdata() -> RefData {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data");
        RefData::load(&usnm_store::LocalStore::new(dir))
            .await
            .unwrap()
    }

    fn place(id: &str, state: &str) -> crate::refdata::Place {
        crate::refdata::Place {
            id: id.into(),
            ordinal: 0,
            name: id.into(),
            state: state.into(),
            lat: 0.0,
            lon: 0.0,
            precision: "city".into(),
        }
    }

    fn title(lccn: &str, place_id: &str, languages: &[&str]) -> crate::refdata::Title {
        crate::refdata::Title {
            lccn: lccn.into(),
            name: lccn.into(),
            place_id: place_id.into(),
            state: String::new(),
            languages: languages.iter().map(|l| l.to_string()).collect(),
            first_year: None,
            last_year: None,
        }
    }

    /// The fixture version with these places, titles and pages instead.
    async fn with(
        places: Vec<crate::refdata::Place>,
        titles: Vec<(crate::refdata::Title, u64)>,
    ) -> RefData {
        let mut rd = fixture_refdata().await;
        rd.place_index = places
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        rd.places = places;
        rd.place_pages = HashMap::new();
        for (t, pages) in &titles {
            *rd.place_pages.entry(t.place_id.clone()).or_default() += pages;
        }
        rd.pages = titles.iter().map(|(_, n)| n).sum();
        rd.title_pages = Some(titles.iter().map(|(t, n)| (t.lccn.clone(), *n)).collect());
        rd.titles = titles
            .into_iter()
            .map(|(t, _)| (t.lccn.clone(), t))
            .collect();
        rd
    }

    #[tokio::test]
    async fn pages_by_year_add_up_to_the_versions_pages() {
        let rd = fixture_refdata().await;
        let p = published(&rd);
        assert!(!p.by_year.is_empty());
        assert_eq!(p.by_year.iter().map(|y| y.pages).sum::<u64>(), p.pages);
        assert!(p.by_year.windows(2).all(|w| w[0].year < w[1].year));
        assert!(p.by_year.iter().all(|y| y.pages > 0));
        assert_eq!(p.by_year.first().unwrap().year, 1895);
        // The fixture searches LoC's text alone.
        assert_eq!(
            p.sources,
            Sources {
                american_stories: None,
                ja_latin: None,
                ocr_local: None
            }
        );
    }

    #[tokio::test]
    async fn sources_count_the_texts_besides_locs() {
        use usnm_state::ocr_local::{Engine, Entry};
        use usnm_state::state::JaLatinPage;
        let mut rd = fixture_refdata().await;
        let lccn = rd.titles.keys().min().unwrap().clone();
        let name = rd.titles[&lccn].name.clone();
        let page = |reason: &str| JaLatinPage {
            batch: "b".into(),
            loc_text: reason.into(),
            ocr_source: "usnm-ndlocr".into(),
            ocr_engine: "ndlocr-lite".into(),
        };
        rd.ja_latin = HashMap::from([
            (format!("{lccn}_1942-01-01_ed-1_seq-1"), page("missing")),
            (format!("{lccn}_1942-01-01_ed-1_seq-2"), page("missing")),
            ("sn99999999_1942-01-01_ed-1_seq-1".into(), page("mixed")),
        ]);
        let engine = |source: &str, engine: &str| Engine {
            source: source.into(),
            engine: engine.into(),
        };
        let entry = |hash: u64, engine: u8| Entry {
            hash,
            engine,
            batch: 0,
        };
        rd.ocr_local = usnm_state::ocr_local::OcrLocalPages {
            engines: vec![
                engine("usnm-a", "a/1 x"),
                engine("usnm-b", "b/1 y"),
                engine("usnm-c", "c/1 z"),
            ],
            entries: vec![entry(1, 1), entry(2, 1), entry(3, 0)],
        };
        rd.current.american_stories = Some(usnm_core::american_stories::VERSION);
        rd.american_stories_built = Some(crate::refdata::AmericanStoriesBuilt {
            years: 3,
            docs: 900,
            only_american_stories: 12,
        });
        let s = sources(&rd);
        let ja = s.ja_latin.unwrap();
        assert_eq!(ja.pages, 3);
        let reasons: Vec<_> = ja
            .by_reason
            .iter()
            .map(|r| (r.reason.as_str(), r.pages))
            .collect();
        assert_eq!(reasons, [("missing", 2), ("mixed", 1)]);
        assert_eq!(
            ja.by_title,
            [
                TitleOcrPages {
                    lccn: lccn.clone(),
                    name,
                    pages: 2
                },
                // Not in the catalog: named by its LCCN.
                TitleOcrPages {
                    lccn: "sn99999999".into(),
                    name: "sn99999999".into(),
                    pages: 1
                }
            ]
        );
        let local = s.ocr_local.unwrap();
        assert_eq!(local.pages, 3);
        // Most pages first; an engine with no pages is left out.
        let engines: Vec<_> = local
            .by_engine
            .iter()
            .map(|e| (e.engine.as_str(), e.pages))
            .collect();
        assert_eq!(engines, [("b/1 y", 2), ("a/1 x", 1)]);
        // The fixture has a delta: the manifest's count isn't the version's.
        assert_eq!(
            s.american_stories,
            Some(AmericanStories {
                searched: true,
                pages: None,
                only_pages: None,
                years: None
            })
        );
        rd.current.indexes.truncate(1);
        rd.american_stories_search = false;
        assert_eq!(
            sources(&rd).american_stories,
            Some(AmericanStories {
                searched: false,
                pages: Some(900),
                only_pages: Some(12),
                years: Some(3)
            })
        );
    }

    #[tokio::test]
    async fn pages_by_state_sum_each_states_places() {
        let rd = with(
            vec![
                place("P1", "IL"),
                place("P2", "IL"),
                place("P3", "NY"),
                place("P4", "XX"),
                place("P5", "DC"),
            ],
            vec![
                (title("a", "P1", &["eng"]), 600),
                (title("b", "P1", &["ger"]), 200),
                (title("c", "P2", &["eng"]), 100),
                (title("d", "P3", &["eng"]), 99),
                (title("e", "P4", &["eng"]), 1),
                (title("f", "P5", &["eng"]), 0),
            ],
        )
        .await;
        let rows = by_state(&rd);
        let got: Vec<_> = rows
            .iter()
            .map(|r| {
                (
                    r.state.as_str(),
                    r.name.as_str(),
                    r.places,
                    r.titles,
                    r.pages,
                    r.percent,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("IL", "Illinois", 2, 3, 900, 90.0),
                ("NY", "New York", 1, 1, 99, 9.9),
                // Not a known state: shown by its code.
                ("XX", "XX", 1, 1, 1, 0.1),
                ("DC", "District of Columbia", 1, 1, 0, 0.0),
            ]
        );
        assert_eq!(rows.iter().map(|r| r.pages).sum::<u64>(), rd.pages);
    }

    #[tokio::test]
    async fn a_title_in_two_languages_counts_in_both() {
        let rd = with(
            vec![place("P1", "PA")],
            vec![
                (title("a", "P1", &["eng"]), 100),
                (title("b", "P1", &["eng", "ger"]), 40),
                (title("c", "P1", &["ger"]), 10),
                // No language, or only an empty one: "Not recorded".
                (title("d", "P1", &[]), 4),
                (title("g", "P1", &[""]), 1),
                // A repeated code counts once.
                (title("e", "P1", &["eng", "eng"]), 1),
                (title("f", "P1", &["pennsylvania german"]), 4),
            ],
        )
        .await;
        let l = by_language(&rd);
        assert!(l.pages_known);
        assert_eq!((l.multilingual_titles, l.multilingual_pages), (1, Some(40)));
        let got: Vec<_> = l
            .rows
            .iter()
            .map(|r| {
                (
                    r.code.as_deref(),
                    r.name.as_str(),
                    r.titles,
                    r.pages,
                    r.percent,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (Some("eng"), "English", 3, Some(141), Some(88.1)),
                (Some("ger"), "German", 2, Some(50), Some(31.3)),
                (None, "Not recorded", 2, Some(5), Some(3.1)),
                (
                    Some("pennsylvania german"),
                    "Pennsylvania German",
                    1,
                    Some(4),
                    Some(2.5)
                ),
            ]
        );
        // The bilingual title's pages are in both rows: more than the total.
        let rows: u64 = l.rows.iter().filter_map(|r| r.pages).sum();
        assert_eq!((rows, rd.pages), (200, 160));

        // An older snapshot without pages per title: newspapers only, most first.
        let mut old = rd;
        old.title_pages = None;
        let l = by_language(&old);
        assert!(!l.pages_known);
        assert_eq!((l.multilingual_titles, l.multilingual_pages), (1, None));
        let got: Vec<_> = l
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.titles, r.pages, r.percent))
            .collect();
        assert_eq!(
            got,
            [
                ("English", 3, None, None),
                ("German", 2, None, None),
                ("Not recorded", 2, None, None),
                ("Pennsylvania German", 1, None, None),
            ]
        );
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v["rows"][0]["pages"], Value::Null);
        assert_eq!(v["multilingual_pages"], Value::Null);
    }

    #[tokio::test]
    async fn published_and_titles_sync() {
        let mut rd = fixture_refdata().await;
        let p = published(&rd);
        assert_eq!((p.deltas, p.max_deltas, p.next_release_full), (1, 8, false));
        assert_eq!(p.batches, None);

        let s = summary();
        let catalog = CatalogTitles {
            lccns: ["sn1", "sn2"].iter().map(|s| s.to_string()).collect(),
            places: 2,
        };
        // Without the published batch list, only the sync counts.
        let t = titles(&rd, Some(&catalog), "", Some(&s), "");
        let tp = t.pipeline.data.as_ref().unwrap();
        assert_eq!(tp.curated_titles, 3);
        assert_eq!(tp.awaiting_sync, Some(1));
        assert_eq!(tp.batches_waiting_for_titles, Some(1));
        assert_eq!(tp.unpublished_batches, None);

        rd.published_batches = Some(
            [("batch_a".to_owned(), 1), ("batch_r".to_owned(), 0)]
                .into_iter()
                .collect(),
        );
        let t = titles(&rd, Some(&catalog), "", Some(&s), "");
        let tp = t.pipeline.data.as_ref().unwrap();
        // b, c and old aren't published, and c waits for sn3; r was
        // published at another version than it is curated at now.
        assert_eq!(tp.unpublished_batches, Some(3));
        assert_eq!(tp.ready_for_release, Some(2));
        assert_eq!(tp.recurated_awaiting_full, Some(1));
        assert_eq!(t.catalog.data.as_ref().unwrap().titles, 2);
        // After 8 deltas the next release is full and takes batch_r too.
        rd.current.indexes = (0..9).map(|i| format!("i{i}")).collect();
        let t = titles(&rd, Some(&catalog), "", Some(&s), "");
        assert_eq!(t.pipeline.data.as_ref().unwrap().ready_for_release, Some(3));

        let t = titles(&rd, None, "no catalog", None, "no pipeline");
        assert!(!t.catalog.available && !t.pipeline.available);
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(
            v["pipeline"],
            json!({"available": false, "reason": "no pipeline"})
        );
    }
}
