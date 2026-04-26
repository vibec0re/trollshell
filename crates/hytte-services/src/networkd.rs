//! Link state from systemd-networkd (`org.freedesktop.network1`).
//!
//! Polls the Manager's `ListLinks` once at startup, then queries each
//! link's properties. Subscribes to `Manager.PropertiesChanged` for
//! refresh signals. (networkd does not emit per-link `PropertiesChanged`
//! universally; a periodic re-poll is the robust path.)

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;
use zbus::Connection;

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
            loop {
                match listen(&links_writer, &primary_writer).await {
                    Ok(()) => tracing::warn!("networkd stream ended, retrying in 2s"),
                    Err(e) => tracing::warn!(error = %e, "networkd error, retrying in 2s"),
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });

        handles
    }
}

async fn listen(
    links_out: &Mutable<Vec<Link>>,
    primary_out: &Mutable<Option<Link>>,
) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;

    loop {
        let links = read_links(&conn).await?;
        let primary = links
            .iter()
            .max_by_key(|l| l.operational.priority())
            .filter(|l| l.operational.priority() > 0)
            .cloned();

        links_out.set(links);
        primary_out.set(primary);

        // Re-poll every 2 seconds. Cheap; networkd has no global property
        // change signal we can listen for portably.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn read_links(conn: &Connection) -> Result<Vec<Link>> {
    let manager = zbus::Proxy::new(
        conn,
        "org.freedesktop.network1",
        "/org/freedesktop/network1",
        "org.freedesktop.network1.Manager",
    )
    .await
    .context("create networkd Manager proxy")?;

    // ListLinks returns array of (idx: i32, name: String, path: ObjectPath).
    let list: Vec<(i32, String, zbus::zvariant::OwnedObjectPath)> =
        manager.call("ListLinks", &()).await.context("ListLinks")?;

    let mut out = Vec::with_capacity(list.len());
    for (idx, name, path) in list {
        let link_proxy = zbus::Proxy::new(
            conn,
            "org.freedesktop.network1",
            path.as_str(),
            "org.freedesktop.network1.Link",
        )
        .await
        .context("create Link proxy")?;

        let op_state: String = link_proxy
            .get_property("OperationalState")
            .await
            .unwrap_or_default();

        let describe_json: String = link_proxy
            .call("Describe", &())
            .await
            .unwrap_or_default();
        let parsed = parse_describe(&describe_json).unwrap_or_default();

        out.push(Link {
            idx,
            name,
            operational: OperationalState::parse(&op_state),
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
        let Some(dest) = bytes_to_ip(r.family, &r.destination) else { continue };
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
        (2, 4) => Some(IpAddr::V4(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))),
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
        assert_eq!(parsed.addresses[0].addr, IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 42)));
        assert_eq!(parsed.addresses[0].prefix_len, 24);
        assert_eq!(parsed.gateway_v4, Some(std::net::Ipv4Addr::new(192, 168, 1, 1)));
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
        assert_eq!(parsed.gateway_v4, Some(std::net::Ipv4Addr::new(192, 168, 0, 1)));
    }
}
