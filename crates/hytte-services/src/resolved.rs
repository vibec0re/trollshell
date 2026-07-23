//! DNS state from systemd-resolved (`org.freedesktop.resolve1`).
//!
//! Reads the Manager's `DNS` property — a list of `(ifindex, family,
//! address)` tuples — and emits a `DnsState` summary. The underlying
//! property subscription lives in `hytte_bus::property`, so reconnects
//! and `PropertiesChanged` tracking are handled by the bus layer.
//!
//! When the `DNS` property is empty (e.g. resolved is not managing DNS, or
//! the system uses a stub resolver configured outside resolved), the service
//! falls back to parsing `nameserver` lines from `/etc/resolv.conf`.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{BusKind, PropState, property};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::net::IpAddr;

pub struct ResolvedService;

#[derive(Clone, Debug, Default)]
pub struct DnsState {
    pub servers: Vec<IpAddr>,
}

impl DnsState {
    #[must_use]
    pub fn configured(&self) -> bool {
        !self.servers.is_empty()
    }
}

#[doc(hidden)]
pub struct ResolvedHandles {
    pub(crate) dns: Mutable<DnsState>,
}

impl Default for ResolvedHandles {
    fn default() -> Self {
        Self {
            dns: Mutable::new(DnsState::default()),
        }
    }
}

impl Service for ResolvedService {
    type Handles = ResolvedHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ResolvedHandles::default();
        let writer = handles.dns.clone();

        // DNS = a(iiay) — array of (ifindex i32, family i32, address bytes).
        let dns_property = property::<Vec<(i32, i32, Vec<u8>)>>("org.freedesktop.resolve1")
            .bus(BusKind::System)
            .at_path("/org/freedesktop/resolve1")
            .iface("org.freedesktop.resolve1.Manager")
            .name("DNS")
            .start();

        spawn_supervised("resolved", move || {
            let dns_property = dns_property.clone();
            let writer = writer.clone();
            async move {
                dns_property
                .signal()
                .for_each(move |state| {
                    let raw = match state {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => Vec::new(),
                    };
                    let mut servers: Vec<IpAddr> = Vec::with_capacity(raw.len());
                    for (_idx, family, bytes) in raw {
                        if let Some(ip) = parse_addr(family, &bytes) {
                            servers.push(ip);
                        }
                    }
                    servers.sort();
                    servers.dedup();

                    // When resolved has no DNS servers configured (e.g. a
                    // stub-resolver or non-resolved setup), fall back to
                    // /etc/resolv.conf so the panel still shows something
                    // useful.
                    if servers.is_empty() {
                        servers = parse_resolv_conf(
                            &std::fs::read_to_string("/etc/resolv.conf")
                                .inspect_err(|e| tracing::warn!(error = %e, "resolved: /etc/resolv.conf read failed; DNS list will be empty"))
                                .unwrap_or_default(),
                        );
                        if !servers.is_empty() {
                            tracing::debug!(
                                "resolved DNS property empty; using /etc/resolv.conf fallback"
                            );
                        }
                    }

                    writer.set(DnsState { servers });
                    std::future::ready(())
                })
                .await;
            }
        });

        handles
    }
}

/// Parse `nameserver` lines from `/etc/resolv.conf` content.
///
/// Lines starting with `#` or `;` are comments and are ignored, as are
/// `search`, `domain`, and `options` directives.  Only `nameserver <ip>`
/// lines are collected.  Invalid IP addresses are silently skipped.
pub(crate) fn parse_resolv_conf(content: &str) -> Vec<IpAddr> {
    let mut servers = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let mut parts = line.split_ascii_whitespace();
        if parts.next() != Some("nameserver") {
            continue;
        }
        if let Some(addr_str) = parts.next()
            && let Ok(ip) = addr_str.parse::<IpAddr>()
        {
            servers.push(ip);
        }
    }
    servers.sort();
    servers.dedup();
    servers
}

fn parse_addr(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    // AF_INET = 2, AF_INET6 = 10 on Linux.
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(std::net::Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(std::net::Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[must_use]
pub fn service() -> ResolvedService {
    ResolvedService
}

pub fn dns() -> impl Signal<Item = DnsState> {
    registry::with(|r| {
        r.get::<ResolvedHandles>()
            .expect("resolved::service() not registered")
            .dns
            .signal_cloned()
    })
}

#[cfg(test)]
mod tests {
    use super::{parse_addr, parse_resolv_conf};
    use std::net::IpAddr;

    // --- parse_addr (existing) ---

    #[test]
    fn parses_ipv4() {
        let ip = parse_addr(2, &[1, 1, 1, 1]).unwrap();
        assert_eq!(ip, IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)));
    }

    #[test]
    fn parses_ipv6() {
        let bytes = [
            0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0x88,
        ];
        let ip = parse_addr(10, &bytes).unwrap();
        assert_eq!(ip, IpAddr::V6("2001:4860:4860::8888".parse().unwrap()));
    }

    #[test]
    fn rejects_unknown_family() {
        assert!(parse_addr(99, &[1, 2, 3, 4]).is_none());
    }

    // --- parse_resolv_conf ---

    #[test]
    fn resolv_conf_basic_nameservers() {
        let content = "nameserver 1.1.1.1\nnameserver 8.8.8.8\n";
        let servers = parse_resolv_conf(content);
        assert_eq!(servers.len(), 2);
        assert!(servers.contains(&"1.1.1.1".parse().unwrap()));
        assert!(servers.contains(&"8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn resolv_conf_ignores_comments_and_search() {
        let content = "\
# Generated by NetworkManager
; another comment style
search example.com local
domain example.com
options ndots:5
nameserver 192.168.1.1
nameserver 192.168.1.2
";
        let servers = parse_resolv_conf(content);
        assert_eq!(servers.len(), 2);
        assert!(servers.contains(&"192.168.1.1".parse::<IpAddr>().unwrap()));
        assert!(servers.contains(&"192.168.1.2".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn resolv_conf_deduplicates() {
        let content = "nameserver 1.1.1.1\nnameserver 1.1.1.1\n";
        let servers = parse_resolv_conf(content);
        assert_eq!(servers.len(), 1);
    }

    #[test]
    fn resolv_conf_empty_string_returns_empty() {
        assert!(parse_resolv_conf("").is_empty());
    }

    #[test]
    fn resolv_conf_ignores_invalid_ips() {
        let content = "nameserver not-an-ip\nnameserver 8.8.4.4\n";
        let servers = parse_resolv_conf(content);
        assert_eq!(servers.len(), 1);
        assert!(servers.contains(&"8.8.4.4".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn resolv_conf_parses_ipv6_nameserver() {
        let content = "nameserver 2001:4860:4860::8888\n";
        let servers = parse_resolv_conf(content);
        assert_eq!(servers.len(), 1);
        assert!(servers.contains(&"2001:4860:4860::8888".parse::<IpAddr>().unwrap()));
    }
}
