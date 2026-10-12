//! `usnm-ocr-worker publish-fetched` (12 Step 11, #368): relays the scan
//! server's index to the exchange container, so the seeder, which runs in
//! Azure and can't reach the LAN, queues only pages whose scans are on
//! pve-big.
//!
//! It reads `GET <scan server>/index/full.jsonl` (the fetcher's index, one
//! JSON line per scan written), keeps the full-size scans' (batch, doc_id),
//! and writes them as `usnm_ocr_contract::fetched`'s list to
//! `ocr-out/fetched/<manifest version>/<node>.<stamp>.parquet`, create-only,
//! with the node's own identity. `ocr-sync`'s pickup never lists
//! `fetched/`, and `ocr-out`'s lifecycle rule deletes old lists after 30
//! days. `run --publish-fetched <version>` does the same every
//! `--publish-every-secs` beside the queue loop; the operator sets it on one
//! node (`install-worker.sh --publish-fetched`).
//!
//! The index is read as a stream, a line at a time, with caps on the line
//! length, the line count and the time, so a large index never sits in
//! memory as text. Lines that aren't a full-size scan with a valid page id
//! and batch are counted and left out.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;
use tokio::sync::watch;
use usnm_ocr_contract::fetched::{self, Fetched};
use usnm_store::ObjectStore;

use crate::loc::USER_AGENT;

/// The longest index line read (a sidecar is under 1 KiB).
const MAX_LINE: usize = 64 * 1024;
/// The most index lines read: every fetch of both sets, with room.
const MAX_LINES: u64 = 40_000_000;
/// The whole read of the index.
const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// What `publish-fetched` reads and writes.
#[derive(Debug, Clone)]
pub struct Options {
    /// The scan server, `http://<host>:8380`.
    pub scan_server: String,
    /// The prefetch manifest's index version the scans were fetched from.
    pub manifest: String,
    /// This node's name, in the list's path.
    pub node_id: String,
}

/// What one publish did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub path: String,
    pub lines: u64,
    pub pages: usize,
    /// Lines left out: scaled scans, unreadable lines, bad ids.
    pub skipped: u64,
}

#[derive(Deserialize)]
struct IndexLine {
    doc_id: String,
    batch: String,
    resolution: String,
}

/// Folds index lines into the set of full-size scans.
#[derive(Default)]
struct Fold {
    pages: HashSet<Fetched>,
    lines: u64,
    skipped: u64,
    partial: Vec<u8>,
}

impl Fold {
    fn line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        self.lines += 1;
        if self.lines > MAX_LINES {
            bail!("the index has more than {MAX_LINES} lines");
        }
        let page = serde_json::from_slice::<IndexLine>(line)
            .ok()
            .filter(|l| l.resolution == "full")
            .map(|l| Fetched {
                batch: l.batch,
                doc_id: l.doc_id,
            })
            .filter(|p| p.check().is_ok());
        match page {
            Some(p) => {
                self.pages.insert(p);
                if self.pages.len() > fetched::MAX_PAGES {
                    bail!("the index lists more than {} scans", fetched::MAX_PAGES);
                }
            }
            None => self.skipped += 1,
        }
        Ok(())
    }

    fn chunk(&mut self, mut bytes: &[u8]) -> anyhow::Result<()> {
        while let Some(i) = bytes.iter().position(|&b| b == b'\n') {
            let (head, rest) = bytes.split_at(i);
            if self.partial.len() + head.len() > MAX_LINE {
                bail!("an index line is longer than {MAX_LINE} bytes");
            }
            if self.partial.is_empty() {
                self.line(head)?;
            } else {
                self.partial.extend_from_slice(head);
                let line = std::mem::take(&mut self.partial);
                self.line(&line)?;
            }
            bytes = &rest[1..];
        }
        if self.partial.len() + bytes.len() > MAX_LINE {
            bail!("an index line is longer than {MAX_LINE} bytes");
        }
        self.partial.extend_from_slice(bytes);
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<Self> {
        let rest = std::mem::take(&mut self.partial);
        self.line(&rest)?;
        Ok(self)
    }
}

/// The full-size scans the scan server's index lists.
pub async fn read_index(scan_server: &str) -> anyhow::Result<(HashSet<Fetched>, u64, u64)> {
    let url = format!("{}/index/full.jsonl", scan_server.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(5))
        .timeout(READ_TIMEOUT)
        .build()?;
    let mut resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    let mut fold = Fold::default();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("reading {url}"))?
    {
        fold.chunk(&chunk)?;
    }
    let fold = fold.finish()?;
    Ok((fold.pages, fold.lines, fold.skipped))
}

/// `YYYYMMDDTHHMMSSZ`, now.
fn stamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Reads the index and writes the list once.
pub async fn publish(out: &dyn ObjectStore, opts: &Options) -> anyhow::Result<Published> {
    if !usnm_store::is_safe_segment(&opts.manifest) {
        bail!("unsafe manifest version {:?}", opts.manifest);
    }
    let (pages, lines, skipped) = read_index(&opts.scan_server).await?;
    let body = fetched::encode(&opts.manifest, &pages)?;
    let path = fetched::path(&opts.manifest, &opts.node_id, &stamp());
    if !out
        .put_new(&path, body, "application/vnd.apache.parquet")
        .await?
    {
        bail!("{path} already exists");
    }
    Ok(Published {
        path,
        lines,
        pages: pages.len(),
        skipped,
    })
}

/// [`publish`] every `every` until `stop`; a failed publish is logged and
/// tried again at the next turn.
pub async fn every(
    out: Arc<dyn ObjectStore>,
    opts: Options,
    every: Duration,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        match publish(out.as_ref(), &opts).await {
            Ok(p) => tracing::info!(
                path = p.path,
                lines = p.lines,
                pages = p.pages,
                skipped = p.skipped,
                "fetched list published"
            ),
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "publishing the fetched list failed"
            ),
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(doc: &str, batch: &str, res: &str) -> String {
        format!(
            "{{\"doc_id\":\"{doc}\",\"batch\":\"{batch}\",\"batch_version\":1,\"loc_url\":\"u\",\"iiif_base\":\"b\",\"resolution\":\"{res}\",\"fetched_at\":\"2026-10-11T23:00:00Z\",\"bytes\":1,\"sha256\":\"x\",\"download_ms\":1,\"user_agent\":\"a\"}}\n"
        )
    }

    async fn server(body: String) -> String {
        use axum::routing::get;
        let app = axum::Router::new().route("/index/full.jsonl", get(move || async move { body }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn publishes_the_full_size_scans_once_each() {
        let body = [
            line("sn1_1890-05-03_ed-1_seq-1", "nbu_gage", "full"),
            line("sn1_1890-05-03_ed-1_seq-1", "nbu_gage", "full"),
            line("sn1_1890-05-03_ed-1_seq-2", "nbu_gage", "scaled:50"),
            line("sn1_1890-05-03_ed-1_seq-3", "../x", "full"),
            "not json\n\n".to_owned(),
            line("sn2_1901-01-01_ed-2_seq-9", "lu_Cypher", "full")
                .trim_end()
                .to_owned(),
        ]
        .concat();
        let url = server(body).await;
        let dir = tempfile::tempdir().unwrap();
        let out = usnm_store::open(dir.path().to_str().unwrap()).unwrap();
        let opts = Options {
            scan_server: url,
            manifest: "pages-v20261010-2".into(),
            node_id: "node2".into(),
        };
        let p = publish(out.as_ref(), &opts).await.unwrap();
        assert_eq!((p.lines, p.pages, p.skipped), (6, 2, 3));
        assert!(p.path.starts_with("fetched/pages-v20261010-2/node2."));
        let list = fetched::decode(out.get(&p.path).await.unwrap().unwrap().into()).unwrap();
        assert_eq!(list.manifest, "pages-v20261010-2");
        assert!(list.contains("lu_Cypher", "sn2_1901-01-01_ed-2_seq-9"));
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn lines_split_across_chunks_are_joined() {
        let l = line("sn1_1890-05-03_ed-1_seq-1", "nbu_gage", "full");
        let (a, b) = l.as_bytes().split_at(17);
        let mut f = Fold::default();
        f.chunk(a).unwrap();
        f.chunk(b).unwrap();
        let f = f.finish().unwrap();
        assert_eq!((f.lines, f.pages.len()), (1, 1));
        let mut f = Fold::default();
        assert!(f.chunk(&vec![b'x'; MAX_LINE + 1]).is_err());
        let mut long = vec![b'x'; MAX_LINE + 1];
        long.push(b'\n');
        let mut f = Fold::default();
        assert!(f.chunk(&long).is_err());
    }
}
