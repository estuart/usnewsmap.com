//! The warm-up's searches from the search log never reach the console log.
//! One test in its own binary: it captures the console, and tests running
//! alongside it without a subscriber could change what tracing records.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing_subscriber::layer::SubscriberExt;
use usnm_api::config::Config;
use usnm_api::prewarm::{self, Trigger};
use usnm_api::refdata::RefData;
use usnm_api::searchlog::{day_path, LogConfig, Record, SearchLog};
use usnm_api::AppState;
use usnm_search::memory::MemoryBackend;
use usnm_store::{LocalStore, ObjectStore};

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data")
}

fn fixture_backend() -> MemoryBackend {
    let mut backend = MemoryBackend::new();
    for id in ["pages-base-fixture", "pages-delta-fixture-1"] {
        let path = data_dir().join("indexes").join(format!("{id}.jsonl"));
        let docs: Vec<_> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str::<usnm_search::PageDoc>(l).unwrap())
            .collect();
        backend.add_index(id, docs);
    }
    backend
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn record(q: &str, day: chrono::NaiveDate) -> Record {
    Record {
        v: 1,
        day,
        source: "api".into(),
        q: q.into(),
        query: q.into(),
        mode: None,
        near: None,
        fuzzy: 0,
        from: chrono::NaiveDate::from_ymd_opt(1896, 1, 1).unwrap(),
        to: chrono::NaiveDate::from_ymd_opt(1896, 12, 31).unwrap(),
        state: vec![],
        lccn: vec![],
        lang: vec![],
        front: false,
        corpus: None,
        series: Vec::new(),
        bucket: "month".into(),
        pages: Some(3),
        index_version: Some("fixture-v1".into()),
    }
}

#[tokio::test]
async fn searches_from_the_log_are_logged_by_rank_only() {
    let console = Captured::default();
    let subscriber = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer().json().with_writer({
            let c = console.clone();
            move || c.clone()
        }));
    let _guard = tracing::subscriber::set_default(subscriber);

    let dir = std::env::temp_dir().join(format!("usnm-prewarm-log-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Arc::new(LocalStore::new(&dir));
    let yesterday = chrono::Utc::now().date_naive().pred_opt().unwrap();
    // And one that no longer parses, which is left out.
    let lines: Vec<String> = ["zebrasecret", "cholera", "text:parensecret"]
        .iter()
        .map(|q| serde_json::to_string(&record(q, yesterday)).unwrap())
        .collect();
    store
        .put(
            &day_path(yesterday),
            lines.join("\n").into_bytes(),
            "application/x-ndjson",
        )
        .await
        .unwrap();
    let log = SearchLog::start(
        store,
        LogConfig {
            flush_interval: Duration::from_secs(3600),
            ..LogConfig::default()
        },
        &opentelemetry::global::meter("test"),
    );
    let mut cfg = Config::from_lookup(|_| None).unwrap();
    cfg.rate_limit = None;
    let refdata = RefData::load(&LocalStore::new(data_dir())).await.unwrap();
    let state = Arc::new(
        AppState::new(cfg, Arc::new(fixture_backend()), refdata).with_search_log(log.clone()),
    );
    let report = prewarm::run(&state, state.snapshot.load_full(), Trigger::Startup).await;
    // `text:parensecret` doesn't parse, so it never makes the list.
    assert_eq!(report.from_log, 2, "{report:?}");
    log.shutdown(Duration::from_secs(5)).await;

    let console = String::from_utf8(console.0.lock().unwrap().clone()).unwrap();
    assert!(console.contains("\"from_log\":2"), "{console}");
    for secret in ["zebrasecret", "parensecret", "cholera"] {
        assert!(
            !console.contains(secret),
            "`{secret}` was logged: {console}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
