//! The worker loop: take a page, fetch its scan, read it, write ALTO and an
//! overlay part, delete the message.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use usnm_core::ids::PageKey;
use usnm_overlay::OverlayRow;
use usnm_store::{ObjectStore, StoreError};

use crate::engine::{set_alto_file_name, Engine, EngineError};
use crate::lease::Lease;
use crate::loc::{FetchError, LocClient, Scan};
use crate::message::{self, OcrMessage, MAX_MESSAGE_BYTES};
use crate::queue::{random_token, LeaseLost, Queue, Received, BULK, DEADLETTER, PRIORITY};

/// Where a page's scan comes from (loc.gov, or a stand-in in tests).
#[async_trait]
pub trait ScanSource: Send + Sync + std::fmt::Debug {
    async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError>;
}

#[async_trait]
impl ScanSource for LocClient {
    async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError> {
        LocClient::scan(self, key).await
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Names this node in part names and logs: `[A-Za-z0-9-]`, 1 to 32.
    pub node_id: String,
    pub concurrency: usize,
    /// How long a received message stays hidden.
    pub visibility: Duration,
    /// How often a held message's visibility is renewed.
    pub renew_every: Duration,
    /// A message received more often than this goes to `ocr-deadletter`.
    pub max_dequeue: u32,
    /// How long a message that failed for a passing reason stays hidden.
    pub retry_delay: Duration,
    /// The longest wait between polls when both queues are empty.
    pub idle_max: Duration,
    /// Stop once both queues are empty and nothing is in flight.
    pub once: bool,
    /// After SIGTERM, how long in-flight pages may take before they are
    /// released to the queue.
    pub shutdown_grace: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            node_id: "local".into(),
            concurrency: 1,
            visibility: Duration::from_secs(600),
            renew_every: Duration::from_secs(180),
            max_dequeue: 5,
            retry_delay: Duration::from_secs(300),
            idle_max: Duration::from_secs(60),
            once: false,
            shutdown_grace: Duration::from_secs(60),
        }
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
    /// Try again later.
    Transient(String),
}

impl From<FetchError> for Failure {
    fn from(e: FetchError) -> Self {
        match e {
            FetchError::Permanent(m) => Failure::Permanent(m),
            FetchError::Transient(m) => Failure::Transient(m),
        }
    }
}

impl From<StoreError> for Failure {
    fn from(e: StoreError) -> Self {
        Failure::Transient(format!("output store: {e}"))
    }
}

/// What was written for a page.
#[derive(Debug)]
pub struct Done {
    pub part: String,
    pub chars: usize,
    pub status: &'static str,
    pub engine: String,
}

/// The overlay part path for a page: `pages/<issue>.<node>.<stamp>.parquet`.
pub fn part_path(key: &PageKey, node: &str) -> String {
    let issue = format!(
        "{}_{}_ed-{}",
        key.lccn,
        key.date.format("%Y-%m-%d"),
        key.edition
    );
    let stamp = Utc::now().format("%Y%m%dT%H%M%S%6fZ");
    format!("pages/{issue}.{node}.{stamp}.parquet")
}

#[derive(Debug)]
pub struct Worker {
    pub queue: Arc<dyn Queue>,
    pub out: Arc<dyn ObjectStore>,
    pub scans: Arc<dyn ScanSource>,
    pub engine: Arc<dyn Engine>,
    pub cfg: Config,
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
    /// In-flight pages get `shutdown_grace` to finish; then they're released.
    pub async fn run(self: Arc<Self>, mut stop: watch::Receiver<bool>) -> anyhow::Result<()> {
        let slots = Arc::new(Semaphore::new(self.cfg.concurrency.max(1)));
        let (hard_tx, hard_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();
        let mut idle = Duration::from_secs(1);
        tracing::info!(node = %self.cfg.node_id, concurrency = self.cfg.concurrency, "worker started");
        loop {
            while tasks.try_join_next().is_some() {}
            let permit = tokio::select! {
                p = slots.clone().acquire_owned() => p?,
                _ = stop.changed() => break,
            };
            if *stop.borrow() {
                break;
            }
            let next = tokio::select! {
                r = self.next_message() => r,
                // A message received as this is dropped comes back when its lease ends.
                _ = stop.changed() => break,
            };
            match next {
                Ok(Some(msg)) => {
                    idle = Duration::from_secs(1);
                    let me = self.clone();
                    let hard = hard_rx.clone();
                    tasks.spawn(async move {
                        me.handle(msg, hard).await;
                        drop(permit);
                    });
                    continue;
                }
                Ok(None) => {
                    drop(permit);
                    if self.cfg.once && tasks.is_empty() {
                        tracing::info!("queues empty; stopping (--once)");
                        break;
                    }
                }
                Err(e) => {
                    drop(permit);
                    tracing::warn!(error = %e, "receive failed");
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
                tracing::info!(
                    in_flight = tasks.len(),
                    "grace over; releasing in-flight pages"
                );
                hard_tx.send_replace(true);
                while tasks.join_next().await.is_some() {}
            }
        }
        tracing::info!("worker stopped");
        Ok(())
    }

    async fn dead_letter(&self, msg: &Received, error: &str) {
        let original: String = msg.text.chars().take(MAX_MESSAGE_BYTES).collect();
        let body = serde_json::json!({
            "v": 1,
            "from": msg.queue,
            "message": original,
            "error": error,
            "dequeue_count": msg.dequeue_count,
            "node": self.cfg.node_id,
            "at": Utc::now().to_rfc3339(),
        });
        use base64::Engine as _;
        let text = base64::engine::general_purpose::STANDARD.encode(body.to_string());
        if let Err(e) = self.queue.send(DEADLETTER, &text).await {
            // Leave the message: it comes back and is dead-lettered again.
            tracing::error!(error = %e, "dead-letter send failed");
            return;
        }
        match self.queue.delete(msg).await {
            Ok(()) => {
                tracing::warn!(queue = %msg.queue, message_id = %msg.id, error, "dead-lettered")
            }
            Err(e) => tracing::error!(error = %e, "delete after dead-letter failed"),
        }
    }

    /// Do one message, start to finish; never fails (the outcome is in the queue).
    pub async fn handle(&self, msg: Received, mut hard_stop: watch::Receiver<bool>) {
        if msg.dequeue_count > self.cfg.max_dequeue {
            let why = format!("received {} times", msg.dequeue_count);
            self.dead_letter(&msg, &why).await;
            return;
        }
        let (m, key) = match message::decode(&msg.text) {
            Ok(v) => v,
            Err(e) => {
                self.dead_letter(&msg, &e.to_string()).await;
                return;
            }
        };
        let lease = Lease::new(self.queue.clone(), msg.clone());
        let renewer = lease.keep_alive(self.cfg.renew_every, self.cfg.visibility);
        let t0 = Instant::now();
        let outcome = tokio::select! {
            r = self.process(&m, &key) => Some(r),
            _ = hard_stop.wait_for(|v| *v) => None,
        };
        drop(renewer);
        let page = key.to_string();
        let current = lease.message().await;
        match outcome {
            Some(Ok(done)) => {
                tracing::info!(
                    page, queue = %msg.queue, reason = m.reason.as_str(), language = %m.language,
                    engine = %done.engine, status = done.status, chars = done.chars,
                    part = %done.part, secs = t0.elapsed().as_secs_f64(), "page done"
                );
                if let Err(e) = lease.delete().await {
                    if e.downcast_ref::<LeaseLost>().is_some() {
                        tracing::warn!(
                            page,
                            "message was gone at delete; another node may redo it"
                        );
                    } else {
                        tracing::error!(page, error = %e, "delete failed; the page will be redone");
                    }
                }
            }
            Some(Err(Failure::Permanent(e))) => {
                self.dead_letter(&current, &e).await;
            }
            Some(Err(Failure::Transient(e))) => {
                tracing::warn!(page, error = %e, dequeue_count = msg.dequeue_count, retry_secs = self.cfg.retry_delay.as_secs(), "page failed; will retry");
                if let Err(e) = lease.renew(self.cfg.retry_delay).await {
                    tracing::warn!(page, error = %e, "couldn't set the retry delay");
                }
            }
            None => {
                tracing::info!(page, "released unfinished");
                if let Err(e) = lease.renew(Duration::ZERO).await {
                    tracing::warn!(page, error = %e, "release failed; it comes back when its lease ends");
                }
            }
        }
    }

    async fn process(&self, m: &OcrMessage, key: &PageKey) -> Result<Done, Failure> {
        let scan = self.scans.scan(key).await?;
        let work =
            tempfile::tempdir().map_err(|e| Failure::Transient(format!("scratch dir: {e}")))?;
        let image = work.path().join("page.jpg");
        tokio::fs::write(&image, &scan.jpeg)
            .await
            .map_err(|e| Failure::Transient(format!("scratch image: {e}")))?;
        drop(scan.jpeg);
        let out = match self.engine.ocr(&image, &m.language, work.path()).await {
            Ok(o) => o,
            // Another node may have the model: try again later, and let
            // the dequeue count dead-letter it if no node does.
            Err(EngineError::Unsupported(l)) => {
                return Err(Failure::Transient(format!(
                    "no model for `{l}` on node {}",
                    self.cfg.node_id
                )))
            }
            Err(EngineError::Failed(e)) => return Err(Failure::Transient(format!("{e:#}"))),
        };
        let text = usnm_core::text::normalize_ocr(&out.text);
        let alto = set_alto_file_name(
            &String::from_utf8_lossy(&out.alto),
            &format!("{}/full/full/0/default.jpg", scan.iiif_base),
        );
        let row = OverlayRow {
            key: key.clone(),
            batch: m.batch.clone(),
            ocr_source: self.engine.source().to_owned(),
            ocr_engine: out.engine.clone(),
            // The overlay's `loc_text` says why the page was OCRed: here,
            // the queue's reason (`bulk` or `user`).
            loc_text: m.reason.as_str().to_owned(),
            text,
            image_url: scan.iiif_base,
            ocred_at: Utc::now(),
        };
        let status = usnm_overlay::status_str(row.status());
        let chars = row.text.chars().count();
        let part = usnm_overlay::write_part(std::slice::from_ref(&row))
            .map_err(|e| Failure::Transient(format!("overlay part: {e:#}")))?;
        // ALTO first, as the Japanese job does: a part is only written once
        // its page's ALTO is there. A newer OCR of the page replaces the ALTO.
        self.out
            .put(
                &format!("alto/{}.xml", key.doc_id()),
                alto.into_bytes(),
                "application/xml",
            )
            .await?;
        for _ in 0..3 {
            let path = part_path(key, &self.cfg.node_id);
            if self
                .out
                .put_new(&path, part.clone(), "application/vnd.apache.parquet")
                .await?
            {
                return Ok(Done {
                    part: path,
                    chars,
                    status,
                    engine: out.engine,
                });
            }
        }
        Err(Failure::Transient(format!(
            "no free part name for {key} ({})",
            random_token()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EngineOutput;
    use crate::message::Reason;
    use crate::queue::LocalQueue;
    use std::path::Path;

    #[derive(Debug)]
    struct FakeScans;

    #[async_trait]
    impl ScanSource for FakeScans {
        async fn scan(&self, key: &PageKey) -> Result<Scan, FetchError> {
            if key.seq == 99 {
                return Err(FetchError::Permanent("no image".into()));
            }
            if key.seq == 98 {
                return Err(FetchError::Transient("HTTP 500".into()));
            }
            Ok(Scan {
                iiif_base: format!(
                    "https://tile.loc.gov/image-services/iiif/service:ndnp:x:{}",
                    key.seq
                ),
                jpeg: bytes::Bytes::from_static(b"jpeg"),
            })
        }
    }

    #[derive(Debug)]
    struct FakeEngine;

    #[async_trait]
    impl Engine for FakeEngine {
        fn source(&self) -> &str {
            "usnm-tesseract"
        }
        async fn ocr(
            &self,
            image: &Path,
            language: &str,
            _: &Path,
        ) -> Result<EngineOutput, EngineError> {
            assert_eq!(std::fs::read(image).unwrap(), b"jpeg");
            if language == "zzz" {
                return Err(EngineError::Unsupported(language.into()));
            }
            Ok(EngineOutput {
                text: "Die  Nebraska Staats-Zeitung\nerscheint  jeden\u{0007} Sonn-\nabend.\n"
                    .into(),
                alto: b"<alto><fileName>/tmp/page.jpg</fileName></alto>".to_vec(),
                engine: "tesseract-5.5.1/deu_latf+deu".into(),
            })
        }
    }

    fn msg(page: &str, language: &str) -> String {
        OcrMessage {
            v: 1,
            page: page.into(),
            batch: "nbu_gage_ver01".into(),
            language: language.into(),
            reason: Reason::Bulk,
            score: Some(0.62),
            queued_at: "2026-10-10T12:00:00Z".into(),
        }
        .encode()
    }

    fn worker(dir: &Path) -> (Arc<Worker>, Arc<LocalQueue>, Arc<dyn ObjectStore>) {
        let queue = Arc::new(LocalQueue::new(dir.join("q")));
        let out: Arc<dyn ObjectStore> = Arc::new(usnm_store::LocalStore::new(dir.join("out")));
        let w = Arc::new(Worker {
            queue: queue.clone(),
            out: out.clone(),
            scans: Arc::new(FakeScans),
            engine: Arc::new(FakeEngine),
            cfg: Config {
                node_id: "node1".into(),
                concurrency: 2,
                once: true,
                ..Config::default()
            },
        });
        (w, queue, out)
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
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-8", "zzz"))
            .await
            .unwrap();
        queue.send(BULK, "not a message").await.unwrap();
        let (_tx, rx) = watch::channel(false);
        w.clone().run(rx).await.unwrap();

        let alto = out
            .get("alto/sn83045462_1890-05-03_ed-1_seq-7.xml")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8(alto).unwrap(),
            "<alto><fileName>https://tile.loc.gov/image-services/iiif/service:ndnp:x:7/full/full/0/default.jpg</fileName></alto>"
        );
        let parts = out.list("pages").await.unwrap();
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert!(parts[0].starts_with("pages/sn83045462_1890-05-03_ed-1.node1."));
        let bytes = out.get(&parts[0]).await.unwrap().unwrap();
        let pages = usnm_ingest::ocr_ja::read_part(bytes::Bytes::from(bytes), &parts[0]).unwrap();
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        assert_eq!(p.key.to_string(), "sn83045462/1890-05-03/ed-1/seq-7");
        assert_eq!(p.batch, "nbu_gage_ver01");
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
            dead.push(body["error"].as_str().unwrap().to_owned());
        }
        dead.sort();
        assert_eq!(dead.len(), 2, "{dead:?}");
        assert!(dead.iter().any(|e| e.contains("no image")));
        assert!(dead.iter().any(|e| e.contains("base64")));
    }

    #[tokio::test]
    async fn transient_failures_retry_then_dead_letter() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = worker(dir.path());
        let w = Arc::new(Worker {
            cfg: Config {
                retry_delay: Duration::ZERO,
                ..w.cfg.clone()
            },
            queue: w.queue.clone(),
            out: w.out.clone(),
            scans: w.scans.clone(),
            engine: w.engine.clone(),
        });
        queue
            .send(PRIORITY, &msg("sn83045462/1890-05-03/ed-1/seq-98", "deu"))
            .await
            .unwrap();
        let (_tx, rx) = watch::channel(false);
        for n in 1..=5 {
            let m = queue
                .receive(PRIORITY, Duration::from_secs(600))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(m.dequeue_count, n);
            w.handle(m, rx.clone()).await;
        }
        // The sixth time it goes to the dead-letter queue.
        let m = queue
            .receive(PRIORITY, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        w.handle(m, rx.clone()).await;
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
        assert!(out.list("pages").await.unwrap().is_empty());
    }

    #[test]
    fn part_names() {
        let key: PageKey = "sn83045462/1890-05-03/ed-2/seq-7".parse().unwrap();
        let p = part_path(&key, "nuc1");
        assert!(p.starts_with("pages/sn83045462_1890-05-03_ed-2.nuc1."));
        assert!(p.ends_with("Z.parquet"));
        assert!(usnm_store::validate_path(&p).is_ok());
        assert!(check_node_id("nuc-1").is_ok());
        assert!(check_node_id("a.b").is_err());
        assert!(check_node_id("").is_err());
    }

    #[derive(Debug)]
    struct SlowEngine;

    #[async_trait]
    impl Engine for SlowEngine {
        fn source(&self) -> &str {
            "usnm-slow"
        }
        async fn ocr(&self, _: &Path, _: &str, _: &Path) -> Result<EngineOutput, EngineError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            unreachable!()
        }
    }

    #[tokio::test]
    async fn a_stop_releases_unfinished_pages_after_the_grace() {
        let dir = tempfile::tempdir().unwrap();
        let (w, queue, out) = worker(dir.path());
        let w = Arc::new(Worker {
            engine: Arc::new(SlowEngine),
            cfg: Config {
                once: false,
                shutdown_grace: Duration::from_millis(50),
                ..w.cfg.clone()
            },
            queue: w.queue.clone(),
            out: w.out.clone(),
            scans: w.scans.clone(),
        });
        queue
            .send(BULK, &msg("sn83045462/1890-05-03/ed-1/seq-7", "deu"))
            .await
            .unwrap();
        let (tx, rx) = watch::channel(false);
        let run = tokio::spawn(w.run(rx));
        tokio::time::sleep(Duration::from_millis(300)).await;
        tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(10), run)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // Visible again at once, for another node.
        let m = queue
            .receive(BULK, Duration::from_secs(600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.dequeue_count, 2);
        assert!(out.list("pages").await.unwrap().is_empty());
    }
}
