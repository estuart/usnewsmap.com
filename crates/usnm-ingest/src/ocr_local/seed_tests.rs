//! The seeder against a local exchange: damage tables written directly, a
//! directory queue, and in-memory pipeline state.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::message::{decode, OcrMessage};
use usnm_state::docs::MemoryDocs;
use usnm_state::state::{Curated, RunBatch, State};
use usnm_store::queue::{LocalQueue, Queue, BULK};
use usnm_store::ObjectStore;

use super::ledger::{self, Outcome, Queued, DEAD_LETTERED, IMPORTED};
use super::seed::{self, Options, SeedReport};
use super::sync;
use crate::ocr_damage::{self, PageRow, VersionBatch, VersionList};

const GER: &str = "sn00000010";
const ENG: &str = "sn00000020";
const FRE: &str = "sn00000030";
const JPN: &str = "sn00000040";

struct Fx {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    state: State,
    curated: Arc<dyn ObjectStore>,
    reference: Arc<dyn ObjectStore>,
    queue: Arc<dyn Queue>,
}

fn open(dir: &std::path::Path, sub: &str) -> Arc<dyn ObjectStore> {
    usnm_store::open(dir.join(sub).to_str().unwrap()).unwrap()
}

fn page(batch: &str, lccn: &str, day: u32, seq: u16, lang: &str, rate: f64) -> PageRow {
    let date = NaiveDate::from_ymd_opt(1890, 5, day).unwrap();
    PageRow {
        doc_id: PageKey::new(lccn, date, 1, seq).unwrap().doc_id(),
        lccn: lccn.into(),
        date,
        edition: 1,
        seq,
        batch: batch.into(),
        batch_version: 1,
        text_status: "ok",
        lang: Some(lang.into()),
        decision: "detected",
        mixed_with: None,
        script: Some("latin"),
        damage_rate: Some(rate),
        function_share: Some(0.3),
        garbage_share: Some(0.01),
        words: 500,
        head: 100,
        near: (rate * 100.0) as u32,
    }
}

fn doc(lccn: &str, day: u32, seq: u16) -> String {
    PageKey::new(lccn, NaiveDate::from_ymd_opt(1890, 5, day).unwrap(), 1, seq)
        .unwrap()
        .doc_id()
}

/// Version `v1`:
/// - `aa_ger`: a German title, 6 pages at 0.5 (two issues, given out of
///   order), rank 2.4;
/// - `zz_dup`: two of `aa_ger`'s pages again (copies the version doesn't
///   keep) and one of its own at 0.4, rank 1.1;
/// - `bb_eng`: an English title, 4 pages at 0.3 and 2 at 0.05, rank 0.8;
/// - `cc_fre`: a French title at 0.9 (not a pilot language);
/// - `dd_jpn`: a Japanese title at 0.9.
async fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let curated = open(dir.path(), "curated");
    let reference = open(dir.path(), "reference");
    let queue: Arc<dyn Queue> = Arc::new(LocalQueue::new(dir.path().join("queue")));
    let mut tables: Vec<(&str, &str, Vec<PageRow>)> = vec![
        (
            "aa_ger",
            GER,
            [(4, 1), (4, 2), (4, 3), (3, 1), (3, 2), (3, 3)]
                .iter()
                .map(|&(d, s)| page("aa_ger", GER, d, s, "ger", 0.5))
                .collect(),
        ),
        (
            "zz_dup",
            GER,
            vec![
                page("zz_dup", GER, 3, 1, "ger", 0.5),
                page("zz_dup", GER, 3, 2, "ger", 0.5),
                page("zz_dup", GER, 5, 1, "ger", 0.4),
            ],
        ),
        (
            "bb_eng",
            ENG,
            (1..=6)
                .map(|s| page("bb_eng", ENG, 7, s, "eng", if s <= 4 { 0.3 } else { 0.05 }))
                .collect(),
        ),
        ("cc_fre", FRE, vec![page("cc_fre", FRE, 8, 1, "fre", 0.9)]),
        ("dd_jpn", JPN, vec![page("dd_jpn", JPN, 9, 1, "eng", 0.9)]),
    ];
    tables.sort_by(|a, b| a.0.cmp(b.0));
    let mut batches = Vec::new();
    let mut listed = Vec::new();
    for (batch, lccn, rows) in &tables {
        let table = ocr_damage::table_path(batch, 1);
        curated
            .put_new(&table, ocr_damage::encode_table(rows).unwrap(), "x")
            .await
            .unwrap();
        listed.push(VersionBatch {
            batch: (*batch).into(),
            version: 1,
            table,
            summary: ocr_damage::summary_path(batch, 1),
        });
        batches.push(RunBatch {
            batch: (*batch).into(),
            curated: Curated {
                version: 1,
                parts: vec![],
                counts: String::new(),
                pages: rows.len() as u64,
                ok_pages: rows.len() as u64,
                lccns: vec![(*lccn).into()],
                source_sha256: String::new(),
                first: String::new(),
                last: String::new(),
            },
        });
    }
    let list = VersionList {
        index_version: "v1".into(),
        metric_version: usnm_core::damage::VERSION,
        batches: listed,
    };
    curated
        .put_new(
            &ocr_damage::versions_path("v1"),
            serde_json::to_vec(&list).unwrap(),
            "x",
        )
        .await
        .unwrap();
    reference
        .put_new(
            "v1/batches.json",
            serde_json::to_vec(&batches).unwrap(),
            "x",
        )
        .await
        .unwrap();
    reference
        .put_new(
            "v1/titles.json",
            serde_json::to_vec(&serde_json::json!([
                {"lccn": GER, "languages": ["ger"]},
                {"lccn": ENG, "languages": ["eng"]},
                {"lccn": FRE, "languages": ["fre"]},
                {"lccn": JPN, "languages": ["jpn", "eng"]},
            ]))
            .unwrap(),
            "x",
        )
        .await
        .unwrap();
    let state = State::new(Arc::new(MemoryDocs::default()));
    state.set_current_version("v1").await.unwrap();
    Fx {
        dir: dir.path().to_path_buf(),
        _dir: dir,
        state,
        curated,
        reference,
        queue,
    }
}

fn plan(window: u64) -> Options {
    Options {
        plan: "bulk-1".into(),
        window,
        ..Options::default()
    }
}

impl Fx {
    async fn seed(&self, opts: &Options) -> anyhow::Result<SeedReport> {
        let stores = seed::Stores {
            curated: self.curated.as_ref(),
            reference: self.reference.as_ref(),
            queue: &self.queue,
        };
        seed::run(&self.state, "test", &stores, opts).await
    }

    /// Take every message off `ocr-bulk`, as workers would.
    async fn drain(&self) -> Vec<OcrMessage> {
        let mut out = Vec::new();
        while let Some(m) = self
            .queue
            .receive(BULK, Duration::from_secs(1800))
            .await
            .unwrap()
        {
            out.push(decode(&m.text).unwrap().0);
            self.queue.delete(&m).await.unwrap();
        }
        out
    }

    async fn outcome(&self, doc_id: &str, outcome: &str, cause: Option<&str>, at: DateTime<Utc>) {
        ledger::write_outcomes(
            self.curated.as_ref(),
            &[Outcome {
                doc_id: doc_id.into(),
                outcome: outcome.into(),
                cause: cause.map(str::to_owned),
                kind: None,
                source: "test".into(),
                at,
                run: "test".into(),
            }],
            at,
            &super::run_id(at),
        )
        .await
        .unwrap();
    }

    async fn ledger_files(&self) -> Vec<String> {
        self.curated
            .list(&format!("{}/queued", super::ROOT))
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn a_dry_run_ranks_units_and_writes_nothing() {
    let fx = fixture().await;
    let r = fx
        .seed(&Options {
            dry_run: true,
            ..plan(100)
        })
        .await
        .unwrap();
    let order: Vec<(&str, u64)> = r
        .units
        .iter()
        .map(|u| (u.batch.as_str(), u.queued))
        .collect();
    // French isn't a pilot language and Japanese titles are never seeded;
    // the 0.05 English pages are under the minimum damage.
    assert_eq!(order, [("aa_ger", 6), ("zz_dup", 1), ("bb_eng", 4)]);
    assert_eq!(r.queued, 11);
    assert_eq!(r.skipped.get("duplicate"), Some(&2));
    // Issue order within a unit.
    assert_eq!(
        r.units[0].pages,
        [
            doc(GER, 3, 1),
            doc(GER, 3, 2),
            doc(GER, 3, 3),
            doc(GER, 4, 1),
            doc(GER, 4, 2),
            doc(GER, 4, 3)
        ]
    );
    assert!((r.units[0].score - 2.4).abs() < 1e-9);
    assert!(fx.ledger_files().await.is_empty());
    assert_eq!(fx.queue.approximate_count(BULK).await.unwrap(), 0);
    assert!(fx
        .curated
        .list(&format!("{}/seed", super::ROOT))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn never_more_than_the_window_outstanding() {
    let fx = fixture().await;
    let r = fx.seed(&plan(5)).await.unwrap();
    assert_eq!(r.queued, 5);
    assert_eq!(fx.queue.approximate_count(BULK).await.unwrap(), 5);
    let l = ledger::load(fx.curated.as_ref()).await.unwrap();
    assert_eq!(l.outstanding_count(), 5);

    // The queue is above the refill threshold (half the window).
    let r = fx.seed(&plan(5)).await.unwrap();
    assert!(!r.refill);
    assert_eq!(r.queued, 0);

    // Workers took every message, but no result is back: still 5 outstanding.
    let msgs = fx.drain().await;
    assert_eq!(msgs.len(), 5);
    assert!(msgs
        .iter()
        .all(|m| m.language == "deu" && m.batch == "aa_ger"));
    assert!(msgs.iter().all(|m| m.score == Some(0.5)));
    let r = fx.seed(&plan(5)).await.unwrap();
    assert!(r.refill);
    assert_eq!(r.in_flight, 5);
    assert_eq!(r.queued, 0);

    // Three come back imported: three more go out, in rank order.
    for m in &msgs[..3] {
        let id = m.key().unwrap().doc_id();
        fx.outcome(&id, IMPORTED, None, Utc::now()).await;
    }
    let r = fx.seed(&plan(5)).await.unwrap();
    assert_eq!(r.in_flight, 2);
    assert_eq!(r.queued, 3);
    let units: Vec<(&str, u64)> = r
        .units
        .iter()
        .map(|u| (u.batch.as_str(), u.queued))
        .collect();
    assert_eq!(units, [("aa_ger", 1), ("zz_dup", 1), ("bb_eng", 1)]);
    // aa_ger and zz_dup are finished for the plan; bb_eng isn't.
    let done = fx
        .curated
        .list(&format!("{}/seed/plans/bulk-1/done", super::ROOT))
        .await
        .unwrap();
    assert_eq!(done.len(), 1);
    let body = fx.curated.get(&done[0]).await.unwrap().unwrap();
    let units: Vec<seed::DoneUnit> = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = units.iter().map(|u| u.batch.as_str()).collect();
    assert_eq!(names, ["aa_ger", "zz_dup"]);
}

#[tokio::test]
async fn a_pilot_is_one_group_within_its_budget() {
    let fx = fixture().await;
    let two = Options {
        pilot: true,
        max_pages: Some(3),
        ..plan(100)
    };
    assert!(fx.seed(&two).await.is_err(), "two groups");
    let unbounded = Options {
        pilot: true,
        languages: vec!["deu".into()],
        ..plan(100)
    };
    assert!(fx.seed(&unbounded).await.is_err(), "no budget");
    let too_big = Options {
        max_pages: Some(seed::PILOT_MAX_PAGES + 1),
        ..unbounded.clone()
    };
    assert!(fx.seed(&too_big).await.is_err(), "budget over the cap");

    let pilot = Options {
        plan: "pilot-deu".into(),
        max_pages: Some(4),
        ..unbounded
    };
    let r = fx.seed(&pilot).await.unwrap();
    assert_eq!(r.queued, 4);
    assert_eq!(r.budget_left, Some(4));
    let msgs = fx.drain().await;
    assert!(msgs.iter().all(|m| m.language == "deu"));
    let r = fx.seed(&pilot).await.unwrap();
    assert_eq!(r.budget_left, Some(0));
    assert_eq!(r.queued, 0);

    // A batch list is a group too.
    let batches = Options {
        plan: "pilot-eng".into(),
        pilot: true,
        batches: vec!["bb_eng_ver01".into()],
        max_pages: Some(10),
        dry_run: true,
        ..plan(100)
    };
    let r = fx.seed(&batches).await.unwrap();
    assert_eq!(r.queued, 4);
    assert!(r.units.iter().all(|u| u.batch == "bb_eng"));
}

#[tokio::test]
async fn other_languages_on_request() {
    let fx = fixture().await;
    let r = fx
        .seed(&Options {
            languages: vec!["fra".into()],
            dry_run: true,
            ..plan(100)
        })
        .await
        .unwrap();
    assert_eq!(r.queued, 1);
    assert_eq!(r.units[0].batch, "cc_fre");
    let bad = Options {
        languages: vec!["xx".into()],
        ..plan(100)
    };
    assert!(fx.seed(&bad).await.is_err());
    let years = Options {
        years: Some((1900, 1910)),
        dry_run: true,
        ..plan(100)
    };
    assert_eq!(fx.seed(&years).await.unwrap().queued, 0);
}

fn mine(doc_id: &str, attempt: u32, at: DateTime<Utc>) -> Queued {
    Queued {
        doc_id: doc_id.into(),
        batch: "aa_ger".into(),
        batch_version: 1,
        reason: "bulk".into(),
        language: "deu".into(),
        plan: "bulk-1".into(),
        attempt,
        queued_at: at,
    }
}

#[tokio::test]
async fn retries_after_14_days_dequeue_count_and_unroutable() {
    let fx = fixture().await;
    let now = Utc::now();
    let old = now - chrono::Duration::days(15);
    let rows = vec![
        // Timed out: queued again.
        mine(&doc(GER, 3, 1), 1, old),
        // Timed out with its three retries spent.
        mine(&doc(GER, 3, 2), 4, old),
        // Dead-lettered after 5 receives: queued again.
        mine(&doc(GER, 3, 3), 1, now - chrono::Duration::hours(3)),
        // Unroutable an hour ago: waits.
        mine(&doc(GER, 4, 1), 1, now - chrono::Duration::hours(3)),
        // Unroutable 7 hours ago: queued again.
        mine(&doc(GER, 4, 2), 1, now - chrono::Duration::hours(9)),
        // Recent, no result: in flight.
        mine(&doc(GER, 4, 3), 1, now - chrono::Duration::hours(1)),
    ];
    ledger::write_queued(fx.curated.as_ref(), &rows, old, "r1")
        .await
        .unwrap();
    fx.outcome(
        &doc(GER, 3, 3),
        DEAD_LETTERED,
        Some("dequeue_count"),
        now - chrono::Duration::hours(2),
    )
    .await;
    fx.outcome(
        &doc(GER, 4, 1),
        DEAD_LETTERED,
        Some("unroutable"),
        now - chrono::Duration::hours(1),
    )
    .await;
    fx.outcome(
        &doc(GER, 4, 2),
        DEAD_LETTERED,
        Some("unroutable"),
        now - chrono::Duration::hours(7),
    )
    .await;
    let r = fx
        .seed(&Options {
            dry_run: true,
            ..plan(100)
        })
        .await
        .unwrap();
    assert_eq!(r.in_flight, 1);
    assert_eq!(r.requeued.get("timed_out"), Some(&1));
    assert_eq!(r.requeued.get("dequeue_count"), Some(&1));
    assert_eq!(r.requeued.get("unroutable"), Some(&1));
    assert_eq!(r.skipped.get("retries_spent"), Some(&1));
    let mut again = r.requeue_pages.clone();
    again.sort();
    assert_eq!(again, [doc(GER, 3, 1), doc(GER, 3, 3), doc(GER, 4, 2)]);

    let r = fx.seed(&plan(100)).await.unwrap();
    assert_eq!(r.requeued.values().sum::<u64>(), 3);
    let l = ledger::load(fx.curated.as_ref()).await.unwrap();
    assert_eq!(l.queued[&doc(GER, 3, 1)].attempt, 2);
    assert_eq!(l.unroutable[&doc(GER, 4, 2)], 1);
}

#[test]
fn unroutable_waits_double_up_to_48_hours() {
    let w: Vec<i64> = (1..=6).map(seed::unroutable_wait_hours).collect();
    assert_eq!(w, [6, 12, 24, 48, 48, 48]);
}

#[test]
fn window_and_years() {
    assert_eq!(seed::auto_window(0.0), seed::MIN_AUTO_WINDOW);
    assert_eq!(seed::auto_window(1000.0), 2000);
    assert_eq!(seed::parse_years("").unwrap(), None);
    assert_eq!(seed::parse_years("1890").unwrap(), Some((1890, 1890)));
    assert_eq!(seed::parse_years("1850-1919").unwrap(), Some((1850, 1919)));
    assert!(seed::parse_years("1919-x").is_err());
    assert!(Options {
        years: Some((1919, 1850)),
        ..plan(1)
    }
    .check()
    .is_err());
}

#[tokio::test]
async fn ocr_sync_seeds_after_its_imports() {
    let fx = fixture().await;
    let stores = sync::Stores {
        curated: Arc::clone(&fx.curated),
        reference: Arc::clone(&fx.reference),
        out: open(&fx.dir, "out"),
        quarantine: open(&fx.dir, "quarantine"),
        queue: Arc::clone(&fx.queue),
    };
    // No plan: nothing queued.
    let status = sync::run(&fx.state, "test", &stores, &sync::Options::default())
        .await
        .unwrap();
    assert!(status.seed.is_none());
    assert_eq!(fx.queue.approximate_count(BULK).await.unwrap(), 0);

    let opts = sync::Options {
        seed: plan(4),
        ..sync::Options::default()
    };
    let status = sync::run(&fx.state, "test", &stores, &opts).await.unwrap();
    assert_eq!(status.seed.as_ref().map(|s| s.queued), Some(4));
    assert_eq!(status.queues.get(BULK), Some(&4));
}
