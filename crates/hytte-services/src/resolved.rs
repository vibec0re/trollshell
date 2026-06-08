//! DNS state from systemd-resolved (`org.freedesktop.resolve1`).
//!
//! Reads the Manager's `DNS` property — a list of `(ifindex, family,
//! address)` tuples — and emits a `DnsState` summary. The underlying
//! property subscription lives in `hytte_bus::property`, so reconnects
//! and `PropertiesChanged` tracking are handled by the bus layer.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{BusKind, PropState, property};
use hytte_reactive::{Service, registry};
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

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = ResolvedHandles::default();
        let writer = handles.dns.clone();

        // DNS = a(iiay) — array of (ifindex i32, family i32, address bytes).
        let dns_property = property::<Vec<(i32, i32, Vec<u8>)>>("org.freedesktop.resolve1")
            .bus(BusKind::System)
            .at_path("/org/freedesktop/resolve1")
            .iface("org.freedesktop.resolve1.Manager")
            .name("DNS")
            .start();

        rt.spawn(async move {
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
                    writer.set(DnsState { servers });
                    std::future::ready(())
                })
                .await;
        });

        handles
    }
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
    use super::parse_addr;
    use std::net::IpAddr;

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
}
