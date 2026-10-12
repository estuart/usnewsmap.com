//! The OCR nodes: each worker's `GET /status`, and its journal of finished
//! pages (`GET /journal?from=<offset>`), whose OCRed pages are copied here
//! so the totals outlive a worker's restart and a page finished while the
//! dashboard was down still counts.
//!
//! Memory stays small: per page OCRed, its doc_id and three small numbers
//! (node, language, engine tag, the strings interned); per node, the
//! latest pages for the page. The copy on disk holds only pages OCRed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{BufRead, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use usnm_ocr_contract::status::{Completion, WorkerStatus};

/// The largest status or journal answer read.
const MAX_BODY: usize = 4 * 1024 * 1024;
/// Pages OCRed kept per node for when its status can't be had.
const LATEST: usize = 100;

#[derive(Debug)]
pub struct Node {
    pub name: String,
    pub url: String,
    pub status: Option<WorkerStatus>,
    pub last_ok: Option<DateTime<Utc>>,
    pub error: Option<String>,
    /// How much of the worker's journal has been copied, and which journal.
    pub offset: u64,
    pub journal_id: String,
    /// The latest pages OCRed, newest last, from the copy.
    pub latest: VecDeque<Completion>,
}

/// A node's name: `[A-Za-z0-9-]`, 1 to 32; it names a file here.
pub fn check_name(s: &str) -> anyhow::Result<()> {
    if !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        Ok(())
    } else {
        bail!("a node name is 1 to 32 letters, digits or `-`")
    }
}

/// A page OCRed: its node, language and engine tag, as indices into
/// [`Nodes::names`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ocred {
    pub node: u16,
    pub language: u32,
    pub engine: u32,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Mark {
    offset: u64,
    journal_id: String,
}

#[derive(Debug, Default)]
pub struct Nodes {
    pub nodes: Vec<Node>,
    state_dir: PathBuf,
    /// Pages OCRed, by doc_id: the last node to report each.
    pub ocred: HashMap<Box<str>, Ocred>,
    /// Interned languages and engine tags.
    pub names: Vec<String>,
    index: HashMap<String, u32>,
}

/// What one poll of a node brought.
pub struct Polled {
    status: WorkerStatus,
    /// The pages OCRed in the new lines.
    done: Vec<Completion>,
    offset: u64,
    journal_id: String,
}

impl Nodes {
    /// The nodes (`name=url`), and what was copied before, from `state_dir`.
    pub fn open(state_dir: PathBuf, nodes: &[(String, String)]) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let marks: BTreeMap<String, Mark> = match std::fs::read(state_dir.join("offsets.json")) {
            Ok(b) => serde_json::from_slice(&b).context("offsets.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e.into()),
        };
        let mut me = Nodes {
            state_dir,
            ..Nodes::default()
        };
        for (name, url) in nodes {
            check_name(name)?;
            let mark = marks.get(name);
            me.nodes.push(Node {
                name: name.clone(),
                url: url.trim_end_matches('/').to_owned(),
                status: None,
                last_ok: None,
                error: None,
                offset: mark.map_or(0, |m| m.offset),
                journal_id: mark.map(|m| m.journal_id.clone()).unwrap_or_default(),
                latest: VecDeque::new(),
            });
        }
        for i in 0..me.nodes.len() {
            let path = me.copy_path(i);
            let f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for l in std::io::BufReader::new(f).lines() {
                if let Ok(c) = serde_json::from_str::<Completion>(&l?) {
                    if c.check().is_ok() && c.outcome == "done" {
                        me.add(i, c);
                    }
                }
            }
        }
        Ok(me)
    }

    fn copy_path(&self, i: usize) -> PathBuf {
        self.state_dir
            .join(format!("journal-{}.jsonl", self.nodes[i].name))
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(i) = self.index.get(s) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(s.to_owned());
        self.index.insert(s.to_owned(), i);
        i
    }

    /// Count a page OCRed. Repeats (a journal copied twice) change nothing
    /// but which node and engine it is under.
    fn add(&mut self, i: usize, c: Completion) {
        let o = Ocred {
            node: i as u16,
            language: self.intern(&c.language),
            engine: self.intern(&c.engine),
        };
        self.ocred.insert(c.doc_id.as_str().into(), o);
        let latest = &mut self.nodes[i].latest;
        if !latest
            .iter()
            .any(|l| l.doc_id == c.doc_id && l.finished_at == c.finished_at)
        {
            latest.push_back(c);
            if latest.len() > LATEST {
                latest.pop_front();
            }
        }
    }

    /// Ask a node for its status and any journal past `offset`. A journal
    /// with another id, or shorter than the offset, is read from the start.
    pub async fn poll(
        client: &reqwest::Client,
        url: &str,
        offset: u64,
        journal_id: &str,
    ) -> anyhow::Result<Polled> {
        let mut status: WorkerStatus =
            serde_json::from_slice(&get(client, &format!("{url}/status")).await?)
                .context("the status isn't JSON")?;
        // Only pages and tags reach the page.
        status.sanitize();
        let mut offset = offset;
        if status.journal_bytes < offset || status.journal_id != journal_id {
            offset = 0;
        }
        let mut done = Vec::new();
        if status.journal_bytes > offset {
            // A prefix at most MAX_BODY long, so a dashboard far behind
            // still moves on a chunk at a time.
            let body = get_prefix(client, &format!("{url}/journal?from={offset}")).await?;
            if let Some(end) = body.iter().rposition(|b| *b == b'\n') {
                offset += end as u64 + 1;
                for l in String::from_utf8_lossy(&body[..end]).lines() {
                    match serde_json::from_str::<Completion>(l) {
                        Ok(c) if c.check().is_ok() => {
                            if c.outcome == "done" {
                                done.push(c);
                            }
                        }
                        _ => tracing::warn!(url, "a journal line that isn't a completion; skipped"),
                    }
                }
            }
        }
        Ok(Polled {
            journal_id: status.journal_id.clone(),
            status,
            done,
            offset,
        })
    }

    /// Apply a poll of node `i`: keep its status, and copy its pages OCRed.
    /// The offset moves only once they are on disk, so a failed write is
    /// tried again at the next poll.
    pub fn apply(&mut self, i: usize, r: anyhow::Result<Polled>, now: DateTime<Utc>) {
        let p = match r {
            Ok(p) => p,
            Err(e) => {
                self.nodes[i].error = Some(format!("{e:#}"));
                return;
            }
        };
        match self.persist(i, &p) {
            Ok(()) => {
                for c in p.done {
                    self.add(i, c);
                }
                let n = &mut self.nodes[i];
                n.offset = p.offset;
                n.journal_id = p.journal_id;
                n.error = None;
            }
            Err(e) => {
                tracing::warn!(error = format!("{e:#}"), "can't save the copied journal");
                self.nodes[i].error = Some(format!("can't save its journal here: {e:#}"));
            }
        }
        let n = &mut self.nodes[i];
        n.status = Some(p.status);
        n.last_ok = Some(now);
    }

    fn persist(&self, i: usize, p: &Polled) -> anyhow::Result<()> {
        if !p.done.is_empty() {
            let mut buf = Vec::new();
            for c in &p.done {
                serde_json::to_writer(&mut buf, c)?;
                buf.push(b'\n');
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.copy_path(i))?;
            f.write_all(&buf)?;
            f.sync_data()?;
        }
        let n = &self.nodes[i];
        if n.offset != p.offset || n.journal_id != p.journal_id {
            let mut marks: BTreeMap<&str, Mark> = self
                .nodes
                .iter()
                .map(|n| {
                    (
                        n.name.as_str(),
                        Mark {
                            offset: n.offset,
                            journal_id: n.journal_id.clone(),
                        },
                    )
                })
                .collect();
            marks.insert(
                &n.name,
                Mark {
                    offset: p.offset,
                    journal_id: p.journal_id.clone(),
                },
            );
            let tmp = self.state_dir.join("offsets.json.tmp");
            std::fs::write(&tmp, serde_json::to_vec(&marks)?)?;
            std::fs::rename(&tmp, self.state_dir.join("offsets.json"))?;
        }
        Ok(())
    }
}

async fn get(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    let (body, whole) = read(client, url).await?;
    if !whole {
        bail!("the answer is over {MAX_BODY} bytes");
    }
    Ok(body)
}

/// The first MAX_BODY bytes at most.
async fn get_prefix(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<u8>> {
    Ok(read(client, url).await?.0)
}

/// The body up to MAX_BODY, and whether that was all of it.
async fn read(client: &reqwest::Client, url: &str) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut resp = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?;
    if !resp.status().is_success() {
        bail!("HTTP {}", resp.status().as_u16());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| anyhow::anyhow!("{}", e.without_url()))?
    {
        if body.len() + chunk.len() > MAX_BODY {
            body.extend_from_slice(&chunk[..MAX_BODY - body.len()]);
            return Ok((body, false));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::routing::get as route;
    use axum::Router;
    use std::sync::{Arc, Mutex};

    fn line(doc: &str, outcome: &str) -> String {
        serde_json::to_string(&Completion {
            doc_id: doc.into(),
            language: "deu".into(),
            engine: "tesseract/5.5.0 profile=frak2021".into(),
            queue: "ocr-bulk".into(),
            reason: "bulk".into(),
            outcome: outcome.into(),
            scan: "cache".into(),
            uploaded: outcome == "done",
            finished_at: "2026-10-11T23:00:00Z".parse().unwrap(),
            ..Completion::default()
        })
        .unwrap()
            + "\n"
    }

    /// A worker whose journal (id, text) is whatever the test puts in it.
    async fn fake_worker(journal: Arc<Mutex<(String, String)>>) -> String {
        let j1 = journal.clone();
        let app = Router::new()
            .route(
                "/status",
                route(move || {
                    let j = j1.clone();
                    async move {
                        let j = j.lock().unwrap();
                        axum::Json(WorkerStatus {
                            node_id: "n1".into(),
                            journal_id: j.0.clone(),
                            journal_bytes: j.1.len() as u64,
                            ..WorkerStatus::default()
                        })
                    }
                }),
            )
            .route(
                "/journal",
                route(move |Query(q): Query<HashMap<String, usize>>| {
                    let j = journal.clone();
                    async move { j.lock().unwrap().1[q["from"]..].to_owned() }
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn poll(n: &mut Nodes, client: &reqwest::Client, url: &str) {
        let (o, id) = (n.nodes[0].offset, n.nodes[0].journal_id.clone());
        let r = Nodes::poll(client, url, o, &id).await;
        n.apply(0, r, Utc::now());
    }

    #[tokio::test]
    async fn copies_pages_ocred_once_and_keeps_them_over_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Mutex::new(("j1".to_owned(), String::new())));
        let url = fake_worker(journal.clone()).await;
        let client = reqwest::Client::new();
        let nodes = [("node1".to_owned(), url.clone())];
        let mut n = Nodes::open(dir.path().to_owned(), &nodes).unwrap();
        {
            let mut j = journal.lock().unwrap();
            j.1.push_str(&line("sn1_1890-01-01_ed-1_seq-1", "done"));
            j.1.push_str("{\"doc_id\":\"Seitentext hier\"}\n");
            j.1.push_str(&line("sn1_1890-01-01_ed-1_seq-2", "failed"));
        }
        poll(&mut n, &client, &url).await;
        assert_eq!(n.ocred.len(), 1);
        assert_eq!(n.nodes[0].latest.len(), 1);
        assert!(n.nodes[0].error.is_none());
        let first = n.nodes[0].offset;
        assert!(first > 0);
        // Nothing new: nothing added.
        poll(&mut n, &client, &url).await;
        assert_eq!(n.ocred.len(), 1);
        // A new journal, as long as the old one, with a new page first: it
        // is read from the start because its id changed.
        {
            let mut j = journal.lock().unwrap();
            let mut text = line("sn1_1890-01-01_ed-1_seq-3", "done");
            while text.len() < first as usize + 10 {
                text.push_str(&line("sn1_1890-01-01_ed-1_seq-1", "done"));
            }
            *j = ("j2".to_owned(), text);
        }
        poll(&mut n, &client, &url).await;
        assert_eq!(n.ocred.len(), 2);
        assert_eq!(n.nodes[0].latest.len(), 2);
        // Reopened: the copy, the offset and the id are back.
        let again = Nodes::open(dir.path().to_owned(), &nodes).unwrap();
        assert_eq!(again.ocred.len(), 2);
        assert_eq!(again.nodes[0].offset, n.nodes[0].offset);
        assert_eq!(again.nodes[0].journal_id, "j2");
        let o = again.ocred["sn1_1890-01-01_ed-1_seq-3"];
        assert_eq!(again.names[o.language as usize], "deu");
    }

    #[tokio::test]
    async fn a_journal_far_ahead_is_copied_a_chunk_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let mut text = String::new();
        let mut n = 0;
        while text.len() <= MAX_BODY * 2 {
            n += 1;
            text.push_str(&line(&format!("sn1_1890-01-01_ed-1_seq-{n}"), "done"));
        }
        let url = fake_worker(Arc::new(Mutex::new(("j1".to_owned(), text)))).await;
        let client = reqwest::Client::new();
        let mut nodes =
            Nodes::open(dir.path().to_owned(), &[("node1".to_owned(), url.clone())]).unwrap();
        poll(&mut nodes, &client, &url).await;
        assert!(nodes.nodes[0].error.is_none());
        assert!(nodes.ocred.len() > 1 && nodes.ocred.len() < n);
        for _ in 0..3 {
            poll(&mut nodes, &client, &url).await;
        }
        assert_eq!(nodes.ocred.len(), n);
    }

    #[tokio::test]
    async fn a_failed_save_leaves_the_offset_for_the_next_poll() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(Mutex::new((
            "j1".to_owned(),
            line("sn1_1890-01-01_ed-1_seq-1", "done"),
        )));
        let url = fake_worker(journal).await;
        let client = reqwest::Client::new();
        let state = dir.path().join("state");
        let mut n = Nodes::open(state.clone(), &[("node1".to_owned(), url.clone())]).unwrap();
        // The copy can't be written: a directory where its file goes.
        std::fs::create_dir(state.join("journal-node1.jsonl")).unwrap();
        poll(&mut n, &client, &url).await;
        assert_eq!(n.nodes[0].offset, 0);
        assert!(n.ocred.is_empty());
        assert!(n.nodes[0].error.is_some());
        assert!(n.nodes[0].status.is_some());
        std::fs::remove_dir(state.join("journal-node1.jsonl")).unwrap();
        poll(&mut n, &client, &url).await;
        assert!(n.nodes[0].offset > 0);
        assert_eq!(n.ocred.len(), 1);
    }

    #[tokio::test]
    async fn a_node_that_doesnt_answer_shows_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let nodes = [("node2".to_owned(), "http://127.0.0.1:9".to_owned())];
        let mut n = Nodes::open(dir.path().to_owned(), &nodes).unwrap();
        let r = Nodes::poll(&reqwest::Client::new(), "http://127.0.0.1:9", 0, "").await;
        n.apply(0, r, Utc::now());
        assert!(n.nodes[0].error.is_some());
        assert!(n.nodes[0].status.is_none());
        assert!(check_name("../x").is_err());
    }
}
