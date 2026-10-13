use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use std::{path::PathBuf, sync::Arc};
use usnm_boinc::{records, ExportOptions};
use usnm_state::{
    cosmos::CosmosDocs,
    docs::{DocStore, FileDocs},
    state::State,
};

#[derive(Parser)]
#[command(about = "Opt-in stock BOINC newspaper OCR batch pilot")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    /// Export selected cached pages and reserve their native requested-work ledger entries.
    Export {
        #[arg(long)]
        manifest: PathBuf,
        /// Text file: one canonical page key per line, at most 100 pages from one batch.
        #[arg(long)]
        pages: PathBuf,
        #[arg(long)]
        batch: String,
        #[arg(long)]
        scans: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        plan: String,
        #[arg(long)]
        image: String,
        #[arg(long, default_value_t = 600)]
        timeout_secs: u32,
        #[arg(long)]
        curated_url: String,
        #[arg(long)]
        reference_url: String,
        #[arg(long, required_unless_present = "cosmos", conflicts_with = "cosmos")]
        state_file: Option<PathBuf>,
        #[arg(long)]
        cosmos: Option<String>,
    },
    /// Run one page inside the pinned BOINC worker container.
    Work {
        #[arg(long, default_value = "/work")]
        dir: PathBuf,
    },
    /// Stage and submit one reserved job with stock create_work.
    Submit {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        job: String,
    },
    /// Reconcile an uncertain submission after checking the server for its workunit name.
    Reconcile {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        job: String,
        #[arg(long, value_enum)]
        found: Found,
    },
    /// Read stock batch_collect_assimilator's spool and publish native ALTO/Parquet.
    Collect {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        job: String,
        #[arg(long)]
        curated_url: String,
        #[arg(long)]
        out_url: String,
    },
    /// Verify the gold bundle and download the original files it refers to (network; outside the
    /// job container).
    GoldStage {
        /// The bundle directory, or its tar.
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        work: PathBuf,
    },
    /// Score this host's Tesseract on the gold bundle, offline; writes counts only, no text.
    GoldRun {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        work: PathBuf,
        /// Where gold-result.json goes.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value = "tesseract")]
        tesseract: PathBuf,
        #[arg(long, default_value_t = 3)]
        psm: u8,
        #[arg(long, default_value_t = 300)]
        dpi: u32,
        #[arg(long, default_value_t = 600)]
        timeout_secs: u32,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// The JP2 against IIIF JPEG assessment (#456), app `usnm_assess`.
    #[command(subcommand)]
    Assess(Assess),
    /// Inspect the local submission/publication journal; emits no page text.
    Status {
        #[arg(long)]
        root: PathBuf,
    },
}
#[derive(Subcommand)]
enum Assess {
    /// Draw the stratified OCR sample and the larger crash-only list.
    Sample {
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        out_ocr: PathBuf,
        #[arg(long)]
        out_crash: PathBuf,
        #[arg(long, default_value_t = 3000)]
        ocr_pages: usize,
        #[arg(long, default_value_t = 20000)]
        crash_pages: usize,
        #[arg(long, default_value_t = 2)]
        min_per_stratum: usize,
        #[arg(long)]
        seed: String,
        /// JSON object: LCCN to the title's MARC language codes.
        #[arg(long)]
        title_languages: Option<PathBuf>,
    },
    /// Fetch sampled pages from LoC (one request at a time) into unit directories.
    Stage {
        #[arg(long)]
        sample: PathBuf,
        #[arg(long)]
        root: PathBuf,
        #[arg(long, value_enum)]
        mode: AssessMode,
        /// The worker image's full digest (sha256:...), the pinned engine build.
        #[arg(long)]
        engine_image: String,
        /// The Tesseract image the worker image was built on (its TESSERACT_IMAGE).
        #[arg(long)]
        engine_base: String,
        #[arg(long, default_value = "5.5.0")]
        engine_version: String,
        /// Page deadline; defaults to 1200 for ocr and 20 for crash.
        #[arg(long)]
        timeout_secs: Option<u32>,
        #[arg(long, default_value_t = 5)]
        gap_secs: u64,
        #[arg(long, default_value_t = 100)]
        max_pages: usize,
        #[arg(long)]
        batch_cache: Option<PathBuf>,
    },
    /// Submit staged units with stock create_work, two results each.
    Submit {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        project: PathBuf,
        #[arg(long, value_enum)]
        mode: AssessMode,
        #[arg(long, default_value_t = 500)]
        max_units: usize,
    },
    /// script_validator's init script: check one result's files.
    Check {
        #[arg(long, num_args = 3, required = true)]
        files: Vec<PathBuf>,
    },
    /// script_validator's compare script: do two results agree?
    Compare {
        #[arg(long, num_args = 3, required = true)]
        files: Vec<PathBuf>,
        #[arg(long, num_args = 3, required = true)]
        files2: Vec<PathBuf>,
    },
    /// Write score.py/aggregate.py inputs from the canonical results.
    Collect {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum AssessMode {
    Ocr,
    Crash,
}
impl From<AssessMode> for usnm_ocr_contract::assess::Mode {
    fn from(m: AssessMode) -> Self {
        match m {
            AssessMode::Ocr => Self::Ocr,
            AssessMode::Crash => Self::Crash,
        }
    }
}
#[derive(Clone, Copy, ValueEnum)]
enum Found {
    Present,
    Absent,
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Commands::Export {
            manifest,
            pages,
            batch,
            scans,
            root,
            plan,
            image,
            timeout_secs,
            curated_url,
            reference_url,
            state_file,
            cosmos,
        } => {
            let docs: Arc<dyn DocStore> = if let Some(endpoint) = cosmos {
                Arc::new(CosmosDocs::new(
                    &endpoint,
                    usnm_state::DATABASE,
                    usnm_store::credential::from_env_for(usnm_store::credential::COSMOS_RESOURCE),
                )?)
            } else {
                Arc::new(FileDocs::open(
                    state_file.context("--state-file required")?,
                )?)
            };
            let store = usnm_store::open(&curated_url)?;
            let reference = usnm_store::open(&reference_url)?;
            for id in usnm_boinc::export(
                &ExportOptions {
                    manifest,
                    pages,
                    batch,
                    scans,
                    root,
                    plan,
                    image,
                    timeout_secs,
                },
                &State::new(docs),
                store.as_ref(),
                reference.as_ref(),
            )
            .await?
            {
                println!("{id}");
            }
        }
        Commands::Work { dir } => usnm_boinc::worker::run(&dir).await?,
        Commands::Submit { root, project, job } => {
            usnm_boinc::submit::submit(&root, &project, &job).await?
        }
        Commands::Reconcile { root, job, found } => {
            usnm_boinc::submit::reconcile(&root, &job, matches!(found, Found::Present))?
        }
        Commands::Collect {
            root,
            spool,
            job,
            curated_url,
            out_url,
        } => {
            let curated = usnm_store::open(&curated_url)?;
            let out = usnm_store::open(&out_url)?;
            usnm_boinc::collect(&root, &spool, &job, curated.as_ref(), out.as_ref()).await?;
        }
        Commands::GoldStage {
            bundle,
            cache,
            work,
        } => {
            let s = usnm_boinc::gold::stage(&bundle, &cache, &work).await?;
            println!("{}", serde_json::to_string(&s)?);
            anyhow::ensure!(s.failed.is_empty(), "{} downloads failed", s.failed.len());
        }
        Commands::GoldRun {
            bundle,
            cache,
            work,
            out,
            tesseract,
            psm,
            dpi,
            timeout_secs,
            limit,
        } => {
            let report = usnm_boinc::gold::run(&usnm_boinc::gold::RunOptions {
                bundle,
                cache,
                work,
                tesseract,
                psm,
                dpi,
                timeout_secs,
                limit,
            })
            .await?;
            usnm_boinc::atomic(&out, &serde_json::to_vec_pretty(&report)?)?;
            println!("{}", serde_json::to_string(&report.summary)?);
        }
        Commands::Assess(a) => assess(a).await?,
        Commands::Status { root } => {
            for r in records(&root)? {
                println!(
                    "{}",
                    serde_json::json!({"job":r.job.id,"workunit":r.job.workunit(),"page":r.job.message.page,"stage":r.stage,"error":r.error})
                );
            }
        }
    }
    Ok(())
}

async fn assess(a: Assess) -> anyhow::Result<()> {
    use usnm_boinc::assess as x;
    match a {
        Assess::Sample {
            manifest,
            out_ocr,
            out_crash,
            ocr_pages,
            crash_pages,
            min_per_stratum,
            seed,
            title_languages,
        } => {
            let s = x::sample::run(&x::sample::Options {
                manifest,
                out_ocr,
                out_crash,
                ocr_pages,
                crash_pages,
                min_per_stratum,
                seed,
                title_languages,
            })?;
            eprintln!("{s:?}");
        }
        Assess::Stage {
            sample,
            root,
            mode,
            engine_image,
            engine_base,
            engine_version,
            timeout_secs,
            gap_secs,
            max_pages,
            batch_cache,
        } => {
            let mode = mode.into();
            let s = x::stage::run(&x::stage::Options {
                sample,
                root,
                mode,
                engine_image,
                engine_base,
                engine_version,
                timeout_secs: timeout_secs.unwrap_or(match mode {
                    usnm_ocr_contract::assess::Mode::Ocr => 1200,
                    usnm_ocr_contract::assess::Mode::Crash => 20,
                }),
                gap: std::time::Duration::from_secs(gap_secs),
                max_pages,
                batch_cache,
            })
            .await?;
            eprintln!("{s:?}");
        }
        Assess::Submit {
            root,
            project,
            mode,
            max_units,
        } => {
            let s = x::submit::run(&x::submit::Options {
                root,
                project,
                mode: mode.into(),
                max_units,
            })
            .await?;
            eprintln!("{s:?}");
        }
        Assess::Check { files } => x::validate::check(&files)?,
        Assess::Compare { files, files2 } => x::validate::compare(&files, &files2)?,
        Assess::Collect { root, spool, out } => {
            let s = x::collect::run(&x::collect::Options { root, spool, out })?;
            eprintln!("{s:?}");
        }
    }
    Ok(())
}
