//! Trusted-proxy CIDR list (std only).

use std::net::IpAddr;

/// A set of CIDR ranges (`10.0.0.0/8`, `fd00::/8`, a bare address means a
/// single host) naming peers whose forwarding headers may be believed.
///
/// IPv4 clients, including IPv4-mapped IPv6 peers (`::ffff:a.b.c.d`), are
/// matched only by IPv4 ranges, so `::/0` does not trust IPv4 clients (use
/// `0.0.0.0/0` for those). Host bits are ignored (`10.0.0.1/8` equals
/// `10.0.0.0/8`). An IPv4-mapped IPv6 range is only accepted with a prefix
/// of at least 96 (`::ffff:10.0.0.0/104` means `10.0.0.0/8`); shorter
/// prefixes are rejected as ambiguous.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    nets: Vec<(IpAddr, u8)>,
}

fn normalise(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

fn bits(ip: IpAddr) -> (u128, u8) {
    match ip {
        IpAddr::V4(v4) => (u32::from(v4) as u128, 32),
        IpAddr::V6(v6) => (u128::from(v6), 128),
    }
}

impl TrustedProxies {
    /// Parse CIDR strings. Errors name the offending entry.
    pub fn parse<S: AsRef<str>>(cidrs: &[S]) -> Result<Self, crate::Error> {
        let mut nets = Vec::with_capacity(cidrs.len());
        for raw in cidrs {
            let raw = raw.as_ref();
            let bad = |why: &str| crate::Error::InvalidCidr {
                entry: raw.to_string(),
                reason: why.to_string(),
            };
            let (addr, prefix) = match raw.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (raw, None),
            };
            let ip: IpAddr = addr.parse().map_err(|_| bad("not an IP address"))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let len = match prefix {
                None => max,
                Some(p) => Some(p)
                    .filter(|p| p.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|p| p.parse::<u8>().ok())
                    .filter(|l| *l <= max)
                    .ok_or_else(|| bad(&format!("prefix length must be 0..={max}")))?,
            };
            // `::ffff:a.b.c.d/N` (N >= 96) is the IPv4 range a.b.c.d/(N-96).
            let net = match (normalise(ip), ip) {
                (IpAddr::V4(v4), IpAddr::V6(_)) if len >= 96 => (IpAddr::V4(v4), len - 96),
                (IpAddr::V4(_), IpAddr::V6(_)) => {
                    return Err(bad("IPv4-mapped IPv6 range needs prefix length >= 96"))
                }
                _ => (ip, len),
            };
            nets.push(net);
        }
        Ok(Self { nets })
    }

    /// True when no ranges are configured.
    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }

    /// Does any configured range contain `ip`?
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = normalise(ip);
        let (addr, width) = bits(ip);
        self.nets.iter().any(|&(net, len)| {
            let (naddr, nwidth) = bits(net);
            if nwidth != width {
                return false;
            }
            let mask = if len == 0 { 0 } else { !0u128 << (width - len) };
            (addr ^ naddr) & mask & (u128::MAX >> (128 - width)) == 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(s: &[&str]) -> TrustedProxies {
        TrustedProxies::parse(s).unwrap()
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v4_ranges() {
        let t = tp(&["10.0.0.0/8", "192.168.1.7/32"]);
        assert!(t.contains(ip("10.255.0.1")));
        assert!(!t.contains(ip("11.0.0.1")));
        assert!(t.contains(ip("192.168.1.7")));
        assert!(!t.contains(ip("192.168.1.8")));
    }

    #[test]
    fn v6_ranges() {
        let t = tp(&["fd00::/8", "::1/128"]);
        assert!(t.contains(ip("fdab::1")));
        assert!(!t.contains(ip("fe80::1")));
        assert!(t.contains(ip("::1")));
        assert!(!t.contains(ip("::2")));
    }

    #[test]
    fn v4_mapped_peer_matches_v4_range() {
        let t = tp(&["10.0.0.0/8"]);
        assert!(t.contains(ip("::ffff:10.1.2.3")));
        assert!(!t.contains(ip("::ffff:11.1.2.3")));
        // And a mapped CIDR matches plain v4 peers.
        let t = tp(&["::ffff:10.0.0.0/104"]);
        assert!(t.contains(ip("10.9.9.9")));
        assert!(!t.contains(ip("11.9.9.9")));
    }

    #[test]
    fn slash_zero_matches_own_family_only() {
        let t = tp(&["0.0.0.0/0"]);
        assert!(t.contains(ip("8.8.8.8")));
        assert!(t.contains(ip("::ffff:8.8.8.8")));
        assert!(!t.contains(ip("2001:db8::1")));
        let t = tp(&["::/0"]);
        assert!(t.contains(ip("2001:db8::1")));
        assert!(!t.contains(ip("8.8.8.8")));
    }

    #[test]
    fn bare_address_is_single_host() {
        let t = tp(&["10.0.0.1", "fd00::1"]);
        assert!(t.contains(ip("10.0.0.1")));
        assert!(!t.contains(ip("10.0.0.2")));
        assert!(t.contains(ip("fd00::1")));
        assert!(!t.contains(ip("fd00::2")));
    }

    #[test]
    fn host_bits_ignored_and_v6_zero_excludes_v4() {
        assert!(tp(&["10.0.0.1/8"]).contains(ip("10.9.9.9")));
        assert!(!tp(&["::/0"]).contains(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn empty_trusts_nobody() {
        let t = TrustedProxies::default();
        assert!(t.is_empty());
        assert!(!t.contains(ip("127.0.0.1")));
    }

    #[test]
    fn rejects_bad_entries() {
        for bad in [
            "10.0.0.0/33",
            "::/129",
            "10.0.0.0/-1",
            "10.0.0.0/",
            "10.0.0.0/abc",
            "10.0.0.0/256",
            "nonsense",
            "",
            "10.0.0/8",
            "10.0.0.0/8/8",
            "example.com",
        ] {
            let err = TrustedProxies::parse(&[bad]).unwrap_err().to_string();
            assert!(
                err.contains("trusted_proxies") && err.contains(bad),
                "{err}"
            );
        }
    }
}
