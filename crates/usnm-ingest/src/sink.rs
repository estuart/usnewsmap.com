//! Where index documents go: JSONL files (the API's memory backend, local
//! development and tests) or a Quickwit writer node (08 §8.4.1).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use async_trait::async_trait;
use opentelemetry::KeyValue;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use usnm_core::text_layout::TextLayout;

use crate::merges::{self, IndexLayout, MergeWait, NodeEvents};
use crate::progress;
use crate::telemetry;

/// The index config every base and delta shares (05 §5.5.1).
pub const INDEX_TEMPLATE: &str = include_str!("../../../infra/quickwit/pages-index.yaml");

/// Documents per ingest request, bounded well under Quickwit's 10 MiB body limit.
const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// Per-shard ingest rate on the writer node: Quickwit's maximum (its default
/// is 5 MB/s). A single-node writer can't spread load over more shards, so
/// past the ~50 MiB burst allowance ingest runs at this rate.
const SHARD_THROUGHPUT_LIMIT: &str = "20MB";
/// The node's in-memory ingest queue (Quickwit's default is 2 GiB). The first
/// prod release ran out of memory at 4 GiB with the defaults.
const INGEST_QUEUE_MEMORY: &str = "1GiB";

/// How hard the writer may work for one run (#172): Quickwit's indexing heap
/// and commit timeout for the new index, and the node's in-memory ingest
/// queue. The defaults are the index template's and [`INGEST_QUEUE_MEMORY`],
/// sized for the Consumption profile's 7.5 GiB container. On the E4 profile
/// (26 GiB, a full rebuild) the ingest job sets more through
/// `USNM_WRITER_HEAP`, `USNM_WRITER_COMMIT_SECS` and `USNM_WRITER_QUEUE`: with
/// a heap that holds a whole 30,000-page split and a commit timeout longer than
/// it takes to index one, the indexer writes splits at the target size, so
/// they need little merging. At 30 s, a full rebuild's indexer cut splits of
/// about 7,000 pages, and merging them cost the writer its memory and much of
/// its CPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterTuning {
    pub heap: String,
    pub commit_timeout_secs: u32,
    pub queue: String,
}

/// How a main index lays out its pages by decade (05 §5.5.5, #123). Only
/// this run's new main index takes it; the Japanese index never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decades {
    /// No `decade` field: the template as it is.
    #[default]
    Off,
    /// The `decade` field, recorded as a split tag (`tag_fields`): a delta
    /// on a version laid out by decade. Its splits mix decades as before,
    /// but a search skips each split that holds none of its decades.
    Tagged,
    /// Also `partition_key: decade`, so every split holds one decade: a
    /// full base built with `--partition-decade`.
    Partitioned,
}

/// The template lines [`Decades`] adds to: the doc mapping's timestamp
/// field and the `year` field mapping.
const TEMPLATE_TIMESTAMP: &str = "timestamp_field: date";
const TEMPLATE_YEAR: &str = "- { name: year, type: u64, fast: true, indexed: true }";

/// Partitions an indexer keeps apart at once, more than the 15 decades
/// (1820s to 1960s, `usnm_core::decade`). Past it, Quickwit 0.9.1 puts the
/// pages of further decades into one shared split, which a search can't
/// skip. Each partition has its own in-memory split while the indexer
/// fills, all within the one indexing heap.
pub const MAX_NUM_PARTITIONS: u32 = 32;

impl Decades {
    /// Whether the pages carry the `decade` field.
    pub fn on(self) -> bool {
        self != Decades::Off
    }

    /// `template` with the `decade` field, and its tag and partitioning as
    /// this layout has them. `Off` leaves it as it is. Each line it adds to
    /// must be there exactly once, so a template change can't silently
    /// drop the layout.
    pub fn apply(self, template: &str) -> anyhow::Result<String> {
        if self == Decades::Off {
            return Ok(template.to_owned());
        }
        let mut found = (0, 0);
        let mut out = Vec::new();
        for line in template.lines() {
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push(line.to_owned());
            match line.trim() {
                TEMPLATE_TIMESTAMP => {
                    found.0 += 1;
                    out.push(format!(
                        "{indent}# Decade partitions (05 §5.5.5, #123): searches name their \
                         decades (`decade:IN [...]`), and Quickwit skips the splits of the others."
                    ));
                    out.push(format!("{indent}tag_fields: [decade]"));
                    if self == Decades::Partitioned {
                        out.push(format!("{indent}partition_key: decade"));
                        out.push(format!("{indent}max_num_partitions: {MAX_NUM_PARTITIONS}"));
                    }
                }
                TEMPLATE_YEAR => {
                    found.1 += 1;
                    out.push(format!(
                        "{indent}# The page's decade, from 1820 (usnm_core::decade::of_year)."
                    ));
                    out.push(format!(
                        "{indent}- {{ name: decade, type: u64, fast: false, indexed: true, stored: false }}"
                    ));
                }
                _ => {}
            }
        }
        if found != (1, 1) {
            bail!(
                "the index template needs exactly one `{TEMPLATE_TIMESTAMP}` and one `{TEMPLATE_YEAR}` line to add the decade to, not {} and {}",
                found.0,
                found.1
            );
        }
        let mut out = out.join("\n");
        if template.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

/// How this run's new main index lays its pages out: by decade (05
/// §5.5.5), in which text fields (05 §5.5.7) and in splits of how many
/// pages. Only the main index takes it; the Japanese index keeps its own
/// mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MainLayout {
    pub decades: Decades,
    pub text: TextLayout,
    /// The split target in place of the template's (`--split-docs`, #283):
    /// only a full base takes one. `None` keeps the template's.
    pub split_docs: Option<u64>,
}

impl From<Decades> for MainLayout {
    fn from(decades: Decades) -> Self {
        MainLayout {
            decades,
            ..MainLayout::default()
        }
    }
}

impl MainLayout {
    /// The pages a merge stops at in this index: `split_docs`, or the
    /// template's `split_num_docs_target`.
    pub fn split_target(&self) -> u64 {
        self.split_docs
            .unwrap_or_else(merges::template_split_target)
    }
}

/// The main index's config for this run: the template with the writer's
/// tuning, the split target, the decade layout and the text fields.
pub fn main_template(tuning: &WriterTuning, layout: MainLayout) -> anyhow::Result<String> {
    let mut template = text_fields(layout.text, &tuning.apply(INDEX_TEMPLATE)?)?;
    if let Some(docs) = layout.split_docs {
        template = split_target(docs, &template)?;
    }
    layout.decades.apply(&template)
}

/// The template line [`split_target`] replaces.
const TEMPLATE_SPLIT_TARGET: &str = "split_num_docs_target: 30000";

/// The split targets [`split_target`] takes are below this. A
/// million-page split (about 23 GB before the common-word pairs) killed
/// the writer while it uploaded in October 2026 (08 §8.4).
pub const MAX_SPLIT_DOCS: u64 = 1_000_000;

/// `template` with `split_num_docs_target: docs` (#283): a merge stops
/// taking splits once it holds that many pages, so a larger target leaves
/// fewer, larger splits. The line must be there exactly once, so a
/// template change can't silently keep the template's target.
pub fn split_target(docs: u64, template: &str) -> anyhow::Result<String> {
    if docs == 0 || docs >= MAX_SPLIT_DOCS {
        bail!("the split target must be above 0 and below {MAX_SPLIT_DOCS}, not {docs}");
    }
    let found = template
        .lines()
        .filter(|l| l.trim() == TEMPLATE_SPLIT_TARGET)
        .count();
    if found != 1 {
        bail!("the index template needs exactly one `{TEMPLATE_SPLIT_TARGET}` line, not {found}");
    }
    let mut out = template
        .lines()
        .map(|l| {
            if l.trim() == TEMPLATE_SPLIT_TARGET {
                let indent = &l[..l.len() - l.trim_start().len()];
                format!("{indent}split_num_docs_target: {docs}")
            } else {
                l.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if template.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// The template's text field mappings for [`TextLayout::Separate`], in the
/// order they appear: LoC's text and pairs, American Stories' text and pairs.
const TEMPLATE_TEXT_FIELDS: [&str; 4] = [
    "- { name: text, type: text, tokenizer: usnm_text, record: position, stored: true, fieldnorms: false }",
    "- { name: text_cg, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
    "- { name: text_as, type: text, tokenizer: usnm_text, record: position, stored: true, fieldnorms: false }",
    "- { name: text_as_cg, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
];
const TEMPLATE_SEARCH_FIELDS: &str = "default_search_fields: [text]";

/// `template` with the text fields of `layout` (05 §5.5.7). `Separate`
/// leaves it as it is. `Single` keeps `text` and `text_as` stored but not
/// indexed, drops their pairs, and adds `text_all` and `text_all_cg`, each
/// with the comment lines above it replaced. Each line it changes must be
/// there exactly once, so a template change can't silently keep a searched
/// field of one text.
pub fn text_fields(layout: TextLayout, template: &str) -> anyhow::Result<String> {
    if layout == TextLayout::Separate {
        return Ok(template.to_owned());
    }
    let mut found = [0usize; 5];
    let mut out: Vec<String> = Vec::new();
    for line in template.lines() {
        let indent = &line[..line.len() - line.trim_start().len()];
        let trimmed = line.trim();
        if let Some(i) = TEMPLATE_TEXT_FIELDS.iter().position(|f| *f == trimmed) {
            found[i] += 1;
            // The comment above the field described its old role.
            while out
                .last()
                .is_some_and(|l| l.trim_start().starts_with('#') && l.starts_with(indent))
            {
                out.pop();
            }
            if i == 0 {
                for l in [
                    "# One searched field for both texts (05 §5.5.7, #283). LoC's text (or",
                    "# our OCR's Latin text, #203) and American Stories' (05 §5.5.4) are",
                    "# stored for snippets and `matched_in`, not searched.",
                    "- { name: text, type: text, indexed: false, stored: true }",
                    "- { name: text_as, type: text, indexed: false, stored: true }",
                    "# Both texts' words, one token per word as the release folds them,",
                    "# with 32 positions no query can match between the texts",
                    "# (usnm_core::text_layout::index_fields), and their common-word",
                    "# pairs (05 §5.5.3), made separately for each text.",
                    "- { name: text_all, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
                    "- { name: text_all_cg, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
                ] {
                    out.push(format!("{indent}{l}"));
                }
            }
        } else if trimmed == TEMPLATE_SEARCH_FIELDS {
            found[4] += 1;
            out.push(format!(
                "{indent}default_search_fields: [{}]",
                usnm_core::text_layout::FIELD
            ));
        } else {
            out.push(line.to_owned());
        }
    }
    if found != [1; 5] {
        bail!(
            "the index template needs each of its four text fields and `{TEMPLATE_SEARCH_FIELDS}` \
             exactly once to lay them out in one field, not {found:?}"
        );
    }
    let mut out = out.join("\n");
    if template.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// The template lines [`WriterTuning`] replaces.
const TEMPLATE_HEAP: &str = "heap_size: 1GiB";
const TEMPLATE_COMMIT: &str = "commit_timeout_secs: 30";

impl Default for WriterTuning {
    fn default() -> Self {
        WriterTuning {
            heap: "1GiB".into(),
            commit_timeout_secs: 30,
            queue: INGEST_QUEUE_MEMORY.into(),
        }
    }
}

impl WriterTuning {
    /// From the environment, the defaults for what isn't set.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let d = WriterTuning::default();
        // A whole number of MiB or GiB above zero, with exactly one unit.
        let size = |key: &str, default: String| -> anyhow::Result<String> {
            let Some(v) = get(key) else {
                return Ok(default);
            };
            let amount = v.strip_suffix("GiB").or_else(|| v.strip_suffix("MiB"));
            match amount {
                Some(n)
                    if n.bytes().all(|b| b.is_ascii_digit())
                        && n.parse::<u64>().is_ok_and(|n| n > 0) =>
                {
                    Ok(v)
                }
                _ => bail!("{key} must be a size like 6GiB or 512MiB, not `{v}`"),
            }
        };
        let commit_timeout_secs = match get("USNM_WRITER_COMMIT_SECS") {
            None => d.commit_timeout_secs,
            Some(v) => match v.parse::<u32>() {
                Ok(n) if (1..=3600).contains(&n) => n,
                _ => bail!("USNM_WRITER_COMMIT_SECS must be 1 to 3600 seconds, not `{v}`"),
            },
        };
        Ok(WriterTuning {
            heap: size("USNM_WRITER_HEAP", d.heap)?,
            commit_timeout_secs,
            queue: size("USNM_WRITER_QUEUE", d.queue)?,
        })
    }

    /// `template` with this run's heap and commit timeout. Each must be an
    /// active setting line (not a comment) and there must be exactly one of
    /// each, so a template change can't silently drop the tuning.
    pub fn apply(&self, template: &str) -> anyhow::Result<String> {
        let mut found = (0, 0);
        let lines: Vec<String> = template
            .lines()
            .map(|line| {
                let indent = &line[..line.len() - line.trim_start().len()];
                match line.trim() {
                    TEMPLATE_HEAP => {
                        found.0 += 1;
                        format!("{indent}heap_size: {}", self.heap)
                    }
                    TEMPLATE_COMMIT => {
                        found.1 += 1;
                        format!("{indent}commit_timeout_secs: {}", self.commit_timeout_secs)
                    }
                    _ => line.to_owned(),
                }
            })
            .collect();
        if found != (1, 1) {
            bail!(
                "the index template needs exactly one `{TEMPLATE_HEAP}` and one `{TEMPLATE_COMMIT}` setting to tune, not {} and {}",
                found.0,
                found.1
            );
        }
        let mut out = lines.join("\n");
        if template.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}
/// The writer's local cache of uploaded splits (Quickwit's default is 100 GiB).
/// A job replica has ~19.5 GiB of disk: the first prod release filled it,
/// the ingester closed its shards, and every request got 503 "no shards
/// available" until the release gave up.
const SPLIT_STORE_BYTES: &str = "4GiB";
/// The ingest write-ahead log's disk cap (Quickwit's default, set here so
/// the scratch disk budget in 08 §8.4 adds up): past it, ingest gets 429s.
const WAL_DISK_BYTES: &str = "4GiB";
/// One merge at a time (the default is two thirds of the CPUs): the largest
/// merge needs its inputs and its output on disk at once, about twice
/// `split_num_docs_target` pages (08 §8.4).
const MERGE_CONCURRENCY: u32 = 1;

/// How long one ingest request keeps retrying while the node pushes back
/// (503 "no shards available" once the shard's rate limit is spent, or 429).
/// No attempt starts after this window, but one already in flight runs to the
/// client's 300 s timeout: cutting it short could abandon documents the node
/// has already taken, and resending them would duplicate them.
const INGEST_RETRY_FOR: Duration = Duration::from_secs(600);

#[async_trait]
pub trait IndexSink: Send {
    /// Start a new, empty main index, laid out as `layout` says: by decade
    /// (05 §5.5.5) and in its text fields (05 §5.5.7). Fails if it already
    /// exists.
    async fn create(&mut self, index_id: &str, layout: MainLayout) -> anyhow::Result<()>;
    /// Create an index with another mapping (the Japanese pages',
    /// `ocr_ja::JA_TEMPLATE`). Sinks without mappings just create it.
    async fn create_with(&mut self, index_id: &str, _template: &str) -> anyhow::Result<()> {
        self.create(index_id, MainLayout::default()).await
    }
    async fn add(&mut self, doc: &Value) -> anyhow::Result<()>;
    /// One document as the JSON line `add` would send, without its newline:
    /// for documents already written out (`crate::decade_order`).
    async fn add_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        self.add(&serde_json::from_slice(line)?).await
    }
    /// Flush and confirm the index holds exactly `expected` documents.
    async fn finish(&mut self, expected: u64) -> anyhow::Result<()>;
    /// `memory` or `quickwit`, recorded in `current.json`.
    fn backend(&self) -> &'static str;
    /// The search engine's name and version ("Quickwit 0.9.1"), for the
    /// version's build record (#161): the engine this sink writes to, however
    /// it was started. `None` when the sink has no engine or can't tell.
    async fn engine(&mut self) -> Option<String> {
        None
    }
    /// The published splits of `index_ids`, for the release log and
    /// manifest. Sinks without splits report none.
    async fn layout(&mut self, _index_ids: &[String]) -> anyhow::Result<Vec<IndexLayout>> {
        Ok(Vec::new())
    }
    /// Counters for release progress, shared with whoever reports it. A
    /// sink that doesn't count reports zeros.
    fn stats(&self) -> Arc<SinkStats> {
        Arc::default()
    }
}

/// What a sink has sent so far, and where to look for its resources.
#[derive(Debug, Default)]
pub struct SinkStats {
    docs_sent: AtomicU64,
    bytes_sent: AtomicU64,
    retries_429: AtomicU64,
    retries_503: AtomicU64,
    /// The file system whose free space the progress line reports.
    pub work_dir: Option<PathBuf>,
    /// The Quickwit writer node's process, for its memory use.
    pub node_pid: Option<u32>,
}

impl SinkStats {
    pub fn docs_sent(&self) -> u64 {
        self.docs_sent.load(Ordering::Relaxed)
    }
    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }
    pub fn retries_429(&self) -> u64 {
        self.retries_429.load(Ordering::Relaxed)
    }
    pub fn retries_503(&self) -> u64 {
        self.retries_503.load(Ordering::Relaxed)
    }

    fn sent(&self, docs: u64, bytes: u64) {
        self.docs_sent.fetch_add(docs, Ordering::Relaxed);
        self.bytes_sent.fetch_add(bytes, Ordering::Relaxed);
        let m = telemetry::metrics();
        m.docs_sent.add(docs, &[]);
        m.bytes_sent.add(bytes, &[]);
    }

    fn retried(&self, status: u16) {
        match status {
            429 => self.retries_429.fetch_add(1, Ordering::Relaxed),
            _ => self.retries_503.fetch_add(1, Ordering::Relaxed),
        };
        telemetry::metrics()
            .retries
            .add(1, &[KeyValue::new("status", i64::from(status))]);
    }
}

/// `{dir}/{index_id}.jsonl`, the layout the memory backend loads.
pub struct JsonlSink {
    dir: PathBuf,
    current: Option<(PathBuf, std::io::BufWriter<std::fs::File>)>,
    stats: Arc<SinkStats>,
}

impl JsonlSink {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            stats: Arc::new(SinkStats {
                work_dir: Some(dir.clone()),
                ..SinkStats::default()
            }),
            dir,
            current: None,
        }
    }
}

#[async_trait]
impl IndexSink for JsonlSink {
    /// The memory backend has no splits: the layout only changes the
    /// documents, which the release writes.
    async fn create(&mut self, index_id: &str, _layout: MainLayout) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.dir.join(format!("{index_id}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("{} (indexes are never rewritten)", path.display()))?;
        self.current = Some((path, std::io::BufWriter::new(file)));
        Ok(())
    }

    async fn add(&mut self, doc: &Value) -> anyhow::Result<()> {
        self.add_line(&serde_json::to_vec(doc)?).await
    }

    async fn add_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let (_, w) = self.current.as_mut().context("no index created")?;
        w.write_all(line)?;
        w.write_all(b"\n")?;
        self.stats.sent(1, line.len() as u64 + 1);
        Ok(())
    }

    async fn finish(&mut self, expected: u64) -> anyhow::Result<()> {
        use std::io::{BufRead, Write};
        let (path, mut w) = self.current.take().context("no index created")?;
        w.flush()?;
        w.get_ref().sync_all()?;
        let lines = std::io::BufReader::new(std::fs::File::open(&path)?)
            .lines()
            .count() as u64;
        if lines != expected {
            bail!(
                "{} holds {lines} documents, expected {expected}",
                path.display()
            );
        }
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "memory"
    }

    fn stats(&self) -> Arc<SinkStats> {
        self.stats.clone()
    }
}

/// Ingests into a Quickwit node that runs the indexer (the sole metastore writer).
pub struct QuickwitSink {
    base: String,
    index_root: String,
    http: reqwest::Client,
    index: Option<String>,
    buf: Vec<u8>,
    /// First pause after pushback; doubles up to 8 s.
    retry_initial: Duration,
    retry_for: Duration,
    stats: Arc<SinkStats>,
    /// How `finish` waits for the new index's merges.
    merge_wait: MergeWait,
    /// The new index's split target when it isn't the template's (a main
    /// index with `MainLayout::split_docs`), for the merge checks.
    split_docs: Option<u64>,
    /// The writer node's output, when this release runs the node.
    events: Option<Arc<NodeEvents>>,
}

impl QuickwitSink {
    /// `index_root` is where new indexes live (`azure://qw-index` or `file:///…`).
    pub fn new(base_url: &str, index_root: &str) -> anyhow::Result<Self> {
        Ok(Self {
            base: base_url.trim_end_matches('/').to_owned(),
            index_root: index_root.trim_end_matches('/').to_owned(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(300))
                .build()?,
            index: None,
            buf: Vec::new(),
            retry_initial: Duration::from_millis(500),
            retry_for: INGEST_RETRY_FOR,
            stats: Arc::default(),
            merge_wait: MergeWait::default(),
            split_docs: None,
            events: None,
        })
    }

    /// Report the free disk of `node`'s work directory and its memory in
    /// release progress, and stop the release if its output reports a full
    /// disk or a failed merge. Call before sending anything.
    pub fn watching(mut self, node: &QuickwitNode) -> Self {
        self.stats = Arc::new(SinkStats {
            work_dir: Some(node.work_dir.clone()),
            node_pid: node.pid(),
            ..SinkStats::default()
        });
        self.events = Some(node.events.clone());
        self
    }

    /// How long `finish` may wait for merges, and how it polls.
    pub fn merges(mut self, wait: MergeWait) -> Self {
        self.merge_wait = wait;
        self
    }

    fn node(&self) -> merges::Node<'_> {
        merges::Node {
            http: &self.http,
            base: &self.base,
        }
    }

    async fn send(&mut self, commit: &str) -> anyhow::Result<()> {
        if self.buf.is_empty() && commit != "force" {
            return Ok(());
        }
        if let Some(e) = &self.events {
            e.check()?;
        }
        let id = self.index.as_deref().context("no index created")?;
        let body = bytes::Bytes::from(std::mem::take(&mut self.buf));
        let url = format!("{}/api/v1/{id}/ingest?commit={commit}", self.base);
        let deadline = tokio::time::Instant::now() + self.retry_for;
        let mut pause = self.retry_initial;
        let mut attempt = 1u32;
        let (status, text) = loop {
            let sent = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .body(body.clone())
                .send()
                .await
                .map_err(anyhow::Error::from);
            let resp = merges::explained(self.events.as_deref(), sent).await?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            // The whole request is resent; `finish` checks the final count, so
            // a duplicate would fail the release rather than go unnoticed.
            let pushback = status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
            if !pushback || tokio::time::Instant::now() + pause > deadline {
                break (status, text);
            }
            tracing::warn!(
                index = id,
                status = status.as_u16(),
                attempt,
                pause_ms = pause.as_millis() as u64,
                error = %text.chars().take(200).collect::<String>(),
                "Quickwit pushed back; retrying"
            );
            self.stats.retried(status.as_u16());
            tokio::time::sleep(pause).await;
            pause = (pause * 2).min(Duration::from_secs(8));
            attempt += 1;
        };
        if !status.is_success() {
            bail!(
                "ingest into `{id}` returned {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&text).context("ingest response")?;
        if v["num_rejected_docs"].as_u64() != Some(0) {
            bail!(
                "ingest into `{id}` rejected documents: {}",
                text.chars().take(500).collect::<String>()
            );
        }
        let docs = body.iter().filter(|&&b| b == b'\n').count() as u64;
        self.stats.sent(docs, body.len() as u64);
        Ok(())
    }

    async fn count(&self, id: &str) -> anyhow::Result<u64> {
        let counted = self.count_once(id).await;
        merges::explained(self.events.as_deref(), counted).await
    }

    async fn count_once(&self, id: &str) -> anyhow::Result<u64> {
        let v: Value = self
            .http
            .get(format!(
                "{}/api/v1/{id}/search?query=*&max_hits=0",
                self.base
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
}

#[async_trait]
impl IndexSink for QuickwitSink {
    async fn create(&mut self, index_id: &str, layout: MainLayout) -> anyhow::Result<()> {
        // The main index takes this run's writer tuning and layout; the
        // Japanese index (about 11k pages) keeps its template's.
        let tuning = WriterTuning::from_env()?;
        if tuning != WriterTuning::default() {
            tracing::info!(heap = %tuning.heap, commit_timeout_secs = tuning.commit_timeout_secs, queue = %tuning.queue, "writer tuning");
        }
        if layout.decades.on() {
            tracing::info!(index = index_id, layout = ?layout.decades, "decade layout");
        }
        if layout.text != TextLayout::Separate {
            tracing::info!(
                index = index_id,
                text_layout = layout.text.version(),
                "one searched text field"
            );
        }
        if let Some(docs) = layout.split_docs {
            tracing::info!(index = index_id, split_docs = docs, "split target");
        }
        let template = main_template(&tuning, layout)?;
        self.create_with(index_id, &template).await?;
        self.split_docs = layout.split_docs;
        Ok(())
    }

    async fn create_with(&mut self, index_id: &str, template: &str) -> anyhow::Result<()> {
        let config = template
            .replace("${INDEX_ID}", index_id)
            .replace("${INDEX_URI}", &format!("{}/{index_id}", self.index_root));
        let sent = self
            .http
            .post(format!("{}/api/v1/indexes", self.base))
            .header("content-type", "application/yaml")
            .body(config)
            .send()
            .await
            .map_err(anyhow::Error::from);
        let resp = merges::explained(self.events.as_deref(), sent).await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "creating index `{index_id}` returned {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        self.index = Some(index_id.to_owned());
        self.split_docs = None;
        Ok(())
    }

    async fn add(&mut self, doc: &Value) -> anyhow::Result<()> {
        self.add_line(&serde_json::to_vec(doc)?).await
    }

    async fn add_line(&mut self, line: &[u8]) -> anyhow::Result<()> {
        if line.len() + 1 > CHUNK_BYTES {
            let doc_id = serde_json::from_slice::<Value>(line)
                .ok()
                .and_then(|d| d["doc_id"].as_str().map(str::to_owned));
            bail!(
                "document `{}` is {} bytes, over the {CHUNK_BYTES}-byte ingest request limit",
                doc_id.as_deref().unwrap_or("?"),
                line.len() + 1
            );
        }
        // Send first if this document would push the request over the limit.
        if self.buf.len() + line.len() + 1 > CHUNK_BYTES {
            self.send("auto").await?;
        }
        self.buf.extend_from_slice(line);
        self.buf.push(b'\n');
        Ok(())
    }

    /// Commit, confirm the count, then wait for the index's merges and
    /// close it (`merges::seal`): nothing is published half merged.
    async fn finish(&mut self, expected: u64) -> anyhow::Result<()> {
        // A forced commit publishes everything still buffered in the node.
        // `add` always leaves its document in the buffer, so an empty one
        // means none was added (a batch with no text): there is nothing to
        // commit, and the node rejects an empty request (411).
        if !self.buf.is_empty() {
            self.send("force").await?;
        }
        let id = self.index.clone().context("no index created")?;
        let mut last = 0;
        for _ in 0..120 {
            last = self.count(&id).await?;
            if last >= expected {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if last != expected {
            bail!("index `{id}` holds {last} documents, expected {expected}");
        }
        let layout = merges::seal(
            &self.node(),
            &id,
            expected,
            self.split_docs
                .unwrap_or_else(merges::template_split_target),
            self.events.as_deref(),
            &self.merge_wait,
            self.stats.work_dir.as_deref(),
        )
        .await?;
        tracing::info!(
            index = %id,
            splits = layout.splits,
            "merged; the index is closed to further writes"
        );
        Ok(())
    }

    fn backend(&self) -> &'static str {
        "quickwit"
    }

    async fn engine(&mut self) -> Option<String> {
        let v: Value = self
            .http
            .get(format!("{}/api/v1/version", self.base))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()?;
        let version = v["build"]["version"].as_str()?;
        Some(match v["build"]["commit_short_hash"].as_str() {
            Some(c) if !c.is_empty() => format!("Quickwit {version} ({c})"),
            _ => format!("Quickwit {version}"),
        })
    }

    async fn layout(&mut self, index_ids: &[String]) -> anyhow::Result<Vec<IndexLayout>> {
        let node = self.node();
        let mut out = Vec::new();
        for id in index_ids {
            let splits = merges::explained(self.events.as_deref(), node.splits(id).await).await?;
            out.push(IndexLayout::of(id, &splits));
        }
        Ok(out)
    }

    fn stats(&self) -> Arc<SinkStats> {
        self.stats.clone()
    }
}

/// Lines of the writer's output kept for error messages.
const TAIL_LINES: usize = 50;

/// The last lines of the writer node's output, ANSI colors removed.
#[derive(Clone, Default)]
pub struct Tail(Arc<Mutex<VecDeque<String>>>);

impl Tail {
    fn push(&self, line: String) {
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if t.len() == TAIL_LINES {
            t.pop_front();
        }
        t.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        let t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        t.iter().cloned().collect()
    }
}

/// Remove ANSI escape sequences (Quickwit colors its log even when piped).
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte @–~.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: up to BEL or ESC \.
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some()) {
                        break;
                    }
                }
            }
            // Any other escape is two characters.
            _ => {}
        }
    }
    out
}

/// Quickwit's own line for an ingest request it answered 429 or 503:
/// `ERROR http_request{… url.path=/api/v1/<index>/ingest}:
/// tower_http::trace::on_failure: response failed classification=Status
/// code: 503 Service Unavailable …`. The pipeline logs each of those as a
/// `Quickwit pushed back; retrying` warning, with the status (or fails with
/// it), so forwarding Quickwit's copy only doubled the lines (#231). It
/// stays in the tail and is still observed.
fn is_pushback_echo(line: &str) -> bool {
    line.contains("tower_http::trace::on_failure")
        && line.contains("/ingest}")
        && (line.contains("Status code: 429") || line.contains("Status code: 503"))
}

/// The level a writer line is forwarded at: warnings and errors, and the
/// lines that say the node is up. `stderr` lines without a level (a panic,
/// the CLI's final `Error: …`) are warnings at least. Quickwit's echo of a
/// pushback isn't forwarded ([`is_pushback_echo`]).
fn forward_level(line: &str, stderr: bool) -> Option<tracing::Level> {
    if is_pushback_echo(line) {
        return None;
    }
    // `2026-09-29T00:20:17.017Z  WARN quickwit_config::…: peer seeds are empty`
    let level = line.split_whitespace().take(3).find_map(|w| match w {
        "ERROR" => Some(tracing::Level::ERROR),
        "WARN" => Some(tracing::Level::WARN),
        "INFO" => Some(tracing::Level::INFO),
        "DEBUG" => Some(tracing::Level::DEBUG),
        "TRACE" => Some(tracing::Level::TRACE),
        _ => None,
    });
    match level {
        Some(l) if l <= tracing::Level::WARN => Some(l),
        Some(tracing::Level::INFO)
            if line.contains("REST server is ready")
                || line.contains("starting REST server listening on")
                || line.contains("has transitioned to ready state") =>
        {
            Some(tracing::Level::INFO)
        }
        Some(_) => None,
        None if line.starts_with("Error") => Some(tracing::Level::ERROR),
        None if stderr && !line.trim().is_empty() => Some(tracing::Level::WARN),
        None => None,
    }
}

/// Read the writer's `stream` line by line into `tail` and `events`,
/// forwarding lines through `tracing` (target `quickwit`) at their level.
fn forward(
    stream: impl AsyncRead + Unpin + Send + 'static,
    stderr: bool,
    tail: Tail,
    events: Arc<NodeEvents>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(l)) => strip_ansi(&l),
                // Invalid UTF-8 ends `lines`; nothing more can be read anyway.
                Ok(None) | Err(_) => break,
            };
            match forward_level(&line, stderr) {
                Some(tracing::Level::ERROR) => tracing::error!(target: "quickwit", "{line}"),
                Some(tracing::Level::WARN) => tracing::warn!(target: "quickwit", "{line}"),
                Some(_) => tracing::info!(target: "quickwit", "{line}"),
                None => {}
            }
            events.observe(&line);
            tail.push(line);
        }
    })
}

/// The writer's `RUST_LOG`: the job's filter (Quickwit's default `info` when
/// there is none), with the merge pipelines and the indexing service always
/// at `info`. The release follows each merge pipeline through their lines
/// (`merges::NodeEvents`), so a quieter filter, or one naming only other
/// crates, must not hide them.
fn writer_log_filter(inherited: Option<&str>) -> String {
    let base = inherited
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .unwrap_or("info");
    format!(
        "{base},quickwit_indexing::actors::merge_pipeline=info,\
         quickwit_indexing::actors::indexing_service=info"
    )
}

/// A Quickwit indexer node run as a child process for the length of a
/// release: the one writer of the file-backed metastore. Its output goes to
/// the job's console: warnings, errors and readiness through `tracing`, and
/// the last lines into error messages.
pub struct QuickwitNode {
    pid: Option<u32>,
    pub url: String,
    work_dir: PathBuf,
    tail: Tail,
    events: Arc<NodeEvents>,
    /// Owns the process: waits for it to exit, then records why (`supervise`).
    supervisor: tokio::task::JoinHandle<()>,
    /// Why the process exited, once it has.
    exit: tokio::sync::watch::Receiver<Option<String>>,
}

impl Drop for QuickwitNode {
    /// A node that wasn't stopped (a release that failed) is killed: the
    /// supervisor owns the child, which is killed when it is dropped.
    fn drop(&mut self) {
        self.supervisor.abort();
    }
}

/// Why the writer exited, for the release's error: a SIGKILL nobody sent is
/// almost always the kernel's out-of-memory killer, which the container's
/// cgroup counts when it can be read.
pub fn describe_exit(
    status: std::process::ExitStatus,
    memory: Option<progress::CgroupMemory>,
) -> String {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let mut why = match (signal, status.code()) {
        (Some(9), _) => "the Quickwit writer was killed by signal 9 (SIGKILL)".to_owned(),
        (Some(sig), _) => format!("the Quickwit writer was killed by signal {sig}"),
        (None, Some(code)) => format!("the Quickwit writer exited with status {code}"),
        (None, None) => format!("the Quickwit writer exited ({status})"),
    };
    let limit = memory
        .and_then(|m| m.limit)
        .map(|b| {
            format!(
                "; the container's memory limit is {:.1} GiB",
                b as f64 / f64::from(1u32 << 30)
            )
        })
        .unwrap_or_default();
    match (signal, memory.and_then(|m| m.oom_kills)) {
        (Some(9), Some(n)) if n > 0 => why.push_str(&format!(
            ": it ran out of memory (the container's cgroup counts {n} out-of-memory kill{}{limit})",
            if n == 1 { "" } else { "s" }
        )),
        // The counter can't be read: say what is most likely. A counter
        // of 0 means someone else sent the signal.
        (Some(9), None) => why.push_str(&format!(
            ", most likely by the kernel's out-of-memory killer{}",
            if limit.is_empty() { String::new() } else { format!(" ({})", &limit[2..]) }
        )),
        _ => {}
    }
    why
}

/// Wait for the writer to exit, let the readers take in its last output,
/// then record why it exited in `events` and on the returned channel.
fn supervise(
    mut child: tokio::process::Child,
    mut readers: Vec<tokio::task::JoinHandle<()>>,
    tail: Tail,
    events: Arc<NodeEvents>,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::watch::Receiver<Option<String>>,
) {
    let (tx, rx) = tokio::sync::watch::channel(None);
    let task = tokio::spawn(async move {
        let status = child.wait().await;
        for r in readers.iter_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(2), r).await;
        }
        let mut why = match status {
            Ok(s) => describe_exit(s, progress::cgroup_memory()),
            Err(e) => format!("the Quickwit writer could not be waited for: {e}"),
        };
        let last: Vec<String> = tail.lines().into_iter().rev().take(5).rev().collect();
        if !last.is_empty() {
            why.push_str(&format!(". Its last output: {}", last.join(" | ")));
        }
        events.writer_exited(&why);
        let _ = tx.send(Some(why));
    });
    (task, rx)
}

impl QuickwitNode {
    /// `metastore` and `index_root` are `azure://qw-index` in Azure and
    /// `file:///…` locally. On Azure, Quickwit authenticates with the job's
    /// system-assigned identity (08 §8.2).
    pub async fn start(
        bin: &Path,
        work_dir: &Path,
        port: u16,
        metastore: &str,
        index_root: &str,
    ) -> anyhow::Result<Self> {
        let data = work_dir.join("qwdata");
        // Only this release writes here (it holds the writer lock), and a
        // previous run's data must not come back: on a persistent scratch
        // volume, its write-ahead log would replay into an index that a
        // published version may already list.
        if data.exists() {
            tracing::info!(dir = %data.display(), "removing a previous writer's data");
            std::fs::remove_dir_all(&data)
                .with_context(|| format!("removing {}", data.display()))?;
        }
        std::fs::create_dir_all(&data)?;
        let queue = WriterTuning::from_env()?.queue;
        let mut config = format!(
            "version: 0.8\ncluster_id: usnm-writer\nnode_id: writer\nlisten_address: 127.0.0.1\n\
             rest:\n  listen_port: {port}\ngrpc_listen_port: {}\ndata_dir: {}\n\
             metastore_uri: {metastore}\ndefault_index_root_uri: {index_root}\n\
             ingest_api:\n  shard_throughput_limit: {SHARD_THROUGHPUT_LIMIT}\n  \
             max_queue_memory_usage: {queue}\n  \
             max_queue_disk_usage: {WAL_DISK_BYTES}\n\
             indexer:\n  split_store_max_num_bytes: {SPLIT_STORE_BYTES}\n  \
             merge_concurrency: {MERGE_CONCURRENCY}\n",
            port.checked_add(1)
                .context("--quickwit-port must be below 65535")?,
            data.display()
        );
        if let Ok(account) = std::env::var("QW_AZURE_STORAGE_ACCOUNT") {
            config.push_str(&format!("storage:\n  azure:\n    account: {account}\n"));
        }
        let config_path = work_dir.join("writer.yaml");
        std::fs::write(&config_path, config)?;
        let mut cmd = tokio::process::Command::new(bin);
        // Quickwit's environment overrides its config file, and the official
        // image sets QW_LISTEN_ADDRESS=0.0.0.0, QW_DATA_DIR and QW_CONFIG: the
        // writer refused to start on it ("listen address `0.0.0.0` is
        // unspecified"). None is passed through: the storage account is in
        // the config above, and a QW_AZURE_STORAGE_ACCESS_KEY must never
        // reach it (ADR-0009, Entra identities only).
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("QW_") {
                cmd.env_remove(&key);
            }
        }
        let mut child = cmd
            .args(["run", "--config"])
            .arg(&config_path)
            .env("QW_DISABLE_TELEMETRY", "1")
            .env(
                "RUST_LOG",
                writer_log_filter(std::env::var("RUST_LOG").ok().as_deref()),
            )
            // AZURE_CLIENT_ID selects the pipeline's user-assigned identity;
            // Quickwit's credential chain uses the system-assigned one (08 §8.2).
            .env_remove("AZURE_CLIENT_ID")
            // The node's own telemetry setting isn't the pipeline's.
            .env_remove(telemetry::CONNECTION_STRING_VAR)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting {}", bin.display()))?;
        let tail = Tail::default();
        let events = Arc::new(NodeEvents::default());
        let mut readers = Vec::new();
        if let Some(out) = child.stdout.take() {
            readers.push(forward(out, false, tail.clone(), events.clone()));
        }
        if let Some(err) = child.stderr.take() {
            readers.push(forward(err, true, tail.clone(), events.clone()));
        }
        let pid = child.id();
        let (supervisor, mut exit) = supervise(child, readers, tail.clone(), events.clone());
        let node = Self {
            pid,
            url: format!("http://127.0.0.1:{port}"),
            work_dir: work_dir.to_owned(),
            tail,
            events,
            supervisor,
            exit: exit.clone(),
        };
        let http = reqwest::Client::new();
        for _ in 0..120 {
            // A node that has already exited won't become ready.
            if exit.borrow().is_some() {
                break;
            }
            if http
                .get(format!("{}/health/readyz", node.url))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return Ok(node);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // An exited node: the supervisor records its last output first.
        let _ = tokio::time::timeout(Duration::from_secs(5), exit.wait_for(Option::is_some)).await;
        let lines = node.tail.lines().join("\n");
        match node.exited() {
            Some(why) => bail!("Quickwit writer did not become ready: {why}; its output:\n{lines}"),
            None => bail!("Quickwit writer did not become ready; its last output:\n{lines}"),
        }
    }

    /// What the node's output has reported so far.
    pub fn events(&self) -> Arc<NodeEvents> {
        self.events.clone()
    }

    /// The node's process id.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Why the node exited, if it has.
    pub fn exited(&self) -> Option<String> {
        self.exit.borrow().clone()
    }

    /// Stop the node and wait for it to exit.
    pub async fn stop(mut self) -> anyhow::Result<()> {
        if let (Some(pid), None) = (self.pid, self.exited()) {
            // SIGTERM lets Quickwit shut down cleanly; `kill` is a shell builtin.
            let _ = tokio::process::Command::new("sh")
                .args(["-c", "kill -TERM \"$0\"", &pid.to_string()])
                .status()
                .await;
        }
        let stopped =
            tokio::time::timeout(Duration::from_secs(60), self.exit.wait_for(Option::is_some))
                .await;
        if stopped.is_err() {
            // Expected after a release: the closed index's ingest shards
            // have no pipeline left to drain them, and the ingester waits
            // for that. Everything is published by now, and the next
            // writer starts from an empty data directory.
            tracing::info!("the Quickwit writer did not stop within 60 s; killing it");
            // Dropping the supervisor's child kills it (`kill_on_drop`).
            self.supervisor.abort();
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), &mut self.supervisor).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn the_writer_always_logs_its_merge_pipelines() {
        let pipeline = "quickwit_indexing::actors::merge_pipeline=info,\
                        quickwit_indexing::actors::indexing_service=info";
        assert_eq!(writer_log_filter(None), format!("info,{pipeline}"));
        assert_eq!(writer_log_filter(Some(" ")), format!("info,{pipeline}"));
        assert_eq!(writer_log_filter(Some("warn")), format!("warn,{pipeline}"));
        assert_eq!(
            writer_log_filter(Some("usnm_ingest=info")),
            format!("usnm_ingest=info,{pipeline}")
        );
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let t = Tail::default();
        for i in 0..TAIL_LINES + 3 {
            t.push(i.to_string());
        }
        let lines = t.lines();
        assert_eq!(lines.len(), TAIL_LINES);
        assert_eq!(lines[0], "3");
        assert_eq!(lines[TAIL_LINES - 1], (TAIL_LINES + 2).to_string());
    }

    #[test]
    fn strips_ansi_colors() {
        // As Quickwit 0.9 writes it to a pipe.
        let raw = "\u{1b}[2m2026-09-29T00:20:17.017Z\u{1b}[0m \u{1b}[33m WARN\u{1b}[0m \
                   \u{1b}[2mquickwit_config\u{1b}[0m\u{1b}[2m:\u{1b}[0m peer seeds are empty \
                   \u{1b}[3mkey\u{1b}[0m\u{1b}[2m=\u{1b}[0mvalue";
        assert_eq!(
            strip_ansi(raw),
            "2026-09-29T00:20:17.017Z  WARN quickwit_config: peer seeds are empty key=value"
        );
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}plain"), "plain");
        assert_eq!(strip_ansi("no escapes"), "no escapes");
        // A trailing lone escape doesn't panic.
        assert_eq!(strip_ansi("x\u{1b}"), "x");
    }

    #[test]
    fn forwards_warnings_errors_and_readiness() {
        use tracing::Level;
        let line = |level: &str, msg: &str| {
            format!("2026-09-29T00:20:17.017Z {level} quickwit_serve: {msg}")
        };
        assert_eq!(
            forward_level(&line(" WARN", "no shards available"), false),
            Some(Level::WARN)
        );
        assert_eq!(
            forward_level(&line("ERROR", "data dir volume too small"), false),
            Some(Level::ERROR)
        );
        assert_eq!(
            forward_level(&line(" INFO", "REST server is ready"), false),
            Some(Level::INFO)
        );
        assert_eq!(
            forward_level(
                &line(" INFO", "starting REST server listening on 127.0.0.1:7380"),
                false
            ),
            Some(Level::INFO)
        );
        assert_eq!(
            forward_level(&line(" INFO", "starting janitor service"), false),
            None
        );
        assert_eq!(forward_level(&line("DEBUG", "x"), true), None);
        assert_eq!(
            forward_level("Error: failed to bind", false),
            Some(Level::ERROR)
        );
        assert_eq!(
            forward_level("thread 'main' panicked at src/main.rs", true),
            Some(Level::WARN)
        );
        assert_eq!(forward_level("some unlevelled stdout line", false), None);
        assert_eq!(forward_level("", true), None);
    }

    #[test]
    fn pushback_echoes_are_not_forwarded() {
        use tracing::Level;
        // As the writer logged them in prod (October 2026).
        let echo = |path: &str, status: &str| {
            format!(
                "2026-10-08T10:00:00.123Z ERROR http_request{{otel.kind=\"Server\" \
                 http.request.method=POST url.path={path}}}: tower_http::trace::on_failure: \
                 response failed classification=Status code: {status} latency=12 ms"
            )
        };
        let ingest = "/api/v1/pages-base-20261008-1/ingest";
        assert_eq!(
            forward_level(&echo(ingest, "503 Service Unavailable"), false),
            None
        );
        assert_eq!(
            forward_level(&echo(ingest, "429 Too Many Requests"), false),
            None
        );
        // Other failures are still forwarded: another status, another path.
        assert_eq!(
            forward_level(&echo(ingest, "500 Internal Server Error"), false),
            Some(Level::ERROR)
        );
        assert_eq!(
            forward_level(&echo("/health/readyz", "503 Service Unavailable"), false),
            Some(Level::ERROR)
        );
    }

    /// A writer that exits at startup: the error carries its last output.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_writer_that_exits_explains_why() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("quickwit");
        std::fs::write(
            &bin,
            "#!/bin/sh\n\
             printf '\\033[2m2026-09-29T00:20:17Z\\033[0m \\033[33m WARN\\033[0m quickwit_config: peer seeds are empty\\n'\n\
             echo 'Error: data dir volume too small' >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = QuickwitNode::start(&bin, dir.path(), 7399, "file:///m", "file:///i")
            .await
            .err()
            .expect("the writer exited")
            .to_string();
        assert!(err.contains("did not become ready"), "{err}");
        assert!(err.contains("exited with status 1"), "{err}");
        assert!(
            err.contains("2026-09-29T00:20:17Z  WARN quickwit_config: peer seeds are empty"),
            "{err}"
        );
        assert!(err.contains("Error: data dir volume too small"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn explains_how_the_writer_exited() {
        use std::os::unix::process::ExitStatusExt;
        let killed = std::process::ExitStatus::from_raw(9);
        let oom = progress::CgroupMemory {
            limit: Some(7680 << 20),
            oom_kills: Some(1),
        };
        assert_eq!(
            describe_exit(killed, Some(oom)),
            "the Quickwit writer was killed by signal 9 (SIGKILL): it ran out of memory (the \
             container's cgroup counts 1 out-of-memory kill; the container's memory limit is 7.5 GiB)"
        );
        let unknown = progress::CgroupMemory {
            limit: Some(7680 << 20),
            oom_kills: None,
        };
        assert_eq!(
            describe_exit(killed, Some(unknown)),
            "the Quickwit writer was killed by signal 9 (SIGKILL), most likely by the kernel's \
             out-of-memory killer (the container's memory limit is 7.5 GiB)"
        );
        assert_eq!(
            describe_exit(killed, None),
            "the Quickwit writer was killed by signal 9 (SIGKILL), most likely by the kernel's \
             out-of-memory killer"
        );
        // No out-of-memory kill counted: someone else sent the signal.
        let none = progress::CgroupMemory {
            limit: Some(7680 << 20),
            oom_kills: Some(0),
        };
        assert_eq!(
            describe_exit(killed, Some(none)),
            "the Quickwit writer was killed by signal 9 (SIGKILL)"
        );
        assert_eq!(
            describe_exit(std::process::ExitStatus::from_raw(15), Some(oom)),
            "the Quickwit writer was killed by signal 15"
        );
        assert_eq!(
            describe_exit(std::process::ExitStatus::from_raw(1 << 8), None),
            "the Quickwit writer exited with status 1"
        );
    }

    /// A writer the kernel kills mid-release: the exit is recorded with
    /// its signal and last output, for the release's error.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_killed_writer_is_reported() {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "echo 'INFO merging splits'; kill -KILL $$"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tail = Tail::default();
        let events = Arc::new(NodeEvents::default());
        let readers = vec![
            forward(
                child.stdout.take().unwrap(),
                false,
                tail.clone(),
                events.clone(),
            ),
            forward(
                child.stderr.take().unwrap(),
                true,
                tail.clone(),
                events.clone(),
            ),
        ];
        let (task, mut exit) = supervise(child, readers, tail, events.clone());
        tokio::time::timeout(Duration::from_secs(10), exit.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
        let why = events.exit().unwrap();
        assert!(
            why.starts_with("the Quickwit writer was killed by signal 9 (SIGKILL)"),
            "{why}"
        );
        assert!(
            why.ends_with("Its last output: INFO merging splits"),
            "{why}"
        );
        let err = events.check().unwrap_err().to_string();
        assert!(err.contains("not publishing"), "{err}");
    }

    #[tokio::test]
    async fn requests_stay_under_the_ingest_limit() {
        let mut s = QuickwitSink::new("http://127.0.0.1:9", "file:///tmp/x").unwrap();
        s.index = Some("i".into());
        // Documents that fit are buffered without sending.
        let doc = serde_json::json!({"doc_id": "a", "text": "x".repeat(1000)});
        s.add(&doc).await.unwrap();
        assert!(s.buf.len() < CHUNK_BYTES);
        // One document over the limit is refused, not sent.
        let big = serde_json::json!({"doc_id": "big", "text": "x".repeat(CHUNK_BYTES)});
        let err = s.add(&big).await.unwrap_err().to_string();
        assert!(err.contains("`big`"), "{err}");
    }

    #[tokio::test]
    async fn engine_is_the_nodes_version() {
        let app = axum::Router::new().route(
            "/api/v1/version",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "build": {"version": "0.9.1", "commit_short_hash": "abc1234"}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut s = QuickwitSink::new(&format!("http://{addr}"), "file:///tmp/x").unwrap();
        assert_eq!(
            s.engine().await.as_deref(),
            Some("Quickwit 0.9.1 (abc1234)")
        );
        // Nothing listening: unknown, not an error.
        let mut gone = QuickwitSink::new("http://127.0.0.1:9", "file:///tmp/x").unwrap();
        assert_eq!(gone.engine().await, None);
    }

    #[test]
    fn writer_tuning_defaults_and_overrides() {
        let none = |_: &str| None;
        assert_eq!(
            WriterTuning::from_lookup(none).unwrap(),
            WriterTuning::default()
        );
        // The defaults leave the template as it is.
        assert_eq!(
            WriterTuning::default().apply(INDEX_TEMPLATE).unwrap(),
            INDEX_TEMPLATE
        );
        let e4 = |k: &str| match k {
            "USNM_WRITER_HEAP" => Some("6GiB".to_string()),
            "USNM_WRITER_COMMIT_SECS" => Some("600".to_string()),
            "USNM_WRITER_QUEUE" => Some("4GiB".to_string()),
            _ => None,
        };
        let t = WriterTuning::from_lookup(e4).unwrap();
        assert_eq!(
            (t.heap.as_str(), t.commit_timeout_secs, t.queue.as_str()),
            ("6GiB", 600, "4GiB")
        );
        let yaml = t.apply(INDEX_TEMPLATE).unwrap();
        assert!(yaml.contains("heap_size: 6GiB") && yaml.contains("commit_timeout_secs: 600"));
        assert!(!yaml.contains("heap_size: 1GiB") && !yaml.contains("commit_timeout_secs: 30"));
        // Everything else is the template's.
        assert_eq!(yaml.lines().count(), INDEX_TEMPLATE.lines().count());
    }

    #[test]
    fn decade_layouts_add_the_field_and_its_tag_or_partitions() {
        assert_eq!(Decades::Off.apply(INDEX_TEMPLATE).unwrap(), INDEX_TEMPLATE);
        let tagged = Decades::Tagged.apply(INDEX_TEMPLATE).unwrap();
        let partitioned = Decades::Partitioned.apply(INDEX_TEMPLATE).unwrap();
        for yaml in [&tagged, &partitioned] {
            assert!(yaml.contains("\n  tag_fields: [decade]\n"), "{yaml}");
            assert!(yaml.contains(
                "\n    - { name: decade, type: u64, fast: false, indexed: true, stored: false }\n"
            ));
            // Only the doc mapping changes: the indexing settings, which the
            // merge checks read, are the template's.
            let settings = |t: &str| t[t.find("indexing_settings:").unwrap()..].to_owned();
            assert_eq!(settings(yaml), settings(INDEX_TEMPLATE));
            // The template's lines, in order, with the new ones between them.
            let mut lines = yaml.lines();
            assert!(INDEX_TEMPLATE.lines().all(|l| lines.any(|y| y == l)));
        }
        assert!(!tagged.contains("partition_key"));
        assert!(partitioned.contains("\n  partition_key: decade\n  max_num_partitions: 32\n"));
        // With the writer's tuning too.
        let t = WriterTuning {
            heap: "6GiB".into(),
            commit_timeout_secs: 120,
            queue: "4GiB".into(),
        };
        let yaml = main_template(&t, Decades::Partitioned.into()).unwrap();
        assert!(yaml.contains("heap_size: 6GiB") && yaml.contains("partition_key: decade"));
        // A template without the lines it adds to is refused.
        assert!(Decades::Tagged.apply("version: 0.9").is_err());
        let twice = format!("{INDEX_TEMPLATE}\n  timestamp_field: date\n");
        assert!(Decades::Partitioned.apply(&twice).is_err());
    }

    #[test]
    fn one_text_field_keeps_the_texts_stored_and_searches_text_all() {
        assert_eq!(
            text_fields(TextLayout::Separate, INDEX_TEMPLATE).unwrap(),
            INDEX_TEMPLATE
        );
        let yaml = text_fields(TextLayout::Single, INDEX_TEMPLATE).unwrap();
        let fields: Vec<&str> = yaml
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("- { name: text"))
            .collect();
        assert_eq!(
            fields,
            [
                "- { name: text, type: text, indexed: false, stored: true }",
                "- { name: text_as, type: text, indexed: false, stored: true }",
                "- { name: text_all, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
                "- { name: text_all_cg, type: text, tokenizer: whitespace, record: position, stored: false, fieldnorms: false }",
            ]
        );
        assert!(
            yaml.contains("\n  default_search_fields: [text_all]"),
            "{yaml}"
        );
        assert!(!yaml.contains("default_search_fields: [text]"));
        // The comments about the old fields went with them.
        assert!(!yaml.contains("Searched only when"), "{yaml}");
        assert!(!yaml.contains("Phrases holding a common"), "{yaml}");
        // Everything but the text fields is the template's.
        let settings = |t: &str| t[t.find("    - { name: date").unwrap()..].to_owned();
        assert_eq!(
            settings(&yaml).replace("[text_all]", "[text]"),
            settings(INDEX_TEMPLATE)
        );
        assert!(
            yaml.starts_with(&INDEX_TEMPLATE[..INDEX_TEMPLATE.find("field_mappings:").unwrap()])
        );
        // Strict, as every main index: a document with a field of the other
        // layout (`text_cg` here, `text_all` in the template) is refused.
        assert!(yaml.contains("\n  mode: strict\n"));
        assert!(!INDEX_TEMPLATE.contains("name: text_all"));
        // With the writer's tuning, a decade layout and a split target too.
        let t = WriterTuning {
            heap: "6GiB".into(),
            commit_timeout_secs: 120,
            queue: "4GiB".into(),
        };
        let layout = MainLayout {
            decades: Decades::Partitioned,
            text: TextLayout::Single,
            split_docs: Some(120_000),
        };
        let yaml = main_template(&t, layout).unwrap();
        assert!(yaml.contains("\n  split_num_docs_target: 120000\n"));
        assert!(yaml.contains("heap_size: 6GiB") && yaml.contains("partition_key: decade"));
        assert!(yaml.contains("name: text_all,") && yaml.contains("name: decade,"));
        // A template without one of the lines it changes is refused.
        let no_pairs = INDEX_TEMPLATE.replace(TEMPLATE_TEXT_FIELDS[1], "");
        assert!(text_fields(TextLayout::Single, &no_pairs).is_err());
        assert!(text_fields(TextLayout::Single, "version: 0.9").is_err());
    }

    #[test]
    fn a_split_target_replaces_the_templates() {
        let default = MainLayout::default();
        assert_eq!(default.split_target(), 30_000);
        assert_eq!(
            main_template(&WriterTuning::default(), default).unwrap(),
            INDEX_TEMPLATE
        );
        let layout = MainLayout {
            split_docs: Some(90_000),
            ..MainLayout::default()
        };
        assert_eq!(layout.split_target(), 90_000);
        let yaml = main_template(&WriterTuning::default(), layout).unwrap();
        assert_eq!(
            yaml,
            INDEX_TEMPLATE.replace(
                "  split_num_docs_target: 30000\n",
                "  split_num_docs_target: 90000\n"
            )
        );
        assert!(split_target(0, INDEX_TEMPLATE).is_err());
        assert!(split_target(MAX_SPLIT_DOCS, INDEX_TEMPLATE).is_err());
        // A template without the line, or with it twice, is refused.
        assert!(split_target(90_000, "version: 0.9").is_err());
        let twice = format!("{INDEX_TEMPLATE}  {TEMPLATE_SPLIT_TARGET}\n");
        assert!(split_target(90_000, &twice).is_err());
    }

    #[test]
    fn writer_tuning_refuses_bad_values_and_untunable_templates() {
        for (k, v) in [
            ("USNM_WRITER_HEAP", "6"),
            ("USNM_WRITER_HEAP", "6GB"),
            ("USNM_WRITER_HEAP", "0GiB"),
            ("USNM_WRITER_HEAP", "00GiB"),
            ("USNM_WRITER_HEAP", "6GiBGiB"),
            ("USNM_WRITER_HEAP", "6GiBMiB"),
            ("USNM_WRITER_HEAP", "GiB"),
            ("USNM_WRITER_HEAP", "-6GiB"),
            ("USNM_WRITER_QUEUE", "lots"),
            ("USNM_WRITER_COMMIT_SECS", "0"),
            ("USNM_WRITER_COMMIT_SECS", "7200"),
            ("USNM_WRITER_COMMIT_SECS", "ten"),
        ] {
            let got = WriterTuning::from_lookup(|key| (key == k).then(|| v.to_string()));
            assert!(got.is_err(), "{k}={v}");
        }
        assert!(WriterTuning::default().apply("version: 0.9").is_err());
        // A commented-out setting doesn't count, and neither do two.
        let commented = INDEX_TEMPLATE.replace("heap_size: 1GiB", "# heap_size: 1GiB");
        assert!(WriterTuning::default().apply(&commented).is_err());
        let twice = format!("{INDEX_TEMPLATE}\n  commit_timeout_secs: 30\n");
        assert!(WriterTuning::default().apply(&twice).is_err());
    }

    /// A fake ingest endpoint that answers with `statuses` in turn (then 200)
    /// and records every body it receives.
    async fn fake_node(statuses: Vec<u16>) -> (QuickwitSink, Arc<Mutex<Vec<Vec<u8>>>>) {
        use axum::http::StatusCode;
        let seen: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let queue = Arc::new(Mutex::new(std::collections::VecDeque::from(statuses)));
        let app = axum::Router::new().route(
            "/api/v1/{id}/ingest",
            axum::routing::post({
                let seen = seen.clone();
                move |body: axum::body::Bytes| async move {
                    seen.lock().unwrap().push(body.to_vec());
                    match queue.lock().unwrap().pop_front() {
                        Some(code) => (
                            StatusCode::from_u16(code).unwrap(),
                            r#"{"message":"ingest service is unavailable (no shards available)"}"#,
                        ),
                        None => (StatusCode::OK, r#"{"num_rejected_docs":0}"#),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut s = QuickwitSink::new(&format!("http://{addr}"), "file:///tmp/x").unwrap();
        s.index = Some("i".into());
        s.retry_initial = Duration::from_millis(1);
        s.retry_for = Duration::from_secs(5);
        (s, seen)
    }

    #[tokio::test]
    async fn ingest_retries_while_the_node_pushes_back() {
        let (mut s, seen) = fake_node(vec![503, 429, 503]).await;
        s.buf = b"{\"doc_id\":\"a\"}\n".to_vec();
        s.send("auto").await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 4);
        assert!(seen.iter().all(|b| b == b"{\"doc_id\":\"a\"}\n"));
        // Progress counts what was accepted, and the retries by status.
        let stats = s.stats();
        assert_eq!((stats.docs_sent(), stats.bytes_sent()), (1, 15));
        assert_eq!((stats.retries_429(), stats.retries_503()), (1, 2));
    }

    #[tokio::test]
    async fn ingest_gives_up_after_the_retry_window() {
        let (mut s, _) = fake_node(vec![503; 1000]).await;
        s.retry_for = Duration::from_millis(50);
        s.buf = b"{}\n".to_vec();
        let err = s.send("auto").await.unwrap_err().to_string();
        assert!(err.contains("503"), "{err}");
    }

    #[tokio::test]
    async fn other_ingest_errors_are_not_retried() {
        let (mut s, seen) = fake_node(vec![400]).await;
        s.buf = b"{}\n".to_vec();
        let err = s.send("auto").await.unwrap_err().to_string();
        assert!(err.contains("400"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
}
