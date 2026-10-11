//! Rejected node output, in the exchange account's `ocr-quarantine`
//! container (no node role, a 30-day lifecycle rule; 12 §12.5 Step 8). It
//! never reaches the lake.
//!
//! Each reject is `rejects/<run>/<source path>` (a dead letter's text as
//! `rejects/<run>/deadletter/<message id>.txt`), with a reason record beside it,
//! `<…>.reason.json`, that holds the reason code and where the object came
//! from, never its content. A source over its size limit isn't read, so
//! only its record is written. `ocr-sync --from-quarantine --reason <r>`
//! puts the rejects with that reason back where they came from after a fix,
//! for the next pass to check again.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use usnm_store::queue::{Queue, DEADLETTER};
use usnm_store::ObjectStore;

/// What a reject was.
pub const PART: &str = "part";
pub const ALTO: &str = "alto";
pub const DEAD_LETTER: &str = "deadletter";

/// The container's one top-level folder.
pub const TOP: &str = "rejects";

/// A reject's reason record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub reason: String,
    /// [`PART`], [`ALTO`] or [`DEAD_LETTER`].
    pub kind: String,
    /// The path in `ocr-out`, or the dead letter's message id.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub size: u64,
    /// The source blob's creation time on the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<DateTime<Utc>>,
    pub run: String,
    pub at: DateTime<Utc>,
    /// Where the content is in `ocr-quarantine`; none when it wasn't read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// One reject to keep.
#[derive(Debug, Clone)]
pub struct Item {
    pub reason: String,
    pub kind: &'static str,
    pub source: String,
    pub etag: Option<String>,
    pub size: u64,
    pub created: Option<DateTime<Utc>>,
    pub content: Option<Vec<u8>>,
}

/// Where a reject's content goes: under its source's own path when that
/// fits the store's path rules with the prefix and the record's suffix, or
/// else under the sha256 of the source, so no source name can make the
/// write fail and stop every run.
fn content_path(run: &str, kind: &str, source: &str) -> String {
    let p = if kind == DEAD_LETTER {
        format!("{TOP}/{run}/deadletter/{source}.txt")
    } else {
        format!("{TOP}/{run}/{source}")
    };
    if usnm_store::validate_path(&format!("{p}.reason.json")).is_ok() {
        return p;
    }
    use sha2::{Digest, Sha256};
    let h: String = Sha256::digest(source.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{TOP}/{run}/hashed/{kind}-{h}")
}

/// Keep `item` in quarantine: its content (if read), then its record.
pub async fn put(quarantine: &dyn ObjectStore, run: &str, item: Item) -> anyhow::Result<Record> {
    let path = content_path(run, item.kind, &item.source);
    let content = match item.content {
        Some(bytes) => {
            // A second reject of the same source in one run keeps the first.
            quarantine
                .put_new(&path, bytes, "application/octet-stream")
                .await
                .with_context(|| format!("quarantine {path}"))?;
            Some(path.clone())
        }
        None => None,
    };
    let record = Record {
        reason: item.reason,
        kind: item.kind.to_owned(),
        source: item.source,
        etag: item.etag,
        size: item.size,
        created: item.created,
        run: run.to_owned(),
        at: Utc::now(),
        content,
    };
    quarantine
        .put_new(
            &format!("{path}.reason.json"),
            serde_json::to_vec_pretty(&record)?,
            "application/json",
        )
        .await
        .with_context(|| format!("quarantine record {path}"))?;
    Ok(record)
}

/// What a replay did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct Replayed {
    pub parts: usize,
    pub alto: usize,
    pub dead_letters: usize,
    /// Records with no content to put back (over a size limit).
    pub without_content: usize,
    /// Sources already back in `ocr-out` with the same bytes.
    pub already_there: usize,
    /// Sources whose path in `ocr-out` holds other bytes: kept in quarantine.
    pub conflicts: usize,
}

/// Put every reject with `reason` back where it came from, and remove it
/// from quarantine.
pub async fn replay(
    quarantine: &dyn ObjectStore,
    out: &dyn ObjectStore,
    queue: &Arc<dyn Queue>,
    reason: &str,
) -> anyhow::Result<Replayed> {
    let mut report = Replayed::default();
    let mut runs = Vec::new();
    let mut etags: HashMap<String, String> = HashMap::new();
    for info in quarantine.list_info(TOP).await? {
        if info.path.ends_with(".reason.json") {
            runs.push(info.path.clone());
        }
        etags.insert(info.path, info.etag);
    }
    for record_path in runs {
        let Some(body) = quarantine.get(&record_path).await? else {
            continue;
        };
        let Ok(record) = serde_json::from_slice::<Record>(&body) else {
            tracing::warn!(record = %record_path, "unreadable quarantine record; left as it is");
            continue;
        };
        if record.reason != reason {
            continue;
        }
        let Some(content) = &record.content else {
            report.without_content += 1;
            continue;
        };
        let Some(bytes) = quarantine.get(content).await? else {
            continue;
        };
        match record.kind.as_str() {
            DEAD_LETTER => {
                let text = String::from_utf8(bytes).context("dead letter isn't UTF-8")?;
                queue.send(DEADLETTER, &text).await?;
                report.dead_letters += 1;
            }
            PART | ALTO => {
                let ct = if record.kind == PART {
                    "application/vnd.apache.parquet"
                } else {
                    "application/xml"
                };
                if out.put_new(&record.source, bytes.clone(), ct).await? {
                    if record.kind == PART {
                        report.parts += 1;
                    } else {
                        report.alto += 1;
                    }
                } else if out.get(&record.source).await?.as_deref() == Some(bytes.as_slice()) {
                    report.already_there += 1;
                } else {
                    // Something else is at the source path now: keep the
                    // reject for inspection.
                    tracing::warn!(source = %record.source, record = %record_path, "source path holds other bytes; reject kept in quarantine");
                    report.conflicts += 1;
                    continue;
                }
            }
            _ => continue,
        }
        for p in [content.as_str(), record_path.as_str()] {
            if let Some(etag) = etags.get(p) {
                quarantine.delete_if_match(p, etag).await?;
            }
        }
        tracing::info!(source = %record.source, kind = %record.kind, reason, "replayed from quarantine");
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_sources_get_a_hashed_path() {
        let short = content_path("r1", PART, "pages/a.parquet");
        assert_eq!(short, "rejects/r1/pages/a.parquet");
        let long = format!("pages/{}", "a/".repeat(500));
        let p = content_path("r1", PART, &long);
        assert!(p.starts_with("rejects/r1/hashed/part-"));
        assert!(usnm_store::validate_path(&format!("{p}.reason.json")).is_ok());
    }
}
