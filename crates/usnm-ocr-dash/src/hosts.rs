//! The Proxmox hosts' stats (`scripts/ocr-node/hoststats.sh`): read from a
//! file (pve-big's, bind-mounted read-only) or pushed by the host
//! (`PUT /api/host/<name>` from that host's address only).

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use usnm_ocr_contract::status::HostStats;

/// The largest host stats accepted.
pub const MAX_HOST_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct Host {
    pub stats: Option<HostStats>,
    /// When it was read or pushed.
    pub received: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct Hosts {
    pub files: Vec<PathBuf>,
    pub by_name: BTreeMap<String, Host>,
}

/// Host stats as sent: JSON, and the host they name must be `name` when
/// one is given.
pub fn parse(body: &[u8], name: Option<&str>) -> anyhow::Result<HostStats> {
    anyhow::ensure!(body.len() <= MAX_HOST_BYTES, "over {MAX_HOST_BYTES} bytes");
    let s: HostStats = serde_json::from_slice(body)?;
    anyhow::ensure!(
        crate::nodes::check_name(&s.host).is_ok(),
        "the host name isn't a plain name"
    );
    if let Some(n) = name {
        anyhow::ensure!(s.host == n, "these are {}'s stats, not {n}'s", s.host);
    }
    Ok(s)
}

impl Hosts {
    pub fn new(files: Vec<PathBuf>) -> Self {
        Hosts {
            files,
            by_name: BTreeMap::new(),
        }
    }

    /// Read the files. Blocking.
    pub fn poll(&mut self, now: DateTime<Utc>) {
        for f in self.files.clone() {
            let key = f
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let r = std::fs::read(&f)
                .map_err(anyhow::Error::from)
                .and_then(|b| parse(&b, None));
            match r {
                Ok(s) => self.put(s, now),
                Err(e) => {
                    let h = self.by_name.entry(key).or_insert(Host {
                        stats: None,
                        received: None,
                        error: None,
                    });
                    h.error = Some(format!("{}: {e:#}", f.display()));
                }
            }
        }
    }

    pub fn put(&mut self, s: HostStats, now: DateTime<Utc>) {
        self.by_name.insert(
            s.host.clone(),
            Host {
                stats: Some(s),
                received: Some(now),
                error: None,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushed_stats_must_name_their_host() {
        let body = br#"{"schema":1,"host":"pve","pools":[]}"#;
        assert_eq!(parse(body, Some("pve")).unwrap().host, "pve");
        assert!(parse(body, Some("pve-big")).is_err());
        assert!(parse(br#"{"host":"../etc"}"#, None).is_err());
        assert!(parse(b"not json", None).is_err());
    }

    #[test]
    fn reads_a_file_by_the_host_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("pve-big.json");
        std::fs::write(&p, br#"{"schema":1,"host":"pve-big"}"#).unwrap();
        let mut h = Hosts::new(vec![p, dir.path().join("gone.json")]);
        h.poll(Utc::now());
        assert!(h.by_name["pve-big"].stats.is_some());
        assert!(h.by_name["gone"].error.is_some());
    }
}
