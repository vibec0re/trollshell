//! Link state from systemd-networkd (`org.freedesktop.network1`).
//!
//! Polls the Manager's `ListLinks` once at startup and then whenever
//! `StateChanged` fires on the Manager, falling back to a 5-second timer so
//! newly-appeared links (hot-plug) never stall longer than 5 s.
//!
//! All D-Bus I/O goes through [`hytte_bus::call`] and [`hytte_bus::signals`]
//! so the shared connection supervisor handles reconnects automatically.

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::{BusKind, call, signals};
use hytte_reactive::{Service, registry};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

const NETWORKD_NAME: &str = "org.freedesktop.network1";
const MANAGER_PATH: &str = "/org/freedesktop/network1";
const MANAGER_IFACE: &str = "org.freedesktop.network1.Manager";
const LINK_IFACE: &str = "org.freedesktop.network1.Link";

pub struct NetworkdService;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OperationalState {
    #[default]
    Missing,
    Off,
    NoCarrier,
    Dormant,
    DegradedCarrier,
    Carrier,
    Degraded,
    EnslavedRouting,
    Routable,
    Unknown,
}

impl OperationalState {
    fn parse(s: &str) -> Self {
        match s {
            "missing" => Self::Missing,
            "off" => Self::Off,
            "no-carrier" => Self::NoCarrier,
            "dormant" => Self::Dormant,
            "degraded-carrier" => Self::DegradedCarrier,
            "carrier" => Self::Carrier,
            "degraded" => Self::Degraded,
            "enslaved" => Self::EnslavedRouting,
            "routable" => Self::Routable,
            _ => Self::Unknown,
        }
    }

    /// Coarse priority used to pick a "primary" link (highest wins).
    fn priority(self) -> u8 {
        match self {
            Self::Routable => 5,
            Self::Degraded => 4,
            Self::EnslavedRouting => 3,
            Self::Carrier | Self::DegradedCarrier => 2,
            Self::Dormant => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LinkAddress {
    pub addr: IpAddr,
    pub prefix_len: u8,
}

#[derive(Clone, Debug, Default)]
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
    pub addresses: Vec<LinkAddress>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

#[derive(Clone, Debug)]
pub struct RouteSummary {
    pub destination: IpAddr,
    pub prefix_len: u8,
    pub gateway: Option<IpAddr>,
    pub family: i32,
}

#[doc(hidden)]
pub struct NetworkdHandles {
    pub(crate) links: Mutable<Vec<Link>>,
    pub(crate) primary: Mutable<Option<Link>>,
}

impl Default for NetworkdHandles {
    fn default() -> Self {
        Self {
            links: Mutable::new(Vec::new()),
            primary: Mutable::new(None),
        }
    }
}

impl Service for NetworkdService {
    type Handles = NetworkdHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = NetworkdHandles::default();
        let links_writer = handles.links.clone();
        let primary_writer = handles.primary.clone();

        rt.spawn(async move {
            // Startup probe: if the initial refresh fails, networkd isn't
            // running on this host (e.g. NetworkManager-only setups). Log
            // once at info and stay inert rather than entering a 2s retry
            // loop that hammers dbus for the rest of the process lifetime.
            if let Err(e) = refresh(&links_writer, &primary_writer).await {
                tracing::info!(error = ?e, "networkd unreachable at startup; service inert");
                return;
            }
            loop {
                match listen(&links_writer, &primary_writer).await {
                    Ok(()) => tracing::warn!("networkd stream ended, retrying in 2s"),
                    Err(e) => tracing::warn!(error = ?e, "networkd error, retrying in 2s"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
    }
}

async fn listen(links_out: &Mutable<Vec<Link>>, primary_out: &Mutable<Option<Link>>) -> Result<()> {
    // Subscribe to StateChanged on the Manager so we react quickly to
    // link state transitions.  Missed-emissions on reconnect trigger a
    // re-poll too, so we never miss a change across a D-Bus restart.
    let state_changed = signals(NETWORKD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .signal("StateChanged")
        .start();

    let mut events = state_changed.events();

    // Initial poll.
    refresh(links_out, primary_out).await?;

    // 5-second fallback timer — catches hot-plug when StateChanged is
    // not emitted (e.g. older networkd, or newly added links).
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Discard the immediate first tick (we already polled above).
    interval.tick().await;

    loop {
        tokio::select! {
            _ = events.next() => {
                tracing::debug!("networkd StateChanged; refreshing links");
                if let Err(e) = refresh(links_out, primary_out).await {
                    tracing::warn!(error = ?e, "networkd refresh after StateChanged failed");
                }
            }
            _ = interval.tick() => {
                if let Err(e) = refresh(links_out, primary_out).await {
                    tracing::warn!(error = ?e, "networkd periodic refresh failed");
                }
            }
        }
    }
}

async fn refresh(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
) -> Result<()> {
    let links = read_links().await?;
    let primary = links
        .iter()
        .max_by_key(|l| l.operational.priority())
        .filter(|l| l.operational.priority() > 0)
        .cloned();

    links_out.set(links);
    primary_out.set(primary);
    Ok(())
}

async fn read_links() -> Result<Vec<Link>> {
    // ListLinks returns array of (idx: i32, name: String, path: ObjectPath).
    let list: Vec<(i32, String, zbus::zvariant::OwnedObjectPath)> = call(NETWORKD_NAME)
        .bus(BusKind::System)
        .at_path(MANAGER_PATH)
        .iface(MANAGER_IFACE)
        .method("ListLinks")
        .args(())
        .send()
        .await
        .context("ListLinks")?;

    let mut out = Vec::with_capacity(list.len());
    for (idx, name, path) in list {
        let path_str = path.as_str().to_string();

        let describe_json: String = call(NETWORKD_NAME)
            .bus(BusKind::System)
            .at_path(path_str.clone())
            .iface(LINK_IFACE)
            .method("Describe")
            .args(())
            .send()
            .await
            .unwrap_or_default();

        // OperationalState is also in the Describe JSON, but older networkd
        // only exposes it as a property.  Read it directly so we always have it.
        let op_prop: String = call(NETWORKD_NAME)
            .bus(BusKind::System)
            .at_path(path_str.clone())
            .iface("org.freedesktop.DBus.Properties")
            .method("Get")
            .args((LINK_IFACE, "OperationalState"))
            .send::<zbus::zvariant::OwnedValue>()
            .await
            .ok()
            .and_then(|v| String::try_from(v).ok())
            .unwrap_or_default();

        // The `Describe` method returns a JSON blob; parse addresses & routes.
        let parsed = parse_describe(&describe_json).unwrap_or_default();

        out.push(Link {
            idx,
            name,
            operational: OperationalState::parse(&op_prop),
            addresses: parsed.addresses,
            gateway_v4: parsed.gateway_v4,
            gateway_v6: parsed.gateway_v6,
            routes: parsed.routes,
        });
    }
    Ok(out)
}

#[must_use]
pub fn service() -> NetworkdService {
    NetworkdService
}

pub fn links() -> impl Signal<Item = Vec<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .links
            .signal_cloned()
    })
}

pub fn primary() -> impl Signal<Item = Option<Link>> {
    registry::with(|r| {
        r.get::<NetworkdHandles>()
            .expect("networkd::service() not registered")
            .primary
            .signal_cloned()
    })
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeLink {
    addresses: Vec<DescribeAddress>,
    route_data: Vec<DescribeRoute>,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeAddress {
    family: i32,
    address: Vec<u8>,
    prefix_length: u8,
}

#[derive(Default, serde::Deserialize)]
#[serde(default, rename_all = "PascalCase")]
struct DescribeRoute {
    family: i32,
    destination: Vec<u8>,
    destination_prefix_length: u8,
    gateway: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub(crate) struct ParsedDescribe {
    pub addresses: Vec<LinkAddress>,
    pub gateway_v4: Option<Ipv4Addr>,
    pub gateway_v6: Option<Ipv6Addr>,
    pub routes: Vec<RouteSummary>,
}

pub(crate) fn parse_describe(json: &str) -> anyhow::Result<ParsedDescribe> {
    let raw: DescribeLink = serde_json::from_str(json).context("parse Describe JSON")?;
    let mut out = ParsedDescribe::default();

    for a in raw.addresses {
        if let Some(addr) = bytes_to_ip(a.family, &a.address) {
            out.addresses.push(LinkAddress {
                addr,
                prefix_len: a.prefix_length,
            });
        }
    }

    for r in raw.route_data {
        let Some(dest) = bytes_to_ip(r.family, &r.destination) else {
            continue;
        };
        let gw = r.gateway.as_ref().and_then(|g| bytes_to_ip(r.family, g));
        let is_default = r.destination_prefix_length == 0
            && match dest {
                IpAddr::V4(v4) => v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_unspecified(),
            };
        if is_default {
            if let Some(IpAddr::V4(g4)) = gw {
                out.gateway_v4 = Some(g4);
            } else if let Some(IpAddr::V6(g6)) = gw {
                out.gateway_v6 = Some(g6);
            }
        }
        out.routes.push(RouteSummary {
            destination: dest,
            prefix_len: r.destination_prefix_length,
            gateway: gw,
            family: r.family,
        });
    }

    Ok(out)
}

fn bytes_to_ip(family: i32, bytes: &[u8]) -> Option<IpAddr> {
    match (family, bytes.len()) {
        (2, 4) => Some(IpAddr::V4(Ipv4Addr::new(
            bytes[0], bytes[1], bytes[2], bytes[3],
        ))),
        (10, 16) => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(bytes);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_DESCRIBE: &str = r#"{
        "Index": 3,
        "Name": "wlp1s0",
        "OperationalState": "routable",
        "Addresses": [
            {"Family": 2, "Address": [192, 168, 1, 42], "PrefixLength": 24}
        ],
        "RouteData": [
            {
                "Family": 2,
                "Destination": [0, 0, 0, 0],
                "DestinationPrefixLength": 0,
                "Gateway": [192, 168, 1, 1]
            },
            {
                "Family": 2,
                "Destination": [192, 168, 1, 0],
                "DestinationPrefixLength": 24
            }
        ]
    }"#;

    #[test]
    fn parses_describe_json_minimal() {
        let parsed = parse_describe(SAMPLE_DESCRIBE).expect("parse");
        assert_eq!(parsed.addresses.len(), 1);
        assert_eq!(
            parsed.addresses[0].addr,
            IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42))
        );
        assert_eq!(parsed.addresses[0].prefix_len, 24);
        assert_eq!(
            parsed.gateway_v4,
            Some(std::net::Ipv4Addr::new(192, 168, 1, 1))
        );
        assert_eq!(parsed.gateway_v6, None);
        assert_eq!(parsed.routes.len(), 2);
    }

    #[test]
    fn handles_unknown_fields() {
        let json = r#"{
            "Index": 1,
            "FutureField": "anything",
            "Addresses": [{"Family": 2, "Address": [10, 0, 0, 1], "PrefixLength": 8, "ExtraJunk": 99}]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(parsed.addresses.len(), 1);
    }

    #[test]
    fn default_route_populates_gateway_v4() {
        let json = r#"{
            "RouteData": [
                {"Family": 2, "Destination": [10, 0, 0, 0], "DestinationPrefixLength": 8, "Gateway": [10, 0, 0, 1]},
                {"Family": 2, "Destination": [0, 0, 0, 0], "DestinationPrefixLength": 0, "Gateway": [192, 168, 0, 1]}
            ]
        }"#;
        let parsed = parse_describe(json).expect("parse");
        assert_eq!(
            parsed.gateway_v4,
            Some(std::net::Ipv4Addr::new(192, 168, 0, 1))
        );
    }
}
