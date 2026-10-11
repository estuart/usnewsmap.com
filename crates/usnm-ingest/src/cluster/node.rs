//! Starting a cluster node (#239): Container Apps gives each replica an IP
//! of its own (100.100.x.x, reachable only inside the environment), and an
//! app's name resolves to a service IP that carries only its ingress port,
//! so Quickwit's UDP gossip and gRPC must use replica IPs. Those change
//! whenever a replica restarts, so the nodes find each other through a seed
//! registry: one blob per node, `seeds/{node_id}.json`, holding its current
//! gossip address.
//!
//! At start a node:
//! 1. finds its replica IP (its hostname's address, as `hostname -i`);
//! 2. writes its entry;
//! 3. reads every other node's entry, and passes their addresses to Quickwit
//!    as `QW_PEER_SEEDS`, with its own IP as `QW_ADVERTISE_ADDRESS`;
//! 4. becomes Quickwit (exec), with the node config filled in.
//!
//! Every node writes before it reads, so of two nodes starting together at
//! least one sees the other's new entry, and gossip needs only one live
//! seed: they join. A node that restarts at a new IP reads the others'
//! current addresses and joins through them; they learn its new address by
//! gossip, and its old one stops answering and is dropped as dead. An entry
//! left by a replica that is gone is a seed that never answers, which
//! Quickwit tolerates.
//!
//! A **standalone** node (`--standalone`, the version comparison of #251)
//! skips the registry: it has a cluster id of its own and no seeds, so two
//! of them never join each other (a root would then spread one search over
//! both versions). It runs as the API's sidecar does: searcher and
//! metastore, reading a file-backed metastore it polls.
//!
//! Thread counts (#251): the API's sidecar runs Quickwit 0.9.1 with
//! `QW_TOKIO_RUNTIME_NUM_THREADS` and `RAYON_NUM_THREADS` both 4 at 3.75
//! vCPU (infra/modules/containerapp.bicep). A node does the same by default:
//! both pools get one thread per vCPU (`--cpus`) and share the quota (an
//! idle pool uses none). Left to itself Quickwit gives its main runtime a
//! third of the CPUs, rounded up: one thread at 2 vCPU, which the downloads
//! and split opening of cold searches keep busy while the search pool waits.

use std::collections::BTreeMap;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use usnm_store::ObjectStore;

/// The node config every member fills in (infra/quickwit/cluster-node.yaml).
pub const NODE_TEMPLATE: &str = include_str!("../../../../infra/quickwit/cluster-node.yaml");

/// Where the entries live in the registry store.
pub const SEEDS_PREFIX: &str = "seeds";

/// Quickwit services a node may run (`enabled_services`).
const SERVICES: &[&str] = &[
    "control_plane",
    "indexer",
    "janitor",
    "metastore",
    "searcher",
];

/// One node's settings, from its flags.
#[derive(Debug, Clone)]
pub struct NodeSpec {
    pub node_id: String,
    pub services: Vec<String>,
    /// REST and gossip (UDP); gRPC is the next port.
    pub rest_port: u16,
    pub data_dir: PathBuf,
    pub metastore: String,
    pub index_root: String,
    pub storage_account: String,
    /// `searcher.max_num_concurrent_split_searches`; `None` keeps the
    /// sidecar's value (infra/quickwit/searcher.yaml).
    pub split_searches: Option<u32>,
    /// Outside the cluster: a cluster id of its own (`usnm-qwcmp-{node_id}`).
    /// The caller passes no seeds.
    pub standalone: bool,
    /// The Quickwit binary is a build from before 0.9: drop the searcher
    /// keys it doesn't know ([`LEGACY_DROPPED`]) and pin
    /// [`LEGACY_WARMUP_ALLOCATION`].
    pub legacy_searcher: bool,
    /// `searcher.split_cache.max_num_bytes` in GiB: whole splits kept on
    /// the local disk under the data directory (#251's local-disk test).
    pub split_cache_gib: Option<u32>,
}

/// How many splits the split cache downloads at once. Quickwit's default is
/// 1; 4 fills the cache with a test index's 60 splits sooner.
pub const SPLIT_CACHE_DOWNLOADS: u32 = 4;

/// The template's key for concurrent split searches.
const SPLIT_SEARCHES_KEY: &str = "max_num_concurrent_split_searches:";

/// The template's cluster id; a standalone node replaces it.
const CLUSTER_ID_LINE: &str = "cluster_id: usnm-qwcluster";

/// Searcher keys a Quickwit build from before 0.9 rejects (its
/// `SearcherConfig` denies unknown fields): the predicate cache came with
/// quickwit #6024 (December 2025) and the leaf timeout with #6255 (April
/// 2026). The `qw-azure-fix` nightly (main of 23 September 2025) has every
/// other key of the template.
pub const LEGACY_DROPPED: &[&str] = &["predicate_cache_capacity:", "leaf_request_timeout_secs:"];

/// `searcher.warmup_single_split_initial_allocation` for a pre-0.9 build:
/// 0.9.1's default (300 MB), where the September 2025 nightly's is 1 GB, so
/// both admit the same number of splits under `warmup_memory_budget`.
pub const LEGACY_WARMUP_ALLOCATION: &str = "  warmup_single_split_initial_allocation: 300MB";

impl NodeSpec {
    /// The node config, from [`NODE_TEMPLATE`].
    pub fn config(&self) -> anyhow::Result<String> {
        if !usnm_store::is_safe_segment(&self.node_id) {
            bail!("`{}` isn't a usable node id", self.node_id);
        }
        if self.services.is_empty() {
            bail!("a node needs at least one service");
        }
        for s in &self.services {
            if !SERVICES.contains(&s.as_str()) {
                bail!("unknown Quickwit service `{s}` (one of {SERVICES:?})");
            }
        }
        let grpc = self
            .rest_port
            .checked_add(1)
            .context("the REST port must be below 65535")?;
        let out = NODE_TEMPLATE
            .replace("__NODE_ID__", &self.node_id)
            .replace("__SERVICES__", &self.services.join(", "))
            .replace("__REST_PORT__", &self.rest_port.to_string())
            .replace("__GRPC_PORT__", &grpc.to_string())
            .replace("__DATA_DIR__", &self.data_dir.display().to_string())
            .replace("__METASTORE__", &self.metastore)
            .replace("__INDEX_ROOT__", &self.index_root)
            .replace("__STORAGE_ACCOUNT__", &self.storage_account);
        if let Some(n) = self.split_searches {
            anyhow::ensure!(n > 0, "concurrent split searches must be at least 1");
        }
        anyhow::ensure!(
            self.split_cache_gib != Some(0),
            "a split cache needs at least 1 GiB"
        );
        if self.standalone {
            anyhow::ensure!(
                out.lines().any(|l| l == CLUSTER_ID_LINE),
                "the node config has no `{CLUSTER_ID_LINE}` line to change"
            );
        }
        let mut lines: Vec<String> = Vec::new();
        let mut found_split_searches = false;
        for line in out.lines() {
            let key = line.trim_start();
            if self.standalone && line == CLUSTER_ID_LINE {
                lines.push(format!("cluster_id: usnm-qwcmp-{}", self.node_id));
                continue;
            }
            if self.legacy_searcher && LEGACY_DROPPED.iter().any(|k| key.starts_with(k)) {
                continue;
            }
            if key.starts_with(SPLIT_SEARCHES_KEY) {
                found_split_searches = true;
                if let Some(n) = self.split_searches {
                    let indent = &line[..line.len() - key.len()];
                    lines.push(format!("{indent}{SPLIT_SEARCHES_KEY} {n}"));
                    continue;
                }
            }
            lines.push(line.to_owned());
            if self.legacy_searcher && line == "searcher:" {
                lines.push(LEGACY_WARMUP_ALLOCATION.to_owned());
            }
            if let (Some(gib), "searcher:") = (self.split_cache_gib, line) {
                lines.extend([
                    "  split_cache:".to_owned(),
                    format!("    max_num_bytes: {gib}GiB"),
                    "    max_num_splits: 10000".to_owned(),
                    format!("    num_concurrent_downloads: {SPLIT_CACHE_DOWNLOADS}"),
                ]);
            }
        }
        anyhow::ensure!(
            found_split_searches || self.split_searches.is_none(),
            "the node config has no `{SPLIT_SEARCHES_KEY}` line to change"
        );
        let mut out = lines.join("\n");
        out.push('\n');
        if let Some(line) = out
            .lines()
            .find(|l| !l.trim_start().starts_with('#') && l.contains("__"))
        {
            bail!("the node config still has a placeholder: {line}");
        }
        Ok(out)
    }
}

/// A node's entry in the seed registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub node_id: String,
    /// `ip:port`, the node's gossip address (its REST port).
    pub addr: String,
    pub registered_at: DateTime<Utc>,
    /// The replica's name (`HOSTNAME`), for the record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replica: Option<String>,
}

fn entry_path(node_id: &str) -> String {
    format!("{SEEDS_PREFIX}/{node_id}.json")
}

/// Write (or replace) this node's entry.
pub async fn register(store: &dyn ObjectStore, reg: &Registration) -> anyhow::Result<()> {
    store
        .put(
            &entry_path(&reg.node_id),
            serde_json::to_vec(reg)?,
            "application/json",
        )
        .await
        .with_context(|| format!("writing {}", entry_path(&reg.node_id)))
}

/// Every node's entry, by node id. An entry that doesn't parse is skipped
/// with a warning: one bad blob mustn't keep a node from starting.
pub async fn registered(store: &dyn ObjectStore) -> anyhow::Result<BTreeMap<String, Registration>> {
    let mut out = BTreeMap::new();
    for path in store.list(SEEDS_PREFIX).await? {
        if !path.ends_with(".json") {
            continue;
        }
        let Some(bytes) = store.get(&path).await? else {
            continue;
        };
        match serde_json::from_slice::<Registration>(&bytes) {
            Ok(r) => {
                out.insert(r.node_id.clone(), r);
            }
            Err(e) => tracing::warn!(path, error = %e, "skipping a seed entry that doesn't parse"),
        }
    }
    Ok(out)
}

/// The other nodes' addresses, sorted: the peer seeds.
pub fn seeds(all: &BTreeMap<String, Registration>, self_id: &str) -> Vec<String> {
    let mut out: Vec<String> = all
        .values()
        .filter(|r| r.node_id != self_id)
        .map(|r| r.addr.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The address another replica reaches this one at: of `candidates` (the
/// hostname's addresses), the first IPv4 address that is neither loopback
/// nor link-local, else the first such IPv6 one. A workload-profiles
/// environment's Consumption replicas resolve their hostname to 100.100.x.x;
/// in 2023 some also listed 127.0.0.1 and 169.254.x.x (#238).
pub fn pick_address(candidates: &[IpAddr]) -> Option<IpAddr> {
    let usable = |ip: &&IpAddr| match ip {
        IpAddr::V4(v4) => !(v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()),
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    };
    candidates
        .iter()
        .filter(usable)
        .find(|ip| ip.is_ipv4())
        .or_else(|| candidates.iter().find(|ip| usable(ip)))
        .copied()
}

/// This replica's address: its hostname resolved, as `hostname -i` does.
pub fn replica_address() -> anyhow::Result<IpAddr> {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()))
        .context("can't tell this replica's hostname; pass --advertise")?;
    let found: Vec<IpAddr> = (host.as_str(), 0)
        .to_socket_addrs()
        .with_context(|| format!("resolving this replica's hostname `{host}`"))?
        .map(|a| a.ip())
        .collect();
    pick_address(&found).with_context(|| {
        format!("`{host}` resolves to no address another replica can reach: {found:?}")
    })
}

/// Quickwit's CPU count and thread counts (#251).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Threads {
    /// `QW_NUM_CPUS`: the container's vCPUs. Quickwit sizes its small pools
    /// and its default main runtime from it.
    pub cpus: Option<u32>,
    /// `QW_TOKIO_RUNTIME_NUM_THREADS`: the main runtime, which runs the
    /// searcher's downloads, TLS and split opening.
    pub runtime: Option<u32>,
    /// `RAYON_NUM_THREADS`: the search pool, one split's query and
    /// aggregation per thread.
    pub search: Option<u32>,
}

impl Threads {
    /// `runtime` and `search` default to `cpus`, as the API's sidecar runs
    /// (4 and 4 at its 3.75 vCPU, rounded to 4): at 2 vCPU, 2 and 2.
    pub fn new(
        cpus: Option<u32>,
        runtime: Option<u32>,
        search: Option<u32>,
    ) -> anyhow::Result<Self> {
        for (name, n) in [
            ("--cpus", cpus),
            ("--runtime-threads", runtime),
            ("--search-threads", search),
        ] {
            anyhow::ensure!(n != Some(0), "{name} must be at least 1");
        }
        Ok(Threads {
            cpus,
            runtime: runtime.or(cpus),
            search: search.or(cpus),
        })
    }
}

/// The environment Quickwit runs with: what it inherited, without any
/// `QW_*` (the official image sets QW_LISTEN_ADDRESS, QW_DATA_DIR and
/// QW_CONFIG, which would override the config), without AZURE_CLIENT_ID
/// (it selects the node's user-assigned identity for the registry; Quickwit
/// authenticates with the system-assigned one, 08 §8.2) and without an
/// inherited RAYON_NUM_THREADS when `threads` sets one, plus its address,
/// its seeds and its thread counts.
pub fn quickwit_env(
    inherited: impl IntoIterator<Item = (String, String)>,
    advertise: IpAddr,
    seeds: &[String],
    threads: Threads,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = inherited
        .into_iter()
        .filter(|(k, _)| !k.starts_with("QW_") && k != "AZURE_CLIENT_ID")
        .filter(|(k, _)| !(k == "RAYON_NUM_THREADS" && threads.search.is_some()))
        .collect();
    env.push(("QW_ADVERTISE_ADDRESS".into(), advertise.to_string()));
    if !seeds.is_empty() {
        env.push(("QW_PEER_SEEDS".into(), seeds.join(",")));
    }
    // Quickwit sizes its thread pools from this, before the cgroup.
    if let Some(n) = threads.cpus {
        env.push(("QW_NUM_CPUS".into(), n.to_string()));
    }
    if let Some(n) = threads.runtime {
        env.push(("QW_TOKIO_RUNTIME_NUM_THREADS".into(), n.to_string()));
    }
    if let Some(n) = threads.search {
        env.push(("RAYON_NUM_THREADS".into(), n.to_string()));
    }
    env.push(("QW_DISABLE_TELEMETRY".into(), "1".into()));
    env
}

/// `(major, minor)` of the first version number in `quickwit --version`'s
/// output (`quickwit 0.9.1 (x86_64-unknown-linux-gnu ...)`; a build from
/// main before 0.9 says `0.8.0`).
pub fn parse_version(text: &str) -> Option<(u32, u32)> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|word| {
            let mut parts = word.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            parts.next()?.parse::<u32>().ok()?;
            Some((major, minor))
        })
}

/// Whether `quickwit` is a build from before 0.9, which takes the legacy
/// searcher config ([`LEGACY_DROPPED`]). `None` when its version can't be
/// read: the caller keeps the 0.9 config, and a pre-0.9 build then refuses
/// it at start, naming the key.
pub fn is_pre_09(quickwit: &Path) -> Option<bool> {
    let out = std::process::Command::new(quickwit)
        .arg("--version")
        .env("QW_DISABLE_TELEMETRY", "1")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (major, minor) = parse_version(&text)?;
    Some(major == 0 && minor < 9)
}

/// Register, find the peers, write the config and run Quickwit in this
/// process's place; a standalone node (no `registry`) skips the first two.
/// Returns only on failure.
pub async fn start(
    registry: Option<&dyn ObjectStore>,
    spec: &NodeSpec,
    advertise: IpAddr,
    threads: Threads,
    quickwit: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        spec.standalone == registry.is_none(),
        "a standalone node takes no seed registry, and a cluster node needs one"
    );
    let config = spec.config()?;
    std::fs::create_dir_all(&spec.data_dir)
        .with_context(|| format!("creating {}", spec.data_dir.display()))?;
    let addr = std::net::SocketAddr::new(advertise, spec.rest_port).to_string();
    let peers = match registry {
        Some(registry) => {
            let reg = Registration {
                node_id: spec.node_id.clone(),
                addr: addr.clone(),
                registered_at: Utc::now(),
                replica: std::env::var("HOSTNAME").ok(),
            };
            register(registry, &reg).await?;
            let all = registered(registry).await?;
            seeds(&all, &spec.node_id)
        }
        None => Vec::new(),
    };
    tracing::info!(
        node = %spec.node_id,
        addr = %addr,
        services = %spec.services.join(","),
        seeds = %peers.join(","),
        standalone = spec.standalone,
        legacy_searcher = spec.legacy_searcher,
        cpus = ?threads.cpus,
        runtime_threads = ?threads.runtime,
        search_threads = ?threads.search,
        "starting a cluster node"
    );
    // The disk the data directory (and a split cache) is on, for the
    // local-disk test (#251): Container Apps documents 8 GiB a replica.
    if let Some(df) = disk_free(&spec.data_dir) {
        tracing::info!(dir = %spec.data_dir.display(), df = %df, "data directory disk");
    }
    let path = spec.data_dir.join("node.yaml");
    std::fs::write(&path, config).with_context(|| format!("writing {}", path.display()))?;
    let env = quickwit_env(std::env::vars(), advertise, &peers, threads);
    let mut cmd = std::process::Command::new(quickwit);
    cmd.env_clear()
        .envs(env)
        .args(["run", "--config"])
        .arg(&path);
    exec(cmd, quickwit)
}

/// `df -k`'s line for `dir`, if `df` runs.
pub fn disk_free(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("df")
        .arg("-k")
        .arg(dir)
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
}

#[cfg(unix)]
fn exec(mut cmd: std::process::Command, bin: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let err = cmd.exec();
    Err(err).with_context(|| format!("starting {}", bin.display()))
}

#[cfg(not(unix))]
fn exec(mut cmd: std::process::Command, bin: &Path) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("starting {}", bin.display()))?;
    bail!("{} exited: {status}", bin.display())
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;

    fn spec(services: &[&str]) -> NodeSpec {
        NodeSpec {
            node_id: "qw-0".into(),
            services: services.iter().map(|s| s.to_string()).collect(),
            rest_port: 7280,
            data_dir: "/work/qwdata".into(),
            metastore: "azure://qw-cluster".into(),
            index_root: "azure://qw-cluster".into(),
            storage_account: "stusnmdabc123".into(),
            split_searches: None,
            standalone: false,
            legacy_searcher: false,
            split_cache_gib: None,
        }
    }

    #[test]
    fn a_split_cache_goes_in_the_searcher_section() {
        let mut s = spec(&["searcher", "metastore"]);
        assert!(!s.config().unwrap().contains("split_cache"));
        s.split_cache_gib = Some(40);
        let c = s.config().unwrap();
        assert!(
            c.contains("searcher:\n  split_cache:\n    max_num_bytes: 40GiB\n    max_num_splits: 10000\n    num_concurrent_downloads: 4\n"),
            "{c}"
        );
        s.split_cache_gib = Some(0);
        assert!(s.config().is_err());
    }

    #[test]
    fn concurrent_split_searches_can_be_set() {
        let mut s = spec(&["searcher"]);
        assert!(s
            .config()
            .unwrap()
            .contains("max_num_concurrent_split_searches: 48"));
        s.split_searches = Some(24);
        let c = s.config().unwrap();
        assert!(
            c.contains("  max_num_concurrent_split_searches: 24\n"),
            "{c}"
        );
        assert!(!c.contains("max_num_concurrent_split_searches: 48"));
        s.split_searches = Some(0);
        assert!(s.config().is_err());
    }

    /// A standalone node has a cluster id of its own, so two never join.
    #[test]
    fn a_standalone_node_has_its_own_cluster() {
        let mut s = spec(&["searcher", "metastore"]);
        s.node_id = "qws-1".into();
        s.standalone = true;
        let c = s.config().unwrap();
        assert!(
            c.lines().any(|l| l == "cluster_id: usnm-qwcmp-qws-1"),
            "{c}"
        );
        assert!(!c.contains("cluster_id: usnm-qwcluster"));
        s.node_id = "qws-0".into();
        assert!(s.config().unwrap().contains("cluster_id: usnm-qwcmp-qws-0"));
        assert!(spec(&["searcher"])
            .config()
            .unwrap()
            .contains("cluster_id: usnm-qwcluster\n"));
    }

    /// For a pre-0.9 build: the searcher keys it would refuse are gone, the
    /// warm-up allocation is pinned, and everything else is the sidecar's.
    #[test]
    fn the_legacy_searcher_config_drops_only_the_unknown_keys() {
        let mut s = spec(&["searcher", "metastore"]);
        s.legacy_searcher = true;
        s.split_searches = Some(24);
        let c = s.config().unwrap();
        for k in LEGACY_DROPPED {
            assert!(
                !c.lines().any(|l| l.trim_start().starts_with(k)),
                "{k} in\n{c}"
            );
        }
        assert!(
            c.contains("searcher:\n  warmup_single_split_initial_allocation: 300MB\n"),
            "{c}"
        );
        assert!(c.contains("  max_num_concurrent_split_searches: 24\n"));
        for kept in [
            "warmup_memory_budget: 3GB",
            "request_timeout_secs: 125",
            "fast_field_cache_capacity: 1GB",
        ] {
            assert!(c.contains(kept), "{kept} in\n{c}");
        }
        let normal = spec(&["searcher"]).config().unwrap();
        assert_eq!(
            normal.lines().count(),
            c.lines().count() + LEGACY_DROPPED.len() - 1
        );
    }

    #[test]
    fn reads_quickwit_versions() {
        assert_eq!(
            parse_version("quickwit 0.9.1 (x86_64-unknown-linux-gnu 2026-09-22T10:00:00Z 962685f)"),
            Some((0, 9))
        );
        assert_eq!(
            parse_version("Quickwit 0.8.0-nightly (aarch64 2025-09-23 eec5bbc)"),
            Some((0, 8))
        );
        assert_eq!(parse_version("quickwit 1.0.0"), Some((1, 0)));
        // A date or a short hash isn't a version.
        assert_eq!(parse_version("built 2026-09-22 962685f"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn threads_default_to_one_per_vcpu() {
        let t = Threads::new(Some(4), None, None).unwrap();
        assert_eq!((t.runtime, t.search), (Some(4), Some(4)));
        let t = Threads::new(Some(2), Some(3), None).unwrap();
        assert_eq!((t.runtime, t.search), (Some(3), Some(2)));
        assert_eq!(Threads::new(None, None, None).unwrap(), Threads::default());
        assert!(Threads::new(Some(2), Some(0), None).is_err());
        assert!(Threads::new(Some(0), None, None).is_err());
    }

    #[test]
    fn fills_in_the_node_config() {
        let yaml = spec(&[
            "metastore",
            "control_plane",
            "janitor",
            "indexer",
            "searcher",
        ])
        .config()
        .unwrap();
        for line in [
            "node_id: qw-0",
            "enabled_services: [metastore, control_plane, janitor, indexer, searcher]",
            "  listen_port: 7280",
            "grpc_listen_port: 7281",
            "data_dir: /work/qwdata",
            "metastore_uri: azure://qw-cluster",
            "default_index_root_uri: azure://qw-cluster",
            "    account: stusnmdabc123",
            "listen_address: 0.0.0.0",
        ] {
            assert!(yaml.lines().any(|l| l == line), "{line} in\n{yaml}");
        }
        // Never the production index (08 §8.4.1).
        assert!(!yaml.contains("qw-index"));
    }

    #[test]
    fn refuses_bad_node_settings() {
        assert!(spec(&[]).config().is_err());
        assert!(spec(&["searcher", "coordinator"]).config().is_err());
        let mut s = spec(&["searcher"]);
        s.node_id = "../qw".into();
        assert!(s.config().is_err());
        s.node_id = "qw-1".into();
        s.rest_port = 65535;
        assert!(s.config().is_err());
    }

    /// The nodes search as the API's sidecar does: the same searcher settings.
    #[test]
    fn searcher_settings_match_the_sidecar() {
        let section = |yaml: &str| -> Vec<String> {
            yaml.lines()
                .skip_while(|l| !l.starts_with("searcher:"))
                .skip(1)
                .take_while(|l| l.starts_with(' ') || l.trim().is_empty())
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_owned)
                .collect()
        };
        let sidecar = section(include_str!("../../../../infra/quickwit/searcher.yaml"));
        assert!(!sidecar.is_empty());
        assert_eq!(section(NODE_TEMPLATE), sidecar);
    }

    #[test]
    fn picks_the_replica_address() {
        let lo = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let link = IpAddr::V4(Ipv4Addr::new(169, 254, 1, 2));
        let replica = IpAddr::V4(Ipv4Addr::new(100, 100, 196, 158));
        let v6 = IpAddr::V6("fd00::5".parse::<Ipv6Addr>().unwrap());
        let v6_link = IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap());
        assert_eq!(pick_address(&[lo, link, replica]), Some(replica));
        assert_eq!(pick_address(&[v6, replica]), Some(replica));
        assert_eq!(pick_address(&[lo, v6_link, v6]), Some(v6));
        assert_eq!(pick_address(&[lo, link]), None);
    }

    #[test]
    fn quickwit_gets_its_address_and_seeds_and_nothing_inherited_from_qw() {
        let inherited = [
            ("PATH", "/usr/bin"),
            ("QW_LISTEN_ADDRESS", "0.0.0.0"),
            ("QW_CONFIG", "/quickwit/config/quickwit.yaml"),
            ("QW_PEER_SEEDS", "10.0.0.9:7280"),
            ("AZURE_CLIENT_ID", "abc"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()));
        let ip = IpAddr::V4(Ipv4Addr::new(100, 100, 1, 2));
        let env: BTreeMap<String, String> = quickwit_env(
            inherited,
            ip,
            &["100.100.1.3:7280".into(), "100.100.1.4:7280".into()],
            Threads::new(Some(2), None, None).unwrap(),
        )
        .into_iter()
        .collect();
        assert_eq!(env["PATH"], "/usr/bin");
        assert_eq!(env["QW_ADVERTISE_ADDRESS"], "100.100.1.2");
        assert_eq!(env["QW_PEER_SEEDS"], "100.100.1.3:7280,100.100.1.4:7280");
        assert_eq!(env["QW_NUM_CPUS"], "2");
        // Not Quickwit's third of the CPUs (1 thread at 2 vCPU): one per vCPU.
        assert_eq!(env["QW_TOKIO_RUNTIME_NUM_THREADS"], "2");
        assert_eq!(env["RAYON_NUM_THREADS"], "2");
        assert!(!env.contains_key("QW_LISTEN_ADDRESS") && !env.contains_key("QW_CONFIG"));
        assert!(!env.contains_key("AZURE_CLIENT_ID"));
        // Ours replaces an inherited search pool size; one we don't set stays.
        let inherited = [("RAYON_NUM_THREADS".to_owned(), "9".to_owned())];
        let env: BTreeMap<String, String> = quickwit_env(
            inherited.clone(),
            ip,
            &[],
            Threads::new(Some(4), Some(4), Some(3)).unwrap(),
        )
        .into_iter()
        .collect();
        assert_eq!(env["RAYON_NUM_THREADS"], "3");
        assert_eq!(env["QW_TOKIO_RUNTIME_NUM_THREADS"], "4");
        let env: BTreeMap<String, String> = quickwit_env(inherited, ip, &[], Threads::default())
            .into_iter()
            .collect();
        assert_eq!(env["RAYON_NUM_THREADS"], "9");
        // The first node to start has no seeds.
        let env = quickwit_env([], ip, &[], Threads::default());
        assert!(!env.iter().any(|(k, _)| k == "QW_PEER_SEEDS"
            || k == "QW_NUM_CPUS"
            || k == "QW_TOKIO_RUNTIME_NUM_THREADS"));
    }

    #[tokio::test]
    async fn the_registry_holds_one_entry_per_node() {
        let dir = tempfile::tempdir().unwrap();
        let store = usnm_store::open(dir.path().to_str().unwrap()).unwrap();
        let reg = |id: &str, addr: &str| Registration {
            node_id: id.into(),
            addr: addr.into(),
            registered_at: Utc::now(),
            replica: None,
        };
        register(store.as_ref(), &reg("qw-0", "100.100.0.1:7280"))
            .await
            .unwrap();
        register(store.as_ref(), &reg("qw-1", "100.100.0.2:7280"))
            .await
            .unwrap();
        // A restart replaces the node's entry.
        register(store.as_ref(), &reg("qw-1", "100.100.0.9:7280"))
            .await
            .unwrap();
        store
            .put("seeds/junk.json", b"not json".to_vec(), "application/json")
            .await
            .unwrap();
        let all = registered(store.as_ref()).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(seeds(&all, "qw-0"), ["100.100.0.9:7280"]);
        assert_eq!(seeds(&all, "qw-1"), ["100.100.0.1:7280"]);
        assert_eq!(seeds(&all, "qw-2").len(), 2);
    }
}
