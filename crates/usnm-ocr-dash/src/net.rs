//! Who may ask: `--allow` and `--host-push` take address ranges, host
//! names (resolved at start and every few minutes) or `lan`, the subnets
//! this machine is on. No address is written into the repository.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::str::FromStr;
use std::sync::RwLock;

/// An address range: `198.51.100.0/24`, or one address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl FromStr for Cidr {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let (addr, bits) = match s.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (s, None),
        };
        let net: IpAddr = addr.parse().map_err(|_| format!("{s}: not an address"))?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = match bits {
            Some(b) => b
                .parse::<u8>()
                .ok()
                .filter(|b| *b <= max)
                .ok_or_else(|| format!("{s}: the prefix must be 0 to {max}"))?,
            None => max,
        };
        let c = Cidr { net, bits };
        if c.mask(net) != net {
            return Err(format!("{s}: host bits set"));
        }
        Ok(c)
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.net, self.bits)
    }
}

impl Cidr {
    /// One address.
    pub fn host(ip: IpAddr) -> Self {
        let ip = ip.to_canonical();
        Cidr {
            net: ip,
            bits: if ip.is_ipv4() { 32 } else { 128 },
        }
    }

    fn mask(&self, ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(v) => {
                let m = u32::MAX.checked_shl(32 - u32::from(self.bits)).unwrap_or(0);
                IpAddr::V4((u32::from(v) & m).into())
            }
            IpAddr::V6(v) => {
                let m = u128::MAX
                    .checked_shl(128 - u32::from(self.bits))
                    .unwrap_or(0);
                IpAddr::V6((u128::from(v) & m).into())
            }
        }
    }

    /// Whether `ip` (an IPv4-mapped IPv6 address counts as IPv4) is in
    /// the range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        ip.is_ipv4() == self.net.is_ipv4() && self.mask(ip) == self.net
    }
}

/// The IPv4 subnets this machine is directly on, from `/proc/net/route`'s
/// text: routes with a destination and no gateway.
pub fn lan_subnets(proc_net_route: &str) -> Vec<Cidr> {
    let hex = |s: &str| u32::from_str_radix(s, 16).ok();
    proc_net_route
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (dest, gw, mask) = (hex(f.get(1)?)?, hex(f.get(2)?)?, hex(f.get(7)?)?);
            if dest == 0 || gw != 0 {
                return None;
            }
            // The kernel writes them in host (little-endian) order.
            let net = Ipv4Addr::from(dest.to_le_bytes());
            let bits = mask.count_ones() as u8;
            format!("{net}/{bits}").parse().ok()
        })
        .collect()
}

/// One `--allow` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Range(Cidr),
    /// A host name, resolved to its addresses.
    Name(String),
    /// The subnets this machine is on.
    Lan,
}

impl FromStr for Source {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        if s == "lan" {
            return Ok(Source::Lan);
        }
        if let Ok(c) = s.parse::<Cidr>() {
            return Ok(Source::Range(c));
        }
        let name_ok = !s.is_empty()
            && s.len() <= 253
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            && s.bytes().any(|b| b.is_ascii_alphabetic());
        if name_ok {
            Ok(Source::Name(s.to_ascii_lowercase()))
        } else {
            Err(format!("{s}: not an address range, a host name or `lan`"))
        }
    }
}

/// Address ranges from [`Source`]s, the names and `lan` worked out again by
/// [`Allow::refresh`].
#[derive(Debug, Default)]
pub struct Allow {
    sources: Vec<Source>,
    ranges: RwLock<Vec<Cidr>>,
    /// Each name's addresses when it last resolved.
    last: RwLock<HashMap<String, Vec<Cidr>>>,
}

impl Allow {
    pub fn new(sources: Vec<Source>) -> Self {
        let ranges = sources
            .iter()
            .filter_map(|s| match s {
                Source::Range(c) => Some(*c),
                _ => None,
            })
            .collect();
        Allow {
            sources,
            ranges: RwLock::new(ranges),
            last: RwLock::new(HashMap::new()),
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.ranges
            .read()
            .expect("allow lock")
            .iter()
            .any(|c| c.contains(ip))
    }

    pub fn ranges(&self) -> Vec<Cidr> {
        self.ranges.read().expect("allow lock").clone()
    }

    /// Resolve the names and read the subnets again. A name that doesn't
    /// resolve now keeps the addresses it had.
    pub async fn refresh(&self) {
        let old = self.ranges();
        let mut new = Vec::new();
        for s in &self.sources {
            match s {
                Source::Range(c) => new.push(*c),
                Source::Lan => match tokio::fs::read_to_string("/proc/net/route").await {
                    Ok(t) => new.extend(lan_subnets(&t)),
                    Err(e) => tracing::warn!(error = %e, "can't read /proc/net/route for `lan`"),
                },
                Source::Name(n) => match tokio::net::lookup_host((n.as_str(), 0)).await {
                    Ok(addrs) => {
                        let got: Vec<Cidr> = addrs.map(|a| Cidr::host(a.ip())).collect();
                        new.extend(got.iter().copied());
                        self.last
                            .write()
                            .expect("allow lock")
                            .insert(n.clone(), got);
                    }
                    Err(e) => {
                        tracing::warn!(name = n, error = %e, "can't resolve; keeping its last addresses");
                        if let Some(got) = self.last.read().expect("allow lock").get(n) {
                            new.extend(got.iter().copied());
                        }
                    }
                },
            }
        }
        new.sort_by_key(|c| c.to_string());
        new.dedup();
        if new != old {
            tracing::info!(ranges = ?new.iter().map(Cidr::to_string).collect::<Vec<_>>(), "allowed addresses");
        }
        *self.ranges.write().expect("allow lock") = new;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let net: Cidr = "198.51.100.0/24".parse().unwrap();
        assert!(net.contains("198.51.100.243".parse().unwrap()));
        assert!(net.contains("::ffff:198.51.100.12".parse().unwrap()));
        assert!(!net.contains("198.51.101.5".parse().unwrap()));
        assert!(!net.contains("203.0.113.1".parse().unwrap()));
        let one: Cidr = "198.51.100.12".parse().unwrap();
        assert!(one.contains("198.51.100.12".parse().unwrap()));
        assert!(!one.contains("198.51.100.13".parse().unwrap()));
        assert!("198.51.100.1/24".parse::<Cidr>().is_err());
        assert!("198.51.100.0/33".parse::<Cidr>().is_err());
        assert!("lan".parse::<Cidr>().is_err());
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains("8.8.8.8".parse().unwrap()));
        assert!(!all.contains("::1".parse().unwrap()));
    }

    #[test]
    fn reads_the_subnets_this_machine_is_on() {
        // 192.0.2.0/24 on eth0 and a default route through 192.0.2.1, as
        // the kernel writes them.
        let route =
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
                     eth0\t00000000\t010200C0\t0003\t0\t0\t0\t00000000\t0\t0\t0\n\
                     eth0\t000200C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n";
        assert_eq!(
            lan_subnets(route),
            vec!["192.0.2.0/24".parse::<Cidr>().unwrap()]
        );
    }

    #[test]
    fn sources_and_names() {
        assert_eq!("lan".parse::<Source>().unwrap(), Source::Lan);
        assert_eq!(
            "pve.example.org".parse::<Source>().unwrap(),
            Source::Name("pve.example.org".into())
        );
        assert!(matches!(
            "203.0.113.0/24".parse::<Source>().unwrap(),
            Source::Range(_)
        ));
        assert!("not a name!".parse::<Source>().is_err());
    }

    #[tokio::test]
    async fn a_name_resolves_to_its_addresses() {
        let a = Allow::new(vec![
            Source::Name("localhost".into()),
            "203.0.113.0/24".parse().unwrap(),
        ]);
        assert!(!a.contains("127.0.0.1".parse().unwrap()));
        a.refresh().await;
        assert!(a.contains("127.0.0.1".parse().unwrap()));
        assert!(a.contains("203.0.113.9".parse().unwrap()));
    }
}
