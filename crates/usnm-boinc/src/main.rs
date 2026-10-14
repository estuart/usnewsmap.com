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
        /// trust.json from `gold trust`: publish only when --host-id passed the gold check for
        /// the job's engine image; otherwise hold the result in <root>/quarantine/<job>/.
        #[arg(long)]
        trust: Option<PathBuf>,
        /// The BOINC host id of the canonical result (the result table's hostid).
        #[arg(long, requires = "trust")]
        host_id: Option<u64>,
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
    /// The gold check as BOINC work (#462), app `usnm_gold`.
    #[command(subcommand)]
    Gold(Gold),
    /// The Surya 2 test on volunteer CPUs and GPUs (#516), app `usnm_surya`.
    #[command(subcommand)]
    Surya(Surya),
    /// The remote hand-off between the OCR node and the BOINC server (#469, docs/boinc.md).
    #[command(subcommand)]
    Handoff(Handoff),
    /// Inspect the local submission/publication journal; emits no page text.
    Status {
        #[arg(long)]
        root: PathBuf,
    },
}
#[derive(Subcommand)]
enum Handoff {
    /// Copy every object under a prefix of the hand-off container into a directory.
    Get {
        #[arg(long)]
        from_url: String,
        #[arg(long)]
        prefix: String,
        #[arg(long)]
        to: PathBuf,
    },
    /// Write every file under a directory to a prefix of the hand-off container.
    Put {
        #[arg(long)]
        from: PathBuf,
        #[arg(long)]
        to_url: String,
        #[arg(long)]
        prefix: String,
    },
    /// Server side: copy one exported job into the hand-off root. Prints `reserved` (submit
    /// it) or the stage the server already had.
    Pull {
        #[arg(long)]
        from_url: String,
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        job: String,
    },
    /// Server side: write a job's spool files, its host id and trust.json to the container.
    PutSpool {
        #[arg(long)]
        to_url: String,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        job: String,
        #[arg(long)]
        host_id: Option<u64>,
        #[arg(long)]
        trust: Option<PathBuf>,
    },
    /// Hand-off job side: copy a job's spool and trust.json (when there) down; prints the
    /// host id, or `unknown`.
    GetSpool {
        #[arg(long)]
        from_url: String,
        #[arg(long)]
        job: String,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        trust_out: PathBuf,
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
#[derive(Subcommand)]
enum Gold {
    /// Server side, once per bundle: fetch the original files (one request at a time) to learn
    /// their sizes and md5s and the crops' reference counts; writes prepared.json.
    Prepare {
        /// The bundle tar.
        #[arg(long)]
        bundle: PathBuf,
        /// The campaign root.
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 5)]
        gap_secs: u64,
    },
    /// Make units with stock create_work, one result each.
    Submit {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        project: PathBuf,
        /// How many units the campaign has; units already submitted are skipped.
        #[arg(long, default_value_t = 20)]
        units: u32,
        /// The worker image's full digest (sha256:...).
        #[arg(long)]
        engine_image: String,
        /// The Tesseract image the worker image was built on (its TESSERACT_IMAGE).
        #[arg(long)]
        engine_base: String,
        #[arg(long, default_value = "5.5.0")]
        engine_version: String,
        /// Per crop.
        #[arg(long, default_value_t = 300)]
        timeout_secs: u32,
    },
    /// script_validator's init script: check one result against the bundle's expectations.
    Check {
        #[arg(long)]
        file: PathBuf,
        /// The project's gold/ directory, which submit writes.
        #[arg(long)]
        expected_dir: PathBuf,
    },
    /// Write per-host report tables from the canonical results.
    Collect {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        spool: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build trust.json for `collect --trust` from one or more reports.jsonl files.
    Trust {
        #[arg(long, required = true)]
        reports: Vec<PathBuf>,
        /// The highest overall CER that passes.
        #[arg(long, default_value_t = usnm_boinc::trust::DEFAULT_MAX_CER)]
        max_cer: f64,
        #[arg(long)]
        out: PathBuf,
    },
}
#[derive(Subcommand)]
enum Surya {
    /// Fetch each page's full-size IIIF JPEG from LoC, one request at a time, and write its unit.
    Stage {
        /// group:lang:doc_id, e.g. serbian:srp:sn85055101_1917-04-03_ed-1_seq-3; repeat.
        #[arg(long = "page", required = true)]
        pages: Vec<usnm_boinc::surya::Page>,
        #[arg(long)]
        root: PathBuf,
        /// The worker image's registry digest (sha256:...).
        #[arg(long)]
        engine_image: String,
        /// The llama.cpp server image it is built on (its USNM_SURYA_LLAMA_IMAGE).
        #[arg(long)]
        engine_llama: String,
        /// Its USNM_SURYA_WEIGHTS.
        #[arg(long)]
        engine_weights: String,
        #[arg(long, default_value = "0.22.1")]
        engine_surya: String,
        #[arg(long, value_delimiter = ',', default_value = "tiled,whole,capped")]
        gpu_modes: Vec<SuryaMode>,
        #[arg(long, value_delimiter = ',', default_value = "tiled")]
        cpu_modes: Vec<SuryaMode>,
        #[arg(long, default_value_t = 1800)]
        gpu_timeout_secs: u32,
        #[arg(long, default_value_t = 5400)]
        cpu_timeout_secs: u32,
        #[arg(long, default_value_t = 5)]
        gap_secs: u64,
        #[arg(long, default_value_t = 20)]
        max_pages: usize,
    },
    /// Hand staged units to create_work, two results each.
    Submit {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        project: PathBuf,
        #[arg(long, default_value_t = 20)]
        max_units: usize,
    },
    /// Write a unit from a local image, to check a host or the image without LoC or BOINC.
    /// Inside the worker image the engine values default to the image's own.
    LocalUnit {
        #[arg(long)]
        image: PathBuf,
        #[arg(long)]
        dir: PathBuf,
        #[arg(
            long,
            default_value = "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        )]
        engine_image: String,
        #[arg(long, env = "USNM_SURYA_LLAMA_IMAGE")]
        engine_llama: String,
        #[arg(long, env = "USNM_SURYA_WEIGHTS")]
        engine_weights: String,
        #[arg(long, env = "USNM_SURYA_VERSION", default_value = "0.22.1")]
        engine_surya: String,
        #[arg(long, value_delimiter = ',', default_value = "whole")]
        modes: Vec<SuryaMode>,
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u32,
    },
    /// script_validator's init script: check one result (five files); --keep copies it there.
    Check {
        #[arg(long, num_args = 5, required = true)]
        files: Vec<PathBuf>,
        #[arg(long)]
        keep: Option<PathBuf>,
    },
    /// script_validator's compare script: two results of one unit; --log appends the pair.
    Compare {
        #[arg(long, num_args = 5, required = true)]
        files: Vec<PathBuf>,
        #[arg(long, num_args = 5, required = true)]
        files2: Vec<PathBuf>,
        #[arg(long, default_value_t = usnm_ocr_contract::surya::PROBE_MIN_DICE)]
        min_dice: f64,
        #[arg(long)]
        log: Option<PathBuf>,
    },
    /// Print a Markdown table of the kept results and the logged pairs.
    Summary {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        seen: PathBuf,
        #[arg(long)]
        pairs: PathBuf,
    },
}
#[derive(Clone, Copy, ValueEnum)]
enum SuryaMode {
    Tiled,
    Whole,
    Capped,
}
impl From<SuryaMode> for usnm_ocr_contract::surya::InputMode {
    fn from(m: SuryaMode) -> Self {
        match m {
            SuryaMode::Tiled => Self::Tiled,
            SuryaMode::Whole => Self::Whole,
            SuryaMode::Capped => Self::Capped,
        }
    }
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
            trust,
            host_id,
        } => {
            let curated = usnm_store::open(&curated_url)?;
            let out = usnm_store::open(&out_url)?;
            if let Some(trust) = trust {
                let t = usnm_boinc::trust::Trust::load(&trust)?;
                usnm_boinc::trust::collect(
                    &root,
                    &spool,
                    &job,
                    &t,
                    host_id,
                    curated.as_ref(),
                    out.as_ref(),
                )
                .await?;
            } else {
                usnm_boinc::collect(&root, &spool, &job, curated.as_ref(), out.as_ref()).await?;
            }
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
        Commands::Gold(g) => gold(g).await?,
        Commands::Surya(x) => surya(x).await?,
        Commands::Handoff(h) => handoff(h).await?,
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

async fn handoff(h: Handoff) -> anyhow::Result<()> {
    use usnm_boinc::handoff as x;
    match h {
        Handoff::Get {
            from_url,
            prefix,
            to,
        } => {
            let n = x::get(usnm_store::open(&from_url)?.as_ref(), &prefix, &to).await?;
            eprintln!("{n} objects copied from {prefix}/ to {}", to.display());
        }
        Handoff::Put {
            from,
            to_url,
            prefix,
        } => {
            let n = x::put(usnm_store::open(&to_url)?.as_ref(), &from, &prefix).await?;
            eprintln!("{n} files written from {} to {prefix}/", from.display());
        }
        Handoff::Pull {
            from_url,
            root,
            job,
        } => match x::pull(usnm_store::open(&from_url)?.as_ref(), &root, &job).await? {
            x::Pulled::Reserved => println!("reserved"),
            x::Pulled::Already(stage) => {
                println!("{}", serde_json::to_value(stage)?.as_str().unwrap_or("?"))
            }
        },
        Handoff::PutSpool {
            to_url,
            spool,
            job,
            host_id,
            trust,
        } => {
            let store = usnm_store::open(&to_url)?;
            let n = x::put_spool(store.as_ref(), &spool, &job, host_id, trust.as_deref()).await?;
            eprintln!("{n} spool files written for {job}");
        }
        Handoff::GetSpool {
            from_url,
            job,
            spool,
            trust_out,
        } => {
            let store = usnm_store::open(&from_url)?;
            let host = x::get_spool(store.as_ref(), &job, &spool).await?;
            if !x::get_trust(store.as_ref(), &trust_out).await? {
                eprintln!("no gold/trust.json in the hand-off container");
            }
            println!("{}", host.map_or("unknown".into(), |h| h.to_string()));
        }
    }
    Ok(())
}

async fn gold(g: Gold) -> anyhow::Result<()> {
    use usnm_boinc::gold as x;
    match g {
        Gold::Prepare {
            bundle,
            root,
            gap_secs,
        } => {
            let p = x::prepare::run(&x::prepare::Options {
                bundle,
                root,
                gap: std::time::Duration::from_secs(gap_secs),
            })
            .await?;
            eprintln!(
                "bundle v{} manifest {}: {} crops, {} original files",
                p.bundle_version,
                p.manifest_sha256,
                p.crops.len(),
                p.remotes.len()
            );
        }
        Gold::Submit {
            root,
            project,
            units,
            engine_image,
            engine_base,
            engine_version,
            timeout_secs,
        } => {
            let s = x::submit::run(&x::submit::Options {
                root,
                project,
                units,
                engine: usnm_boinc::gold::job::Engine {
                    base: engine_base,
                    dpi: 300,
                    image: engine_image,
                    psm: 3,
                    timeout_secs,
                    version: engine_version,
                },
            })
            .await?;
            eprintln!("{s:?}");
        }
        Gold::Check { file, expected_dir } => x::validate::check(&file, &expected_dir)?,
        Gold::Collect {
            root,
            project,
            spool,
            out,
        } => {
            let s = x::collect::run(&x::collect::Options {
                root,
                project,
                spool,
                out,
            })?;
            eprintln!("{s:?}");
        }
        Gold::Trust {
            reports,
            max_cer,
            out,
        } => {
            let t = usnm_boinc::trust::Trust::build(&reports, max_cer)?;
            let out = if out.parent().is_some_and(|p| p.as_os_str().is_empty()) {
                std::path::Path::new(".").join(out)
            } else {
                out
            };
            usnm_boinc::atomic(&out, &serde_json::to_vec_pretty(&t)?)?;
            for (image, hosts) in &t.hosts {
                eprintln!("{image}: {} trusted hosts", hosts.len());
            }
        }
    }
    Ok(())
}

async fn surya(c: Surya) -> anyhow::Result<()> {
    use usnm_boinc::surya as x;
    match c {
        Surya::Stage {
            pages,
            root,
            engine_image,
            engine_llama,
            engine_weights,
            engine_surya,
            gpu_modes,
            cpu_modes,
            gpu_timeout_secs,
            cpu_timeout_secs,
            gap_secs,
            max_pages,
        } => {
            let s = x::stage::run(&x::stage::Options {
                pages,
                root,
                engine: usnm_ocr_contract::surya::Engine {
                    image: engine_image,
                    llama: engine_llama,
                    surya: engine_surya,
                    weights: engine_weights,
                },
                gpu_modes: gpu_modes.into_iter().map(Into::into).collect(),
                cpu_modes: cpu_modes.into_iter().map(Into::into).collect(),
                gpu_timeout_secs,
                cpu_timeout_secs,
                gap: std::time::Duration::from_secs(gap_secs),
                max_pages,
            })
            .await?;
            eprintln!("{s:?}");
        }
        Surya::Submit {
            root,
            project,
            max_units,
        } => {
            let s = x::submit::run(&x::submit::Options {
                root,
                project,
                max_units,
            })
            .await?;
            eprintln!("{s:?}");
        }
        Surya::LocalUnit {
            image,
            dir,
            engine_image,
            engine_llama,
            engine_weights,
            engine_surya,
            modes,
            timeout_secs,
        } => {
            let j = x::stage::local_unit(
                &image,
                &dir,
                usnm_ocr_contract::surya::Engine {
                    image: engine_image,
                    llama: engine_llama,
                    surya: engine_surya,
                    weights: engine_weights,
                },
                modes.into_iter().map(Into::into).collect(),
                timeout_secs,
            )?;
            println!("{}", j.id);
        }
        Surya::Check { files, keep } => x::validate::check(&files, keep.as_deref())?,
        Surya::Compare {
            files,
            files2,
            min_dice,
            log,
        } => x::validate::compare(&files, &files2, min_dice, log.as_deref())?,
        Surya::Summary { root, seen, pairs } => {
            print!("{}", x::summary::run(&root, &seen, &pairs)?)
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
