//! The scan fetch (#368), from the files the fetcher writes into
//! `tank/ocr/scans` (read-only here): `status-full.json`, `index-full.jsonl`
//! and `failures-full.jsonl`, and the manifest it fetches from.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::Context;
use bytes::Bytes;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use usnm_ocr_contract::prefetch::decode_where;
use usnm_ocr_worker::prefetch::{self, priority, set_of, Order, Set, Sets};

use crate::tail::{Got, Tail};

/// The fetch groups, by [`prefetch::priority`]'s number. An issue goes in
/// its best page's group, as the fetcher orders it.
pub const GROUPS: [&str; 6] = [
    "German before the 1920s",
    "Worst English batches",
    "Czech",
    "Serbian",
    "Russian",
    "The rest of the keep set",
];

/// The fetcher's `status-<tag>.json`, read leniently, so a fetcher a
/// version apart still shows. `loc_holds` is a counter a later fetcher may
/// add; without it, holds are counted from the status as it is seen.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FetchStatus {
    pub state: String,
    pub version: String,
    pub started_at: String,
    pub updated_at: String,
    pub manifest: String,
    pub manifest_sha256: String,
    pub resolution: String,
    pub pages: u64,
    pub finished: u64,
    pub already_there: u64,
    pub fetched: u64,
    pub failed_permanent: u64,
    pub failed_transient: u64,
    /// Pages skipped while their issue's item cools off (#414).
    pub failed_item_error: u64,
    pub issues_cooling: u64,
    pub issues_item_permanent: u64,
    pub bytes_fetched: u64,
    pub bytes_on_disk: u64,
    pub max_total_bytes: Option<u64>,
    pub buffer_bytes: u64,
    pub buffer_cap: u64,
    pub keep_pages: u64,
    pub buffer_pages: u64,
    pub elapsed_secs: u64,
    pub fetched_per_hour: f64,
    pub eta_secs: Option<u64>,
    pub loc_hold_secs: Option<u64>,
    pub loc_holds: Option<u64>,
    pub last_doc_id: String,
    pub error: Option<String>,
}

/// The fields of an index line used here.
#[derive(Debug, Deserialize)]
struct IndexLine {
    doc_id: String,
    fetched_at: DateTime<Utc>,
    bytes: u64,
}

/// A failure line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureLine {
    pub doc_id: String,
    pub kind: String,
    pub error: String,
    pub at: DateTime<Utc>,
}

/// A keep-set page, in fetch order.
#[derive(Debug, Clone)]
pub struct PlanRow {
    pub doc_id: Box<str>,
    /// ISO 639-3, or `und`.
    pub lang: Box<str>,
    pub group: u8,
}

/// The manifest's keep set in the fetcher's order, and the size of its
/// buffer set.
#[derive(Debug, Default)]
pub struct Plan {
    pub path: PathBuf,
    pub sha256: String,
    pub keep: Vec<PlanRow>,
    pub pos: HashMap<Box<str>, u32>,
    pub buffer_pages: u64,
    /// The buffer set's pages, as hashes of their doc_ids (a buffer set
    /// can run to millions of pages).
    pub buffer: HashSet<u64>,
    pub group_pages: [u64; 6],
}

/// A doc_id's hash, for [`Plan::buffer`].
pub fn doc_hash(doc_id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    doc_id.hash(&mut h);
    h.finish()
}

fn iso(lang: &str) -> &str {
    usnm_core::lang::marc_to_iso3(lang).unwrap_or(lang)
}

impl Plan {
    /// Read a manifest and order its keep set as `prefetch --sets keep
    /// --order priority` does.
    pub fn load(path: &Path) -> anyhow::Result<Plan> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let sha256 = prefetch::sha256(&bytes);
        let mut buffer = HashSet::new();
        let rows = decode_where(Bytes::from(bytes), |r| {
            let keep = set_of(r) == Set::Keep;
            if !keep {
                buffer.insert(doc_hash(&r.doc_id));
            }
            keep
        })?;
        let buffer_pages = buffer.len() as u64;
        let rows = prefetch::select(
            rows,
            &prefetch::Options {
                sets: Sets::Keep,
                order: Order::Priority,
                ..prefetch::Options::default()
            },
        );
        let mut best: HashMap<(&str, NaiveDate, u16), u8> = HashMap::new();
        for r in &rows {
            let e = best.entry((&r.lccn, r.date, r.edition)).or_insert(u8::MAX);
            *e = (*e).min(priority(r));
        }
        let mut plan = Plan {
            path: path.to_owned(),
            sha256,
            buffer_pages,
            buffer,
            ..Plan::default()
        };
        for r in &rows {
            let group = best[&(r.lccn.as_str(), r.date, r.edition)].min(5);
            plan.group_pages[usize::from(group)] += 1;
            plan.pos
                .insert(r.doc_id.clone().into(), plan.keep.len() as u32);
            plan.keep.push(PlanRow {
                doc_id: r.doc_id.clone().into(),
                lang: iso(r.lang_or_und()).into(),
                group,
            });
        }
        Ok(plan)
    }
}

/// Pages and bytes.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Tally {
    pub pages: u64,
    pub bytes: u64,
}

/// What the fetcher has done, built up from its files.
#[derive(Debug)]
pub struct Fetch {
    pub scans: PathBuf,
    /// The manifest to use when the status doesn't name one.
    pub manifest: Option<PathBuf>,
    pub plan: Option<Plan>,
    pub plan_error: Option<String>,
    pub status: Option<FetchStatus>,
    pub status_error: Option<String>,
    index: Tail,
    failures_tail: Tail,
    /// The pages fetched, and when each was last.
    pub fetched: HashMap<Box<str>, DateTime<Utc>>,
    pub groups: [Tally; 6],
    pub buffer: Tally,
    /// Fetches (time, bytes) over the last day.
    events: VecDeque<(DateTime<Utc>, u64)>,
    /// The latest failures, newest last.
    pub failures: VecDeque<FailureLine>,
    pub dead: HashSet<String>,
    /// Pages skipped because loc.gov couldn't serve their issue's item
    /// (`loc_item_error`), and when they last were; until permanent.
    pub cooling: HashMap<String, DateTime<Utc>>,
    failure_times: VecDeque<DateTime<Utc>>,
    /// LoC holds seen over the last day.
    holds: VecDeque<DateTime<Utc>>,
    last_hold_until: Option<DateTime<Utc>>,
    last_hold_count: Option<u64>,
    /// The first keep page not yet fetched.
    cursor: usize,
    /// The manifest last read, or tried: (path, sha256), when.
    attempt: Option<((PathBuf, String), DateTime<Utc>)>,
}

const KEEP_FAILURES: usize = 50;

impl Fetch {
    pub fn new(scans: PathBuf, manifest: Option<PathBuf>) -> Self {
        Fetch {
            index: Tail::new(scans.join("index-full.jsonl")),
            failures_tail: Tail::new(scans.join("failures-full.jsonl")),
            scans,
            manifest,
            plan: None,
            plan_error: None,
            status: None,
            status_error: None,
            fetched: HashMap::new(),
            groups: [Tally::default(); 6],
            buffer: Tally::default(),
            events: VecDeque::new(),
            failures: VecDeque::new(),
            dead: HashSet::new(),
            cooling: HashMap::new(),
            failure_times: VecDeque::new(),
            holds: VecDeque::new(),
            last_hold_until: None,
            last_hold_count: None,
            cursor: 0,
            attempt: None,
        }
    }

    /// Forget what the index said, to read it again from the start.
    fn reset_index(&mut self) {
        self.index.offset = 0;
        self.fetched.clear();
        self.groups = [Tally::default(); 6];
        self.buffer = Tally::default();
        self.events.clear();
        self.cursor = 0;
    }

    /// Read what changed. Blocking (file reads); run it off the async
    /// threads.
    pub fn poll(&mut self, now: DateTime<Utc>) {
        self.read_status(now);
        self.load_plan(now);
        if let Err(e) = self.read_index() {
            tracing::warn!(error = %e, "can't read the fetch index");
        }
        if let Err(e) = self.read_failures() {
            tracing::warn!(error = %e, "can't read the fetch failures");
        }
        let day = now - Duration::days(1);
        while self.events.front().is_some_and(|(t, _)| *t < day) {
            self.events.pop_front();
        }
        while self.failure_times.front().is_some_and(|t| *t < day) {
            self.failure_times.pop_front();
        }
        while self.holds.front().is_some_and(|t| *t < day) {
            self.holds.pop_front();
        }
    }

    fn read_status(&mut self, now: DateTime<Utc>) {
        let path = self.scans.join("status-full.json");
        match std::fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<FetchStatus>(&b)?))
        {
            Ok(s) => {
                self.count_holds(&s, now);
                self.status = Some(s);
                self.status_error = None;
            }
            Err(e) => self.status_error = Some(format!("{}: {e:#}", path.display())),
        }
    }

    /// A hold is counted from the fetcher's counter when it has one, else
    /// each time the status shows a new end to a hold.
    fn count_holds(&mut self, s: &FetchStatus, now: DateTime<Utc>) {
        if let Some(n) = s.loc_holds {
            if let Some(prev) = self.last_hold_count {
                for _ in 0..n.saturating_sub(prev).min(1000) {
                    self.holds.push_back(now);
                }
            }
            self.last_hold_count = Some(n);
            return;
        }
        let Some(secs) = s.loc_hold_secs else {
            return;
        };
        let Ok(at) = DateTime::parse_from_rfc3339(&s.updated_at) else {
            return;
        };
        let until = at.with_timezone(&Utc) + Duration::seconds(secs as i64);
        let new = self
            .last_hold_until
            .is_none_or(|prev| until > prev + Duration::seconds(5));
        if new {
            self.holds.push_back(at.with_timezone(&Utc));
            self.last_hold_until = Some(until);
        }
    }

    fn load_plan(&mut self, now: DateTime<Utc>) {
        let want = self
            .status
            .as_ref()
            .map(|s| s.manifest.as_str())
            .filter(|m| !m.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.manifest.clone());
        let Some(want) = want else {
            return;
        };
        let sha = self
            .status
            .as_ref()
            .map(|s| s.manifest_sha256.clone())
            .unwrap_or_default();
        if self
            .plan
            .as_ref()
            .is_some_and(|p| p.path == want && (sha.is_empty() || sha == p.sha256))
        {
            return;
        }
        // A manifest that can't be read is tried again when the status
        // names another, or after ten minutes.
        let key = (want.clone(), sha);
        if self
            .attempt
            .as_ref()
            .is_some_and(|(k, t)| *k == key && now - *t < Duration::minutes(10))
        {
            return;
        }
        self.attempt = Some((key.clone(), now));
        let loaded = Plan::load(&want).and_then(|p| {
            // The file must be the one the fetcher read; one swapped in
            // under the same path would mix two plans.
            anyhow::ensure!(
                key.1.is_empty() || p.sha256 == key.1,
                "{}'s sha256 is {}, not the {} the fetcher's status names",
                want.display(),
                p.sha256,
                key.1
            );
            Ok(p)
        });
        match loaded {
            Ok(p) => {
                tracing::info!(manifest = %want.display(), keep = p.keep.len(), buffer = p.buffer_pages, "manifest read");
                self.plan = Some(p);
                self.plan_error = None;
                self.reset_index();
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "can't read the manifest");
                self.plan_error = Some(format!("{e:#}"));
                // The plan held is no longer the fetcher's: drop it rather
                // than mix it with the new status and index.
                if self.plan.take().is_some() {
                    self.reset_index();
                }
            }
        }
    }

    fn read_index(&mut self) -> anyhow::Result<()> {
        loop {
            let lines = match self.index.read()? {
                Got::Reset => {
                    self.reset_index();
                    continue;
                }
                Got::Lines(l) => l,
            };
            if lines.is_empty() {
                return Ok(());
            }
            for l in lines {
                let Ok(line) = serde_json::from_str::<IndexLine>(&l) else {
                    continue;
                };
                self.add_fetch(line);
            }
        }
    }

    fn add_fetch(&mut self, line: IndexLine) {
        self.events.push_back((line.fetched_at, line.bytes));
        // A page of neither set (an older manifest's, or before the
        // manifest is read) counts in the rates only.
        let set = self
            .plan
            .as_ref()
            .and_then(|p| match p.pos.get(line.doc_id.as_str()) {
                Some(i) => Some(Some(p.keep[*i as usize].group)),
                None => p.buffer.contains(&doc_hash(&line.doc_id)).then_some(None),
            });
        let Some(group) = set else {
            return;
        };
        if let Some(old) = self.fetched.get_mut(line.doc_id.as_str()) {
            *old = (*old).max(line.fetched_at);
            return;
        }
        self.fetched.insert(line.doc_id.into(), line.fetched_at);
        let t = match group {
            Some(g) => &mut self.groups[usize::from(g)],
            None => &mut self.buffer,
        };
        t.pages += 1;
        t.bytes += line.bytes;
    }

    fn read_failures(&mut self) -> anyhow::Result<()> {
        loop {
            let lines = match self.failures_tail.read()? {
                Got::Reset => {
                    self.failures.clear();
                    self.dead.clear();
                    self.cooling.clear();
                    self.failure_times.clear();
                    continue;
                }
                Got::Lines(l) => l,
            };
            if lines.is_empty() {
                return Ok(());
            }
            for l in lines {
                let Ok(f) = serde_json::from_str::<FailureLine>(&l) else {
                    continue;
                };
                match f.kind.as_str() {
                    "permanent" => {
                        self.cooling.remove(&f.doc_id);
                        self.dead.insert(f.doc_id.clone());
                    }
                    "loc_item_error" => {
                        let t = self.cooling.entry(f.doc_id.clone()).or_insert(f.at);
                        *t = (*t).max(f.at);
                    }
                    _ => {}
                }
                self.failure_times.push_back(f.at);
                self.failures.push_back(f);
                if self.failures.len() > KEEP_FAILURES {
                    self.failures.pop_front();
                }
            }
        }
    }

    /// Fetches and bytes since `since`.
    pub fn since(&self, since: DateTime<Utc>) -> Tally {
        self.events
            .iter()
            .rev()
            .take_while(|(t, _)| *t >= since)
            .fold(Tally::default(), |a, (_, b)| Tally {
                pages: a.pages + 1,
                bytes: a.bytes + b,
            })
    }

    pub fn holds_since(&self, since: DateTime<Utc>) -> usize {
        self.holds.iter().rev().take_while(|t| **t >= since).count()
    }

    /// Pages skipped for an item error and not fetched since.
    pub fn cooling_pages(&self) -> usize {
        self.cooling
            .iter()
            .filter(|(d, at)| self.fetched.get(d.as_str()).is_none_or(|f| f < *at))
            .count()
    }

    pub fn failures_since(&self, since: DateTime<Utc>) -> usize {
        self.failure_times.iter().filter(|t| **t >= since).count()
    }

    /// The next `n` keep pages not yet fetched, in fetch order.
    pub fn next_to_fetch(&mut self, n: usize) -> Vec<&PlanRow> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        while self.cursor < plan.keep.len()
            && self.fetched.contains_key(&plan.keep[self.cursor].doc_id)
        {
            self.cursor += 1;
        }
        plan.keep[self.cursor..]
            .iter()
            .filter(|r| !self.fetched.contains_key(&r.doc_id))
            .take(n)
            .collect()
    }

    /// The group the fetcher is in: its last page's.
    pub fn current_group(&self) -> Option<u8> {
        let s = self.status.as_ref()?;
        let p = self.plan.as_ref()?;
        p.pos
            .get(s.last_doc_id.as_str())
            .map(|i| p.keep[*i as usize].group)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usnm_core::ids::PageKey;
    use usnm_ocr_contract::prefetch::{encode, ManifestRow};

    fn row(doc: &str, batch: &str, lang: &str) -> ManifestRow {
        let key = PageKey::from_doc_id(doc).unwrap();
        ManifestRow::new(&key, batch, 1, Some(lang.into()), "detected", 0.5)
    }

    #[test]
    fn the_status_the_fetcher_writes_reads_here() {
        let s = prefetch::Status {
            state: "running".into(),
            pages: 10,
            loc_hold_secs: Some(30),
            ..prefetch::Status::default()
        };
        let back: FetchStatus = serde_json::from_slice(&serde_json::to_vec(&s).unwrap()).unwrap();
        assert_eq!(back.state, "running");
        assert_eq!(back.pages, 10);
        assert_eq!(back.loc_hold_secs, Some(30));
        assert_eq!(back.loc_holds, None);
    }

    #[test]
    fn follows_the_fetcher_through_its_files() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.parquet");
        let rows = vec![
            row("sn1_1890-01-01_ed-1_seq-1", "az_a", "eng"),
            row("sn2_1925-01-01_ed-1_seq-1", "az_a", "ger"),
            row("sn3_1890-01-01_ed-1_seq-1", "az_a", "cze"),
            row("sn4_1890-01-01_ed-1_seq-1", "az_a", "ger"),
            row("sn4_1890-01-01_ed-1_seq-2", "az_a", "ger"),
        ];
        std::fs::write(&manifest, encode(&rows).unwrap()).unwrap();
        let mut f = Fetch::new(dir.path().to_owned(), Some(manifest));
        let now = Utc::now();
        f.poll(now);
        let plan = f.plan.as_ref().unwrap();
        assert_eq!(plan.buffer_pages, 1);
        let order: Vec<&str> = plan.keep.iter().map(|r| &*r.doc_id).collect();
        assert_eq!(
            order,
            [
                "sn4_1890-01-01_ed-1_seq-1",
                "sn4_1890-01-01_ed-1_seq-2",
                "sn3_1890-01-01_ed-1_seq-1",
                "sn2_1925-01-01_ed-1_seq-1"
            ]
        );
        assert_eq!(plan.group_pages, [2, 0, 1, 0, 0, 1]);
        assert_eq!(&*plan.keep[0].lang, "deu");

        let at = (now - Duration::minutes(5)).to_rfc3339();
        let old = (now - Duration::hours(3)).to_rfc3339();
        let later = (now - Duration::minutes(1)).to_rfc3339();
        std::fs::write(
            dir.path().join("index-full.jsonl"),
            format!(
                "{{\"doc_id\":\"sn4_1890-01-01_ed-1_seq-1\",\"fetched_at\":\"{old}\",\"bytes\":100}}\n\
                 {{\"doc_id\":\"sn1_1890-01-01_ed-1_seq-1\",\"fetched_at\":\"{at}\",\"bytes\":7}}\n\
                 {{\"doc_id\":\"sn4_1890-01-01_ed-1_seq-1\",\"fetched_at\":\"{at}\",\"bytes\":100}}\n\
                 {{\"doc_id\":\"sn9_1890-01-01_ed-1_seq-1\",\"fetched_at\":\"{at}\",\"bytes\":5}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("failures-full.jsonl"),
            format!("{{\"doc_id\":\"sn3_1890-01-01_ed-1_seq-1\",\"kind\":\"permanent\",\"error\":\"HTTP 404\",\"at\":\"{at}\"}}\n\
                     {{\"doc_id\":\"sn4_1890-01-01_ed-1_seq-1\",\"kind\":\"loc_item_error\",\"error\":\"HTTP 503\",\"at\":\"{old}\"}}\n\
                     {{\"doc_id\":\"sn1_1890-01-01_ed-1_seq-1\",\"kind\":\"loc_item_error\",\"error\":\"HTTP 503\",\"at\":\"{later}\"}}\n\
                     {{\"doc_id\":\"sn5_1890-01-01_ed-1_seq-1\",\"kind\":\"loc_item_error\",\"error\":\"HTTP 503\",\"at\":\"{at}\"}}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("status-full.json"),
            format!("{{\"state\":\"running\",\"updated_at\":\"{at}\",\"loc_hold_secs\":60,\"last_doc_id\":\"sn4_1890-01-01_ed-1_seq-1\"}}"),
        )
        .unwrap();
        f.poll(now);
        assert_eq!(f.groups[0].pages, 1);
        assert_eq!(f.groups[0].bytes, 100);
        assert_eq!(f.buffer.pages, 1);
        // An older manifest's page is in the rates, not in either set.
        assert_eq!(f.buffer.bytes, 7);
        assert_eq!(f.since(now - Duration::hours(1)).pages, 3);
        assert_eq!(f.since(now - Duration::days(1)).bytes, 212);
        assert_eq!(f.current_group(), Some(0));
        assert!(f.dead.contains("sn3_1890-01-01_ed-1_seq-1"));
        // sn4's page was skipped for an item error and fetched since; sn1's
        // was fetched before its item error, so it counts, as does sn5's.
        assert_eq!(f.cooling_pages(), 2);
        assert_eq!(f.holds_since(now - Duration::hours(1)), 1);
        // The same hold seen again isn't counted twice.
        f.poll(now);
        assert_eq!(f.holds_since(now - Duration::hours(1)), 1);
        let next: Vec<&str> = f.next_to_fetch(2).iter().map(|r| &*r.doc_id).collect();
        assert_eq!(
            next,
            ["sn4_1890-01-01_ed-1_seq-2", "sn3_1890-01-01_ed-1_seq-1"]
        );
    }

    #[test]
    fn refuses_a_manifest_that_isnt_the_one_the_fetcher_read() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join("manifest.parquet");
        let rows = vec![row("sn4_1890-01-01_ed-1_seq-1", "az_a", "ger")];
        std::fs::write(&manifest, encode(&rows).unwrap()).unwrap();
        std::fs::write(
            dir.path().join("status-full.json"),
            format!(
                "{{\"state\":\"running\",\"manifest\":\"{}\",\"manifest_sha256\":\"{}\"}}",
                manifest.display(),
                "0".repeat(64)
            ),
        )
        .unwrap();
        let mut f = Fetch::new(dir.path().to_owned(), None);
        f.poll(Utc::now());
        assert!(f.plan.is_none());
        assert!(f.plan_error.as_deref().unwrap().contains("sha256"));
    }
}
