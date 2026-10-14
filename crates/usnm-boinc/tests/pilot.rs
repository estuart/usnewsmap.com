use chrono::{NaiveDate, Utc};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use usnm_boinc::{ExportOptions, Stage};
use usnm_core::ids::PageKey;
use usnm_ingest::{
    curated::{CuratedRow, PartWriter},
    ocr_local::{ledger, sync},
};
use usnm_ocr_contract::{
    boinc::{self, Job, ResultManifest},
    prefetch::{self, ManifestRow},
};
use usnm_ocr_worker::{
    engine,
    loc::{ImageSize, ScanFormat},
    prefetch::{scan_path, sidecar_path, Sidecar},
    testimg,
};
use usnm_state::{
    docs::MemoryDocs,
    state::{Curated, RunBatch, State},
};
use usnm_store::{queue::LocalQueue, ObjectStore};
const IMAGE: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ALTO: &str = r#"<alto xmlns="http://www.loc.gov/standards/alto/ns-v3#"><Description><sourceImageInformation><fileName>page</fileName></sourceImageInformation></Description><Layout><Page WIDTH="1000" HEIGHT="1000"><PrintSpace><TextBlock><TextLine><String CONTENT="HELLO"/></TextLine></TextBlock></PrintSpace></Page></Layout></alto>"#;
struct Fixture {
    tmp: tempfile::TempDir,
    state: State,
    stores: sync::Stores,
    options: ExportOptions,
}
fn store(root: &Path, name: &str) -> Arc<dyn ObjectStore> {
    usnm_store::open(root.join(name).to_str().unwrap()).unwrap()
}
impl Fixture {
    async fn new(language: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let key = PageKey::new(
            "sn00000001",
            NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
            1,
            1,
        )
        .unwrap();
        let row = ManifestRow::new(
            &key,
            "test_pilot",
            1,
            Some(language.into()),
            "detected",
            0.5,
        );
        let scan = testimg::text_image("HELLO WORLD");
        let path = scan_path(&root.join("scans"), &row, ImageSize::Full, ScanFormat::Jpeg);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &scan).unwrap();
        let side = Sidecar {
            doc_id: key.doc_id(),
            batch: "test_pilot".into(),
            batch_version: 1,
            loc_url:
                "https://tile.loc.gov/image-services/iiif/service:ndnp:test/full/full/0/default.jpg"
                    .into(),
            iiif_base: "https://tile.loc.gov/image-services/iiif/service:ndnp:test".into(),
            resolution: "full".into(),
            fetched_at: Utc::now(),
            bytes: scan.len() as u64,
            sha256: boinc::sha256(&scan),
            download_ms: 1,
            user_agent: "synthetic".into(),
            format: "jpeg".into(),
            md5: None,
        };
        std::fs::write(sidecar_path(&path), serde_json::to_vec(&side).unwrap()).unwrap();
        std::fs::write(
            root.join("manifest.parquet"),
            prefetch::encode(&[row]).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join("pages.txt"), key.to_string()).unwrap();
        let curated = store(root, "curated");
        let reference = store(root, "reference");
        let mut writer = PartWriter::new();
        writer
            .push(CuratedRow::from_ocr(
                key,
                "Old text",
                "test_pilot",
                1,
                "loc",
                Utc::now(),
            ))
            .unwrap();
        curated
            .put_new(
                "pages/test_pilot_ver01/part-0.parquet",
                writer.finish_part().unwrap().unwrap(),
                "x",
            )
            .await
            .unwrap();
        let batch = RunBatch {
            batch: "test_pilot".into(),
            curated: Curated {
                version: 1,
                parts: vec!["pages/test_pilot_ver01/part-0.parquet".into()],
                counts: String::new(),
                pages: 1,
                ok_pages: 1,
                lccns: vec!["sn00000001".into()],
                source_sha256: String::new(),
                first: String::new(),
                last: String::new(),
            },
        };
        reference
            .put_new(
                "v1/batches.json",
                serde_json::to_vec(&[batch]).unwrap(),
                "x",
            )
            .await
            .unwrap();
        reference
            .put_new(
                "v1/titles.json",
                serde_json::to_vec(
                    &serde_json::json!([{"lccn":"sn00000001","languages":[language]}]),
                )
                .unwrap(),
                "x",
            )
            .await
            .unwrap();
        let state = State::new(Arc::new(MemoryDocs::default()));
        state.set_current_version("v1").await.unwrap();
        let stores = sync::Stores {
            curated,
            reference,
            out: store(root, "out"),
            quarantine: store(root, "quarantine"),
            queue: Arc::new(LocalQueue::new(root.join("queue"))),
        };
        let options = ExportOptions {
            manifest: root.join("manifest.parquet"),
            pages: root.join("pages.txt"),
            batch: "test_pilot".into(),
            scans: root.join("scans"),
            root: root.join("dispatch"),
            plan: "pilot-test".into(),
            image: IMAGE.into(),
            timeout_secs: 600,
        };
        Self {
            tmp,
            state,
            stores,
            options,
        }
    }
    // Selected pages have cached scans; unrelated manifest rows deliberately
    // have neither caches nor a supported language.
    fn cohort(&self, selected: u16, unrelated: u16) {
        let base = prefetch::decode(bytes::Bytes::from(
            std::fs::read(&self.options.manifest).unwrap(),
        ))
        .unwrap()
        .remove(0);
        let source = scan_path(
            &self.options.scans,
            &base,
            ImageSize::Full,
            ScanFormat::Jpeg,
        );
        let scan = std::fs::read(&source).unwrap();
        let side: Sidecar =
            serde_json::from_slice(&std::fs::read(sidecar_path(&source)).unwrap()).unwrap();
        let mut rows = Vec::new();
        let mut pages = Vec::new();
        for seq in 1..=selected + unrelated {
            let key = PageKey::new(&base.lccn, base.date, base.edition, seq).unwrap();
            let row = ManifestRow::new(
                &key,
                &base.batch,
                base.batch_version,
                Some(if seq <= selected { "eng" } else { "jpn" }.into()),
                "detected",
                0.5,
            );
            if seq <= selected {
                let path = scan_path(&self.options.scans, &row, ImageSize::Full, ScanFormat::Jpeg);
                std::fs::write(&path, &scan).unwrap();
                let mut side = side.clone();
                side.doc_id = key.doc_id();
                std::fs::write(sidecar_path(&path), serde_json::to_vec(&side).unwrap()).unwrap();
                pages.push(key.to_string());
            }
            rows.push(row);
        }
        std::fs::write(&self.options.manifest, prefetch::encode(&rows).unwrap()).unwrap();
        std::fs::write(&self.options.pages, pages.join("\n")).unwrap();
    }
    async fn export(&self) -> String {
        usnm_boinc::export(
            &self.options,
            &self.state,
            self.stores.curated.as_ref(),
            self.stores.reference.as_ref(),
        )
        .await
        .unwrap()
        .remove(0)
    }
    fn result(&self, id: &str) -> (PathBuf, PathBuf, PathBuf) {
        let job = usnm_boinc::load(&self.options.root, id).unwrap().job;
        let text = b"HELLO WORLD";
        let result = ResultManifest {
            v: 1,
            job_id: id.into(),
            scan_sha256: job.scan.sha256,
            engine: job.engine,
            text_sha256: boinc::sha256(text),
            alto_sha256: boinc::sha256(ALTO.as_bytes()),
            elapsed_ms: 100,
            completed_at: Utc::now().to_rfc3339(),
        };
        let root = self.tmp.path().join("returned");
        std::fs::create_dir_all(&root).unwrap();
        let manifest = root.join("result.json");
        let txt = root.join("text.txt");
        let alto = root.join("layout.xml");
        std::fs::write(&manifest, serde_json::to_vec(&result).unwrap()).unwrap();
        std::fs::write(&txt, text).unwrap();
        std::fs::write(&alto, ALTO).unwrap();
        (manifest, txt, alto)
    }
    async fn import(&self, id: &str, files: &(PathBuf, PathBuf, PathBuf)) -> anyhow::Result<()> {
        usnm_boinc::import(
            &self.options.root,
            id,
            &files.0,
            &files.1,
            &files.2,
            self.stores.curated.as_ref(),
            self.stores.out.as_ref(),
        )
        .await
    }
}
#[tokio::test]
async fn round_trip_through_native_ocr_sync_and_duplicate_import() {
    let fx = Fixture::new("ger").await;
    let id = fx.export().await;
    assert_eq!(fx.export().await, id); // Stable identity and reservation replay.
    let r = usnm_boinc::load(&fx.options.root, &id).unwrap();
    assert_eq!(r.job.engine.profile, "frak2021");
    let files = fx.result(&id);
    fx.import(&id, &files).await.unwrap();
    fx.import(&id, &files).await.unwrap();
    assert_eq!(fx.stores.out.list("pages").await.unwrap().len(), 1);
    let status = sync::run(
        &fx.state,
        "test-boinc",
        &fx.stores,
        &sync::Options::default(),
    )
    .await
    .unwrap();
    assert_eq!(status.last_run.imported, 1);
    assert!(status.last_run.rejected.is_empty());
    let mut pending = usnm_boinc::load(&fx.options.root, &id).unwrap();
    pending.stage = Stage::Submitted;
    usnm_boinc::save(&fx.options.root, &pending).unwrap(); // Lost local commit, but native import is durable.
    fx.import(&id, &files).await.unwrap(); // No re-publication after sync consumes it.
    assert!(fx.stores.out.list("pages").await.unwrap().is_empty());
    assert_eq!(
        ledger::load(fx.stores.curated.as_ref())
            .await
            .unwrap()
            .outcomes[&r.job.message.key().unwrap().doc_id()]
            .outcome,
        ledger::IMPORTED
    );
}
#[tokio::test]
async fn wrong_identity_corrupt_output_and_malformed_alto_never_publish() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let files = fx.result(&id);
    let mut m: ResultManifest = serde_json::from_slice(&std::fs::read(&files.0).unwrap()).unwrap();
    m.job_id = "b".repeat(64);
    std::fs::write(&files.0, serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(fx.import(&id, &files).await.is_err());
    let files = fx.result(&id);
    std::fs::write(&files.1, "Changed text").unwrap();
    assert!(fx.import(&id, &files).await.is_err());
    let files = fx.result(&id);
    let invalid = b"<!DOCTYPE alto><alto/>";
    std::fs::write(&files.2, invalid).unwrap();
    let mut m: ResultManifest = serde_json::from_slice(&std::fs::read(&files.0).unwrap()).unwrap();
    m.alto_sha256 = boinc::sha256(invalid);
    std::fs::write(&files.0, serde_json::to_vec(&m).unwrap()).unwrap();
    assert!(fx.import(&id, &files).await.is_err());
    assert!(fx.stores.out.list("pages").await.unwrap().is_empty());
}
#[tokio::test]
async fn refuses_prior_native_work_and_changed_scan() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let mut other = fx.options.clone();
    other.root = fx.tmp.path().join("other");
    other.plan = "another-plan".into();
    assert!(usnm_boinc::export(
        &other,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    let r = usnm_boinc::load(&fx.options.root, &id).unwrap();
    let mut job: Job = r.job.clone();
    job.scan.sha256 = "b".repeat(64);
    assert!(job.validate().is_err());
    let path = fx.options.root.join("jobs").join(id).join("request.json");
    assert!(path.exists());
}
#[tokio::test]
async fn restart_after_alto_publish_and_lost_journal_commit() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let files = fx.result(&id);
    let record = usnm_boinc::load(&fx.options.root, &id).unwrap();
    let (alto_path, part_path) = record.paths().unwrap();
    let alto = engine::set_alto_source(ALTO, &record.job.scan.url, &record.job.scan.sha256, None);
    fx.stores
        .out
        .put_new(&alto_path, alto.into_bytes(), "x")
        .await
        .unwrap();
    fx.import(&id, &files).await.unwrap();
    let mut record = usnm_boinc::load(&fx.options.root, &id).unwrap();
    record.stage = Stage::Submitted;
    usnm_boinc::save(&fx.options.root, &record).unwrap();
    fx.import(&id, &files).await.unwrap();
    assert_eq!(fx.stores.out.list("pages").await.unwrap(), vec![part_path]);
}
#[tokio::test]
async fn uncertain_submission_is_not_retried_and_requires_reconciliation() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let project = fx.tmp.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    assert!(usnm_boinc::submit::submit(&fx.options.root, &project, &id)
        .await
        .is_err());
    assert_eq!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Submitting
    );
    assert!(usnm_boinc::submit::submit(&fx.options.root, &project, &id)
        .await
        .is_err());
    usnm_boinc::submit::reconcile(&fx.options.root, &id, false).unwrap();
    assert_eq!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Reserved
    );
}
#[test]
fn bounded_files_and_mutually_exclusive_bridge_processes() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("large");
    std::fs::write(&file, b"12345").unwrap();
    assert!(usnm_boinc::read(&file, 4).is_err());
    let lock = usnm_boinc::Lock::take(root.path()).unwrap();
    assert!(usnm_boinc::Lock::take(root.path()).is_err());
    drop(lock);
    assert!(usnm_boinc::Lock::take(root.path()).is_ok());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&file, root.path().join("link")).unwrap();
        assert!(usnm_boinc::read(&root.path().join("link"), 10).is_err());
    }
}

#[tokio::test]
async fn stock_spool_and_terminal_failure_are_recognized() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let files = fx.result(&id);
    let record = usnm_boinc::load(&fx.options.root, &id).unwrap();
    let spool = fx.tmp.path().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    for (source, logical) in [
        (&files.0, "result.json"),
        (&files.1, "text.txt"),
        (&files.2, "layout.xml"),
    ] {
        std::fs::copy(
            source,
            spool.join(format!("{}__file_{logical}", record.job.workunit())),
        )
        .unwrap();
    }
    usnm_boinc::collect(
        &fx.options.root,
        &spool,
        &id,
        fx.stores.curated.as_ref(),
        fx.stores.out.as_ref(),
    )
    .await
    .unwrap();
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let workunit = usnm_boinc::load(&fx.options.root, &id)
        .unwrap()
        .job
        .workunit();
    std::fs::write(spool.join(format!("{workunit}_error")), "0x1").unwrap();
    assert!(usnm_boinc::collect(
        &fx.options.root,
        &spool,
        &id,
        fx.stores.curated.as_ref(),
        fx.stores.out.as_ref()
    )
    .await
    .is_err());
    assert_eq!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Failed
    );
    assert!(usnm_boinc::submit::reconcile(&fx.options.root, &id, false).is_err());
    usnm_boinc::submit::reconcile(&fx.options.root, &id, true).unwrap();
    assert_eq!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Submitted
    );
}

#[tokio::test]
async fn unpublished_batch_and_japanese_titles_are_not_reserved() {
    let fx = Fixture::new("eng").await;
    std::fs::write(fx.tmp.path().join("reference/v1/batches.json"), b"[]").unwrap();
    assert!(usnm_boinc::export(
        &fx.options,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    assert!(ledger::load(fx.stores.curated.as_ref())
        .await
        .unwrap()
        .queued
        .is_empty());
    let fx = Fixture::new("eng").await;
    std::fs::write(
        fx.tmp.path().join("reference/v1/titles.json"),
        br#"[{"lccn":"sn00000001","languages":["jpn"]}]"#,
    )
    .unwrap();
    assert!(usnm_boinc::export(
        &fx.options,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    assert!(ledger::load(fx.stores.curated.as_ref())
        .await
        .unwrap()
        .queued
        .is_empty());
}

#[tokio::test]
async fn changed_cached_scan_and_oversized_return_are_rejected() {
    let fx = Fixture::new("eng").await;
    let rows = prefetch::decode(bytes::Bytes::from(
        std::fs::read(&fx.options.manifest).unwrap(),
    ))
    .unwrap();
    let path = scan_path(
        &fx.options.scans,
        &rows[0],
        ImageSize::Full,
        ScanFormat::Jpeg,
    );
    std::fs::write(path, b"changed cached bytes").unwrap();
    assert!(usnm_boinc::export(
        &fx.options,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    assert!(ledger::load(fx.stores.curated.as_ref())
        .await
        .unwrap()
        .queued
        .is_empty());
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let files = fx.result(&id);
    std::fs::write(&files.1, vec![b'a'; boinc::MAX_TEXT + 1]).unwrap();
    assert!(fx.import(&id, &files).await.is_err());
    assert!(fx.stores.out.list("pages").await.unwrap().is_empty());
}

#[derive(Debug)]
struct ObservedStore {
    inner: Arc<dyn ObjectStore>,
    calls: Mutex<Vec<String>>,
    lose_write_response: AtomicBool,
}
impl ObservedStore {
    fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
            lose_write_response: AtomicBool::new(false),
        }
    }
    fn count(&self, call: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|s| *s == call)
            .count()
    }
}
#[async_trait::async_trait]
impl ObjectStore for ObservedStore {
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, usnm_store::StoreError> {
        self.calls.lock().unwrap().push(format!("get:{path}"));
        self.inner.get(path).await
    }
    async fn put_new(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<bool, usnm_store::StoreError> {
        self.calls.lock().unwrap().push(format!("put_new:{path}"));
        let written = self.inner.put_new(path, body, content_type).await?;
        if path.starts_with("ocr-local/queued/")
            && written
            && self.lose_write_response.swap(false, Ordering::SeqCst)
        {
            return Err(usnm_store::StoreError::Io(
                "lost reservation response".into(),
            ));
        }
        Ok(written)
    }
    async fn put(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<(), usnm_store::StoreError> {
        self.inner.put(path, body, content_type).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<String>, usnm_store::StoreError> {
        self.calls.lock().unwrap().push(format!("list:{prefix}"));
        self.inner.list(prefix).await
    }
}

#[tokio::test]
async fn cohort_filters_unrelated_pages_and_reads_snapshot_and_ledger_once() {
    let fx = Fixture::new("eng").await;
    fx.cohort(3, 1000);
    let curated = ObservedStore::new(fx.stores.curated.clone());
    let reference = ObservedStore::new(fx.stores.reference.clone());
    let ids = usnm_boinc::export(&fx.options, &fx.state, &curated, &reference)
        .await
        .unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(reference.count("get:v1/batches.json"), 1);
    assert_eq!(reference.count("get:v1/titles.json"), 1);
    assert_eq!(reference.count("get:catalog/titles.json"), 0);
    assert_eq!(curated.count("list:ocr-local/queued"), 1);
    assert_eq!(curated.count("list:ocr-local/outcomes"), 1);
    assert_eq!(
        fx.stores
            .curated
            .list("ocr-local/queued")
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ledger::load(fx.stores.curated.as_ref())
            .await
            .unwrap()
            .queued
            .len(),
        3
    );
    for id in ids {
        assert_eq!(
            usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
            Stage::Reserved
        );
    }
}

#[tokio::test]
async fn undetected_language_uses_published_title_fallback() {
    for detected in [None, Some("und".into())] {
        let fx = Fixture::new("ger").await;
        let mut rows = prefetch::decode(bytes::Bytes::from(
            std::fs::read(&fx.options.manifest).unwrap(),
        ))
        .unwrap();
        rows[0].lang = detected;
        std::fs::write(&fx.options.manifest, prefetch::encode(&rows).unwrap()).unwrap();
        let id = fx.export().await;
        let record = usnm_boinc::load(&fx.options.root, &id).unwrap();
        assert_eq!(record.job.message.language, "deu");
        assert_eq!(record.job.engine.profile, "frak2021");
        assert_eq!(
            ledger::load(fx.stores.curated.as_ref())
                .await
                .unwrap()
                .queued[&rows[0].doc_id]
                .language,
            "deu"
        );
    }
}

#[tokio::test]
async fn cohort_reservation_replays_lost_response_and_partial_journal_updates() {
    let fx = Fixture::new("eng").await;
    fx.cohort(3, 0);
    let curated = ObservedStore::new(fx.stores.curated.clone());
    curated.lose_write_response.store(true, Ordering::SeqCst);
    assert!(usnm_boinc::export(
        &fx.options,
        &fx.state,
        &curated,
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    let prepared = usnm_boinc::records(&fx.options.root).unwrap();
    assert_eq!(prepared.len(), 3);
    assert!(prepared.iter().all(|r| r.stage == Stage::Prepared));
    let mut first = prepared[0].clone();
    first.stage = Stage::Reserved; // Simulate one journal update succeeding before a crash.
    usnm_boinc::save(&fx.options.root, &first).unwrap();
    let ids = usnm_boinc::export(
        &fx.options,
        &fx.state,
        &curated,
        fx.stores.reference.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        fx.stores
            .curated
            .list("ocr-local/queued")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(usnm_boinc::records(&fx.options.root)
        .unwrap()
        .iter()
        .all(|r| r.stage == Stage::Reserved));
    for old in prepared {
        assert_eq!(
            usnm_boinc::load(&fx.options.root, &old.job.id)
                .unwrap()
                .queued()
                .unwrap(),
            old.queued().unwrap()
        );
    }
}

#[tokio::test]
async fn conflicting_cohort_does_not_reserve_its_fresh_pages_and_unlocks() {
    let fx = Fixture::new("eng").await;
    fx.cohort(2, 0);
    // Only the second page has prior work: the first row of the next cohort
    // is fresh, so a per-page writer would incorrectly reserve it before failing.
    let second = std::fs::read_to_string(&fx.options.pages)
        .unwrap()
        .lines()
        .nth(1)
        .unwrap()
        .to_owned();
    std::fs::write(&fx.options.pages, second).unwrap();
    fx.export().await;
    fx.cohort(3, 0);
    let mut options = fx.options.clone();
    options.root = fx.tmp.path().join("another-dispatch");
    options.plan = "other-plan".into();
    assert!(usnm_boinc::export(
        &options,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref()
    )
    .await
    .is_err());
    assert_eq!(
        ledger::load(fx.stores.curated.as_ref())
            .await
            .unwrap()
            .queued
            .len(),
        1
    );
    // Returning to the original plan proves the rejected cohort released its lease.
    usnm_boinc::export(
        &fx.options,
        &fx.state,
        fx.stores.curated.as_ref(),
        fx.stores.reference.as_ref(),
    )
    .await
    .unwrap();
    assert_eq!(
        ledger::load(fx.stores.curated.as_ref())
            .await
            .unwrap()
            .queued
            .len(),
        3
    );
}

#[tokio::test]
async fn snapshot_change_prevents_reservation() {
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let snapshot =
        usnm_ingest::ocr_local::boinc::Snapshot::load(&fx.state, fx.stores.reference.as_ref())
            .await
            .unwrap();
    let row = usnm_boinc::load(&fx.options.root, &id)
        .unwrap()
        .queued()
        .unwrap();
    fx.state.set_current_version("v2").await.unwrap();
    let error = usnm_ingest::ocr_local::boinc::reserve(
        &fx.state,
        fx.stores.curated.as_ref(),
        &snapshot,
        &[row],
        "test-changed",
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("snapshot changed"));
    assert_eq!(
        fx.stores
            .curated
            .list("ocr-local/queued")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn snapshot_rejects_unsafe_version_before_reading_reference_paths() {
    for version in ["v1/other", "../v1", "", "/v1", "v1?other"] {
        let fx = Fixture::new("eng").await;
        fx.state.set_current_version(version).await.unwrap();
        let reference = ObservedStore::new(fx.stores.reference.clone());
        let error = usnm_boinc::export(
            &fx.options,
            &fx.state,
            fx.stores.curated.as_ref(),
            &reference,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("invalid published version"),
            "{error:#}"
        );
        assert!(reference.calls.lock().unwrap().is_empty());
        assert!(ledger::load(fx.stores.curated.as_ref())
            .await
            .unwrap()
            .queued
            .is_empty());
    }
}

#[tokio::test]
async fn unpublished_catalog_title_cannot_route_or_reserve_a_page() {
    let fx = Fixture::new("eng").await;
    fx.stores
        .reference
        .put("v1/titles.json", b"[]".to_vec(), "application/json")
        .await
        .unwrap();
    fx.stores
        .reference
        .put_new(
            "catalog/titles.json",
            br#"[{"lccn":"sn00000001","languages":["eng"]}]"#.to_vec(),
            "application/json",
        )
        .await
        .unwrap();
    let reference = ObservedStore::new(fx.stores.reference.clone());
    let error = usnm_boinc::export(
        &fx.options,
        &fx.state,
        fx.stores.curated.as_ref(),
        &reference,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("title is not published"),
        "{error:#}"
    );
    assert_eq!(reference.count("get:catalog/titles.json"), 0);
    assert!(ledger::load(fx.stores.curated.as_ref())
        .await
        .unwrap()
        .queued
        .is_empty());

    // The native helper must enforce the same rule independently of export.
    fx.stores
        .reference
        .put(
            "v1/titles.json",
            br#"[{"lccn":"sn00000001","languages":["eng"]}]"#.to_vec(),
            "application/json",
        )
        .await
        .unwrap();
    let id = fx.export().await;
    let row = usnm_boinc::load(&fx.options.root, &id)
        .unwrap()
        .queued()
        .unwrap();
    fx.stores
        .reference
        .put("v1/titles.json", b"[]".to_vec(), "application/json")
        .await
        .unwrap();
    let snapshot = usnm_ingest::ocr_local::boinc::Snapshot::load(&fx.state, &reference)
        .await
        .unwrap();
    let error = usnm_ingest::ocr_local::boinc::reserve(
        &fx.state,
        fx.stores.curated.as_ref(),
        &snapshot,
        &[row],
        "unpublished-title",
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("title is not published"),
        "{error:#}"
    );
    assert_eq!(
        fx.stores
            .curated
            .list("ocr-local/queued")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[cfg(unix)]
mod download_permissions {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn executable(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn shell_path(path: &Path) -> String {
        format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
    }
    fn python_path(path: &Path) -> String {
        // A JSON string is also a Python string literal; this is not shell quoting.
        serde_json::to_string(path.to_str().unwrap()).unwrap()
    }
    /// `fanout` maps inputs two not-yet-created levels below `download/`, like a
    /// new hierarchy bucket, so their creation under the private umask is tested.
    fn project(fx: &Fixture, id: &str, fail_staging: bool, fanout: bool) -> PathBuf {
        let project = fx.tmp.path().join("BOINC project's files");
        std::fs::create_dir_all(project.join("bin")).unwrap();
        std::fs::create_dir_all(project.join("download")).unwrap();
        let new_dirs = if fanout {
            vec![
                project.join("download/fan"),
                project.join("download/fan/3a"),
            ]
        } else {
            Vec::new()
        };
        let download = new_dirs.last().cloned().unwrap_or(project.join("download"));
        let mapping = if fail_staging {
            let blocker = project.join("not-a-directory");
            std::fs::write(&blocker, b"block parent creation").unwrap();
            format!(
                "case \"$*\" in *_request_json) printf '%s\\n' {} ;; *) printf '%s/%s\\n' {} \"$*\" ;; esac",
                shell_path(&blocker.join("request")), shell_path(&download)
            )
        } else {
            format!("printf '%s/%s\\n' {} \"$*\"", shell_path(&download))
        };
        // These fixtures implement the stock tools' interfaces, not new project CLIs.
        executable(
            &project.join("bin/dir_hier_path"),
            &format!("#!/bin/sh\n{mapping}\n"),
        );
        executable(
            &project.join("bin/create_work"),
            &format!(
                r#"#!/usr/bin/env python3
import json, pathlib, stat
source = pathlib.Path({source})
download = pathlib.Path({download})
record = source / "record.json"
assert json.loads(record.read_text())["stage"] == "submitting"
assert stat.S_IMODE(record.stat().st_mode) & 0o077 == 0
for logical, physical in [("page", "{id}_page"), ("request.json", "{id}_request_json")]:
    public = download / physical
    mode = stat.S_IMODE(public.stat().st_mode)
    assert mode & stat.S_IROTH, "web-server identity cannot read staged input"
    assert mode & (stat.S_IWGRP | stat.S_IWOTH) == 0, "public input is writable by another identity"
    assert public.read_bytes() == (source / logical).read_bytes()
    assert stat.S_IMODE((source / logical).stat().st_mode) & 0o077 == 0
for d in {new_dirs}:
    assert stat.S_IMODE(pathlib.Path(d).stat().st_mode) == 0o755, d
pathlib.Path({marker}).write_text("inputs readable when create_work started")
"#,
                source = python_path(&fx.options.root.join("jobs").join(id)),
                download = python_path(&download),
                marker = python_path(&project.join("dispatched")),
                new_dirs = serde_json::to_string(&new_dirs).unwrap(),
            ),
        );
        project
    }
    async fn submit_with_private_umask(
        fx: &Fixture,
        project: &Path,
        id: &str,
    ) -> std::process::Output {
        // Change only the child process's umask; parallel Rust tests keep theirs.
        tokio::process::Command::new("sh")
            .args(["-c", "umask 077; exec \"$@\"", "boinc-submit"])
            .arg(env!("CARGO_BIN_EXE_usnm-boinc"))
            .arg("submit")
            .arg("--root")
            .arg(&fx.options.root)
            .arg("--project")
            .arg(project)
            .arg("--job")
            .arg(id)
            .output()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn downloads_are_readable_at_dispatch_with_private_umask_and_replacement() {
        for replace in [false, true] {
            let fx = Fixture::new("eng").await;
            let id = fx.export().await;
            let project = project(&fx, &id, false, false);
            if replace {
                for suffix in ["page", "request_json"] {
                    let path = project.join("download").join(format!("{id}_{suffix}"));
                    usnm_boinc::atomic(&path, b"old private input").unwrap();
                }
            }
            let output = submit_with_private_umask(&fx, &project, &id).await;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(project.join("dispatched").exists());
            assert_eq!(
                usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
                Stage::Submitted
            );
            for name in ["record.json", "request.json", "page"] {
                let mode = std::fs::metadata(fx.options.root.join("jobs").join(&id).join(name))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o077, 0, "{name} became readable outside its owner");
            }
        }
    }

    #[tokio::test]
    async fn new_fanout_dirs_are_traversable_with_private_umask() {
        let fx = Fixture::new("eng").await;
        let id = fx.export().await;
        let project = project(&fx, &id, false, true);
        assert!(!project.join("download/fan").exists());
        let output = submit_with_private_umask(&fx, &project, &id).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(project.join("dispatched").exists());
        for name in ["page", "request_json"] {
            let file = project.join(format!("download/fan/3a/{id}_{name}"));
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o7777, 0o644);
        }
    }

    #[tokio::test]
    async fn staging_failure_never_dispatches_or_exposes_the_journal() {
        let fx = Fixture::new("eng").await;
        let id = fx.export().await;
        let project = project(&fx, &id, true, false);
        let output = submit_with_private_umask(&fx, &project, &id).await;
        assert!(!output.status.success());
        assert!(!project.join("dispatched").exists());
        assert_eq!(
            usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
            Stage::Submitting
        );
        let mode = std::fs::metadata(fx.options.root.join("jobs").join(&id).join("record.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn server_wrapper_keeps_updated_journals_private_and_propagates_submit_exit() {
        for exit_code in [0, 37] {
            let tmp = tempfile::tempdir().unwrap();
            let project = tmp.path().join("project");
            let handoff = tmp.path().join("handoff");
            let bin = tmp.path().join("bin");
            for dir in [&project, &handoff, &bin] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(project.join(".usnm-initialised"), b"fixture").unwrap();
            let library = tmp.path().join("lib.sh");
            std::fs::write(&library, format!(
                "PROJ={}\nfetch_secrets() {{ :; }}\nrender_config() {{ :; }}\nfail() {{ exit 1; }}\n",
                shell_path(&project)
            )).unwrap();
            let journal = handoff.join("record.json");
            executable(
                &bin.join("usnm-boinc"),
                &format!(
                    r#"#!/usr/bin/env python3
import argparse, os, sys
parser = argparse.ArgumentParser()
submit = parser.add_subparsers(dest="command", required=True).add_parser("submit")
submit.add_argument("--root", required=True)
submit.add_argument("--project", required=True)
submit.add_argument("--job", required=True)
args = parser.parse_args()
assert (args.root, args.project, args.job) == ({handoff}, {project}, "fixture")
fd = os.open({journal}, os.O_CREAT | os.O_WRONLY | os.O_TRUNC, 0o600)
with os.fdopen(fd, "w") as f: f.write("updated private journal")
sys.exit({exit_code})
"#,
                    handoff = python_path(&handoff),
                    project = python_path(&project),
                    journal = python_path(&journal)
                ),
            );
            // The previous wrapper's chmod ran even if its chgrp target was unavailable.
            // Supply the dependency so this regression does not depend on host groups.
            executable(&bin.join("chgrp"), "#!/bin/sh\nexit 0\n");
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../contrib-compute/images/server/bridge-job.sh");
            let script = std::fs::read_to_string(source).unwrap();
            let include = ". /usr/local/lib/usnm-boinc/lib.sh";
            assert!(script.contains(include));
            let wrapper = tmp.path().join("bridge-job.sh");
            let replacement = format!(". {}", shell_path(&library));
            let patched = script.replace(include, &replacement);
            assert!(!patched.contains(include) && patched.contains(&replacement));
            std::fs::write(&wrapper, patched).unwrap();
            let output = std::process::Command::new("bash")
                .arg(&wrapper)
                .env(
                    "PATH",
                    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()),
                )
                .env("JOB_ID", "fixture")
                .env("HANDOFF_DIR", &handoff)
                .env("BRIDGE_STEP", "submit")
                .output()
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(exit_code),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::metadata(&journal).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }
}

#[tokio::test]
async fn untrusted_host_is_held_back_until_it_passes_gold() {
    use usnm_boinc::trust::{quarantine_dir, Trust};
    let fx = Fixture::new("eng").await;
    let id = fx.export().await;
    let files = fx.result(&id);
    let record = usnm_boinc::load(&fx.options.root, &id).unwrap();
    let spool = fx.tmp.path().join("spool");
    std::fs::create_dir_all(&spool).unwrap();
    for (source, logical) in [
        (&files.0, "result.json"),
        (&files.1, "text.txt"),
        (&files.2, "layout.xml"),
    ] {
        std::fs::copy(
            source,
            spool.join(format!("{}__file_{logical}", record.job.workunit())),
        )
        .unwrap();
    }
    let mut trust = Trust {
        hosts: Default::default(),
        max_cer: 0.1,
        v: usnm_boinc::trust::TRUST_VERSION,
    };
    let run = |t: Trust| {
        let (root, spool, id) = (fx.options.root.clone(), spool.clone(), id.clone());
        let stores = &fx.stores;
        async move {
            usnm_boinc::trust::collect(
                &root,
                &spool,
                &id,
                &t,
                Some(42),
                stores.curated.as_ref(),
                stores.out.as_ref(),
            )
            .await
        }
    };
    assert!(run(trust.clone()).await.is_err());
    let q = quarantine_dir(&fx.options.root, &id);
    assert!(q.join("reason.json").exists() && q.join("result.json").exists());
    assert_ne!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Published
    );
    trust
        .hosts
        .entry(record.job.engine.image.clone())
        .or_default()
        .insert(42);
    run(trust).await.unwrap();
    assert_eq!(
        usnm_boinc::load(&fx.options.root, &id).unwrap().stage,
        Stage::Published
    );
}
