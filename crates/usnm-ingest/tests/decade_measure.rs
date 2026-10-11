//! A measurement, not a check (#123, 05 §5.5.5): what laying a base out by
//! decade costs the Quickwit writer. It builds a synthetic corpus in archive
//! order (batches of titles, each title's pages by date, decades weighted
//! like production's) and indexes it three times into a fresh writer node:
//! without decades, partitioned by decade in archive order, and partitioned
//! with the pages sent a decade at a time (`decade_order`, as a release
//! does). For each it reports the writer's peak memory, the splits the
//! indexer cut (before merging), the merges, the splits left, and the time.
//!
//! ```sh
//! QUICKWIT_BIN=/path/to/quickwit cargo test --release -p usnm-ingest \
//!   --test decade_measure -- --ignored --nocapture
//! ```
//!
//! `USNM_MEASURE_DOCS` sets the corpus size (default 150,000 pages),
//! `USNM_MEASURE_WORDS` the mean words a page (default 1,000),
//! `USNM_MEASURE_RUNS` the runs (default `off,archive,decade`);
//! `USNM_WRITER_HEAP` and `USNM_WRITER_COMMIT_SECS` tune the writer as in a
//! release.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use usnm_ingest::decade_order::DecadeOrder;
use usnm_ingest::merges::MergeWait;
use usnm_ingest::sink::{Decades, IndexSink, QuickwitNode, QuickwitSink};

/// Pages per decade in production (`baseline_pages`, October 2026, in
/// thousands), the decades before 1820 in the 1820s.
const WEIGHTS: [(u16, u32); 15] = [
    (1820, 193),
    (1830, 113),
    (1840, 252),
    (1850, 499),
    (1860, 734),
    (1870, 1013),
    (1880, 1526),
    (1890, 2909),
    (1900, 4531),
    (1910, 5590),
    (1920, 2832),
    (1930, 1241),
    (1940, 1146),
    (1950, 814),
    (1960, 329),
];

/// xorshift64*: deterministic, so every run indexes the same corpus.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// A vocabulary with Zipf frequencies: the common words first, then made-up
/// words, like OCR text's long tail.
struct Words {
    words: Vec<String>,
    cumulative: Vec<f64>,
}

impl Words {
    fn new(rng: &mut Rng) -> Self {
        let mut words: Vec<String> = usnm_core::common_grams::WORDS
            .iter()
            .map(|w| (*w).to_owned())
            .collect();
        const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
        while words.len() < 60_000 {
            let len = 3 + rng.below(8) as usize;
            words.push(
                (0..len)
                    .map(|_| LETTERS[rng.below(26) as usize] as char)
                    .collect(),
            );
        }
        let mut total = 0.0;
        let cumulative = (1..=words.len())
            .map(|rank| {
                total += 1.0 / rank as f64;
                total
            })
            .collect();
        Self { words, cumulative }
    }

    fn pick(&self, rng: &mut Rng) -> &str {
        let last = *self.cumulative.last().unwrap();
        let x = (rng.next() >> 11) as f64 / (1u64 << 53) as f64 * last;
        let i = self.cumulative.partition_point(|c| *c < x);
        &self.words[i.min(self.words.len() - 1)]
    }
}

/// The corpus in archive order: batches of 2 to 6 titles, each title's run
/// 3 to 15 years from a decade drawn by production's weights, its pages by
/// date. `f` gets each page's document.
fn corpus(docs: u64, with_decade: bool, mut f: impl FnMut(Value)) {
    // Words a page, about 6 bytes each: 700 to 1,300 by default.
    let words_per_page: u64 = std::env::var("USNM_MEASURE_WORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let words = Words::new(&mut rng);
    let total: u32 = WEIGHTS.iter().map(|(_, w)| w).sum();
    let mut made = 0u64;
    let mut title = 0u32;
    let mut batch = 0u32;
    while made < docs {
        batch += 1;
        for _ in 0..2 + rng.below(5) {
            title += 1;
            let mut pick = rng.below(u64::from(total)) as u32;
            let decade = WEIGHTS
                .iter()
                .find(|(_, w)| {
                    let hit = pick < *w;
                    pick = pick.saturating_sub(*w);
                    hit
                })
                .map_or(1900, |(d, _)| *d);
            let start = i32::from(decade) + rng.below(10) as i32;
            let years = 3 + rng.below(13) as i32;
            // About 2,000 to 8,000 pages a title in the batch.
            let pages = 2_000 + rng.below(6_000);
            let days = (years * 365) as u64;
            for n in 0..pages {
                if made >= docs {
                    return;
                }
                let date = chrono::NaiveDate::from_ymd_opt(start, 1, 1).unwrap()
                    + chrono::Duration::days((n * days / pages) as i64);
                let text: Vec<&str> = (0..words_per_page * 7 / 10
                    + rng.below(words_per_page * 6 / 10))
                    .map(|_| words.pick(&mut rng))
                    .collect();
                let text = text.join(" ");
                let lccn = format!("sn{:08}", title);
                let mut doc = json!({
                    "doc_id": format!("{lccn}_{date}_ed-1_seq-{}", n % 8 + 1),
                    "day": usnm_core::time::day_number(date),
                    "ym": usnm_core::time::ym_number(date),
                    "year": chrono::Datelike::year(&date),
                    "place_id": format!("P{:05}", title % 2000),
                    "place_shard": title % 8,
                    "lccn": lccn,
                    "state": "IL",
                    "language": ["eng"],
                    "front_page": n % 8 == 0,
                    "edition": 1,
                    "seq": n % 8 + 1,
                    "sort_key": (u64::from(title) << 32) | (n % 8 + 1),
                    "date": date.to_string(),
                    "batch": format!("batch_measure_{batch:04}_ver01"),
                    "text_cg": usnm_core::common_grams::index_text(&text, usnm_core::text::Analyzer::LATEST),
                    "text": text,
                });
                if with_decade {
                    doc["decade"] = usnm_core::decade::of_date(date).into();
                }
                f(doc);
                made += 1;
            }
        }
    }
}

#[derive(Debug, Default)]
struct Seen {
    peak_rss: u64,
    /// Every split seen, by id: (docs, merge ops, decades).
    splits: BTreeMap<String, (u64, u64, Vec<String>)>,
}

/// Every second, the splits the metastore lists (merged-away ones too);
/// every quarter second, the writer's memory.
fn watch(
    url: String,
    index: String,
    pid: u32,
    seen: Arc<Mutex<Seen>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        let mut tick = 0u64;
        loop {
            if let Ok(out) = tokio::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
                .await
            {
                if let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                    let mut s = seen.lock().unwrap();
                    s.peak_rss = s.peak_rss.max(kb * 1024);
                }
            }
            if tick.is_multiple_of(4) {
                for state in ["Published", "MarkedForDeletion"] {
                    let Ok(resp) = http
                        .get(format!("{url}/api/v1/indexes/{index}/splits"))
                        .query(&[("split_states", state), ("limit", "10000")])
                        .send()
                        .await
                    else {
                        continue;
                    };
                    let Ok(v) = resp.json::<Value>().await else {
                        continue;
                    };
                    let mut s = seen.lock().unwrap();
                    for split in v["splits"].as_array().into_iter().flatten() {
                        let tags: Vec<String> = split["tags"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|t| t.as_str()?.strip_prefix("decade:").map(str::to_owned))
                            .collect();
                        s.splits.insert(
                            split["split_id"].as_str().unwrap_or_default().to_owned(),
                            (
                                split["num_docs"].as_u64().unwrap_or(0),
                                split["num_merge_ops"].as_u64().unwrap_or(0),
                                tags,
                            ),
                        );
                    }
                }
            }
            tick += 1;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Run {
    Off,
    PartitionedArchiveOrder,
    PartitionedByDecade,
}

async fn measure(bin: &Path, run: Run, docs: u64, port: u16) {
    let dir = tempfile::tempdir().unwrap();
    let qw = dir.path().join("qw");
    std::fs::create_dir_all(&qw).unwrap();
    let meta = format!("file://{}/meta", qw.display());
    let root = format!("file://{}/indexes", qw.display());
    let node = QuickwitNode::start(bin, &qw, port, &meta, &root)
        .await
        .unwrap();
    let id = "pages-base-measure";
    let decades = match run {
        Run::Off => Decades::Off,
        _ => Decades::Partitioned,
    };
    let mut sink = QuickwitSink::new(&node.url, &root)
        .unwrap()
        .watching(&node)
        .merges(MergeWait {
            poll: Duration::from_secs(1),
            stable_polls: 5,
            timeout: Duration::from_secs(3600),
            finalize_grace: Duration::from_secs(10),
            finalize_stall: Duration::from_secs(120),
            report: Default::default(),
        });
    sink.create(id, decades.into()).await.unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let watcher = watch(
        node.url.clone(),
        id.to_owned(),
        node.pid().unwrap(),
        seen.clone(),
    );
    let started = Instant::now();
    let mut order = (run == Run::PartitionedByDecade).then(|| {
        let chunk = usnm_ingest::merges::template_setting("split_num_docs_target").unwrap();
        DecadeOrder::new(dir.path(), chunk).unwrap()
    });
    // Generated on a thread of its own, a few thousand pages ahead.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Value>(2_000);
    let with_decade = decades.on();
    let generator = std::thread::spawn(move || {
        corpus(docs, with_decade, |d| {
            let _ = tx.blocking_send(d);
        })
    });
    let mut sent = 0u64;
    while let Some(d) = rx.recv().await {
        match order.as_mut() {
            Some(o) => o.add(&mut sink, &d).await.unwrap(),
            None => sink.add(&d).await.unwrap(),
        }
        sent += 1;
    }
    generator.join().unwrap();
    let order_stats = match order.as_mut() {
        Some(o) => Some(o.finish(&mut sink).await.unwrap()),
        None => None,
    };
    let send_secs = started.elapsed().as_secs_f64();
    let sealing = Instant::now();
    sink.finish(sent).await.unwrap();
    let seal_secs = sealing.elapsed().as_secs_f64();
    // One more look at the splits.
    tokio::time::sleep(Duration::from_secs(2)).await;
    watcher.abort();
    let layout = sink.layout(&[id.to_owned()]).await.unwrap().remove(0);
    let s = std::mem::take(&mut *seen.lock().unwrap());
    let first: Vec<u64> = s
        .splits
        .values()
        .filter(|(_, ops, _)| *ops == 0)
        .map(|(d, _, _)| *d)
        .collect();
    let merged: Vec<u64> = s
        .splits
        .values()
        .filter(|(_, ops, _)| *ops > 0)
        .map(|(d, _, _)| *d)
        .collect();
    let mut sorted = first.clone();
    sorted.sort_unstable();
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let mixed = s
        .splits
        .values()
        .filter(|(_, ops, t)| *ops == 0 && t.len() > 1)
        .count();
    eprintln!(
        "{run:?}: {sent} pages; writer peak RSS {:.0} MiB; first-round splits {} (median {median} pages, smallest {}, largest {}, {mixed} with several decades); \
         merges {} ({} pages merged); final splits {} (largest {} pages, by decade {:?}); send {send_secs:.0} s, merge wait {seal_secs:.0} s; order {order_stats:?}",
        s.peak_rss as f64 / 1048576.0,
        first.len(),
        sorted.first().unwrap_or(&0),
        sorted.last().unwrap_or(&0),
        merged.len(),
        merged.iter().sum::<u64>(),
        layout.splits,
        layout.largest_split_docs,
        layout.splits_by_decade,
    );
    node.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: run with --ignored and QUICKWIT_BIN"]
async fn measure_decade_layouts() {
    let Some(bin) = std::env::var_os("QUICKWIT_BIN").filter(|b| !b.is_empty()) else {
        eprintln!("QUICKWIT_BIN not set; skipping");
        return;
    };
    let docs = std::env::var("USNM_MEASURE_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150_000);
    let bin = Path::new(&bin);
    let runs = std::env::var("USNM_MEASURE_RUNS").unwrap_or_else(|_| "off,archive,decade".into());
    for (i, run) in runs.split(',').enumerate() {
        let run = match run {
            "off" => Run::Off,
            "archive" => Run::PartitionedArchiveOrder,
            "decade" => Run::PartitionedByDecade,
            other => panic!("unknown run {other}"),
        };
        measure(bin, run, docs, 7420 + 2 * i as u16).await;
    }
}
