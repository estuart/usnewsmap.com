//! An index copied from Blob to the replica's disk, served from there
//! (#251's local-disk test, variant "copy"): Quickwit then reads its splits
//! as local files, with no download, TLS or Blob client on its main runtime.
//!
//! The file-backed metastore keeps an index at `{metastore}/{index}/
//! metastore.json`, next to its splits when the index lives under the
//! metastore's root, as the cluster's do (`azure://qw-cluster/{index}/`), and
//! lists its indexes in `{metastore}/manifest.json`. The copy takes every
//! object under `{index}/`, points the copied `metastore.json` at the local
//! directory (`index.index_config.index_uri`), and writes a manifest naming
//! only that index, so a node started on `file://{dir}` serves it.
//!
//! Splits are written to `{name}.part` and renamed when complete, so a
//! start that finds them there (on an NFS share, mode `nfs`, they outlive
//! the replica) skips the splits it already has.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context};
use futures::stream::{self, StreamExt, TryStreamExt};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use usnm_store::ObjectStore;

/// Where the local-disk test's `nfs` mode mounts its NFS Azure Files share
/// (infra/modules/searchcluster.bicep); a copy under it is that mode's.
pub const NFS_MOUNT: &str = "/mnt/qwsearch";

/// The per-index metastore file (`quickwit-metastore`'s `METASTORE_FILE_NAME`).
pub const METASTORE_FILE: &str = "metastore.json";

/// Objects copied at once.
const COPIES: usize = 4;

/// What a copy did.
#[derive(Debug, Clone, PartialEq)]
pub struct Copied {
    /// `file://{dir}`: the metastore and index root to start the node on.
    pub root_uri: String,
    pub objects: usize,
    /// Objects already there from an earlier start.
    pub skipped: usize,
    pub bytes: u64,
    pub secs: f64,
}

/// `metastore.json` with the index's `index_uri` set to `index_uri`.
pub fn rewrite_metastore(json_bytes: &[u8], index_uri: &str) -> anyhow::Result<Vec<u8>> {
    let mut v: Value = serde_json::from_slice(json_bytes).context("metastore.json")?;
    let uri = v
        .pointer_mut("/index/index_config/index_uri")
        .context("metastore.json has no index.index_config.index_uri")?;
    if !uri.is_string() {
        bail!("metastore.json's index_uri isn't a string");
    }
    *uri = Value::String(index_uri.to_owned());
    Ok(serde_json::to_vec_pretty(&v)?)
}

/// The file-backed metastore's manifest, naming only `index_id`.
pub fn manifest(index_id: &str) -> Vec<u8> {
    let v = json!({
        "version": "0.9",
        "indexes": { index_id: "active" },
        "templates": [],
    });
    serde_json::to_vec_pretty(&v).unwrap_or_default()
}

/// Copy `index_id` from `store` (the index root, e.g. the `qw-cluster`
/// container) into `dir/{index_id}/` and write the manifest in `dir`.
pub async fn copy_index(
    store: &dyn ObjectStore,
    index_id: &str,
    dir: &Path,
) -> anyhow::Result<Copied> {
    if !usnm_store::is_safe_segment(index_id) || index_id.contains("qw-index") {
        bail!("`{index_id}` isn't a usable index id");
    }
    let started = Instant::now();
    let index_dir = dir.join(index_id);
    if let Some(df) = super::node::disk_free(dir.parent().unwrap_or(dir)) {
        tracing::info!(dir = %dir.display(), df = %df, "local copy disk");
    }
    tokio::fs::create_dir_all(&index_dir)
        .await
        .with_context(|| format!("creating {}", index_dir.display()))?;
    let prefix = format!("{index_id}/");
    // Listed by the bare id (a store path has no empty segment), so other
    // indexes whose ids start the same are filtered out.
    let paths: Vec<String> = store
        .list(index_id)
        .await?
        .into_iter()
        .filter(|p| p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
        .collect();
    let meta_path = format!("{prefix}{METASTORE_FILE}");
    if !paths.contains(&meta_path) {
        bail!("no {meta_path} in the index root: is `{index_id}` one of its indexes?");
    }
    let root_uri = format!("file://{}", dir.display());
    let index_uri = format!("{root_uri}/{index_id}");
    let meta = store
        .get(&meta_path)
        .await?
        .with_context(|| format!("{meta_path} went away"))?;
    tokio::fs::write(
        index_dir.join(METASTORE_FILE),
        rewrite_metastore(&meta, &index_uri)?,
    )
    .await?;
    let others: Vec<String> = paths.into_iter().filter(|p| *p != meta_path).collect();
    let objects = others.len() + 1;
    tracing::info!(index = index_id, objects, dir = %dir.display(), "copying the index to the local disk");
    let results: Vec<(bool, u64)> = stream::iter(others)
        .map(|path| {
            let target = dir.join(&path);
            async move { copy_one(store, &path, &target).await }
        })
        .buffer_unordered(COPIES)
        .try_collect()
        .await?;
    tokio::fs::write(dir.join("manifest.json"), manifest(index_id)).await?;
    let copied = Copied {
        root_uri,
        objects,
        skipped: results.iter().filter(|(skipped, _)| *skipped).count(),
        bytes: results.iter().map(|(_, b)| b).sum(),
        secs: started.elapsed().as_secs_f64(),
    };
    tracing::info!(
        index = index_id,
        objects = copied.objects,
        skipped = copied.skipped,
        gb = copied.bytes as f64 / 1e9,
        secs = copied.secs.round(),
        "index copied to the local disk"
    );
    Ok(copied)
}

/// One object to `target`, through `{target}.part`; `(true, size)` when
/// `target` was there already.
async fn copy_one(
    store: &dyn ObjectStore,
    path: &str,
    target: &PathBuf,
) -> anyhow::Result<(bool, u64)> {
    if let Ok(m) = tokio::fs::metadata(target).await {
        return Ok((true, m.len()));
    }
    let part = target.with_extension(match target.extension() {
        Some(e) => format!("{}.part", e.to_string_lossy()),
        None => "part".to_owned(),
    });
    let mut body = store
        .get_stream(path)
        .await?
        .with_context(|| format!("{path} went away"))?;
    let mut file = tokio::fs::File::create(&part)
        .await
        .with_context(|| format!("creating {}", part.display()))?;
    let mut bytes = 0u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.with_context(|| format!("reading {path}"))?;
        bytes += chunk.len() as u64;
        file.write_all(&chunk)
            .await
            .with_context(|| format!("writing {}", part.display()))?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&part, target).await?;
    Ok((false, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_the_metastore_at_the_copy() {
        let meta = br#"{"version":"0.9","index":{"index_uid":"s1ixb:01","index_config":{"version":"0.9","index_id":"s1ixb","index_uri":"azure://qw-cluster/s1ixb"}},"splits":[{"split_id":"a"}]}"#;
        let out = rewrite_metastore(meta, "file:///qwlocal/index/s1ixb").unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["index"]["index_config"]["index_uri"],
            "file:///qwlocal/index/s1ixb"
        );
        assert_eq!(v["splits"][0]["split_id"], "a");
        assert!(rewrite_metastore(br#"{"index":{}}"#, "file:///x").is_err());
        let m: Value = serde_json::from_slice(&manifest("s1ixb")).unwrap();
        assert_eq!(
            m,
            json!({"version": "0.9", "indexes": {"s1ixb": "active"}, "templates": []})
        );
    }

    #[tokio::test]
    async fn copies_an_index_and_skips_what_it_has() {
        let src = tempfile::tempdir().unwrap();
        let store = usnm_store::open(src.path().to_str().unwrap()).unwrap();
        let meta =
            br#"{"index":{"index_config":{"index_uri":"azure://qw-cluster/ix"}},"splits":[]}"#;
        store
            .put("ix/metastore.json", meta.to_vec(), "application/json")
            .await
            .unwrap();
        store
            .put("ix/01A.split", vec![7u8; 1000], "application/octet-stream")
            .await
            .unwrap();
        store
            .put("ix/01B.split", vec![8u8; 10], "application/octet-stream")
            .await
            .unwrap();
        store
            .put("other/01C.split", vec![9u8; 5], "application/octet-stream")
            .await
            .unwrap();
        let dst = tempfile::tempdir().unwrap();
        let c = copy_index(store.as_ref(), "ix", dst.path()).await.unwrap();
        assert_eq!((c.objects, c.skipped, c.bytes), (3, 0, 1010));
        assert_eq!(c.root_uri, format!("file://{}", dst.path().display()));
        assert_eq!(
            std::fs::read(dst.path().join("ix/01A.split")).unwrap(),
            vec![7u8; 1000]
        );
        assert!(!dst.path().join("other").exists());
        let v: Value =
            serde_json::from_slice(&std::fs::read(dst.path().join("ix/metastore.json")).unwrap())
                .unwrap();
        assert_eq!(
            v["index"]["index_config"]["index_uri"],
            format!("{}/ix", c.root_uri)
        );
        assert!(dst.path().join("manifest.json").exists());
        // A second start keeps the splits it has.
        let again = copy_index(store.as_ref(), "ix", dst.path()).await.unwrap();
        assert_eq!((again.skipped, again.bytes), (2, 1010));
        assert!(copy_index(store.as_ref(), "nothing", dst.path())
            .await
            .is_err());
        assert!(copy_index(store.as_ref(), "../ix", dst.path())
            .await
            .is_err());
    }
}
