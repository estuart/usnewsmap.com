//! The experimental search cluster's commands (#239; docs/operations.md,
//! "Search cluster experiment", and `usnm_ingest::cluster`):
//!
//! - `node`: start a cluster node (each node app's command).
//! - `members`: the cluster as its root sees it.
//! - `sample`: take a sample of the published version's pages.
//! - `load`: load a sample into a new index on the cluster.
//! - `bench`: the benchmark searches against the cluster's root.
//!
//! Each of the last four logs one `qwcluster report` line (JSON) that
//! `scripts/qwcluster-report.py` reads back, and keeps the full report in
//! the bench store (`runs/`).

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use chrono::NaiveDate;
use clap::{Parser, Subcommand};
use usnm_ingest::cluster::{self, bench, load, local, members, node, sample, set};
use usnm_ingest::telemetry;
use usnm_store::ObjectStore;

#[derive(Parser)]
#[command(version, about = "The experimental Quickwit search cluster (#239)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register this node in the seed registry and run Quickwit in its place.
    Node {
        #[arg(long)]
        node_id: String,
        /// Quickwit services, comma separated.
        #[arg(long, value_delimiter = ',')]
        services: Vec<String>,
        /// The seed registry: a Blob container URL or a directory.
        #[arg(
            long,
            required_unless_present = "standalone",
            conflicts_with = "standalone"
        )]
        registry: Option<String>,
        /// Outside the cluster (the version comparison, #251): no registry,
        /// no seeds, a cluster id of its own.
        #[arg(long)]
        standalone: bool,
        #[arg(long)]
        metastore: String,
        #[arg(long)]
        index_root: String,
        #[arg(long, default_value = "none")]
        storage_account: String,
        /// The REST and gossip port; gRPC is the next one.
        #[arg(long, default_value_t = 7280)]
        rest_port: u16,
        /// Quickwit's data directory (default: $USNM_WORK_DIR/qwdata).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// The address to advertise (default: this replica's, from its hostname).
        #[arg(long)]
        advertise: Option<IpAddr>,
        /// The container's vCPUs, for Quickwit's thread pools.
        #[arg(long)]
        cpus: Option<u32>,
        /// Main runtime threads (QW_TOKIO_RUNTIME_NUM_THREADS; default: --cpus).
        #[arg(long)]
        runtime_threads: Option<u32>,
        /// Search pool threads (RAYON_NUM_THREADS; default: --cpus).
        #[arg(long)]
        search_threads: Option<u32>,
        /// `searcher.max_num_concurrent_split_searches` (default: the sidecar's).
        #[arg(long)]
        split_searches: Option<u32>,
        /// Write the searcher config for a Quickwit build from before 0.9.
        /// Without it the node asks `quickwit --version`.
        #[arg(long)]
        legacy_searcher: bool,
        /// Keep whole splits on the local disk, under the data directory, up
        /// to this many GiB (`searcher.split_cache`; #251's local-disk test).
        #[arg(long)]
        split_cache_gib: Option<u32>,
        /// Copy this index from --local-copy-from to --local-dir first, and
        /// serve it from there (`file://`) instead of --metastore and
        /// --index-root (#251's local-disk test).
        #[arg(long, requires_all = ["local_copy_from", "local_dir"])]
        local_copy: Option<String>,
        /// The index root to copy from: a Blob container URL or a directory.
        #[arg(long)]
        local_copy_from: Option<String>,
        /// Where the copy goes (its metastore and index root).
        #[arg(long)]
        local_dir: Option<PathBuf>,
        #[arg(
            long,
            env = "USNM_QUICKWIT_BIN",
            default_value = "/usr/local/bin/quickwit"
        )]
        quickwit_bin: PathBuf,
    },
    /// Print the cluster's members as its root sees them.
    Members {
        #[arg(long, env = "USNM_QWCLUSTER_URL")]
        cluster: String,
    },
    /// Take a sample of the published version's pages (cluster::sample).
    Sample {
        /// Its name: the sample goes to `sample/{name}/` in the bench store.
        #[arg(long)]
        name: String,
        /// Percent of the pages.
        #[arg(long, default_value_t = 1.0)]
        pct: f64,
        /// With American Stories' text, as the release with the setting on.
        #[arg(long)]
        american_stories: bool,
        /// Batches read at once.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Only the version's first N batches (a trial).
        #[arg(long)]
        max_batches: Option<usize>,
        #[arg(long, env = "USNM_CURATED_URL")]
        curated: String,
        #[arg(long, env = "USNM_REFERENCE_URL")]
        reference: String,
        #[arg(long, env = "USNM_QWBENCH_URL")]
        store: String,
    },
    /// Load a sample into a new index on the cluster (cluster::load).
    Load {
        /// The sample's name (in the bench store)...
        #[arg(long, required_unless_present = "set", conflicts_with = "set")]
        sample: Option<String>,
        /// ...or a packaged set's (in the archival account's `sets`).
        #[arg(long)]
        set: Option<String>,
        #[arg(long, env = "USNM_ARCHIVE_SETS_URL")]
        sets: Option<String>,
        /// The new index's id.
        #[arg(long)]
        index: String,
        #[arg(long, default_value_t = 3000)]
        split_docs: u64,
        /// Ingest requests in flight.
        #[arg(long, default_value_t = 4)]
        senders: usize,
        /// Shards (default: one per indexer).
        #[arg(long)]
        min_shards: Option<usize>,
        /// Fill each document's American Stories' fields with its `text`
        /// and `text_cg`, for an index shaped like production's (#251).
        #[arg(long)]
        as_from_text: bool,
        #[arg(long, default_value_t = 14400)]
        merge_timeout_secs: u64,
        #[arg(long, env = "USNM_QWCLUSTER_URL")]
        cluster: String,
        #[arg(long, env = "USNM_QWCLUSTER_INDEX_ROOT")]
        index_root: String,
        #[arg(long, env = "USNM_QWBENCH_URL")]
        store: String,
    },
    /// Package a sample for reuse in the archival account (cluster::set):
    /// `sets/{name}/raw.tar`, `docs.ndjson.zst` and `manifest.json`.
    Bundle {
        /// The set's name with its version, e.g. `loc-1pct-v1`; never replaced.
        #[arg(long)]
        name: String,
        /// The sample whose documents it holds (in the bench store).
        #[arg(long)]
        sample: String,
        /// How the batches were chosen, for the manifest.
        #[arg(long)]
        selection: String,
        #[arg(long, env = "USNM_ARCHIVE_RAW_URL")]
        raw: String,
        #[arg(long, env = "USNM_ARCHIVE_SETS_URL")]
        sets: String,
        #[arg(long, env = "USNM_REFERENCE_URL")]
        reference: String,
        #[arg(long, env = "USNM_QWBENCH_URL")]
        store: String,
    },
    /// Print every stored report (`runs/*/*.json`, `sample/*/manifest.json`)
    /// as `qwcluster report` lines, then wait: with Log Analytics over its
    /// daily cap, `az containerapp job logs show --follow` still streams them
    /// (scripts/qwcluster-report.py --from-file).
    Dump {
        /// Seconds to keep running after printing, for the log stream.
        #[arg(long, default_value_t = 600)]
        hold_secs: u64,
        #[arg(long, env = "USNM_QWBENCH_URL")]
        store: String,
    },
    /// The benchmark searches against the cluster's root (cluster::bench).
    Bench {
        /// Names the run in the report (e.g. `s2-1ix`).
        #[arg(long)]
        label: String,
        /// Indexes to search, comma separated.
        #[arg(long, value_delimiter = ',')]
        index: Vec<String>,
        /// The sample the indexes were loaded from: its corpus bounds and
        /// whether it has American Stories' text...
        #[arg(long, required_unless_present = "set", conflicts_with = "set")]
        sample: Option<String>,
        /// ...or the packaged set.
        #[arg(long)]
        set: Option<String>,
        #[arg(long, env = "USNM_ARCHIVE_SETS_URL")]
        sets: Option<String>,
        /// Search American Stories' fields too, whatever the sample says:
        /// for an index loaded with `--as-from-text`.
        #[arg(long)]
        american_stories: bool,
        #[arg(long, value_delimiter = ',', default_value = "1,2,4,10")]
        levels: Vec<usize>,
        /// Seconds between levels.
        #[arg(long, default_value_t = 30)]
        pause: u64,
        /// Days the levels' windows start shifted back (then one more per level).
        #[arg(long, default_value_t = 0)]
        offset: u32,
        /// Only the concurrency levels, without `first` and `warm`.
        #[arg(long)]
        levels_only: bool,
        /// Wait (up to 10 minutes) for this many ready searchers.
        #[arg(long, default_value_t = 1)]
        expect_searchers: usize,
        #[arg(long, default_value_t = 150)]
        timeout_secs: u64,
        /// First wait (at most this many seconds) for the searchers' split
        /// caches to hold every split of the indexes (#251's local-disk test).
        #[arg(long)]
        wait_split_cache_secs: Option<u64>,
        /// After the passes, each search's summary over gRPC, for Quickwit's
        /// per-split resource stats (warm-up, CPU pool wait, CPU search).
        #[arg(long)]
        probe: bool,
        /// After the passes, profile the root's CPU this many seconds (5 to
        /// 300) while searches run; the flame graph is stored beside the report.
        #[arg(long)]
        profile_secs: Option<u64>,
        /// What the run tests (`blob`, `cache`, `copy`, ...), for grouping
        /// runs in scripts/qwcluster-report.py.
        #[arg(long)]
        variant: Option<String>,
        #[arg(long, env = "USNM_QWCLUSTER_URL")]
        cluster: String,
        #[arg(long, env = "USNM_QWBENCH_URL")]
        store: String,
    },
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Node { .. } => "node",
            Command::Members { .. } => "members",
            Command::Sample { .. } => "sample",
            Command::Load { .. } => "load",
            Command::Bench { .. } => "bench",
            Command::Dump { .. } => "dump",
            Command::Bundle { .. } => "bundle",
        }
    }
}

/// `runs/{name}/{file}` in the bench store, and the report's log lines.
async fn keep(
    store: &dyn ObjectStore,
    name: &str,
    file: &str,
    full: &serde_json::Value,
    lines: &[(&str, serde_json::Value)],
) -> anyhow::Result<()> {
    if !usnm_store::is_safe_segment(name) {
        anyhow::bail!("`{name}` can't name a run");
    }
    for (kind, line) in lines {
        cluster::log_report(kind, line);
    }
    store
        .put(
            &format!("runs/{name}/{file}"),
            serde_json::to_vec_pretty(full)?,
            "application/json",
        )
        .await?;
    Ok(())
}

/// Every stored report as (kind, the summary its log line had): samples'
/// manifests, then each run's load and bench reports, by path.
async fn dump(store: &dyn ObjectStore) -> anyhow::Result<Vec<(String, serde_json::Value)>> {
    let mut out = Vec::new();
    for path in store.list("sample").await? {
        if let Some(name) = path
            .strip_prefix("sample/")
            .and_then(|p| p.strip_suffix("/manifest.json"))
        {
            let m = sample::manifest(store, &format!("sample/{name}")).await?;
            let mut v = serde_json::to_value(&m)?;
            v["parts"] = serde_json::json!(m.parts.len());
            v["name"] = serde_json::json!(name);
            out.push(("sample".to_owned(), v));
        }
    }
    for path in store.list("runs").await? {
        let kind = match path.rsplit('/').next() {
            Some("load.json") => "load",
            Some("bench.json") => "bench",
            _ => continue,
        };
        let Some(bytes) = store.get(&path).await? else {
            continue;
        };
        let mut v: serde_json::Value =
            serde_json::from_slice(&bytes).with_context(|| path.clone())?;
        // As logged: a bench in its parts without each search's calls, a
        // load without its per-poll series. A bench's profile is summarized
        // again from its stored flame graph, so a fix to the summary reaches
        // earlier runs (October 2026: thread names cut to 15 characters).
        if kind == "bench" {
            if let Some(dir) = path.strip_suffix("bench.json") {
                if let Some(svg) = store.get(&format!("{dir}flamegraph.svg")).await? {
                    let summary = cluster::flame::summarize(&String::from_utf8_lossy(&svg), 15);
                    if v.get("profile").is_some_and(|p| p.is_object()) {
                        v["profile"]["summary"] = serde_json::to_value(summary)?;
                    }
                }
            }
            out.extend(
                bench::log_parts(&v)
                    .into_iter()
                    .map(|(k, part)| (k.to_owned(), part)),
            );
        } else {
            v["rate"] = serde_json::json!([]);
            out.push((kind.to_owned(), v));
        }
    }
    Ok(out)
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::Node {
            node_id,
            services,
            registry,
            metastore,
            index_root,
            storage_account,
            rest_port,
            data_dir,
            advertise,
            cpus,
            runtime_threads,
            search_threads,
            split_searches,
            legacy_searcher,
            split_cache_gib,
            local_copy,
            local_copy_from,
            local_dir,
            standalone,
            quickwit_bin,
        } => {
            let data_dir = data_dir.unwrap_or_else(|| {
                std::env::var_os("USNM_WORK_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(std::env::temp_dir)
                    .join("qwdata")
            });
            // The local-disk test's copy: the node then serves it, not Blob.
            let (metastore, index_root) = match (&local_copy, &local_copy_from, &local_dir) {
                (Some(index), Some(from), Some(dir)) => {
                    let from = usnm_store::open(from)?;
                    let copied = local::copy_index(from.as_ref(), index, dir).await?;
                    (copied.root_uri.clone(), copied.root_uri)
                }
                _ => (metastore, index_root),
            };
            let spec = node::NodeSpec {
                node_id,
                services,
                rest_port,
                data_dir,
                metastore,
                index_root,
                storage_account,
                split_searches,
                standalone,
                split_cache_gib,
                legacy_searcher: legacy_searcher
                    || node::is_pre_09(&quickwit_bin).unwrap_or_else(|| {
                        tracing::warn!(bin = %quickwit_bin.display(), "can't read Quickwit's version; writing the 0.9 searcher config");
                        false
                    }),
            };
            let threads = node::Threads::new(cpus, runtime_threads, search_threads)?;
            let ip = match advertise {
                Some(ip) => ip,
                None => node::replica_address()?,
            };
            let registry = registry.as_deref().map(usnm_store::open).transpose()?;
            node::start(registry.as_deref(), &spec, ip, threads, &quickwit_bin).await
        }
        Command::Members { cluster } => {
            let http = reqwest::Client::new();
            let m = members::members(&http, &cluster).await?;
            let c = members::counters(&http, &m, None).await;
            cluster::log_report("members", &serde_json::json!({"members": m, "counters": c}));
            Ok(())
        }
        Command::Sample {
            name,
            pct,
            american_stories,
            concurrency,
            max_batches,
            curated,
            reference,
            store,
        } => {
            if !usnm_store::is_safe_segment(&name) {
                anyhow::bail!("`{name}` can't name a sample");
            }
            let spec = sample::Spec {
                cut: sample::cut_of(pct)?,
                american_stories,
                concurrency,
                part_bytes: sample::PART_BYTES,
                max_batches,
            };
            let curated = usnm_store::open(&curated)?;
            let reference = usnm_store::open(&reference)?;
            let out = usnm_store::open(&store)?;
            let published = sample::published(reference.as_ref()).await?;
            let m = sample::build(
                curated,
                published,
                out.as_ref(),
                &format!("sample/{name}"),
                &spec,
            )
            .await?;
            let mut summary = serde_json::to_value(&m)?;
            summary["parts"] = serde_json::json!(m.parts.len());
            summary["name"] = serde_json::json!(name);
            cluster::log_report("sample", &summary);
            Ok(())
        }
        Command::Load {
            sample,
            set,
            sets,
            index,
            split_docs,
            senders,
            min_shards,
            as_from_text,
            merge_timeout_secs,
            cluster,
            index_root,
            store,
        } => {
            let store = usnm_store::open(&store)?;
            let mut spec = load::Spec::new(&cluster, &index, &index_root);
            spec.split_docs = split_docs;
            spec.senders = senders;
            spec.min_shards = min_shards;
            spec.as_from_text = as_from_text;
            spec.merge_timeout = Duration::from_secs(merge_timeout_secs);
            let report = match (&sample, &set) {
                (Some(sample), _) => {
                    load::run(store.as_ref(), &format!("sample/{sample}"), &spec).await?
                }
                (None, Some(name)) => {
                    let sets = usnm_store::open(
                        sets.as_deref()
                            .context("--set needs the archival sets (USNM_ARCHIVE_SETS_URL)")?,
                    )?;
                    let manifest = set::manifest(sets.as_ref(), name).await?;
                    let input = load::Input::Set {
                        store: sets.as_ref(),
                        manifest: &manifest,
                    };
                    load::run_from(&input, &spec).await?
                }
                (None, None) => anyhow::bail!("name a --sample or a --set"),
            };
            keep(
                store.as_ref(),
                &index,
                "load.json",
                &serde_json::to_value(&report)?,
                &[("load", report.summary())],
            )
            .await
        }
        Command::Bundle {
            name,
            sample,
            selection,
            raw,
            sets,
            reference,
            store,
        } => {
            let store = usnm_store::open(&store)?;
            let reference = usnm_store::open(&reference)?;
            // The batches of the version the sample was taken from, not
            // whatever is published now.
            let sm = sample::manifest(store.as_ref(), &format!("sample/{sample}")).await?;
            let batches = sample::run_batches(reference.as_ref(), &sm.version)
                .await?
                .iter()
                .map(|b| format!("{}_ver{:02}", b.batch, b.curated.version))
                .collect();
            let m = set::bundle(
                usnm_store::open(&sets)?,
                &name,
                &selection,
                set::Sources {
                    raw: usnm_store::open(&raw)?,
                    sample: store.as_ref(),
                    sample_prefix: &format!("sample/{sample}"),
                    batches,
                },
            )
            .await?;
            let mut summary = serde_json::to_value(&m)?;
            summary["batches"] = serde_json::json!(m.batches.len());
            cluster::log_report("set", &summary);
            Ok(())
        }
        Command::Dump { hold_secs, store } => {
            let store = usnm_store::open(&store)?;
            for (kind, report) in dump(store.as_ref()).await? {
                cluster::log_report(&kind, &report);
            }
            tokio::time::sleep(Duration::from_secs(hold_secs)).await;
            Ok(())
        }
        Command::Bench {
            label,
            index,
            sample,
            set,
            sets,
            american_stories: force_american_stories,
            levels,
            pause,
            offset,
            levels_only,
            expect_searchers,
            timeout_secs,
            wait_split_cache_secs,
            probe,
            profile_secs,
            variant,
            cluster,
            store,
        } => {
            let store = usnm_store::open(&store)?;
            let (bounds, american_stories, common_grams) = match (&sample, &set) {
                (Some(sample), _) => {
                    let m = sample::manifest(store.as_ref(), &format!("sample/{sample}")).await?;
                    (m.bounds, m.american_stories, m.common_grams)
                }
                (None, Some(name)) => {
                    let sets = usnm_store::open(
                        sets.as_deref()
                            .context("--set needs the archival sets (USNM_ARCHIVE_SETS_URL)")?,
                    )?;
                    let m = set::manifest(sets.as_ref(), name).await?;
                    (m.bounds, m.american_stories, m.common_grams)
                }
                (None, None) => anyhow::bail!("name a --sample or a --set"),
            };
            let date = |s: &str| {
                NaiveDate::parse_from_str(s, "%Y-%m-%d")
                    .with_context(|| format!("the sample's bounds: `{s}`"))
            };
            let spec = bench::Spec {
                root: cluster.trim_end_matches('/').to_owned(),
                indexes: index,
                american_stories: american_stories || force_american_stories,
                bounds: (date(&bounds.0)?, date(&bounds.1)?),
                analyzer: usnm_core::common_grams::analyzer(common_grams).with_context(|| {
                    format!("the sample's common-word pairs, version {common_grams}, are newer than this tool")
                })?,
                levels,
                offset,
                pause: Duration::from_secs(pause),
                levels_only,
                expect_searchers,
                timeout: Duration::from_secs(timeout_secs),
                wait_split_cache: wait_split_cache_secs.map(Duration::from_secs),
                probe,
                profile: profile_secs.map(Duration::from_secs),
                variant,
            };
            let report = bench::run(&label, &spec).await?;
            if let Some(svg) = report.profile.as_ref().and_then(|p| p.svg.clone()) {
                if !usnm_store::is_safe_segment(&label) {
                    anyhow::bail!("`{label}` can't name a run");
                }
                store
                    .put(
                        &format!("runs/{label}/flamegraph.svg"),
                        svg.into_bytes(),
                        "image/svg+xml",
                    )
                    .await?;
            }
            keep(
                store.as_ref(),
                &label,
                "bench.json",
                &serde_json::to_value(&report)?,
                &report.log_parts(),
            )
            .await
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let telemetry = telemetry::init();
    let name = cli.command.name();
    let result = run(cli).await;
    let code = match &result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(command = name, error = %format!("{e:#}"), "command failed");
            std::process::ExitCode::FAILURE
        }
    };
    telemetry.shutdown().await;
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_node_command() {
        let cli = Cli::try_parse_from([
            "usnm-qwcluster",
            "node",
            "--node-id",
            "qw-0",
            "--services",
            "metastore,control_plane,janitor,indexer,searcher",
            "--registry",
            "https://acct.blob.core.windows.net/qw-bench",
            "--metastore",
            "azure://qw-cluster",
            "--index-root",
            "azure://qw-cluster",
            "--storage-account",
            "acct",
            "--cpus",
            "2",
        ])
        .unwrap();
        let Command::Node {
            services,
            rest_port,
            cpus,
            ..
        } = cli.command
        else {
            panic!("not a node");
        };
        assert_eq!(services.len(), 5);
        assert_eq!((rest_port, cpus), (7280, Some(2)));
    }

    /// A comparison searcher (#251): standalone, so no registry.
    #[test]
    fn parses_a_standalone_node_command() {
        let args = [
            "usnm-qwcluster",
            "node",
            "--node-id",
            "qws-1",
            "--standalone",
            "--services",
            "searcher,metastore",
            "--metastore",
            "azure://qw-cluster#polling_interval=30s",
            "--index-root",
            "azure://qw-cluster",
            "--cpus",
            "4",
            "--runtime-threads",
            "4",
            "--search-threads",
            "4",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        let Command::Node {
            standalone,
            registry,
            runtime_threads,
            search_threads,
            legacy_searcher,
            ..
        } = cli.command
        else {
            panic!("not a node");
        };
        assert!(standalone && registry.is_none() && !legacy_searcher);
        assert_eq!((runtime_threads, search_threads), (Some(4), Some(4)));
        // A cluster node needs the registry; a standalone one refuses it.
        let without: Vec<&str> = args
            .iter()
            .copied()
            .filter(|a| *a != "--standalone")
            .collect();
        assert!(Cli::try_parse_from(without).is_err());
        let mut both = args.to_vec();
        both.extend(["--registry", "/tmp/r"]);
        assert!(Cli::try_parse_from(both).is_err());
        // The local-disk test's copy needs its source and directory.
        let mut copy = args.to_vec();
        copy.extend(["--local-copy", "s1ixb"]);
        assert!(Cli::try_parse_from(copy.clone()).is_err());
        copy.extend([
            "--local-copy-from",
            "/tmp/qw",
            "--local-dir",
            "/work/index",
            "--split-cache-gib",
            "40",
        ]);
        let Command::Node {
            local_copy,
            split_cache_gib,
            ..
        } = Cli::try_parse_from(copy).unwrap().command
        else {
            panic!("not a node");
        };
        assert_eq!(
            (local_copy.as_deref(), split_cache_gib),
            (Some("s1ixb"), Some(40))
        );
    }

    #[tokio::test]
    async fn dumps_every_stored_report() {
        let dir = tempfile::tempdir().unwrap();
        let store = usnm_store::open(dir.path().to_str().unwrap()).unwrap();
        let bench = serde_json::json!({"label": "n1", "passes": [
            {"name": "first", "searches": [{"name": "radio", "calls": [{"kind": "summary"}]}]}]});
        store
            .put(
                "runs/n1/bench.json",
                bench.to_string().into_bytes(),
                "application/json",
            )
            .await
            .unwrap();
        store
            .put(
                "runs/s1ix/load.json",
                br#"{"index_id": "s1ix", "rate": [[10.0, 5]]}"#.to_vec(),
                "application/json",
            )
            .await
            .unwrap();
        store
            .put("runs/s1ix/other.txt", b"x".to_vec(), "text/plain")
            .await
            .unwrap();
        // A run with a profile: its summary comes again from the stored graph.
        let profiled = serde_json::json!({"label": "p1", "passes": [],
            "profile": {"secs": 1.0, "searches": 1, "first_shift_days": 0, "summary": {"total_samples": 1}}});
        store
            .put(
                "runs/p1/bench.json",
                profiled.to_string().into_bytes(),
                "application/json",
            )
            .await
            .unwrap();
        let svg = concat!(
            r#"<svg><g><title>all (10 samples, 100%)</title><rect y="100" fg:x="0" fg:w="10"/></g>"#,
            r#"<g><title>main_runtime_th (10 samples, 100%)</title><rect y="84" fg:x="0" fg:w="10"/></g>"#,
            r#"<g><title>rustls::read (10 samples, 100%)</title><rect y="68" fg:x="0" fg:w="10"/></g></svg>"#
        );
        store
            .put(
                "runs/p1/flamegraph.svg",
                svg.as_bytes().to_vec(),
                "image/svg+xml",
            )
            .await
            .unwrap();
        let got = dump(store.as_ref()).await.unwrap();
        let profile = got
            .iter()
            .find(|(k, v)| k == "bench_profile" && v["label"] == "p1")
            .map(|(_, v)| v["profile"]["summary"].clone())
            .unwrap();
        assert_eq!(profile["total_samples"], 10);
        assert_eq!(profile["main_runtime_top"][0][0], "rustls::read");
        let got: Vec<_> = got
            .into_iter()
            .filter(|(_, v)| v["label"] != "p1")
            .collect();
        let kinds: Vec<&str> = got.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(kinds, ["bench", "bench_pass", "load"]);
        assert_eq!(got[0].1["passes_logged"], 1);
        assert!(got[1].1["pass"]["searches"][0].get("calls").is_none());
        assert_eq!(got[2].1["rate"], serde_json::json!([]));
    }

    #[test]
    fn bench_levels_default_to_the_load_tests() {
        let cli = Cli::try_parse_from([
            "usnm-qwcluster",
            "bench",
            "--label",
            "s1",
            "--index",
            "a,b",
            "--sample",
            "pct1",
            "--cluster",
            "http://x",
            "--store",
            "/tmp/x",
        ])
        .unwrap();
        let Command::Bench { levels, index, .. } = cli.command else {
            panic!("not a bench");
        };
        assert_eq!(levels, [1, 2, 4, 10]);
        assert_eq!(index, ["a", "b"]);
    }
}
