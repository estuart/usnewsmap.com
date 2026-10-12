use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use usnm_ingest::activity::{self, Reporter};
use usnm_ingest::docs::{DocStore, FileDocs};
use usnm_ingest::merges::{self, MergeWait};
use usnm_ingest::release::{self, Published, PublishedUnrecorded, Release, TitlesLeft};
use usnm_ingest::sink::{IndexSink, JsonlSink, QuickwitNode, QuickwitSink};
use usnm_ingest::source::{self, ListedBatch};
use usnm_ingest::state::{BatchStatus, Outcome, State, Step};
use usnm_ingest::telemetry;
use usnm_ingest::titles;
use usnm_ingest::worker::{self, Worker};
use usnm_ingest::{cosmos::CosmosDocs, owner_id};
use usnm_store::credential;

/// US News Map ingest pipeline (docs/design/04 §4.4).
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(flatten)]
    stores: Stores,
    #[command(subcommand)]
    command: Command,
}

/// Why a page is queued for OCR.
#[derive(Clone, Copy, clap::ValueEnum)]
enum OcrReason {
    /// A page someone was shown (`ocr-priority`).
    User,
    /// The damage-ranked backlog (`ocr-bulk`).
    Bulk,
}

impl From<OcrReason> for usnm_ocr_contract::message::Reason {
    fn from(r: OcrReason) -> Self {
        match r {
            OcrReason::User => Self::User,
            OcrReason::Bulk => Self::Bulk,
        }
    }
}

/// The seeder's settings (12 §12.5 Step 10), for `ocr-sync`'s seeding
/// step and `ocr-seed`. Seeding is off while the plan is empty.
#[derive(Args, Clone, Debug)]
struct SeedArgs {
    /// The seeding plan, as the ledger records it (`pilot-deu`). Empty: off.
    #[arg(long, env = "USNM_OCR_SEED_PLAN")]
    seed_plan: Option<String>,
    /// Only these batches (base names or `name_verNN`), comma separated.
    #[arg(long, env = "USNM_OCR_SEED_BATCHES", value_delimiter = ',')]
    seed_batches: Vec<String>,
    /// ISO 639-3 languages to seed (the page's detected language, or its
    /// title's first for `und`), comma separated. Default: the Tesseract
    /// pilot's, ces, deu, eng, pol, rus and srp.
    #[arg(long, env = "USNM_OCR_SEED_LANGUAGES", value_delimiter = ',')]
    seed_languages: Vec<String>,
    /// Only issues in these years: `1890` or `1850-1919`.
    #[arg(long, env = "USNM_OCR_SEED_YEARS")]
    seed_years: Option<String>,
    /// The least damage a page needs to be queued.
    #[arg(long, env = "USNM_OCR_SEED_MIN_DAMAGE", default_value_t = usnm_ingest::ocr_local::seed::MIN_DAMAGE)]
    seed_min_damage: f64,
    /// The plan's budget: the most distinct pages it ever queues.
    #[arg(long, env = "USNM_OCR_SEED_MAX_PAGES")]
    seed_max_pages: Option<u64>,
    /// A pilot: one group (one language, or `--seed-batches`) and a budget
    /// of at most 10,000 pages.
    #[arg(long, env = "USNM_OCR_SEED_PILOT")]
    seed_pilot: bool,
    /// Also skip pages whose American Stories text scores under the
    /// minimum damage (12 §12.6 question 8; off by default).
    #[arg(long, env = "USNM_OCR_SEED_SKIP_AMERICAN_STORIES")]
    seed_skip_american_stories: bool,
    /// The most bulk pages outstanding at once. 0: about two days of pages,
    /// from the import records, at least 500.
    #[arg(long, env = "USNM_OCR_BULK_WINDOW", default_value_t = 0)]
    bulk_window: u64,
    /// Top `ocr-bulk` up only when it holds fewer messages than this.
    /// 0: half the window.
    #[arg(long, env = "USNM_OCR_BULK_REFILL_BELOW", default_value_t = 0)]
    bulk_refill_below: u64,
    /// Where new pages come from: `damage` (units ranked from the damage
    /// tables) or `fetched` (the prefetch manifest's keep set in the
    /// fetcher's order, only pages whose scans are on pve-big, from the
    /// newest list a node published to `ocr-out/fetched/`; 12 Step 11).
    #[arg(long, env = "USNM_OCR_SEED_SOURCE", default_value = "damage")]
    seed_source: usnm_ingest::ocr_local::seed::Source,
    /// With `--seed-source fetched`: the prefetch manifest's index version.
    /// Default: the published version.
    #[arg(long, env = "USNM_OCR_SEED_MANIFEST")]
    seed_manifest: Option<String>,
}

impl SeedArgs {
    fn options(&self, dry_run: bool) -> anyhow::Result<usnm_ingest::ocr_local::seed::Options> {
        let opts = usnm_ingest::ocr_local::seed::Options {
            plan: self.seed_plan.clone().unwrap_or_default().trim().to_owned(),
            batches: self
                .seed_batches
                .iter()
                .map(|b| b.trim().to_owned())
                .filter(|b| !b.is_empty())
                .collect(),
            languages: self
                .seed_languages
                .iter()
                .map(|l| l.trim().to_owned())
                .filter(|l| !l.is_empty())
                .collect(),
            years: usnm_ingest::ocr_local::seed::parse_years(
                self.seed_years.as_deref().unwrap_or_default(),
            )?,
            min_damage: self.seed_min_damage,
            max_pages: self.seed_max_pages,
            window: self.bulk_window,
            refill_below: self.bulk_refill_below,
            pilot: self.seed_pilot,
            skip_american_stories: self.seed_skip_american_stories,
            dry_run,
            deadline: None,
            source: self.seed_source,
            manifest: self
                .seed_manifest
                .clone()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        };
        if opts.on() {
            opts.check()?;
        }
        Ok(opts)
    }
}

#[derive(Args)]
struct Stores {
    /// Cosmos DB endpoint for pipeline state (Azure).
    #[arg(long, env = "USNM_COSMOS_ENDPOINT", conflicts_with = "state_file")]
    cosmos: Option<String>,
    /// Local JSON file for pipeline state (development).
    #[arg(long, env = "USNM_STATE_FILE")]
    state_file: Option<PathBuf>,
    /// Curated lake: a Blob container URL or a directory.
    #[arg(long, env = "USNM_CURATED_URL")]
    curated: String,
    /// Reference store (catalog, snapshots, current.json): a Blob container URL or a directory.
    #[arg(long, env = "USNM_REFERENCE_URL")]
    reference: String,
    /// Where curation retains each batch archive it downloads, and reads it
    /// back instead of LoC (`USNM_RETAIN_RAW`: a Blob container URL or a
    /// directory). Unset keeps nothing (ADR-0006).
    #[arg(long, env = "USNM_RAW_URL")]
    raw: Option<String>,
    /// Scratch space for the Quickwit writer node.
    #[arg(long, env = "USNM_WORK_DIR", default_value_os_t = std::env::temp_dir().join("usnm-ingest"))]
    work_dir: PathBuf,
}

#[derive(Args, Default)]
struct IndexTarget {
    /// Write JSONL indexes to this directory (the API's memory backend).
    #[arg(long, env = "USNM_INDEX_DIR")]
    index_dir: Option<PathBuf>,
    /// An already-running Quickwit writer node.
    #[arg(long, env = "USNM_QUICKWIT_URL")]
    quickwit_url: Option<String>,
    /// Run a Quickwit writer node from this binary for the length of the release.
    #[arg(long, env = "USNM_QUICKWIT_BIN")]
    quickwit_bin: Option<PathBuf>,
    /// Metastore URI for the writer node (`azure://qw-index` in Azure).
    #[arg(long, env = "USNM_QUICKWIT_METASTORE")]
    quickwit_metastore: Option<String>,
    /// Where new indexes are created (`azure://qw-index` in Azure).
    #[arg(long, env = "USNM_QUICKWIT_INDEX_ROOT")]
    quickwit_index_root: Option<String>,
    #[arg(long, env = "USNM_QUICKWIT_PORT", default_value_t = 7380)]
    quickwit_port: u16,
    /// Seconds to wait for the new index's merges before giving up without
    /// publishing (Quickwit targets).
    #[arg(long, env = "USNM_MERGE_TIMEOUT_SECS", default_value_t = merges::DEFAULT_TIMEOUT_SECS)]
    merge_timeout_secs: u64,
    /// Refuse to start a writer node with less free disk than this (GiB) in
    /// the work directory: the scratch volume isn't mounted, or a previous
    /// run's files fill it.
    #[arg(long, env = "USNM_WORK_MIN_FREE_GIB", default_value_t = 0)]
    min_free_gib: u64,
}

#[derive(Subcommand)]
enum Command {
    /// Record listed batches and queue new ones and new versions.
    Enqueue {
        /// Batch list: JSON `[{name, url, sha256?, ocr_source?}]` or LoC's
        /// collection JSON, as a path or https URL.
        #[arg(long, default_value = source::LOC_DATASETS)]
        list: String,
        /// Only these batches (names with their version suffix), e.g. to try a few.
        #[arg(long, value_delimiter = ',')]
        batches: Vec<String>,
        /// Queue the listed batches again even if they are curated (not one
        /// being curated now), for a re-curation: e.g. to retain their
        /// archives (`USNM_RAW_URL`). Needs `--batches`.
        #[arg(long, requires = "batches")]
        force: bool,
    },
    /// Claim and curate queued batches until none are left.
    Curate {
        #[arg(long)]
        max_batches: Option<usize>,
        /// Enqueue from `--list` first (safe to run in many workers at once).
        #[arg(long)]
        enqueue: bool,
        #[arg(long, default_value = source::LOC_DATASETS)]
        list: String,
        /// Seconds between bulk downloads across every worker (LoC allows 10
        /// per 10 minutes per IP); 0 doesn't pace.
        #[arg(long, default_value_t = worker::FETCH_INTERVAL_SECS)]
        fetch_interval_secs: u32,
        /// Seconds after the start to stop claiming batches and waiting for
        /// download slots. A batch already downloading is finished (within
        /// the 45-minute watchdog), then the command exits 0; the rest stays
        /// queued for the next run. Keep it plus 45 minutes under the job's
        /// replica timeout.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
    },
    /// Build a new index and reference snapshot from curated batches, then publish.
    Release {
        /// Rebuild everything into a new base index (compaction).
        #[arg(long)]
        full: bool,
        /// Label the version as synthetic demo data (the fixture batches).
        #[arg(long)]
        synthetic: bool,
        /// Index American Stories' text with LoC's (#218, 04 §4.9): what
        /// `jaocr.py american-stories-write` wrote for the finished years.
        /// The first release with it builds a full base. Keep it on once a
        /// version has the text: a release without it publishes a version
        /// whose searches leave the text out.
        #[arg(long)]
        american_stories: bool,
        /// Give the pages LoC ships without text a main-index document with
        /// the Latin-script text our Japanese OCR read on them (#203, 04
        /// §4.8), so English searches reach their English ads and sections.
        /// The release's new main index takes the pages no index of the
        /// version holds yet; without it, a full release leaves them out.
        #[arg(long)]
        ja_latin: bool,
        /// Weigh our own OCR from `ocr-local` against LoC's text (12 §12.5
        /// Step 9): on the pages of the batches this release indexes, ours
        /// replaces LoC's where rule v1 prefers it. Published batches take
        /// it at the next full release.
        #[arg(long)]
        ocr_local: bool,
        /// Search the Congress pages curated under `congress/` (#229, 04
        /// §4.10): the version gets a Congress index set, rebuilt when the
        /// curated units changed and kept otherwise. Off: no Congress set.
        #[arg(long, env = "USNM_CONGRESS")]
        congress: bool,
        /// Lay a full base out by decade (#123, 05 §5.5.5), so date-limited
        /// searches skip the other decades' splits. Only a full base takes
        /// it; deltas follow the published version.
        #[arg(long)]
        partition_decade: bool,
        /// Search a full base's texts, LoC's and American Stories', in one
        /// field (#283, 05 §5.5.7), so a search looks each word up once.
        /// Only a full base takes it; deltas follow the published version.
        #[arg(long)]
        single_text_field: bool,
        /// Give a full base's pages their own languages, found from their
        /// words (#289, 05 §5.5.8), and the version page-language baselines,
        /// so `lang=` filters by them. Only a full base takes it; deltas
        /// follow the published version.
        #[arg(long)]
        page_language: bool,
        /// Pages per split of a full base, in place of the index template's
        /// 30,000 (#283): fewer, larger splits for a cold search to open.
        /// Bounded by the writer's memory and scratch share (08 §8.4). Only
        /// a full base takes it; deltas keep the template's.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..usnm_ingest::sink::MAX_SPLIT_DOCS))]
        split_docs: Option<u64>,
        #[command(flatten)]
        target: IndexTarget,
    },
    /// Fetch LoC's records for titles with pages, then rebuild the catalog (`geocode`).
    TitlesSync {
        /// Titles to fetch: those named in this batch list (LoC's listing
        /// names them), plus those in curated batches.
        #[arg(long, default_value = source::LOC_DATASETS)]
        list: String,
        /// Only these LCCNs instead.
        #[arg(long, value_delimiter = ',')]
        lccns: Vec<String>,
        /// Re-fetch records that are already cached.
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value = titles::LOC_ITEMS)]
        items: String,
        /// Seconds after the start to stop. Until then, when LoC rate limits
        /// the sync, it sends nothing for 65 minutes and resumes more slowly;
        /// without it, the first block stops the sync. Either way the
        /// command fails if titles are left, and the next run continues.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
    },
    /// Rebuild `catalog/titles.json` and `places.json` from the cached records
    /// and `overrides/places.json` (no network).
    Geocode,
    /// Save titles-sync's record cache and the catalog to the raw store
    /// (`USNM_RAW_URL`) as `titles/{time}.json`, unless the newest copy
    /// there is the same (docs/operations.md, "Dev environment lifecycle").
    /// titles-sync does this itself after each run; this is for doing it by
    /// hand, e.g. before a teardown.
    TitlesArchive,
    /// Start a new environment's record cache and catalog from the newest
    /// copy in the raw store (`USNM_RAW_URL`), so it needs no fresh
    /// titles-sync. Writes nothing if the environment has a record cache
    /// already; never replaces a file.
    TitlesSeed,
    /// Copy retained archives and kept batch lists between raw stores, e.g.
    /// from this environment's own `raw` container into the archival
    /// account (docs/operations.md, "Archival storage"). Each archive is
    /// checked against its manifest's sha256; one already there is skipped.
    ArchiveCopy {
        /// The source raw store (a Blob container URL or a directory).
        #[arg(long)]
        from: String,
        /// The destination (default: USNM_RAW_URL).
        #[arg(long, env = "USNM_RAW_URL")]
        to: String,
        /// The batches (with their version suffix) to copy.
        #[arg(long, value_delimiter = ',', required = true)]
        batches: Vec<String>,
    },
    /// Report pages that ship in more than one curated batch (04 §4.7):
    /// JSON on stdout. Reads every batch's counts and the parts of the
    /// batches that share title-days; changes nothing.
    Duplicates,
    /// Delete the Quickwit indexes that neither the serving version nor its
    /// rollback names (#245): older versions' and stopped rebuilds'. Without
    /// `--delete`, only lists them with their sizes (a dry run). Takes the
    /// writer lock like a release, and refuses if `current.json` can't be
    /// read (docs/operations.md, "Prune old index versions").
    Prune {
        /// Delete the listed indexes. Without it nothing is deleted.
        #[arg(long)]
        delete: bool,
        /// The Quickwit node on the index store: `--quickwit-bin` (with its
        /// metastore and index root), or for a dry run `--quickwit-url`.
        #[command(flatten)]
        target: IndexTarget,
    },
    /// Score every curated page of an index version with the OCR damage
    /// metric (12 §12.5 Step 6, #296): tables under
    /// `audit/damage/v<VERSION>/` in the curated store, one per batch
    /// version, skipping those already scored. With `--sample-pct` and
    /// `--compare-csv`, the parity gate against the Python audit instead
    /// (writes no tables); with `--parity`, a parity file from
    /// `jaocr.py damage-parity`. Logs counts and doc_ids, never text.
    OcrDamage {
        /// The index version whose batches to score (`{version}/batches.json`).
        #[arg(
            long = "version",
            value_name = "INDEX_VERSION",
            required_unless_present = "parity"
        )]
        index_version: Option<String>,
        /// Only these batches (`name` or `name_verNN`). The version's list
        /// of tables is written only by a run over every batch.
        #[arg(long, value_delimiter = ',')]
        batches: Vec<String>,
        /// The gate: score only the pages the audit samples at this
        /// percentage, and compare them with `--compare-csv`.
        #[arg(long, requires_all = ["index_version", "compare_csv"])]
        sample_pct: Option<f64>,
        /// The audit's per-page CSV (`.csv.gz`, a curated path) to compare the sample with.
        #[arg(long, requires = "sample_pct")]
        compare_csv: Option<String>,
        /// A parity file (`.jsonl.zst`, a curated path) to score again.
        #[arg(long)]
        parity: Option<String>,
        /// Seconds after the start to stop starting batches; the next run
        /// continues. Keep it under the job's replica timeout.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
        /// Batches scored at once (default: the CPUs).
        #[arg(long)]
        parallel: Option<usize>,
    },
    /// The scan prefetch manifest (#368): the pages of an index version
    /// above `--min-damage` in the damage tables, as Parquet in issue
    /// order, and a summary by language, decade and batch, under
    /// `audit/damage/v2/prefetch/<version>/` in the curated store. Logs
    /// counts, never text.
    OcrPrefetchManifest {
        /// The index version whose damage tables to read.
        #[arg(long = "version", value_name = "INDEX_VERSION")]
        index_version: String,
        /// Before the damage pass has listed every table: read the batches
        /// that have theirs, into `partial-<stamp>/`.
        #[arg(long)]
        partial: bool,
        /// Pages with damage above this go in.
        #[arg(long, default_value_t = usnm_core::damage::BADLY_DAMAGED)]
        min_damage: f64,
        /// The exchange account's `ocr-out` container (a Blob container URL
        /// or a directory): also copy the manifest and summary to
        /// `manifests/<version>/` there, for the nodes.
        #[arg(long)]
        exchange_url: Option<String>,
    },
    /// Pick up the OCR nodes' output (12 §12.5 Step 8, #296): drain the
    /// dead letters, check each part in the exchange account's `ocr-out`
    /// and its ALTO, import what passes into `ocr-local/` in the curated
    /// store, move the rest to `ocr-quarantine`, delete the sources and
    /// write `status/ocr-local.json`. Holds the `ocr-sync` lock. Logs page
    /// ids and reason codes, never text.
    OcrSync {
        /// The exchange account's queue endpoint (`OCR_QUEUE_ENDPOINT`).
        #[arg(long, env = "USNM_OCR_QUEUE_URL")]
        queue_url: Option<String>,
        /// The exchange account's `ocr-out` container: a Blob container URL or a directory.
        #[arg(long, env = "USNM_OCR_OUT_URL")]
        out_url: Option<String>,
        /// The exchange account's `ocr-quarantine` container: a Blob container URL or a directory.
        #[arg(long, env = "USNM_OCR_QUARANTINE_URL")]
        quarantine_url: Option<String>,
        /// The most parts checked in one run; the rest wait for the next.
        #[arg(long, default_value_t = 2000)]
        max_parts: usize,
        /// Parts per import (each import writes and deletes on its own).
        #[arg(long, default_value_t = 200)]
        chunk: usize,
        /// The most dead letters drained in one run.
        #[arg(long, default_value_t = 5000)]
        max_dead_letters: usize,
        /// Only apply rule v1 to the stored scores of all of `ocr-local`
        /// and write `ocr-local/report/<run>.json`; picks nothing up.
        #[arg(long, conflicts_with = "from_quarantine")]
        report: bool,
        /// First put the rejects with `--reason` back where they came
        /// from, so this run checks them again.
        #[arg(long, requires = "reason")]
        from_quarantine: bool,
        /// The reject reason code to replay (as in the quarantine records).
        #[arg(long, requires = "from_quarantine")]
        reason: Option<String>,
        /// Seconds after the start to stop starting imports. Keep it under
        /// the job's replica timeout.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
        /// With a seed plan, top `ocr-bulk` up after the imports (Step 10).
        #[command(flatten)]
        seed: SeedArgs,
    },
    /// Fill `ocr-bulk` with the pages LoC read worst, ranked by Step 6's
    /// damage tables (12 §12.5 Step 10), as `ocr-sync`'s seeding step does,
    /// once. `--dry-run` prints what it would queue as JSON and writes
    /// nothing; otherwise it holds the `ocr-sync` lock. Run as an execution
    /// of the pickup job: only `id-usnm-ingest` can add messages.
    OcrSeed {
        #[arg(long, env = "USNM_OCR_QUEUE_URL")]
        queue_url: String,
        /// The exchange account's `ocr-out`, for `--seed-source fetched`.
        #[arg(long, env = "USNM_OCR_OUT_URL")]
        out_url: Option<String>,
        #[command(flatten)]
        seed: SeedArgs,
        /// Print what would be queued; write and send nothing.
        #[arg(long)]
        dry_run: bool,
        /// Seconds after the start to stop working out ranks; the next run
        /// goes on from there.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
    },
    /// Queue chosen pages for OCR (12 §12.5 Step 8): finds each page in the
    /// published version, writes a ledger row for it and puts its message
    /// on `ocr-priority` (`--reason user`) or `ocr-bulk` (`bulk`). Run as
    /// an ingest job execution: only `id-usnm-ingest` can add messages.
    OcrEnqueue {
        #[arg(long, env = "USNM_OCR_QUEUE_URL")]
        queue_url: String,
        /// Page doc_ids, comma separated (`sn83045462_1890-05-03_ed-1_seq-7`).
        #[arg(long, value_delimiter = ',', required = true)]
        pages: Vec<String>,
        #[arg(long, value_enum)]
        reason: OcrReason,
        /// The plan name recorded in the ledger.
        #[arg(long, default_value = "manual")]
        plan: String,
        /// Look for the pages only in this base batch.
        #[arg(long)]
        batch: Option<String>,
        /// Queue pages the ledger already has outstanding: after a run that
        /// wrote its ledger rows but failed to put some messages.
        #[arg(long)]
        include_outstanding: bool,
        /// Count what would be queued; write and send nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Queue dead-lettered pages again by their cause and kind (12 §12.5
    /// Step 8), from the dead letters `ocr-sync` drained into `ocr-local`.
    OcrRequeue {
        #[arg(long, env = "USNM_OCR_QUEUE_URL")]
        queue_url: String,
        /// `dequeue_count`, `invalid`, `permanent` or `unroutable`.
        #[arg(long)]
        cause: String,
        /// For `invalid`: the check that failed.
        #[arg(long)]
        kind: Option<String>,
        /// Only dead letters drained on or after this UTC day (YYYY-MM-DD).
        #[arg(long)]
        since: Option<chrono::NaiveDate>,
        /// The plan name recorded in the ledger.
        #[arg(long, default_value = "requeue")]
        plan: String,
        #[arg(long, default_value_t = 1000)]
        max_pages: usize,
        #[arg(long)]
        dry_run: bool,
    },
    /// Fetch the Congressional Globe from UNT's page OCR into curated
    /// Congress pages (#229, 04 §4.10): list the collection over OAI-PMH,
    /// leave out the items another holds, then fetch each item's pages at
    /// UNT's crawl delay and write a curated attempt per item. Items with a
    /// finished attempt are skipped. With USNM_RAW_URL every response is
    /// kept there and read back on a later run. Holds the
    /// `congress-globe` lock. Prints what it did as JSON.
    CongressGlobe {
        /// Only these items (`metadc29274`), comma separated.
        #[arg(long, value_delimiter = ',')]
        items: Vec<String>,
        /// Fetch items that already have a finished attempt again.
        #[arg(long)]
        refetch: bool,
        /// Stop after this many items.
        #[arg(long)]
        max_items: Option<usize>,
        /// Seconds after the start to stop: an item stopped between pages
        /// writes nothing, and the next run resumes it (from the raw store's
        /// copies, when there is one). Keep it under the job's replica timeout.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
        /// Seconds between requests; UNT's robots.txt asks for 3, the least
        /// this takes.
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(3..=60))]
        crawl_delay_secs: u64,
        /// Where to fetch from (UNT; another host only for a copy of its responses).
        #[arg(long, default_value = usnm_ingest::congress::globe::UNT)]
        unt_url: String,
    },
    /// Fetch the Congressional Serial Set from GovInfo into curated Congress
    /// pages (#229, 04 §4.10): read the yearly sitemaps, then fetch each
    /// volume's documents (MODS and PDF, about one request a second), read
    /// the PDFs page by page with pdftotext and delete them, and write a
    /// curated attempt per volume. Volumes with a finished attempt are
    /// skipped unless their packages changed since (ids and sitemap
    /// `lastmod`s). With
    /// USNM_RAW_URL the MODS and page texts are kept there and read back on
    /// a later run. Holds the `congress-serialset` lock. Prints what it did
    /// as JSON.
    CongressSerialset {
        /// Only the volumes with a package in these years' sitemaps
        /// (`1820`), comma separated; default all. Every sitemap is still
        /// read, and each volume is made of its packages in all of them.
        #[arg(long, value_delimiter = ',')]
        years: Vec<u16>,
        /// Only these serial numbers' volumes (`33`), comma separated.
        #[arg(long, value_delimiter = ',')]
        serials: Vec<u32>,
        /// Fetch volumes that already have a finished attempt again.
        #[arg(long)]
        refetch: bool,
        /// Stop after this many volumes.
        #[arg(long)]
        max_volumes: Option<usize>,
        /// Seconds after the start to stop: a volume stopped between
        /// documents writes nothing, and the next run resumes it (from the
        /// raw store's copies, when there is one). Keep it under the job's
        /// replica timeout.
        #[arg(long)]
        max_runtime_secs: Option<u64>,
        /// Seconds between requests; at least 1.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..=60))]
        delay_secs: u64,
        /// Where to fetch from (GovInfo; another host only for a copy of its responses).
        #[arg(long, default_value = usnm_ingest::congress::serialset::GOVINFO)]
        govinfo_url: String,
        /// The pdftotext binary (poppler-utils).
        #[arg(long, env = "USNM_PDFTOTEXT", default_value = "pdftotext")]
        pdftotext: PathBuf,
    },
    /// Enqueue (if a list is given), curate everything queued, then release.
    Run {
        /// As for `enqueue`: LoC's listing unless another list is given.
        #[arg(long, default_value = source::LOC_DATASETS)]
        list: String,
        /// Only these batches from the list.
        #[arg(long, value_delimiter = ',')]
        batches: Vec<String>,
        #[arg(long)]
        full: bool,
        #[arg(long)]
        synthetic: bool,
        /// As for `release`.
        #[arg(long)]
        american_stories: bool,
        /// As for `release`.
        #[arg(long)]
        ja_latin: bool,
        /// As for `release`.
        #[arg(long)]
        ocr_local: bool,
        /// As for `release`.
        #[arg(long, env = "USNM_CONGRESS")]
        congress: bool,
        /// As for `release`.
        #[arg(long)]
        partition_decade: bool,
        /// As for `release`.
        #[arg(long)]
        single_text_field: bool,
        /// As for `release`.
        #[arg(long)]
        page_language: bool,
        /// As for `release`.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..usnm_ingest::sink::MAX_SPLIT_DOCS))]
        split_docs: Option<u64>,
        /// As `curate --max-runtime-secs`, counted from the start of `run`:
        /// batches still queued then wait for the next run, and what was
        /// curated is released. Leave room for titles-sync and the release
        /// under the job's replica timeout.
        #[arg(long)]
        curate_max_runtime_secs: Option<u64>,
        /// As `titles-sync --max-runtime-secs`, counted from the start of
        /// `run`. A full release (asked for, or forced) goes ahead only once
        /// titles-sync has fetched every title LoC has, so a rebuild leaves
        /// no batch out for a missing title; otherwise it fails and the next
        /// run continues the sync.
        #[arg(long)]
        titles_max_runtime_secs: Option<u64>,
        #[command(flatten)]
        target: IndexTarget,
    },
}

fn state(s: &Stores) -> anyhow::Result<State> {
    let docs: Arc<dyn DocStore> = match (&s.cosmos, &s.state_file) {
        (Some(endpoint), _) => Arc::new(CosmosDocs::new(
            endpoint,
            usnm_state::DATABASE,
            credential::from_env_for(credential::COSMOS_RESOURCE),
        )?),
        (None, Some(path)) => Arc::new(FileDocs::open(path)?),
        (None, None) => bail!("set --cosmos (Azure) or --state-file (local)"),
    };
    Ok(State::new(docs))
}

/// Read a batch list. With a raw store (`USNM_RAW_URL`), a copy of a remote
/// list as fetched is kept there too, under `listings/` by time.
async fn read_list(list: &str, raw: Option<&str>) -> anyhow::Result<Vec<ListedBatch>> {
    let dest = tempfile::NamedTempFile::new()?;
    let sha = source::fetch(list, dest.path()).await?;
    let bytes = std::fs::read(dest.path())?;
    if let (Some(raw), true) = (raw, list.starts_with("https://")) {
        let path = format!(
            "listings/{}-{}.json",
            chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
            &sha[..12]
        );
        usnm_store::open(raw)?
            .put(&path, bytes.clone(), "application/json")
            .await?;
        tracing::info!(list, path, "batch list retained");
    }
    source::parse_list(&bytes)
}

async fn enqueue(
    state: &State,
    list: &str,
    only: &[String],
    force: bool,
    raw: Option<&str>,
) -> anyhow::Result<Vec<ListedBatch>> {
    let mut batches = read_list(list, raw).await?;
    if !only.is_empty() {
        batches.retain(|b| only.contains(&b.name));
        let found: Vec<&str> = batches.iter().map(|b| b.name.as_str()).collect();
        let missing: Vec<&String> = only
            .iter()
            .filter(|o| !found.contains(&o.as_str()))
            .collect();
        if !missing.is_empty() {
            bail!("not in the list: {missing:?}");
        }
    }
    let report = if force {
        source::requeue(state, &batches).await?
    } else {
        source::enqueue(state, &batches).await?
    };
    tracing::info!(?report, "enqueued");
    Ok(batches)
}

/// Fetch missing title records for `listed` titles and every curated batch's
/// titles, then rebuild the catalog from what is cached. The report says
/// whether the sync stopped early (rate limited, or at `deadline`), keeping
/// what it fetched.
async fn titles_sync(
    cli: &Stores,
    state: &State,
    listed: impl IntoIterator<Item = String>,
    refresh: bool,
    items: &str,
    deadline: Option<tokio::time::Instant>,
    report: &Reporter,
) -> anyhow::Result<titles::SyncReport> {
    let mut lccns: BTreeSet<String> = listed.into_iter().collect();
    for (b, _) in state.batches(&[BatchStatus::Curated]).await? {
        lccns.extend(b.curated.into_iter().flat_map(|c| c.lccns));
    }
    let reference = usnm_store::open(&cli.reference)?;
    let report = titles::sync(
        reference.as_ref(),
        &lccns,
        &titles::title_records()?,
        refresh,
        items,
        &titles::Pacing::loc(deadline).reporting(report.clone()),
    )
    .await?;
    tracing::info!(?report, "title records");
    geocode(reference.as_ref()).await?;
    // Keep what the sync has outside the environment (#275). A failed copy
    // doesn't fail the sync: the next run tries again.
    if let Some(raw) = &cli.raw {
        if let Err(e) = titles_archive(reference.as_ref(), raw).await {
            tracing::warn!(error = %format!("{e:#}"), "the title records weren't copied to the raw store");
        }
    }
    Ok(report)
}

/// Copy the record cache and catalog to the raw store (`titles/{time}.json`).
async fn titles_archive(reference: &dyn usnm_store::ObjectStore, raw: &str) -> anyhow::Result<()> {
    let saved = usnm_ingest::titles_archive::save(
        reference,
        usnm_store::open(raw)?.as_ref(),
        chrono::Utc::now(),
    )
    .await?;
    tracing::info!(?saved, "title records in the raw store");
    Ok(())
}

/// What titles-sync left undone, if anything: it stopped early, or some
/// fetches failed (retried next run). Titles LoC doesn't have (404) count
/// as done: no run will find them.
fn titles_left(report: &titles::SyncReport) -> Option<String> {
    if !report.finished() {
        return Some(unfinished(report));
    }
    (!report.failed.is_empty()).then(|| {
        format!(
            "titles-sync couldn't fetch {} of {} titles ({}); the next run retries them",
            report.failed.len(),
            report.wanted,
            report
                .failed
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Why titles-sync stopped early, for an error or a warning.
fn unfinished(report: &titles::SyncReport) -> String {
    let why = if report.throttled {
        "LoC rate limited titles-sync"
    } else {
        "titles-sync reached its deadline"
    };
    format!(
        "{why} with {} of {} titles left; the cache kept what was fetched, and the next run continues",
        report.left, report.wanted
    )
}

async fn geocode(reference: &dyn usnm_store::ObjectStore) -> anyhow::Result<()> {
    let report = titles::geocode(reference).await?;
    if !report.unresolved.is_empty() {
        tracing::warn!(unresolved = ?report.unresolved, "titles with no state were left out");
    }
    if report.gazetteer_over_loc > 0 {
        tracing::info!(
            places = report.gazetteer_over_loc,
            list = ?report.gazetteer_over_loc_places,
            "LoC points more than 100 km from the gazetteer's town of the name (not ambiguous); the gazetteer's used"
        );
    }
    if report.disagreements > 0 {
        tracing::warn!(
            places = report.disagreements,
            furthest = ?report.disagreement_examples,
            "LoC points more than 25 km from the gazetteer, kept; review them (catalog/overrides/places.json)"
        );
    }
    if report.loc_apart > 0 {
        tracing::warn!(
            places = report.loc_apart,
            furthest = ?report.loc_apart_examples,
            "places at the median of LoC points more than 25 km apart; review them (catalog/overrides/places.json)"
        );
    }
    tracing::info!(?report, "catalog");
    Ok(())
}

async fn curate(
    cli: &Stores,
    state: &State,
    max: Option<usize>,
    fetch_interval_secs: u32,
    deadline: Option<tokio::time::Instant>,
) -> anyhow::Result<()> {
    let worker = Worker {
        state: state.clone(),
        curated: usnm_store::open(&cli.curated)?,
        owner: owner_id(),
        lease: chrono::Duration::hours(2),
        fetch_interval: (fetch_interval_secs > 0)
            .then_some(chrono::Duration::seconds(i64::from(fetch_interval_secs))),
        batch_limit: worker::BATCH_LIMIT,
        deadline,
        raw: cli.raw.as_deref().map(usnm_store::open).transpose()?,
    };
    let n = worker.run(max).await?;
    tracing::info!(curated = n, "curation finished");
    Ok(())
}

// The command's options, passed through.
#[allow(clippy::too_many_arguments)]
async fn release(
    cli: &Stores,
    state: &State,
    full: bool,
    synthetic: bool,
    american_stories: bool,
    ja_latin: bool,
    ocr_local: bool,
    congress: bool,
    partition_decade: bool,
    single_text_field: bool,
    page_language: bool,
    split_docs: Option<u64>,
    titles_left: Option<String>,
    t: &IndexTarget,
    report: &Reporter,
) -> anyhow::Result<Option<Published>> {
    let r = Release {
        state: state.clone(),
        curated: usnm_store::open(&cli.curated)?,
        reference: usnm_store::open(&cli.reference)?,
        owner: owner_id(),
        full,
        synthetic,
        now: chrono::Utc::now(),
        titles_left,
        american_stories,
        ja_latin,
        ocr_local,
        congress,
        partition_decade,
        single_text_field,
        page_language,
        split_docs,
    };
    // Held from before the writer node starts until after it stops, and
    // released on every path.
    let lease = r.lock().await?;
    let result = release_locked(cli, &r, &lease, t, report).await;
    let unlocked = r.unlock(lease).await;
    let result = result.and_then(|p| unlocked.map(|()| p));
    let published = result?;
    match &published {
        Some(p) => {
            tracing::info!(version = %p.index_version, docs = p.docs, pages = p.pages, full = p.full, "released")
        }
        None => tracing::info!("nothing to release"),
    }
    Ok(published)
}

/// Start the index target (and writer node), release, stop the node.
async fn release_locked(
    cli: &Stores,
    r: &Release,
    lease: &usnm_ingest::release::WriterLease,
    t: &IndexTarget,
    report: &Reporter,
) -> anyhow::Result<Option<Published>> {
    let mut node = None;
    let wait = MergeWait {
        timeout: std::time::Duration::from_secs(t.merge_timeout_secs),
        report: report.clone(),
        ..MergeWait::default()
    };
    let mut sink: Box<dyn IndexSink> = match (&t.index_dir, &t.quickwit_url, &t.quickwit_bin) {
        (Some(dir), None, None) => Box::new(JsonlSink::new(dir)),
        (None, Some(url), None) => Box::new(QuickwitSink::new(url, root(t)?)?.merges(wait)),
        (None, None, Some(bin)) => {
            let metastore = t
                .quickwit_metastore
                .as_deref()
                .context("--quickwit-bin needs --quickwit-metastore")?;
            let dir = cli.work_dir.join("quickwit");
            std::fs::create_dir_all(&dir)?;
            remove_stale_spill(&dir)?;
            check_free_disk(&dir, t.min_free_gib)?;
            let n = QuickwitNode::start(bin, &dir, t.quickwit_port, metastore, root(t)?).await?;
            let sink = QuickwitSink::new(&n.url, root(t)?)?
                .watching(&n)
                .merges(wait);
            node = Some(n);
            Box::new(sink)
        }
        _ => bail!("choose one of --index-dir, --quickwit-url or --quickwit-bin"),
    };
    let result = r.run_reporting(sink.as_mut(), lease, report).await;
    if let Some(n) = node {
        n.stop().await?;
    }
    result
}

/// Remove the decade files (`decade_order::SPILL_DIR`) a release that
/// stopped left in `dir`, whatever this release builds: only a partitioned
/// base would remove them otherwise, and the free-disk check must see the
/// space they take.
fn remove_stale_spill(dir: &std::path::Path) -> anyhow::Result<()> {
    let spill = dir.join(usnm_ingest::decade_order::SPILL_DIR);
    if spill.exists() {
        tracing::info!(dir = %spill.display(), "removing a previous release's decade files");
        std::fs::remove_dir_all(&spill).with_context(|| format!("removing {}", spill.display()))?;
    }
    Ok(())
}

/// Fail before indexing anything if `dir` has less than `min_gib` free. A
/// previous writer's data in `dir/qwdata` is about to be removed, so it
/// counts as free.
fn check_free_disk(dir: &std::path::Path, min_gib: u64) -> anyhow::Result<()> {
    if min_gib == 0 {
        return Ok(());
    }
    let free = usnm_ingest::progress::disk_free(dir)
        .with_context(|| format!("reading the free space of {}", dir.display()))?;
    let stale = dir_size(&dir.join("qwdata"));
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    if free.saturating_add(stale) < min_gib.saturating_mul(1 << 30) {
        bail!(
            "{} has {:.1} GiB free, under the {min_gib} GiB a release needs for indexing and \
             merges (08 §8.4); is the scratch volume mounted?",
            dir.display(),
            gib(free + stale)
        );
    }
    tracing::info!(dir = %dir.display(), free_gib = gib(free + stale).round(), "work disk");
    Ok(())
}

/// Bytes in the files under `path` (0 if it doesn't exist).
fn dir_size(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(_) => e.metadata().map_or(0, |m| m.len()),
            Err(_) => 0,
        })
        .sum()
}

/// `prune`, holding the writer lock throughout (released on every path),
/// against `--quickwit-url` or a node run from `--quickwit-bin` for its
/// length. The node's data goes in a temporary directory: it only reads
/// the metastore and deletes, and a job execution has no scratch share.
async fn prune(
    cli: &Stores,
    state: &State,
    delete: bool,
    t: &IndexTarget,
) -> anyhow::Result<usnm_ingest::prune::Report> {
    prune_target(delete, t)?;
    let reference = usnm_store::open(&cli.reference)?;
    // Before the lock and the node: refuse at once without a published version.
    usnm_ingest::prune::kept(reference.as_ref(), state).await?;
    let owner = owner_id();
    let lease = release::lock_writer(state, &owner).await?;
    let result = async {
        match (&t.quickwit_url, &t.quickwit_bin) {
            (Some(url), None) => {
                usnm_ingest::prune::prune(state, reference.as_ref(), url, delete, &lease).await
            }
            (None, Some(bin)) => {
                let metastore = t
                    .quickwit_metastore
                    .as_deref()
                    .context("--quickwit-bin needs --quickwit-metastore")?;
                let dir = tempfile::Builder::new().prefix("usnm-prune").tempdir()?;
                let node =
                    QuickwitNode::start(bin, dir.path(), t.quickwit_port, metastore, root(t)?)
                        .await?;
                let r =
                    usnm_ingest::prune::prune(state, reference.as_ref(), &node.url, delete, &lease)
                        .await;
                node.stop().await?;
                r
            }
            _ => bail!("choose one of --quickwit-url or --quickwit-bin"),
        }
    }
    .await;
    let unlocked = release::unlock_writer(state, &owner, lease).await;
    let report = result?;
    unlocked?;
    Ok(report)
}

/// Which nodes prune may use. Deleting needs a node it started
/// (`--quickwit-bin`): when the writer lock can't be renewed it stops that
/// node, which ends a delete under way. A node at `--quickwit-url` isn't
/// its to stop, and Quickwit carries on with a delete whose caller went
/// away, so that one is for dry runs.
fn prune_target(delete: bool, t: &IndexTarget) -> anyhow::Result<()> {
    if t.index_dir.is_some() {
        bail!("prune needs a Quickwit node: --quickwit-bin, or --quickwit-url for a dry run");
    }
    if delete && t.quickwit_url.is_some() && t.quickwit_bin.is_none() {
        bail!(
            "prune --delete needs --quickwit-bin: it stops its own node if the writer lock \
             is lost, and can't stop a node at --quickwit-url"
        );
    }
    Ok(())
}

fn root(t: &IndexTarget) -> anyhow::Result<&str> {
    t.quickwit_index_root
        .as_deref()
        .context("Quickwit targets need --quickwit-index-root")
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let telemetry = telemetry::init();
    let result = run(&cli).await;
    // Every failure ends with one JSON line (the whole error chain), which
    // the job-failure alert and the `errors-by-batch` query look for. Its
    // `outcome` is `titles_left` for the expected stop of a full run that
    // titles-sync didn't finish (the alert leaves those out; starting the
    // job again continues), else `failed`. The exit code stays 1 either
    // way, so the execution shows as failed and is started again.
    // Returning an exit code rather than the error keeps Rust from printing
    // it again as plain text after that line.
    let code = match &result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(
                command = cli.command.name(),
                outcome = failure_outcome(e),
                error = %format!("{e:#}"),
                "command failed"
            );
            std::process::ExitCode::FAILURE
        }
    };
    // Before exit on every path: whatever telemetry is still buffered is lost.
    telemetry.shutdown().await;
    code
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Enqueue { .. } => "enqueue",
            Command::Curate { .. } => "curate",
            Command::Release { .. } => "release",
            Command::TitlesSync { .. } => "titles-sync",
            Command::Geocode => "geocode",
            Command::TitlesArchive => "titles-archive",
            Command::TitlesSeed => "titles-seed",
            Command::Duplicates => "duplicates",
            Command::ArchiveCopy { .. } => "archive-copy",
            Command::Prune { .. } => "prune",
            Command::OcrDamage { .. } => "ocr-damage",
            Command::OcrPrefetchManifest { .. } => "ocr-prefetch-manifest",
            Command::OcrSync { .. } => "ocr-sync",
            Command::OcrSeed { .. } => "ocr-seed",
            Command::OcrEnqueue { .. } => "ocr-enqueue",
            Command::OcrRequeue { .. } => "ocr-requeue",
            Command::CongressGlobe { .. } => "congress-globe",
            Command::CongressSerialset { .. } => "congress-serialset",
            Command::Run { .. } => "run",
        }
    }
}

/// `secs` from `start`, if given.
fn deadline(start: tokio::time::Instant, secs: Option<u64>) -> Option<tokio::time::Instant> {
    secs.map(|s| start + std::time::Duration::from_secs(s))
}

/// The step a command starts at, for those that report what they do on
/// the status page (`ops/activity`): the ingest job's `run`, and
/// `titles-sync` and `release` run by hand. Curate workers don't.
fn first_step(command: &Command) -> Option<Step> {
    match command {
        Command::Run { .. } => Some(Step::Listing),
        Command::TitlesSync { .. } => Some(Step::Titles),
        Command::Release { .. } => Some(Step::Indexing),
        Command::Enqueue { .. }
        | Command::Curate { .. }
        | Command::Geocode
        | Command::TitlesArchive
        | Command::TitlesSeed
        | Command::Duplicates
        | Command::ArchiveCopy { .. }
        | Command::Prune { .. }
        | Command::OcrDamage { .. }
        | Command::OcrPrefetchManifest { .. }
        | Command::OcrSync { .. }
        | Command::OcrSeed { .. }
        | Command::OcrEnqueue { .. }
        | Command::OcrRequeue { .. }
        | Command::CongressGlobe { .. }
        | Command::CongressSerialset { .. } => None,
    }
}

/// The `outcome` of the "command failed" line: `titles_left` for a run that
/// stopped because titles-sync left titles (expected; start it again),
/// `failed` for anything else.
fn failure_outcome(e: &anyhow::Error) -> &'static str {
    if e.is::<TitlesLeft>() {
        "titles_left"
    } else {
        "failed"
    }
}

/// How a reporting command ended, for `ops/activity`.
fn outcome(result: &anyhow::Result<Option<Published>>) -> (Outcome, Option<String>) {
    match result {
        Ok(Some(_)) => (Outcome::Published, None),
        Ok(None) => (Outcome::NothingNew, None),
        Err(e) => {
            // The site changed even though the command failed afterwards.
            let kind = if e.is::<PublishedUnrecorded>() {
                Outcome::Published
            } else if e.is::<TitlesLeft>() {
                Outcome::TitlesLeft
            } else {
                Outcome::Failed
            };
            (kind, Some(format!("{e:#}")))
        }
    }
}

async fn run(cli: &Cli) -> anyhow::Result<()> {
    let start = tokio::time::Instant::now();
    let state = state(&cli.stores)?;
    let report = match first_step(&cli.command) {
        Some(step) => Reporter::start(state.clone(), cli.command.name(), &owner_id(), step).await,
        None => Reporter::off(),
    };
    let beat = report.every(activity::HEARTBEAT);
    let result = command(cli, &state, start, &report).await;
    drop(beat);
    let (kind, error) = outcome(&result);
    report.end(kind, error).await;
    result.map(drop)
}

/// Run the command; `Some` when it published a version.
async fn command(
    cli: &Cli,
    state: &State,
    start: tokio::time::Instant,
    report: &Reporter,
) -> anyhow::Result<Option<Published>> {
    let state = state.clone();
    match &cli.command {
        Command::Enqueue {
            list,
            batches,
            force,
        } => enqueue(&state, list, batches, *force, cli.stores.raw.as_deref())
            .await
            .map(|_| None),
        Command::TitlesSync {
            list,
            lccns,
            refresh,
            items,
            max_runtime_secs,
        } => {
            let listed = if lccns.is_empty() {
                read_list(list, cli.stores.raw.as_deref())
                    .await?
                    .into_iter()
                    .flat_map(|b| b.lccns)
                    .collect()
            } else {
                lccns.clone()
            };
            let synced = titles_sync(
                &cli.stores,
                &state,
                listed,
                *refresh,
                items,
                deadline(start, *max_runtime_secs),
                report,
            )
            .await?;
            if !synced.finished() {
                // Fail, so the job reports it and a later run continues.
                return Err(TitlesLeft(unfinished(&synced)).into());
            }
            Ok(None)
        }
        Command::Geocode => geocode(usnm_store::open(&cli.stores.reference)?.as_ref())
            .await
            .map(|()| None),
        Command::TitlesArchive => {
            let raw = cli
                .stores
                .raw
                .as_deref()
                .context("titles-archive needs USNM_RAW_URL")?;
            titles_archive(usnm_store::open(&cli.stores.reference)?.as_ref(), raw)
                .await
                .map(|()| None)
        }
        Command::TitlesSeed => {
            let raw = cli
                .stores
                .raw
                .as_deref()
                .context("titles-seed needs USNM_RAW_URL")?;
            let seeded = usnm_ingest::titles_archive::seed(
                usnm_store::open(raw)?.as_ref(),
                usnm_store::open(&cli.stores.reference)?.as_ref(),
            )
            .await?;
            match (&seeded.from, seeded.written.is_empty()) {
                (None, _) => tracing::warn!(
                    "no title records in the raw store to start from; run titles-sync"
                ),
                (Some(_), true) => tracing::info!(
                    ?seeded,
                    "this environment has its own title records; left as they are"
                ),
                (Some(_), false) => tracing::info!(?seeded, "title records seeded"),
            }
            Ok(None)
        }
        Command::ArchiveCopy { from, to, batches } => {
            let copied = usnm_ingest::raw::copy(
                usnm_store::open(from)?.as_ref(),
                usnm_store::open(to)?.as_ref(),
                batches,
            )
            .await?;
            tracing::info!(?copied, "archives copied");
            Ok(None)
        }
        Command::OcrDamage {
            index_version,
            batches,
            sample_pct,
            compare_csv,
            parity,
            max_runtime_secs,
            parallel,
        } => {
            let opts = usnm_ingest::ocr_damage::Options {
                index_version: index_version.clone().unwrap_or_default(),
                batches: batches.clone(),
                sample_pct: *sample_pct,
                compare_csv: compare_csv.clone(),
                parity: parity.clone(),
                deadline: deadline(start, *max_runtime_secs),
                parallel: parallel.unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
                }),
            };
            let curated = usnm_store::open(&cli.stores.curated)?;
            let reference = usnm_store::open(&cli.stores.reference)?;
            usnm_ingest::ocr_damage::run(curated, reference.as_ref(), &opts)
                .await
                .map(|()| None)
        }
        Command::OcrPrefetchManifest {
            index_version,
            partial,
            min_damage,
            exchange_url,
        } => {
            let curated = usnm_store::open(&cli.stores.curated)?;
            let reference = usnm_store::open(&cli.stores.reference)?;
            let exchange = exchange_url.as_deref().map(usnm_store::open).transpose()?;
            let opts = usnm_ingest::ocr_prefetch::Options {
                index_version: index_version.clone(),
                partial: *partial,
                min_damage: *min_damage,
            };
            usnm_ingest::ocr_prefetch::run(
                curated.as_ref(),
                reference.as_ref(),
                exchange.as_deref(),
                &opts,
            )
            .await
            .map(|_| None)
        }
        Command::OcrSync {
            queue_url,
            out_url,
            quarantine_url,
            max_parts,
            chunk,
            max_dead_letters,
            report,
            from_quarantine,
            reason,
            max_runtime_secs,
            seed,
        } => {
            let seed = seed.options(false)?;
            let curated = usnm_store::open(&cli.stores.curated)?;
            if *report {
                usnm_ingest::ocr_local::report::run(curated.as_ref()).await?;
                return Ok(None);
            }
            let need = |v: &Option<String>, flag: &str| {
                v.clone().with_context(|| format!("ocr-sync needs {flag}"))
            };
            let stores = usnm_ingest::ocr_local::sync::Stores {
                curated,
                reference: usnm_store::open(&cli.stores.reference)?,
                out: usnm_store::open(&need(out_url, "--out-url")?)?,
                quarantine: usnm_store::open(&need(quarantine_url, "--quarantine-url")?)?,
                queue: usnm_store::queue::open(&need(queue_url, "--queue-url")?)?,
            };
            let opts = usnm_ingest::ocr_local::sync::Options {
                max_parts: *max_parts,
                chunk: *chunk,
                max_dead_letters: *max_dead_letters,
                from_quarantine: from_quarantine.then(|| reason.clone()).flatten(),
                deadline: deadline(start, *max_runtime_secs),
                seed,
            };
            usnm_ingest::ocr_local::sync::run(&state, &owner_id(), &stores, &opts)
                .await
                .map(|_| None)
        }
        Command::OcrSeed {
            queue_url,
            out_url,
            seed,
            dry_run,
            max_runtime_secs,
        } => {
            let opts = usnm_ingest::ocr_local::seed::Options {
                deadline: deadline(start, *max_runtime_secs),
                ..seed.options(*dry_run)?
            };
            let curated = usnm_store::open(&cli.stores.curated)?;
            let reference = usnm_store::open(&cli.stores.reference)?;
            let queue = usnm_store::queue::open(queue_url)?;
            let out = out_url.as_deref().map(usnm_store::open).transpose()?;
            let stores = usnm_ingest::ocr_local::seed::Stores {
                curated: curated.as_ref(),
                reference: reference.as_ref(),
                queue: &queue,
                out: out.as_deref(),
            };
            let report =
                usnm_ingest::ocr_local::seed::run(&state, &owner_id(), &stores, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(None)
        }
        Command::OcrEnqueue {
            queue_url,
            pages,
            reason,
            plan,
            batch,
            include_outstanding,
            dry_run,
        } => {
            let opts = usnm_ingest::ocr_local::enqueue::EnqueueOptions {
                pages: pages.clone(),
                reason: (*reason).into(),
                plan: plan.clone(),
                batch: batch.clone(),
                include_outstanding: *include_outstanding,
                dry_run: *dry_run,
            };
            usnm_ingest::ocr_local::enqueue::enqueue(
                &state,
                usnm_store::open(&cli.stores.curated)?.as_ref(),
                usnm_store::open(&cli.stores.reference)?.as_ref(),
                &usnm_store::queue::open(queue_url)?,
                &opts,
            )
            .await
            .map(|_| None)
        }
        Command::OcrRequeue {
            queue_url,
            cause,
            kind,
            since,
            plan,
            max_pages,
            dry_run,
        } => {
            let opts = usnm_ingest::ocr_local::enqueue::RequeueOptions {
                cause: cause.clone(),
                kind: kind.clone(),
                since: *since,
                plan: plan.clone(),
                max_pages: *max_pages,
                dry_run: *dry_run,
            };
            usnm_ingest::ocr_local::enqueue::requeue(
                &state,
                usnm_store::open(&cli.stores.curated)?.as_ref(),
                usnm_store::open(&cli.stores.reference)?.as_ref(),
                &usnm_store::queue::open(queue_url)?,
                &opts,
            )
            .await
            .map(|_| None)
        }
        Command::CongressGlobe {
            items,
            refetch,
            max_items,
            max_runtime_secs,
            crawl_delay_secs,
            unt_url,
        } => {
            use usnm_ingest::congress::globe;
            let curated = usnm_store::open(&cli.stores.curated)?;
            let raw = cli
                .stores
                .raw
                .as_deref()
                .map(usnm_store::open)
                .transpose()?;
            if raw.is_none() {
                tracing::warn!(
                    "no raw store (USNM_RAW_URL): UNT's responses aren't kept, and an item \
                     stopped by the deadline is fetched again from its first page"
                );
            }
            let run_deadline = deadline(start, *max_runtime_secs);
            // 120 s a try, and every request ends within the grace after the
            // deadline, short of the job's hard timeout.
            let fetch = globe::Http::new(
                std::time::Duration::from_secs(*crawl_delay_secs),
                globe::USER_AGENT,
                "UNT",
            )?
            .with_cutoff(run_deadline.map(|d| d + usnm_ingest::congress::fetch::REQUEST_GRACE));
            let opts = globe::Options {
                base: unt_url.trim_end_matches('/').to_owned(),
                items: items.clone(),
                refetch: *refetch,
                deadline: run_deadline,
                max_items: *max_items,
                ..globe::Options::default()
            };
            let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let report = globe::locked(
                &state,
                &owner_id(),
                globe::run(
                    &fetch,
                    curated.as_ref(),
                    raw.as_deref(),
                    &opts,
                    lost.as_ref(),
                ),
                lost.clone(),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(None)
        }
        Command::CongressSerialset {
            years,
            serials,
            refetch,
            max_volumes,
            max_runtime_secs,
            delay_secs,
            govinfo_url,
            pdftotext,
        } => {
            use usnm_ingest::congress::serialset;
            let curated = usnm_store::open(&cli.stores.curated)?;
            let raw = cli
                .stores
                .raw
                .as_deref()
                .map(usnm_store::open)
                .transpose()?;
            if raw.is_none() {
                tracing::warn!(
                    "no raw store (USNM_RAW_URL): GovInfo's MODS and page texts aren't kept, \
                     and a volume stopped by the deadline is fetched again from its first PDF"
                );
            }
            // PDFs are streamed here and deleted once read.
            let scratch = cli.stores.work_dir.join("serialset");
            std::fs::create_dir_all(&scratch)
                .with_context(|| format!("creating {}", scratch.display()))?;
            let run_deadline = deadline(start, *max_runtime_secs);
            // Sitemaps and MODS get 120 s a try, a PDF 30 minutes; every
            // request ends within the grace after the deadline.
            let fetch = usnm_ingest::congress::fetch::Http::new(
                std::time::Duration::from_secs(*delay_secs),
                serialset::USER_AGENT,
                "GovInfo",
            )?
            .with_cutoff(run_deadline.map(|d| d + usnm_ingest::congress::fetch::REQUEST_GRACE));
            let pdf = serialset::Pdftotext::new(pdftotext.clone());
            let opts = serialset::Options {
                base: govinfo_url.trim_end_matches('/').to_owned(),
                years: years.clone(),
                serials: serials.clone(),
                refetch: *refetch,
                deadline: run_deadline,
                max_volumes: *max_volumes,
                scratch,
            };
            let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let report = serialset::locked(
                &state,
                &owner_id(),
                serialset::run(
                    &fetch,
                    &pdf,
                    curated.as_ref(),
                    raw.as_deref(),
                    &opts,
                    lost.as_ref(),
                ),
                lost.clone(),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(None)
        }
        Command::Duplicates => {
            let curated = usnm_store::open(&cli.stores.curated)?;
            let report = usnm_ingest::dedup::report(&state, curated.as_ref()).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(None)
        }
        Command::Prune { delete, target } => {
            let report = prune(&cli.stores, &state, *delete, target).await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            Ok(None)
        }
        Command::Curate {
            max_batches,
            enqueue: first,
            list,
            fetch_interval_secs,
            max_runtime_secs,
        } => {
            if *first {
                enqueue(&state, list, &[], false, cli.stores.raw.as_deref()).await?;
            }
            curate(
                &cli.stores,
                &state,
                *max_batches,
                *fetch_interval_secs,
                deadline(start, *max_runtime_secs),
            )
            .await
            .map(|()| None)
        }
        Command::Release {
            full,
            synthetic,
            american_stories,
            ja_latin,
            ocr_local,
            congress,
            partition_decade,
            single_text_field,
            page_language,
            split_docs,
            target,
        } => {
            release(
                &cli.stores,
                &state,
                *full,
                *synthetic,
                *american_stories,
                *ja_latin,
                *ocr_local,
                *congress,
                *partition_decade,
                *single_text_field,
                *page_language,
                *split_docs,
                None,
                target,
                report,
            )
            .await
        }
        Command::Run {
            list,
            batches,
            full,
            synthetic,
            american_stories,
            ja_latin,
            ocr_local,
            congress,
            partition_decade,
            single_text_field,
            page_language,
            split_docs,
            curate_max_runtime_secs,
            titles_max_runtime_secs,
            target,
        } => {
            // Before hours of curation and titles-sync: the release would refuse.
            if *american_stories {
                let curated = usnm_store::open(&cli.stores.curated)?;
                usnm_ingest::american_stories::check_written(curated.as_ref()).await?;
            }
            let listed = enqueue(&state, list, batches, false, cli.stores.raw.as_deref()).await?;
            report.step(Step::Downloading).await;
            curate(
                &cli.stores,
                &state,
                None,
                worker::FETCH_INTERVAL_SECS,
                deadline(start, *curate_max_runtime_secs),
            )
            .await?;
            // Every curated title needs a catalog entry before release.
            report.step(Step::Titles).await;
            let lccns = listed.into_iter().flat_map(|b| b.lccns);
            let synced = titles_sync(
                &cli.stores,
                &state,
                lccns,
                false,
                titles::LOC_ITEMS,
                deadline(start, *titles_max_runtime_secs),
                report,
            )
            .await?;
            // The release decides: a delta goes ahead, a full base (asked
            // for or forced) refuses (`Release::titles_left`).
            let titles_left = titles_left(&synced);
            report.step(Step::Indexing).await;
            release(
                &cli.stores,
                &state,
                *full,
                *synthetic,
                *american_stories,
                *ja_latin,
                *ocr_local,
                *congress,
                *partition_decade,
                *single_text_field,
                *page_language,
                *split_docs,
                titles_left,
                target,
                report,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_why_titles_sync_stopped() {
        let report = titles::SyncReport {
            wanted: 3122,
            fetched: 342,
            throttled: true,
            left: 2779,
            ..Default::default()
        };
        assert!(unfinished(&report)
            .starts_with("LoC rate limited titles-sync with 2779 of 3122 titles left"));
        let late = titles::SyncReport {
            throttled: false,
            out_of_time: true,
            ..report
        };
        assert!(unfinished(&late).starts_with("titles-sync reached its deadline"));
        assert!(titles_left(&late).is_some());

        let done = titles::SyncReport {
            wanted: 3,
            fetched: 2,
            not_found: vec!["sn3".into()],
            ..Default::default()
        };
        assert_eq!(titles_left(&done), None);
        let failed = titles::SyncReport {
            failed: vec!["sn2".into()],
            ..done
        };
        assert_eq!(
            titles_left(&failed).unwrap(),
            "titles-sync couldn't fetch 1 of 3 titles (sn2); the next run retries them"
        );
    }

    #[test]
    fn records_how_a_command_ended() {
        let published = Published {
            index_version: "v1".into(),
            indexes: vec![],
            full: true,
            docs: 1,
            pages: 1,
        };
        assert_eq!(outcome(&Ok(Some(published))).0, Outcome::Published);
        assert_eq!(outcome(&Ok(None)), (Outcome::NothingNew, None));
        let left: anyhow::Error = TitlesLeft("titles-sync reached its deadline".into()).into();
        let (kind, error) = outcome(&Err(left.context("run")));
        assert_eq!(kind, Outcome::TitlesLeft);
        assert!(error.unwrap().contains("deadline"));
        let live: anyhow::Error = PublishedUnrecorded("v2 is live, but…".into()).into();
        let (kind, error) = outcome(&Err(live));
        assert_eq!(kind, Outcome::Published);
        assert!(error.is_some());
        let (kind, _) = outcome(&Err(anyhow::anyhow!("connection refused")));
        assert_eq!(kind, Outcome::Failed);
        // The failure line's outcome, which the job-failed alert reads.
        let left: anyhow::Error = TitlesLeft("titles-sync reached its deadline".into()).into();
        assert_eq!(failure_outcome(&left.context("run")), "titles_left");
        assert_eq!(
            failure_outcome(&anyhow::anyhow!("connection refused")),
            "failed"
        );
        assert_eq!(
            first_step(
                &Cli::try_parse_from([
                    "usnm-ingest",
                    "--curated",
                    "c",
                    "--reference",
                    "r",
                    "curate"
                ])
                .unwrap()
                .command
            ),
            None
        );
    }

    #[test]
    fn run_takes_a_titles_deadline() {
        let cli = Cli::try_parse_from([
            "usnm-ingest",
            "--curated",
            "c",
            "--reference",
            "r",
            "run",
            "--full",
            "--american-stories",
            "--ja-latin",
            "--ocr-local",
            "--congress",
            "--partition-decade",
            "--single-text-field",
            "--page-language",
            "--split-docs",
            "120000",
            "--titles-max-runtime-secs",
            "28800",
            "--index-dir",
            "x",
        ])
        .unwrap();
        let Command::Run {
            titles_max_runtime_secs,
            full,
            american_stories,
            ja_latin,
            ocr_local,
            congress,
            partition_decade,
            single_text_field,
            page_language,
            split_docs,
            ..
        } = cli.command
        else {
            panic!("not a run");
        };
        assert_eq!(
            (
                titles_max_runtime_secs,
                full,
                american_stories,
                ja_latin,
                ocr_local,
                congress,
                partition_decade,
                single_text_field,
                page_language,
                split_docs
            ),
            (
                Some(28800),
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                true,
                Some(120_000)
            )
        );
        // A split target of 0, or of a million pages or more, is refused.
        for bad in ["0", "1000000", "-1"] {
            assert!(
                Cli::try_parse_from([
                    "usnm-ingest",
                    "--curated",
                    "c",
                    "--reference",
                    "r",
                    "release",
                    "--split-docs",
                    bad,
                    "--index-dir",
                    "x",
                ])
                .is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_writer_needs_its_scratch_disk() {
        let dir = tempfile::tempdir().unwrap();
        check_free_disk(dir.path(), 0).unwrap();
        check_free_disk(dir.path(), 1).unwrap();
        // More than any test machine has: the volume isn't mounted.
        let err = check_free_disk(dir.path(), 1 << 30)
            .unwrap_err()
            .to_string();
        assert!(err.contains("scratch volume"), "{err}");
    }

    #[test]
    fn a_previous_releases_decade_files_are_removed_first() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join(usnm_ingest::decade_order::SPILL_DIR);
        std::fs::create_dir_all(&spill).unwrap();
        std::fs::write(spill.join("1890.jsonl.zst"), vec![0u8; 3000]).unwrap();
        remove_stale_spill(dir.path()).unwrap();
        assert!(!spill.exists());
        // Nothing to remove is fine too.
        remove_stale_spill(dir.path()).unwrap();
    }

    #[test]
    fn a_previous_writers_data_counts_as_free() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("qwdata/wal");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("a"), vec![0u8; 3000]).unwrap();
        std::fs::write(dir.path().join("qwdata/b"), vec![0u8; 500]).unwrap();
        assert_eq!(dir_size(&dir.path().join("qwdata")), 3500);
        assert_eq!(dir_size(&dir.path().join("missing")), 0);
    }

    #[test]
    fn prune_is_a_dry_run_unless_asked_to_delete() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "usnm-ingest",
                "--curated",
                "c",
                "--reference",
                "r",
                "prune",
                "--quickwit-url",
                "http://127.0.0.1:7280",
            ];
            args.extend_from_slice(extra);
            match Cli::try_parse_from(args).unwrap().command {
                Command::Prune { delete, target } => (delete, target.quickwit_url),
                _ => panic!("not a prune"),
            }
        };
        assert_eq!(parse(&[]), (false, Some("http://127.0.0.1:7280".into())));
        assert!(parse(&["--delete"]).0);
        // Named flags only (#277).
        assert!(Cli::try_parse_from([
            "usnm-ingest",
            "--curated",
            "c",
            "--reference",
            "r",
            "prune",
            "delete"
        ])
        .is_err());
    }

    #[test]
    fn prune_deletes_only_through_a_node_it_started() {
        let target = |args: &[&str]| {
            let mut all = vec!["usnm-ingest", "--curated", "c", "--reference", "r", "prune"];
            all.extend_from_slice(args);
            match Cli::try_parse_from(all).unwrap().command {
                Command::Prune { target, .. } => target,
                _ => panic!("not a prune"),
            }
        };
        let url = target(&["--quickwit-url", "http://127.0.0.1:7280"]);
        assert!(prune_target(false, &url).is_ok());
        let err = prune_target(true, &url).unwrap_err();
        assert!(
            format!("{err:#}").contains("needs --quickwit-bin"),
            "{err:#}"
        );
        let bin = target(&["--quickwit-bin", "/usr/local/bin/quickwit"]);
        assert!(prune_target(true, &bin).is_ok());
        assert!(prune_target(false, &target(&["--index-dir", "x"])).is_err());
    }

    #[test]
    fn congress_globe_keeps_unt_s_crawl_delay() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "usnm-ingest",
                "--curated",
                "c",
                "--reference",
                "r",
                "congress-globe",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        let Command::CongressGlobe {
            items,
            crawl_delay_secs,
            unt_url,
            refetch,
            ..
        } = parse(&["--items", "metadc29274,metadc29275"])
            .unwrap()
            .command
        else {
            panic!("not congress-globe");
        };
        assert_eq!(items, ["metadc29274", "metadc29275"]);
        assert_eq!(crawl_delay_secs, 3);
        assert_eq!(unt_url, usnm_ingest::congress::globe::UNT);
        assert!(!refetch);
        assert!(parse(&["--crawl-delay-secs", "2"]).is_err());
        assert!(parse(&["--crawl-delay-secs", "5"]).is_ok());
    }

    #[test]
    fn congress_serialset_paces_at_a_second_at_least() {
        let parse = |extra: &[&str]| {
            let mut args = vec![
                "usnm-ingest",
                "--curated",
                "c",
                "--reference",
                "r",
                "congress-serialset",
            ];
            args.extend_from_slice(extra);
            Cli::try_parse_from(args)
        };
        let Command::CongressSerialset {
            years,
            serials,
            delay_secs,
            govinfo_url,
            pdftotext,
            refetch,
            ..
        } = parse(&["--years", "1820,1990", "--serials", "33"])
            .unwrap()
            .command
        else {
            panic!("not congress-serialset");
        };
        assert_eq!(years, [1820, 1990]);
        assert_eq!(serials, [33]);
        assert_eq!(delay_secs, 1);
        assert_eq!(govinfo_url, usnm_ingest::congress::serialset::GOVINFO);
        assert_eq!(pdftotext, PathBuf::from("pdftotext"));
        assert!(!refetch);
        assert!(parse(&["--delay-secs", "0"]).is_err());
        assert!(parse(&["--delay-secs", "2"]).is_ok());
        assert!(parse(&["1820"]).is_err(), "named arguments only");
    }

    #[test]
    fn merge_and_disk_settings_default() {
        let cli = Cli::try_parse_from([
            "usnm-ingest",
            "--curated",
            "c",
            "--reference",
            "r",
            "release",
            "--index-dir",
            "x",
        ])
        .unwrap();
        let Command::Release {
            target,
            american_stories,
            ja_latin,
            ocr_local,
            congress,
            partition_decade,
            single_text_field,
            page_language,
            split_docs,
            ..
        } = cli.command
        else {
            panic!("not a release");
        };
        assert_eq!(split_docs, None, "the template's unless asked for");
        assert!(!american_stories, "off unless asked for");
        assert!(!ja_latin, "off unless asked for");
        assert!(!ocr_local, "off unless asked for");
        assert!(!congress, "off unless asked for");
        assert!(!partition_decade, "off unless asked for");
        assert!(!single_text_field, "off unless asked for");
        assert!(!page_language, "off unless asked for");
        assert_eq!(target.merge_timeout_secs, merges::DEFAULT_TIMEOUT_SECS);
        assert_eq!(target.min_free_gib, 0);
    }

    #[test]
    fn ocr_damage_arguments() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let parse = |args: &[&str]| {
            let mut all = vec![
                "usnm-ingest",
                "--curated",
                "c",
                "--reference",
                "r",
                "ocr-damage",
            ];
            all.extend_from_slice(args);
            Cli::try_parse_from(all)
        };
        assert!(parse(&[]).is_err(), "needs --version or --parity");
        assert!(parse(&["--parity", "p.jsonl.zst"]).is_ok());
        assert!(
            parse(&["--version", "v1", "--compare-csv", "x.csv.gz"]).is_err(),
            "--compare-csv needs --sample-pct"
        );
        assert!(
            parse(&["--version", "v1", "--sample-pct", "10"]).is_err(),
            "the gate needs --compare-csv"
        );
        assert!(parse(&[
            "--version",
            "v1",
            "--sample-pct",
            "10",
            "--compare-csv",
            "x.csv.gz"
        ])
        .is_ok());
        let cli = parse(&[
            "--version",
            "v1",
            "--batches",
            "a,b_ver02",
            "--max-runtime-secs",
            "60",
        ])
        .unwrap();
        let Command::OcrDamage {
            index_version,
            batches,
            max_runtime_secs,
            ..
        } = cli.command
        else {
            panic!("not ocr-damage");
        };
        assert_eq!(index_version.as_deref(), Some("v1"));
        assert_eq!(batches, ["a", "b_ver02"]);
        assert_eq!(max_runtime_secs, Some(60));
    }

    #[test]
    fn ocr_seed_arguments() {
        let parse = |args: &[&str]| {
            let mut all = vec![
                "usnm-ingest",
                "--curated",
                "c",
                "--reference",
                "r",
                "ocr-seed",
                "--queue-url",
                "q",
            ];
            all.extend_from_slice(args);
            Cli::try_parse_from(all)
        };
        let cli = parse(&[
            "--seed-plan",
            "pilot-deu",
            "--seed-languages",
            "deu",
            "--seed-years",
            "1850-1919",
            "--seed-max-pages",
            "3000",
            "--seed-pilot",
            "--bulk-window",
            "1000",
            "--dry-run",
        ])
        .unwrap();
        let Command::OcrSeed { seed, dry_run, .. } = cli.command else {
            panic!("not ocr-seed");
        };
        assert!(dry_run);
        let opts = seed.options(true).unwrap();
        assert_eq!(opts.plan, "pilot-deu");
        assert_eq!(opts.languages, ["deu"]);
        assert_eq!(opts.years, Some((1850, 1919)));
        assert_eq!(opts.max_pages, Some(3000));
        assert_eq!(opts.window, 1000);
        assert_eq!(opts.refill_below, 0);
        assert!(opts.pilot && !opts.skip_american_stories);
        assert!((opts.min_damage - 0.25).abs() < 1e-12);
        // A pilot needs a budget.
        let cli = parse(&[
            "--seed-plan",
            "p",
            "--seed-pilot",
            "--seed-languages",
            "deu",
        ])
        .unwrap();
        let Command::OcrSeed { seed, .. } = cli.command else {
            panic!("not ocr-seed");
        };
        assert!(seed.options(false).is_err());
        // ocr-sync takes the same settings; no plan leaves seeding off.
        let cli = Cli::try_parse_from([
            "usnm-ingest",
            "--curated",
            "c",
            "--reference",
            "r",
            "ocr-sync",
        ])
        .unwrap();
        let Command::OcrSync { seed, .. } = cli.command else {
            panic!("not ocr-sync");
        };
        assert!(!seed.options(false).unwrap().on());
    }
}
