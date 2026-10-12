//! What the page and `/api/*.json` show, worked out from the state.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use usnm_ocr_contract::status::{Completion, Dataset, InFlight, Outcomes, Pool, QueueStats};

use crate::fetch::{FailureLine, Fetch, FetchStatus, Tally, GROUPS};
use crate::hosts::Hosts;
use crate::nodes::Nodes;
use crate::sset::{SsetStatus, Ssets};

/// Settings that shape the view.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    /// Guests whose names start with this are shown.
    pub guest_prefix: String,
    /// Rows in each "what's next" list and each node's latest pages.
    pub next_n: usize,
    /// The Azure workbook for the nodes, for what only Azure has.
    pub workbook_url: Option<String>,
    /// Seconds between the page's refreshes.
    pub refresh_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct View {
    pub generated_at: DateTime<Utc>,
    pub refresh_secs: u64,
    pub hosts: Vec<HostView>,
    pub fetch: FetchView,
    pub nodes: Vec<NodeView>,
    pub serial_set: Vec<SsetView>,
    pub next: NextView,
    pub ocred: OcredView,
    pub azure: AzureView,
}

#[derive(Debug, Serialize)]
pub struct HostView {
    pub name: String,
    pub received: Option<DateTime<Utc>>,
    pub collected_at: Option<DateTime<Utc>>,
    pub age_secs: Option<i64>,
    pub error: Option<String>,
    pub cpu_pct: f64,
    pub cpus: u32,
    pub load: Vec<f64>,
    pub mem_used: u64,
    pub mem_total: u64,
    pub uptime_secs: u64,
    pub pools: Vec<Pool>,
    pub datasets: Vec<Dataset>,
    pub guests: Vec<GuestView>,
}

#[derive(Debug, Serialize)]
pub struct GuestView {
    pub vmid: u32,
    pub kind: String,
    pub name: String,
    pub status: String,
    /// Of the guest's CPUs.
    pub cpu_pct: f64,
    pub cpus: f64,
    pub mem: u64,
    pub maxmem: u64,
    pub uptime_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct Rate {
    pub pages: u64,
    pub bytes: u64,
    pub pages_per_hour: f64,
    pub mb_per_s: f64,
}

#[derive(Debug, Serialize)]
pub struct SetView {
    pub name: &'static str,
    pub total: u64,
    pub fetched: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct GroupView {
    pub n: u8,
    pub name: &'static str,
    pub total: u64,
    pub fetched: u64,
    pub bytes: u64,
    pub current: bool,
    /// Until the fetch finishes this group, at the last hour's rate.
    pub eta_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct FetchView {
    pub status: Option<FetchStatus>,
    pub status_error: Option<String>,
    pub plan_error: Option<String>,
    pub sets: Vec<SetView>,
    pub groups: Vec<GroupView>,
    pub last_hour: Rate,
    pub last_day: Rate,
    pub holds_last_hour: usize,
    pub holds_last_day: usize,
    /// Holds come from the fetcher's counter, or from watching its status.
    pub holds_counted: bool,
    pub failures_last_hour: usize,
    pub failures_last_day: usize,
    pub dead_pages: usize,
    /// Pages skipped while loc.gov can't serve their issue's item.
    pub cooling_pages: usize,
    pub recent_failures: Vec<FailureLine>,
    pub keep_eta_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct NodeView {
    pub name: String,
    pub url: String,
    pub ok: bool,
    pub last_ok: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub node_id: String,
    pub version: String,
    pub engine: String,
    pub engine_version: String,
    pub models: Vec<String>,
    pub routes: BTreeMap<String, String>,
    pub concurrency: u32,
    pub scan_cache_url: Option<String>,
    pub loc_hold_secs: Option<u64>,
    pub in_flight: Vec<InFlight>,
    pub last_hour: Outcomes,
    pub last_day: Outcomes,
    pub pages_per_min: f64,
    /// Pages done in the last hour.
    pub pages_per_hour: u64,
    /// `working`, `idle` (nothing in flight, no page for [`IDLE_AFTER`])
    /// or `no status`.
    pub state: &'static str,
    pub started_at: Option<DateTime<Utc>>,
    /// By the worker's own clock.
    pub uptime_secs: Option<u64>,
    /// When the latest page finished OK.
    pub last_page_at: Option<DateTime<Utc>>,
}

/// A worker with nothing in flight and no page done for this long is idle.
pub const IDLE_AFTER: Duration = Duration::minutes(10);
/// A Serial Set status older than this is marked stale (it is pushed every
/// ten minutes).
pub const SSET_FRESH: Duration = Duration::minutes(25);

#[derive(Debug, Serialize)]
pub struct SsetView {
    pub name: String,
    pub received: DateTime<Utc>,
    pub age_secs: i64,
    pub stale: bool,
    /// `finished`, `running`, or systemd's word for the unit.
    pub state: String,
    pub status: SsetStatus,
    /// Curated, empty and incomplete volumes.
    pub done: u64,
}

#[derive(Debug, Serialize)]
pub struct NextRow {
    pub doc_id: String,
    pub lang: String,
    pub group: &'static str,
    pub fetched: bool,
    pub in_flight: bool,
}

#[derive(Debug, Serialize)]
pub struct NextView {
    /// Keep-set pages in fetch order not yet OCRed.
    pub to_ocr: Vec<NextRow>,
    pub keep_left_to_ocr: u64,
    /// Keep-set pages in fetch order not yet fetched.
    pub to_fetch: Vec<NextRow>,
    pub seed_plan: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Serialize)]
pub struct NodeLatest {
    pub node: String,
    pub rows: Vec<Completion>,
}

#[derive(Debug, Serialize)]
pub struct OcredView {
    pub pages: u64,
    pub by_language: Vec<(String, u64)>,
    pub by_engine: Vec<(String, u64)>,
    pub latest: Vec<NodeLatest>,
    pub workbook_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AzureView {
    /// The queues as the node that looked last saw them, its dead letters
    /// joined with the other nodes' (by message id).
    pub queues: Option<QueueStats>,
    /// The node whose counts these are.
    pub queues_from: Option<String>,
    /// The look is older than [`QUEUES_FRESH`].
    pub queues_stale: bool,
    pub queues_note: &'static str,
    pub imports: &'static str,
    pub workbook_url: Option<String>,
}

/// A node counts for the queues only if it answered this recently.
pub const NODE_FRESH: Duration = Duration::minutes(3);
/// A look at the queues older than this is marked stale.
pub const QUEUES_FRESH: Duration = Duration::minutes(5);

/// Where the queues come from (12 §12.2.5).
pub const QUEUES_NOTE: &str = "From the nodes: each worker with a status port looks once a minute, with Get Queue \
Metadata and Peek Messages (Storage Queue Data Reader on each queue), and takes nothing. The lengths count hidden \
messages too; the dead letters are the oldest visible ones.";

/// The freshest look at the queues among the nodes, with every node's
/// dead letters joined.
pub fn merge_queues(nodes: &Nodes, now: DateTime<Utc>) -> (Option<QueueStats>, Option<String>) {
    let mut looks: Vec<(&str, &QueueStats)> = nodes
        .nodes
        .iter()
        // Only nodes answering now, so an offline node's old dead
        // letters don't linger.
        .filter(|n| n.error.is_none() && n.last_ok.is_some_and(|t| now - t <= NODE_FRESH))
        .filter_map(|n| Some((n.name.as_str(), n.status.as_ref()?.queues.as_ref()?)))
        .collect();
    looks.sort_by_key(|(_, q)| std::cmp::Reverse(q.checked_at));
    let Some((from, first)) = looks.first() else {
        return (None, None);
    };
    let mut q = (*first).clone();
    let mut seen: HashSet<String> = q
        .dead_letters
        .iter()
        .map(|d| d.message_id.clone())
        .collect();
    for (_, other) in &looks[1..] {
        for d in &other.dead_letters {
            if d.message_id.is_empty() || seen.insert(d.message_id.clone()) {
                q.dead_letters.push(d.clone());
            }
        }
    }
    // Oldest first, across nodes.
    q.dead_letters.sort_by(|a, b| a.at.cmp(&b.at));
    (Some(q), Some((*from).to_owned()))
}

/// Why imports aren't shown.
pub const IMPORTS_NOTE: &str =
    "In Azure. ocr-sync's import counts and the damage of our text against LoC's are in the \
curated store and the ocr-sync job's logs, which nothing on the LAN can read without a new grant.";

fn rate(t: Tally, secs: f64) -> Rate {
    Rate {
        pages: t.pages,
        bytes: t.bytes,
        pages_per_hour: (t.pages as f64 * 3600.0 / secs * 10.0).round() / 10.0,
        mb_per_s: (t.bytes as f64 / secs / 1e6 * 100.0).round() / 100.0,
    }
}

fn fetch_view(f: &mut Fetch, now: DateTime<Utc>) -> FetchView {
    let hour = rate(f.since(now - Duration::hours(1)), 3600.0);
    let day = rate(f.since(now - Duration::days(1)), 86400.0);
    let per_hour = if hour.pages > 0 {
        hour.pages_per_hour
    } else {
        day.pages_per_hour
    };
    let eta = |left: u64| (per_hour > 0.0).then(|| (left as f64 / per_hour * 3600.0) as u64);
    let current = f.current_group();
    let mut groups = Vec::new();
    let mut sets = Vec::new();
    let mut keep_eta = None;
    if let Some(plan) = &f.plan {
        let mut left_so_far = 0;
        for (i, name) in GROUPS.iter().enumerate() {
            let total = plan.group_pages[i];
            let t = f.groups[i];
            let left = total.saturating_sub(t.pages);
            left_so_far += left;
            groups.push(GroupView {
                n: i as u8,
                name,
                total,
                fetched: t.pages,
                bytes: t.bytes,
                current: current == Some(i as u8),
                eta_secs: if left == 0 { None } else { eta(left_so_far) },
            });
        }
        keep_eta = eta(left_so_far);
        let kept: Tally = f.groups.iter().fold(Tally::default(), |a, t| Tally {
            pages: a.pages + t.pages,
            bytes: a.bytes + t.bytes,
        });
        sets.push(SetView {
            name: "keep",
            total: plan.keep.len() as u64,
            fetched: kept.pages,
            bytes: kept.bytes,
        });
        sets.push(SetView {
            name: "buffer",
            total: plan.buffer_pages,
            fetched: f.buffer.pages,
            bytes: f.buffer.bytes,
        });
    }
    FetchView {
        status: f.status.clone(),
        status_error: f.status_error.clone(),
        plan_error: f.plan_error.clone(),
        sets,
        groups,
        last_hour: hour,
        last_day: day,
        holds_last_hour: f.holds_since(now - Duration::hours(1)),
        holds_last_day: f.holds_since(now - Duration::days(1)),
        holds_counted: f.status.as_ref().is_some_and(|s| s.loc_holds.is_some()),
        failures_last_hour: f.failures_since(now - Duration::hours(1)),
        failures_last_day: f.failures_since(now - Duration::days(1)),
        dead_pages: f.dead.len(),
        cooling_pages: f.cooling_pages(),
        recent_failures: f.failures.iter().rev().take(15).cloned().collect(),
        keep_eta_secs: keep_eta,
    }
}

fn host_views(h: &Hosts, s: &Settings, now: DateTime<Utc>) -> Vec<HostView> {
    h.by_name
        .iter()
        .map(|(name, host)| {
            let st = host.stats.clone().unwrap_or_default();
            HostView {
                name: name.clone(),
                received: host.received,
                collected_at: host.stats.as_ref().map(|st| st.collected_at),
                // From when the host collected them, so a stopped timer
                // shows even while its last file is read again and again.
                age_secs: host
                    .stats
                    .as_ref()
                    .map(|st| (now - st.collected_at).num_seconds()),
                error: host.error.clone(),
                cpu_pct: (st.node.cpu * 1000.0).round() / 10.0,
                cpus: st.node.cpuinfo.cpus,
                load: st
                    .node
                    .loadavg
                    .iter()
                    .filter_map(|l| l.parse().ok())
                    .collect(),
                mem_used: st.node.memory.used,
                mem_total: st.node.memory.total,
                uptime_secs: st.node.uptime,
                pools: st.pools.clone(),
                datasets: st.datasets.clone(),
                guests: st
                    .resources
                    .iter()
                    .filter(|g| g.node == st.host && g.name.starts_with(&s.guest_prefix))
                    .map(|g| GuestView {
                        vmid: g.vmid,
                        kind: g.kind.clone(),
                        name: g.name.clone(),
                        status: g.status.clone(),
                        cpu_pct: (g.cpu * 1000.0).round() / 10.0,
                        cpus: g.maxcpu,
                        mem: g.mem,
                        maxmem: g.maxmem,
                        uptime_secs: g.uptime,
                    })
                    .collect(),
            }
        })
        .collect()
}

/// The whole view.
pub fn build(
    fetch: &mut Fetch,
    nodes: &Nodes,
    hosts: &Hosts,
    ssets: &Ssets,
    seed_plan: Option<BTreeMap<String, String>>,
    s: &Settings,
    now: DateTime<Utc>,
) -> View {
    let fv = fetch_view(fetch, now);
    let in_flight: HashSet<&str> = nodes
        .nodes
        .iter()
        .filter_map(|n| n.status.as_ref())
        .flat_map(|st| st.in_flight.iter().map(|p| p.doc_id.as_str()))
        .collect();
    let group_name = |g: u8| GROUPS[usize::from(g.min(5))];
    let to_fetch: Vec<NextRow> = fetch
        .next_to_fetch(s.next_n)
        .into_iter()
        .map(|r| NextRow {
            doc_id: r.doc_id.to_string(),
            lang: r.lang.to_string(),
            group: group_name(r.group),
            fetched: false,
            in_flight: in_flight.contains(&*r.doc_id),
        })
        .collect();
    let (to_ocr, keep_left) = match &fetch.plan {
        Some(p) => (
            p.keep
                .iter()
                .filter(|r| !nodes.ocred.contains_key(&r.doc_id))
                .take(s.next_n)
                .map(|r| NextRow {
                    doc_id: r.doc_id.to_string(),
                    lang: r.lang.to_string(),
                    group: group_name(r.group),
                    fetched: fetch.fetched.contains_key(&r.doc_id),
                    in_flight: in_flight.contains(&*r.doc_id),
                })
                .collect(),
            p.keep
                .iter()
                .filter(|r| !nodes.ocred.contains_key(&r.doc_id))
                .count() as u64,
        ),
        None => (Vec::new(), 0),
    };
    let mut by_language: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_engine: BTreeMap<&str, u64> = BTreeMap::new();
    for o in nodes.ocred.values() {
        *by_language
            .entry(&nodes.names[o.language as usize])
            .or_default() += 1;
        *by_engine
            .entry(&nodes.names[o.engine as usize])
            .or_default() += 1;
    }
    let sorted = |m: BTreeMap<&str, u64>| {
        let mut v: Vec<(String, u64)> = m.into_iter().map(|(k, n)| (k.to_owned(), n)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    };
    // Each node's latest pages: from its status while it answers (failures
    // too), else the pages OCRed copied here.
    let latest = nodes
        .nodes
        .iter()
        .map(|n| NodeLatest {
            node: n.name.clone(),
            rows: match &n.status {
                Some(st) => st.recent.iter().take(s.next_n).cloned().collect(),
                None => n.latest.iter().rev().take(s.next_n).cloned().collect(),
            },
        })
        .collect();
    let node_views = nodes
        .nodes
        .iter()
        .map(|n| {
            let st = n.status.clone().unwrap_or_default();
            let ok = n.error.is_none() && n.status.is_some();
            // The status's recent list is bounded and starts empty after a
            // restart; the pages copied here (`latest`) outlast both.
            let last_page_at = st
                .recent
                .iter()
                .chain(n.latest.iter())
                .filter(|c| c.outcome == "done")
                .map(|c| c.finished_at)
                .max();
            let busy =
                !st.in_flight.is_empty() || last_page_at.is_some_and(|t| st.now - t <= IDLE_AFTER);
            NodeView {
                name: n.name.clone(),
                url: n.url.clone(),
                ok,
                state: match (ok, busy) {
                    (false, _) => "no status",
                    (true, true) => "working",
                    (true, false) => "idle",
                },
                started_at: n.status.as_ref().map(|s| s.started_at),
                uptime_secs: n
                    .status
                    .as_ref()
                    .map(|s| (s.now - s.started_at).num_seconds().max(0) as u64),
                last_page_at,
                pages_per_hour: st.last_hour.done,
                last_ok: n.last_ok,
                error: n.error.clone(),
                node_id: st.node_id,
                version: st.version,
                engine: st.engine,
                engine_version: st.engine_version,
                models: st.models,
                routes: st.routes,
                concurrency: st.concurrency,
                scan_cache_url: st.scan_cache_url,
                loc_hold_secs: st.loc_hold_secs,
                in_flight: st.in_flight,
                pages_per_min: (st.last_hour.done as f64 / 60.0 * 100.0).round() / 100.0,
                last_hour: st.last_hour,
                last_day: st.last_day,
            }
        })
        .collect();
    let queues = merge_queues(nodes, now);
    View {
        generated_at: now,
        refresh_secs: s.refresh_secs,
        hosts: host_views(hosts, s, now),
        fetch: fv,
        nodes: node_views,
        serial_set: ssets
            .by_name
            .iter()
            .map(|(name, s)| {
                let st = &s.status;
                SsetView {
                    name: name.clone(),
                    received: s.received,
                    age_secs: (now - s.received).num_seconds(),
                    stale: now - s.received > SSET_FRESH,
                    state: if st.finished {
                        "finished".to_owned()
                    } else if st.active == "active" {
                        "running".to_owned()
                    } else {
                        st.active.clone()
                    },
                    done: st.curated + st.empty + st.incomplete,
                    status: st.clone(),
                }
            })
            .collect(),
        next: NextView {
            to_ocr,
            keep_left_to_ocr: keep_left,
            to_fetch,
            seed_plan,
        },
        ocred: OcredView {
            pages: nodes.ocred.len() as u64,
            by_language: sorted(by_language),
            by_engine: sorted(by_engine),
            latest,
            workbook_url: s.workbook_url.clone(),
        },
        azure: AzureView {
            queues_stale: queues
                .0
                .as_ref()
                .is_some_and(|q| now - q.checked_at > QUEUES_FRESH),
            queues: queues.0,
            queues_from: queues.1,
            queues_note: QUEUES_NOTE,
            imports: IMPORTS_NOTE,
            workbook_url: s.workbook_url.clone(),
        },
    }
}

/// A seed plan file: a flat JSON object, shown as it is (keys up to 64
/// characters, values up to 200).
pub fn read_seed_plan(bytes: &[u8]) -> anyhow::Result<BTreeMap<String, String>> {
    anyhow::ensure!(bytes.len() <= 64 * 1024, "the seed plan is over 64 KB");
    let v: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(bytes)?;
    Ok(v.into_iter()
        .take(100)
        .map(|(k, v)| {
            let v = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (k.chars().take(64).collect(), v.chars().take(200).collect())
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_ocr_contract::status::HostStats;

    #[test]
    fn a_host_is_as_old_as_its_stats_not_the_last_read() {
        let now = Utc::now();
        let mut h = Hosts::new(Vec::new());
        h.put(
            HostStats {
                host: "pve-big".into(),
                collected_at: now - Duration::hours(2),
                ..HostStats::default()
            },
            now,
        );
        let v = host_views(&h, &Settings::default(), now);
        assert_eq!(v[0].age_secs, Some(7200));
    }

    #[test]
    fn the_freshest_look_at_the_queues_with_every_nodes_dead_letters() {
        use usnm_ocr_contract::status::{DeadLetterView, WorkerStatus};
        let dir = tempfile::tempdir().unwrap();
        let mut n = Nodes::open(
            dir.path().to_owned(),
            &[
                ("n1".to_owned(), "http://n1.example:8381".to_owned()),
                ("n2".to_owned(), "http://n2.example:8381".to_owned()),
            ],
        )
        .unwrap();
        assert_eq!(merge_queues(&n, Utc::now()), (None, None));
        let dl = |id: &str| DeadLetterView {
            message_id: id.into(),
            at: format!("2026-10-11T23:00:0{}Z", id.len()),
            ..DeadLetterView::default()
        };
        let now = Utc::now();
        let look = |at, counts: u64, ids: &[&str]| WorkerStatus {
            queues: Some(QueueStats {
                checked_at: at,
                counts: [("ocr-bulk".to_owned(), counts)].into(),
                dead_letters: ids.iter().map(|i| dl(i)).collect(),
                error: None,
            }),
            ..WorkerStatus::default()
        };
        n.nodes[0].status = Some(look(now - Duration::minutes(5), 10, &["a", "b"]));
        n.nodes[1].status = Some(look(now, 7, &["b", "c"]));
        n.nodes[0].last_ok = Some(now);
        n.nodes[1].last_ok = Some(now);
        let (q, from) = merge_queues(&n, now);
        let q = q.unwrap();
        assert_eq!(from.as_deref(), Some("n2"));
        assert_eq!(q.counts["ocr-bulk"], 7);
        let ids: Vec<&str> = q
            .dead_letters
            .iter()
            .map(|d| d.message_id.as_str())
            .collect();
        assert_eq!(ids.len(), 3);
        // A node whose last poll failed doesn't count.
        n.nodes[1].error = Some("HTTP 503".into());
        let (q, from) = merge_queues(&n, now);
        assert_eq!(from.as_deref(), Some("n1"));
        assert_eq!(q.unwrap().counts["ocr-bulk"], 10);
        // Nor one that hasn't answered within the window.
        n.nodes[0].last_ok = Some(now - Duration::minutes(10));
        assert_eq!(merge_queues(&n, now), (None, None));
    }
}
