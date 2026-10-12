//! Per-client token bucket (06 §6.2, 08 §8.2). With no WAF on the lean
//! profile this, the replica cap and the backend semaphore are the abuse
//! controls.
//!
//! Clients are keyed by a salted hash of their address (IPv6 by /64), held
//! only in memory; addresses are never logged (09 §9.4.2). The address is
//! taken from `X-Forwarded-For` counting back `trusted_proxy_hops` entries
//! from the right, since entries further left are client-supplied.

use std::hash::BuildHasher;
use std::net::IpAddr;
use std::time::Duration;

use axum::http::HeaderMap;
use governor::clock::{Clock, DefaultClock};
use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};

use crate::config::RateLimit;

pub struct Limiter {
    buckets: DefaultKeyedRateLimiter<u64>,
    /// Randomly keyed per process: the salt for client keys.
    salt: std::collections::hash_map::RandomState,
    hops: usize,
}

impl Limiter {
    pub fn new(limit: RateLimit, trusted_proxy_hops: usize) -> Self {
        Self {
            buckets: RateLimiter::keyed(
                Quota::per_minute(limit.per_minute).allow_burst(limit.burst),
            ),
            salt: Default::default(),
            hops: trusted_proxy_hops,
        }
    }

    /// Take one token for this client, or say how long until one is available.
    pub fn check(&self, headers: &HeaderMap, peer: Option<IpAddr>) -> Result<(), Duration> {
        let key = client_ip(headers, peer, self.hops).map_or(0, |ip| self.key(ip));
        self.buckets
            .check_key(&key)
            .map_err(|not_until| not_until.wait_time_from(DefaultClock::default().now()))
    }

    fn key(&self, ip: IpAddr) -> u64 {
        match ip.to_canonical() {
            IpAddr::V4(v4) => self.salt.hash_one(v4.octets()),
            // One household or host usually owns a whole /64.
            IpAddr::V6(v6) => self.salt.hash_one(&v6.octets()[..8]),
        }
    }

    /// Drop idle buckets so memory stays bounded.
    pub fn housekeeping(&self) {
        self.buckets.retain_recent();
        self.buckets.shrink_to_fit();
    }
}

/// The client address: `hops` entries from the right of `X-Forwarded-For`
/// (all header lines, in order). If the header has fewer entries than there
/// are trusted proxies, or the chosen entry isn't an address, no entry can be
/// shown to be proxy-added, so this falls back to the peer address.
pub fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, hops: usize) -> Option<IpAddr> {
    if hops == 0 {
        return peer;
    }
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    match entries.len().checked_sub(hops) {
        Some(i) => parse_ip(entries[i]).or(peer),
        None => peer,
    }
}

/// Accepts `1.2.3.4`, `1.2.3.4:5678`, `2001:db8::1` and `[2001:db8::1]:5678`.
fn parse_ip(s: &str) -> Option<IpAddr> {
    s.parse::<IpAddr>()
        .ok()
        .or_else(|| s.parse::<std::net::SocketAddr>().ok().map(|a| a.ip()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", v.parse().unwrap());
        }
        h
    }

    #[test]
    fn picks_the_entry_the_trusted_proxies_added() {
        let peer = Some("192.0.2.9".parse().unwrap());
        let h = xff(&["6.6.6.6, 1.2.3.4"]);
        assert_eq!(client_ip(&h, peer, 1), Some("1.2.3.4".parse().unwrap()));
        assert_eq!(client_ip(&h, peer, 2), Some("6.6.6.6".parse().unwrap()));
        // More hops than entries: nothing is provably proxy-added.
        assert_eq!(client_ip(&h, peer, 3), peer);
        assert_eq!(client_ip(&h, peer, 0), peer);
        assert_eq!(client_ip(&HeaderMap::new(), peer, 1), peer);
        let h = xff(&["6.6.6.6", "[2001:db8::1]:443"]);
        assert_eq!(client_ip(&h, peer, 1), Some("2001:db8::1".parse().unwrap()));
        assert_eq!(client_ip(&xff(&["garbage"]), peer, 1), peer);
    }

    #[test]
    fn buckets_are_per_client_and_ipv6_by_prefix() {
        let l = Limiter::new(
            RateLimit {
                per_minute: NonZeroU32::new(1).unwrap(),
                burst: NonZeroU32::new(2).unwrap(),
            },
            1,
        );
        let a = xff(&["1.1.1.1"]);
        assert!(l.check(&a, None).is_ok());
        assert!(l.check(&a, None).is_ok());
        let wait = l.check(&a, None).unwrap_err();
        assert!(wait > Duration::from_secs(1) && wait <= Duration::from_secs(60));
        assert!(l.check(&xff(&["2.2.2.2"]), None).is_ok());
        // A spoofed leftmost entry doesn't create a new bucket.
        assert!(l.check(&xff(&["9.9.9.9, 1.1.1.1"]), None).is_err());
        // Same /64.
        assert!(l.check(&xff(&["2001:db8::1"]), None).is_ok());
        assert!(l.check(&xff(&["2001:db8::2"]), None).is_ok());
        assert!(l.check(&xff(&["2001:db8::3"]), None).is_err());
        assert!(l.check(&xff(&["2001:db8:0:1::1"]), None).is_ok());
    }
}
