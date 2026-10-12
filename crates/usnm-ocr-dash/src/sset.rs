//! The Serial Set fetch's progress (`scripts/ocr-node/sset-status.py`):
//! pushed by the node that runs it (`PUT /api/sset/<name>` from that
//! node's address only), every ten minutes.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The largest status accepted.
pub const MAX_SSET_BYTES: usize = 16 * 1024;

/// The status file as `sset-status.py` writes it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SsetStatus {
    pub unit: String,
    /// systemd's `is-active` word for the unit.
    pub active: String,
    /// The `done` marker is there: the fetch finished.
    pub finished: bool,
    pub started: Option<DateTime<Utc>>,
    /// When the status was made, by the node's clock.
    pub at: Option<DateTime<Utc>>,
    pub to_fetch: u64,
    pub curated: u64,
    pub empty: u64,
    pub incomplete: u64,
    pub pages: u64,
    pub current_volume: Option<String>,
    pub documents_read: u64,
    pub volumes_per_hour: f64,
    pub documents_per_hour: f64,
    pub left: u64,
    pub documents_left_estimate: u64,
    pub eta_hours: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Sset {
    pub status: SsetStatus,
    pub received: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct Ssets {
    pub by_name: BTreeMap<String, Sset>,
}

/// A pushed status: JSON.
pub fn parse(body: &[u8]) -> anyhow::Result<SsetStatus> {
    anyhow::ensure!(body.len() <= MAX_SSET_BYTES, "over {MAX_SSET_BYTES} bytes");
    Ok(serde_json::from_slice(body)?)
}

impl Ssets {
    pub fn put(&mut self, name: &str, status: SsetStatus, now: DateTime<Utc>) {
        self.by_name.insert(
            name.to_owned(),
            Sset {
                status,
                received: now,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_the_status_script_writes() {
        let body = br#"{"unit":"usnm-sset.service","active":"active","finished":false,
            "started":"2026-10-11T10:00:00+00:00","at":"2026-10-12T10:00:00+00:00",
            "to_fetch":900,"curated":120,"empty":3,"incomplete":2,"pages":50000,
            "current_volume":"v123","documents_read":4000,"volumes_per_hour":5.5,
            "documents_per_hour":300.0,"left":775,"documents_left_estimate":9000,"eta_hours":30.0}"#;
        let s = parse(body).unwrap();
        assert_eq!(s.curated, 120);
        assert_eq!(s.eta_hours, Some(30.0));
        assert!(parse(b"nope").is_err());
        // A stopped unit has little else.
        let s = parse(br#"{"unit":"u","active":"inactive","started":null}"#).unwrap();
        assert_eq!(s.to_fetch, 0);
    }
}
