//! `ocr-sync`, `ocr-enqueue` and `ocr-requeue` against a local exchange:
//! directories for `ocr-out`, `ocr-quarantine`, the curated and reference
//! stores, a directory queue, and in-memory pipeline state.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use usnm_core::ids::PageKey;
use usnm_ocr_contract::message::{Cause, DeadLetter, OcrMessage, Reason};
use usnm_overlay::OverlayRow;
use usnm_state::docs::MemoryDocs;
use usnm_state::state::{Curated, RunBatch, State};
use usnm_store::queue::{LocalQueue, Queue, BULK, DEADLETTER, PRIORITY};
use usnm_store::ObjectStore;

use super::checks::{self, Reject};
use super::enqueue::{self, EnqueueOptions, RequeueOptions};
use super::ledger::{self, Queued};
use super::quarantine::{self, Record};
use super::sync::{self, ImportRecord, Options, Status, Stores};
use super::{pages, report, ROOT, STATUS_PATH};
use crate::curated::{CuratedRow, PartWriter};

const LCCN: &str = "sn83045462";
const JA_LCCN: &str = "sn00000001";
const BATCH: &str = "nbu_gage";
const NODE: &str = "node1";

const GERMAN: &str = "Der Stadtrath hat am Dienstag beschlossen, dass die neue Schule auf dem Grundstück \
    gebaut werden soll, welches die Behörde im vorigen Jahre gekauft hat. Es ist noch nicht bekannt, wann \
    die Arbeiten beginnen werden, aber man hofft, dass es bald sein wird und dass die Kinder im nächsten \
    Jahre in das neue Haus ziehen können. ";

const ALTO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<alto xmlns="http://www.loc.gov/standards/alto/ns-v3#">
  <Description><MeasurementUnit>pixel</MeasurementUnit></Description>
  <Layout><Page ID="p1" WIDTH="5000" HEIGHT="7000"><PrintSpace>
    <TextBlock ID="b1"><TextLine ID="l1"><String CONTENT="Der"/></TextLine></TextBlock>
  </PrintSpace></Page></Layout>
</alto>
"#;

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(1890, 5, 3).unwrap()
}

fn key(seq: u16) -> PageKey {
    PageKey::new(LCCN, day(), 1, seq).unwrap()
}

/// LoC's text, badly read: the German head words one letter off.
fn damaged() -> String {
    GERMAN
        .replace("die ", "dic ")
        .replace("der ", "dcr ")
        .replace("und ", "nnd ")
        .replace("das ", "bas ")
        .replace("dass ", "baß ")
        .repeat(4)
}

struct Fx {
    _dir: tempfile::TempDir,
    state: State,
    stores: Stores,
    out: Arc<dyn ObjectStore>,
    queue: Arc<dyn Queue>,
}

fn open(dir: &std::path::Path, sub: &str) -> Arc<dyn ObjectStore> {
    usnm_store::open(dir.join(sub).to_str().unwrap()).unwrap()
}

/// Version `v1` with `nbu_gage` (a German title, 9 pages of damaged LoC
/// text) and `dlc_japan` (a Japanese title); `with_tables` runs the damage
/// pass so LoC's scores come from Step 6's tables.
async fn fixture(with_tables: bool) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let curated = open(dir.path(), "curated");
    let reference = open(dir.path(), "reference");
    let out = open(dir.path(), "out");
    let quarantine = open(dir.path(), "quarantine");
    let queue: Arc<dyn Queue> = Arc::new(LocalQueue::new(dir.path().join("queue")));
    let at = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
    let mut batches = Vec::new();
    for (batch, lccn) in [(BATCH, LCCN), ("dlc_japan", JA_LCCN)] {
        let mut w = PartWriter::new();
        for seq in 1..=9 {
            let k = PageKey::new(lccn, day(), 1, seq).unwrap();
            assert!(w
                .push(CuratedRow::from_ocr(k, &damaged(), batch, 1, "loc", at))
                .unwrap()
                .is_none());
        }
        let part = format!("pages/{batch}_ver01/part-0.parquet");
        curated
            .put_new(&part, w.finish_part().unwrap().unwrap(), "x")
            .await
            .unwrap();
        batches.push(RunBatch {
            batch: batch.into(),
            curated: Curated {
                version: 1,
                parts: vec![part],
                counts: String::new(),
                pages: 9,
                ok_pages: 9,
                lccns: vec![lccn.into()],
                source_sha256: String::new(),
                first: String::new(),
                last: String::new(),
            },
        });
    }
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
                {"lccn": LCCN, "languages": ["ger"]},
                {"lccn": JA_LCCN, "languages": ["jpn", "eng"]},
            ]))
            .unwrap(),
            "x",
        )
        .await
        .unwrap();
    if with_tables {
        crate::ocr_damage::full_pass(
            Arc::clone(&curated),
            reference.as_ref(),
            &crate::ocr_damage::Options {
                index_version: "v1".into(),
                parallel: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }
    let state = State::new(Arc::new(MemoryDocs::default()));
    state.set_current_version("v1").await.unwrap();
    Fx {
        _dir: dir,
        state,
        stores: Stores {
            curated,
            reference,
            out: Arc::clone(&out),
            quarantine,
            queue: Arc::clone(&queue),
        },
        out,
        queue,
    }
}

fn queued(doc_id: &str, batch: &str, reason: &str) -> Queued {
    Queued {
        doc_id: doc_id.into(),
        batch: batch.into(),
        batch_version: 1,
        reason: reason.into(),
        language: "deu".into(),
        plan: "test".into(),
        attempt: 1,
        queued_at: Utc::now() - chrono::Duration::hours(1),
    }
}

impl Fx {
    async fn ask(&self, rows: &[Queued]) {
        ledger::write_queued(
            self.stores.curated.as_ref(),
            rows,
            Utc::now(),
            &super::run_id(Utc::now()),
        )
        .await
        .unwrap();
    }

    /// What a worker writes for a page: its ALTO, then its part.
    async fn node_writes(&self, row: &OverlayRow, alto: &str, stamp: &str) -> (String, String) {
        let k = &row.key;
        let alto_path = format!("alto/{}.{NODE}.{stamp}.xml", k.doc_id());
        let part_path = format!(
            "pages/{}_{}_ed-{}.{NODE}.{stamp}.parquet",
            k.lccn,
            k.date.format("%Y-%m-%d"),
            k.edition
        );
        assert!(self
            .out
            .put_new(&alto_path, alto.as_bytes().to_vec(), "x")
            .await
            .unwrap());
        assert!(self
            .out
            .put_new(
                &part_path,
                usnm_overlay::write_part(std::slice::from_ref(row)).unwrap(),
                "x"
            )
            .await
            .unwrap());
        (alto_path, part_path)
    }

    async fn sync(&self) -> Status {
        sync::run(&self.state, "test", &self.stores, &Options::default())
            .await
            .unwrap()
    }

    async fn out_paths(&self) -> Vec<String> {
        let mut v = self.out.list("pages").await.unwrap();
        v.extend(self.out.list("alto").await.unwrap());
        v
    }

    async fn records(&self) -> Vec<Record> {
        let mut out = Vec::new();
        for p in self.stores.quarantine.list(quarantine::TOP).await.unwrap() {
            if p.ends_with(".reason.json") {
                let b = self.stores.quarantine.get(&p).await.unwrap().unwrap();
                out.push(serde_json::from_slice(&b).unwrap());
            }
        }
        out
    }
}

fn row(seq: u16, text: &str) -> OverlayRow {
    OverlayRow {
        key: key(seq),
        batch: BATCH.into(),
        ocr_source: "usnm-tesseract".into(),
        ocr_engine: "tesseract-5.5.1/deu_latf+deu".into(),
        loc_text: "bulk".into(),
        text: text.into(),
        image_url: "https://tile.loc.gov/image-services/iiif/service:ndnp:nbu:batch_nbu_gage_ver01:data:sn83045462:0001".into(),
        ocred_at: Utc::now(),
    }
}

fn stamp(n: u32) -> String {
    format!("20261011T120000{n:06}Z")
}

#[tokio::test]
async fn imports_a_page_it_asked_for() {
    for with_tables in [true, false] {
        let fx = fixture(with_tables).await;
        let doc = key(3).doc_id();
        fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
        let text = GERMAN.repeat(4);
        let (alto_src, part_src) = fx.node_writes(&row(3, &text), ALTO, &stamp(1)).await;

        let status = fx.sync().await;
        assert_eq!(status.last_run.imported, 1, "{:?}", status.last_run);
        assert!(status.last_run.rejected.is_empty());
        assert!(fx.out_paths().await.is_empty(), "ocr-out emptied");

        let c = &fx.stores.curated;
        let alto = c
            .get(&format!("{ROOT}/alto/{doc}.{NODE}.{}.xml", stamp(1)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(alto, ALTO.as_bytes());
        let parts = c.list(&format!("{ROOT}/pages/{LCCN}")).await.unwrap();
        assert_eq!(parts.len(), 1);
        let bytes = bytes::Bytes::from(c.get(&parts[0]).await.unwrap().unwrap());
        let stored = pages::read_scores(bytes.clone()).unwrap();
        assert_eq!(stored.len(), 1);
        let s = &stored[0];
        assert_eq!(s.doc_id, doc);
        assert_eq!(s.ours.lang.as_deref(), Some("ger"));
        assert_eq!(s.loc.lang.as_deref(), Some("ger"));
        let (ours, loc) = (s.ours.damage_rate.unwrap(), s.loc.damage_rate.unwrap());
        assert!(ours < loc, "ours {ours} < LoC's {loc}");
        // The overlay columns read back as a part; ocred_at is the server's.
        let overlay = usnm_overlay::read_part(bytes).unwrap();
        assert_eq!(overlay[0].text.as_deref(), Some(text.as_str()));
        assert_eq!(overlay[0].loc_text, "bulk");

        let records = c.list(&format!("{ROOT}/import")).await.unwrap();
        assert_eq!(records.len(), 1);
        let rec: ImportRecord =
            serde_json::from_slice(&c.get(&records[0]).await.unwrap().unwrap()).unwrap();
        let mut srcs: Vec<&str> = rec.sources.iter().map(|s| s.path.as_str()).collect();
        srcs.sort();
        assert_eq!(srcs, [alto_src.as_str(), part_src.as_str()]);
        let l = ledger::load(c.as_ref()).await.unwrap();
        assert!(l.imported(&doc) && l.outstanding(&doc).is_none());

        let st: Status =
            serde_json::from_slice(&fx.stores.reference.get(STATUS_PATH).await.unwrap().unwrap())
                .unwrap();
        assert_eq!(
            (st.totals.imported, st.parts_left, st.outstanding),
            (1, 0, 0)
        );

        // The same page again is no longer asked for.
        fx.node_writes(&row(3, &text), ALTO, &stamp(2)).await;
        let again = fx.sync().await;
        assert_eq!(again.last_run.imported, 0);
        assert_eq!(again.last_run.rejected.get("not_requested"), Some(&1));
        assert_eq!(again.totals.imported, 1);
    }
}

/// One fixture per reject reason: a change to a good page's output.
#[tokio::test]
async fn each_reject_goes_to_quarantine_with_its_reason() {
    type Case = (
        &'static str,
        Box<dyn Fn(&mut OverlayRow, &mut String, &mut Queued)>,
    );
    let text = GERMAN.repeat(4);
    let cases: Vec<Case> = vec![
        (
            "not_requested",
            Box::new(|_, _, q| q.doc_id = "other".into()),
        ),
        (
            "batch_mismatch",
            Box::new(|_, _, q| q.batch = "nbu_other".into()),
        ),
        (
            "reason_mismatch",
            Box::new(|_, _, q| q.reason = "user".into()),
        ),
        ("text_control", Box::new(|r, _, _| r.text.push('\u{7}'))),
        (
            "engine",
            Box::new(|r, _, _| r.ocr_source = "usnm-other".into()),
        ),
        (
            "engine_tag",
            Box::new(|r, _, _| {
                r.ocr_engine = "tesseract/5.5.3 profile=deu image=local contract=1".into()
            }),
        ),
        (
            "reason",
            Box::new(|r, _, q| {
                r.loc_text = "search".into();
                q.reason = "search".into();
            }),
        ),
        (
            "batch_unpublished",
            Box::new(|r, _, q| {
                r.batch = "nbu_other".into();
                q.batch = "nbu_other".into();
            }),
        ),
        (
            "title_unknown",
            Box::new(|r, _, q| {
                r.key = PageKey::new("sn99999999", day(), 1, 3).unwrap();
                q.doc_id = r.key.doc_id();
            }),
        ),
        (
            "title_japanese",
            Box::new(|r, _, q| {
                r.key = PageKey::new(JA_LCCN, day(), 1, 3).unwrap();
                r.batch = "dlc_japan".into();
                q.doc_id = r.key.doc_id();
                q.batch = "dlc_japan".into();
            }),
        ),
        (
            "image_url",
            Box::new(|r, _, _| r.image_url = "https://evil.example/x".into()),
        ),
        (
            "alto_doctype",
            Box::new(|_, a, _| *a = a.replace("<alto ", "<!DOCTYPE alto>\n<alto ")),
        ),
        (
            "alto_no_page",
            Box::new(|_, a, _| *a = a.replace("WIDTH=\"5000\"", "")),
        ),
        ("alto_not_alto", Box::new(|_, a, _| *a = "<html/>".into())),
    ];
    for (want, change) in cases {
        let fx = fixture(false).await;
        let mut r = row(3, &text);
        let mut alto = ALTO.to_owned();
        let mut q = queued(&key(3).doc_id(), BATCH, "bulk");
        change(&mut r, &mut alto, &mut q);
        fx.ask(&[q]).await;
        let (alto_src, part_src) = fx.node_writes(&r, &alto, &stamp(1)).await;
        let status = fx.sync().await;
        assert_eq!(status.last_run.imported, 0, "{want}");
        assert_eq!(
            status.last_run.rejected.get(want),
            Some(&1),
            "{want}: {:?}",
            status.last_run.rejected
        );
        assert!(fx.out_paths().await.is_empty(), "{want}: ocr-out emptied");
        let records = fx.records().await;
        let mut sources: Vec<(&str, &str)> = records
            .iter()
            .map(|r| (r.source.as_str(), r.reason.as_str()))
            .collect();
        sources.sort();
        assert_eq!(
            sources,
            [(alto_src.as_str(), want), (part_src.as_str(), want)],
            "{want}"
        );
        // Records hold no text.
        for rec in &records {
            let json = serde_json::to_string(rec).unwrap();
            assert!(!json.contains("Stadtrath"), "{want}");
        }
        assert!(fx
            .stores
            .curated
            .list(&format!("{ROOT}/pages"))
            .await
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn rejects_bad_names_sizes_and_parts() {
    let fx = fixture(false).await;
    let doc = key(3).doc_id();
    fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
    // A name outside the pattern.
    fx.out
        .put_new("pages/whatever.parquet", b"x".to_vec(), "x")
        .await
        .unwrap();
    // Too large to read.
    fx.out
        .put_new(
            &format!("pages/{LCCN}_1890-05-03_ed-1.{NODE}.{}.parquet", stamp(1)),
            vec![0; checks::MAX_PART_BYTES as usize + 1],
            "x",
        )
        .await
        .unwrap();
    // Not Parquet.
    fx.out
        .put_new(
            &format!("pages/{LCCN}_1890-05-03_ed-1.{NODE}.{}.parquet", stamp(2)),
            b"not parquet".to_vec(),
            "x",
        )
        .await
        .unwrap();
    // Two rows.
    let two = usnm_overlay::write_part(&[row(3, "a"), row(4, "b")]).unwrap();
    fx.out
        .put_new(
            &format!("pages/{LCCN}_1890-05-03_ed-1.{NODE}.{}.parquet", stamp(3)),
            two,
            "x",
        )
        .await
        .unwrap();
    // A row from another issue than its name.
    let other = OverlayRow {
        key: PageKey::new(LCCN, day(), 2, 3).unwrap(),
        ..row(3, "x")
    };
    fx.out
        .put_new(
            &format!("pages/{LCCN}_1890-05-03_ed-1.{NODE}.{}.parquet", stamp(4)),
            usnm_overlay::write_part(&[other]).unwrap(),
            "x",
        )
        .await
        .unwrap();
    // No ALTO.
    fx.out
        .put_new(
            &format!("pages/{LCCN}_1890-05-03_ed-1.{NODE}.{}.parquet", stamp(5)),
            usnm_overlay::write_part(&[row(3, &GERMAN.repeat(4))]).unwrap(),
            "x",
        )
        .await
        .unwrap();
    let status = fx.sync().await;
    let r = &status.last_run.rejected;
    for code in [
        "name",
        "part_too_large",
        "part_unreadable",
        "rows",
        "issue_mismatch",
        "alto_missing",
    ] {
        assert_eq!(r.get(code), Some(&1), "{code}: {r:?}");
    }
    assert!(fx.out_paths().await.is_empty());
    // The oversized part's record has no content.
    let big = fx
        .records()
        .await
        .into_iter()
        .find(|r| r.reason == "part_too_large")
        .unwrap();
    assert_eq!(big.content, None);
}

#[tokio::test]
async fn a_run_stopped_before_its_deletes_is_finished_without_importing_again() {
    let fx = fixture(true).await;
    let doc = key(3).doc_id();
    fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
    let (alto_src, part_src) = fx
        .node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    // A run that wrote its import record and stopped: its sources are still
    // in ocr-out.
    let listed = {
        let mut v = fx.out.list_info("pages").await.unwrap();
        v.extend(fx.out.list_info("alto").await.unwrap());
        v
    };
    let record = ImportRecord {
        run: super::run_id(Utc::now()),
        at: Utc::now(),
        index_version: "v1".into(),
        sources: listed
            .iter()
            .map(|o| sync::Consumed {
                path: o.path.clone(),
                etag: o.etag.clone(),
                outcome: "imported".into(),
                reason: None,
            })
            .collect(),
        pages: vec![],
        outcomes: None,
    };
    fx.stores
        .curated
        .put_new(
            &super::import_path(&record.run),
            serde_json::to_vec(&record).unwrap(),
            "x",
        )
        .await
        .unwrap();
    let status = fx.sync().await;
    assert_eq!((status.last_run.cleaned, status.last_run.imported), (2, 0));
    assert!(fx.out_paths().await.is_empty());
    assert!(!fx.out.exists(&alto_src).await.unwrap() && !fx.out.exists(&part_src).await.unwrap());
    // A source written again since that record is new content: checked again.
    fx.node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    let status = fx.sync().await;
    assert_eq!(status.last_run.imported, 1);
}

#[tokio::test]
async fn drains_dead_letters_and_quarantines_bad_ones() {
    let fx = fixture(false).await;
    let msg = OcrMessage {
        v: 1,
        page: key(5).to_string(),
        batch: BATCH.into(),
        language: "deu".into(),
        reason: Reason::Bulk,
        score: Some(0.6),
        queued_at: "2026-10-10T12:00:00Z".into(),
    };
    fx.ask(&[queued(&key(5).doc_id(), BATCH, "bulk")]).await;
    // One page per cause.
    for (seq, cause) in [
        (5, Cause::Permanent),
        (6, Cause::Unroutable),
        (7, Cause::DequeueCount),
    ] {
        let engines = (cause == Cause::Unroutable).then(|| vec!["tesseract-5.5".to_owned()]);
        let msg = OcrMessage {
            page: key(seq).to_string(),
            ..msg.clone()
        };
        let d = DeadLetter::new(
            "ocr-bulk",
            "m1",
            Some(&msg),
            cause,
            None,
            engines,
            "no image",
            6,
            NODE,
        );
        fx.queue.send(DEADLETTER, &d.encode()).await.unwrap();
    }
    // A dead letter with a field the schema doesn't have.
    let bad = r#"{"v":1,"from":"ocr-bulk","message_id":"m2","message":null,"error":"x","dequeue_count":6,"node":"n","at":"2026-10-10T12:00:00Z","q":"cross of gold"}"#;
    fx.queue.send(DEADLETTER, bad).await.unwrap();
    let status = fx.sync().await;
    let d = &status.last_run.dead_letters;
    assert_eq!(
        (
            d.get("permanent"),
            d.get("unroutable"),
            d.get("dequeue_count")
        ),
        (Some(&1), Some(&1), Some(&1))
    );
    assert_eq!(
        status
            .last_run
            .dead_letters_rejected
            .get("deadletter_bad_json"),
        Some(&1)
    );
    assert_eq!(fx.queue.approximate_count(DEADLETTER).await.unwrap(), 0);
    let files = fx
        .stores
        .curated
        .list(&format!("{ROOT}/deadletter"))
        .await
        .unwrap();
    assert_eq!(files.len(), 1);
    let body = fx.stores.curated.get(&files[0]).await.unwrap().unwrap();
    let text = String::from_utf8(body).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(
        !text.contains("cross of gold"),
        "rejects never reach the lake"
    );
    let rec = fx.records().await;
    assert_eq!(rec.len(), 1);
    assert_eq!(
        (rec[0].kind.as_str(), rec[0].reason.as_str()),
        ("deadletter", "deadletter_bad_json")
    );
    // The page's latest outcome is the last dead letter drained.
    let l = ledger::load(fx.stores.curated.as_ref()).await.unwrap();
    assert!(l.outcomes.contains_key(&key(5).doc_id()));

    // ocr-requeue: the permanent one, once.
    let opts = RequeueOptions {
        cause: "permanent".into(),
        kind: None,
        since: None,
        plan: "requeue".into(),
        max_pages: 10,
        dry_run: false,
    };
    // Make sure the outcome is newer than the queued row (same-second runs).
    let r = enqueue::requeue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(r.queued, 1, "{r:?}");
    let m = fx
        .queue
        .receive(BULK, std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    let (back, k) = usnm_ocr_contract::message::decode(&m.text).unwrap();
    assert_eq!((k, back.batch.as_str()), (key(5), BATCH));
    // Queued again since: not twice.
    let again = enqueue::requeue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(again.queued, 0);
    // The requeued page then dead-letters for another cause: no longer
    // `permanent`, so not queued again under that cause.
    ledger::write_outcomes(
        fx.stores.curated.as_ref(),
        &[ledger::Outcome {
            doc_id: key(5).doc_id(),
            outcome: ledger::DEAD_LETTERED.into(),
            cause: Some("dequeue_count".into()),
            kind: None,
            source: "m9".into(),
            at: Utc::now() + chrono::Duration::seconds(5),
            run: "later".into(),
        }],
        Utc::now(),
        "later",
    )
    .await
    .unwrap();
    let third = enqueue::requeue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(
        third.skipped.get("latest_cause_differs"),
        Some(&1),
        "{third:?}"
    );
}

#[tokio::test]
async fn replays_from_quarantine_after_a_fix() {
    let fx = fixture(false).await;
    let doc = key(3).doc_id();
    fx.node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    let first = fx.sync().await;
    assert_eq!(first.last_run.rejected.get("not_requested"), Some(&1));
    // The fix: the ledger row that was missing.
    fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
    let opts = Options {
        from_quarantine: Some("not_requested".into()),
        ..Options::default()
    };
    let status = sync::run(&fx.state, "test", &fx.stores, &opts)
        .await
        .unwrap();
    assert_eq!(status.last_run.imported, 1, "{:?}", status.last_run);
    assert!(fx.records().await.is_empty(), "quarantine emptied");
}

#[tokio::test]
async fn enqueue_finds_the_page_and_writes_the_ledger_first() {
    let fx = fixture(true).await;
    let doc = key(4).doc_id();
    let opts = EnqueueOptions {
        pages: vec![
            doc.clone(),
            PageKey::new(JA_LCCN, day(), 1, 1).unwrap().doc_id(),
            "junk".into(),
        ],
        reason: Reason::User,
        plan: "pilot-test".into(),
        batch: None,
        include_outstanding: false,
        dry_run: false,
    };
    let r = enqueue::enqueue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(r.queued, 1, "{r:?}");
    assert_eq!(r.skipped.get("japanese_title"), Some(&1));
    assert_eq!(r.skipped.get("bad_doc_id"), Some(&1));
    let m = fx
        .queue
        .receive(PRIORITY, std::time::Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    let (msg, k) = usnm_ocr_contract::message::decode(&m.text).unwrap();
    assert_eq!(k, key(4));
    assert_eq!((msg.batch.as_str(), msg.language.as_str()), (BATCH, "deu"));
    assert!(msg.score.is_some_and(|s| s > 0.1));
    let l = ledger::load(fx.stores.curated.as_ref()).await.unwrap();
    let q = l.outstanding(&doc).unwrap();
    assert_eq!(
        (q.plan.as_str(), q.attempt, q.reason.as_str()),
        ("pilot-test", 1, "user")
    );
    // Outstanding now: not queued twice.
    let again = enqueue::enqueue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &opts,
    )
    .await
    .unwrap();
    assert_eq!(again.skipped.get("outstanding"), Some(&1));
    // A send that failed after the ledger was written: queued again on request.
    let retry = enqueue::enqueue(
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
        &fx.queue,
        &EnqueueOptions {
            include_outstanding: true,
            ..opts.clone()
        },
    )
    .await
    .unwrap();
    assert_eq!(retry.queued, 1);
    let l = ledger::load(fx.stores.curated.as_ref()).await.unwrap();
    assert_eq!(l.outstanding(&doc).unwrap().attempt, 2);
}

#[test]
fn part_names() {
    let n = checks::parse_part_name(&format!(
        "pages/{LCCN}_1890-05-03_ed-2.nuc-1.{}.parquet",
        stamp(7)
    ))
    .unwrap();
    assert_eq!(
        (n.lccn.as_str(), n.edition, n.node.as_str()),
        (LCCN, 2, "nuc-1")
    );
    assert_eq!(n.date, day());
    for bad in [
        format!("pages/{LCCN}_1890-05-03_ed-2.nuc1.20261011.parquet"),
        format!("pages/{LCCN}_1890-5-3_ed-2.nuc1.{}.parquet", stamp(1)),
        format!("pages/{LCCN}_1890-05-03_ed-02.nuc1.{}.parquet", stamp(1)),
        format!("pages/{LCCN}_1890-05-03_ed-2.nuc_1.{}.parquet", stamp(1)),
        format!(
            "pages/{LCCN}_1890-05-03_ed-2.{}.{}.parquet",
            "n".repeat(33),
            stamp(1)
        ),
        format!("alto/{LCCN}_1890-05-03_ed-2.nuc1.{}.parquet", stamp(1)),
        format!("pages/SN1_1890-05-03_ed-2.nuc1.{}.parquet", stamp(1)),
    ] {
        assert!(checks::parse_part_name(&bad).is_none(), "{bad}");
    }
    assert_eq!(
        Reject::DeadLetter("v1 has no cause").code(),
        "deadletter_v1_has_no_cause"
    );
}

#[test]
fn rule_v1_on_stored_scores() {
    let s = |lang: &str, rate: f64, head: u32| pages::Scores {
        lang: Some(lang.into()),
        decision: Some("detected".into()),
        damage_rate: Some(rate),
        function_share: None,
        head: Some(head),
    };
    assert_eq!(
        report::verdict(&s("ger", 0.1, 50), &s("ger", 0.4, 40)),
        Ok(())
    );
    assert_eq!(
        report::verdict(&s("ger", 0.38, 50), &s("ger", 0.4, 40)),
        Err("margin")
    );
    assert_eq!(
        report::verdict(&s("ger", 0.1, 30), &s("ger", 0.4, 40)),
        Err("head")
    );
    assert_eq!(
        report::verdict(&s("eng", 0.1, 50), &s("ger", 0.4, 40)),
        Err("language_differs")
    );
    let und = pages::Scores {
        lang: Some("und".into()),
        ..Default::default()
    };
    assert_eq!(report::verdict(&s("ger", 0.1, 50), &und), Ok(()));
    assert_eq!(
        report::verdict(&und, &s("ger", 0.4, 40)),
        Err("ours_undecided")
    );
    assert_eq!(
        report::engine_key(
            "tesseract/5.5.3 profile=deu_latf+deu image=sha256:3f9a route=x@r1 contract=1"
        ),
        "tesseract/5.5.3 deu_latf+deu"
    );
    assert_eq!(
        report::engine_key("tesseract-5.5.1/deu"),
        "tesseract-5.5.1/deu"
    );
}

#[tokio::test]
async fn report_counts_the_imported_pages() {
    let fx = fixture(true).await;
    let doc = key(3).doc_id();
    fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
    fx.node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    fx.sync().await;
    let r = report::run(fx.stores.curated.as_ref()).await.unwrap();
    assert_eq!((r.pages.considered, r.pages.replaced), (1, 1), "{r:?}");
    assert_eq!(
        fx.stores
            .curated
            .list(&format!("{ROOT}/report"))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn every_import_gets_loc_scores_from_curated_text() {
    // No damage tables: LoC's scores come from the curated text, for each
    // import's own pages.
    let fx = fixture(false).await;
    fx.ask(&[
        queued(&key(3).doc_id(), BATCH, "bulk"),
        queued(&key(4).doc_id(), BATCH, "bulk"),
    ])
    .await;
    fx.node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    fx.node_writes(&row(4, &GERMAN.repeat(4)), ALTO, &stamp(2))
        .await;
    let opts = Options {
        chunk: 1,
        ..Options::default()
    };
    let status = sync::run(&fx.state, "test", &fx.stores, &opts)
        .await
        .unwrap();
    assert_eq!(status.last_run.imported, 2);
    let c = &fx.stores.curated;
    let mut rows = Vec::new();
    for p in c.list(&format!("{ROOT}/pages/{LCCN}")).await.unwrap() {
        let b = bytes::Bytes::from(c.get(&p).await.unwrap().unwrap());
        rows.extend(pages::read_scores(b).unwrap());
    }
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.loc.damage_rate.is_some()), "{rows:?}");
}

#[tokio::test]
async fn an_alto_already_imported_with_other_bytes_stops_the_run() {
    let fx = fixture(false).await;
    let doc = key(3).doc_id();
    fx.ask(&[queued(&doc, BATCH, "bulk")]).await;
    fx.node_writes(&row(3, &GERMAN.repeat(4)), ALTO, &stamp(1))
        .await;
    // An earlier run's copy, then the node rewrote the source.
    fx.stores
        .curated
        .put_new(
            &super::alto_path(&doc, NODE, &stamp(1)),
            b"<alto/>".to_vec(),
            "x",
        )
        .await
        .unwrap();
    let err = sync::run(&fx.state, "test", &fx.stores, &Options::default())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("other bytes"), "{err:#}");
    // Nothing was deleted.
    assert_eq!(fx.out_paths().await.len(), 2);
}
