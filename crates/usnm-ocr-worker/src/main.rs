//! `usnm-ocr-worker`: see the crate's README.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::watch;
use usnm_ocr_worker::cache::CachedScans;
use usnm_ocr_worker::engine::{Engine, Limits, Remote, Runner, Tesseract, TesseractOptions};
use usnm_ocr_worker::loc::{ImageSize, LocClient, LocConfig};
use usnm_ocr_worker::message::{OcrMessage, Reason};
use usnm_ocr_worker::prefetch;
use usnm_ocr_worker::publish;
use usnm_ocr_worker::queue::{self, BULK, PRIORITY};
use usnm_ocr_worker::status::{self, About, Tracker};
use usnm_ocr_worker::telemetry::{self, Metrics};
use usnm_ocr_worker::worker::{check_node_id, Config, ScanSource, Worker};

/// `--version`: the crate version and the commit CI built it from
/// (`USNM_GIT_SHA` at build time), or `unknown commit` for a local build.
static VERSION: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{} ({})",
        env!("CARGO_PKG_VERSION"),
        option_env!("USNM_GIT_SHA")
            .filter(|s| !s.is_empty())
            .map_or_else(|| "unknown commit".to_owned(), |s| format!("commit {s}"))
    )
});

#[derive(Parser)]
#[command(
    version = VERSION.as_str(),
    about = "Re-OCR damaged newspaper pages from a work queue (#296)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum EngineName {
    Tesseract,
}

#[derive(clap::Args)]
struct EngineArgs {
    /// OCR engine.
    #[arg(long, value_enum, default_value = "tesseract")]
    engine: EngineName,
    /// The Tesseract binary, when Tesseract is installed on the host.
    #[arg(long, default_value = "tesseract", conflicts_with = "tesseract_image")]
    tesseract_bin: PathBuf,
    /// Run Tesseract from this container image with podman, one container
    /// per page: `sha256:<image id>` or `<name>@sha256:<digest>`, never a
    /// tag. `scripts/ocr-node/install-worker.sh` loads the release's image
    /// and sets it.
    #[arg(long, env = "USNM_OCR_TESSERACT_IMAGE")]
    tesseract_image: Option<String>,
    /// The podman binary, for --tesseract-image.
    #[arg(long, default_value = "podman")]
    podman_bin: PathBuf,
    /// The engine runtime's Podman API socket (`podman --remote --url`,
    /// ADR-0016), instead of the worker's own user's podman. Needs
    /// --scratch-dir.
    #[arg(long, env = "USNM_OCR_PODMAN_URL")]
    podman_url: Option<String>,
    /// With --podman-url: the size-capped tmpfs that pages' scratch
    /// directories go in, which the engine account's containers can mount.
    #[arg(long, env = "USNM_OCR_SCRATCH_DIR")]
    scratch_dir: Option<PathBuf>,
    /// With --podman-url: the engine container's memory limit, with no
    /// swap.
    #[arg(long, env = "USNM_OCR_ENGINE_MEMORY_MIB", default_value_t = Limits::default().memory_mib, value_parser = clap::value_parser!(u32).range(64..))]
    engine_memory_mib: u32,
    /// With --podman-url: CPUs for the engine container.
    #[arg(long, env = "USNM_OCR_ENGINE_CPUS", default_value_t = Limits::default().cpus, value_parser = clap::value_parser!(u32).range(1..))]
    engine_cpus: u32,
    /// With --podman-url: the engine container's process limit.
    #[arg(long, env = "USNM_OCR_ENGINE_PIDS", default_value_t = Limits::default().pids, value_parser = clap::value_parser!(u32).range(8..))]
    engine_pids: u32,
    /// Tesseract page segmentation mode (3: automatic, no orientation detection).
    #[arg(long, default_value_t = 3)]
    psm: u8,
    /// Resolution Tesseract assumes for the scan (IIIF JPEGs carry none).
    #[arg(long, default_value_t = 300)]
    dpi: u32,
    /// OpenMP threads per page (1 when pages run in parallel).
    #[arg(long, default_value_t = 1)]
    threads_per_page: u32,
    /// Give up on a page after this long.
    #[arg(long, env = "USNM_OCR_PAGE_TIMEOUT_SECS", default_value_t = 600)]
    page_timeout_secs: u64,
}

impl EngineArgs {
    async fn engine(&self) -> anyhow::Result<Arc<dyn Engine>> {
        match self.engine {
            EngineName::Tesseract => {
                let runner = match self.tesseract_image.as_deref().filter(|s| !s.is_empty()) {
                    Some(image) => {
                        let remote = match self.podman_url.as_deref().filter(|s| !s.is_empty()) {
                            Some(url) => Some(Remote {
                                url: url.to_owned(),
                                scratch: self
                                    .scratch_dir
                                    .clone()
                                    .filter(|p| !p.as_os_str().is_empty())
                                    .context("--podman-url needs --scratch-dir")?,
                                limits: Limits {
                                    memory_mib: self.engine_memory_mib,
                                    cpus: self.engine_cpus,
                                    pids: self.engine_pids,
                                },
                            }),
                            None => None,
                        };
                        Runner::Podman {
                            podman: self.podman_bin.clone(),
                            remote,
                            image: image.to_owned(),
                        }
                    }
                    None => Runner::Host(self.tesseract_bin.clone()),
                };
                let t = Tesseract::detect(TesseractOptions {
                    runner,
                    psm: self.psm,
                    dpi: self.dpi,
                    threads: self.threads_per_page,
                    timeout: Duration::from_secs(self.page_timeout_secs),
                })
                .await?;
                tracing::info!(
                    version = t.version(),
                    runner = ?t.runner(),
                    models = ?t.installed(),
                    "tesseract found"
                );
                Ok(Arc::new(t))
            }
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Take pages from the queues and OCR them until stopped.
    Run {
        /// `https://<account>.queue.core.windows.net`, or a directory.
        #[arg(long, env = "USNM_OCR_QUEUE_URL")]
        queue_url: String,
        /// `https://<account>.blob.core.windows.net/<container>`, or a directory.
        #[arg(long, env = "USNM_OCR_OUT_URL")]
        out_url: String,
        /// This node's name, in part names and logs.
        #[arg(long, env = "USNM_OCR_NODE_ID")]
        node_id: String,
        /// Pages in flight at once.
        #[arg(long, env = "USNM_OCR_CONCURRENCY", default_value_t = 1)]
        concurrency: usize,
        /// Stop once both queues are empty.
        #[arg(long)]
        once: bool,
        /// Gap between loc.gov API requests, shared by all pages on this
        /// node. With several nodes, multiply by their number.
        #[arg(long, env = "USNM_OCR_API_GAP_MS", default_value_t = 3500)]
        api_gap_ms: u64,
        /// Gap between image requests to LoC's IIIF server.
        #[arg(long, env = "USNM_OCR_TILE_GAP_MS", default_value_t = 1000)]
        tile_gap_ms: u64,
        /// The scan server on pve-big (`http://<host>:8380`): a page's scan
        /// is read from there when it checks out against its sidecar, else
        /// fetched from LoC. Unset: no cache.
        #[arg(long, env = "USNM_OCR_SCAN_CACHE_URL")]
        scan_cache_url: Option<String>,
        /// How long a taken message stays hidden. It isn't renewed, so it
        /// must cover the slowest page; a page not done by 80% of it is
        /// abandoned without output.
        #[arg(long, env = "USNM_OCR_VISIBILITY_SECS", default_value_t = 1800, value_parser = clap::value_parser!(u64).range(60..=604_800))]
        visibility_secs: u64,
        /// How long in-flight pages may take after SIGTERM before they are
        /// abandoned (they reappear when their visibility runs out).
        #[arg(long, default_value_t = 60)]
        shutdown_grace_secs: u64,
        /// Also publish the scan server's fetched list for this prefetch
        /// manifest version to `ocr-out/fetched/` every
        /// --publish-every-secs, for the seeder (12 Step 11). Needs
        /// --scan-cache-url. Set on one node.
        #[arg(long, env = "USNM_OCR_PUBLISH_FETCHED")]
        publish_fetched: Option<String>,
        #[arg(long, env = "USNM_OCR_PUBLISH_EVERY_SECS", default_value_t = 1800, value_parser = clap::value_parser!(u64).range(60..))]
        publish_every_secs: u64,
        /// Serve this worker's status to the LAN dashboard here
        /// (`0.0.0.0:8381`): `GET /status` and `GET /journal`. Unset: none.
        #[arg(long, env = "USNM_OCR_STATUS_LISTEN")]
        status_listen: Option<SocketAddr>,
        /// The only hosts the status is served to (the dashboard's), as
        /// names or addresses, comma-separated. Names are resolved each
        /// time the server starts.
        #[arg(long, env = "USNM_OCR_STATUS_ALLOW", value_delimiter = ',')]
        status_allow: Vec<String>,
        /// The journal of finished pages (doc_id, language, engine tag,
        /// times, outcome; no text), appended to across restarts.
        #[arg(long, env = "USNM_OCR_STATUS_JOURNAL")]
        status_journal: Option<PathBuf>,
        #[command(flatten)]
        engine: EngineArgs,
    },
    /// Publish the scan server's fetched list once (12 Step 11): read
    /// `<scan server>/index/full.jsonl` and write the full-size scans'
    /// pages to `ocr-out/fetched/<manifest>/<node>.<stamp>.parquet`.
    PublishFetched {
        /// The scan server, `http://<host>:8380`.
        #[arg(long, env = "USNM_OCR_SCAN_CACHE_URL")]
        scan_cache_url: String,
        /// `https://<account>.blob.core.windows.net/ocr-out`, or a directory.
        #[arg(long, env = "USNM_OCR_OUT_URL")]
        out_url: String,
        /// This node's name, in the list's path.
        #[arg(long, env = "USNM_OCR_NODE_ID")]
        node_id: String,
        /// The prefetch manifest's index version (`pages-v20261010-2`).
        #[arg(long, env = "USNM_OCR_PUBLISH_FETCHED")]
        manifest: String,
    },
    /// Fetch the scans a prefetch manifest lists onto local disk (#368),
    /// with the worker's LoC client and pacing, until done, stopped,
    /// `--max-total-bytes` is reached or 20 pages fail in a row. Resumable:
    /// scans already there and matching their sidecar are skipped.
    Prefetch {
        /// The manifest (`manifest.parquet` from `usnm-ingest
        /// ocr-prefetch-manifest`).
        #[arg(long, env = "USNM_PREFETCH_MANIFEST")]
        manifest: PathBuf,
        /// Where the scans go: `<out>/<keep|buffer>/<batch>/<lccn>/<date>/<ed>/<seq>.<ext>`.
        #[arg(long, env = "USNM_PREFETCH_OUT")]
        out: PathBuf,
        /// The keep set's resolution: `full`, or `scaled:<1-99>` (IIIF `pct:<n>`).
        #[arg(long, env = "USNM_PREFETCH_RESOLUTION", default_value = "full")]
        resolution: ImageSize,
        /// The buffer set's resolution.
        #[arg(long, env = "USNM_PREFETCH_BUFFER_RESOLUTION", default_value = "full")]
        buffer_resolution: ImageSize,
        /// `keep` (non-English pages and the worst English batches, kept
        /// for good), `buffer` (the rest of English, emptied by the
        /// workers) or `both` (keep first).
        #[arg(long, env = "USNM_PREFETCH_SETS", value_enum, default_value = "both")]
        sets: PrefetchSets,
        /// Wait while the buffer set's scans on disk reach this many bytes.
        #[arg(
            long,
            env = "USNM_PREFETCH_BUFFER_CAP",
            default_value_t = 1_000_000_000_000
        )]
        buffer_cap: u64,
        /// Only the first N pages (after --sample and the order).
        #[arg(long)]
        limit: Option<usize>,
        /// About N pages, stratified by language and decade.
        #[arg(long)]
        sample: Option<usize>,
        /// `priority` (German before the 1920s, the worst English batches,
        /// Czech, Serbian, Russian, the rest) or `issue` (the manifest's).
        #[arg(
            long,
            env = "USNM_PREFETCH_ORDER",
            value_enum,
            default_value = "priority"
        )]
        order: PrefetchOrder,
        /// Stop starting pages once this run's scans on disk add up to this.
        #[arg(long, env = "USNM_PREFETCH_MAX_TOTAL_BYTES")]
        max_total_bytes: Option<u64>,
        /// Pages in flight at once; they share the pacers.
        #[arg(long, env = "USNM_PREFETCH_CONCURRENCY", default_value_t = 4)]
        concurrency: usize,
        /// The lock that keeps it to one fetching process (default
        /// `<out>/.prefetch.lock`).
        #[arg(long, env = "USNM_PREFETCH_LOCK")]
        lock_file: Option<PathBuf>,
        /// A status write and progress line every N pages.
        #[arg(long, default_value_t = 100)]
        progress_every: u64,
        /// Gap between loc.gov API requests.
        #[arg(long, env = "USNM_OCR_API_GAP_MS", default_value_t = 3500)]
        api_gap_ms: u64,
        /// Gap between image requests to LoC's IIIF server.
        #[arg(long, env = "USNM_OCR_TILE_GAP_MS", default_value_t = 1000)]
        tile_gap_ms: u64,
    },
    /// Serve the prefetched scans to the OCR nodes over HTTP (#368): GET
    /// scans and sidecars, DELETE buffer-set scans, PUT manifests, from the
    /// --allow hosts only. Runs in the scan-fetch container on pve-big.
    ServeScans {
        /// `/tank/ocr/scans`, holding `keep/` and `buffer/`.
        #[arg(long, env = "USNM_SCANSERVE_SCANS", default_value = "/tank/ocr/scans")]
        scans: PathBuf,
        /// `/tank/ocr/manifests`.
        #[arg(
            long,
            env = "USNM_SCANSERVE_MANIFESTS",
            default_value = "/tank/ocr/manifests"
        )]
        manifests: PathBuf,
        /// Address and port to listen on.
        #[arg(long, env = "USNM_SCANSERVE_LISTEN", default_value = "0.0.0.0:8380")]
        listen: std::net::SocketAddr,
        /// The only clients served (the OCR nodes), by host name,
        /// comma-separated. Resolved at startup, which fails if one doesn't
        /// resolve, and again every --allow-refresh-secs.
        #[arg(
            long,
            env = "USNM_SCANSERVE_ALLOW",
            value_delimiter = ',',
            required = true
        )]
        allow: Vec<String>,
        /// How often the --allow names are resolved again.
        #[arg(long, env = "USNM_SCANSERVE_ALLOW_REFRESH_SECS", default_value_t = 300)]
        allow_refresh_secs: u64,
    },
    /// Project a manifest's size and fetch time from a sample's index
    /// (`index-<tag>.jsonl` from `prefetch --sample`). Prints JSON.
    PrefetchEstimate {
        /// The whole manifest to project for.
        #[arg(long)]
        manifest: PathBuf,
        /// The sample run's index file.
        #[arg(long)]
        index: PathBuf,
        #[arg(long, default_value_t = 3500)]
        api_gap_ms: u64,
        #[arg(long, default_value_t = 1000)]
        tile_gap_ms: u64,
    },
    /// Queue one page (for testing).
    Enqueue {
        #[arg(long)]
        queue_url: String,
        /// `lccn/YYYY-MM-DD/ed-N/seq-N`.
        #[arg(long)]
        page: String,
        /// The LoC batch the page is in.
        #[arg(long)]
        batch: String,
        /// `user` queues to ocr-priority, `bulk` to ocr-bulk.
        #[arg(long, value_enum)]
        priority: Priority,
        /// ISO 639-3.
        #[arg(long)]
        language: String,
        /// The page's damage score, if known.
        #[arg(long)]
        score: Option<f64>,
    },
    /// OCR one image file (for benchmarking an engine on a node).
    OcrFile {
        #[arg(long)]
        image: PathBuf,
        /// ISO 639-3.
        #[arg(long)]
        language: String,
        /// Directory for `<image stem>.txt` (normalized) and `<image stem>.xml` (ALTO).
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        engine: EngineArgs,
    },
    /// Read a rendered line of English with the engine and fail unless it
    /// comes back: a node's check before the unit is enabled.
    CheckEngine {
        #[command(flatten)]
        engine: EngineArgs,
    },
}

/// The line `check-engine` renders and must read back.
const CHECK_TEXT: &str = "HELLO WORLD";

#[derive(Clone, Copy, ValueEnum)]
enum PrefetchSets {
    Keep,
    Buffer,
    Both,
}

#[derive(Clone, Copy, ValueEnum)]
enum PrefetchOrder {
    Priority,
    Issue,
}

#[derive(Clone, Copy, ValueEnum)]
enum Priority {
    User,
    Bulk,
}

/// Turns true on SIGTERM or Ctrl-C.
fn stop_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("stop requested");
        tx.send_replace(true);
    });
    rx
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    // `run` reports as its node; with no connection string, logs only.
    let node = match &cli.command {
        Command::Run { node_id, .. } if check_node_id(node_id).is_ok() => Some(node_id.as_str()),
        _ => None,
    };
    let telemetry = telemetry::init(node);
    let result = command(cli).await;
    let code = match &result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::info_span!("ocr_worker_exit")
                .in_scope(|| tracing::error!(error = format!("{e:#}"), "usnm-ocr-worker failed"));
            std::process::ExitCode::FAILURE
        }
    };
    // Flush what's buffered before exit, on every path.
    telemetry.shutdown().await;
    code
}

async fn command(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Run {
            queue_url,
            out_url,
            node_id,
            concurrency,
            once,
            api_gap_ms,
            tile_gap_ms,
            scan_cache_url,
            visibility_secs,
            shutdown_grace_secs,
            publish_fetched,
            publish_every_secs,
            status_listen,
            status_allow,
            status_journal,
            engine,
        } => {
            check_node_id(&node_id)?;
            let publish = match publish_fetched.filter(|m| !m.is_empty()) {
                Some(manifest) => Some(publish::Options {
                    scan_server: scan_cache_url
                        .clone()
                        .filter(|u| !u.is_empty())
                        .context("--publish-fetched needs --scan-cache-url")?,
                    manifest,
                    node_id: node_id.clone(),
                }),
                None => None,
            };
            let cfg = Config {
                node_id,
                concurrency: concurrency.max(1),
                once,
                visibility: Duration::from_secs(visibility_secs),
                shutdown_grace: Duration::from_secs(shutdown_grace_secs),
                ..Config::default()
            };
            if Duration::from_secs(engine.page_timeout_secs) >= cfg.budget() {
                let _span = tracing::info_span!("ocr_worker_start").entered();
                tracing::warn!(
                    page_timeout_secs = engine.page_timeout_secs,
                    budget_secs = cfg.budget().as_secs(),
                    "--page-timeout-secs is over 80% of --visibility-secs; slow pages will be abandoned"
                );
            }
            let scan_cache_url = scan_cache_url.filter(|u| !u.is_empty());
            let scans: Arc<dyn ScanSource> = {
                let cfg = LocConfig {
                    api_gap: Duration::from_millis(api_gap_ms),
                    tile_gap: Duration::from_millis(tile_gap_ms),
                    ..LocConfig::default()
                };
                let iiif_root = cfg.iiif_root.clone();
                let loc: Arc<dyn ScanSource> = Arc::new(LocClient::new(cfg)?);
                match scan_cache_url.as_deref() {
                    Some(url) => {
                        tracing::info_span!("ocr_worker_start")
                            .in_scope(|| tracing::info!(url, "reading prefetched scans first"));
                        Arc::new(CachedScans::new(url, iiif_root, loc)?)
                    }
                    None => loc,
                }
            };
            let engine = engine.engine().await?;
            let tracker = Arc::new(Tracker::new(
                About {
                    node_id: cfg.node_id.clone(),
                    version: VERSION.clone(),
                    concurrency: u32::try_from(cfg.concurrency).unwrap_or(u32::MAX),
                    engine: engine.source().to_owned(),
                    scan_cache_url: scan_cache_url.clone(),
                },
                status_journal.filter(|p| !p.as_os_str().is_empty()),
                Some(scans.clone()),
            )?);
            let stop = stop_signal();
            let queue = queue::open(&queue_url)?;
            if let Some(listen) = status_listen {
                anyhow::ensure!(
                    !status_allow.is_empty(),
                    "--status-listen needs --status-allow (the dashboard's address)"
                );
                // The queues' lengths and the front of the dead letters, for
                // the dashboard: a look a minute, peeks only.
                {
                    let (tracker, queue) = (tracker.clone(), queue.clone());
                    tokio::spawn(async move {
                        let mut every = tokio::time::interval(status::QUEUE_LOOK_EVERY);
                        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        loop {
                            every.tick().await;
                            tracker.set_queues(status::look_at_queues(queue.as_ref()).await);
                        }
                    });
                }
                // Supervised apart from the OCR loop, which runs whatever
                // happens to it.
                status::supervise(
                    listen,
                    tracker.clone(),
                    status_allow,
                    stop.clone(),
                    Metrics::global(),
                    status::RESTART_FIRST,
                );
            }
            let worker = Arc::new(Worker {
                queue,
                out: usnm_store::open(&out_url)?,
                scans,
                engine,
                cfg,
                metrics: Metrics::global(),
                tracker,
            });
            if let Some(opts) = publish {
                tracing::info_span!("ocr_worker_start").in_scope(|| {
                    tracing::info!(
                        manifest = opts.manifest,
                        every_secs = publish_every_secs,
                        "publishing the fetched list"
                    )
                });
                tokio::spawn(publish::every(
                    Arc::clone(&worker.out),
                    opts,
                    Duration::from_secs(publish_every_secs),
                    stop.clone(),
                ));
            }
            worker.run(stop).await
        }
        Command::PublishFetched {
            scan_cache_url,
            out_url,
            node_id,
            manifest,
        } => {
            check_node_id(&node_id)?;
            let out = usnm_store::open(&out_url)?;
            let p = publish::publish(
                out.as_ref(),
                &publish::Options {
                    scan_server: scan_cache_url,
                    manifest,
                    node_id,
                },
            )
            .await?;
            println!(
                "{}",
                serde_json::json!({"path": p.path, "lines": p.lines, "pages": p.pages, "skipped": p.skipped})
            );
            Ok(())
        }
        Command::Prefetch {
            manifest,
            out,
            resolution,
            buffer_resolution,
            sets,
            buffer_cap,
            limit,
            sample,
            order,
            max_total_bytes,
            concurrency,
            lock_file,
            progress_every,
            api_gap_ms,
            tile_gap_ms,
        } => {
            let loc = Arc::new(LocClient::new(LocConfig {
                api_gap: Duration::from_millis(api_gap_ms),
                tile_gap: Duration::from_millis(tile_gap_ms),
                ..LocConfig::default()
            })?);
            let opts = prefetch::Options {
                manifest,
                out,
                size: resolution,
                buffer_size: buffer_resolution,
                sets: match sets {
                    PrefetchSets::Keep => prefetch::Sets::Keep,
                    PrefetchSets::Buffer => prefetch::Sets::Buffer,
                    PrefetchSets::Both => prefetch::Sets::Both,
                },
                buffer_cap,
                limit,
                sample,
                concurrency: concurrency.max(1),
                progress_every,
                order: match order {
                    PrefetchOrder::Priority => prefetch::Order::Priority,
                    PrefetchOrder::Issue => prefetch::Order::Issue,
                },
                max_total_bytes,
                lock_file,
                ..prefetch::Options::default()
            };
            let status = prefetch::run(opts, loc, stop_signal()).await?;
            println!("{}", serde_json::to_string(&status)?);
            Ok(())
        }
        Command::ServeScans {
            scans,
            manifests,
            listen,
            allow,
            allow_refresh_secs,
        } => {
            let allow = usnm_ocr_worker::serve::AllowList::resolve(&allow).await?;
            usnm_ocr_worker::serve::serve(
                listen,
                usnm_ocr_worker::serve::ServeOptions {
                    scans,
                    manifests,
                    allow: Arc::new(allow),
                },
                std::time::Duration::from_secs(allow_refresh_secs),
                stop_signal(),
            )
            .await
        }
        Command::PrefetchEstimate {
            manifest,
            index,
            api_gap_ms,
            tile_gap_ms,
        } => {
            let rows = usnm_ocr_contract::prefetch::decode(bytes::Bytes::from(
                std::fs::read(&manifest)
                    .with_context(|| format!("reading {}", manifest.display()))?,
            ))?;
            let index = prefetch::read_index(
                &std::fs::read_to_string(&index)
                    .with_context(|| format!("reading {}", index.display()))?,
            )?;
            let e = prefetch::estimate(
                &rows,
                &index,
                Duration::from_millis(api_gap_ms),
                Duration::from_millis(tile_gap_ms),
            )?;
            println!("{}", serde_json::to_string_pretty(&e)?);
            Ok(())
        }
        Command::Enqueue {
            queue_url,
            page,
            batch,
            priority,
            language,
            score,
        } => {
            let (reason, queue_name) = match priority {
                Priority::User => (Reason::User, PRIORITY),
                Priority::Bulk => (Reason::Bulk, BULK),
            };
            let msg = OcrMessage {
                v: 1,
                page,
                batch,
                language,
                reason,
                score,
                queued_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            };
            msg.validate()?;
            queue::open(&queue_url)?
                .send(queue_name, &msg.encode())
                .await?;
            tracing::info!(page = %msg.page, queue = queue_name, "queued");
            Ok(())
        }
        Command::OcrFile {
            image,
            language,
            out,
            engine,
        } => {
            let engine = engine.engine().await?;
            let stem = image
                .file_stem()
                .and_then(|s| s.to_str())
                .context("--image has no file name")?
                .to_owned();
            std::fs::create_dir_all(&out)?;
            let work = tempfile::tempdir()?;
            let t0 = Instant::now();
            let result = engine.ocr(&image, &language, work.path()).await?;
            let secs = t0.elapsed().as_secs_f64();
            let text = usnm_core::text::normalize_ocr(&result.text);
            std::fs::write(out.join(format!("{stem}.txt")), &text)?;
            std::fs::write(out.join(format!("{stem}.xml")), &result.alto)?;
            let status = usnm_core::text::text_status(&text);
            println!(
                "{}",
                serde_json::json!({
                    "image": image.display().to_string(),
                    "language": language,
                    "engine": result.engine,
                    "secs": (secs * 1000.0).round() / 1000.0,
                    "chars": text.chars().count(),
                    "status": usnm_overlay::status_str(status),
                })
            );
            Ok(())
        }
        Command::CheckEngine { engine } => {
            let engine = engine.engine().await?;
            let work = tempfile::tempdir()?;
            let image = work.path().join("check.pgm");
            std::fs::write(&image, usnm_ocr_worker::testimg::text_image(CHECK_TEXT))?;
            let t0 = Instant::now();
            let result = engine.ocr(&image, "eng", work.path()).await?;
            let text = usnm_core::text::normalize_ocr(&result.text);
            anyhow::ensure!(
                text.contains(CHECK_TEXT),
                "the engine read {text:?}, not {CHECK_TEXT:?}"
            );
            anyhow::ensure!(
                String::from_utf8_lossy(&result.alto).contains("<alto"),
                "the engine wrote no ALTO"
            );
            println!(
                "{}",
                serde_json::json!({
                    "engine": result.engine,
                    "secs": (t0.elapsed().as_secs_f64() * 1000.0).round() / 1000.0,
                    "read": text.trim(),
                })
            );
            Ok(())
        }
    }
}
