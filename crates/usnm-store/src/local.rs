//! A directory with the same layout as a Blob container.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use async_trait::async_trait;

use bytes::Bytes;

use crate::{validate_path, ByteStream, ObjectInfo, ObjectStore, StoreError, MAX_OBJECT_BYTES};

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn io(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Io(format!("{}: {e}", path.display()))
}

/// A local object's ETag: the file's identity (device and inode on Unix),
/// size and modification time. Every write makes a new file (a temp file
/// renamed or linked into place), so the identity changes with each one
/// even when the clock's resolution doesn't.
fn local_etag(m: &std::fs::Metadata) -> String {
    let nanos = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    #[cfg(unix)]
    let id = {
        use std::os::unix::fs::MetadataExt;
        format!("{:x}.{:x}", m.dev(), m.ino())
    };
    #[cfg(not(unix))]
    let id = String::from("0");
    format!("{id}-{:x}-{nanos:x}", m.len())
}

/// Bytes per chunk of a streamed read (`get_stream`).
const READ_CHUNK: usize = 1024 * 1024;

/// A temp file beside `full`, unique per process, thread and call.
fn temp_path(full: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    full.with_file_name(format!(
        ".tmp-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ))
}

#[async_trait]
impl ObjectStore for LocalStore {
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        match tokio::fs::metadata(&full).await {
            Ok(m) if m.len() > MAX_OBJECT_BYTES => return Err(StoreError::TooLarge(path.into())),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&full, e)),
        }
        match tokio::fs::read(&full).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&full, e)),
        }
    }

    async fn put(&self, path: &str, body: Vec<u8>, _content_type: &str) -> Result<(), StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        tokio::task::spawn_blocking(move || {
            let dir = full.parent().expect("validated paths have a parent");
            std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            // Readers see the old object or the new one, never a partial write.
            let tmp = dir.join(format!(
                ".tmp-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            std::fs::rename(&tmp, &full).map_err(|e| io(&full, e))
        })
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?
    }

    async fn put_new(
        &self,
        path: &str,
        body: Vec<u8>,
        _content_type: &str,
    ) -> Result<bool, StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        tokio::task::spawn_blocking(move || {
            let dir = full.parent().expect("validated paths have a parent");
            std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
            // Write a private temp file, then hard-link it into place: the
            // link fails if the object exists, so creation is atomic.
            let tmp = dir.join(format!(
                ".tmp-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::write(&tmp, &body).map_err(|e| io(&tmp, e))?;
            let linked = std::fs::hard_link(&tmp, &full);
            let _ = std::fs::remove_file(&tmp);
            match linked {
                Ok(()) => Ok(true),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(false),
                Err(e) => Err(io(&full, e)),
            }
        })
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?
    }

    /// Reads the file in chunks, however large.
    async fn get_stream(&self, path: &str) -> Result<Option<ByteStream>, StoreError> {
        use tokio::io::AsyncReadExt;
        validate_path(path)?;
        let full = self.root.join(path);
        let file = match tokio::fs::File::open(&full).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io(&full, e)),
        };
        let s = futures::stream::try_unfold((file, full), |(mut file, full)| async move {
            let mut buf = Vec::with_capacity(READ_CHUNK);
            let n = (&mut file)
                .take(READ_CHUNK as u64)
                .read_to_end(&mut buf)
                .await
                .map_err(|e| io(&full, e))?;
            Ok((n > 0).then(|| (Bytes::from(buf), (file, full))))
        });
        Ok(Some(Box::pin(s)))
    }

    /// Writes the chunks to a temp file as they arrive, then hard-links it
    /// into place on commit: the link fails if the object exists, so
    /// creation is atomic and an existing object is never replaced.
    async fn put_stream(
        &self,
        path: &str,
        _content_type: &str,
        _tier: Option<&str>,
        mut chunks: tokio::sync::mpsc::Receiver<Bytes>,
        commit: tokio::sync::oneshot::Receiver<bool>,
    ) -> Result<u64, StoreError> {
        use tokio::io::AsyncWriteExt;
        validate_path(path)?;
        let full = self.root.join(path);
        let dir = full.parent().expect("validated paths have a parent");
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io(dir, e))?;
        let tmp = temp_path(&full);
        let written = async {
            let mut f = tokio::fs::File::create(&tmp)
                .await
                .map_err(|e| io(&tmp, e))?;
            let mut n = 0u64;
            while let Some(c) = chunks.recv().await {
                f.write_all(&c).await.map_err(|e| io(&tmp, e))?;
                n += c.len() as u64;
            }
            f.sync_all().await.map_err(|e| io(&tmp, e))?;
            if commit.await != Ok(true) {
                return Err(StoreError::Io(format!("`{path}`: upload abandoned")));
            }
            match tokio::fs::hard_link(&tmp, &full).await {
                Ok(()) => Ok(n),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    Err(StoreError::AlreadyExists(path.into()))
                }
                Err(e) => Err(io(&full, e)),
            }
        }
        .await;
        let _ = tokio::fs::remove_file(&tmp).await;
        written
    }

    async fn exists(&self, path: &str) -> Result<bool, StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        match tokio::fs::metadata(&full).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(io(&full, e)),
        }
    }

    async fn modified(&self, path: &str) -> Result<Option<std::time::SystemTime>, StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        match tokio::fs::metadata(&full).await {
            Ok(m) => Ok(m.modified().ok()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io(&full, e)),
        }
    }

    async fn list_info(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StoreError> {
        let mut out = Vec::new();
        for path in self.list(prefix).await? {
            let full = self.root.join(&path);
            match tokio::fs::metadata(&full).await {
                Ok(m) => out.push(ObjectInfo {
                    etag: local_etag(&m),
                    size: m.len(),
                    created: m.modified().ok(),
                    path,
                }),
                // Deleted since the listing.
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(io(&full, e)),
            }
        }
        Ok(out)
    }

    /// Reads from one open handle, so the ETag and size checked are the
    /// file's whose bytes are read, and reads at most `max_bytes` + 1.
    async fn get_if_match(
        &self,
        path: &str,
        etag: &str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        use std::io::Read;
        validate_path(path)?;
        let full = self.root.join(path);
        let etag = etag.to_owned();
        let path = path.to_owned();
        tokio::task::spawn_blocking(move || {
            let file = match std::fs::File::open(&full) {
                Ok(f) => f,
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io(&full, e)),
            };
            let m = file.metadata().map_err(|e| io(&full, e))?;
            if local_etag(&m) != etag {
                return Ok(None);
            }
            let max = max_bytes.min(MAX_OBJECT_BYTES);
            if m.len() > max {
                return Err(StoreError::TooLarge(path));
            }
            let mut body = Vec::new();
            file.take(max + 1)
                .read_to_end(&mut body)
                .map_err(|e| io(&full, e))?;
            if body.len() as u64 > max {
                return Err(StoreError::TooLarge(path));
            }
            Ok(Some(body))
        })
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?
    }

    /// Moves the file aside first (atomic), then compares the ETag of the
    /// file it moved, so a write that lands between the check and the
    /// delete is never the one deleted: a file that doesn't match is linked
    /// back unless a newer one has taken its place.
    async fn delete_if_match(&self, path: &str, etag: &str) -> Result<bool, StoreError> {
        validate_path(path)?;
        let full = self.root.join(path);
        let etag = etag.to_owned();
        tokio::task::spawn_blocking(move || {
            let aside = temp_path(&full);
            match std::fs::rename(&full, &aside) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(io(&full, e)),
            }
            let matched = std::fs::metadata(&aside)
                .map(|m| local_etag(&m) == etag)
                .map_err(|e| io(&aside, e))?;
            if !matched {
                match std::fs::hard_link(&aside, &full) {
                    Ok(()) => {}
                    // Written again meanwhile: the newer file stays.
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(io(&full, e)),
                }
            }
            std::fs::remove_file(&aside).map_err(|e| io(&aside, e))?;
            Ok(matched)
        })
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        validate_path(prefix)?;
        let root = self.root.clone();
        let prefix = prefix.to_owned();
        tokio::task::spawn_blocking(move || {
            fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> Result<(), StoreError> {
                let entries = match std::fs::read_dir(dir) {
                    Ok(e) => e,
                    Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
                    Err(e) => return Err(io(dir, e)),
                };
                for entry in entries {
                    let entry = entry.map_err(|e| io(dir, e))?;
                    let name = entry.file_name().to_string_lossy().into_owned();
                    let path = format!("{rel}/{name}");
                    // Temporary files and anything else that isn't a valid path.
                    if validate_path(&path).is_err() {
                        continue;
                    }
                    if entry.path().is_dir() {
                        walk(&entry.path(), &path, out)?;
                    } else {
                        out.push(path);
                    }
                }
                Ok(())
            }
            let mut out = Vec::new();
            walk(&root.join(&prefix), &prefix, &mut out)?;
            out.sort();
            Ok(out)
        })
        .await
        .map_err(|e| StoreError::Io(e.to_string()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn conditional_reads_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path());
        assert!(store
            .put_new("out/a.bin", b"one".to_vec(), "x")
            .await
            .unwrap());
        assert!(store.put_new("out/b.bin", vec![0; 10], "x").await.unwrap());
        let listed = store.list_info("out").await.unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|o| (o.path.as_str(), o.size))
                .collect::<Vec<_>>(),
            [("out/a.bin", 3), ("out/b.bin", 10)]
        );
        assert!(listed.iter().all(|o| o.created.is_some()));
        let a = &listed[0];
        assert_eq!(
            store.get_if_match(&a.path, &a.etag, 3).await.unwrap(),
            Some(b"one".to_vec())
        );
        assert!(matches!(
            store.get_if_match(&a.path, &a.etag, 2).await,
            Err(StoreError::TooLarge(_))
        ));
        assert_eq!(store.get_if_match(&a.path, "other", 3).await.unwrap(), None);
        assert!(!store.delete_if_match(&a.path, "other").await.unwrap());
        // Written again since the listing, at the same size: neither read
        // nor deleted.
        store.put(&a.path, b"two".to_vec(), "x").await.unwrap();
        assert_eq!(store.get_if_match(&a.path, &a.etag, 9).await.unwrap(), None);
        assert!(!store.delete_if_match(&a.path, &a.etag).await.unwrap());
        assert_eq!(store.get(&a.path).await.unwrap().unwrap(), b"two", "kept");
        let now = store.list_info("out").await.unwrap();
        assert!(store
            .delete_if_match(&now[0].path, &now[0].etag)
            .await
            .unwrap());
        assert!(!store
            .delete_if_match(&now[0].path, &now[0].etag)
            .await
            .unwrap());
        assert_eq!(store.list("out").await.unwrap(), ["out/b.bin"]);
    }

    #[tokio::test]
    async fn create_only_round_trip() {
        let dir = std::env::temp_dir().join(format!("usnm-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = LocalStore::new(&dir);
        assert_eq!(s.get("v1/a.json").await.unwrap(), None);
        assert!(s.put_new("v1/a.json", b"one".to_vec(), "").await.unwrap());
        assert!(!s.put_new("v1/a.json", b"two".to_vec(), "").await.unwrap());
        assert_eq!(s.get("v1/a.json").await.unwrap().unwrap(), b"one");
        s.put("v1/a.json", b"three".to_vec(), "").await.unwrap();
        assert_eq!(s.get("v1/a.json").await.unwrap().unwrap(), b"three");
        assert!(s.get("../etc/passwd").await.is_err());
        assert!(s.exists("v1/a.json").await.unwrap());
        assert!(!s.exists("v1/b.json").await.unwrap());
        assert!(s.modified("v1/a.json").await.unwrap().is_some());
        assert_eq!(s.modified("v1/b.json").await.unwrap(), None);
        assert!(s.put_new("log/d/2.jsonl", b"2".to_vec(), "").await.unwrap());
        assert!(s.put_new("log/d/1.jsonl", b"1".to_vec(), "").await.unwrap());
        assert!(s.put_new("logs/x.jsonl", b"x".to_vec(), "").await.unwrap());
        assert_eq!(
            s.list("log").await.unwrap(),
            ["log/d/1.jsonl", "log/d/2.jsonl"]
        );
        assert!(s.list("none").await.unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn stream_in(
        s: &LocalStore,
        path: &str,
        data: &[u8],
        commit: bool,
    ) -> Result<u64, StoreError> {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let (done, committed) = tokio::sync::oneshot::channel();
        let feed = async {
            for c in data.chunks(300_001) {
                tx.send(Bytes::copy_from_slice(c)).await.unwrap();
            }
            drop(tx);
            done.send(commit).unwrap();
        };
        let (written, ()) = tokio::join!(s.put_stream(path, "", None, rx, committed), feed);
        written
    }

    /// Streamed writes are create-only and visible only once committed;
    /// streamed reads come back in chunks, with no size limit.
    #[tokio::test]
    async fn streams_create_only_and_read_in_chunks() {
        use futures::StreamExt;
        let dir = std::env::temp_dir().join(format!("usnm-store-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = LocalStore::new(&dir);
        let data: Vec<u8> = (0..(READ_CHUNK * 2 + 777))
            .map(|i| (i % 251) as u8)
            .collect();
        assert!(matches!(
            stream_in(&s, "raw/b/b.tar.bz2", b"partial", false).await,
            Err(StoreError::Io(_))
        ));
        assert!(!s.exists("raw/b/b.tar.bz2").await.unwrap());
        assert_eq!(
            stream_in(&s, "raw/b/b.tar.bz2", &data, true).await.unwrap(),
            data.len() as u64
        );
        assert!(matches!(
            stream_in(&s, "raw/b/b.tar.bz2", b"another", true).await,
            Err(StoreError::AlreadyExists(_))
        ));
        assert!(
            s.list("raw").await.unwrap() == ["raw/b/b.tar.bz2"],
            "no temp files left"
        );
        let mut chunks = 0;
        let mut back = Vec::new();
        let mut st = s.get_stream("raw/b/b.tar.bz2").await.unwrap().unwrap();
        while let Some(c) = st.next().await {
            back.extend_from_slice(&c.unwrap());
            chunks += 1;
        }
        assert_eq!(back, data);
        assert_eq!(chunks, 3);
        assert!(s.get_stream("raw/none").await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
