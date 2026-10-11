//! `GET /v1/status`: the data behind the site's dataset details page (06 §6.3.6).
//!
//! The document combines the version the API is serving (its reference
//! data, always available) with the pipeline state in Cosmos DB (batches,
//! index runs, locks and the LoC download pacer) and the titles catalog.
//! It is computed at most once per refresh interval (60 s) per replica: concurrent
//! requests share one computation, and a request that arrives while one is
//! running gets the previous document rather than waiting. When the pipeline
//! state can't be read, the last good reading is served with `stale: true`
//! and a sanitized error. Nothing identifying (hosts, identities, full worker
//! ids) and no query text is ever included.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use usnm_state::docs::{DocStore, FileDocs};
use usnm_state::summary::{self, Summary};
use usnm_store::ObjectStore;

pub mod activity;
pub mod assemble;
pub mod ocr_ja;
pub mod ocr_quality;
pub mod sanitize;
pub mod schedule;

use activity::{Activity, TitlesCache};
use assemble::{Backfill, CatalogTitles, Indexing, Published, Section, Titles};

/// Bump when the document's shape changes incompatibly.
pub const SCHEMA: u32 = 1;

/// Longest a refresh may take before it counts as failed.
const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// Where the pipeline state comes from.
pub enum PipelineSource {
    /// Not configured (fixtures, local development without a state file).
    None,
    /// Cosmos DB (or any store), read-only.
    Docs(Arc<dyn DocStore>),
    /// The ingest CLI's local state file, re-read on every refresh.
    File(PathBuf),
}

#[derive(Debug, Clone, Serialize)]
pub struct Pipeline {
    pub available: bool,
    /// When the pipeline state in this document was read.
    pub read_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The document `/v1/status` returns.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub schema: u32,
    pub generated_at: DateTime<Utc>,
    /// The pipeline sections come from an earlier reading: the latest failed.
    pub stale: bool,
    /// Why the latest reading failed (sanitized).
    pub error: Option<String>,
    pub pipeline: Pipeline,
    pub published: Published,
    /// What the pipeline is doing at this moment ("Right now").
    pub activity: Section<Activity>,
    pub backfill: Section<Backfill>,
    pub indexing: Section<Indexing>,
    pub titles: Titles,
    /// The Japanese OCR job's progress.
    pub ocr_ja: Section<ocr_ja::OcrJa>,
    /// The OCR quality audit's progress and, once finished, its summary.
    pub ocr_quality: Section<ocr_quality::OcrQuality>,
}

struct Entry {
    at: Instant,
    body: Arc<Vec<u8>>,
    /// The published version the document describes.
    version: String,
}

/// The last good pipeline reading.
struct Reading {
    at: DateTime<Utc>,
    summary: Arc<Summary>,
}

pub struct StatusService {
    source: PipelineSource,
    refresh: Duration,
    latest: RwLock<Option<Arc<Entry>>>,
    last_good: RwLock<Option<Arc<Reading>>>,
    /// Held while a refresh runs: one at a time.
    refreshing: Mutex<()>,
    /// The title-record cache's LCCNs, re-read only when it changes.
    titles_cache: Mutex<Option<TitlesCache>>,
}

const NOT_CONFIGURED: &str = "This server is not connected to the pipeline state.";
const UNREADABLE: &str = "The pipeline state could not be read.";
const NO_CATALOG: &str = "The titles catalog is not available on this server.";
const NO_OCR_JA: &str = "The Japanese OCR job has not reported any progress.";
const UNREADABLE_OCR_JA: &str = "The Japanese OCR job's progress could not be read.";
const NO_OCR_QUALITY: &str = "The OCR quality audit has not run.";
const UNREADABLE_OCR_QUALITY: &str = "The OCR quality audit's progress could not be read.";

impl StatusService {
    pub fn new(source: PipelineSource, refresh: Duration) -> Self {
        Self {
            source,
            refresh,
            latest: RwLock::new(None),
            last_good: RwLock::new(None),
            refreshing: Mutex::new(()),
            titles_cache: Mutex::new(None),
        }
    }

    fn cached(&self) -> Option<Arc<Entry>> {
        self.latest.read().expect("status cache").clone()
    }

    fn fresh(&self) -> Option<Arc<Entry>> {
        self.cached().filter(|e| e.at.elapsed() < self.refresh)
    }

    /// The current document and the published version it describes,
    /// refreshed if it is older than the interval.
    pub async fn get(&self, app: &crate::AppState) -> (Arc<Vec<u8>>, String) {
        let entry = self.entry(app).await;
        (entry.body.clone(), entry.version.clone())
    }

    async fn entry(&self, app: &crate::AppState) -> Arc<Entry> {
        if let Some(e) = self.fresh() {
            return e;
        }
        // Someone else is refreshing: serve what there is instead of queueing.
        let guard = match self.refreshing.try_lock() {
            Ok(g) => g,
            Err(_) => {
                if let Some(e) = self.cached() {
                    return e;
                }
                self.refreshing.lock().await
            }
        };
        if let Some(e) = self.fresh() {
            return e;
        }
        let status = self.compute(app).await;
        let entry = Arc::new(Entry {
            at: Instant::now(),
            body: Arc::new(serde_json::to_vec(&status).unwrap_or_else(|_| b"{}".to_vec())),
            version: status.published.index_version.clone(),
        });
        *self.latest.write().expect("status cache") = Some(entry.clone());
        drop(guard);
        entry
    }

    /// The pipeline reading the current document was built from, refreshed
    /// like [`Self::get`] (so `/v1/versions` costs no extra reads), and when it
    /// was taken. `None` without pipeline state or before a first good read.
    pub async fn reading(&self, app: &crate::AppState) -> Option<(DateTime<Utc>, Arc<Summary>)> {
        self.entry(app).await;
        self.last_good
            .read()
            .expect("status reading")
            .as_ref()
            .map(|r| (r.at, r.summary.clone()))
    }

    async fn read_pipeline(&self) -> Result<Option<Summary>, String> {
        let read = async {
            match &self.source {
                PipelineSource::None => Ok(None),
                PipelineSource::Docs(docs) => summary::read(docs.as_ref()).await.map(Some),
                PipelineSource::File(path) => {
                    let docs = FileDocs::open(path)?;
                    summary::read(&docs).await.map(Some)
                }
            }
        };
        match tokio::time::timeout(READ_TIMEOUT, read).await {
            Ok(r) => r.map_err(|e| format!("{e:#}")),
            Err(_) => Err(format!(
                "reading the pipeline state took over {} s",
                READ_TIMEOUT.as_secs()
            )),
        }
    }

    async fn compute(&self, app: &crate::AppState) -> Status {
        let now = Utc::now();
        let reference = app.loader.as_ref().map(|l| l.reference.clone());
        let catalog = async {
            tokio::time::timeout(READ_TIMEOUT, read_catalog(reference))
                .await
                .unwrap_or_else(|_| {
                    Err(format!(
                        "reading the titles catalog took over {} s",
                        READ_TIMEOUT.as_secs()
                    ))
                })
        };
        let cache = async {
            tokio::time::timeout(READ_TIMEOUT, self.read_titles_cache(reference_store(app)))
                .await
                .unwrap_or_else(|_| Err("reading the title cache timed out".to_owned()))
        };
        let ocr = async {
            tokio::time::timeout(READ_TIMEOUT, ocr_ja::read(reference_store(app)))
                .await
                .unwrap_or_else(|_| Err("reading the OCR progress timed out".to_owned()))
        };
        let quality = async {
            tokio::time::timeout(READ_TIMEOUT, ocr_quality::read(reference_store(app)))
                .await
                .unwrap_or_else(|_| Err("reading the OCR quality audit timed out".to_owned()))
        };
        let (pipeline, catalog, cache, ocr, quality) =
            tokio::join!(self.read_pipeline(), catalog, cache, ocr, quality);
        let ocr_quality = match quality {
            Ok(Some(p)) => Section::of(ocr_quality::section(p, now)),
            Ok(None) => Section::unavailable(NO_OCR_QUALITY),
            Err(e) => {
                tracing::warn!(error = %e, "status: could not read the OCR quality audit");
                Section::unavailable(UNREADABLE_OCR_QUALITY)
            }
        };
        let ocr_ja = match ocr {
            Ok(Some(p)) => Section::of(ocr_ja::section(p, now)),
            Ok(None) => Section::unavailable(NO_OCR_JA),
            Err(e) => {
                tracing::warn!(error = %e, "status: could not read the OCR progress");
                Section::unavailable(UNREADABLE_OCR_JA)
            }
        };
        let cache = cache.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "status: could not read the title cache");
            None
        });
        let (reading, stale, error, reason) = match pipeline {
            Ok(Some(s)) => {
                if let Some(ru) = s.request_charge {
                    tracing::info!(request_units = ru, "status: read the pipeline state");
                }
                if s.unreadable > 0 {
                    tracing::warn!(items = s.unreadable, "status: skipped unreadable items");
                }
                let r = Arc::new(Reading {
                    at: now,
                    summary: Arc::new(s),
                });
                *self.last_good.write().expect("status reading") = Some(r.clone());
                (Some(r), false, None, None)
            }
            Ok(None) => (None, false, None, Some(NOT_CONFIGURED)),
            Err(e) => {
                // The full error goes to the API's own logs only.
                tracing::warn!(error = %e, "status: could not read the pipeline state");
                let last = self.last_good.read().expect("status reading").clone();
                let stale = last.is_some();
                (last, stale, Some(sanitize::sanitize(&e)), Some(UNREADABLE))
            }
        };
        let snap = app.snapshot.load();
        let rd = &snap.refdata;
        let catalog = catalog.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "status: could not read the titles catalog");
            None
        });
        let reason = reason.unwrap_or(UNREADABLE);
        let summary = reading.as_ref().map(|r| r.summary.as_ref());
        let backfill: Section<Backfill> = match &reading {
            Some(r) => Section::of(assemble::backfill(r.at, &r.summary)),
            None => Section::unavailable(reason),
        };
        let indexing: Section<Indexing> = match &reading {
            Some(r) => Section::of(assemble::indexing(r.at, &r.summary)),
            None => Section::unavailable(reason),
        };
        let next_run = app
            .config
            .ingest_cron
            .as_ref()
            .and_then(|c| c.next_after(now));
        let activity: Section<Activity> = match (&reading, &backfill.data) {
            (Some(r), Some(b)) => Section::of(activity::activity(&activity::Inputs {
                at: r.at,
                summary: &r.summary,
                backfill: b,
                catalog: catalog.as_ref(),
                cache: cache.as_ref(),
                next_run,
            })),
            _ => Section::unavailable(reason),
        };
        Status {
            schema: SCHEMA,
            generated_at: now,
            stale,
            error,
            pipeline: Pipeline {
                available: reading.is_some(),
                read_at: reading.as_ref().map(|r| r.at),
                reason: reading.is_none().then(|| reason.to_owned()),
            },
            published: assemble::published(rd),
            activity,
            backfill,
            indexing,
            titles: assemble::titles(rd, catalog.as_ref(), NO_CATALOG, summary, reason),
            ocr_ja,
            ocr_quality,
        }
    }
}

fn reference_store(app: &crate::AppState) -> Option<Arc<dyn ObjectStore>> {
    app.loader.as_ref().map(|l| l.reference.clone())
}

impl StatusService {
    /// The LCCNs in titles-sync's record cache (`raw/titles.json`) and when
    /// it was saved, read again only when it has changed since the last
    /// refresh. `None` without a reference store or a cache.
    async fn read_titles_cache(
        &self,
        store: Option<Arc<dyn ObjectStore>>,
    ) -> Result<Option<TitlesCache>, String> {
        let Some(store) = store else {
            return Ok(None);
        };
        let modified = store
            .modified(TITLES_CACHE)
            .await
            .map_err(|e| e.to_string())?
            .map(DateTime::<Utc>::from);
        let mut held = self.titles_cache.lock().await;
        if let Some(c) = held.as_ref() {
            if modified.is_some() && c.modified == modified {
                return Ok(Some(c.clone()));
            }
        }
        let Some(bytes) = store.get(TITLES_CACHE).await.map_err(|e| e.to_string())? else {
            *held = None;
            return Ok(None);
        };
        let lccns = tokio::task::spawn_blocking(move || {
            serde_json::from_slice::<std::collections::BTreeMap<String, serde::de::IgnoredAny>>(
                &bytes,
            )
            .map(|m| m.into_keys().collect::<HashSet<_>>())
            .map_err(|e| format!("{TITLES_CACHE}: {e}"))
        })
        .await
        .map_err(|e| e.to_string())??;
        let cache = TitlesCache { lccns, modified };
        *held = Some(cache.clone());
        Ok(Some(cache))
    }
}

/// titles-sync's record cache in the reference store (04 §4.6).
const TITLES_CACHE: &str = "raw/titles.json";

#[derive(Deserialize)]
struct CatalogTitle {
    lccn: String,
}

/// Titles and place counts from `catalog/`, or `None` where there is no catalog.
async fn read_catalog(
    store: Option<Arc<dyn ObjectStore>>,
) -> Result<Option<CatalogTitles>, String> {
    let Some(store) = store else {
        return Ok(None);
    };
    let (titles, places) = tokio::join!(
        store.get("catalog/titles.json"),
        store.get("catalog/places.json")
    );
    let (Some(titles), Some(places)) = (
        titles.map_err(|e| e.to_string())?,
        places.map_err(|e| e.to_string())?,
    ) else {
        return Ok(None);
    };
    tokio::task::spawn_blocking(move || {
        let titles: Vec<CatalogTitle> =
            serde_json::from_slice(&titles).map_err(|e| format!("catalog/titles.json: {e}"))?;
        let places: Vec<serde::de::IgnoredAny> =
            serde_json::from_slice(&places).map_err(|e| format!("catalog/places.json: {e}"))?;
        Ok(Some(CatalogTitles {
            lccns: titles.into_iter().map(|t| t.lccn).collect::<HashSet<_>>(),
            places: places.len(),
        }))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use serde_json::{json, Value};
    use usnm_state::docs::{Field, MemoryDocs, Versioned};

    use crate::config::Config;
    use crate::refdata::RefData;
    use crate::AppState;

    /// Memory state that counts reads, takes a while, and can be made to fail.
    #[derive(Default)]
    struct Slow {
        mem: MemoryDocs,
        selects: AtomicUsize,
        broken: AtomicBool,
    }

    #[async_trait::async_trait]
    impl DocStore for Slow {
        async fn get(&self, c: &str, pk: &str, id: &str) -> anyhow::Result<Option<Versioned>> {
            self.mem.get(c, pk, id).await
        }
        async fn create(&self, c: &str, pk: &str, doc: &Value) -> anyhow::Result<Option<String>> {
            self.mem.create(c, pk, doc).await
        }
        async fn replace(
            &self,
            c: &str,
            pk: &str,
            doc: &Value,
            etag: &str,
        ) -> anyhow::Result<Option<String>> {
            self.mem.replace(c, pk, doc, etag).await
        }
        async fn upsert(&self, c: &str, pk: &str, doc: &Value) -> anyhow::Result<()> {
            self.mem.upsert(c, pk, doc).await
        }
        async fn list(&self, c: &str, f: &str, v: &[&str]) -> anyhow::Result<Vec<Versioned>> {
            if self.broken.load(Ordering::SeqCst) {
                anyhow::bail!("Cosmos query in `ops` returned 403 Forbidden: principal 3fa85f64-5717-4562-b3fc-2c963f66afa6 on cosmos-usnm-prod-x.documents.azure.com");
            }
            self.mem.list(c, f, v).await
        }
        async fn select(&self, c: &str, fields: &[Field]) -> anyhow::Result<Vec<Value>> {
            self.selects.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            if self.broken.load(Ordering::SeqCst) {
                anyhow::bail!("unreachable");
            }
            let all = self.mem.list(c, "id", &[]).await?;
            Ok(all
                .iter()
                .map(|v| usnm_state::docs::project(&v.doc, fields))
                .collect())
        }
    }

    async fn app(source: PipelineSource, refresh: Duration) -> Arc<AppState> {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/data");
        let rd = RefData::load(&usnm_store::LocalStore::new(dir))
            .await
            .unwrap();
        let mut config = Config::from_lookup(|_| None).unwrap();
        config.status_refresh = refresh;
        Arc::new(
            AppState::new(
                config,
                Arc::new(usnm_search::memory::MemoryBackend::new()),
                rd,
            )
            .with_pipeline(source),
        )
    }

    fn parse(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    async fn seeded() -> Arc<Slow> {
        let docs = Arc::new(Slow::default());
        docs.mem
            .upsert(
                "batches",
                "b1",
                &json!({"id": "b1", "batch": "b1", "version": 1, "status": "queued",
                        "updated_at": Utc::now()}),
            )
            .await
            .unwrap();
        docs
    }

    #[tokio::test]
    async fn without_pipeline_state_the_reference_sections_still_work() {
        let app = app(PipelineSource::None, Duration::from_secs(60)).await;
        let v = parse(&app.status.get(&app).await.0);
        assert_eq!(v["schema"], 1);
        assert_eq!(v["stale"], false);
        assert_eq!(v["pipeline"]["available"], false);
        assert_eq!(
            v["backfill"],
            json!({"available": false, "reason": NOT_CONFIGURED})
        );
        assert_eq!(v["indexing"]["available"], false);
        assert_eq!(v["activity"]["available"], false);
        assert_eq!(v["titles"]["pipeline"]["available"], false);
        assert_eq!(v["titles"]["catalog"]["available"], false);
        assert_eq!(
            v["ocr_ja"],
            json!({"available": false, "reason": NO_OCR_JA})
        );
        assert_eq!(
            v["ocr_quality"],
            json!({"available": false, "reason": NO_OCR_QUALITY})
        );
        assert_eq!(v["published"]["index_version"], "fixture-v1");
        assert_eq!(v["published"]["deltas"], 1);
        assert!(v["published"]["pages"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_reading() {
        let docs = seeded().await;
        let app = app(PipelineSource::Docs(docs.clone()), Duration::from_secs(60)).await;
        let gets = (0..10).map(|_| app.status.get(&app));
        let bodies: Vec<_> = futures::future::join_all(gets)
            .await
            .into_iter()
            .map(|(body, version)| {
                assert_eq!(version, "fixture-v1");
                body
            })
            .collect();
        // Two selects (batches, index runs) for the one refresh.
        assert_eq!(docs.selects.load(Ordering::SeqCst), 2);
        assert!(bodies.windows(2).all(|w| w[0] == w[1]));
        let v = parse(&bodies[0]);
        assert_eq!(v["backfill"]["total"], 1);
        assert_eq!(v["pipeline"]["available"], true);
        // Within the interval, nothing is read again.
        app.status.get(&app).await;
        assert_eq!(docs.selects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn runs_show_their_batch_count_in_either_item_format() {
        let docs = seeded().await;
        // Written before the batch list moved to the reference snapshot.
        docs.mem
            .upsert(
                "index_runs",
                "v1",
                &json!({"id": "v1", "index_version": "v1", "full": true, "indexes": ["b1"],
                        "new_index": "b1", "status": "published", "docs": 9, "pages": 10,
                        "batches": [{"batch": "x1", "curated": {}}, {"batch": "x2", "curated": {}}],
                        "started_at": "2026-09-28T10:00:00Z",
                        "published_at": "2026-09-28T11:00:00Z"}),
            )
            .await
            .unwrap();
        docs.mem
            .upsert(
                "index_runs",
                "v2",
                &json!({"id": "v2", "index_version": "v2", "full": true, "indexes": ["b2"],
                        "new_index": "b2", "status": "published", "docs": 90, "pages": 100,
                        "batch_count": 3000, "batch_list": "v2/batches.json",
                        "started_at": "2026-09-29T10:00:00Z",
                        "published_at": "2026-09-29T11:00:00Z"}),
            )
            .await
            .unwrap();
        let app = app(PipelineSource::Docs(docs), Duration::from_secs(60)).await;
        let v = parse(&app.status.get(&app).await.0);
        let runs: Vec<(&str, u64)> = v["indexing"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["index_version"].as_str().unwrap(),
                    r["batches"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(runs, [("v2", 3000), ("v1", 2)]);
    }

    #[tokio::test]
    async fn versions_read_each_runs_build_from_the_same_reading() {
        let docs = seeded().await;
        docs.mem
            .upsert(
                "index_runs",
                "v1",
                &json!({"id": "v1", "index_version": "v1", "full": true, "indexes": ["b1"],
                        "new_index": "b1", "status": "published", "docs": 9, "pages": 10,
                        "started_at": "2026-10-06T10:00:00Z",
                        "build": {"commit": "abc", "engine": "Quickwit 0.9.1"}}),
            )
            .await
            .unwrap();
        let app = app(PipelineSource::Docs(docs.clone()), Duration::from_secs(60)).await;
        let (_, summary) = app.status.reading(&app).await.unwrap();
        assert_eq!(summary.runs[0].build.as_ref().unwrap()["commit"], "abc");
        // The status document and the versions share one reading.
        app.status.get(&app).await;
        assert_eq!(docs.selects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_reading_serves_the_last_good_one_as_stale() {
        let docs = seeded().await;
        let app = app(PipelineSource::Docs(docs.clone()), Duration::from_millis(1)).await;
        let first = parse(&app.status.get(&app).await.0);
        assert_eq!(first["stale"], false);
        docs.broken.store(true, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        let v = parse(&app.status.get(&app).await.0);
        assert_eq!(v["stale"], true);
        assert_eq!(v["backfill"]["total"], 1);
        assert_eq!(v["pipeline"]["read_at"], first["pipeline"]["read_at"]);
        let err = v["error"].as_str().unwrap();
        assert!(
            !err.contains("3fa85f64") && !err.contains("documents.azure.com"),
            "{err}"
        );
        // Recovers once the state is readable again.
        docs.broken.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        let v = parse(&app.status.get(&app).await.0);
        assert_eq!(
            (v["stale"].clone(), v["error"].clone()),
            (json!(false), Value::Null)
        );
    }

    #[tokio::test]
    async fn reports_what_the_ingest_job_is_doing() {
        let docs = seeded().await;
        let now = Utc::now();
        usnm_state::state::State::new(docs.clone())
            .set_activity(&usnm_state::state::Activity {
                command: "run".into(),
                owner: "caj-usnm-ingest-prod-x-1-0a1b2c3d".into(),
                started_at: now,
                step: usnm_state::state::Step::Titles,
                step_started_at: now,
                updated_at: now,
                done: Some(342),
                total: Some(3464),
                paused_until: Some(now + chrono::Duration::minutes(65)),
                index_version: None,
                merge: None,
                ended_at: None,
                outcome: None,
                error: None,
                previous: None,
            })
            .await
            .unwrap();
        let app = app(PipelineSource::Docs(docs), Duration::from_secs(60)).await;
        let v = parse(&app.status.get(&app).await.0);
        let a = &v["activity"];
        assert_eq!(a["available"], true);
        assert_eq!(
            (a["now"].as_str(), a["source"].as_str()),
            (Some("titles"), Some("job"))
        );
        assert_eq!(
            (a["done"].as_u64(), a["total"].as_u64()),
            (Some(342), Some(3464))
        );
        assert!(a["paused_until"].is_string());
        // Only the last six characters of the execution's id.
        assert_eq!(a["run"], "1b2c3d");
        assert_eq!(a["next_run"], Value::Null);
    }

    #[tokio::test]
    async fn a_failed_first_reading_reports_unavailable() {
        let docs = seeded().await;
        docs.broken.store(true, Ordering::SeqCst);
        let app = app(PipelineSource::Docs(docs), Duration::from_secs(60)).await;
        let v = parse(&app.status.get(&app).await.0);
        assert_eq!(v["stale"], false);
        assert_eq!(v["pipeline"]["available"], false);
        assert_eq!(v["backfill"]["available"], false);
        assert!(v["error"].is_string());
    }

    #[tokio::test]
    async fn reads_a_local_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let docs = FileDocs::open(&path).unwrap();
        docs.upsert(
            "batches",
            "b1",
            &json!({"id": "b1", "batch": "b1", "version": 1, "status": "failed",
                    "attempts": 5, "updated_at": Utc::now()}),
        )
        .await
        .unwrap();
        let app = app(PipelineSource::File(path), Duration::from_secs(60)).await;
        let v = parse(&app.status.get(&app).await.0);
        assert_eq!(v["backfill"]["by_status"]["failed"], 1);
    }
}
