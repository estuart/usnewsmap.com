//! Object storage for published reference data, the persistent response
//! cache and the search log (04 §4.3, 06 §6.5, 06 §6.8).
//!
//! Production reads and writes Azure Blob Storage over its private endpoint
//! with the app's managed identity (08 §8.3): Entra ID bearer tokens only, no
//! account keys or SAS. Development and tests use a local directory with the
//! same layout.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;

pub mod blob;
pub mod credential;
pub mod local;
pub mod queue;

pub use blob::BlobStore;
pub use local::LocalStore;

/// An object's chunks as they are read (`ObjectStore::get_stream`).
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StoreError>> + Send>>;

/// Largest object `get` will read into memory.
pub const MAX_OBJECT_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid object path `{0}`")]
    InvalidPath(String),
    #[error("invalid store location: {0}")]
    InvalidLocation(String),
    #[error("{0}")]
    Io(String),
    #[error("{op} `{path}` returned HTTP {status}")]
    Http {
        op: &'static str,
        path: String,
        status: u16,
    },
    #[error("credential: {0}")]
    Credential(String),
    #[error("object `{0}` exceeds {MAX_OBJECT_BYTES} bytes")]
    TooLarge(String),
    /// A create-only write found an object already there (`put_stream`).
    #[error("object `{0}` already exists")]
    AlreadyExists(String),
}

/// One object as a listing shows it ([`ObjectStore::list_info`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub path: String,
    pub size: u64,
    /// Changes whenever the object is written: a conditional read or delete
    /// ([`ObjectStore::get_if_match`], [`ObjectStore::delete_if_match`])
    /// acts only on the bytes the listing saw.
    pub etag: String,
    /// When the store created the object, by the store's clock (Blob
    /// Storage's `Creation-Time`; a local file's modification time).
    pub created: Option<std::time::SystemTime>,
}

fn unsupported(op: &str) -> StoreError {
    StoreError::Io(format!("{op} is not supported by this store"))
}

#[async_trait]
pub trait ObjectStore: Send + Sync + std::fmt::Debug {
    /// Read a whole object; `None` if it doesn't exist.
    async fn get(&self, path: &str) -> Result<Option<Vec<u8>>, StoreError>;

    /// Create an object unless one already exists at `path` (objects are
    /// immutable once written). Returns whether this call wrote it.
    async fn put_new(
        &self,
        path: &str,
        body: Vec<u8>,
        content_type: &str,
    ) -> Result<bool, StoreError>;

    /// Create or replace an object. Only the publisher uses this, for the
    /// version pointer (`current.json`); everything else is create-only.
    async fn put(&self, path: &str, body: Vec<u8>, content_type: &str) -> Result<(), StoreError>;

    /// Whether an object exists.
    async fn exists(&self, path: &str) -> Result<bool, StoreError> {
        Ok(self.get(path).await?.is_some())
    }

    /// When the object was last written; `None` if it doesn't exist or the
    /// store doesn't say. Only the status page uses this.
    async fn modified(&self, path: &str) -> Result<Option<std::time::SystemTime>, StoreError> {
        let _ = path;
        Ok(None)
    }

    /// Read an object as a stream of chunks, however large; `None` if it
    /// doesn't exist. The default reads it whole (`get`, so at most
    /// [`MAX_OBJECT_BYTES`]); Blob Storage streams it.
    async fn get_stream(&self, path: &str) -> Result<Option<ByteStream>, StoreError> {
        Ok(self.get(path).await?.map(|b| {
            let s: ByteStream = Box::pin(futures::stream::once(async move { Ok(Bytes::from(b)) }));
            s
        }))
    }

    /// Create an object from `chunks` as they arrive, without holding it
    /// whole, in access tier `tier` where the store has tiers (`Cold` in
    /// Blob Storage). It becomes visible only if `commit` says `true` once
    /// the chunks end; otherwise (`false`, or the sender dropped) nothing is
    /// written and this fails. Create-only, like `put_new`: if an object is
    /// already at `path` it stays as it is and this fails with
    /// [`StoreError::AlreadyExists`]. Returns its size. The default buffers
    /// the object and `put_new`s it.
    async fn put_stream(
        &self,
        path: &str,
        content_type: &str,
        tier: Option<&str>,
        mut chunks: tokio::sync::mpsc::Receiver<Bytes>,
        commit: tokio::sync::oneshot::Receiver<bool>,
    ) -> Result<u64, StoreError> {
        let _ = tier;
        let mut body = Vec::new();
        while let Some(c) = chunks.recv().await {
            body.extend_from_slice(&c);
        }
        if commit.await != Ok(true) {
            return Err(StoreError::Io(format!("`{path}`: upload abandoned")));
        }
        let n = body.len() as u64;
        if !self.put_new(path, body, content_type).await? {
            return Err(StoreError::AlreadyExists(path.into()));
        }
        Ok(n)
    }

    /// The paths of the objects under `prefix` (a directory-like path
    /// without a trailing `/`), sorted. Only the search log (`usnm-api`) uses this.
    async fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;

    /// The objects under `prefix` with their size, ETag and creation time,
    /// sorted by path. The OCR pickup (`usnm-ingest ocr-sync`) checks sizes
    /// before it reads anything.
    async fn list_info(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StoreError> {
        let _ = prefix;
        Err(unsupported("list_info"))
    }

    /// Read an object only while its ETag is still `etag`, and only if it is
    /// at most `max_bytes`: `None` if it is gone or was written again since.
    /// Fails with [`StoreError::TooLarge`] past `max_bytes`, without reading
    /// the rest.
    async fn get_if_match(
        &self,
        path: &str,
        etag: &str,
        max_bytes: u64,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let _ = (path, etag, max_bytes);
        Err(unsupported("get_if_match"))
    }

    /// Delete an object only while its ETag is still `etag`. Returns whether
    /// this call deleted it: `false` if it is gone or was written again.
    async fn delete_if_match(&self, path: &str, etag: &str) -> Result<bool, StoreError> {
        let _ = (path, etag);
        Err(unsupported("delete_if_match"))
    }
}

/// Open a store: an `https://{account}.blob.core.windows.net/{container}[/prefix]`
/// URL uses Blob Storage with the ambient Entra ID credential
/// ([`credential::from_env`]); anything else is a local directory.
pub fn open(location: &str) -> Result<Arc<dyn ObjectStore>, StoreError> {
    if location.starts_with("https://") || location.starts_with("http://") {
        Ok(Arc::new(BlobStore::new(location, credential::from_env())?))
    } else {
        Ok(Arc::new(LocalStore::new(location)))
    }
}

/// A single path segment: `[A-Za-z0-9][A-Za-z0-9._-]{0,127}`. Version and
/// index ids are checked with this before they become part of a path.
pub fn is_safe_segment(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// Object paths are `/`-separated safe segments, so they need no URL encoding
/// and can't escape a local root.
pub fn validate_path(path: &str) -> Result<(), StoreError> {
    if path.len() <= 1024 && path.split('/').all(is_safe_segment) {
        Ok(())
    } else {
        Err(StoreError::InvalidPath(path.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        for ok in [
            "current.json",
            "fixture-v1/places.json",
            "v1/f1/ab12.json.zst",
        ] {
            assert!(validate_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "/abs", "a//b", "../x", "a/../b", ".hidden", "a b", "a?b", "a%2fb", "a\\b",
        ] {
            assert!(validate_path(bad).is_err(), "{bad}");
        }
    }
}
