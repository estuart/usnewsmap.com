//! Sending a partitioned base's pages a decade at a time (05 §5.5.5, #123).
//!
//! A base built with `--partition-decade` keeps each decade in its own
//! splits. Quickwit 0.9.1 cuts one split per decade from each commit, and a
//! commit takes whatever pages came in since the last one. Curated parts are
//! in archive order (title, then date), so a commit's pages span several
//! decades, and each commit would cut several small splits that all need
//! merging. Measured locally (05 §5.5.5), that is 5× the splits cut, 1.8×
//! the pages merged and 1.8× the writer's memory of a base without decades.
//!
//! So the release sends the pages of one decade together: each page goes to
//! its decade's file on the writer's scratch disk (zstd JSON lines), and a
//! decade's file is sent as soon as it holds `chunk` pages, the split target.
//! Each commit then holds one or two decades: locally, one merge pass over
//! the pages, as without decades, and 1.4× the writer's memory. At the end,
//! the files left are sent, oldest decade first. The disk this needs is
//! bounded: under `chunk` pages per decade, at most 15 × 30,000 pages (a
//! few GB compressed), never the whole corpus. Each page is read from the
//! curated store once, as without partitions.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::Value;

use crate::sink::IndexSink;

/// The directory under the writer's work directory that holds the files.
pub const SPILL_DIR: &str = "decade-spill";

/// zstd's level for the files: fast, and about a quarter of the JSON.
const LEVEL: i32 = 1;

/// One decade's pages not sent yet.
struct Spill {
    path: PathBuf,
    file: zstd::stream::write::Encoder<'static, BufWriter<File>>,
    docs: u64,
}

/// What [`DecadeOrder`] has done, for the release log and the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrderStats {
    /// Runs of one decade's pages sent together.
    pub runs: u64,
    pub docs: u64,
    /// The most pages held on disk at once.
    pub peak_docs: u64,
    /// The most compressed bytes on disk at once (as of the last flush of
    /// each file, so a little under).
    pub peak_bytes: u64,
}

/// Holds pages back by decade and sends them a decade at a time.
pub struct DecadeOrder {
    dir: PathBuf,
    chunk: u64,
    open: BTreeMap<u16, Spill>,
    held: u64,
    stats: OrderStats,
}

impl DecadeOrder {
    /// Files in `work_dir/decade-spill`, emptied first: a previous release
    /// that stopped may have left some. Only the release holding the writer
    /// lock writes there.
    pub fn new(work_dir: &Path, chunk: u64) -> anyhow::Result<Self> {
        let dir = work_dir.join(SPILL_DIR);
        if dir.exists() {
            tracing::info!(dir = %dir.display(), "removing a previous release's decade files");
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
        }
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir,
            chunk: chunk.max(1),
            open: BTreeMap::new(),
            held: 0,
            stats: OrderStats::default(),
        })
    }

    pub fn stats(&self) -> OrderStats {
        self.stats
    }

    /// Hold `doc` (with its `decade`) back, and send its decade's pages
    /// once there are `chunk` of them.
    pub async fn add(&mut self, sink: &mut dyn IndexSink, doc: &Value) -> anyhow::Result<()> {
        let decade = doc["decade"]
            .as_u64()
            .and_then(|d| u16::try_from(d).ok())
            .with_context(|| {
                format!(
                    "document `{}` has no decade",
                    doc["doc_id"].as_str().unwrap_or("?")
                )
            })?;
        let line = serde_json::to_vec(doc)?;
        let spill = match self.open.entry(decade) {
            std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::btree_map::Entry::Vacant(e) => {
                let path = self.dir.join(format!("{decade}.jsonl.zst"));
                let file =
                    File::create(&path).with_context(|| format!("creating {}", path.display()))?;
                let file = zstd::stream::write::Encoder::new(BufWriter::new(file), LEVEL)?;
                e.insert(Spill {
                    path,
                    file,
                    docs: 0,
                })
            }
        };
        spill.file.write_all(&line)?;
        spill.file.write_all(b"\n")?;
        spill.docs += 1;
        self.held += 1;
        self.stats.peak_docs = self.stats.peak_docs.max(self.held);
        if spill.docs >= self.chunk {
            self.send(sink, decade).await?;
        }
        Ok(())
    }

    /// Send every decade's pages left, oldest first.
    pub async fn finish(&mut self, sink: &mut dyn IndexSink) -> anyhow::Result<OrderStats> {
        let decades: Vec<u16> = self.open.keys().copied().collect();
        for d in decades {
            self.send(sink, d).await?;
        }
        Ok(self.stats)
    }

    async fn send(&mut self, sink: &mut dyn IndexSink, decade: u16) -> anyhow::Result<()> {
        let Some(spill) = self.open.remove(&decade) else {
            return Ok(());
        };
        let mut file = spill.file.finish()?;
        file.flush()?;
        let bytes = self.disk_bytes() + file.get_ref().metadata()?.len();
        self.stats.peak_bytes = self.stats.peak_bytes.max(bytes);
        drop(file);
        let mut lines = BufReader::new(zstd::stream::read::Decoder::new(
            File::open(&spill.path).with_context(|| format!("opening {}", spill.path.display()))?,
        )?);
        let mut line = Vec::new();
        let mut sent = 0u64;
        loop {
            line.clear();
            if lines.read_until(b'\n', &mut line)? == 0 {
                break;
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            sink.add_line(&line).await?;
            sent += 1;
        }
        anyhow::ensure!(
            sent == spill.docs,
            "{} held {} pages, but {sent} came back",
            spill.path.display(),
            spill.docs
        );
        std::fs::remove_file(&spill.path)?;
        self.held -= sent;
        self.stats.runs += 1;
        self.stats.docs += sent;
        tracing::info!(
            decade,
            pages = sent,
            held_pages = self.held,
            disk_mb = bytes / (1024 * 1024),
            "sent a decade's pages"
        );
        Ok(())
    }

    /// The compressed bytes of the other decades' files, as flushed so far.
    fn disk_bytes(&self) -> u64 {
        self.open
            .values()
            .filter_map(|s| s.file.get_ref().get_ref().metadata().ok())
            .map(|m| m.len())
            .sum()
    }
}

impl Drop for DecadeOrder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::sink::MainLayout;

    #[derive(Default, Clone)]
    struct Recorder(Arc<Mutex<Vec<Value>>>);

    #[async_trait]
    impl IndexSink for Recorder {
        async fn create(&mut self, _: &str, _: MainLayout) -> anyhow::Result<()> {
            Ok(())
        }
        async fn add(&mut self, doc: &Value) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(doc.clone());
            Ok(())
        }
        async fn finish(&mut self, _: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn backend(&self) -> &'static str {
            "test"
        }
    }

    fn doc(i: u32, decade: u16) -> Value {
        json!({"doc_id": format!("p{i}"), "decade": decade, "text": "gold ".repeat(50)})
    }

    #[tokio::test]
    async fn pages_go_out_a_decade_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        // A previous release's files are cleared.
        std::fs::create_dir_all(dir.path().join(SPILL_DIR)).unwrap();
        std::fs::write(dir.path().join(SPILL_DIR).join("1890.jsonl.zst"), b"x").unwrap();
        let mut sink = Recorder::default();
        let mut order = DecadeOrder::new(dir.path(), 3).unwrap();
        // Archive order: decades interleaved.
        let input = [1890, 1900, 1890, 1880, 1900, 1890, 1900, 1880, 1890];
        for (i, d) in input.iter().enumerate() {
            order.add(&mut sink, &doc(i as u32, *d)).await.unwrap();
        }
        // Two decades reached 3 pages and went out whole, in the order they did.
        let sent: Vec<String> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|d| d["doc_id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(sent, ["p0", "p2", "p5", "p1", "p4", "p6"]);
        let stats = order.finish(&mut sink).await.unwrap();
        let sent: Vec<(String, u64)> = sink
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["doc_id"].as_str().unwrap().to_owned(),
                    d["decade"].as_u64().unwrap(),
                )
            })
            .collect();
        // Then what is left, oldest decade first, and every page once.
        assert_eq!(
            sent[6..],
            [
                ("p3".to_owned(), 1880),
                ("p7".to_owned(), 1880),
                ("p8".to_owned(), 1890)
            ]
        );
        assert_eq!(sent.len(), input.len());
        assert_eq!((stats.runs, stats.docs), (4, 9));
        assert_eq!(stats.peak_docs, 6);
        assert!(stats.peak_bytes > 0);
        // The files are gone once sent, and the directory with the order.
        assert_eq!(
            std::fs::read_dir(dir.path().join(SPILL_DIR))
                .unwrap()
                .count(),
            0
        );
        drop(order);
        assert!(!dir.path().join(SPILL_DIR).exists());
        // Documents come back byte for byte.
        assert_eq!(sink.0.lock().unwrap()[0], doc(0, 1890));
    }

    #[tokio::test]
    async fn a_page_without_a_decade_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut order = DecadeOrder::new(dir.path(), 3).unwrap();
        let err = order
            .add(&mut Recorder::default(), &json!({"doc_id": "x"}))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`x` has no decade"), "{err}");
    }
}
