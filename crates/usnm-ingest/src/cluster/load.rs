//! Load a sample ([`super::sample`]) into a new index on the cluster, and
//! time each step:
//!
//! 1. **Send:** the sample's documents in requests of at most 8 MiB, from
//!    `senders` requests in flight at once, to the root's ingest API. With
//!    ingest v2 the root routes each request to an indexer's shard; the
//!    index asks for `min_shards` shards (one per indexer by default), so
//!    every indexer gets a share from the start. A request the cluster
//!    pushes back on (429, or 503 "no shards available") is retried, as the
//!    release does, and counted.
//! 2. **Committed:** every document searchable (`num_hits` of `*`).
//! 3. **Settled:** no merge running or queued on any indexer, and the same
//!    splits for three polls in a row.
//! 4. **Sealed:** the index's ingest source disabled, which runs each
//!    indexer's final merges, and settled again.
//!
//! The index config is the production one (`infra/quickwit/pages-index.yaml`)
//! with only `split_num_docs_target` changed, and the shards. A 1% sample at
//! the production target of 30,000 pages would be about 8 splits, too few
//! for a root to spread over three nodes the way it spreads about 800 in
//! production; at 3,000 (the default here) it is about 80 splits of a tenth
//! the size, so each node gets dozens, as in production. Per-split fixed
//! costs (opening a split, its footer) then weigh about 10 times more than
//! in production, so the 30,000 target is worth a second index to bracket it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use usnm_store::ObjectStore;

use super::members::{self, Member, NodeCounters};
use super::sample;
use crate::merges::{self, Settle};
use crate::sink::INDEX_TEMPLATE;

/// Request bodies stay well under Quickwit's 10 MiB limit, as the release's.
pub const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// The template line the split target replaces.
const TEMPLATE_TARGET: &str = "split_num_docs_target: 30000";

/// The production index config for `index_id`, with `split_docs` pages per
/// split and `min_shards` ingest shards.
pub fn index_config(
    index_id: &str,
    index_uri: &str,
    split_docs: u64,
    min_shards: usize,
) -> anyhow::Result<String> {
    if split_docs == 0 || min_shards == 0 {
        bail!("the split target and the shard count must be above 0");
    }
    let found = INDEX_TEMPLATE
        .lines()
        .filter(|l| l.trim() == TEMPLATE_TARGET)
        .count();
    if found != 1 || INDEX_TEMPLATE.contains("ingest_settings") {
        bail!("the index template needs exactly one `{TEMPLATE_TARGET}` and no ingest_settings");
    }
    let lines: Vec<String> = INDEX_TEMPLATE
        .lines()
        .map(|l| {
            if l.trim() == TEMPLATE_TARGET {
                let indent = &l[..l.len() - l.trim_start().len()];
                format!("{indent}split_num_docs_target: {split_docs}")
            } else {
                l.to_owned()
            }
        })
        .collect();
    let mut out = lines
        .join("\n")
        .replace("${INDEX_ID}", index_id)
        .replace("${INDEX_URI}", index_uri);
    out.push_str(&format!(
        "\n# The search cluster's loader (usnm-qwcluster load): a shard on each indexer.\n\
         ingest_settings:\n  min_shards: {min_shards}\n"
    ));
    Ok(out)
}

/// How to load a sample.
#[derive(Debug, Clone)]
pub struct Spec {
    /// The root node's REST API.
    pub root: String,
    pub index_id: String,
    /// Where the index lives (`azure://qw-cluster`).
    pub index_root: String,
    pub split_docs: u64,
    /// Shards to open; `None`: one per indexer.
    pub min_shards: Option<usize>,
    /// Ingest requests in flight at once.
    pub senders: usize,
    /// Most bytes per request ([`CHUNK_BYTES`]; tests use less).
    pub chunk_bytes: usize,
    /// Merges must settle and the index seal within this.
    pub merge_timeout: Duration,
    pub poll: Duration,
    /// After disabling the source, how long to give the indexers' final
    /// merges before a quiet cluster counts as done.
    pub finalize_grace: Duration,
    /// How long a request keeps retrying while the cluster pushes back.
    pub retry_for: Duration,
    /// Give every document `text`'s words again as American Stories' text
    /// ([`as_from_text`]): an index shaped like production's, which has
    /// both texts, from a sample without American Stories (#251).
    pub as_from_text: bool,
}

impl Spec {
    pub fn new(root: &str, index_id: &str, index_root: &str) -> Self {
        Spec {
            root: root.trim_end_matches('/').to_owned(),
            index_id: index_id.to_owned(),
            index_root: index_root.trim_end_matches('/').to_owned(),
            split_docs: 3_000,
            min_shards: None,
            senders: 4,
            chunk_bytes: CHUNK_BYTES,
            merge_timeout: Duration::from_secs(4 * 3600),
            poll: Duration::from_secs(10),
            finalize_grace: Duration::from_secs(60),
            retry_for: Duration::from_secs(600),
            as_from_text: false,
        }
    }
}

/// `ndjson` with each document's `text` and `text_cg` copied to `text_as`
/// and `text_as_cg` where those are missing or empty (#251). The sample
/// sets so far have no American Stories' text, so their indexes hold about
/// half of what a production split holds per page (05 §5.5.4: 37 KB
/// against 74 KB); with the copy, a search reads two positional fields per
/// text, as production's do. The copy is LoC's OCR again, not American
/// Stories': the shape of the work is production's, the matches aren't.
pub fn as_from_text(ndjson: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(ndjson.len() * 2);
    for line in ndjson.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut doc: Value = serde_json::from_slice(line).context("a sample document")?;
        let o = doc
            .as_object_mut()
            .context("a sample document isn't an object")?;
        for (from, to) in [("text", "text_as"), ("text_cg", "text_as_cg")] {
            let empty = o
                .get(to)
                .is_none_or(|v| v.is_null() || v.as_str() == Some(""));
            if let (true, Some(v)) = (empty, o.get(from).cloned()) {
                o.insert(to.to_owned(), v);
            }
        }
        serde_json::to_writer(&mut out, &doc)?;
        out.push(b'\n');
    }
    Ok(out)
}

/// One published split, with the node that wrote it.
#[derive(Debug, Clone, Serialize)]
pub struct SplitInfo {
    pub id: String,
    pub node_id: String,
    pub docs: u64,
    pub bytes: u64,
    pub footer_bytes: u64,
}

fn parse_splits(v: &Value) -> anyhow::Result<Vec<SplitInfo>> {
    v["splits"]
        .as_array()
        .context("the split list has no `splits`")?
        .iter()
        .map(|s| {
            let end = s["footer_offsets"]["end"].as_u64().unwrap_or(0);
            let start = s["footer_offsets"]["start"].as_u64().unwrap_or(end);
            Ok(SplitInfo {
                id: s["split_id"]
                    .as_str()
                    .context("a split has no id")?
                    .to_owned(),
                node_id: s["node_id"].as_str().unwrap_or_default().to_owned(),
                docs: s["num_docs"].as_u64().context("a split has no num_docs")?,
                bytes: end,
                footer_bytes: end.saturating_sub(start),
            })
        })
        .collect()
}

/// Every published split of `index_id`.
pub async fn splits(
    http: &reqwest::Client,
    root: &str,
    index_id: &str,
) -> anyhow::Result<Vec<SplitInfo>> {
    const PAGE: usize = 1000;
    let mut all = Vec::new();
    loop {
        let v: Value = http
            .get(format!("{root}/api/v1/indexes/{index_id}/splits"))
            .query(&[
                ("split_states", "Published"),
                ("offset", &all.len().to_string()),
                ("limit", &PAGE.to_string()),
            ])
            .send()
            .await?
            .error_for_status()
            .with_context(|| format!("listing the splits of `{index_id}`"))?
            .json()
            .await?;
        let page = parse_splits(&v)?;
        let n = page.len();
        all.extend(page);
        if n < PAGE {
            break;
        }
    }
    all.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(all)
}

/// Splits in all, and by node, with their sizes.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SplitStats {
    pub splits: u64,
    pub docs: u64,
    pub bytes: u64,
    pub footer_bytes: u64,
    pub largest_footer_bytes: u64,
    pub min_docs: u64,
    pub median_docs: u64,
    pub max_docs: u64,
    pub median_bytes: u64,
    pub max_bytes: u64,
    pub by_node: BTreeMap<String, u64>,
}

impl SplitStats {
    pub fn of(splits: &[SplitInfo]) -> Self {
        let mut docs: Vec<u64> = splits.iter().map(|s| s.docs).collect();
        let mut bytes: Vec<u64> = splits.iter().map(|s| s.bytes).collect();
        docs.sort_unstable();
        bytes.sort_unstable();
        let median = |v: &[u64]| v.get(v.len() / 2).copied().unwrap_or(0);
        let mut by_node = BTreeMap::new();
        for s in splits {
            *by_node.entry(s.node_id.clone()).or_default() += 1;
        }
        SplitStats {
            splits: splits.len() as u64,
            docs: docs.iter().sum(),
            bytes: bytes.iter().sum(),
            footer_bytes: splits.iter().map(|s| s.footer_bytes).sum(),
            largest_footer_bytes: splits.iter().map(|s| s.footer_bytes).max().unwrap_or(0),
            min_docs: docs.first().copied().unwrap_or(0),
            median_docs: median(&docs),
            max_docs: docs.last().copied().unwrap_or(0),
            median_bytes: median(&bytes),
            max_bytes: bytes.last().copied().unwrap_or(0),
            by_node,
        }
    }
}

/// Counters shared by the senders.
#[derive(Debug, Default)]
struct Sent {
    docs: AtomicU64,
    bytes: AtomicU64,
    requests: AtomicU64,
    retries_429: AtomicU64,
    retries_503: AtomicU64,
}

/// What a load did, step by step.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub index_id: String,
    pub sample: String,
    pub sample_version: String,
    pub members: Vec<Member>,
    pub indexers: Vec<String>,
    pub senders: usize,
    pub split_docs: u64,
    pub min_shards: usize,
    /// American Stories' fields filled from `text` ([`as_from_text`]).
    pub as_from_text: bool,
    pub docs: u64,
    pub bytes: u64,
    pub requests: u64,
    pub retries_429: u64,
    pub retries_503: u64,
    pub started_at: DateTime<Utc>,
    pub sent_at: DateTime<Utc>,
    pub committed_at: DateTime<Utc>,
    pub settled_at: DateTime<Utc>,
    pub sealed_at: DateTime<Utc>,
    /// Seconds from the start to each step.
    pub send_secs: f64,
    pub commit_secs: f64,
    pub settle_secs: f64,
    pub seal_secs: f64,
    /// Documents per second while sending.
    pub docs_per_sec: f64,
    /// Documents acknowledged, every `poll` from the start: (secs, docs).
    pub rate: Vec<(f64, u64)>,
    /// Splits after sealing.
    pub splits: SplitStats,
    /// Each node's counters over the load (documents indexed, by node).
    pub nodes: BTreeMap<String, NodeCounters>,
}

impl Report {
    /// The report without its per-poll series (for the job's log line),
    /// with documents per second by minute instead.
    pub fn summary(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        let mut per_min: Vec<Value> = Vec::new();
        let mut last = (0.0, 0u64);
        for &(t, d) in &self.rate {
            if t - last.0 >= 60.0 {
                per_min.push(json!([
                    t.round(),
                    ((d - last.1) as f64 / (t - last.0)).round()
                ]));
                last = (t, d);
            }
        }
        v["rate"] = Value::Array(per_min);
        v
    }
}

async fn send_chunk(
    http: &reqwest::Client,
    url: &str,
    body: bytes::Bytes,
    retry_for: Duration,
    sent: &Sent,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + retry_for;
    let mut pause = Duration::from_millis(500);
    loop {
        let resp = http
            .post(url)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let pushback = status == reqwest::StatusCode::SERVICE_UNAVAILABLE
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
        if pushback && tokio::time::Instant::now() + pause <= deadline {
            match status.as_u16() {
                429 => sent.retries_429.fetch_add(1, Ordering::Relaxed),
                _ => sent.retries_503.fetch_add(1, Ordering::Relaxed),
            };
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_secs(8));
            continue;
        }
        if !status.is_success() {
            bail!(
                "ingest returned {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&text).context("ingest response")?;
        if v["num_rejected_docs"].as_u64() != Some(0) {
            bail!(
                "ingest rejected documents: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        let docs = body.iter().filter(|&&b| b == b'\n').count() as u64;
        sent.docs.fetch_add(docs, Ordering::Relaxed);
        sent.bytes.fetch_add(body.len() as u64, Ordering::Relaxed);
        sent.requests.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
}

/// Split NDJSON into request bodies of at most [`CHUNK_BYTES`], whole lines.
pub fn chunks(ndjson: &[u8], limit: usize) -> anyhow::Result<Vec<bytes::Bytes>> {
    let mut out = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    for line in ndjson.split_inclusive(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if line.len() > limit {
            bail!("a document is {} bytes, over the request limit", line.len());
        }
        if cur.len() + line.len() > limit {
            out.push(bytes::Bytes::from(std::mem::take(&mut cur)));
        }
        cur.extend_from_slice(line);
        if !line.ends_with(b"\n") {
            cur.push(b'\n');
        }
    }
    if !cur.is_empty() {
        out.push(bytes::Bytes::from(cur));
    }
    Ok(out)
}

async fn count(http: &reqwest::Client, root: &str, index_id: &str) -> anyhow::Result<u64> {
    let v: Value = http
        .get(format!(
            "{root}/api/v1/{index_id}/search?query=*&max_hits=0"
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    v["num_hits"]
        .as_u64()
        .context("search response has no num_hits")
}

/// What to load: a sample ([`sample`]) or a packaged set ([`super::set`]).
pub enum Input<'a> {
    Sample {
        store: &'a dyn ObjectStore,
        prefix: &'a str,
    },
    Set {
        store: &'a dyn ObjectStore,
        manifest: &'a super::set::Manifest,
    },
}

/// Load the sample at `prefix` in `store` into a new index, as `spec` says.
pub async fn run(store: &dyn ObjectStore, prefix: &str, spec: &Spec) -> anyhow::Result<Report> {
    run_from(&Input::Sample { store, prefix }, spec).await
}

/// Load `input` into a new index, as `spec` says.
pub async fn run_from(input: &Input<'_>, spec: &Spec) -> anyhow::Result<Report> {
    if !usnm_store::is_safe_segment(&spec.index_id) || spec.index_id.contains("qw-index") {
        bail!("`{}` isn't a usable index id", spec.index_id);
    }
    if spec.index_root.contains("qw-index") {
        bail!("the cluster's indexes never go in qw-index");
    }
    // What is loaded: its documents, version and name.
    let (expected_docs, version, label, parts) = match input {
        Input::Sample { store, prefix } => {
            let m = sample::manifest(*store, prefix).await?;
            (m.docs, m.version, (*prefix).to_owned(), m.parts)
        }
        Input::Set { manifest, .. } => (
            manifest.docs,
            manifest.source_version.clone(),
            format!("set/{}", manifest.name),
            Vec::new(),
        ),
    };
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;
    let root = spec.root.as_str();
    let all = members::members(&http, root).await?;
    let indexers: Vec<Member> = all
        .iter()
        .filter(|m| m.ready && m.runs("indexer"))
        .cloned()
        .collect();
    if indexers.is_empty() {
        bail!("no ready indexer in the cluster: {all:?}");
    }
    let min_shards = spec.min_shards.unwrap_or(indexers.len());
    let config = index_config(
        &spec.index_id,
        &format!("{}/{}", spec.index_root, spec.index_id),
        spec.split_docs,
        min_shards,
    )?;
    let resp = http
        .post(format!("{root}/api/v1/indexes"))
        .header("content-type", "application/yaml")
        .body(config)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        bail!(
            "creating `{}` returned {status}: {}",
            spec.index_id,
            resp.text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    let before = members::counters(&http, &all, Some(&spec.index_id)).await;
    tracing::info!(
        index = %spec.index_id,
        input = %label,
        docs = expected_docs,
        indexers = indexers.len(),
        min_shards,
        senders = spec.senders,
        split_docs = spec.split_docs,
        as_from_text = spec.as_from_text,
        "loading the sample"
    );

    let started_at = Utc::now();
    let start = tokio::time::Instant::now();
    let sent = Arc::new(Sent::default());
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(spec.senders.max(1) * 2);
    let rx = Arc::new(tokio::sync::Mutex::new(rx));
    let url = format!("{root}/api/v1/{}/ingest?commit=auto", spec.index_id);
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..spec.senders.max(1) {
        let (rx, sent, http, url, retry_for, copy_as, limit) = (
            rx.clone(),
            sent.clone(),
            http.clone(),
            url.clone(),
            spec.retry_for,
            spec.as_from_text,
            spec.chunk_bytes,
        );
        workers.spawn(async move {
            loop {
                let next = rx.lock().await.recv().await;
                let Some(body) = next else { return Ok(()) };
                // The copy about doubles a chunk: cut it again under the limit.
                let bodies = if copy_as {
                    chunks(&as_from_text(&body)?, limit)?
                } else {
                    vec![body]
                };
                for body in bodies {
                    send_chunk(&http, &url, body, retry_for, &sent).await?;
                }
            }
        });
    }
    // Only the senders hold the receiver: if they all fail, the channel
    // closes and the feed stops instead of waiting on a full buffer.
    drop(rx);
    // Progress every poll, and the series for the report.
    let rate = Arc::new(std::sync::Mutex::new(Vec::<(f64, u64)>::new()));
    let ticker = {
        let (sent, rate, poll, expected) = (sent.clone(), rate.clone(), spec.poll, expected_docs);
        tokio::spawn(async move {
            let mut last = (0.0f64, 0u64);
            loop {
                tokio::time::sleep(poll).await;
                let t = start.elapsed().as_secs_f64();
                let d = sent.docs.load(Ordering::Relaxed);
                rate.lock().unwrap_or_else(|e| e.into_inner()).push((t, d));
                tracing::info!(
                    docs_sent = d,
                    docs_expected = expected,
                    docs_per_sec = ((d - last.1) as f64 / (t - last.0).max(0.001)).round(),
                    mb_sent = sent.bytes.load(Ordering::Relaxed) / (1024 * 1024),
                    retries_429 = sent.retries_429.load(Ordering::Relaxed),
                    retries_503 = sent.retries_503.load(Ordering::Relaxed),
                    "cluster load progress"
                );
                last = (t, d);
            }
        })
    };
    let feed = async {
        match input {
            Input::Sample { store, .. } => {
                for p in &parts {
                    let alive = sample::feed(
                        *store,
                        &p.path,
                        Some(p.compressed_bytes),
                        None,
                        spec.chunk_bytes,
                        tx.clone(),
                    )
                    .await?;
                    if !alive {
                        // A sender failed; its error comes from the join below.
                        return Ok::<_, anyhow::Error>(());
                    }
                }
                Ok(())
            }
            Input::Set { store, manifest } => {
                super::set::feed_docs(*store, manifest, spec.chunk_bytes, tx.clone()).await
            }
        }
    };
    let fed = feed.await;
    drop(tx);
    let mut failed = fed.err();
    while let Some(r) = workers.join_next().await {
        if let Err(e) = r.context("a sender's task failed").and_then(|r| r) {
            failed.get_or_insert(e);
        }
    }
    let send_secs = start.elapsed().as_secs_f64();
    let sent_at = Utc::now();
    ticker.abort();
    if let Some(e) = failed {
        return Err(e.context(format!("loading `{}`", spec.index_id)));
    }
    let docs = sent.docs.load(Ordering::Relaxed);
    if docs != expected_docs {
        bail!("sent {docs} documents, {label} has {expected_docs}");
    }

    // Committed: every document searchable.
    let commit_deadline = tokio::time::Instant::now() + Duration::from_secs(1800);
    loop {
        let n = count(&http, root, &spec.index_id).await?;
        if n == docs {
            break;
        }
        if n > docs {
            bail!("`{}` holds {n} documents, {docs} were sent", spec.index_id);
        }
        if tokio::time::Instant::now() >= commit_deadline {
            bail!(
                "`{}` holds {n} of {docs} documents after 30 minutes",
                spec.index_id
            );
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let commit_secs = start.elapsed().as_secs_f64();
    let committed_at = Utc::now();
    tracing::info!(index = %spec.index_id, docs, secs = commit_secs.round(), "every document committed");

    // Settle, close, settle again.
    let deadline = tokio::time::Instant::now() + spec.merge_timeout;
    let node = merges::Node {
        http: &http,
        base: root,
    };
    let settle = |label: &'static str, grace: Duration| {
        let (http, node, indexers, index_id) = (&http, &node, &indexers, &spec.index_id);
        async move {
            let since = tokio::time::Instant::now();
            let mut s = Settle::new(3);
            loop {
                let c = members::counters(http, indexers, Some(index_id)).await;
                let running: f64 = c.values().map(|n| n.merges_running + n.merges_queued).sum();
                // An indexer whose metrics didn't answer may be merging.
                let heard = c.len() == indexers.len();
                let split_ids: Vec<String> = node
                    .splits(index_id)
                    .await?
                    .into_iter()
                    .map(|s| s.id)
                    .collect();
                tracing::info!(
                    index = %index_id,
                    step = label,
                    splits = split_ids.len(),
                    merges = running,
                    indexers_heard = c.len(),
                    "cluster merges"
                );
                if s.observe(heard && running == 0.0, &split_ids) && since.elapsed() >= grace {
                    return Ok::<_, anyhow::Error>(());
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!("merges into `{index_id}` did not settle in time ({label})");
                }
                tokio::time::sleep(spec.poll).await;
            }
        }
    };
    settle("settle", Duration::ZERO).await?;
    let settle_secs = start.elapsed().as_secs_f64();
    let settled_at = Utc::now();
    node.close(&spec.index_id).await?;
    settle("finalize", spec.finalize_grace).await?;
    let seal_secs = start.elapsed().as_secs_f64();
    let sealed_at = Utc::now();

    let layout = SplitStats::of(&splits(&http, root, &spec.index_id).await?);
    if layout.docs != docs {
        bail!(
            "the splits of `{}` hold {} documents, {docs} were sent",
            spec.index_id,
            layout.docs
        );
    }
    let after = members::counters(&http, &all, Some(&spec.index_id)).await;
    let nodes = after
        .iter()
        .map(|(id, a)| {
            let b = before.get(id).cloned().unwrap_or_default();
            (id.clone(), a.since(&b))
        })
        .collect();
    let rate = rate.lock().unwrap_or_else(|e| e.into_inner()).clone();
    Ok(Report {
        index_id: spec.index_id.clone(),
        sample: label,
        sample_version: version,
        members: all,
        indexers: indexers.iter().map(|m| m.node_id.clone()).collect(),
        senders: spec.senders,
        split_docs: spec.split_docs,
        min_shards,
        as_from_text: spec.as_from_text,
        docs,
        bytes: sent.bytes.load(Ordering::Relaxed),
        requests: sent.requests.load(Ordering::Relaxed),
        retries_429: sent.retries_429.load(Ordering::Relaxed),
        retries_503: sent.retries_503.load(Ordering::Relaxed),
        started_at,
        sent_at,
        committed_at,
        settled_at,
        sealed_at,
        send_secs,
        commit_secs,
        settle_secs,
        seal_secs,
        docs_per_sec: docs as f64 / send_secs.max(0.001),
        rate,
        splits: layout,
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_config_is_productions_with_the_target_and_shards() {
        let yaml = index_config("sample-1", "azure://qw-cluster/sample-1", 3000, 2).unwrap();
        assert!(yaml.contains("index_id: sample-1\n"));
        assert!(yaml.contains("index_uri: azure://qw-cluster/sample-1\n"));
        assert!(yaml.contains("  split_num_docs_target: 3000\n"));
        assert!(!yaml.contains("30000"));
        assert!(yaml.ends_with("ingest_settings:\n  min_shards: 2\n"));
        // Everything else is the template's.
        let template: Vec<&str> = INDEX_TEMPLATE
            .lines()
            .filter(|l| !l.contains("split_num_docs_target") && !l.contains("${"))
            .collect();
        for l in template {
            assert!(yaml.lines().any(|y| y == l), "{l}");
        }
        assert!(index_config("x1", "u", 0, 1).is_err());
        assert!(index_config("x1", "u", 1, 0).is_err());
    }

    #[test]
    fn american_stories_fields_copy_the_text_where_empty() {
        let ndjson = br#"{"doc_id":"a","text":"cross of gold","text_cg":"cross_of of_gold"}
{"doc_id":"b","text":"loc","text_cg":"","text_as":"stories","text_as_cg":""}

"#;
        let out = as_from_text(ndjson).unwrap();
        let docs: Vec<Value> = out
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0]["text_as"], "cross of gold");
        assert_eq!(docs[0]["text_as_cg"], "cross_of of_gold");
        // A document's own American Stories' text stays.
        assert_eq!(docs[1]["text_as"], "stories");
        assert_eq!(docs[1]["text_as_cg"], "");
        assert!(as_from_text(b"not json\n").is_err());
        // Cut again under the limit, the copy still makes whole lines.
        let bodies = chunks(&out, 130).unwrap();
        assert!(bodies.len() >= 2 && bodies.iter().all(|b| b.len() <= 130 && b.ends_with(b"\n")));
    }

    #[test]
    fn chunks_hold_whole_lines_under_the_limit() {
        let ndjson = b"{\"a\":1}\n{\"b\":22}\n\n{\"c\":333}";
        let c = chunks(ndjson, 18).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(&c[0][..], b"{\"a\":1}\n{\"b\":22}\n");
        assert_eq!(&c[1][..], b"{\"c\":333}\n");
        assert!(chunks(b"{\"long\":\"xxxxxxxxxxxxxxxx\"}\n", 10).is_err());
        assert!(chunks(b"", 10).unwrap().is_empty());
    }

    #[test]
    fn split_stats_by_node() {
        let s = |id: &str, node: &str, docs: u64| SplitInfo {
            id: id.into(),
            node_id: node.into(),
            docs,
            bytes: docs * 10,
            footer_bytes: docs,
        };
        let st = SplitStats::of(&[
            s("a", "qw-0", 3000),
            s("b", "qw-1", 1000),
            s("c", "qw-0", 2000),
        ]);
        assert_eq!((st.splits, st.docs, st.median_docs), (3, 6000, 2000));
        assert_eq!(
            (st.min_docs, st.max_docs, st.largest_footer_bytes),
            (1000, 3000, 3000)
        );
        assert_eq!(st.by_node["qw-0"], 2);
        assert_eq!(SplitStats::of(&[]).splits, 0);
    }

    #[test]
    fn the_summary_rate_is_per_minute() {
        let r = Report {
            index_id: "i".into(),
            sample: "s".into(),
            sample_version: "v".into(),
            members: vec![],
            indexers: vec![],
            senders: 1,
            split_docs: 1,
            min_shards: 1,
            as_from_text: false,
            docs: 0,
            bytes: 0,
            requests: 0,
            retries_429: 0,
            retries_503: 0,
            started_at: Utc::now(),
            sent_at: Utc::now(),
            committed_at: Utc::now(),
            settled_at: Utc::now(),
            sealed_at: Utc::now(),
            send_secs: 0.0,
            commit_secs: 0.0,
            settle_secs: 0.0,
            seal_secs: 0.0,
            docs_per_sec: 0.0,
            rate: (1..=13u32)
                .map(|i| (f64::from(i) * 10.0, u64::from(i) * 1000))
                .collect(),
            splits: SplitStats::default(),
            nodes: BTreeMap::new(),
        };
        let v = r.summary();
        assert_eq!(v["rate"], json!([[60.0, 100.0], [120.0, 100.0]]));
    }
}
