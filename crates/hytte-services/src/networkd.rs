//! Link state from systemd-networkd (`org.freedesktop.network1`).
//!
//! Polls the Manager's `ListLinks` once at startup, then queries each
//! link's properties. Subscribes to `Manager.PropertiesChanged` for
//! refresh signals. (networkd does not emit per-link `PropertiesChanged`
//! universally; a periodic re-poll is the robust path.)

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::time::Duration;
use zbus::Connection;

pub struct NetworkdService;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationalState {
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
pub struct Link {
    pub idx: i32,
    pub name: String,
    pub operational: OperationalState,
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

        out.push(Link {
            idx,
            name,
            operational: OperationalState::parse(&op_state),
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
