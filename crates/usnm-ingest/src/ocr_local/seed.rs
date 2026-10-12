//! The seeder (12 §12.5 Step 10, #296): fills `ocr-bulk` a window at a
//! time with the pages LoC read worst, as a step of `ocr-sync` and as
//! `usnm-ingest ocr-seed` (a dry run, or one run by hand).
//!
//! **Off** while the plan is empty (`USNM_OCR_SEED_PLAN`), which keeps
//! ADR-0015 decision 7: nothing is queued until someone names a plan.
//!
//! **Window.** At most `window` bulk pages are outstanding at once: pages
//! the ledger has queued with no result yet (or the queue's approximate
//! count, whichever is larger; ADR-0015's queue hides a message for 1800 s
//! per receive and dead-letters it after 5, so a page stays outstanding
//! until `ocr-sync` imports it or drains its dead letter). A run tops up
//! only when the queue holds fewer than `refill_below` messages. With
//! `window` 0 the window is about two days of pages, from the pages a day
//! `ocr-sync` imported over the last week (its `imported` outcomes), never below
//! [`MIN_AUTO_WINDOW`]; `refill_below` 0 is half the window.
//!
//! **Order.** (batch, lccn) units ranked by summed max(0, damage_rate −
//! 0.1) over their pages in the languages asked for (ISO 639-3, by default
//! the Tesseract pilot's [`PILOT_LANGUAGES`]), from Step 6's tables for the
//! published version. A unit's pages go in issue order (date, edition,
//! seq), so the worker's issue cache hits. Each batch's units are worked
//! out once per batch version and set of filters, and kept under
//! `ocr-local/seed/rank/<key>/`, so a run stopped by its deadline goes on
//! where it stopped and later runs read them back; a change to a title's
//! languages gets new ones. Units a plan has finished are listed under
//! `ocr-local/seed/plans/<plan>/done/<filters>/`, so changing the filters
//! looks at every unit again.
//!
//! **Skips.** Titles that list Japanese, titles in the `ocr-ja` overlay and
//! pages in `ja_latin.json`; pages the ledger has (imported, outstanding or
//! dead-lettered: those come back only by the retry rules below); copies
//! of a duplicated page the version doesn't keep (`duplicates.json`, and
//! any copy where a batch sorting first has the page with text, the
//! release's rule); pages under the minimum damage. With
//! `skip_american_stories` (off by default, 12 §12.6 question 8), also
//! pages whose American Stories text scores under the minimum damage.
//!
//! **Retries**, for the plan's own pages: a page outstanding 14 days is
//! queued again, and so is one dead-lettered with cause `dequeue_count`,
//! at most [`MAX_REQUEUES`] times; one dead-lettered as `unroutable` is
//! queued again outside that limit after 6 hours, doubling per unroutable
//! cycle of the page up to 48 (13 §13.7.6). A page whose message a failed
//! run never put (`not_sent`) is queued again at once, within the limit.
//! Other causes are left to `ocr-requeue`. `ocr-enqueue` and `ocr-requeue`
//! hold the same lock as the seeder, so the window count never races them;
//! bulk pages they queue count toward the window but aren't limited by it.
//!
//! **Fetched scans.** With [`Source::Fetched`] (Step 11, #368) a plan's new
//! pages come from the prefetch manifest's keep set instead, in the order
//! the fetcher takes it, and only pages on the newest list a node relayed
//! from the scan server (`usnm_ocr_contract::fetched`), so the workers read
//! cached scans. The list only narrows the manifest's pages.
//!
//! **Pilot.** `pilot` limits a plan to one group (one language, or a list
//! of batches) and a budget of at most [`PILOT_MAX_PAGES`] pages (Step 11).
//!
//! Every message gets its ledger row first, as `ocr-enqueue` writes them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use anyhow::{bail, Context};
use arrow_array::{Array, Date32Array, Float64Array, Int16Array, StringArray};
use bytes::Bytes;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use futures::{StreamExt, TryStreamExt};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use usnm_core::ids::PageKey;
use usnm_ocr_contract::fetched;
use usnm_ocr_contract::message::{OcrMessage, Reason};
use usnm_ocr_contract::prefetch::{self, ManifestRow};
use usnm_state::state::{JaLatin, RunBatch, State, DUPLICATES_FILE, JA_LATIN_FILE};
use usnm_store::queue::{Queue, BULK};
use usnm_store::ObjectStore;

use super::enqueue::{self, message_language, QueueReport, MAX_PAGES};
use super::ledger::{self, Ledger, Queued, DEAD_LETTERED, IMPORTED, NOT_SENT, UNROUTABLE};
use super::sync::Lease;
use super::{run_id, ROOT};
use crate::curated::from_unix_days;
use crate::ocr_damage::{self, VersionList};

/// The languages the Tesseract pilot routes (ADR-0016): the default.
pub const PILOT_LANGUAGES: [&str; 6] = ["ces", "deu", "eng", "pol", "rus", "srp"];
/// Damage under this adds nothing to a unit's rank.
pub const DAMAGE_FLOOR: f64 = 0.1;
/// The default minimum damage of a page to queue (a badly damaged page).
pub const MIN_DAMAGE: f64 = 0.25;
/// Days a page may stay outstanding before it is queued again.
pub const REQUEUE_AFTER_DAYS: i64 = 14;
/// Times a page is queued again after a timeout or a `dequeue_count`
/// dead letter.
pub const MAX_REQUEUES: u32 = 3;
/// The wait before an `unroutable` page is queued again, doubling per cycle.
pub const UNROUTABLE_FIRST_HOURS: i64 = 6;
pub const UNROUTABLE_MAX_HOURS: i64 = 48;
/// The largest budget a pilot plan may have.
pub const PILOT_MAX_PAGES: u64 = 10_000;
/// The automatic window's floor, for a start with no imports to measure.
pub const MIN_AUTO_WINDOW: u64 = 500;
/// The automatic window: this many days of pages, measured over the last
/// [`RATE_DAYS`] of imports.
pub const WINDOW_DAYS: u64 = 2;
pub const RATE_DAYS: i64 = 7;
/// Page tables held in memory at once.
const TABLE_CACHE: usize = 8;
/// Rank files read at once.
const RANK_READS: usize = 32;

/// What to seed. Settings `USNM_OCR_SEED_*` and `USNM_OCR_BULK_*`.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// The plan, as the ledger records it; empty: seeding is off.
    pub plan: String,
    /// Only these base batches (`name` or `name_verNN`); empty: all.
    pub batches: Vec<String>,
    /// ISO 639-3: the page's detected language, or its title's first for
    /// `und`. Empty: [`PILOT_LANGUAGES`].
    pub languages: Vec<String>,
    /// Only issues in these years (inclusive).
    pub years: Option<(i32, i32)>,
    pub min_damage: f64,
    /// The plan's budget: distinct pages it ever queues.
    pub max_pages: Option<u64>,
    /// Bulk pages outstanding at most; 0: about two days of pages.
    pub window: u64,
    /// Top up only below this many messages on `ocr-bulk`; 0: half the window.
    pub refill_below: u64,
    /// One group and a small budget (Step 11).
    pub pilot: bool,
    /// Skip pages whose American Stories text scores under `min_damage`.
    pub skip_american_stories: bool,
    /// Work out and report what would be queued; write and send nothing.
    pub dry_run: bool,
    /// Stop working out ranks after this; the next run goes on.
    pub deadline: Option<Instant>,
    /// Where new pages come from.
    pub source: Source,
    /// With [`Source::Fetched`]: the prefetch manifest's index version
    /// (`pages-v20261010-2`); empty: the published version.
    pub manifest: String,
}

/// Where a plan's new pages come from (`USNM_OCR_SEED_SOURCE`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Units ranked from the damage tables (Step 10).
    #[default]
    Damage,
    /// The prefetch manifest's keep set, in the fetcher's order, only pages
    /// whose scans the newest fetched list has (Step 11, #368).
    Fetched,
}

impl std::str::FromStr for Source {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.trim() {
            "" | "damage" => Ok(Self::Damage),
            "fetched" => Ok(Self::Fetched),
            other => bail!("seed source `{other}`: damage or fetched"),
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            plan: String::new(),
            batches: Vec::new(),
            languages: Vec::new(),
            years: None,
            min_damage: MIN_DAMAGE,
            max_pages: None,
            window: 0,
            refill_below: 0,
            pilot: false,
            skip_american_stories: false,
            dry_run: false,
            deadline: None,
            source: Source::Damage,
            manifest: String::new(),
        }
    }
}

impl Options {
    /// Whether a plan is set.
    pub fn on(&self) -> bool {
        !self.plan.is_empty()
    }

    /// The languages asked for, or the pilot's, in ISO 639-3. A MARC code
    /// (`ger`, the damage tables' and the manifest's) is taken as its ISO
    /// code (`deu`).
    pub fn language_set(&self) -> BTreeSet<String> {
        if self.languages.is_empty() {
            PILOT_LANGUAGES.iter().map(|l| (*l).to_owned()).collect()
        } else {
            self.languages
                .iter()
                .map(|l| {
                    if usnm_core::lang::iso3_to_marc(l).is_some() {
                        l.clone()
                    } else {
                        usnm_core::lang::marc_to_iso3(l).map_or_else(|| l.clone(), str::to_owned)
                    }
                })
                .collect()
        }
    }

    /// The base batch names asked for.
    pub fn batch_set(&self) -> BTreeSet<String> {
        self.batches
            .iter()
            .map(|b| base_batch(b).to_owned())
            .collect()
    }

    /// Refuse settings that make no sense, before anything is read.
    pub fn check(&self) -> anyhow::Result<()> {
        if !usnm_store::is_safe_segment(&self.plan) {
            bail!("the seed plan must be letters, digits, `.`, `_` or `-`");
        }
        for l in self.language_set() {
            if usnm_core::lang::iso3_to_marc(&l).is_none() {
                bail!("seed language `{l}` isn't an ISO 639-3 code usnm_core::lang knows");
            }
        }
        for b in &self.batches {
            if !usnm_store::is_safe_segment(b) {
                bail!("unsafe seed batch name {b:?}");
            }
        }
        if !self.manifest.is_empty() && !usnm_store::is_safe_segment(&self.manifest) {
            bail!("unsafe seed manifest version {:?}", self.manifest);
        }
        if !(self.min_damage > 0.0 && self.min_damage <= 1.0) {
            bail!("the minimum damage must be above 0 and at most 1");
        }
        if let Some((from, to)) = self.years {
            if from > to {
                bail!("seed years {from}-{to} run backwards");
            }
        }
        if self.source == Source::Fetched && self.skip_american_stories {
            bail!("the fetched seed source doesn't apply the American Stories skip; turn one off");
        }
        if self.max_pages == Some(0) {
            bail!("a seed budget of 0 pages queues nothing; leave it unset for no budget");
        }
        if self.pilot {
            match self.max_pages {
                Some(n) if n <= PILOT_MAX_PAGES => {}
                _ => bail!("a pilot needs a budget (max pages) of at most {PILOT_MAX_PAGES}"),
            }
            if self.batches.is_empty() && self.language_set().len() != 1 {
                bail!("a pilot takes one group: one language, or a list of batches");
            }
        }
        Ok(())
    }

    /// The key of the rank files: everything that changes a unit's score
    /// or page count.
    fn rank_key(&self) -> String {
        let langs: Vec<String> = self.language_set().into_iter().collect();
        let basis = format!(
            "metric={}|languages={}|years={:?}|min_damage={}",
            usnm_core::damage::VERSION,
            langs.join(","),
            self.years,
            self.min_damage
        );
        crate::source::hex(&Sha256::digest(basis.as_bytes()))[..16].to_owned()
    }
}

/// `YYYY` or `YYYY-YYYY`; empty: none.
pub fn parse_years(s: &str) -> anyhow::Result<Option<(i32, i32)>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let year = |y: &str| -> anyhow::Result<i32> {
        let n: i32 = y
            .trim()
            .parse()
            .with_context(|| format!("seed years: `{y}` isn't a year"))?;
        if !(1600..=2100).contains(&n) {
            bail!("seed years: {n} is out of range");
        }
        Ok(n)
    };
    Ok(Some(match s.split_once('-') {
        Some((a, b)) => (year(a)?, year(b)?),
        None => {
            let y = year(s)?;
            (y, y)
        }
    }))
}

/// `name_verNN` → `name`.
fn base_batch(b: &str) -> &str {
    match b.rsplit_once("_ver") {
        Some((base, v)) if v.len() == 2 && v.bytes().all(|c| c.is_ascii_digit()) => base,
        _ => b,
    }
}

/// One (batch, lccn) unit and its rank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub batch: String,
    pub batch_version: u16,
    pub lccn: String,
    /// Summed max(0, damage_rate − 0.1) over the unit's pages in the
    /// languages and years asked for.
    pub score: f64,
    /// Of those, the pages at or above the minimum damage.
    pub pages: u64,
}

/// One page of a damage table, as far as the seeder reads it.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    doc_id: String,
    lccn: String,
    date: NaiveDate,
    edition: u16,
    seq: u16,
    ok: bool,
    lang: Option<String>,
    damage_rate: Option<f64>,
}

/// A damage table's rows, reading only the columns the seeder needs.
fn read_rows(bytes: Bytes) -> anyhow::Result<Vec<Row>> {
    const COLS: [&str; 8] = [
        "doc_id",
        "lccn",
        "date",
        "edition",
        "seq",
        "text_status",
        "lang",
        "damage_rate",
    ];
    let builder = ParquetRecordBatchReaderBuilder::try_new(bytes)?;
    let schema = builder.schema().clone();
    let wanted: Vec<usize> = COLS
        .iter()
        .map(|n| {
            schema
                .index_of(n)
                .with_context(|| format!("damage table has no `{n}` column"))
        })
        .collect::<anyhow::Result<_>>()?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), wanted);
    let mut out = Vec::new();
    for batch in builder.with_projection(mask).build()? {
        let batch = batch?;
        macro_rules! col {
            ($name:expr, $t:ty) => {
                batch
                    .column_by_name($name)
                    .with_context(|| format!("damage table has no `{}` column", $name))?
                    .as_any()
                    .downcast_ref::<$t>()
                    .with_context(|| format!("column `{}` has an unexpected type", $name))?
            };
        }
        let doc_id = col!("doc_id", StringArray);
        let lccn = col!("lccn", StringArray);
        let date = col!("date", Date32Array);
        let edition = col!("edition", Int16Array);
        let seq = col!("seq", Int16Array);
        let status = col!("text_status", StringArray);
        let lang = col!("lang", StringArray);
        let rate = col!("damage_rate", Float64Array);
        for i in 0..batch.num_rows() {
            out.push(Row {
                doc_id: doc_id.value(i).to_owned(),
                lccn: lccn.value(i).to_owned(),
                date: from_unix_days(date.value(i)),
                edition: u16::try_from(edition.value(i)).unwrap_or(0),
                seq: u16::try_from(seq.value(i)).unwrap_or(0),
                ok: status.value(i) == "ok",
                lang: (!lang.is_null(i)).then(|| lang.value(i).to_owned()),
                damage_rate: (!rate.is_null(i)).then(|| rate.value(i)),
            });
        }
    }
    Ok(out)
}

/// The filters a page must pass to count in a unit.
struct Filter {
    languages: BTreeSet<String>,
    years: Option<(i32, i32)>,
    min_damage: f64,
}

impl Filter {
    fn new(opts: &Options) -> Self {
        Self {
            languages: opts.language_set(),
            years: opts.years,
            min_damage: opts.min_damage,
        }
    }

    /// The page's message language when it is in the languages and years
    /// asked for and has a damage rate.
    fn language(&self, row: &Row, title_languages: &[String]) -> Option<String> {
        row.damage_rate?;
        if let Some((from, to)) = self.years {
            if !(from..=to).contains(&row.date.year()) {
                return None;
            }
        }
        let l = message_language(row.lang.as_deref(), title_languages)?;
        self.languages.contains(&l).then_some(l)
    }

    fn damaged(&self, row: &Row) -> bool {
        row.damage_rate.is_some_and(|r| r >= self.min_damage)
    }
}

fn japanese(langs: &[String]) -> bool {
    langs.iter().any(|l| l == "jpn")
}

/// A batch version's units, from its damage table.
fn units_of(
    batch: &str,
    version: u16,
    rows: &[Row],
    titles: &HashMap<String, Vec<String>>,
    filter: &Filter,
) -> Vec<Unit> {
    let none = Vec::new();
    let mut by: BTreeMap<&str, (f64, u64)> = BTreeMap::new();
    for row in rows {
        let langs = titles.get(&row.lccn).unwrap_or(&none);
        if japanese(langs) || filter.language(row, langs).is_none() {
            continue;
        }
        let e = by.entry(&row.lccn).or_default();
        e.0 += (row.damage_rate.unwrap_or(0.0) - DAMAGE_FLOOR).max(0.0);
        e.1 += u64::from(filter.damaged(row));
    }
    by.into_iter()
        .filter(|(_, (_, pages))| *pages > 0)
        .map(|(lccn, (score, pages))| Unit {
            batch: batch.to_owned(),
            batch_version: version,
            lccn: lccn.to_owned(),
            score,
            pages,
        })
        .collect()
}

/// A batch's rank file. `titles` tags the title languages its units were
/// worked out with, so a release that corrects a title's languages gets
/// new units for the same batch version.
fn rank_path(key: &str, batch: &RunBatch, titles: &HashMap<String, Vec<String>>) -> String {
    let mut lccns: Vec<&String> = batch.curated.lccns.iter().collect();
    lccns.sort();
    let mut h = Sha256::new();
    for l in lccns {
        h.update(l.as_bytes());
        h.update(b"=");
        for lang in titles.get(l).map(Vec::as_slice).unwrap_or_default() {
            h.update(lang.as_bytes());
            h.update(b",");
        }
        h.update(b";");
    }
    let tag = &crate::source::hex(&h.finalize())[..12];
    format!(
        "{ROOT}/seed/rank/{key}/{}_ver{:02}-{tag}.json",
        batch.batch, batch.curated.version
    )
}

/// Where a plan's finished units are listed, per set of filters: units
/// finished under other filters may have pages these make eligible.
fn done_dir(plan: &str, opts: &Options) -> String {
    let skip = if opts.skip_american_stories {
        "-as"
    } else {
        ""
    };
    format!("{ROOT}/seed/plans/{plan}/done/{}{skip}", opts.rank_key())
}

/// A unit a plan has nothing more to queue from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DoneUnit {
    pub batch: String,
    pub batch_version: u16,
    pub lccn: String,
}

/// The damage tables of the published version: base batch → (version, table).
type Tables = HashMap<String, (u16, String)>;

/// Read and cache page tables.
struct TableCache<'a> {
    curated: &'a dyn ObjectStore,
    tables: &'a Tables,
    held: HashMap<String, Arc<Vec<Row>>>,
}

impl<'a> TableCache<'a> {
    fn new(curated: &'a dyn ObjectStore, tables: &'a Tables) -> Self {
        Self {
            curated,
            tables,
            held: HashMap::new(),
        }
    }

    async fn rows(&mut self, batch: &str) -> anyhow::Result<Arc<Vec<Row>>> {
        if let Some(r) = self.held.get(batch) {
            return Ok(Arc::clone(r));
        }
        let (_, path) = self
            .tables
            .get(batch)
            .with_context(|| format!("the damage tables' list has no table for {batch}"))?;
        let bytes = self
            .curated
            .get(path)
            .await?
            .with_context(|| format!("damage table {path} not found"))?;
        let path = path.clone();
        let rows = tokio::task::spawn_blocking(move || read_rows(Bytes::from(bytes)))
            .await?
            .with_context(|| format!("reading {path}"))?;
        if self.held.len() >= TABLE_CACHE {
            self.held.clear();
        }
        let rows = Arc::new(rows);
        self.held.insert(batch.to_owned(), Arc::clone(&rows));
        Ok(rows)
    }
}

/// The ranked units, and how many batches are still to be worked out.
struct Rank {
    units: Vec<Unit>,
    pending: u64,
}

/// Rank the units of `batches`: each batch's from its rank file, or worked
/// out from its table (and stored, unless `write` is off) until the deadline.
async fn rank(
    cache: &mut TableCache<'_>,
    batches: &[&RunBatch],
    titles: &HashMap<String, Vec<String>>,
    opts: &Options,
    write: bool,
) -> anyhow::Result<Rank> {
    let key = opts.rank_key();
    let filter = Filter::new(opts);
    let curated = cache.curated;
    let paths: Vec<String> = batches.iter().map(|b| rank_path(&key, b, titles)).collect();
    let stored: Vec<Option<Vec<u8>>> = futures::stream::iter(paths.iter())
        .map(|p| curated.get(p))
        .buffered(RANK_READS)
        .try_collect()
        .await?;
    let mut units = Vec::new();
    let mut pending = 0u64;
    for ((b, path), body) in batches.iter().zip(&paths).zip(stored) {
        if let Some(body) = body {
            let u: Vec<Unit> =
                serde_json::from_slice(&body).with_context(|| format!("reading {path}"))?;
            units.extend(u);
            continue;
        }
        if opts.deadline.is_some_and(|d| Instant::now() >= d) {
            pending += 1;
            continue;
        }
        let rows = cache.rows(&b.batch).await?;
        let u = units_of(&b.batch, b.curated.version, &rows, titles, &filter);
        if write {
            // Two runs working out the same batch write the same units.
            curated
                .put_new(path, serde_json::to_vec(&u)?, "application/json")
                .await?;
        }
        units.extend(u);
    }
    units.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.batch.cmp(&b.batch))
            .then_with(|| a.lccn.cmp(&b.lccn))
    });
    Ok(Rank { units, pending })
}

/// Pages a day over the last [`RATE_DAYS`]: the `imported` outcomes
/// `ocr-sync` wrote to the ledger with each import.
fn import_rate(ledger: &Ledger, now: DateTime<Utc>) -> f64 {
    let since = now - chrono::Duration::days(RATE_DAYS);
    let mut pages = 0usize;
    let mut first: Option<DateTime<Utc>> = None;
    for o in ledger.outcomes.values() {
        if o.outcome == IMPORTED && o.at >= since {
            pages += 1;
            first = Some(first.map_or(o.at, |f| f.min(o.at)));
        }
    }
    let Some(first) = first else {
        return 0.0;
    };
    // Over the days the imports span, at least one.
    let days = ((now - first).num_seconds() as f64 / 86_400.0).clamp(1.0, RATE_DAYS as f64);
    pages as f64 / days
}

/// The window: the setting, or about [`WINDOW_DAYS`] of pages.
pub fn auto_window(pages_a_day: f64) -> u64 {
    let w = (pages_a_day * WINDOW_DAYS as f64).ceil();
    (w as u64).max(MIN_AUTO_WINDOW)
}

/// Why a page of the plan is queued again, if it is.
fn retry_kind(q: &Queued, ledger: &Ledger, now: DateTime<Utc>) -> Option<(&'static str, bool)> {
    let after = ledger
        .outcomes
        .get(&q.doc_id)
        .filter(|o| o.at >= q.queued_at);
    match after {
        None => (now - q.queued_at >= chrono::Duration::days(REQUEUE_AFTER_DAYS))
            .then_some(("timed_out", true)),
        Some(o) if o.outcome == IMPORTED => None,
        Some(o) if o.outcome == NOT_SENT => Some((NOT_SENT, true)),
        Some(o) if o.outcome == DEAD_LETTERED => match o.cause.as_deref() {
            Some("dequeue_count") => Some(("dequeue_count", true)),
            Some(UNROUTABLE) => {
                let cycles = ledger.unroutable_cycles(&q.doc_id).max(1);
                let wait = unroutable_wait_hours(cycles);
                (now - o.at >= chrono::Duration::hours(wait)).then_some((UNROUTABLE, false))
            }
            _ => None,
        },
        Some(_) => None,
    }
}

/// Hours before the `cycles`th `unroutable` page is queued again: 6,
/// doubling per cycle, at most 48.
pub fn unroutable_wait_hours(cycles: u32) -> i64 {
    let doublings = cycles.saturating_sub(1).min(8);
    (UNROUTABLE_FIRST_HOURS << doublings).min(UNROUTABLE_MAX_HOURS)
}

/// Whether a page counts against the window now: outstanding, bulk, not
/// waiting out an `unroutable` dead letter, and not timed out.
fn in_flight(q: &Queued, ledger: &Ledger, now: DateTime<Utc>) -> bool {
    if q.reason != Reason::Bulk.as_str() || ledger.outstanding(&q.doc_id).is_none() {
        return false;
    }
    let unroutable = ledger
        .outcomes
        .get(&q.doc_id)
        .is_some_and(|o| o.at >= q.queued_at && o.outcome == DEAD_LETTERED);
    !unroutable && now - q.queued_at < chrono::Duration::days(REQUEUE_AFTER_DAYS)
}

/// What a seeding run queued from one unit.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnitReport {
    pub batch: String,
    pub lccn: String,
    pub score: f64,
    pub queued: u64,
    /// The pages, in a dry run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages: Vec<String>,
}

/// What a seeding run did, or would have.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeedReport {
    pub plan: String,
    pub index_version: String,
    pub dry_run: bool,
    pub window: u64,
    pub refill_below: u64,
    /// `ocr-bulk`'s approximate message count, hidden ones included.
    pub queue_depth: u64,
    /// Bulk pages the ledger has outstanding (not timed out or waiting out
    /// an `unroutable` dead letter).
    pub in_flight: u64,
    /// Whether the queue was under the refill threshold: a run tops up only
    /// then (a dry run reports what it would queue either way).
    pub refill: bool,
    /// Pages of the plan's budget left before this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_left: Option<u64>,
    /// Pages queued again, by why.
    pub requeued: BTreeMap<String, u64>,
    /// In a dry run, the pages that would be queued again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requeue_pages: Vec<String>,
    /// New pages queued.
    pub queued: u64,
    pub units: Vec<UnitReport>,
    /// Pages left out, by why.
    pub skipped: BTreeMap<String, u64>,
    /// Batches whose units weren't worked out before the deadline; no new
    /// pages are queued until every batch has them.
    pub rank_pending: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<String>,
    /// With [`Source::Fetched`]: what the manifest and the list gave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched: Option<FetchedReport>,
}

/// The fetched source's counts.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchedReport {
    /// The manifest's index version and path.
    pub manifest: String,
    /// The fetched list read (`fetched/<version>/<node>.<stamp>.parquet`).
    pub list: String,
    /// Pages on the list.
    pub listed: u64,
    /// Keep-set pages in the languages, years and damage asked for.
    pub keep_pages: u64,
    /// Of those, the pages whose scans are on the list.
    pub on_list: u64,
}

fn skip(r: &mut SeedReport, why: &str, n: u64) {
    if n > 0 {
        *r.skipped.entry(why.to_owned()).or_default() += n;
    }
}

/// The lccns of the titles in the `ocr-ja` overlay.
async fn ja_overlay_titles(curated: &dyn ObjectStore) -> anyhow::Result<HashSet<String>> {
    Ok(curated
        .list(crate::ocr_ja::PREFIX)
        .await?
        .iter()
        .filter_map(|p| {
            let name = p.rsplit('/').next()?;
            Some(name.split_once('_')?.0.to_owned())
        })
        .collect())
}

/// Pages and titles every source skips.
struct Skips {
    /// Titles in the `ocr-ja` overlay.
    ja_titles: HashSet<String>,
    ja_latin: JaLatin,
    /// (doc_id, batch) copies the version hides.
    hidden: HashSet<(String, String)>,
}

impl Skips {
    async fn load(
        curated: &dyn ObjectStore,
        reference: &dyn ObjectStore,
        version: &str,
    ) -> anyhow::Result<Self> {
        let ja_latin: JaLatin = match reference.get(&format!("{version}/{JA_LATIN_FILE}")).await? {
            Some(b) => serde_json::from_slice(&b).context("reading ja_latin.json")?,
            None => JaLatin::default(),
        };
        let hidden: HashSet<(String, String)> = match reference
            .get(&format!("{version}/{DUPLICATES_FILE}"))
            .await?
        {
            Some(b) => serde_json::from_slice::<Vec<crate::dedup::Hidden>>(&b)
                .context("reading duplicates.json")?
                .into_iter()
                .map(|h| (h.doc_id, h.batch))
                .collect(),
            None => HashSet::new(),
        };
        Ok(Self {
            ja_titles: ja_overlay_titles(curated).await?,
            ja_latin,
            hidden,
        })
    }
}

/// [`Source::Fetched`]'s candidates: the manifest's keep set in the
/// fetcher's order (`usnm_ocr_contract::prefetch::keep_order`), in the
/// languages, years and damage asked for, that the newest fetched list in
/// `ocr-out` has, each with its message language. The manifest comes from
/// the curated store (the ingest job wrote it); the list came from a node,
/// so it only narrows the manifest's pages, never adds one.
async fn fetched_pages(
    curated: &dyn ObjectStore,
    out: &dyn ObjectStore,
    opts: &Options,
    version: &str,
    titles: &HashMap<String, Vec<String>>,
) -> anyhow::Result<(Vec<(ManifestRow, String)>, FetchedReport)> {
    let mv = if opts.manifest.is_empty() {
        version
    } else {
        opts.manifest.as_str()
    };
    let path = format!("{}/manifest.parquet", crate::ocr_prefetch::dir(mv, None));
    let bytes = curated.get(&path).await?.with_context(|| {
        format!("no prefetch manifest at {path}: run ocr-prefetch-manifest --version {mv}")
    })?;
    let filter = Filter::new(opts);
    let none = Vec::new();
    let language = |r: &ManifestRow| -> Option<String> {
        if let Some((from, to)) = filter.years {
            if !(from..=to).contains(&r.date.year()) {
                return None;
            }
        }
        if r.damage_rate < filter.min_damage {
            return None;
        }
        let langs = titles.get(&r.lccn).unwrap_or(&none);
        let l = message_language(r.lang.as_deref(), langs)?;
        filter.languages.contains(&l).then_some(l)
    };
    let batches = opts.batch_set();
    // Each issue's priority comes from all its keep pages, as the
    // fetcher's does, before the filters drop any.
    let mut best = HashMap::new();
    let mut rows = prefetch::decode_where(Bytes::from(bytes), |r| {
        prefetch::note_priority(&mut best, r);
        prefetch::set_of(r) == prefetch::Set::Keep
            && (batches.is_empty() || batches.contains(&r.batch))
            && language(r).is_some()
    })
    .with_context(|| format!("reading {path}"))?;
    prefetch::sort_keep(&mut rows, &best);

    // The newest by the store's creation time: a node chose the name.
    let lists = out.list_info(&fetched::dir(mv)).await?;
    let newest = lists
        .iter()
        .filter(|o| fetched::stamp_of(&o.path).is_some())
        .filter_map(|o| o.created.map(|c| (c, o)))
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)))
        .map(|(_, o)| o.clone())
        .with_context(|| {
            format!(
                "no fetched list under ocr-out/{}/: publish one from a node (usnm-ocr-worker publish-fetched)",
                fetched::dir(mv)
            )
        })?;
    // A node wrote it: check the size before reading it, and read only the
    // bytes listed.
    if newest.size > fetched::MAX_BYTES as u64 {
        bail!(
            "ocr-out/{} is {} bytes, more than {}",
            newest.path,
            newest.size,
            fetched::MAX_BYTES
        );
    }
    let newest_path = newest.path.clone();
    let body = out
        .get_if_match(&newest.path, &newest.etag, fetched::MAX_BYTES as u64)
        .await?
        .with_context(|| {
            format!("ocr-out/{newest_path} changed or went away; the next run reads it again")
        })?;
    let newest = newest_path;
    let list =
        fetched::decode(Bytes::from(body)).with_context(|| format!("reading ocr-out/{newest}"))?;
    if list.manifest != mv {
        bail!(
            "ocr-out/{newest} lists scans for {}, not {mv}",
            list.manifest
        );
    }
    let mut report = FetchedReport {
        manifest: path,
        list: newest,
        listed: list.len() as u64,
        keep_pages: rows.len() as u64,
        on_list: 0,
    };
    let pages: Vec<(ManifestRow, String)> = rows
        .into_iter()
        .filter(|r| list.contains(&r.batch, &r.doc_id))
        .filter_map(|r| language(&r).map(|l| (r, l)))
        .collect();
    report.on_list = pages.len() as u64;
    tracing::info!(
        manifest = report.manifest,
        list = report.list,
        listed = report.listed,
        keep_pages = report.keep_pages,
        on_list = report.on_list,
        "seeding from fetched scans"
    );
    Ok((pages, report))
}

/// A plan's finished units under these filters.
async fn load_done(curated: &dyn ObjectStore, opts: &Options) -> anyhow::Result<HashSet<DoneUnit>> {
    let mut out = HashSet::new();
    for path in curated.list(&done_dir(&opts.plan, opts)).await? {
        let Some(body) = curated.get(&path).await? else {
            continue;
        };
        let units: Vec<DoneUnit> =
            serde_json::from_slice(&body).with_context(|| format!("reading {path}"))?;
        out.extend(units);
    }
    Ok(out)
}

/// American Stories' text of one title in one batch, for the optional skip.
async fn american_stories_texts(
    curated: &dyn ObjectStore,
    source: &crate::american_stories::Source,
    batch: &RunBatch,
    lccn: &str,
) -> anyhow::Result<HashMap<String, String>> {
    let mut counts = crate::dedup::load_counts(curated, batch).await?;
    counts.retain(|l, _| l == lccn);
    Ok(source.load(curated, &counts).await?.texts)
}

/// The stores a seeding run uses.
pub struct Stores<'a> {
    pub curated: &'a dyn ObjectStore,
    pub reference: &'a dyn ObjectStore,
    pub queue: &'a Arc<dyn Queue>,
    /// The exchange account's `ocr-out`, for [`Source::Fetched`]'s list.
    pub out: Option<&'a dyn ObjectStore>,
}

/// `usnm-ingest ocr-seed`: one seeding run, under the `ocr-sync` lock
/// unless it is a dry run.
pub async fn run(
    state: &State,
    owner: &str,
    stores: &Stores<'_>,
    opts: &Options,
) -> anyhow::Result<SeedReport> {
    if !opts.on() {
        bail!("no seed plan (--seed-plan, USNM_OCR_SEED_PLAN): seeding is off");
    }
    opts.check()?;
    if opts.dry_run {
        return seed(state, stores, opts, None).await;
    }
    let lease = Lease::take(state, owner).await?;
    let result = seed(state, stores, opts, Some(&lease)).await;
    drop(lease);
    let unlocked = state.unlock(super::LOCK, owner).await;
    let report = result?;
    unlocked?;
    Ok(report)
}

/// One seeding run. `ocr-sync` calls it under its lock.
pub(super) async fn seed(
    state: &State,
    stores: &Stores<'_>,
    opts: &Options,
    lease: Option<&Lease>,
) -> anyhow::Result<SeedReport> {
    opts.check()?;
    let curated = stores.curated;
    let version = state
        .current_version()
        .await?
        .context("no published version to seed from")?;
    let list: VersionList = match curated.get(&ocr_damage::versions_path(&version)).await? {
        Some(b) => serde_json::from_slice(&b).context("reading the damage tables' version list")?,
        None => bail!(
            "{version} has no damage tables ({}): run ocr-damage first (12 Step 6)",
            ocr_damage::versions_path(&version)
        ),
    };
    if list.metric_version != usnm_core::damage::VERSION {
        bail!(
            "{version}'s damage tables are metric v{}, this build scores v{}: run ocr-damage again",
            list.metric_version,
            usnm_core::damage::VERSION
        );
    }
    let tables: Tables = list
        .batches
        .into_iter()
        .map(|b| (b.batch, (b.version, b.table)))
        .collect();
    let published = ocr_damage::version_batches(stores.reference, &version, &[]).await?;
    let by_name: HashMap<&str, &RunBatch> =
        published.iter().map(|b| (b.batch.as_str(), b)).collect();
    // A `name_verNN` filter must name the version that is published.
    for b in &opts.batches {
        let base = base_batch(b);
        let Some(p) = by_name.get(base) else {
            bail!("seed batch {b} isn't in the published version {version}");
        };
        if base != b && *b != format!("{base}_ver{:02}", p.curated.version) {
            bail!(
                "seed batch {b}: {version} publishes {base}_ver{:02}",
                p.curated.version
            );
        }
    }
    let wanted = opts.batch_set();
    let mut seeded: Vec<&RunBatch> = published
        .iter()
        .filter(|b| wanted.is_empty() || wanted.contains(&b.batch))
        .collect();
    seeded.sort_by(|a, b| a.batch.cmp(&b.batch));
    let titles = ocr_damage::title_languages(stores.reference, &version).await?;
    let ledger = ledger::load(curated).await?;
    let now = Utc::now();
    let run = run_id(now);
    let mut report = SeedReport {
        plan: opts.plan.clone(),
        index_version: version.clone(),
        dry_run: opts.dry_run,
        ..SeedReport::default()
    };

    // The window.
    report.window = if opts.window > 0 {
        opts.window
    } else {
        auto_window(import_rate(&ledger, now))
    };
    report.refill_below = if opts.refill_below > 0 {
        opts.refill_below
    } else {
        (report.window / 2).max(1)
    };
    report.queue_depth = stores
        .queue
        .approximate_count(BULK)
        .await
        .context("the seeder needs ocr-bulk's message count")?;
    report.in_flight = ledger
        .queued
        .values()
        .filter(|q| in_flight(q, &ledger, now))
        .count() as u64;
    report.refill = report.queue_depth < report.refill_below;
    if !report.refill && !opts.dry_run {
        tracing::info!(
            plan = opts.plan,
            queue_depth = report.queue_depth,
            refill_below = report.refill_below,
            "ocr-bulk is full enough; nothing seeded"
        );
        return Ok(report);
    }
    let outstanding = report.in_flight.max(report.queue_depth);
    let mut free = report
        .window
        .saturating_sub(outstanding)
        .min(MAX_PAGES as u64);
    let queued_at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let mut out: Vec<(Queued, OcrMessage)> = Vec::new();
    let attempt = |doc_id: &str| {
        ledger
            .attempts
            .get(&(doc_id.to_owned(), opts.plan.clone()))
            .copied()
            .unwrap_or(0)
            + 1
    };

    // Retries of the plan's own pages, oldest first.
    let mut mine: Vec<&Queued> = ledger
        .queued
        .values()
        .filter(|q| q.plan == opts.plan && q.reason == Reason::Bulk.as_str())
        .collect();
    mine.sort_by(|a, b| (a.queued_at, &a.doc_id).cmp(&(b.queued_at, &b.doc_id)));
    for q in mine {
        let Some((why, limited)) = retry_kind(q, &ledger, now) else {
            continue;
        };
        if limited {
            let unroutable = ledger.unroutable_cycles(&q.doc_id);
            let used = attempt(&q.doc_id)
                .saturating_sub(2)
                .saturating_sub(unroutable);
            if used >= MAX_REQUEUES {
                skip(&mut report, "retries_spent", 1);
                continue;
            }
        }
        if free == 0 {
            skip(&mut report, "window_full", 1);
            continue;
        }
        let Some(b) = by_name.get(q.batch.as_str()) else {
            skip(&mut report, "not_published", 1);
            continue;
        };
        let Ok(key) = PageKey::from_doc_id(&q.doc_id) else {
            skip(&mut report, "bad_doc_id", 1);
            continue;
        };
        let msg = OcrMessage {
            v: 1,
            page: key.to_string(),
            batch: q.batch.clone(),
            language: q.language.clone(),
            reason: Reason::Bulk,
            score: None,
            queued_at: queued_at.clone(),
        };
        if let Err(e) = msg.validate() {
            skip(&mut report, e.kind(), 1);
            continue;
        }
        *report.requeued.entry(why.to_owned()).or_default() += 1;
        if opts.dry_run {
            report.requeue_pages.push(q.doc_id.clone());
        }
        out.push((
            Queued {
                doc_id: q.doc_id.clone(),
                batch: q.batch.clone(),
                batch_version: b.curated.version,
                reason: Reason::Bulk.as_str().to_owned(),
                language: q.language.clone(),
                plan: opts.plan.clone(),
                attempt: attempt(&q.doc_id),
                queued_at: now,
            },
            msg,
        ));
        free -= 1;
    }

    // New pages, within the plan's budget.
    let plan_pages = ledger
        .attempts
        .keys()
        .filter(|(_, p)| *p == opts.plan)
        .count() as u64;
    report.budget_left = opts.max_pages.map(|m| m.saturating_sub(plan_pages));
    let mut slots = free.min(report.budget_left.unwrap_or(u64::MAX));
    let mut done_now: Vec<DoneUnit> = Vec::new();
    if slots > 0 && opts.source == Source::Fetched {
        let out_store = stores
            .out
            .context("the fetched seed source needs ocr-out (--out-url)")?;
        let (pages, fr) = fetched_pages(curated, out_store, opts, &version, &titles).await?;
        report.fetched = Some(fr);
        let skips = Skips::load(curated, stores.reference, &version).await?;
        let none = Vec::new();
        let mut ur: Option<UnitReport> = None;
        // The manifest holds one copy of each page (the copy the version
        // keeps); a page listed twice is still queued once.
        let mut seen: HashSet<String> = HashSet::new();
        for (row, language) in pages {
            if slots == 0 {
                break;
            }
            let langs = titles.get(&row.lccn).unwrap_or(&none);
            let doc_id = &row.doc_id;
            let why = if japanese(langs) || skips.ja_titles.contains(&row.lccn) {
                Some("japanese_title")
            } else if ledger.queued.contains_key(doc_id) {
                Some(if ledger.imported(doc_id) {
                    "imported"
                } else if ledger.outstanding(doc_id).is_some() {
                    "outstanding"
                } else {
                    "in_ledger"
                })
            } else if skips.ja_latin.pages.contains_key(doc_id) {
                Some("ja_latin")
            } else if skips.hidden.contains(&(doc_id.clone(), row.batch.clone()))
                || !seen.insert(doc_id.clone())
            {
                Some("duplicate")
            } else {
                match by_name.get(row.batch.as_str()) {
                    None => Some("not_published"),
                    Some(b) if b.curated.version != row.batch_version => Some("batch_version"),
                    Some(_) => None,
                }
            };
            if let Some(why) = why {
                skip(&mut report, why, 1);
                continue;
            }
            let Ok(key) = PageKey::from_doc_id(doc_id) else {
                skip(&mut report, "bad_doc_id", 1);
                continue;
            };
            let msg = OcrMessage {
                v: 1,
                page: key.to_string(),
                batch: row.batch.clone(),
                language: language.clone(),
                reason: Reason::Bulk,
                score: Some(row.damage_rate).filter(|s| (0.0..=1.0).contains(s)),
                queued_at: queued_at.clone(),
            };
            if let Err(e) = msg.validate() {
                skip(&mut report, e.kind(), 1);
                continue;
            }
            // Report by (batch, lccn) as the damage source does.
            if ur
                .as_ref()
                .is_none_or(|u| u.batch != row.batch || u.lccn != row.lccn)
            {
                if let Some(u) = ur.take() {
                    report.units.push(u);
                }
                ur = Some(UnitReport {
                    batch: row.batch.clone(),
                    lccn: row.lccn.clone(),
                    ..UnitReport::default()
                });
            }
            let u = ur.as_mut().expect("set above");
            u.queued += 1;
            u.score += (row.damage_rate - DAMAGE_FLOOR).max(0.0);
            if opts.dry_run {
                u.pages.push(doc_id.clone());
            }
            report.queued += 1;
            out.push((
                Queued {
                    doc_id: doc_id.clone(),
                    batch: row.batch.clone(),
                    batch_version: row.batch_version,
                    reason: Reason::Bulk.as_str().to_owned(),
                    language,
                    plan: opts.plan.clone(),
                    attempt: attempt(doc_id),
                    queued_at: now,
                },
                msg,
            ));
            slots -= 1;
        }
        if let Some(u) = ur.take() {
            report.units.push(u);
        }
    } else if slots > 0 {
        let mut cache = TableCache::new(curated, &tables);
        let ranked = rank(&mut cache, &seeded, &titles, opts, !opts.dry_run).await?;
        report.rank_pending = ranked.pending;
        if ranked.pending > 0 {
            tracing::info!(
                pending = ranked.pending,
                "seed ranks not worked out for every batch yet; no new pages this run"
            );
            slots = 0;
        }
        let done = load_done(curated, opts).await?;
        let Skips {
            ja_titles,
            ja_latin,
            hidden,
        } = Skips::load(curated, stores.reference, &version).await?;
        let american_stories = if opts.skip_american_stories {
            Some(crate::american_stories::Source::list(curated).await?)
        } else {
            None
        };
        let filter = Filter::new(opts);
        let none = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for unit in &ranked.units {
            if slots == 0 {
                break;
            }
            let id = DoneUnit {
                batch: unit.batch.clone(),
                batch_version: unit.batch_version,
                lccn: unit.lccn.clone(),
            };
            if done.contains(&id) {
                continue;
            }
            let Some(b) = by_name.get(unit.batch.as_str()) else {
                continue;
            };
            if b.curated.version != unit.batch_version {
                continue;
            }
            let langs = titles.get(&unit.lccn).unwrap_or(&none);
            if japanese(langs) || ja_titles.contains(&unit.lccn) {
                skip(&mut report, "japanese_title", unit.pages);
                done_now.push(id);
                continue;
            }
            let rows = cache.rows(&unit.batch).await?;
            let mut pages: Vec<(&Row, String)> = rows
                .iter()
                .filter(|r| r.lccn == unit.lccn && filter.damaged(r))
                .filter_map(|r| filter.language(r, langs).map(|l| (r, l)))
                .collect();
            pages.sort_by(|(a, _), (b, _)| {
                (a.date, a.edition, a.seq, &a.doc_id).cmp(&(b.date, b.edition, b.seq, &b.doc_id))
            });
            // Copies a batch sorting first keeps (it has the page with text).
            let mut kept_elsewhere: HashSet<String> = HashSet::new();
            for other in published
                .iter()
                .filter(|o| o.batch < unit.batch && o.curated.lccns.contains(&unit.lccn))
            {
                let other_rows = cache.rows(&other.batch).await?;
                kept_elsewhere.extend(
                    other_rows
                        .iter()
                        .filter(|r| r.lccn == unit.lccn && r.ok)
                        .map(|r| r.doc_id.clone()),
                );
            }
            let as_texts = match &american_stories {
                Some(src) => Some(american_stories_texts(curated, src, b, &unit.lccn).await?),
                None => None,
            };
            let as_langs: Vec<&str> = langs.iter().map(String::as_str).collect();
            let mut ur = UnitReport {
                batch: unit.batch.clone(),
                lccn: unit.lccn.clone(),
                score: unit.score,
                ..UnitReport::default()
            };
            let mut finished = true;
            for (row, language) in pages {
                if slots == 0 {
                    finished = false;
                    break;
                }
                let doc_id = &row.doc_id;
                let why = if ledger.queued.contains_key(doc_id) {
                    Some(if ledger.imported(doc_id) {
                        "imported"
                    } else if ledger.outstanding(doc_id).is_some() {
                        "outstanding"
                    } else {
                        "in_ledger"
                    })
                } else if ja_latin.pages.contains_key(doc_id) {
                    Some("ja_latin")
                } else if hidden.contains(&(doc_id.clone(), unit.batch.clone()))
                    || kept_elsewhere.contains(doc_id)
                    || seen.contains(doc_id)
                {
                    Some("duplicate")
                } else if as_texts
                    .as_ref()
                    .and_then(|t| t.get(doc_id))
                    .and_then(|text| usnm_core::damage::score(text, &as_langs).damage_rate)
                    .is_some_and(|r| r < opts.min_damage)
                {
                    Some("american_stories_undamaged")
                } else {
                    None
                };
                if let Some(why) = why {
                    skip(&mut report, why, 1);
                    continue;
                }
                let Ok(key) = PageKey::from_doc_id(doc_id) else {
                    skip(&mut report, "bad_doc_id", 1);
                    continue;
                };
                let msg = OcrMessage {
                    v: 1,
                    page: key.to_string(),
                    batch: unit.batch.clone(),
                    language: language.clone(),
                    reason: Reason::Bulk,
                    score: row.damage_rate.filter(|s| (0.0..=1.0).contains(s)),
                    queued_at: queued_at.clone(),
                };
                if let Err(e) = msg.validate() {
                    skip(&mut report, e.kind(), 1);
                    continue;
                }
                seen.insert(doc_id.clone());
                ur.queued += 1;
                if opts.dry_run {
                    ur.pages.push(doc_id.clone());
                }
                out.push((
                    Queued {
                        doc_id: doc_id.clone(),
                        batch: unit.batch.clone(),
                        batch_version: unit.batch_version,
                        reason: Reason::Bulk.as_str().to_owned(),
                        language,
                        plan: opts.plan.clone(),
                        attempt: attempt(doc_id),
                        queued_at: now,
                    },
                    msg,
                ));
                slots -= 1;
            }
            if finished {
                done_now.push(id);
            }
            if ur.queued > 0 {
                report.queued += ur.queued;
                report.units.push(ur);
            }
        }
    }

    if opts.dry_run {
        tracing::info!(
            plan = opts.plan,
            would_queue = report.queued,
            would_requeue = report.requeued.values().sum::<u64>(),
            units = report.units.len(),
            skipped = ?report.skipped,
            "ocr seed dry run"
        );
        return Ok(report);
    }
    if !out.is_empty() {
        let mut qr = QueueReport::default();
        enqueue::send_all(curated, stores.queue, lease, &run, now, out, &mut qr).await?;
        report.ledger = qr.ledger;
    }
    if !done_now.is_empty() {
        if let Some(l) = lease {
            l.check()?;
        }
        let path = format!("{}/{run}.json", done_dir(&opts.plan, opts));
        if !curated
            .put_new(&path, serde_json::to_vec(&done_now)?, "application/json")
            .await?
        {
            bail!("{path} already exists");
        }
    }
    tracing::info!(
        plan = opts.plan,
        queued = report.queued,
        requeued = report.requeued.values().sum::<u64>(),
        units = report.units.len(),
        window = report.window,
        in_flight = report.in_flight,
        queue_depth = report.queue_depth,
        skipped = ?report.skipped,
        ledger = ?report.ledger,
        "ocr seed finished"
    );
    Ok(report)
}
