//! `activity`: what the pipeline is doing at this moment, for the status
//! page's "Right now" line (06 §6.3.6).
//!
//! The ingest job execution reports its step in `ops/activity` (heartbeat
//! every minute). Where there is no live report (an execution started
//! before the job reported, or the backfill job's workers, which don't),
//! the step is inferred from what the pipeline leaves behind: the writer
//! lock and `ops/release-progress` (indexing, then merging once every page
//! is sent), live batch leases (downloading), and the title-record cache
//! `raw/titles.json`, which titles-sync saves every 100 titles and before
//! each pause (looking up newspaper details).

use std::collections::HashSet;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use usnm_state::state::{MergeProgress, Outcome, RunStatus, Step};
use usnm_state::summary::{RunSummary, Summary};

use super::assemble::{Backfill, CatalogTitles};
use super::sanitize::{sanitize, short_id};

/// A report older than this means the execution stopped without saying
/// (the job writes one a minute).
pub const STALE_AFTER: Duration = Duration::seconds(usnm_state::state::ACTIVITY_STALE_SECS);

/// Without a report, a title cache saved this recently means titles-sync is
/// running: it saves every 100 titles (about 8 minutes) and before each
/// 65-minute pause for LoC.
pub const TITLES_CACHE_LIVE: Duration = Duration::minutes(80);

/// The title records titles-sync has cached (`raw/titles.json`).
#[derive(Debug, Clone, Default)]
pub struct TitlesCache {
    pub lccns: HashSet<String>,
    pub modified: Option<DateTime<Utc>>,
}

/// What is happening now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Now {
    /// Reading LoC's batch list.
    Listing,
    /// Downloading and processing batches.
    Downloading,
    /// Looking up newspaper details on loc.gov (titles-sync).
    Titles,
    /// Building the search index (sending pages).
    Indexing,
    /// Merging the new index's pieces.
    Merging,
    /// Writing the snapshot and switching the site to the new version.
    Publishing,
    /// Nothing is running.
    Idle,
}

impl From<Step> for Now {
    fn from(s: Step) -> Self {
        match s {
            Step::Listing => Now::Listing,
            Step::Downloading => Now::Downloading,
            Step::Titles => Now::Titles,
            Step::Indexing => Now::Indexing,
            Step::Merging => Now::Merging,
            Step::Publishing => Now::Publishing,
        }
    }
}

/// How the last execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LastOutcome {
    Published,
    NothingNew,
    /// titles-sync ran out of time (LoC rate limits it); nothing was
    /// published and the next execution continues.
    TitlesLeft,
    Failed,
    /// It stopped reporting without finishing (killed, or out of memory).
    Stopped,
}

impl From<Outcome> for LastOutcome {
    fn from(o: Outcome) -> Self {
        match o {
            Outcome::Published => LastOutcome::Published,
            Outcome::NothingNew => LastOutcome::NothingNew,
            Outcome::TitlesLeft => LastOutcome::TitlesLeft,
            Outcome::Failed => LastOutcome::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LastRun {
    pub outcome: LastOutcome,
    /// When it ended. For `stopped`, its last report. For a failed index run
    /// without a report: its recorded `failed_at`, else its last progress
    /// write, else when the run started (runs that failed before `failed_at`
    /// was recorded).
    pub ended_at: DateTime<Utc>,
    /// The step it was on when it ended, where known.
    pub step: Option<Now>,
    /// Sanitized.
    pub error: Option<String>,
    pub index_version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Merge {
    /// `settle` (merging while open) or `finalize` (closed, last merges).
    pub step: String,
    pub splits: u64,
    pub merges_running: u64,
    pub merges_queued: u64,
}

impl From<&MergeProgress> for Merge {
    fn from(m: &MergeProgress) -> Self {
        Self {
            step: m.step.clone(),
            splits: m.splits,
            merges_running: m.merges_running,
            merges_queued: m.merges_queued,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub now: Now,
    /// `job`: the ingest execution reports it; `inferred`: worked out from
    /// locks, leases, progress and the title cache; `none` when idle.
    pub source: &'static str,
    /// When the current step started, where known.
    pub since: Option<DateTime<Utc>>,
    /// The reporting execution: when it started, and a short opaque id.
    pub run_started_at: Option<DateTime<Utc>>,
    pub run: Option<String>,
    /// The latest evidence for `now` (a report, a progress write, a cache save).
    pub reported_at: Option<DateTime<Utc>>,
    /// The step's progress: titles looked up, batches processed, pages sent.
    pub done: Option<u64>,
    pub total: Option<u64>,
    pub percent: Option<f64>,
    /// When the step should finish at its rate so far.
    pub eta: Option<DateTime<Utc>>,
    /// loc.gov rate limited titles-sync, which sends nothing until then.
    pub paused_until: Option<DateTime<Utc>>,
    pub index_version: Option<String>,
    pub merge: Option<Merge>,
    /// How the last execution ended, when known.
    pub last: Option<LastRun>,
    /// The ingest job's next scheduled start; `None` when it isn't scheduled.
    pub next_run: Option<DateTime<Utc>>,
}

fn percent(done: u64, total: u64) -> Option<f64> {
    (total > 0).then(|| (1000.0 * done as f64 / total as f64).round() / 10.0)
}

/// At the rate from `since` to `at`, when `total` is reached.
fn eta(since: DateTime<Utc>, at: DateTime<Utc>, done: u64, total: u64) -> Option<DateTime<Utc>> {
    let secs = (at - since).num_seconds();
    if done == 0 || done >= total || secs < 60 {
        return None;
    }
    let left = (total - done) as f64 * secs as f64 / done as f64;
    Some(at + Duration::seconds(left as i64))
}

/// The newest index run, if it is still building.
fn building(s: &Summary) -> Option<&RunSummary> {
    s.runs
        .iter()
        .max_by_key(|r| r.started_at)
        .filter(|r| r.status == RunStatus::Building)
}

/// Titles in curated batches the catalog lacks, and how many of those the
/// cache already has.
fn titles_counts(
    s: &Summary,
    catalog: Option<&CatalogTitles>,
    cache: Option<&TitlesCache>,
) -> Option<(u64, u64)> {
    let (catalog, cache) = (catalog?, cache?);
    let missing: HashSet<&str> = s
        .batches
        .iter()
        .filter(|b| b.curated_version.is_some())
        .flat_map(|b| b.curated_lccns.iter().map(String::as_str))
        .filter(|l| !catalog.lccns.contains(*l))
        .collect();
    let cached = missing.iter().filter(|l| cache.lccns.contains(**l)).count();
    Some((cached as u64, missing.len() as u64))
}

/// How a reported execution that isn't running ended: its recorded
/// outcome, or `stopped` at its last report if it stopped without one.
fn ended(j: &usnm_state::state::Activity) -> LastRun {
    match j.ended_at {
        Some(end) => LastRun {
            outcome: j.outcome.map_or(LastOutcome::Failed, LastOutcome::from),
            ended_at: end,
            step: Some(j.step.into()),
            error: j.error.as_deref().map(sanitize),
            index_version: j.index_version.clone(),
        },
        None => LastRun {
            outcome: LastOutcome::Stopped,
            ended_at: j.updated_at,
            step: Some(j.step.into()),
            error: None,
            index_version: j.index_version.clone(),
        },
    }
}

pub struct Inputs<'a> {
    pub at: DateTime<Utc>,
    pub summary: &'a Summary,
    pub backfill: &'a Backfill,
    pub catalog: Option<&'a CatalogTitles>,
    pub cache: Option<&'a TitlesCache>,
    pub next_run: Option<DateTime<Utc>>,
}

pub fn activity(i: &Inputs) -> Activity {
    let (at, s) = (i.at, i.summary);
    let mut a = Activity {
        now: Now::Idle,
        source: "none",
        since: None,
        run_started_at: None,
        run: None,
        reported_at: None,
        done: None,
        total: None,
        percent: None,
        eta: None,
        paused_until: None,
        index_version: None,
        merge: None,
        last: None,
        next_run: i.next_run,
    };
    let writer_held = s
        .ops
        .writer
        .as_ref()
        .is_some_and(|w| !w.owner.is_empty() && w.until > at);
    let running = building(s).filter(|_| writer_held);
    // Pages sent for the run being built, if it reports them.
    let progress = s
        .ops
        .release_progress
        .as_ref()
        .filter(|p| running.is_some_and(|r| r.index_version == p.index_version));

    let job = s.ops.activity.as_ref();
    let live = job.filter(|j| j.ended_at.is_none() && at - j.updated_at <= STALE_AFTER);
    // How the reported execution ended; while it still runs, how the one
    // before it ended (kept in the item as `previous`).
    a.last = match (job, live) {
        (Some(j), None) => Some(ended(j)),
        (Some(j), Some(_)) => j.previous.as_deref().map(ended),
        (None, _) => None,
    };

    if let Some(j) = live {
        a.now = j.step.into();
        a.source = "job";
        a.since = Some(j.step_started_at);
        a.run_started_at = Some(j.started_at);
        a.run = Some(short_id(&j.owner));
        a.reported_at = Some(j.updated_at);
        a.index_version = j.index_version.clone();
        a.merge = j.merge.as_ref().map(Merge::from);
        a.paused_until = j.paused_until.filter(|u| *u > at);
        match j.step {
            Step::Indexing => {
                if let Some(p) = progress {
                    a.done = Some(p.docs_sent);
                    a.total = Some(p.docs_expected);
                    a.reported_at = Some(p.updated_at.max(j.updated_at));
                }
            }
            Step::Downloading => {
                a.done = Some(i.backfill.by_status.curated);
                a.total = Some(i.backfill.total);
            }
            _ => {
                a.done = j.done;
                a.total = j.total;
            }
        }
    } else if let Some(run) = running {
        a.source = "inferred";
        a.index_version = Some(run.index_version.clone());
        a.since = Some(run.started_at);
        match progress {
            Some(p) if p.docs_expected > 0 && p.docs_sent >= p.docs_expected => {
                a.now = Now::Merging;
                a.since = None;
                a.reported_at = Some(p.updated_at);
            }
            Some(p) => {
                a.now = Now::Indexing;
                a.done = Some(p.docs_sent);
                a.total = Some(p.docs_expected);
                a.reported_at = Some(p.updated_at);
            }
            None => a.now = Now::Indexing,
        }
    } else if i.backfill.in_progress > 0 {
        a.now = Now::Downloading;
        a.source = "inferred";
        a.since = i
            .backfill
            .in_progress_batches
            .iter()
            .filter(|b| !b.lease_expired)
            .map(|b| b.since)
            .min();
        a.done = Some(i.backfill.by_status.curated);
        a.total = Some(i.backfill.total);
    } else if let (Some((done, total)), Some(modified)) = (
        titles_counts(s, i.catalog, i.cache),
        i.cache.and_then(|c| c.modified),
    ) {
        // A save from before the last reported run ended is that run's own.
        let after_last_run = job.is_none_or(|j| modified > j.ended_at.unwrap_or(j.updated_at));
        if done < total && at - modified <= TITLES_CACHE_LIVE && after_last_run {
            a.now = Now::Titles;
            a.source = "inferred";
            a.done = Some(done);
            a.total = Some(total);
            a.reported_at = Some(modified);
        }
    }

    if let (Some(done), Some(total)) = (a.done, a.total) {
        a.percent = percent(done, total);
        if a.now == Now::Indexing || (a.now == Now::Titles && a.paused_until.is_none()) {
            a.eta = a
                .since
                .and_then(|since| eta(since, a.reported_at.unwrap_or(at), done, total));
        }
    }

    // Without a report of how the last execution ended, the newest index
    // run (other than one being built now) says how the last build went.
    if a.last.is_none() {
        let newest = s
            .runs
            .iter()
            .filter(|r| running.is_none_or(|b| b.index_version != r.index_version))
            .max_by_key(|r| r.started_at);
        if let Some(r) = newest {
            a.last = match r.status {
                RunStatus::Failed => Some(LastRun {
                    outcome: LastOutcome::Failed,
                    // When it was marked failed; for runs that failed before
                    // that was recorded, its last progress write, else its start.
                    ended_at: r.failed_at.unwrap_or_else(|| {
                        s.ops
                            .release_progress
                            .as_ref()
                            .filter(|p| p.index_version == r.index_version)
                            .map_or(r.started_at, |p| p.updated_at)
                    }),
                    step: Some(Now::Indexing),
                    error: r.last_error.as_deref().map(sanitize),
                    index_version: Some(r.index_version.clone()),
                }),
                RunStatus::Published => r.published_at.map(|p| LastRun {
                    outcome: LastOutcome::Published,
                    ended_at: p,
                    step: None,
                    error: None,
                    index_version: Some(r.index_version.clone()),
                }),
                // A build left `building` with no writer: it stopped.
                RunStatus::Building => Some(LastRun {
                    outcome: LastOutcome::Stopped,
                    ended_at: r.started_at,
                    step: Some(Now::Indexing),
                    error: None,
                    index_version: Some(r.index_version.clone()),
                }),
            };
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::assemble;
    use serde_json::json;
    use usnm_state::state::{Activity as Job, ReleaseProgress};
    use usnm_state::summary::LockSummary;

    fn at() -> DateTime<Utc> {
        "2026-10-02T19:30:00Z".parse().unwrap()
    }

    fn curated(name: &str, lccns: &[&str]) -> usnm_state::summary::BatchSummary {
        serde_json::from_value(json!({
            "batch": name, "version": 1, "status": "curated", "attempts": 1,
            "updated_at": at() - Duration::days(1),
            "curated_version": 1, "curated_pages": 100, "curated_ok_pages": 100,
            "curated_lccns": lccns,
        }))
        .unwrap()
    }

    fn run(version: &str, status: &str, started: DateTime<Utc>) -> RunSummary {
        serde_json::from_value(json!({
            "index_version": version, "full": true, "status": status,
            "started_at": started, "new_index": "i",
            "published_at": (status == "published").then(|| started + Duration::hours(1)),
            "last_error": (status == "failed").then_some("tcp connect error: Connection refused (os error 111) to 192.0.2.4"),
        }))
        .unwrap()
    }

    fn job(step: Step) -> Job {
        Job {
            command: "run".into(),
            owner: "caj-usnm-ingest-prod-be60y97-xyz-1-0a1b2c3d".into(),
            started_at: at() - Duration::hours(2),
            step,
            step_started_at: at() - Duration::hours(1),
            updated_at: at() - Duration::seconds(30),
            done: None,
            total: None,
            paused_until: None,
            index_version: None,
            merge: None,
            ended_at: None,
            outcome: None,
            error: None,
            previous: None,
        }
    }

    fn base() -> Summary {
        Summary {
            batches: vec![curated("b1", &["sn1", "sn2"]), curated("b2", &["sn3"])],
            runs: vec![run("v1", "published", at() - Duration::days(3))],
            ..Summary::default()
        }
    }

    fn get(s: &Summary, catalog: &[&str], cache: Option<TitlesCache>) -> Activity {
        let b = assemble::backfill(at(), s);
        let catalog = CatalogTitles {
            lccns: catalog.iter().map(|l| l.to_string()).collect(),
            places: 1,
        };
        activity(&Inputs {
            at: at(),
            summary: s,
            backfill: &b,
            catalog: Some(&catalog),
            cache: cache.as_ref(),
            next_run: None,
        })
    }

    #[test]
    fn titles_sync_reported_by_the_job_with_a_pause() {
        let mut s = base();
        let mut j = job(Step::Titles);
        j.done = Some(342);
        j.total = Some(3464);
        j.paused_until = Some(at() + Duration::minutes(45));
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        assert_eq!((a.now, a.source), (Now::Titles, "job"));
        assert_eq!(
            (a.done, a.total, a.percent),
            (Some(342), Some(3464), Some(9.9))
        );
        assert_eq!(a.paused_until, Some(at() + Duration::minutes(45)));
        // No estimate while paused; the run id is shortened.
        assert_eq!(a.eta, None);
        assert_eq!(a.run.as_deref(), Some("1b2c3d"));
        // The last build, from the index runs.
        assert_eq!(a.last.unwrap().outcome, LastOutcome::Published);
    }

    #[test]
    fn titles_sync_running_gives_an_estimate() {
        let mut s = base();
        let mut j = job(Step::Titles);
        j.done = Some(100);
        j.total = Some(300);
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        // 100 in the hour to the last report, so 200 more take 2 hours.
        let reported = at() - Duration::seconds(30);
        assert_eq!(a.eta, Some(reported + Duration::seconds(2 * 3570)));
    }

    #[test]
    fn indexing_uses_the_release_progress() {
        let mut s = base();
        let mut j = job(Step::Indexing);
        j.index_version = Some("v2".into());
        s.ops.activity = Some(j);
        s.runs
            .push(run("v2", "building", at() - Duration::hours(1)));
        s.ops.writer = Some(LockSummary {
            owner: "w".into(),
            until: at() + Duration::hours(1),
        });
        s.ops.release_progress = Some(ReleaseProgress {
            index_version: "v2".into(),
            docs_sent: 4_100_000,
            docs_expected: 7_800_000,
            mb_sent: 1.0,
            updated_at: at() - Duration::seconds(10),
        });
        let a = get(&s, &[], None);
        assert_eq!(a.now, Now::Indexing);
        assert_eq!(
            (a.done, a.total, a.percent),
            (Some(4_100_000), Some(7_800_000), Some(52.6))
        );
        assert!(a.eta.is_some_and(|e| e > at()));
    }

    #[test]
    fn merging_and_publishing_are_reported() {
        let mut s = base();
        let mut j = job(Step::Merging);
        j.merge = Some(MergeProgress {
            step: "finalize".into(),
            splits: 240,
            merges_running: 1,
            merges_queued: 0,
        });
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        assert_eq!(a.now, Now::Merging);
        assert_eq!(a.merge.as_ref().unwrap().splits, 240);
        s.ops.activity = Some(job(Step::Publishing));
        assert_eq!(get(&s, &[], None).now, Now::Publishing);
    }

    #[test]
    fn without_a_report_a_build_is_inferred() {
        let mut s = base();
        s.runs
            .push(run("v2", "building", at() - Duration::hours(3)));
        s.ops.writer = Some(LockSummary {
            owner: "w".into(),
            until: at() + Duration::hours(1),
        });
        s.ops.release_progress = Some(ReleaseProgress {
            index_version: "v2".into(),
            docs_sent: 50,
            docs_expected: 100,
            mb_sent: 1.0,
            updated_at: at(),
        });
        let a = get(&s, &[], None);
        assert_eq!(
            (a.now, a.source, a.percent),
            (Now::Indexing, "inferred", Some(50.0))
        );
        // Every page sent and the lock still held: the merge wait.
        s.ops.release_progress.as_mut().unwrap().docs_sent = 100;
        assert_eq!(get(&s, &[], None).now, Now::Merging);
        // The lock gone: the build stopped.
        s.ops.writer = None;
        let a = get(&s, &[], None);
        assert_eq!(a.now, Now::Idle);
        assert_eq!(a.last.unwrap().outcome, LastOutcome::Stopped);
    }

    #[test]
    fn without_a_report_titles_sync_is_inferred_from_its_cache() {
        let s = base();
        let cache = |mins: i64| TitlesCache {
            lccns: ["sn1".to_owned()].into(),
            modified: Some(at() - Duration::minutes(mins)),
        };
        // sn1, sn2, sn3 missing from the catalog; sn1 cached 5 minutes ago.
        let a = get(&s, &[], Some(cache(5)));
        assert_eq!((a.now, a.done, a.total), (Now::Titles, Some(1), Some(3)));
        // A cache untouched for longer than a pause: nothing is running.
        assert_eq!(get(&s, &[], Some(cache(120))).now, Now::Idle);
        // Every title catalogued: nothing to look up.
        assert_eq!(
            get(&s, &["sn1", "sn2", "sn3"], Some(cache(5))).now,
            Now::Idle
        );
        // Saved by a run that has since reported its end: not a new run.
        let mut s = base();
        let mut j = job(Step::Titles);
        j.ended_at = Some(at() - Duration::minutes(4));
        j.outcome = Some(Outcome::TitlesLeft);
        s.ops.activity = Some(j);
        let a = get(&s, &[], Some(cache(5)));
        assert_eq!(
            (a.now, a.last.unwrap().outcome),
            (Now::Idle, LastOutcome::TitlesLeft)
        );
        // Saved after it ended: a run that doesn't report.
        assert_eq!(get(&s, &[], Some(cache(3))).now, Now::Titles);
    }

    #[test]
    fn downloading_is_inferred_from_live_leases() {
        let mut s = base();
        s.batches.push(
            serde_json::from_value(json!({
                "batch": "b3", "version": 1, "status": "downloading", "attempts": 1,
                "updated_at": at() - Duration::minutes(3),
                "lease": {"owner": "w-1-0a1b2c3d", "until": at() + Duration::hours(1)},
            }))
            .unwrap(),
        );
        let a = get(&s, &["sn1", "sn2", "sn3"], None);
        assert_eq!(
            (a.now, a.done, a.total),
            (Now::Downloading, Some(2), Some(3))
        );
    }

    #[test]
    fn idle_says_how_the_last_execution_ended() {
        let mut s = base();
        // Nothing reported: the newest index run.
        let a = get(&s, &[], None);
        assert_eq!(
            (a.now, a.last.as_ref().unwrap().outcome),
            (Now::Idle, LastOutcome::Published)
        );
        s.runs.push(run("v2", "failed", at() - Duration::hours(5)));
        let last = get(&s, &[], None).last.unwrap();
        assert_eq!(last.outcome, LastOutcome::Failed);
        // No failure time and no progress for it: the start is all there is.
        assert_eq!(last.ended_at, at() - Duration::hours(5));
        // Its last progress write says when it stopped.
        s.ops.release_progress = Some(usnm_state::state::ReleaseProgress {
            index_version: "v2".into(),
            docs_sent: 10,
            docs_expected: 10,
            mb_sent: 1.0,
            updated_at: at() - Duration::hours(2),
        });
        assert_eq!(
            get(&s, &[], None).last.unwrap().ended_at,
            at() - Duration::hours(2)
        );
        // A recorded failure time wins.
        s.runs.last_mut().unwrap().failed_at = Some(at() - Duration::minutes(90));
        let last = get(&s, &[], None).last.unwrap();
        assert_eq!(last.ended_at, at() - Duration::minutes(90));
        s.ops.release_progress = None;
        // Sanitized.
        assert!(!last.error.unwrap().contains("192.0.2.4"));

        // Reported: titles left, as the job recorded it.
        let mut j = job(Step::Indexing);
        j.ended_at = Some(at() - Duration::hours(1));
        j.outcome = Some(Outcome::TitlesLeft);
        j.error = Some("LoC rate limited titles-sync with 2779 of 3122 titles left".into());
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        assert_eq!(
            (a.now, a.last.unwrap().outcome),
            (Now::Idle, LastOutcome::TitlesLeft)
        );

        // A report that went quiet: stopped.
        let mut j = job(Step::Titles);
        j.updated_at = at() - Duration::minutes(30);
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        assert_eq!(a.now, Now::Idle);
        let last = a.last.unwrap();
        assert_eq!(
            (last.outcome, last.ended_at),
            (LastOutcome::Stopped, at() - Duration::minutes(30))
        );
    }

    #[test]
    fn while_a_run_reports_the_last_is_the_one_before_it() {
        // The 2026-10-03 case: an index build failed on 10-02, then two
        // executions ran out of time on titles-sync, and a third runs now.
        let mut s = base();
        let mut failed = run("v2", "failed", at() - Duration::hours(26));
        failed.failed_at = Some(at() - Duration::hours(25));
        s.runs.push(failed);
        let mut before = job(Step::Titles);
        before.owner = "caj-usnm-ingest-prod-cs6qk3l-4fwwp-1-0a1b2c3d".into();
        before.ended_at = Some(at() - Duration::hours(2));
        before.outcome = Some(Outcome::TitlesLeft);
        before.error = Some(
            "titles-sync reached its deadline with 1457 of 2262 titles left; the cache kept \
             what was fetched, and the next run continues"
                .into(),
        );
        let mut j = job(Step::Titles);
        j.started_at = at() - Duration::hours(1);
        j.previous = Some(Box::new(before));
        s.ops.activity = Some(j);
        let a = get(&s, &[], None);
        assert_eq!((a.now, a.source), (Now::Titles, "job"));
        let last = a.last.unwrap();
        assert_eq!(
            (last.outcome, last.ended_at, last.step),
            (
                LastOutcome::TitlesLeft,
                at() - Duration::hours(2),
                Some(Now::Titles)
            )
        );
        assert!(last.error.unwrap().contains("1457 of 2262"));

        // The one before stopped without saying: stopped at its last report.
        let mut killed = job(Step::Indexing);
        killed.updated_at = at() - Duration::hours(3);
        s.ops.activity.as_mut().unwrap().previous = Some(Box::new(killed));
        let last = get(&s, &[], None).last.unwrap();
        assert_eq!(
            (last.outcome, last.ended_at),
            (LastOutcome::Stopped, at() - Duration::hours(3))
        );

        // No previous (an item from before it was kept): the index runs.
        s.ops.activity.as_mut().unwrap().previous = None;
        assert_eq!(
            get(&s, &[], None).last.unwrap().outcome,
            LastOutcome::Failed
        );
    }
}
