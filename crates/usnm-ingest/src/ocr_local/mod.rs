//! Our own OCR of pages LoC read badly, from the on-prem nodes into the
//! lake (#296, 12 §12.5 Steps 8 and 10): `usnm-ingest ocr-sync`,
//! `ocr-enqueue`, `ocr-requeue` and the seeder ([`seed`], `ocr-seed`).
//!
//! The nodes write to the exchange account's `ocr-out` and `ocr-deadletter`
//! (ADR-0015). Everything there is untrusted: any member of the nodes'
//! group can write or overwrite any blob in `ocr-out` and put any message
//! of up to 64 KiB on `ocr-deadletter`. `ocr-sync` checks each part, its
//! ALTO and each dead letter ([`checks`]), copies what passes into the
//! curated store under `ocr-local/`, moves what fails to the exchange
//! account's `ocr-quarantine` with a reason record that holds no content
//! ([`quarantine`]), and deletes the sources with If-Match on the ETags it
//! read, so it only ever writes the bytes it checked.
//!
//! Layout in the curated store (the `curated` container):
//!
//! - `ocr-local/alto/<doc_id>.<node>.<stamp>.xml`: each imported page's
//!   ALTO, as the node wrote it.
//! - `ocr-local/pages/<lccn>/<run>.parquet`: one create-only part per title
//!   and import: the 16 overlay columns ([`usnm_overlay::schema`]) plus
//!   where the row came from and both texts' scores ([`pages`]).
//! - `ocr-local/import/<run>.json`: each import's record, listing the
//!   sources it consumed with their ETags; a run that stops before deleting
//!   them leaves them for the next run, which finds them here and deletes
//!   them without importing them again.
//! - `ocr-local/queued/<day>/<run>.parquet` and
//!   `ocr-local/outcomes/<day>/<run>.parquet`: the ledger ([`ledger`]).
//! - `ocr-local/seed/rank/<key>/<batch>_verNN.json` and
//!   `ocr-local/seed/plans/<plan>/done/<run>.json`: the seeder's ranked
//!   units and each plan's finished ones ([`seed`]).
//! - `ocr-local/deadletter/<day>/<run>.jsonl`: drained dead letters,
//!   re-encoded from their checked fields.
//! - `ocr-local/report/<stamp>.json`: `ocr-sync --report` ([`report`]).
//! - `ocr-local/revoked/<stamp>.json`: rows the release leaves out, by node,
//!   engine, import run or time ([`replace::Revocation`]), written by hand.
//!
//! The release (`--ocr-local`, Step 9) reads the merged parts batch by batch
//! and gives a page our text in place of LoC's where rule v1 ([`rule`])
//! prefers it ([`replace`]).
//!
//! The reference store gets `status/ocr-local.json` ([`sync::Status`]).
//! Logs carry page ids, paths, counts and reason codes, never text.

pub mod checks;
pub mod enqueue;
pub mod ledger;
pub mod pages;
pub mod quarantine;
pub mod replace;
pub mod report;
pub mod rule;
pub mod seed;
pub mod sync;

use chrono::{DateTime, Utc};

/// The curated store's prefix for our OCR.
pub const ROOT: &str = "ocr-local";
/// The exchange account's containers and queues (ADR-0015).
pub const OUT_CONTAINER: &str = "ocr-out";
pub const QUARANTINE_CONTAINER: &str = "ocr-quarantine";
/// The reference store's status file.
pub const STATUS_PATH: &str = "status/ocr-local.json";
/// The Cosmos lock overlapping runs take (`ops/ocr-sync`).
pub const LOCK: &str = "ocr-sync";

/// A run's id, unique and sortable: its UTC start and a random token.
pub fn run_id(now: DateTime<Utc>) -> String {
    format!(
        "{}-{}",
        now.format("%Y%m%dT%H%M%S%6fZ"),
        &usnm_store::queue::random_token()[..8]
    )
}

/// `YYYY-MM-DD`, the ledger's day folders.
pub fn day(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%d").to_string()
}

pub fn alto_path(doc_id: &str, node: &str, stamp: &str) -> String {
    format!("{ROOT}/alto/{doc_id}.{node}.{stamp}.xml")
}

pub fn pages_path(lccn: &str, run: &str) -> String {
    format!("{ROOT}/pages/{lccn}/{run}.parquet")
}

pub fn import_path(run: &str) -> String {
    format!("{ROOT}/import/{run}.json")
}

pub fn queued_path(at: DateTime<Utc>, run: &str) -> String {
    format!("{ROOT}/queued/{}/{run}.parquet", day(at))
}

pub fn outcomes_path(at: DateTime<Utc>, run: &str) -> String {
    format!("{ROOT}/outcomes/{}/{run}.parquet", day(at))
}

pub fn deadletter_path(at: DateTime<Utc>, run: &str) -> String {
    format!("{ROOT}/deadletter/{}/{run}.jsonl", day(at))
}

pub fn report_path(run: &str) -> String {
    format!("{ROOT}/report/{run}.json")
}

/// Microseconds since the epoch as a time.
pub fn from_micros(us: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(us).unwrap_or_default()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod seed_tests;
