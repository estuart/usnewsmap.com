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
    /// Inspect the local submission/publication journal; emits no page text.
    Status {
        #[arg(long)]
        root: PathBuf,
    },
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
