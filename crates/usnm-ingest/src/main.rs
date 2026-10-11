use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use usnm_ingest::activity::{self, Reporter};
use usnm_ingest::docs::{DocStore, FileDocs};
use usnm_ingest::merges::{self, MergeWait};
use usnm_ingest::release::{Published, PublishedUnrecorded, Release, TitlesLeft};
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
        partition_decade: bool,
        /// As for `release`.
        #[arg(long)]
        single_text_field: bool,
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
    Ok(report)
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
    partition_decade: bool,
    single_text_field: bool,
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
        partition_decade,
        single_text_field,
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
            Command::Duplicates => "duplicates",
            Command::ArchiveCopy { .. } => "archive-copy",
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
        | Command::Duplicates
        | Command::ArchiveCopy { .. } => None,
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
        Command::Duplicates => {
            let curated = usnm_store::open(&cli.stores.curated)?;
            let report = usnm_ingest::dedup::report(&state, curated.as_ref()).await?;
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
            partition_decade,
            single_text_field,
            target,
        } => {
            release(
                &cli.stores,
                &state,
                *full,
                *synthetic,
                *american_stories,
                *ja_latin,
                *partition_decade,
                *single_text_field,
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
            partition_decade,
            single_text_field,
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
                *partition_decade,
                *single_text_field,
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
            "--partition-decade",
            "--single-text-field",
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
            partition_decade,
            single_text_field,
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
                partition_decade,
                single_text_field
            ),
            (Some(28800), true, true, true, true, true)
        );
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
            partition_decade,
            single_text_field,
            ..
        } = cli.command
        else {
            panic!("not a release");
        };
        assert!(!american_stories, "off unless asked for");
        assert!(!ja_latin, "off unless asked for");
        assert!(!partition_decade, "off unless asked for");
        assert!(!single_text_field, "off unless asked for");
        assert_eq!(target.merge_timeout_secs, merges::DEFAULT_TIMEOUT_SECS);
        assert_eq!(target.min_free_gib, 0);
    }
}
