//! The pages whose main-index document holds our own OCR's text in place of
//! LoC's (12 §12.5 Step 9): a version's `ocr_local_pages.bin`, which the
//! release writes and the API loads to mark those hits as our OCR.
//!
//! A version can hold hundreds of thousands of such pages, so the file is
//! compact rather than JSON: about 11 bytes a page. Each entry is a page's
//! 64-bit doc_id hash ([`doc_hash`]), an index into the file's table of
//! engines and the index of the batch whose copy holds our text in the
//! version's batch list (`batches.json`, in the manifest's
//! `built_from.batches` order). A delta carries the published version's
//! entries forward.
//!
//! Layout, little-endian:
//!
//! - [`MAGIC`] (8 bytes);
//! - `u16` engine count, then per engine `u16` length and UTF-8 bytes of its
//!   `ocr_source`, then the same of its short engine key
//!   (`<family>/<version> <profile>`, 13 §13.8.1);
//! - `u32` entry count, then per entry `u64` hash, `u8` engine, `u16` batch,
//!   sorted by hash with no hash twice.

use sha2::{Digest, Sha256};

/// The file in a version's reference snapshot with the pages it holds with
/// our OCR's text ([`OcrLocalPages`]). Only versions with such pages have it.
pub const OCR_LOCAL_PAGES_FILE: &str = "ocr_local_pages.bin";

/// The release's record of how it weighed our OCR: the rule, the metric,
/// the parts read and the verdicts. Only versions built with `--ocr-local`
/// have it.
pub const OCR_LOCAL_FILE: &str = "ocr_local.json";

/// The format's first 8 bytes; the last is its version.
pub const MAGIC: [u8; 8] = *b"USNMOLP\x01";

/// Bytes per entry.
pub const ENTRY_BYTES: usize = 11;

/// A page's 64-bit key: the first 8 bytes of the SHA-256 of its doc_id,
/// read big-endian.
pub fn doc_hash(doc_id: &str) -> u64 {
    let d = Sha256::digest(doc_id.as_bytes());
    u64::from_be_bytes(d[..8].try_into().expect("8 bytes"))
}

/// Who read a page.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Engine {
    /// `ocr_source`: `usnm-<family>`.
    pub source: String,
    /// The short engine key.
    pub engine: String,
}

/// One page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub hash: u64,
    /// Into [`OcrLocalPages::engines`].
    pub engine: u8,
    /// Into the version's batch list.
    pub batch: u16,
}

/// A version's `ocr_local_pages.bin`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OcrLocalPages {
    pub engines: Vec<Engine>,
    /// Sorted by hash, no hash twice.
    pub entries: Vec<Entry>,
}

impl OcrLocalPages {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The page's entry, if it has our text.
    pub fn get(&self, doc_id: &str) -> Option<&Entry> {
        let h = doc_hash(doc_id);
        self.entries
            .binary_search_by_key(&h, |e| e.hash)
            .ok()
            .map(|i| &self.entries[i])
    }

    /// The engine of `entry`.
    pub fn engine(&self, entry: &Entry) -> Option<&Engine> {
        self.engines.get(usize::from(entry.engine))
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::with_capacity(16 + self.entries.len() * ENTRY_BYTES);
        out.extend_from_slice(&MAGIC);
        let n = u16::try_from(self.engines.len()).map_err(|_| "too many engines")?;
        out.extend_from_slice(&n.to_le_bytes());
        for e in &self.engines {
            for s in [&e.source, &e.engine] {
                let len = u16::try_from(s.len()).map_err(|_| "an engine name is too long")?;
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
        let n = u32::try_from(self.entries.len()).map_err(|_| "too many pages")?;
        out.extend_from_slice(&n.to_le_bytes());
        let mut last = None;
        for e in &self.entries {
            if last.is_some_and(|l| l >= e.hash) {
                return Err("entries aren't sorted by hash, or a hash repeats".into());
            }
            if usize::from(e.engine) >= self.engines.len() {
                return Err("an entry's engine isn't in the table".into());
            }
            last = Some(e.hash);
            out.extend_from_slice(&e.hash.to_le_bytes());
            out.push(e.engine);
            out.extend_from_slice(&e.batch.to_le_bytes());
        }
        Ok(out)
    }

    /// Parse a file, checking its every field: `batches` is the version's
    /// batch count, which every entry's batch must be under.
    pub fn decode(bytes: &[u8], batches: usize) -> Result<Self, String> {
        let mut r = Reader { bytes, at: 0 };
        if r.take(8)? != MAGIC {
            return Err("not an ocr_local_pages file of a version this code reads".into());
        }
        let n = u16::from_le_bytes(r.array()?);
        let mut engines = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            let mut s = || -> Result<String, String> {
                let len = u16::from_le_bytes(r.array()?);
                String::from_utf8(r.take(usize::from(len))?.to_vec())
                    .map_err(|_| "an engine name isn't UTF-8".to_owned())
            };
            let source = s()?;
            let engine = s()?;
            engines.push(Engine { source, engine });
        }
        let n = u32::from_le_bytes(r.array()?) as usize;
        if r.left() != n * ENTRY_BYTES {
            return Err(format!(
                "{n} entries need {} bytes; the file has {}",
                n * ENTRY_BYTES,
                r.left()
            ));
        }
        let mut entries = Vec::with_capacity(n);
        let mut last: Option<u64> = None;
        for _ in 0..n {
            let hash = u64::from_le_bytes(r.array()?);
            let [engine] = r.array()?;
            let batch = u16::from_le_bytes(r.array()?);
            if last.is_some_and(|l| l >= hash) {
                return Err("entries aren't sorted by hash, or a hash repeats".into());
            }
            if usize::from(engine) >= engines.len() || usize::from(batch) >= batches {
                return Err("an entry's engine or batch is out of range".into());
            }
            last = Some(hash);
            entries.push(Entry {
                hash,
                engine,
                batch,
            });
        }
        Ok(Self { engines, entries })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|e| *e <= self.bytes.len())
            .ok_or("the file ends early")?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        Ok(self.take(N)?.try_into().expect("N bytes"))
    }

    fn left(&self) -> usize {
        self.bytes.len() - self.at
    }
}

/// Builds a file from pages in any order: engines get table indexes as
/// they come, and a page added twice keeps the last.
#[derive(Debug, Default)]
pub struct Builder {
    engines: Vec<Engine>,
    pages: std::collections::BTreeMap<u64, (u8, u16)>,
}

impl Builder {
    /// Start from a published version's file, its batch indexes mapped by
    /// `batch` (`None` drops the entry).
    pub fn from_pages(
        pages: &OcrLocalPages,
        mut batch: impl FnMut(&Entry) -> Option<u16>,
    ) -> Result<Self, String> {
        let mut b = Self::default();
        for e in &pages.entries {
            let Some(to) = batch(e) else { continue };
            let engine = pages
                .engine(e)
                .ok_or("an entry's engine isn't in the table")?;
            b.add_hash(e.hash, engine.clone(), to)?;
        }
        Ok(b)
    }

    pub fn add(&mut self, doc_id: &str, engine: Engine, batch: u16) -> Result<(), String> {
        self.add_hash(doc_hash(doc_id), engine, batch)
    }

    fn add_hash(&mut self, hash: u64, engine: Engine, batch: u16) -> Result<(), String> {
        let i = match self.engines.iter().position(|e| *e == engine) {
            Some(i) => i,
            None => {
                self.engines.push(engine);
                self.engines.len() - 1
            }
        };
        let i = u8::try_from(i).map_err(|_| "more than 256 engines")?;
        self.pages.insert(hash, (i, batch));
        Ok(())
    }

    /// Drop a page.
    pub fn remove(&mut self, doc_id: &str) {
        self.pages.remove(&doc_hash(doc_id));
    }

    /// The page's batch index, if it is in.
    pub fn batch_of(&self, doc_id: &str) -> Option<u16> {
        self.pages.get(&doc_hash(doc_id)).map(|(_, b)| *b)
    }

    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// The file, with only the engines its entries use.
    pub fn finish(self) -> OcrLocalPages {
        let mut used: Vec<Option<u8>> = vec![None; self.engines.len()];
        let mut engines = Vec::new();
        let mut entries = Vec::with_capacity(self.pages.len());
        for (hash, (engine, batch)) in self.pages {
            let slot = &mut used[usize::from(engine)];
            let engine = *slot.get_or_insert_with(|| {
                engines.push(self.engines[usize::from(engine)].clone());
                u8::try_from(engines.len() - 1).expect("at most 256 engines")
            });
            entries.push(Entry {
                hash,
                engine,
                batch,
            });
        }
        OcrLocalPages { engines, entries }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(n: &str) -> Engine {
        Engine {
            source: "usnm-tesseract".into(),
            engine: n.into(),
        }
    }

    #[test]
    fn round_trips_at_eleven_bytes_a_page() {
        let mut b = Builder::default();
        b.add(
            "sn1_1900-01-01_ed-1_seq-1",
            engine("tesseract/5.5.1 deu"),
            0,
        )
        .unwrap();
        b.add("sn1_1900-01-01_ed-1_seq-2", engine("kraken/5.3 default"), 2)
            .unwrap();
        b.add(
            "sn1_1900-01-01_ed-1_seq-3",
            engine("tesseract/5.5.1 deu"),
            1,
        )
        .unwrap();
        let pages = b.finish();
        assert_eq!(pages.engines.len(), 2);
        let bytes = pages.encode().unwrap();
        let header = 8
            + 2
            + pages
                .engines
                .iter()
                .map(|e| 4 + e.source.len() + e.engine.len())
                .sum::<usize>()
            + 4;
        assert_eq!(bytes.len(), header + 3 * ENTRY_BYTES);
        let back = OcrLocalPages::decode(&bytes, 3).unwrap();
        assert_eq!(back, pages);
        let e = back.get("sn1_1900-01-01_ed-1_seq-2").unwrap();
        assert_eq!(e.batch, 2);
        assert_eq!(back.engine(e).unwrap().engine, "kraken/5.3 default");
        assert!(back.get("sn1_1900-01-01_ed-1_seq-4").is_none());
    }

    #[test]
    fn refuses_a_damaged_file() {
        let mut b = Builder::default();
        b.add("a", engine("x"), 1).unwrap();
        let bytes = b.finish().encode().unwrap();
        assert!(OcrLocalPages::decode(&bytes, 2).is_ok());
        assert!(OcrLocalPages::decode(&bytes, 1).is_err(), "batch range");
        assert!(OcrLocalPages::decode(&bytes[..bytes.len() - 1], 2).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(OcrLocalPages::decode(&longer, 2).is_err());
        let mut magic = bytes;
        magic[7] = 2;
        assert!(OcrLocalPages::decode(&magic, 2).is_err());
    }

    #[test]
    fn carries_forward_with_batches_remapped_and_engines_pruned() {
        let mut b = Builder::default();
        b.add("a", engine("x"), 0).unwrap();
        b.add("b", engine("y"), 1).unwrap();
        let before = b.finish();
        let mut b = Builder::from_pages(&before, |e| (e.batch == 1).then_some(5)).unwrap();
        assert_eq!(b.batch_of("b"), Some(5));
        assert_eq!(b.batch_of("a"), None);
        b.add("c", engine("y"), 6).unwrap();
        let after = b.finish();
        assert_eq!(after.engines, vec![engine("y")]);
        assert_eq!(after.len(), 2);
    }
}
