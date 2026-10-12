//! What the LAN dashboard (`usnm-ocrdash`, 12 §12.2.5) reads from the
//! pipeline's local parts: a worker's status and its journal of finished
//! pages, and a Proxmox host's stats.
//!
//! None of it carries page text: pages are named by doc_id, and the rest
//! is counts, sizes, times and engine tags. [`Completion::check`] holds a
//! journal line to that before the dashboard keeps it.
//!
//! Every struct takes `#[serde(default)]`, so a dashboard and a worker or
//! host script a version apart still read each other: a field one side
//! doesn't know is ignored, and one it misses is empty.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use usnm_core::ids::PageKey;

/// [`WorkerStatus::schema`] and [`HostStats::schema`] today.
pub const SCHEMA: u32 = 1;

/// The most [`WorkerStatus::recent`] holds.
pub const RECENT_MAX: usize = 50;

/// A worker's `GET /status`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkerStatus {
    pub schema: u32,
    pub node_id: String,
    /// `usnm-ocr-worker --version`.
    pub version: String,
    pub started_at: DateTime<Utc>,
    /// When this status was made, by the worker's clock.
    pub now: DateTime<Utc>,
    /// Pages in flight at most.
    pub concurrency: u32,
    /// The engine's source tag (`tesseract`), and its version.
    pub engine: String,
    pub engine_version: String,
    /// The models the engine has.
    pub models: Vec<String>,
    /// Language (ISO 639-3) → the model or engine it is read with, where
    /// the worker routes languages; empty where it doesn't.
    pub routes: BTreeMap<String, String>,
    /// The scan server the worker asks before LoC, if any.
    pub scan_cache_url: Option<String>,
    /// Seconds loc.gov still holds this node's requests for.
    pub loc_hold_secs: Option<u64>,
    pub in_flight: Vec<InFlight>,
    pub last_hour: Outcomes,
    pub last_day: Outcomes,
    pub since_start: Outcomes,
    /// The latest finished pages, newest first, at most [`RECENT_MAX`].
    pub recent: Vec<Completion>,
    /// The journal's size, for `GET /journal?from=<offset>`.
    pub journal_bytes: u64,
    /// Which journal file it is (it changes when the file is replaced), so
    /// a reader knows to start again from 0.
    pub journal_id: String,
}

/// A page a worker is on now.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InFlight {
    pub doc_id: String,
    /// ISO 639-3, from the message.
    pub language: String,
    /// `ocr-priority` or `ocr-bulk`.
    pub queue: String,
    pub started_at: DateTime<Utc>,
    /// `fetch`, `ocr` or `upload`.
    pub stage: String,
}

/// Pages finished in a window, by outcome, and where their scans came
/// from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Outcomes {
    pub done: u64,
    /// Failed for now; the message comes back.
    pub failed: u64,
    /// Out of time, nothing written; the message comes back.
    pub abandoned: u64,
    pub dead_lettered: u64,
    /// Scans read from the scan server, and from LoC.
    pub from_cache: u64,
    pub from_loc: u64,
}

impl Outcomes {
    pub fn add(&mut self, c: &Completion) {
        match c.outcome.as_str() {
            "done" => self.done += 1,
            "failed" => self.failed += 1,
            "abandoned" => self.abandoned += 1,
            "dead_lettered" => self.dead_lettered += 1,
            _ => {}
        }
        match c.scan.as_str() {
            "cache" => self.from_cache += 1,
            "loc" => self.from_loc += 1,
            _ => {}
        }
    }
}

/// One finished page: a line of a worker's journal (`GET /journal`), and an
/// entry of [`WorkerStatus::recent`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Completion {
    pub doc_id: String,
    /// ISO 639-3, from the message.
    pub language: String,
    /// The engine tag of the page's row (`ocr_engine`, 13 §13.8.1), empty
    /// if it wasn't read.
    pub engine: String,
    pub queue: String,
    /// `bulk` or `user`.
    pub reason: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub secs: f64,
    /// `done`, `failed`, `abandoned` or `dead_lettered`.
    pub outcome: String,
    /// Where the scan came from: `cache`, `loc`, or empty if none was read.
    pub scan: String,
    /// The ALTO and the part are in `ocr-out`.
    pub uploaded: bool,
    /// For a failure, its kind (`fetch`, `engine`, `no_model`, `output`,
    /// `scratch`, `permanent`, `invalid`, `dequeue_count`); never the
    /// message.
    pub error_kind: Option<String>,
}

const OUTCOMES: [&str; 4] = ["done", "failed", "abandoned", "dead_lettered"];

/// A short tag: letters, digits and ` ._:+-()/=@,`, at most `max` bytes.
/// The engine tag (13 §13.8.1, `tesseract/5.5.0 profile=frak2021 image=…`)
/// and the kinds are tags; nothing longer gets through.
pub fn is_tag(s: &str, max: usize) -> bool {
    s.len() <= max
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b' ' | b'.'
                        | b'_'
                        | b':'
                        | b'+'
                        | b'-'
                        | b'('
                        | b')'
                        | b'/'
                        | b'='
                        | b'@'
                        | b','
                )
        })
}

fn canonical_doc_id(s: &str) -> bool {
    PageKey::from_doc_id(s).is_ok_and(|k| k.doc_id() == s)
}

impl InFlight {
    /// A real doc_id and short tags only.
    pub fn check(&self) -> bool {
        canonical_doc_id(&self.doc_id)
            && is_tag(&self.language, 8)
            && is_tag(&self.queue, 32)
            && is_tag(&self.stage, 16)
    }
}

impl WorkerStatus {
    /// Hold a status from the network to what the dashboard may show:
    /// pages with real doc_ids, and short tags. Anything else is dropped
    /// (a list entry) or blanked (a field), so a broken or hostile status
    /// can't put text on the page.
    pub fn sanitize(&mut self) {
        let tag = |s: &mut String, max: usize| {
            if !is_tag(s, max) {
                *s = "?".into();
            }
        };
        tag(&mut self.node_id, 64);
        tag(&mut self.version, 120);
        tag(&mut self.engine, 120);
        tag(&mut self.engine_version, 120);
        self.models.retain(|m| is_tag(m, 64));
        self.routes.retain(|k, v| is_tag(k, 8) && is_tag(v, 120));
        if self
            .scan_cache_url
            .as_deref()
            .is_some_and(|u| !is_tag(u, 200))
        {
            self.scan_cache_url = None;
        }
        self.in_flight.retain(InFlight::check);
        self.recent.retain(|c| c.check().is_ok());
    }
}

impl Completion {
    /// Holds a line to the journal's shape: a real doc_id, a known outcome
    /// and short tags only, so a broken or hostile line can't put text on
    /// the dashboard.
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(canonical_doc_id(&self.doc_id), "not a canonical doc_id");
        anyhow::ensure!(OUTCOMES.contains(&self.outcome.as_str()), "unknown outcome");
        anyhow::ensure!(
            matches!(self.scan.as_str(), "" | "cache" | "loc"),
            "unknown scan source"
        );
        anyhow::ensure!(
            is_tag(&self.language, 8)
                && is_tag(&self.engine, 240)
                && is_tag(&self.queue, 32)
                && is_tag(&self.reason, 16)
                && self.error_kind.as_deref().is_none_or(|k| is_tag(k, 32)),
            "a field isn't a short tag"
        );
        anyhow::ensure!(self.secs.is_finite() && self.secs >= 0.0, "bad secs");
        Ok(())
    }
}

/// A Proxmox host's stats, written every minute by
/// `scripts/ocr-node/hoststats.sh` as root on the host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostStats {
    pub schema: u32,
    pub host: String,
    pub collected_at: DateTime<Utc>,
    /// `pvesh get /nodes/<host>/status`, as Proxmox gives it.
    pub node: PveNode,
    /// `pvesh get /cluster/resources --type vm`: the host's guests.
    pub resources: Vec<PveGuest>,
    pub pools: Vec<Pool>,
    pub datasets: Vec<Dataset>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PveNode {
    /// CPU in use, 0 to 1.
    pub cpu: f64,
    pub cpuinfo: PveCpuInfo,
    /// The load averages, as Proxmox gives them (strings).
    pub loadavg: Vec<String>,
    pub memory: PveMemory,
    pub uptime: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PveCpuInfo {
    pub cpus: u32,
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PveMemory {
    pub total: u64,
    pub used: u64,
    pub available: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PveGuest {
    pub vmid: u32,
    /// `qemu` or `lxc`.
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub node: String,
    pub status: String,
    /// CPU in use, 0 to `maxcpu`.
    pub cpu: f64,
    pub maxcpu: f64,
    pub mem: u64,
    pub maxmem: u64,
    pub uptime: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pool {
    pub name: String,
    pub size: u64,
    pub alloc: u64,
    pub free: u64,
    pub frag_pct: u32,
    pub cap_pct: u32,
    /// `ONLINE`, `DEGRADED`, …
    pub health: String,
    /// `zpool status -x <pool>`'s line, its last scan and its errors line.
    pub status_x: String,
    pub scan: String,
    pub errors: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Dataset {
    pub name: String,
    pub used: u64,
    pub available: u64,
    /// 0: none.
    pub quota: u64,
    pub logicalused: u64,
    pub compressratio: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(doc: &str) -> Completion {
        Completion {
            doc_id: doc.into(),
            language: "deu".into(),
            engine: "tesseract/5.5.0 profile=frak2021 image=sha256:0123456789ab models=sha256:0123456789ab route=minimal@r1 contract=0".into(),
            queue: "ocr-bulk".into(),
            reason: "bulk".into(),
            outcome: "done".into(),
            scan: "cache".into(),
            uploaded: true,
            secs: 41.5,
            ..Completion::default()
        }
    }

    #[test]
    fn a_status_round_trips_and_reads_with_fields_missing_or_added() {
        let mut s = WorkerStatus {
            schema: SCHEMA,
            node_id: "usnm-ocr-node1-arcs-prod".into(),
            concurrency: 4,
            ..WorkerStatus::default()
        };
        s.recent.push(done("sn84027718_1890-01-02_ed-1_seq-3"));
        s.routes.insert("deu".into(), "frak2021".into());
        let back: WorkerStatus = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        let old: WorkerStatus =
            serde_json::from_str(r#"{"node_id":"n1","later_field":[1,2]}"#).unwrap();
        assert_eq!(old.node_id, "n1");
        assert!(old.in_flight.is_empty());
    }

    #[test]
    fn a_journal_line_must_be_a_page_and_tags() {
        let c = done("sn84027718_1890-01-02_ed-1_seq-3");
        c.check().unwrap();
        let mut bad = c.clone();
        bad.doc_id = "Ein langer Satz aus der Seite".into();
        assert!(bad.check().is_err());
        let mut bad = c.clone();
        bad.engine = "x".repeat(500);
        assert!(bad.check().is_err());
        let mut bad = c.clone();
        bad.engine = "Der Text\nder Seite".into();
        assert!(bad.check().is_err());
        let mut bad = c.clone();
        bad.error_kind = Some("Die Zeitung schrieb: \"…\"".into());
        assert!(bad.check().is_err());
        let mut bad = c.clone();
        bad.outcome = "maybe".into();
        assert!(bad.check().is_err());
        let mut bad = c;
        bad.secs = f64::NAN;
        assert!(bad.check().is_err());
    }

    #[test]
    fn outcomes_count_by_outcome_and_scan_source() {
        let mut o = Outcomes::default();
        let mut c = done("sn84027718_1890-01-02_ed-1_seq-3");
        o.add(&c);
        c.outcome = "failed".into();
        c.scan = "loc".into();
        o.add(&c);
        assert_eq!(
            o,
            Outcomes {
                done: 1,
                failed: 1,
                from_cache: 1,
                from_loc: 1,
                ..Outcomes::default()
            }
        );
    }

    #[test]
    fn reads_proxmox_and_zfs_output() {
        let j = r#"{"schema":1,"host":"pve-big","collected_at":"2026-10-11T23:20:00Z",
          "node":{"cpu":0.12,"cpuinfo":{"cpus":8,"model":"i7","flags":"fpu"},"loadavg":["0.46","0.38","0.46"],
                  "memory":{"available":9290186752,"free":1,"total":50394210304,"used":41104023552},"uptime":81439},
          "resources":[{"cpu":0.0017,"id":"qemu/200","maxcpu":8,"maxmem":42949672960,"mem":30764625920,
                        "name":"usnm-ocr-node2-arcs-prod","node":"pve-big","status":"running","type":"qemu","uptime":80540,"vmid":200}],
          "pools":[{"name":"tank","size":4982162063360,"alloc":8345436160,"free":4973816627200,"frag_pct":0,"cap_pct":0,
                    "health":"ONLINE","status_x":"pool 'tank' is healthy","scan":"scrub repaired 0B","errors":"No known data errors"}],
          "datasets":[{"name":"tank/ocr","used":3380035584,"available":3295154847744,"quota":3298534883328,"logicalused":3726113280,"compressratio":1.10}]}"#;
        let h: HostStats = serde_json::from_str(j).unwrap();
        assert_eq!(h.node.cpuinfo.cpus, 8);
        assert_eq!(h.resources[0].kind, "qemu");
        assert_eq!(h.pools[0].health, "ONLINE");
        assert!((h.datasets[0].compressratio - 1.10).abs() < 1e-9);
    }

    #[test]
    fn a_status_from_the_network_keeps_only_pages_and_tags() {
        let mut s = WorkerStatus {
            node_id: "usnm-ocr-node1".into(),
            version: "0.1.0 (commit 0123abc)".into(),
            engine: "Die ganze Seite\nmit Text".into(),
            models: vec!["frak2021".into(), "x".repeat(300)],
            scan_cache_url: Some("http://scans.example:8380".into()),
            ..WorkerStatus::default()
        };
        s.in_flight.push(InFlight {
            doc_id: "sn84027718_1890-01-02_ed-1_seq-3".into(),
            language: "deu".into(),
            queue: "ocr-bulk".into(),
            stage: "ocr".into(),
            ..InFlight::default()
        });
        s.in_flight.push(InFlight {
            doc_id: "Ein Satz aus der Zeitung".into(),
            ..InFlight::default()
        });
        s.recent.push(done("sn84027718_1890-01-02_ed-1_seq-3"));
        s.recent.push(done("kein doc_id"));
        s.sanitize();
        assert_eq!(s.node_id, "usnm-ocr-node1");
        assert_eq!(s.engine, "?");
        assert_eq!(s.models, ["frak2021"]);
        assert!(s.scan_cache_url.is_some());
        assert_eq!(s.in_flight.len(), 1);
        assert_eq!(s.recent.len(), 1);
    }
}
