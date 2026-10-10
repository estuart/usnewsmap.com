//! `usnm-ocr-worker`: see the crate's README.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use tokio::sync::watch;
use usnm_ocr_worker::engine::{Engine, Tesseract, TesseractOptions};
use usnm_ocr_worker::loc::{LocClient, LocConfig};
use usnm_ocr_worker::message::{OcrMessage, Reason};
use usnm_ocr_worker::queue::{self, BULK, PRIORITY};
use usnm_ocr_worker::telemetry::{self, Metrics};
use usnm_ocr_worker::worker::{check_node_id, Config, Worker};

#[derive(Parser)]
#[command(
    version,
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
    /// The Tesseract binary.
    #[arg(long, default_value = "tesseract")]
    tesseract_bin: PathBuf,
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
    #[arg(long, default_value_t = 600)]
    page_timeout_secs: u64,
}

impl EngineArgs {
    async fn engine(&self) -> anyhow::Result<Arc<dyn Engine>> {
        match self.engine {
            EngineName::Tesseract => {
                let t = Tesseract::detect(TesseractOptions {
                    bin: self.tesseract_bin.clone(),
                    psm: self.psm,
                    dpi: self.dpi,
                    threads: self.threads_per_page,
                    timeout: Duration::from_secs(self.page_timeout_secs),
                })
                .await?;
                tracing::info!(
                    version = t.version(),
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
        #[arg(long)]
        queue_url: String,
        /// `https://<account>.blob.core.windows.net/<container>`, or a directory.
        #[arg(long)]
        out_url: String,
        /// This node's name, in part names and logs.
        #[arg(long)]
        node_id: String,
        /// Pages in flight at once.
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        /// Stop once both queues are empty.
        #[arg(long)]
        once: bool,
        /// Gap between loc.gov API requests, shared by all pages on this
        /// node. With several nodes, multiply by their number.
        #[arg(long, default_value_t = 3500)]
        api_gap_ms: u64,
        /// Gap between image requests to LoC's IIIF server.
        #[arg(long, default_value_t = 1000)]
        tile_gap_ms: u64,
        /// How long a taken message stays hidden. It isn't renewed, so it
        /// must cover the slowest page; a page not done by 80% of it is
        /// abandoned without output.
        #[arg(long, default_value_t = 1800, value_parser = clap::value_parser!(u64).range(60..=604_800))]
        visibility_secs: u64,
        /// How long in-flight pages may take after SIGTERM before they are
        /// abandoned (they reappear when their visibility runs out).
        #[arg(long, default_value_t = 60)]
        shutdown_grace_secs: u64,
        #[command(flatten)]
        engine: EngineArgs,
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
            visibility_secs,
            shutdown_grace_secs,
            engine,
        } => {
            check_node_id(&node_id)?;
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
            let worker = Arc::new(Worker {
                queue: queue::open(&queue_url)?,
                out: usnm_store::open(&out_url)?,
                scans: Arc::new(LocClient::new(LocConfig {
                    api_gap: Duration::from_millis(api_gap_ms),
                    tile_gap: Duration::from_millis(tile_gap_ms),
                    ..LocConfig::default()
                })?),
                engine: engine.engine().await?,
                cfg,
                metrics: Metrics::global(),
            });
            worker.run(stop_signal()).await
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
    }
}
