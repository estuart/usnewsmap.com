//! The worker's status on the LAN, for the dashboard (`usnm-ocrdash`,
//! 12 §12.2.5): what it is reading now, what it has finished, and a
//! journal of finished pages that survives a restart.
//!
//! - `GET /status`: [`WorkerStatus`].
//! - `GET /journal?from=<offset>`: the journal's lines from a byte offset,
//!   at most 1 MiB and whole lines only. Each line is a [`Completion`].
//!
//! Only `--status-allow` addresses are answered; the node's nftables rule
//! (`install-worker-node.sh`) says the same. Nothing here holds page text:
//! a page is its doc_id, with its language, queue, engine tag, times,
//! outcome and scan source.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use usnm_core::ids::PageKey;
pub use usnm_ocr_contract::status::{
    Completion, DeadLetterView, InFlight, Outcomes, QueueStats, WorkerStatus, DEAD_LETTERS_MAX,
    RECENT_MAX,
};

use crate::message::{clean_error, decode_dead_letter};
use crate::queue::{Queue, BULK, DEADLETTER, PRIORITY};

use crate::worker::ScanSource;

/// The most `GET /journal` returns at once.
pub const JOURNAL_CHUNK: u64 = 1024 * 1024;

/// What the worker is, for the status.
#[derive(Debug, Clone, Default)]
pub struct About {
    pub node_id: String,
    pub version: String,
    pub concurrency: u32,
    pub engine: String,
    pub scan_cache_url: Option<String>,
}

#[derive(Debug, Default)]
struct Inner {
    in_flight: HashMap<String, InFlight>,
    /// Finished pages over the last day, oldest first.
    day: VecDeque<Completion>,
    since_start: Outcomes,
    recent: VecDeque<Completion>,
    /// Language → the engine tag's profile (or the tag) of its last page
    /// done.
    routes: BTreeMap<String, String>,
    /// Where each page in flight got its scan: `cache` or `loc`.
    sources: HashMap<String, &'static str>,
    queues: Option<QueueStats>,
}

/// The worker's record of its pages. Cheap: a lock and a few counters per
/// page.
pub struct Tracker {
    about: About,
    started_at: DateTime<Utc>,
    inner: Mutex<Inner>,
    journal: Option<(PathBuf, Mutex<std::fs::File>)>,
    scans: Option<Arc<dyn ScanSource>>,
}

impl std::fmt::Debug for Tracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tracker")
            .field("node_id", &self.about.node_id)
            .finish_non_exhaustive()
    }
}

fn open_journal(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
}

/// Whether `f` is still the file at `path`.
fn same_file(f: &std::fs::File, path: &std::path::Path) -> bool {
    match (f.metadata(), std::fs::metadata(path)) {
        (Ok(a), Ok(b)) => journal_id(&a) == journal_id(&b),
        _ => false,
    }
}

/// A journal file's identity: its device and inode, which change when the
/// file is replaced, so the dashboard reads a new one from the start.
#[cfg(unix)]
fn journal_id(m: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}-{}", m.dev(), m.ino())
}

#[cfg(not(unix))]
fn journal_id(_: &std::fs::Metadata) -> String {
    String::new()
}

/// The short form of an engine tag for the routes: its `profile=`, else
/// the tag.
fn profile(tag: &str) -> String {
    tag.split_whitespace()
        .find_map(|w| w.strip_prefix("profile="))
        .unwrap_or(tag)
        .to_owned()
}

impl Tracker {
    /// A tracker; with `journal`, finished pages are appended to that file.
    pub fn new(
        about: About,
        journal: Option<PathBuf>,
        scans: Option<Arc<dyn ScanSource>>,
    ) -> anyhow::Result<Self> {
        let journal = match journal {
            Some(p) => {
                let f = open_journal(&p)
                    .map_err(|e| anyhow::anyhow!("opening the journal {}: {e}", p.display()))?;
                Some((p, Mutex::new(f)))
            }
            None => None,
        };
        Ok(Tracker {
            about,
            started_at: Utc::now(),
            inner: Mutex::new(Inner::default()),
            journal,
            scans,
        })
    }

    /// A tracker that keeps counts but no journal and serves nothing (tests,
    /// and a worker run without `--status-listen`).
    pub fn quiet() -> Self {
        Tracker {
            about: About::default(),
            started_at: Utc::now(),
            inner: Mutex::new(Inner::default()),
            journal: None,
            scans: None,
        }
    }

    /// A page taken, under `token` (its message's id), which the page's
    /// later calls name: the same page can be in flight twice.
    pub fn begin(&self, token: &str, doc_id: &str, language: &str, queue: &str) {
        self.inner.lock().expect("tracker lock").in_flight.insert(
            token.to_owned(),
            InFlight {
                doc_id: doc_id.to_owned(),
                language: language.to_owned(),
                queue: queue.to_owned(),
                started_at: Utc::now(),
                stage: "fetch".into(),
            },
        );
    }

    pub fn stage(&self, token: &str, stage: &str) {
        if let Some(p) = self
            .inner
            .lock()
            .expect("tracker lock")
            .in_flight
            .get_mut(token)
        {
            p.stage = stage.to_owned();
        }
    }

    pub fn set_queues(&self, q: QueueStats) {
        self.inner.lock().expect("tracker lock").queues = Some(q);
    }

    /// Where a page's scan came from.
    pub fn scanned(&self, token: &str, from_cache: bool) {
        self.inner
            .lock()
            .expect("tracker lock")
            .sources
            .insert(token.to_owned(), if from_cache { "cache" } else { "loc" });
    }

    /// A page finished: off the in-flight list, into the counts and the
    /// journal. A line that fails [`Completion::check`] is counted but not
    /// written.
    pub fn end(&self, token: &str, mut c: Completion) {
        c.finished_at = Utc::now();
        {
            let mut i = self.inner.lock().expect("tracker lock");
            if let Some(p) = i.in_flight.remove(token) {
                c.started_at = p.started_at;
            }
            if let Some(src) = i.sources.remove(token) {
                if c.scan.is_empty() {
                    c.scan = src.to_owned();
                }
            }
            c.secs = ((c.finished_at - c.started_at).num_milliseconds().max(0) as f64) / 1000.0;
            i.since_start.add(&c);
            if c.outcome == "done" && !c.engine.is_empty() {
                let p = profile(&c.engine);
                i.routes.insert(c.language.clone(), p);
            }
            i.day.push_back(c.clone());
            let cut = c.finished_at - Duration::days(1);
            while i.day.front().is_some_and(|d| d.finished_at < cut) {
                i.day.pop_front();
            }
            i.recent.push_front(c.clone());
            i.recent.truncate(RECENT_MAX);
        }
        if let Some((path, f)) = &self.journal {
            if c.check().is_err() {
                tracing::warn!(
                    doc_id = c.doc_id,
                    "a journal line that fails its check; not written"
                );
                return;
            }
            let mut line = match serde_json::to_vec(&c) {
                Ok(l) => l,
                Err(_) => return,
            };
            line.push(b'\n');
            let mut f = f.lock().expect("journal lock");
            // The file at the path may have been replaced (and the
            // dashboard told so by its new id): write to the one there.
            if !same_file(&f, path) {
                match open_journal(path) {
                    Ok(new) => *f = new,
                    Err(e) => {
                        tracing::warn!(error = %e, path = %path.display(), "can't reopen the journal")
                    }
                }
            }
            if let Err(e) = f.write_all(&line) {
                tracing::warn!(error = %e, path = %path.display(), "can't write the journal");
            }
        }
    }

    pub fn status(&self) -> WorkerStatus {
        let now = Utc::now();
        let i = self.inner.lock().expect("tracker lock");
        let mut hour = Outcomes::default();
        let mut day = Outcomes::default();
        let cut_day = now - Duration::days(1);
        let cut_hour = now - Duration::hours(1);
        for c in i.day.iter().filter(|c| c.finished_at >= cut_day) {
            day.add(c);
            if c.finished_at >= cut_hour {
                hour.add(c);
            }
        }
        let journal = self
            .journal
            .as_ref()
            .and_then(|(p, _)| std::fs::metadata(p).ok());
        let mut in_flight: Vec<InFlight> = i.in_flight.values().cloned().collect();
        in_flight.sort_by_key(|p| p.started_at);
        WorkerStatus {
            schema: usnm_ocr_contract::status::SCHEMA,
            node_id: self.about.node_id.clone(),
            version: self.about.version.clone(),
            started_at: self.started_at,
            now,
            concurrency: self.about.concurrency,
            engine: self.about.engine.clone(),
            engine_version: String::new(),
            models: Vec::new(),
            routes: i.routes.clone(),
            scan_cache_url: self.about.scan_cache_url.clone(),
            loc_hold_secs: self.scans.as_ref().and_then(|s| {
                s.held_until().map(|t| {
                    t.saturating_duration_since(tokio::time::Instant::now())
                        .as_secs()
                })
            }),
            in_flight,
            last_hour: hour,
            last_day: day,
            since_start: i.since_start,
            recent: i.recent.iter().cloned().collect(),
            journal_bytes: journal.as_ref().map_or(0, |m| m.len()),
            journal_id: journal.as_ref().map_or_else(String::new, journal_id),
            queues: i.queues.clone(),
        }
    }

    /// The journal from `from`: whole lines, at most [`JOURNAL_CHUNK`].
    pub fn journal_from(&self, from: u64) -> std::io::Result<Vec<u8>> {
        let Some((path, _)) = &self.journal else {
            return Ok(Vec::new());
        };
        let mut f = std::fs::File::open(path)?;
        let len = f.metadata()?.len();
        if from >= len {
            return Ok(Vec::new());
        }
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::new();
        f.take((len - from).min(JOURNAL_CHUNK))
            .read_to_end(&mut buf)?;
        match buf.iter().rposition(|b| *b == b'\n') {
            Some(end) => buf.truncate(end + 1),
            None => buf.clear(),
        }
        Ok(buf)
    }
}

/// How often a worker looks at the queues for its status.
pub const QUEUE_LOOK_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// A dead letter as the dashboard shows it: its page, cause, kind, error,
/// count, node and time; nothing else of its message.
pub fn dead_letter_view(text: &str) -> DeadLetterView {
    match decode_dead_letter(text) {
        Ok(d) => DeadLetterView {
            message_id: clean_error(&d.message_id).chars().take(64).collect(),
            doc_id: d
                .message
                .as_ref()
                .and_then(|m| m.page.parse::<PageKey>().ok())
                .map(|k| k.doc_id())
                .unwrap_or_default(),
            from: clean_error(&d.from).chars().take(32).collect(),
            cause: d.cause.map(|c| c.as_str().to_owned()).unwrap_or_default(),
            kind: d.kind.map(|k| clean_error(&k).chars().take(32).collect()),
            error: clean_error(&d.error),
            dequeue_count: d.dequeue_count,
            node: clean_error(&d.node).chars().take(32).collect(),
            at: clean_error(&d.at).chars().take(40).collect(),
        },
        Err(e) => DeadLetterView {
            error: format!("a dead letter this worker can't read ({e})"),
            ..DeadLetterView::default()
        },
    }
}

/// Look at the queues: each one's length, and the dead letters at the
/// front of `ocr-deadletter`. Peeks only; takes nothing.
pub async fn look_at_queues(queue: &dyn Queue) -> QueueStats {
    let mut q = QueueStats {
        checked_at: Utc::now(),
        ..QueueStats::default()
    };
    let mut errors = Vec::new();
    for name in [PRIORITY, BULK, DEADLETTER] {
        match queue.approximate_count(name).await {
            Ok(n) => {
                q.counts.insert(name.to_owned(), n);
            }
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    match queue.peek(DEADLETTER, DEAD_LETTERS_MAX as u32).await {
        Ok(texts) => q.dead_letters = texts.iter().map(|t| dead_letter_view(t)).collect(),
        Err(e) => errors.push(format!("{e:#}")),
    }
    if !errors.is_empty() {
        q.error = Some(clean_error(&errors.join("; ")));
    }
    q
}

#[derive(Clone)]
struct Served {
    tracker: Arc<Tracker>,
    allow: Arc<Vec<IpAddr>>,
}

async fn allow_only(
    State(s): State<Served>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    if s.allow.contains(&peer.ip().to_canonical()) {
        next.run(req).await
    } else {
        StatusCode::FORBIDDEN.into_response()
    }
}

#[derive(Deserialize)]
struct From {
    #[serde(default)]
    from: u64,
}

pub fn router(tracker: Arc<Tracker>, allow: Vec<IpAddr>) -> Router {
    let s = Served {
        tracker,
        allow: Arc::new(allow),
    };
    Router::new()
        .route(
            "/status",
            get(|State(s): State<Served>| async move { Json(s.tracker.status()) }),
        )
        .route(
            "/journal",
            get(
                |State(s): State<Served>, Query(q): Query<From>| async move {
                    let t = s.tracker.clone();
                    match tokio::task::spawn_blocking(move || t.journal_from(q.from)).await {
                        Ok(Ok(b)) => {
                            ([(header::CONTENT_TYPE, "application/x-ndjson")], b).into_response()
                        }
                        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                    }
                },
            ),
        )
        .layer(middleware::from_fn_with_state(s.clone(), allow_only))
        .with_state(s)
}

/// The first wait before the status server is started again; it doubles
/// to [`RESTART_MAX`].
pub const RESTART_FIRST: std::time::Duration = std::time::Duration::from_secs(5);
pub const RESTART_MAX: std::time::Duration = std::time::Duration::from_secs(300);

/// Serve the status on `listen` to `allow` until `stop`, supervised: a
/// failure to bind, an error while serving or a panic is logged, counted
/// in `ocr.status_restarts` and followed by a restart after a backoff
/// (`first`, doubling to [`RESTART_MAX`], back to `first` after an hour
/// up). It never touches the OCR loop: the worker runs on whatever happens
/// here.
pub fn supervise(
    listen: SocketAddr,
    tracker: Arc<Tracker>,
    allow: Vec<String>,
    stop: tokio::sync::watch::Receiver<bool>,
    metrics: crate::telemetry::Metrics,
    first: std::time::Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut wait = first;
        loop {
            let started = std::time::Instant::now();
            let run = tokio::spawn(serve_once(
                listen,
                tracker.clone(),
                allow.clone(),
                stop.clone(),
            ));
            let cause = match run.await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(("error", e.to_string())),
                Err(e) => Some(("panic", e.to_string())),
            };
            if *stop.borrow() {
                return;
            }
            let (cause, error) = cause.unwrap_or(("stopped", String::new()));
            if started.elapsed() > std::time::Duration::from_secs(3600) {
                wait = first;
            }
            metrics
                .status_restarts
                .add(1, &[opentelemetry::KeyValue::new("cause", cause)]);
            tracing::info_span!("ocr_status").in_scope(|| {
                tracing::error!(
                    cause,
                    error,
                    retry_secs = wait.as_secs(),
                    "the status server failed; starting it again"
                )
            });
            let mut stop_wait = stop.clone();
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = stop_wait.wait_for(|s| *s) => return,
            }
            wait = (wait * 2).min(RESTART_MAX);
        }
    })
}

/// Bind and serve until `stop` or an error.
/// The addresses of `hosts`: each an address, or a name resolved now.
pub async fn resolve(hosts: &[String]) -> anyhow::Result<Vec<IpAddr>> {
    let mut out = Vec::new();
    for h in hosts {
        match h.parse::<IpAddr>() {
            Ok(ip) => out.push(ip.to_canonical()),
            Err(_) => {
                let addrs = tokio::net::lookup_host((h.as_str(), 0))
                    .await
                    .map_err(|e| anyhow::anyhow!("resolving {h}: {e}"))?;
                out.extend(addrs.map(|a| a.ip().to_canonical()));
            }
        }
    }
    anyhow::ensure!(!out.is_empty(), "no address to serve the status to");
    Ok(out)
}

async fn serve_once(
    listen: SocketAddr,
    tracker: Arc<Tracker>,
    hosts: Vec<String>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let allow = resolve(&hosts).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|e| anyhow::anyhow!("binding the status port {listen}: {e}"))?;
    tracing::info_span!("ocr_status")
        .in_scope(|| tracing::info!(%listen, allow = ?allow, "status listening"));
    axum::serve(
        listener,
        router(tracker, allow).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = stop.wait_for(|s| *s).await;
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::Request as HttpRequest;
    use tower::ServiceExt;

    fn page(doc: &str, outcome: &str, scan: &str) -> Completion {
        Completion {
            doc_id: doc.into(),
            language: "deu".into(),
            engine: "tesseract/5.5.0 profile=frak2021 image=sha256:0123456789ab".into(),
            queue: "ocr-bulk".into(),
            reason: "bulk".into(),
            outcome: outcome.into(),
            scan: scan.into(),
            uploaded: outcome == "done",
            ..Completion::default()
        }
    }

    async fn get(tracker: Arc<Tracker>, from: &str, uri: &str) -> (StatusCode, Vec<u8>) {
        let peer: SocketAddr = format!("{from}:5000").parse().unwrap();
        let r = router(tracker, vec!["203.0.113.5".parse().unwrap()])
            .layer(MockConnectInfo(peer))
            .oneshot(HttpRequest::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let s = r.status();
        (
            s,
            axum::body::to_bytes(r.into_body(), 1 << 22)
                .await
                .unwrap()
                .to_vec(),
        )
    }

    #[tokio::test]
    async fn tracks_pages_and_serves_its_journal_to_the_dashboard_only() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.jsonl");
        let about = About {
            node_id: "usnm-ocr-node1".into(),
            concurrency: 4,
            engine: "usnm-tesseract".into(),
            ..About::default()
        };
        let t = Arc::new(Tracker::new(about.clone(), Some(journal.clone()), None).unwrap());
        t.begin("m1", "sn1_1890-01-01_ed-1_seq-1", "deu", "ocr-bulk");
        t.begin("m2", "sn1_1890-01-01_ed-1_seq-2", "deu", "ocr-bulk");
        // The same page again, under another message: tracked apart.
        t.begin("m3", "sn1_1890-01-01_ed-1_seq-2", "deu", "ocr-bulk");
        t.stage("m2", "ocr");
        t.end("m1", page("sn1_1890-01-01_ed-1_seq-1", "done", "cache"));
        let s = t.status();
        assert_eq!(s.in_flight.len(), 2);
        assert!(s.in_flight.iter().any(|p| p.stage == "ocr"));
        assert_eq!(s.last_hour.done, 1);
        assert_eq!(s.last_hour.from_cache, 1);
        assert_eq!(s.routes["deu"], "frak2021");
        assert_eq!(s.recent.len(), 1);
        assert!(s.journal_bytes > 0);
        assert!(!s.journal_id.is_empty());
        t.end("m2", page("sn1_1890-01-01_ed-1_seq-2", "failed", "loc"));
        assert_eq!(t.status().in_flight.len(), 1);

        let (code, body) = get(t.clone(), "203.0.113.5", "/journal?from=0").await;
        assert_eq!(code, StatusCode::OK);
        let lines: Vec<Completion> = String::from_utf8(body.clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].outcome, "failed");
        let (_, rest) = get(
            t.clone(),
            "203.0.113.5",
            &format!("/journal?from={}", body.len()),
        )
        .await;
        assert!(rest.is_empty());
        let (code, _) = get(t.clone(), "203.0.113.50", "/status").await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, body) = get(t.clone(), "203.0.113.5", "/status").await;
        assert_eq!(code, StatusCode::OK);
        let s: WorkerStatus = serde_json::from_slice(&body).unwrap();
        assert_eq!(s.node_id, "usnm-ocr-node1");
        assert_eq!(s.since_start.failed, 1);

        // A restarted worker carries on with the same journal.
        drop(t);
        let t = Tracker::new(about, Some(journal), None).unwrap();
        t.end("m4", page("sn1_1890-01-01_ed-1_seq-3", "done", "loc"));
        let all = String::from_utf8(t.journal_from(0).unwrap()).unwrap();
        assert_eq!(all.lines().count(), 3);
    }

    #[tokio::test]
    async fn looks_at_the_queues_without_taking_anything() {
        use crate::message::{Cause, DeadLetter, OcrMessage, Reason};
        use crate::queue::LocalQueue;
        let dir = tempfile::tempdir().unwrap();
        let q = LocalQueue::new(dir.path());
        q.send(BULK, "x").await.unwrap();
        let m = OcrMessage {
            v: 1,
            page: "sn83045462/1890-05-03/ed-1/seq-7".into(),
            batch: "nbu_gage".into(),
            language: "deu".into(),
            reason: Reason::Bulk,
            score: None,
            queued_at: "2026-10-11T23:00:00Z".into(),
        };
        let d = DeadLetter::new(
            BULK,
            "m1",
            Some(&m),
            Cause::Permanent,
            None,
            None,
            "HTTP 404",
            1,
            "node1",
        );
        q.send(DEADLETTER, &d.encode()).await.unwrap();
        q.send(DEADLETTER, "not a dead letter").await.unwrap();
        let s = look_at_queues(&q).await;
        assert_eq!(s.counts[BULK], 1);
        assert_eq!(s.counts[DEADLETTER], 2);
        assert_eq!(s.dead_letters.len(), 2);
        let v = s
            .dead_letters
            .iter()
            .find(|v| v.message_id == "m1")
            .unwrap();
        assert_eq!(v.doc_id, "sn83045462_1890-05-03_ed-1_seq-7");
        assert_eq!(v.cause, "permanent");
        assert_eq!(v.error, "HTTP 404");
        assert!(s
            .dead_letters
            .iter()
            .any(|v| v.doc_id.is_empty() && v.error.contains("can't read")));
        // Nothing was taken.
        assert_eq!(q.approximate_count(DEADLETTER).await.unwrap(), 2);
        assert!(s.error.is_none());
    }

    #[tokio::test]
    async fn the_status_server_comes_back_when_its_port_frees() {
        // Someone else holds the port: the server can't bind, and keeps
        // trying; the worker isn't touched.
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = held.local_addr().unwrap();
        let t = Arc::new(Tracker::quiet());
        let (tx, rx) = tokio::sync::watch::channel(false);
        let task = supervise(
            addr,
            t,
            vec!["localhost".to_owned()],
            rx,
            crate::telemetry::Metrics::global(),
            std::time::Duration::from_millis(50),
        );
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert!(!task.is_finished());
        drop(held);
        let url = format!("http://{addr}/status");
        let mut ok = false;
        for _ in 0..50 {
            if let Ok(r) = reqwest::get(&url).await {
                ok = r.status().is_success();
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ok, "the status server didn't come back");
        tx.send_replace(true);
        tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn a_replaced_journal_is_written_to_where_it_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.jsonl");
        let t = Tracker::new(About::default(), Some(journal.clone()), None).unwrap();
        t.end("m1", page("sn1_1890-01-01_ed-1_seq-1", "done", "loc"));
        let before = t.status().journal_id;
        std::fs::rename(&journal, dir.path().join("old.jsonl")).unwrap();
        std::fs::write(&journal, "").unwrap();
        t.end("m2", page("sn1_1890-01-01_ed-1_seq-2", "done", "loc"));
        let s = t.status();
        assert_ne!(s.journal_id, before);
        let all = String::from_utf8(t.journal_from(0).unwrap()).unwrap();
        assert_eq!(all.lines().count(), 1);
        assert!(all.contains("seq-2"));
    }

    #[test]
    fn a_journal_read_stops_at_a_whole_line() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("journal.jsonl");
        std::fs::write(&journal, "{\"a\":1}\n{\"b\":").unwrap();
        let t = Tracker::new(About::default(), Some(journal), None).unwrap();
        assert_eq!(t.journal_from(0).unwrap(), b"{\"a\":1}\n");
        assert!(t.journal_from(100).unwrap().is_empty());
        assert!(Tracker::quiet().journal_from(0).unwrap().is_empty());
    }
}
