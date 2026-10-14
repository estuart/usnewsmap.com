//! The worker loop: take a page, fetch its scan, read it, write ALTO and an
//! overlay part, delete the message. Messages are hidden for a fixed time
//! when taken and never renewed (ADR-0015).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use opentelemetry::KeyValue;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tracing::field::Empty;
use tracing::Instrument;
use usnm_core::ids::PageKey;
use usnm_overlay::OverlayRow;
use usnm_store::{ObjectStore, StoreError};

use crate::engine::{exited_136, Engine, EngineError, EngineOutput};
use crate::loc::{FetchError, LocClient, Scan, ScanFormat};
use crate::message::{self, Cause, DeadLetter, OcrMessage};
use crate::queue::{random_token, LeaseLost, Queue, Received, BULK, DEADLETTER, PRIORITY};
use crate::status::{Completion, Tracker};
use crate::telemetry::{page_attrs, Metrics};

/// Where a page's scan comes from (loc.gov, or a stand-in in tests).
#[async_trait]
pub trait ScanSource: Send + Sync + std::fmt::Debug {
    /// The page's full scan. `batch` (the message's) finds it in the cache.
    async fn scan(&self, key: &PageKey, batch: &str) -> Result<Scan, FetchError>;
    /// Until when the source holds every request (loc.gov said to wait).
    fn held_until(&self) -> Option<tokio::time::Instant> {
        None
    }
    /// The page's full-size IIIF JPEG (`<iiif_base>/full/full/0/default.jpg`),
    /// read when the engine crashes on its JP2 (#454).
    async fn jpeg(&self, key: &PageKey, batch: &str, iiif_base: &str) -> Result<Scan, FetchError>;
    /// Delete a buffer-set scan the page no longer needs (best effort).
    async fn evict(&self, _url: &str) {}
}

#[async_trait]
impl ScanSource for LocClient {
    async fn scan(&self, key: &PageKey, _batch: &str) -> Result<Scan, FetchError> {
        LocClient::scan(self, key).await
    }

    async fn jpeg(
        &self,
        _key: &PageKey,
        _batch: &str,
        iiif_base: &str,
    ) -> Result<Scan, FetchError> {
        self.iiif_scan(iiif_base.to_owned(), crate::loc::ImageSize::Full)
            .await
    }

    fn held_until(&self) -> Option<tokio::time::Instant> {
        LocClient::held_until(self)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Names this node in output names and logs: `[A-Za-z0-9-]`, 1 to 32.
    pub node_id: String,
    pub concurrency: usize,
    /// How long a received message stays hidden. It isn't renewed (the
    /// nodes can't update messages, ADR-0015), so it must cover the slowest page.
    pub visibility: Duration,
    /// A page not done by this share of `visibility` after it was received
    /// is abandoned without output, so it reappears cleanly for another node.
    pub abandon_at: f64,
    /// A message received more often than this goes to `ocr-deadletter`.
    pub max_dequeue: u32,
    /// The longest wait between polls when both queues are empty.
    pub idle_max: Duration,
    /// Stop once both queues are empty and nothing is in flight.
    pub once: bool,
    /// After SIGTERM, how long in-flight pages may take before they are
    /// abandoned (they reappear when their visibility runs out).
    pub shutdown_grace: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_id: "local".into(),
            concurrency: 1,
            visibility: Duration::from_secs(1800),
            abandon_at: 0.8,
            max_dequeue: 5,
            idle_max: Duration::from_secs(60),
            once: false,
            shutdown_grace: Duration::from_secs(60),
        }
    }
}

impl Config {
    /// How long after receiving a message its page may still write output.
    pub fn budget(&self) -> Duration {
        self.visibility.mul_f64(self.abandon_at.clamp(0.0, 1.0))
    }
}

pub fn check_node_id(id: &str) -> anyhow::Result<()> {
    if !id.is_empty()
        && id.len() <= 32
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        Ok(())
    } else {
        anyhow::bail!("--node-id must be 1 to 32 letters, digits or `-`")
    }
}

#[derive(Debug)]
enum Failure {
    /// Retrying won't help: dead-letter it.
    Permanent(String),
    /// Leave the message; it reappears when its visibility runs out. The
    /// first field is the kind, for metrics: fetch, engine, no_model,
    /// convert, output or scratch.
    Transient(&'static str, String),
    /// Out of time: no output written; the message reappears.
    Abandoned,
}

impl From<FetchError> for Failure {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Permanent(m) => Failure::Permanent(m),
            // An item loc.gov can't serve just now: the message comes
            // back later.
            FetchError::Transient(m) | FetchError::Item(m) => Failure::Transient("fetch", m),
        }
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Failure::Transient("output", format!("output store: {e}"))
    }
}

/// What was written for a page.
#[derive(Debug)]
pub struct Done {
    pub alto: String,
    pub part: String,
    pub chars: usize,
    pub status: &'static str,
    pub engine: String,
    /// The engine crashed on the JP2, and the IIIF JPEG was read (#454).
    pub jpeg_fallback: bool,
}

/// A scratch-file failure inside [`Worker::read_scan`], told apart from
/// the engine's own.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub(crate) struct ScratchError(pub(crate) String);

/// The page's failure for an engine error.
fn engine_failure(e: EngineError, node: &str) -> Failure {
    match e {
        // Another node may have the model: leave it, and let the dequeue
        // count dead-letter it if no node does.
        EngineError::Unsupported(l) => {
            Failure::Transient("no_model", format!("no model for `{l}` on node {node}"))
        }
        EngineError::Failed(e) if e.is::<ScratchError>() => {
            Failure::Transient("scratch", e.to_string())
        }
        EngineError::Failed(e) => Failure::Transient("engine", format!("{e:#}")),
    }
}

/// A page's output names, unique per node and moment (the nodes can create
/// blobs in `ocr-out` but not replace or delete them, ADR-0015):
/// `alto/<doc_id>.<node>.<stamp>.xml` and
/// `pages/<issue>.<node>.<stamp>.parquet`, with the same stamp.
pub fn output_paths(key: &PageKey, node: &str) -> (String, String) {
    let issue = format!(
        "{}_{}_ed-{}",
        key.lccn,
        key.date.format("%Y-%m-%d"),
        key.edition
    );
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
    (
        format!("alto/{}.{node}.{stamp}.xml", key.doc_id()),
        format!("pages/{issue}.{node}.{stamp}.parquet"),
    )
}

#[derive(Debug)]
pub struct Worker {
    pub queue: Arc<dyn Queue>,
    pub out: Arc<dyn ObjectStore>,
    pub scans: Arc<dyn ScanSource>,
    pub engine: Arc<dyn Engine>,
    pub cfg: Config,
    pub metrics: Metrics,
    /// What this worker is on and has finished, for the LAN dashboard.
    pub tracker: Arc<Tracker>,
}

/// A page in the tracker, ended as abandoned if dropped before its
/// outcome is known (the task was cancelled at shutdown).
struct Tracked {
    tracker: Arc<Tracker>,
    token: String,
    page: Option<Completion>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if let Some(c) = self.page.take() {
            self.tracker.end(&self.token, c);
        }
    }
}

/// How a message ended, for the tracker.
struct Ended {
    outcome: &'static str,
    engine: String,
    uploaded: bool,
    error_kind: Option<&'static str>,
    jpeg_fallback: bool,
}

impl Ended {
    /// A dead letter: `dead_lettered` once it is on `ocr-deadletter` and
    /// off its work queue, else `failed` (the message comes back).
    fn dead_lettered(moved: bool, cause: &'static str) -> Self {
        if moved {
            Ended::not_done("dead_lettered", cause)
        } else {
            Ended::not_done("failed", "dead_letter")
        }
    }

    fn not_done(outcome: &'static str, error_kind: &'static str) -> Self {
        Ended {
            outcome,
            engine: String::new(),
            uploaded: false,
            error_kind: Some(error_kind),
            jpeg_fallback: false,
        }
    }
}

impl Worker {
    /// The next message, `ocr-priority` first.
    async fn next_message(&self) -> anyhow::Result<Option<Received>> {
        for q in [PRIORITY, BULK] {
            if let Some(m) = self.queue.receive(q, self.cfg.visibility).await? {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    /// Run until `stop` turns true (or, with `once`, the queues are empty).
    /// In-flight pages get `shutdown_grace` to finish; then they're
    /// abandoned, and reappear when their visibility runs out.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) -> anyhow::Result<()> {
        let slots = Arc::new(Semaphore::new(self.cfg.concurrency.max(1)));
        let mut tasks = JoinSet::new();
        let mut idle = Duration::from_secs(1);
        tracing::info!(
            node = %self.cfg.node_id,
            concurrency = self.cfg.concurrency,
            visibility_secs = self.cfg.visibility.as_secs(),
            "worker started"
        );
        loop {
            while tasks.try_join_next().is_some() {}
            let permit = tokio::select! {
                p = slots.clone().acquire_owned() => p?,
                _ = stop.changed() => break,
            };
            if *stop.borrow() {
                break;
            }
            // While loc.gov holds this node's requests for more than half a
            // new page's budget, a page taken now would mostly wait, and
            // could be abandoned and spend one of its receives; take none
            // until at least half the budget is left after the hold, for
            // the fetch and the OCR.
            if let Some(until) = self.scans.held_until() {
                let resume = until.checked_sub(self.cfg.budget() / 2);
                if let Some(resume) = resume.filter(|r| *r > tokio::time::Instant::now()) {
                    drop(permit);
                    tracing::info_span!("ocr_hold").in_scope(|| {
                        tracing::warn!(
                            held_secs = until.duration_since(tokio::time::Instant::now()).as_secs(),
                            "loc.gov holds requests past a page's budget; taking no pages until it eases"
                        )
                    });
                    tokio::select! {
                        _ = tokio::time::sleep_until(resume) => {}
                        _ = stop.changed() => break,
                    }
                    continue;
                }
            }
            let next = tokio::select! {
                r = self.next_message() => r,
                // A message received as this is dropped reappears when its visibility runs out.
                _ = stop.changed() => break,
            };
            match next {
                Ok(Some(msg)) => {
                    idle = Duration::from_secs(1);
                    let me = self.clone();
                    let received = Instant::now();
                    tasks.spawn(async move {
                        me.handle(msg, received).await;
                        drop(permit);
                    });
                    continue;
                }
                Ok(None) => {
                    drop(permit);
                    self.metrics.empty_polls.add(1, &[]);
                    if self.cfg.once && tasks.is_empty() {
                        tracing::info!("queues empty; stopping (--once)");
                        break;
                    }
                }
                Err(e) => {
                    drop(permit);
                    // In a span of its own, so the warning reaches
                    // Application Insights too.
                    tracing::info_span!("ocr_receive")
                        .in_scope(|| tracing::warn!(error = format!("{e:#}"), "receive failed"));
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(idle) => {}
                _ = stop.changed() => break,
            }
            idle = (idle * 2).min(self.cfg.idle_max);
        }
        if !tasks.is_empty() {
            tracing::info!(
                in_flight = tasks.len(),
                grace_secs = self.cfg.shutdown_grace.as_secs(),
                "stopping; finishing in-flight pages"
            );
            let drained = tokio::time::timeout(self.cfg.shutdown_grace, async {
                while tasks.join_next().await.is_some() {}
            })
            .await;
            if drained.is_err() {
                let n = tasks.len();
                self.metrics
                    .pages_abandoned
                    .add(n as u64, &[KeyValue::new("cause", "shutdown")]);
                tracing::info_span!("ocr_shutdown").in_scope(|| {
                    tracing::warn!(
                        in_flight = n,
                        "grace over; abandoning in-flight pages, which reappear when their visibility runs out"
                    )
                });
                tasks.shutdown().await;
            }
        }
        tracing::info!("worker stopped");
        Ok(())
    }

    /// Move `msg` to `ocr-deadletter`. The dead letter keeps the message
    /// only if it was valid (`valid`, re-encoded from its checked fields):
    /// a message that failed its checks might carry anything, search text
    /// included, so only its id and what was wrong with it (`kind`) are
    /// kept.
    async fn dead_letter(
        &self,
        msg: &Received,
        cause: Cause,
        kind: Option<&'static str>,
        error: &str,
        valid: Option<&OcrMessage>,
    ) -> bool {
        let body = DeadLetter::new(
            &msg.queue,
            &msg.id,
            valid,
            cause,
            kind,
            None,
            error,
            msg.dequeue_count,
            &self.cfg.node_id,
        );
        let text = body.encode();
        if let Err(e) = self.queue.send(DEADLETTER, &text).await {
            // Leave the message: it comes back and is dead-lettered again.
            tracing::error!(error = format!("{e:#}"), "dead-letter send failed");
            return false;
        }
        match self.queue.delete(msg).await {
            Ok(()) => {
                self.metrics.dead_lettered.add(
                    1,
                    &[
                        KeyValue::new("queue", msg.queue.clone()),
                        KeyValue::new("cause", cause.as_str()),
                    ],
                );
                tracing::warn!(queue = %msg.queue, message_id = %msg.id, cause = cause.as_str(), kind, error, "dead-lettered");
                true
            }
            Err(e) => {
                tracing::error!(error = format!("{e:#}"), "delete after dead-letter failed");
                false
            }
        }
    }

    /// Do one message, received at `received`, start to finish; never
    /// fails (the outcome is in the queue). One span per message (a request
    /// in Application Insights), named by its page id only.
    pub async fn handle(&self, msg: Received, received: Instant) {
        let span = tracing::info_span!(
            "ocr_page",
            otel.kind = "consumer",
            queue = %msg.queue,
            message_id = %msg.id,
            dequeue_count = msg.dequeue_count,
            page = Empty,
            language = Empty,
            reason = Empty,
            outcome = Empty,
            otel.status_code = Empty,
        );
        let page = message::decode(&msg.text)
            .ok()
            .map(|(m, k)| (k.doc_id(), m.language, m.reason.as_str().to_owned()));
        // Ends the page in the tracker however the task ends: with its
        // outcome, or as abandoned at shutdown if the task is dropped.
        // One attempt: the message as received this time (its id and pop
        // receipt), so a redelivered message running beside an earlier
        // attempt is tracked apart.
        let attempt = format!("{}/{}", msg.id, msg.pop_receipt);
        let mut guard = page.map(|(doc_id, language, reason)| {
            self.tracker.begin(&attempt, &doc_id, &language, &msg.queue);
            Tracked {
                tracker: self.tracker.clone(),
                token: attempt.clone(),
                page: Some(Completion {
                    doc_id,
                    language,
                    queue: msg.queue.clone(),
                    reason,
                    outcome: "abandoned".into(),
                    error_kind: Some("shutdown".into()),
                    ..Completion::default()
                }),
            }
        });
        let ended = self
            .handle_in(msg, &attempt, received, &span)
            .instrument(span.clone())
            .await;
        if let Some((g, mut c)) = guard
            .as_mut()
            .and_then(|g| g.page.take().map(|c| (g.token.clone(), c)))
        {
            c.engine = ended.engine;
            c.outcome = ended.outcome.into();
            c.uploaded = ended.uploaded;
            c.error_kind = ended.error_kind.map(Into::into);
            c.jpeg_fallback = ended.jpeg_fallback;
            self.tracker.end(&g, c);
        }
    }

    async fn handle_in(
        &self,
        msg: Received,
        attempt: &str,
        received: Instant,
        span: &tracing::Span,
    ) -> Ended {
        let decoded = message::decode(&msg.text);
        if let Ok((m, key)) = &decoded {
            span.record("page", tracing::field::display(key));
            span.record("language", m.language.as_str());
            span.record("reason", m.reason.as_str());
        }
        let outcome = |o: &'static str| {
            span.record("outcome", o);
            span.record("otel.status_code", if o == "done" { "ok" } else { "error" });
        };
        if msg.dequeue_count > self.cfg.max_dequeue {
            let why = format!("received {} times", msg.dequeue_count);
            let valid = decoded.as_ref().ok().map(|(m, _)| m);
            let moved = self
                .dead_letter(&msg, Cause::DequeueCount, None, &why, valid)
                .await;
            outcome("dead_lettered");
            return Ended::dead_lettered(moved, "dequeue_count");
        }
        let (m, key) = match decoded {
            Ok(v) => v,
            Err(e) => {
                let why = format!("invalid message: {}", e.kind());
                let moved = self
                    .dead_letter(&msg, Cause::Invalid, Some(e.kind()), &why, None)
                    .await;
                outcome("dead_lettered");
                return Ended::dead_lettered(moved, "invalid");
            }
        };
        let deadline = received + self.cfg.budget();
        let page = key.to_string();
        let attrs = page_attrs(
            self.engine.source(),
            &m.language,
            m.reason.as_str(),
            &msg.queue,
        );
        match self.process(&m, &key, attempt, deadline).await {
            Ok(done) => {
                let ended = Ended {
                    outcome: "done",
                    engine: done.engine.clone(),
                    uploaded: true,
                    error_kind: None,
                    jpeg_fallback: done.jpeg_fallback,
                };
                self.metrics.pages_done.add(1, &attrs);
                outcome("done");
                tracing::info!(
                    page, queue = %msg.queue, reason = m.reason.as_str(), language = %m.language,
                    engine = %done.engine, status = done.status, chars = done.chars,
                    jpeg_fallback = done.jpeg_fallback,
                    alto = %done.alto, part = %done.part,
                    secs = received.elapsed().as_secs_f64(), "page done"
                );
                if let Err(e) = self.queue.delete(&msg).await {
                    if e.downcast_ref::<LeaseLost>().is_some() {
                        tracing::warn!(
                            page,
                            "message was gone at delete; another node may redo it"
                        );
                    } else {
                        tracing::error!(
                            page,
                            error = format!("{e:#}"),
                            "delete failed; the page will be redone"
                        );
                    }
                }
                ended
            }
            Err(Failure::Permanent(e)) => {
                let moved = self
                    .dead_letter(&msg, Cause::Permanent, None, &e, Some(&m))
                    .await;
                outcome("dead_lettered");
                Ended::dead_lettered(moved, "permanent")
            }
            Err(Failure::Transient(kind, e)) => {
                let mut attrs = attrs;
                attrs.push(KeyValue::new("error", kind));
                self.metrics.pages_failed.add(1, &attrs);
                outcome("failed");
                tracing::warn!(
                    page, error = %e, kind, dequeue_count = msg.dequeue_count,
                    "page failed; it reappears when its visibility runs out"
                );
                Ended::not_done("failed", kind)
            }
            Err(Failure::Abandoned) => {
                let mut attrs = attrs;
                attrs.push(KeyValue::new("cause", "budget"));
                self.metrics.pages_abandoned.add(1, &attrs);
                outcome("abandoned");
                tracing::warn!(
                    page,
                    budget_secs = self.cfg.budget().as_secs(),
                    "page abandoned: out of time, nothing written; it reappears for another node"
                );
                Ended::not_done("abandoned", "budget")
            }
        }
    }

    async fn process(
        &self,
        m: &OcrMessage,
        key: &PageKey,
        token: &str,
        deadline: Instant,
    ) -> Result<Done, Failure> {
        let work = tempfile::tempdir()
            .map_err(|e| Failure::Transient("scratch", format!("scratch dir: {e}")))?;
        // Another node may have the model: leave the message, and let the
        // dequeue count dead-letter it if no node does. Asked before the
        // scan is fetched, so it costs loc.gov nothing.
        if !self.engine.can_read(&m.language) {
            return Err(Failure::Transient(
                "no_model",
                format!("no model for `{}` on node {}", m.language, self.cfg.node_id),
            ));
        }
        let read = async {
            let t = Instant::now();
            let scan = self.scans.scan(key, &m.batch).await;
            self.metrics.image_fetch_seconds.record(
                t.elapsed().as_secs_f64(),
                &[
                    KeyValue::new("outcome", if scan.is_ok() { "ok" } else { "error" }),
                    KeyValue::new(
                        "source",
                        match &scan {
                            Ok(s) if s.from_cache => "cache",
                            _ => "loc",
                        },
                    ),
                ],
            );
            let scan = scan?;
            self.tracker.scanned(token, scan.from_cache);
            self.tracker.stage(token, "ocr");
            if scan.from_cache {
                tracing::debug!(doc_id = %key.doc_id(), format = scan.format.name(), "scan read from the cache");
            }
            let t = Instant::now();
            let mut jpeg_fallback = false;
            let mut evict_jp2 = None;
            let (scan, out) = match self.read_scan(&scan, m, work.path()).await {
                Ok(out) => (scan, out),
                // Some intact JP2s crash the engine with status 136 (#454);
                // their IIIF JPEG reads. One more run on it; no second
                // fallback, and its failure is the page's, retried as any.
                Err(EngineError::Failed(e)) if scan.format == ScanFormat::Jp2 && exited_136(&e) => {
                    tracing::warn!(
                        doc_id = %key.doc_id(), error = format!("{e:#}"),
                        "the engine crashed on the JP2 (status 136); reading the IIIF JPEG instead"
                    );
                    let jpeg = self.scans.jpeg(key, &m.batch, &scan.iiif_base).await?;
                    // The scan read is the JPEG now, wherever it came from.
                    self.tracker.scanned(token, jpeg.from_cache);
                    // A directory of its own, so nothing the crashed run
                    // left behind is read as this run's output.
                    let again = work.path().join("jpeg");
                    tokio::fs::create_dir(&again)
                        .await
                        .map_err(|e| Failure::Transient("scratch", format!("scratch dir: {e}")))?;
                    let out = self
                        .read_scan(&jpeg, m, &again)
                        .await
                        .map_err(|e| engine_failure(e, &self.cfg.node_id))?;
                    // The JP2's buffer copy goes with the JPEG's, after the upload.
                    evict_jp2 = scan.evict.clone();
                    jpeg_fallback = true;
                    (jpeg, out)
                }
                Err(e) => return Err(engine_failure(e, &self.cfg.node_id)),
            };
            let scan_sha256 = crate::prefetch::sha256(&scan.bytes);
            self.metrics.ocr_seconds.record(
                t.elapsed().as_secs_f64(),
                &[
                    KeyValue::new("engine", self.engine.source().to_owned()),
                    // The short key (`tesseract/5.5.0 frak2021`), so the
                    // attribute doesn't grow with image and model digests.
                    KeyValue::new(
                        "models",
                        usnm_core::ocr_engine::EngineTag::parse(&out.engine)
                            .map_or_else(|_| out.engine.clone(), |t| t.short_key()),
                    ),
                    KeyValue::new("language", m.language.clone()),
                ],
            );
            Ok((
                scan.iiif_base,
                scan.url,
                scan_sha256,
                scan.md5,
                scan.evict,
                out,
                jpeg_fallback,
                evict_jp2,
            ))
        };
        let (iiif_base, scan_url, scan_sha256, scan_md5, evict, out, jpeg_fallback, evict_jp2) =
            tokio::time::timeout_at(deadline.into(), read)
                .await
                .map_err(|_| Failure::Abandoned)??;
        // The stored ALTO is written from the engine's parsed ALTO, and the
        // text is derived from it in reading order (13 §13.4.7), so the two
        // hold the same words. The ALTO names the file read (the IIIF JPEG
        // or the batch's JP2) and its sha256, and a JP2's bag md5; the
        // overlay row names the page by its IIIF base either way.
        let source = crate::stored::Source {
            url: &scan_url,
            sha256: &scan_sha256,
            md5: scan_md5.as_deref(),
        };
        let mut out = out;
        let stored = crate::stored::from_output(
            &mut out,
            &m.language,
            self.engine.dpi(),
            &source,
            &crate::stored::worker_version(),
            Utc::now(),
        )
        .map_err(|e| Failure::Transient("convert", e.to_string()))?;
        let (text, alto) = (stored.text, stored.alto);
        let row = OverlayRow {
            key: key.clone(),
            batch: m.batch.clone(),
            ocr_source: self.engine.source().to_owned(),
            ocr_engine: out.engine.clone(),
            // The overlay's `loc_text` says why the page was OCRed: here,
            // the queue's reason (`bulk` or `user`).
            loc_text: m.reason.as_str().to_owned(),
            text,
            image_url: iiif_base,
            ocred_at: Utc::now(),
        };
        let status = usnm_overlay::status_str(row.status());
        let chars = row.text.chars().count();
        let part = usnm_overlay::write_part(std::slice::from_ref(&row))
            .map_err(|e| Failure::Transient("output", format!("overlay part: {e:#}")))?;
        // The uploads are cut off at the deadline too: past it the message
        // may be with another node already. An ALTO without its part is
        // harmless; parts are only read with their ALTO.
        self.tracker.stage(token, "upload");
        let (alto_path, part_path) =
            tokio::time::timeout_at(deadline.into(), self.write_output(key, alto, part))
                .await
                .map_err(|_| Failure::Abandoned)??;
        // The output is uploaded: a buffer-set scan isn't needed any more.
        // Best effort; a failure is logged, never the page's.
        for url in evict.into_iter().chain(evict_jp2) {
            // Within the page's deadline, so a slow server can't push the
            // message past its hide.
            let _ = tokio::time::timeout_at(deadline.into(), self.scans.evict(&url)).await;
        }
        Ok(Done {
            alto: alto_path,
            part: part_path,
            chars,
            status,
            engine: out.engine,
            jpeg_fallback,
        })
    }

    /// Write `scan` to the scratch dir and read it.
    async fn read_scan(
        &self,
        scan: &Scan,
        m: &OcrMessage,
        work: &Path,
    ) -> Result<EngineOutput, EngineError> {
        // The extension names the format: Tesseract (Leptonica) reads
        // JPEG and, through OpenJPEG, JP2.
        let image = work.join(format!("page.{}", scan.format.ext()));
        tokio::fs::write(&image, &scan.bytes)
            .await
            .map_err(|e| anyhow::anyhow!(ScratchError(format!("scratch image: {e}"))))?;
        self.engine.ocr(&image, &m.language, work).await
    }

    /// Write a page's ALTO, then its part, create-only under fresh names.
    async fn write_output(
        &self,
        key: &PageKey,
        alto: String,
        part: Vec<u8>,
    ) -> Result<(String, String), Failure> {
        for _ in 0..3 {
            let (alto_path, part_path) = output_paths(key, &self.cfg.node_id);
            // ALTO first, as the Japanese job does: a part is only written
            // once its page's ALTO is there. Both are create-only.
            if !self
                .out
                .put_new(&alto_path, alto.clone().into_bytes(), "application/xml")
                .await?
            {
                continue;
            }
            if self
                .out
                .put_new(&part_path, part.clone(), "application/vnd.apache.parquet")
                .await?
            {
                return Ok((alto_path, part_path));
            }
        }
        Err(Failure::Transient(
            "output",
            format!("no free output name for {key} ({})", random_token()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Reason;
    use crate::queue::LocalQueue;

    /// Counts scan requests; a hold, when set, is reported as loc.gov's.
    /// Pages 95 to 97 come as JP2s (see [`FakeEngine`]).
    #[derive(Debug, Default)]
    struct FakeScans {
        requests: std::sync::atomic::AtomicUsize,
        jpeg_requests: std::sync::atomic::AtomicUsize,
        held_until: std::sync::Mutex<Option<tokio::time::Instant>>,
    }

    #[async_trait]
    impl ScanSource for FakeScans {
        fn held_until(&self) -> Option<tokio::time::Instant> {
            *self.held_until.lock().unwrap()
        }

        async fn scan(&self, key: &PageKey, _batch: &str) -> Result<Scan, FetchError> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if key.seq == 99 {
                return Err(FetchError::Permanent("no image".into()));
            }
            if key.seq == 98 {
                return Err(FetchError::Transient("HTTP 500".into()));
            }
            let iiif_base = format!(
                "https://tile.loc.gov/image-services/iiif/service:ndnp:x:{}",
                key.seq
            );
            if (95..=97).contains(&key.seq) {
                let body: &'static [u8] = match key.seq {
                    95 => b"jp2 exit 1",
                    _ => b"jp2 exit 136",
                };
                return Ok(Scan {
                    url: format!(
                        "https://tile.loc.gov/storage-services/service/x/{}.jp2",
                        key.seq
                    ),
                    iiif_base,
                    format: crate::loc::ScanFormat::Jp2,
                    bytes: bytes::Bytes::from_static(body),
                    md5: Some("0123".into()),
                    download: std::time::Duration::ZERO,
                    from_cache: false,
                    evict: None,
                });
            }
            Ok(Scan {
                url: format!("{iiif_base}/full/full/0/default.jpg"),
                iiif_base,
                format: crate::loc::ScanFormat::Jpeg,
                bytes: bytes::Bytes::from_static(b"jpeg"),
                md5: None,
                download: std::time::Duration::ZERO,
                from_cache: false,
                evict: None,
            })
        }

        /// Page 96's JPEG crashes the engine too.
        async fn jpeg(
            &self,
            key: &PageKey,
            _batch: &str,
            iiif_base: &str,
        ) -> Result<Scan, FetchError> {
            self.jpeg_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Scan {
                url: format!("{iiif_base}/full/full/0/default.jpg"),
                iiif_base: iiif_base.into(),
                format: crate::loc::ScanFormat::Jpeg,
                bytes: bytes::Bytes::from_static(if key.seq == 96 {
                    b"jpeg exit 136"
                } else {
                    b"jpeg"
                }),
                md5: None,
                download: std::time::Duration::ZERO,
                from_cache: false,
                evict: None,
            })
        }
    }

    /// An engine exit with `code`, as Tesseract's would be.
    fn exited(code: i32) -> EngineError {
        use std::os::unix::process::ExitStatusExt;
        EngineError::Failed(anyhow::Error::new(crate::engine::Exited {
            status: std::process::ExitStatus::from_raw(code << 8),
            stderr: String::new(),
        }))
    }

    /// Reads `jpeg`; a scan whose bytes say `exit <code>` exits with it.
    #[derive(Debug)]
    struct FakeEngine;

    #[async_trait]
    impl Engine for FakeEngine {
        fn source(&self) -> &str {
            "usnm-tesseract"
        }
        /// Every language but Hawaiian.
        fn can_read(&self, language: &str) -> bool {
            language != "haw"
        }
        async fn ocr(
            &self,
            image: &Path,
            language: &str,
            _: &Path,
        ) -> Result<EngineOutput, EngineError> {
            let bytes = std::fs::read(image).unwrap();
            if let Some(code) = std::str::from_utf8(&bytes)
                .unwrap()
                .split_once(" exit ")
                .map(|(_, c)| c.parse().unwrap())
            {
                return Err(exited(code));
            }
            assert_eq!(bytes, b"jpeg");
            if language == "haw" {
                return Err(EngineError::Unsupported(language.into()));
            }
            Ok(EngineOutput {
                text: "Die  Nebraska Staats-Zeitung\nerscheint  jeden\u{0007} Sonn-\nabend.\n"
                    .into(),
                alto: FAKE_ALTO.as_bytes().to_vec(),
                page: None,
                layout_from: "ALTO",
                engine: "tesseract-5.5.1/deu_latf+deu".into(),
            })
        }
    }

    /// ALTO as Tesseract 5 writes it, with the fake engine's text.
    const FAKE_ALTO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<alto xmlns="http://www.loc.gov/standards/alto/ns-v3#" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.loc.gov/standards/alto/ns-v3# http://www.loc.gov/alto/v3/alto-3-0.xsd">
 <Description><MeasurementUnit>pixel</MeasurementUnit><sourceImageInformation><fileName>/tmp/page.jpg</fileName></sourceImageInformation>
  <OCRProcessing ID="OCR_0"><ocrProcessingStep><processingSoftware><softwareName>tesseract 5.5.1</softwareName></processingSoftware></ocrProcessingStep></OCRProcessing></Description>
 <Layout><Page WIDTH="1000" HEIGHT="800" PHYSICAL_IMG_NR="0" ID="page_0"><PrintSpace HPOS="0" VPOS="0" WIDTH="1000" HEIGHT="800">
  <ComposedBlock ID="cblock_0" HPOS="10" VPOS="10" WIDTH="500" HEIGHT="100"><TextBlock ID="block_0" HPOS="10" VPOS="10" WIDTH="500" HEIGHT="100">
   <TextLine ID="line_0" HPOS="10" VPOS="10" WIDTH="500" HEIGHT="30"><String ID="string_0" HPOS="10" VPOS="10" WIDTH="40" HEIGHT="30" WC="0.96" CONTENT="Die"/><SP WIDTH="10" VPOS="10" HPOS="50"/><String ID="string_1" HPOS="60" VPOS="10" WIDTH="150" HEIGHT="30" WC="0.95" CONTENT="Nebraska"/><SP WIDTH="10" VPOS="10" HPOS="210"/><String ID="string_2" HPOS="220" VPOS="10" WIDTH="290" HEIGHT="30" WC="0.9" CONTENT="Staats-Zeitung"/></TextLine>
   <TextLine ID="line_1" HPOS="10" VPOS="45" WIDTH="400" HEIGHT="30"><String ID="string_3" HPOS="10" VPOS="45" WIDTH="150" HEIGHT="30" WC="0.93" CONTENT="erscheint"/><SP WIDTH="10" VPOS="45" HPOS="160"/><String ID="string_4" HPOS="170" VPOS="45" WIDTH="100" HEIGHT="30" WC="0.91" CONTENT="jeden"/><SP WIDTH="10" VPOS="45" HPOS="270"/><String ID="string_5" HPOS="280" VPOS="45" WIDTH="100" HEIGHT="30" WC="0.88" CONTENT="Sonn-"/></TextLine>
   <TextLine ID="line_2" HPOS="10" VPOS="80" WIDTH="100" HEIGHT="30"><String ID="string_6" HPOS="10" VPOS="80" WIDTH="100" HEIGHT="30" WC="0.97" CONTENT="abend."/></TextLine>
  </TextBlock></ComposedBlock>
 </PrintSpace></Page></Layout>
</alto>
"#;

    fn msg(page: &str, language: &str) -> String {
        OcrMessage {
            v: 1,
            page: page.into(),
            batch: "nbu_gage".into(),
            language: language.into(),
            reason: Reason::Bulk,
            score: Some(0.62),
            queued_at: "2026-10-10T12:00:00Z".into(),
        }
        .encode()
    }

    fn worker(dir: &Path) -> (Arc<Worker>, Arc<LocalQueue>, Arc<dyn ObjectStore>) {
        let (w, queue, out, _) = worker_with_scans(dir);
        (w, queue, out)
    }

    fn worker_with_scans(
        dir: &Path,
    ) -> (
        Arc<Worker>,
        Arc<LocalQueue>,
        Arc<dyn ObjectStore>,
        Arc<FakeScans>,
    ) {
        let queue = Arc::new(LocalQueue::new(dir.join("q")));
        let out: Arc<dyn ObjectStore> = Arc::new(usnm_store::LocalStore::new(dir.join("out")));
        let scans = Arc::new(FakeScans::default());
        let w = Arc::new(Worker {
            queue: queue.clone(),
            out: out.clone(),
            scans: scans.clone(),
            engine: Arc::new(FakeEngine),
            cfg: Config {
                node_id: "node1".into(),
                concurrency: 2,
                once: true,
                ..Config::default()
            },
            metrics: Metrics::global(),
            tracker: Arc::new(Tracker::quiet()),
        });
        (w, queue, out, scans)
    }

    #[tokio::test]
    async fn writes_alto_and_a_part_the_release_reads() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = worker(dir.path());
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-99", "deu"))
            .await
            .unwrap();
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-8", "haw"))
            .await
            .unwrap();
        queue.send(BULK, "not a message").await.unwrap();
        // Refused for its extra field; the search text mustn't be kept.
        queue
            .send(
                BULK,
                r#"{"v":1,"page":"sn83045462/1890-05-03/ed-1/seq-9","batch":"nbu_gage","language":"deu","reason":"user","queued_at":"2026-10-10T12:00:00Z","q":"secret search"}"#,
            )
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(false);
        w.clone().run(rx).await.unwrap();
        // The tracker saw the page through, with its engine tag and scan
        // source, and holds nothing in flight.
        let st = w.tracker.status();
        assert_eq!(st.since_start.done, 1);
        assert_eq!(st.since_start.from_loc, 1);
        assert!(st.in_flight.is_empty());
        assert_eq!(st.recent[0].doc_id, "sn83045462_1890-05-03_ed-1_seq-7");
        assert!(st.recent[0].uploaded);

        let altos = out.list("alto").await.unwrap();
        assert_eq!(altos.len(), 1, "{altos:?}");
        let prefix = "alto/sn83045462_1890-05-03_ed-1_seq-7.node1.";
        let stamp = altos[0]
            .strip_prefix(prefix)
            .unwrap()
            .strip_suffix(".xml")
            .unwrap();
        let alto = out.get(&altos[0]).await.unwrap().unwrap();
        // The stored ALTO 3.1, written from the engine's: the scan's URL,
        // and the sha256 of the bytes read (ADR-0015 decision 9).
        let alto = String::from_utf8(alto).unwrap();
        assert!(alto.contains(&format!(
            "<fileName>https://tile.loc.gov/image-services/iiif/service:ndnp:x:7/full/full/0/default.jpg</fileName>\n      <fileIdentifier fileIdentifierLocation=\"sha256\">{}</fileIdentifier>",
            crate::prefetch::sha256(b"jpeg")
        )), "{alto}");
        assert!(
            alto.contains("alto-3-1.xsd")
                && alto.contains("<MeasurementUnit>pixel</MeasurementUnit>")
        );
        assert!(alto.contains("CONTENT=\"Staats-Zeitung\"") && !alto.contains("/tmp/page.jpg"));
        let parts = out.list("pages").await.unwrap();
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(
            parts[0],
            format!("pages/sn83045462_1890-05-03_ed-1.node1.{stamp}.parquet")
        );
        let bytes = out.get(&parts[0]).await.unwrap().unwrap();
        let pages = usnm_ingest::ocr_ja::read_part(bytes::Bytes::from(bytes), &parts[0]).unwrap();
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!(p.key.to_string(), "sn83045462/1890-05-03/ed-1/seq-7");
        assert_eq!(p.batch, "nbu_gage");
        assert_eq!(p.loc_text, "bulk");
        assert_eq!(p.ocr_source, "usnm-tesseract");
        assert_eq!(p.ocr_engine, "tesseract-5.5.1/deu_latf+deu");
        assert!(p.ok);
        // Normalized as LoC's text is (04 §4.5): hyphenation rejoined, spaces collapsed.
        assert_eq!(
            p.printed.as_deref(),
            Some("Die Nebraska Staats-Zeitung\nerscheint jeden Sonnabend.")
        );

        // Nothing visible: the bad ones are dead-lettered, and the page in
        // a language this node has no model for waits for another node.
        assert_eq!(
            queue.receive(BULK, Duration::from_secs(1)).await.unwrap(),
            None
        );
        let mut dead = Vec::new();
        let mut kept = Vec::new();
        while let Some(d) = queue
            .receive(DEADLETTER, Duration::from_secs(600))
            .await
            .unwrap()
        {
            use base64::Engine as _;
            let body: serde_json::Value = serde_json::from_slice(
                &base64::engine::general_purpose::STANDARD
                    .decode(&d.text)
                    .unwrap(),
            )
            .unwrap();
            assert!(!body.to_string().contains("secret"), "{body}");
            assert!(body["message_id"].is_string());
            // Each is a valid v2 dead letter, with its cause's fields.
            let parsed = message::decode_dead_letter(&d.text).unwrap();
            assert_eq!(parsed.v, 2);
            match parsed.cause.unwrap() {
                Cause::Invalid => assert_eq!(parsed.kind.as_deref(), Some("bad_json")),
                Cause::Permanent => assert!(parsed.kind.is_none() && parsed.message.is_some()),
                c => panic!("unexpected cause {c:?}"),
            }
            kept.push(body["message"]["page"].as_str().map(str::to_owned));
            dead.push(body["error"].as_str().unwrap().to_owned());
        }
        dead.sort();
        assert_eq!(dead.len(), 3, "{dead:?}");
        // Only the valid message is kept, re-encoded from its checked fields.
        kept.sort();
        assert_eq!(
            kept,
            [
                None,
                None,
                Some("sn83045462/1890-05-03/ed-1/seq-99".to_owned())
            ]
        );
        assert!(dead.iter().any(|e| e.contains("no image")));
        assert!(dead.iter().any(|e| e == "invalid message: bad_json"));
    }

    #[tokio::test]
    async fn transient_failures_retry_then_dead_letter() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = worker(dir.path());
        queue
            .send(PRIORITY, &msg("sn83045462/1890-05-03/ed-1/seq-98", "deu"))
            .await
            .unwrap();
        for n in 1..=5 {
            // Visible again at once, as if its time had run out.
            let m = queue
                .receive(PRIORITY, Duration::ZERO)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(m.dequeue_count, n);
            w.handle(m, Instant::now()).await;
        }
        // The sixth time it goes to the dead-letter queue.
        let m = queue
            .receive(PRIORITY, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        w.handle(m, Instant::now()).await;
        assert_eq!(
            queue
                .receive(PRIORITY, Duration::from_secs(1))
                .await
                .unwrap(),
            None
        );
        assert!(queue
            .receive(DEADLETTER, Duration::from_secs(1))
            .await
            .unwrap()
            .is_some());
        let st = w.tracker.status();
        assert!(st.since_start.failed > 0);
        assert_eq!(st.since_start.dead_lettered, 1);
        assert_eq!(st.recent[0].error_kind.as_deref(), Some("dequeue_count"));
        assert!(out.list("pages").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_page_with_no_model_fails_without_an_image_request() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out, scans) = worker_with_scans(dir.path());
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-8", "haw"))
            .await
            .unwrap();
        let m = queue.receive(BULK, Duration::ZERO).await.unwrap().unwrap();
        w.handle(m, Instant::now()).await;
        assert_eq!(scans.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(out.list("pages").await.unwrap().is_empty());
        // Left for another node, not dead-lettered.
        assert!(queue
            .receive(DEADLETTER, Duration::from_secs(1))
            .await
            .unwrap()
            .is_none());
        assert_eq!(queue.approximate_count(BULK).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn a_long_loc_hold_takes_no_messages() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, _out, scans) = worker_with_scans(dir.path());
        // A 1-hour Retry-After, past the page budget (1440 s).
        *scans.held_until.lock().unwrap() =
            Some(tokio::time::Instant::now() + Duration::from_secs(3600));
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        let (tx, rx) = watch::channel(false);
        let run = tokio::spawn(w.clone().run(rx));
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send_replace(true);
        run.await.unwrap().unwrap();
        assert_eq!(scans.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
        // Never received: its first receive is still to come.
        let m = queue
            .receive(BULK, Duration::from_secs(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.dequeue_count, 1);
    }

    #[tokio::test]
    async fn a_short_loc_hold_still_takes_messages() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out, scans) = worker_with_scans(dir.path());
        // Shorter than a page's budget: the page waits it out in the pacer.
        *scans.held_until.lock().unwrap() =
            Some(tokio::time::Instant::now() + Duration::from_secs(60));
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(false);
        w.clone().run(rx).await.unwrap();
        assert_eq!(out.list("pages").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_jp2_that_crashes_the_engine_is_read_from_its_jpeg() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out, scans) = worker_with_scans(dir.path());
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-97", "deu"))
            .await
            .unwrap();
        let m = queue.receive(BULK, Duration::ZERO).await.unwrap().unwrap();
        w.handle(m, Instant::now()).await;
        assert_eq!(scans.jpeg_requests.load(SeqCst), 1);
        // Done on its first receive: deleted, not retried.
        assert_eq!(queue.approximate_count(BULK).await.unwrap(), 0);
        let st = w.tracker.status();
        assert_eq!(st.since_start.done, 1);
        assert!(st.recent[0].jpeg_fallback);
        // The ALTO names the JPEG read, with no bag md5.
        let altos = out.list("alto").await.unwrap();
        let alto = String::from_utf8(out.get(&altos[0]).await.unwrap().unwrap()).unwrap();
        assert!(
            alto.contains("service:ndnp:x:97/full/full/0/default.jpg</fileName>"),
            "{alto}"
        );
        assert!(alto.contains(&crate::prefetch::sha256(b"jpeg")), "{alto}");
        assert!(!alto.contains("md5"), "{alto}");
        assert_eq!(st.since_start.from_loc, 1);
        assert_eq!(out.list("pages").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_jpeg_fallback_or_another_exit_takes_the_retry_path() {
        use std::sync::atomic::Ordering::SeqCst;
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out, scans) = worker_with_scans(dir.path());
        // 96: the JPEG crashes too, and isn't retried a second way. 95: an
        // exit other than 136 gets no fallback.
        for (page, jpegs) in [("seq-96", 1), ("seq-95", 1)] {
            queue
                .send(
                    BULK,
                    &msg(&format!("sn83045462/1890-05-03/ed-1/{page}"), "deu"),
                )
                .await
                .unwrap();
            let m = queue.receive(BULK, Duration::ZERO).await.unwrap().unwrap();
            w.handle(m, Instant::now()).await;
            assert_eq!(scans.jpeg_requests.load(SeqCst), jpegs, "{page}");
            let st = w.tracker.status();
            assert_eq!(st.recent[0].outcome, "failed");
            assert_eq!(st.recent[0].error_kind.as_deref(), Some("engine"));
            assert!(!st.recent[0].jpeg_fallback);
            // Left on the queue, to reappear when its visibility runs out.
            assert_eq!(queue.approximate_count(BULK).await.unwrap(), 1);
            let again = queue
                .receive(BULK, Duration::from_secs(600))
                .await
                .unwrap()
                .unwrap();
            queue.delete(&again).await.unwrap();
        }
        assert!(out.list("alto").await.unwrap().is_empty());
    }

    #[test]
    fn status_136_is_told_apart() {
        use std::os::unix::process::ExitStatusExt;
        let e = |status| {
            anyhow::Error::new(crate::engine::Exited {
                status,
                stderr: String::new(),
            })
        };
        // From a container: 128 + SIGFPE.
        assert!(exited_136(&e(std::process::ExitStatus::from_raw(136 << 8))));
        // From a host process: the signal itself.
        assert!(exited_136(&e(std::process::ExitStatus::from_raw(8))));
        assert!(!exited_136(&e(std::process::ExitStatus::from_raw(1 << 8))));
        assert!(!exited_136(&anyhow::anyhow!(
            "tesseract exited with exit status: 136"
        )));
    }

    #[test]
    fn output_names() {
        let key: PageKey = "sn83045462/1890-05-03/ed-2/seq-7".parse().unwrap();
        let (a, p) = output_paths(&key, "nuc1");
        assert!(a.starts_with("alto/sn83045462_1890-05-03_ed-2_seq-7.nuc1."));
        assert!(a.ends_with("Z.xml"));
        assert!(p.starts_with("pages/sn83045462_1890-05-03_ed-2.nuc1."));
        assert!(p.ends_with("Z.parquet"));
        assert!(usnm_store::validate_path(&a).is_ok());
        assert!(usnm_store::validate_path(&p).is_ok());
        assert!(check_node_id("nuc-1").is_ok());
        assert!(check_node_id("a.b").is_err());
        assert!(check_node_id("").is_err());
        let cfg = Config::default();
        assert_eq!(cfg.budget(), Duration::from_secs(1440));
    }

    #[derive(Debug)]
    struct SlowEngine;

    #[async_trait]
    impl Engine for SlowEngine {
        fn source(&self) -> &str {
            "usnm-slow"
        }
        fn can_read(&self, _: &str) -> bool {
            true
        }
        async fn ocr(&self, _: &Path, _: &str, _: &Path) -> Result<EngineOutput, EngineError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!()
        }
    }

    fn slow_worker(
        dir: &Path,
        visibility: Duration,
    ) -> (Arc<Worker>, Arc<LocalQueue>, Arc<dyn ObjectStore>) {
        let (w, queue, out) = worker(dir);
        let w = Arc::new(Worker {
            engine: Arc::new(SlowEngine),
            cfg: Config {
                once: false,
                visibility,
                shutdown_grace: Duration::from_millis(50),
                ..w.cfg.clone()
            },
            queue: w.queue.clone(),
            out: w.out.clone(),
            scans: w.scans.clone(),
            metrics: w.metrics.clone(),
            tracker: w.tracker.clone(),
        });
        (w, queue, out)
    }

    #[tokio::test]
    async fn a_page_over_its_budget_is_abandoned_without_output() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = slow_worker(dir.path(), Duration::from_secs(2));
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        let m = queue
            .receive(BULK, w.cfg.visibility)
            .await
            .unwrap()
            .unwrap();
        let t0 = Instant::now();
        tokio::time::timeout(Duration::from_secs(10), w.handle(m, t0))
            .await
            .unwrap();
        assert!(t0.elapsed() >= Duration::from_millis(1600));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "gave up before its visibility ended"
        );
        assert!(out.list("pages").await.unwrap().is_empty());
        assert!(out.list("alto").await.unwrap().is_empty());
        // Not deleted: it reappears once its visibility runs out.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let again = queue
            .receive(BULK, w.cfg.visibility)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(again.dequeue_count, 2);
    }

    #[tokio::test]
    async fn a_stop_abandons_unfinished_pages_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = slow_worker(dir.path(), Duration::from_secs(2));
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        let (tx, rx) = watch::channel(false);
        let tracker = w.tracker.clone();
        let t0 = Instant::now();
        let run = tokio::spawn(w.run(rx));
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            t0.elapsed() < Duration::from_secs(1),
            "stopped after the grace"
        );
        // The tracker ends the dropped page as abandoned at shutdown.
        let st = tracker.status();
        assert!(st.in_flight.is_empty());
        assert_eq!(st.since_start.abandoned, 1);
        assert_eq!(st.recent[0].error_kind.as_deref(), Some("shutdown"));
        assert!(out.list("pages").await.unwrap().is_empty());
        // Hidden until its visibility runs out, then back for another node.
        assert_eq!(
            queue.receive(BULK, Duration::from_secs(600)).await.unwrap(),
            None
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        let m = queue
            .receive(BULK, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.dequeue_count, 2);
    }
}
