//! The searcher sidecar's cache, thread pool and runtime metrics (#125,
//! #251): what reaches Application Insights (a fake ingestion endpoint) from
//! each scrape, and what is logged while the sidecar is down.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use opentelemetry::metrics::MeterProvider as _;
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt;
use usnm_api::searcher_metrics::{self, Instruments, Scraper, HEARTBEAT};
use usnm_api::telemetry::SERVICE;
use usnm_search::cache_metrics::{Cache, CacheStats};
use usnm_search::quickwit::{QuickwitBackend, SearcherMetrics};
use usnm_search::thread_metrics::{Pool, PoolTasks, Runtime, RuntimeStats, ThreadReport};
use usnm_telemetry::testing::{fake_ingestion, Plain, Seen};

/// Console output captured from the JSON log layer.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn count(&self, message: &str) -> usize {
        self.text()
            .lines()
            .filter(|l| serde_json::from_str::<Value>(l).unwrap()["fields"]["message"] == message)
            .count()
    }
}

fn subscriber(console: &Captured) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new("info"))
        .with(tracing_subscriber::fmt::layer().json().with_writer({
            let c = console.clone();
            move || c.clone()
        }))
}

/// The searcher metrics in uploads `from..`: `(name, attributes, value)`,
/// sorted. The attributes are the values of `cache`, `pool`, `state` and
/// `runtime` that the row has, joined by spaces.
fn exported(seen: &Seen, from: usize) -> Vec<(String, String, f64)> {
    let mut out: Vec<_> = seen.uploads()[from..]
        .iter()
        .flat_map(|u| match serde_json::from_str::<Value>(&u.json).unwrap() {
            Value::Array(items) => items,
            other => vec![other],
        })
        .filter(|e| e["data"]["baseType"] == "MetricData")
        .flat_map(|e| {
            let d = &e["data"]["baseData"];
            let attrs = ["cache", "pool", "state", "runtime"]
                .iter()
                .filter_map(|k| d["properties"][k].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            d["metrics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|m| m["name"].as_str().unwrap().starts_with("api.searcher_"))
                .map(|m| {
                    (
                        m["name"].as_str().unwrap().to_owned(),
                        attrs.clone(),
                        m["value"].as_f64().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        })
        .collect();
    out.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    out
}

/// The cache metrics among [`exported`]: the pools are recorded at every
/// scrape.
fn cache_rows(seen: &Seen, from: usize) -> Vec<(String, String, f64)> {
    exported(seen, from)
        .into_iter()
        .filter(|r| r.0.starts_with("api.searcher_cache_"))
        .collect()
}

fn row(name: &str, cache: &str, value: f64) -> (String, String, f64) {
    row_of(&format!("cache_{name}"), cache, value)
}

fn row_of(name: &str, attrs: &str, value: f64) -> (String, String, f64) {
    (format!("api.searcher_{name}"), attrs.to_owned(), value)
}

fn report(entries: &[(Cache, CacheStats)]) -> SearcherMetrics {
    SearcherMetrics {
        caches: entries.iter().copied().collect(),
        threads: ThreadReport::default(),
    }
}

fn s(bytes: u64, items: u64, hits: u64, misses: u64, evictions: u64) -> CacheStats {
    CacheStats {
        bytes,
        items,
        hits,
        misses,
        evictions,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrapes_become_a_few_metrics_and_quiet_logs() {
    let (endpoint, seen) = fake_ingestion().await;
    let conn = format!(
        "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint={endpoint}"
    );
    let (tracer, meter_provider) =
        usnm_telemetry::providers(&conn, Plain::with_token("tok"), SERVICE).unwrap();
    let meter_provider = Arc::new(meter_provider);
    let meter = meter_provider.meter(SERVICE.name);
    let console = Captured::default();
    let guard = tracing::subscriber::set_default(subscriber(&console));
    // Exports what was recorded since the last flush; returns the uploads
    // seen before it.
    let flush = || {
        let p = meter_provider.clone();
        let before = seen.uploads().len();
        async move {
            tokio::task::spawn_blocking(move || p.force_flush().unwrap())
                .await
                .unwrap();
            before
        }
    };

    let mut scraper = Scraper::new(Instruments::new(&meter));
    let t0 = Instant::now();
    let a = report(&[
        (Cache::SplitFooter, s(17_220, 2, 4, 2, 0)),
        (Cache::FastField, s(663, 4, 0, 4, 0)),
    ]);

    // The first scrape: both gauges of each cache, and everything since the
    // searcher started. Zero counts aren't sent.
    assert!(!scraper.observe(Ok(a.clone()), t0));
    let from = flush().await;
    assert_eq!(
        cache_rows(&seen, from),
        vec![
            row("bytes", "fast_field", 663.0),
            row("bytes", "split_footer", 17_220.0),
            row("hits", "split_footer", 4.0),
            row("items", "fast_field", 4.0),
            row("items", "split_footer", 2.0),
            row("misses", "fast_field", 4.0),
            row("misses", "split_footer", 2.0),
        ]
    );

    // Nothing changed: no cache metric is sent.
    assert!(!scraper.observe(Ok(a.clone()), t0 + Duration::from_secs(60)));
    let from = flush().await;
    assert_eq!(cache_rows(&seen, from), vec![]);

    // The sidecar goes away for a few scrapes: one warning, nothing sent.
    for i in 2..6 {
        assert!(!scraper.observe(
            Err("connection refused".into()),
            t0 + Duration::from_secs(60 * i),
        ));
    }
    assert_eq!(scraper.failures(), 4);
    let from = flush().await;
    assert_eq!(cache_rows(&seen, from), vec![]);
    assert_eq!(
        console.count("searcher metrics unavailable; retrying quietly"),
        1
    );

    // Back, with the footer cache full: its gauges and increments; the fast
    // field cache didn't change. The first footer eviction is logged once,
    // as what was seen: an eviction alone doesn't say the footers don't fit.
    let b = report(&[
        (Cache::SplitFooter, s(268_000_000, 790, 10, 802, 12)),
        (Cache::FastField, s(663, 4, 0, 4, 0)),
    ]);
    let t1 = t0 + Duration::from_secs(360);
    // Higher counters after the gap: the same searcher, not a restart.
    assert!(!scraper.observe(Ok(b.clone()), t1));
    let c = report(&[
        (Cache::SplitFooter, s(268_000_000, 790, 11, 803, 13)),
        (Cache::FastField, s(663, 4, 0, 4, 0)),
    ]);
    scraper.observe(Ok(c.clone()), t1 + Duration::from_secs(60));
    let from = flush().await;
    assert_eq!(
        cache_rows(&seen, from),
        vec![
            row("bytes", "split_footer", 268_000_000.0),
            row("evictions", "split_footer", 13.0),
            row("hits", "split_footer", 7.0),
            row("items", "split_footer", 790.0),
            row("misses", "split_footer", 801.0),
        ]
    );
    assert_eq!(scraper.failures(), 0);
    assert_eq!(console.count("searcher metrics available again"), 1);
    assert_eq!(console.count("split footer cache evictions seen"), 1);

    // Quiet for the heartbeat: the gauges again, so a quiet replica still
    // reports what its caches hold.
    scraper.observe(Ok(c.clone()), t1 + HEARTBEAT);
    let from = flush().await;
    assert_eq!(
        cache_rows(&seen, from),
        vec![
            row("bytes", "fast_field", 663.0),
            row("bytes", "split_footer", 268_000_000.0),
            row("items", "fast_field", 4.0),
            row("items", "split_footer", 790.0),
        ]
    );

    // The searcher restarted, and only the footer cache's counters went
    // down: the fast field cache's 6 misses, more than before, are also all
    // since the restart, not 2 more.
    let d = report(&[
        (Cache::SplitFooter, s(8_000, 1, 0, 1, 0)),
        (Cache::FastField, s(663, 4, 0, 6, 0)),
    ]);
    assert!(scraper.observe(Ok(d), t1 + HEARTBEAT + Duration::from_secs(60)));
    let from = flush().await;
    assert_eq!(
        cache_rows(&seen, from),
        vec![
            row("bytes", "split_footer", 8_000.0),
            row("items", "split_footer", 1.0),
            row("misses", "fast_field", 6.0),
            row("misses", "split_footer", 1.0),
        ]
    );
    assert_eq!(
        console.count("the searcher restarted; its counters start again from 0"),
        1
    );
    // A new searcher run logs its first footer eviction again.
    let e = report(&[
        (Cache::SplitFooter, s(8_000, 1, 0, 2, 1)),
        (Cache::FastField, s(663, 4, 0, 6, 0)),
    ]);
    assert!(!scraper.observe(Ok(e), t1 + HEARTBEAT + Duration::from_secs(120)));
    assert_eq!(console.count("split footer cache evictions seen"), 2);

    drop(guard);
    tokio::task::spawn_blocking(move || {
        tracer.shutdown().unwrap();
        meter_provider.shutdown().unwrap();
    })
    .await
    .unwrap();
}

fn threads(search: (u64, u64), main_busy_ms: u64) -> ThreadReport {
    ThreadReport {
        pools: [
            (
                Pool::Search,
                PoolTasks {
                    ongoing: search.0,
                    pending: search.1,
                },
            ),
            (Pool::SmallTasks, PoolTasks::default()),
        ]
        .into(),
        runtimes: [(
            Runtime::Main,
            RuntimeStats {
                busy_ms: main_busy_ms,
                threads: 1,
            },
        )]
        .into(),
    }
}

fn with_threads(caches: &[(Cache, CacheStats)], threads: ThreadReport) -> SearcherMetrics {
    SearcherMetrics {
        threads,
        ..report(caches)
    }
}

/// The thread pools and runtimes (#251): both pools' tasks and every
/// runtime's threads at each scrape, 0 included, also before Quickwit has
/// used a pool; the busy time and thread time since the last scrape of the
/// same searcher run, gaps included; and a restart seen in a runtime alone
/// resets the caches' counts too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pools_and_runtimes_are_recorded_at_every_scrape() {
    let (endpoint, seen) = fake_ingestion().await;
    let conn = format!(
        "InstrumentationKey=00000000-0000-0000-0000-000000000000;IngestionEndpoint={endpoint}"
    );
    let (tracer, meter_provider) =
        usnm_telemetry::providers(&conn, Plain::with_token("tok"), SERVICE).unwrap();
    let meter_provider = Arc::new(meter_provider);
    let meter = meter_provider.meter(SERVICE.name);
    let console = Captured::default();
    let guard = tracing::subscriber::set_default(subscriber(&console));
    let flush = || {
        let p = meter_provider.clone();
        let before = seen.uploads().len();
        async move {
            tokio::task::spawn_blocking(move || p.force_flush().unwrap())
                .await
                .unwrap();
            before
        }
    };
    let pools = |search: (f64, f64), small: (f64, f64)| {
        vec![
            row_of("pool_tasks", "search ongoing", search.0),
            row_of("pool_tasks", "search pending", search.1),
            row_of("pool_tasks", "small_tasks ongoing", small.0),
            row_of("pool_tasks", "small_tasks pending", small.1),
        ]
    };

    let mut scraper = Scraper::new(Instruments::new(&meter));
    let t0 = Instant::now();
    let footers = [(Cache::SplitFooter, s(17_220, 2, 4, 2, 0))];
    let sorted = |mut rows: Vec<(String, String, f64)>| {
        rows.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        rows
    };

    // The first scrape, before Quickwit has used either pool, so it lists
    // neither: both are reported at 0. No busy or thread time: Quickwit's
    // busy time covers its whole life so far.
    let mut first = threads((0, 0), 13);
    first.pools.clear();
    scraper.observe(Ok(with_threads(&footers, first)), t0);
    let from = flush().await;
    let mut want = pools((0.0, 0.0), (0.0, 0.0));
    want.extend([
        row("bytes", "split_footer", 17_220.0),
        row("hits", "split_footer", 4.0),
        row("items", "split_footer", 2.0),
        row("misses", "split_footer", 2.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));

    // A minute of cold searches: the search pool full and queued, the main
    // runtime busy 59 of its 60 s of thread time. The caches didn't change.
    let t1 = t0 + Duration::from_secs(60);
    scraper.observe(Ok(with_threads(&footers, threads((3, 41), 59_013))), t1);
    let from = flush().await;
    let mut want = pools((3.0, 41.0), (0.0, 0.0));
    want.extend([
        row_of("runtime_busy_ms", "main", 59_000.0),
        row_of("runtime_capacity_ms", "main", 60_000.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));

    // Nothing changed: the pools, threads and thread time again, but no
    // busy time.
    scraper.observe(
        Ok(with_threads(&footers, threads((3, 41), 59_013))),
        t1 + Duration::from_secs(60),
    );
    let from = flush().await;
    let mut want = pools((3.0, 41.0), (0.0, 0.0));
    want.extend([
        row_of("runtime_capacity_ms", "main", 60_000.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));

    // A failed scrape records nothing, not even the pools.
    scraper.observe(Err("timed out".into()), t1 + Duration::from_secs(120));
    let from = flush().await;
    assert_eq!(exported(&seen, from), vec![]);

    // The next one covers both minutes, busy time and thread time alike.
    scraper.observe(
        Ok(with_threads(&footers, threads((1, 0), 59_013 + 90_000))),
        t1 + Duration::from_secs(180),
    );
    let from = flush().await;
    let mut want = pools((1.0, 0.0), (0.0, 0.0));
    want.extend([
        row_of("runtime_busy_ms", "main", 90_000.0),
        row_of("runtime_capacity_ms", "main", 120_000.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));

    // The searcher restarted. Its footer cache already counts more misses
    // than before, so only the main runtime's busy time shows the restart:
    // the 5 misses are all since then, not 3 more. Its busy time covers the
    // new searcher's life, so neither it nor the thread time is recorded.
    let footers = [(Cache::SplitFooter, s(17_220, 2, 4, 5, 0))];
    assert!(scraper.observe(
        Ok(with_threads(&footers, threads((0, 0), 700))),
        t1 + Duration::from_secs(240),
    ));
    let from = flush().await;
    let mut want = pools((0.0, 0.0), (0.0, 0.0));
    want.extend([
        row("hits", "split_footer", 4.0),
        row("misses", "split_footer", 5.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));
    assert_eq!(
        console.count("the searcher restarted; its counters start again from 0"),
        1
    );

    // From the next scrape on, both again.
    scraper.observe(
        Ok(with_threads(&footers, threads((0, 0), 30_700))),
        t1 + Duration::from_secs(300),
    );
    let from = flush().await;
    let mut want = pools((0.0, 0.0), (0.0, 0.0));
    want.extend([
        row_of("runtime_busy_ms", "main", 30_000.0),
        row_of("runtime_capacity_ms", "main", 60_000.0),
        row_of("runtime_threads", "main", 1.0),
    ]);
    assert_eq!(exported(&seen, from), sorted(want));

    drop(guard);
    tokio::task::spawn_blocking(move || {
        tracer.shutdown().unwrap();
        meter_provider.shutdown().unwrap();
    })
    .await
    .unwrap();
}

/// The scrape loop against a sidecar that isn't there: it keeps running and
/// logs one line, however many scrapes fail.
#[tokio::test]
async fn a_missing_sidecar_is_logged_once_and_the_loop_keeps_going() {
    let console = Captured::default();
    let _guard = tracing::subscriber::set_default(subscriber(&console));
    // Nothing listens on port 9 (discard) of localhost.
    let qw = Arc::new(QuickwitBackend::new("http://127.0.0.1:9", Duration::from_secs(1)).unwrap());
    let calls = Arc::new(Mutex::new(0u32));
    let handle = searcher_metrics::spawn(
        Duration::from_millis(20),
        &opentelemetry::global::meter("test"),
        {
            let calls = calls.clone();
            move || {
                let qw = qw.clone();
                *calls.lock().unwrap() += 1;
                async move { qw.searcher_metrics().await.map_err(|e| e.to_string()) }
            }
        },
        || panic!("no scrape succeeded, so none can show a restart"),
    );
    for _ in 0..200 {
        if *calls.lock().unwrap() >= 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(*calls.lock().unwrap() >= 5, "the loop stopped scraping");
    assert!(!handle.is_finished(), "the loop ended");
    handle.abort();
    assert_eq!(
        console.count("searcher metrics unavailable; retrying quietly"),
        1,
        "{}",
        console.text()
    );
}

/// The scrape loop tells `on_restart` about each restart its scrapes show,
/// once: a searcher in its own app restarts without the API (#267).
#[tokio::test]
async fn the_loop_reports_a_restart_once() {
    let footers = |misses| {
        let mut m = SearcherMetrics::default();
        m.caches
            .insert(Cache::SplitFooter, s(17_220, 2, 4, misses, 0));
        m
    };
    // A searcher run, a failed scrape while it restarts, the new run.
    let scrapes = Arc::new(Mutex::new(
        vec![
            Ok(footers(800)),
            Ok(footers(802)),
            Err("connection refused".to_owned()),
            Ok(footers(3)),
            Ok(footers(9)),
        ]
        .into_iter(),
    ));
    let left = {
        let scrapes = scrapes.clone();
        move || scrapes.lock().unwrap().as_slice().len()
    };
    let restarts = Arc::new(Mutex::new(Vec::new()));
    let handle = searcher_metrics::spawn(
        Duration::from_millis(10),
        &opentelemetry::global::meter("test"),
        {
            let scrapes = scrapes.clone();
            move || {
                let next = scrapes.lock().unwrap().next();
                async move { next.unwrap_or_else(|| Err("no more scrapes".to_owned())) }
            }
        },
        {
            let restarts = restarts.clone();
            let left = left.clone();
            // How many scrapes were left when it was called.
            move || restarts.lock().unwrap().push(left())
        },
    );
    for _ in 0..200 {
        if left() == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.abort();
    // After the fourth scrape, with one left.
    assert_eq!(*restarts.lock().unwrap(), vec![1]);
}

/// The scrape loop against a sidecar that answers with Quickwit 0.9.1's
/// metrics text.
#[tokio::test]
async fn reads_a_sidecar_over_http() {
    let text = include_str!("../../usnm-search/tests/data/quickwit-0.9.1-metrics.txt");
    let app =
        axum::Router::new().route("/metrics", axum::routing::get(move || async move { text }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let qw = QuickwitBackend::new(&format!("http://{addr}"), Duration::from_secs(1)).unwrap();
    let r = qw.searcher_metrics().await.unwrap();
    assert_eq!(r.caches[&Cache::SplitFooter], s(17_220, 2, 4, 2, 0));
    assert_eq!(
        r.threads.runtimes[&Runtime::Main],
        RuntimeStats {
            busy_ms: 13,
            threads: 3,
        }
    );
    // Something else on the port: an error, not a panic.
    let other = QuickwitBackend::new(&format!("http://{addr}/nope"), Duration::from_secs(1))
        .unwrap()
        .searcher_metrics()
        .await
        .unwrap_err()
        .to_string();
    assert!(other.contains("404"), "{other}");
}
