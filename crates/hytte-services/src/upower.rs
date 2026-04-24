//! Battery state via `UPower`.
//!
//! Subscribes to `org.freedesktop.UPower.Device.PropertiesChanged` on the
//! `/org/freedesktop/UPower/devices/DisplayDevice` path of the `UPower`
//! daemon (the aggregated battery — one entry covering all batteries on
//! the system).

use anyhow::{anyhow, Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_reactive::{registry, Service};
use std::time::Duration;
use zbus::Connection;

pub struct UpowerService;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BatteryState {
    Unknown,
    Charging,
    Discharging,
    Empty,
    FullyCharged,
    PendingCharge,
    PendingDischarge,
}

impl BatteryState {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::FullyCharged,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Battery {
    /// Charge percentage, `0.0..=100.0`.
    pub percentage: f64,
    /// Charge/discharge state.
    pub state: BatteryState,
    /// Approximate seconds until empty (when discharging).
    pub time_to_empty: Option<Duration>,
    /// Approximate seconds until full (when charging).
    pub time_to_full: Option<Duration>,
    /// Free-form icon name from `UPower` (e.g. `"battery-good-symbolic"`).
    pub icon_name: String,
}

impl Default for Battery {
    fn default() -> Self {
        Self {
            percentage: 0.0,
            state: BatteryState::Unknown,
            time_to_empty: None,
            time_to_full: None,
            icon_name: String::new(),
        }
    }
}

#[doc(hidden)]
pub struct UpowerHandles {
    pub(crate) battery: Mutable<Battery>,
}

impl Default for UpowerHandles {
    fn default() -> Self {
        Self {
            battery: Mutable::new(Battery::default()),
        }
    }
}

impl Service for UpowerService {
    type Handles = UpowerHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = UpowerHandles::default();
        let writer = handles.battery.clone();

        rt.spawn(async move {
            loop {
                match listen(&writer).await {
                    Ok(()) => tracing::warn!("upower stream closed, reconnecting in 1s"),
                    Err(e) => tracing::warn!(error = %e, "upower error, reconnecting in 1s"),
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        handles
    }
}

async fn listen(battery: &Mutable<Battery>) -> Result<()> {
    let conn = Connection::system().await.context("connect system bus")?;

    // Read all properties of the DisplayDevice.
    let read = || async {
        let proxy = zbus::Proxy::new(
            &conn,
            "org.freedesktop.UPower",
            "/org/freedesktop/UPower/devices/DisplayDevice",
            "org.freedesktop.UPower.Device",
        )
        .await
        .context("create DisplayDevice proxy")?;

        let percentage: f64 = proxy.get_property("Percentage").await.unwrap_or(0.0);
        let state: u32 = proxy.get_property("State").await.unwrap_or(0);
        let time_to_empty: i64 = proxy.get_property("TimeToEmpty").await.unwrap_or(0);
        let time_to_full: i64 = proxy.get_property("TimeToFull").await.unwrap_or(0);
        let icon_name: String = proxy.get_property("IconName").await.unwrap_or_default();

        Ok::<Battery, anyhow::Error>(Battery {
            percentage,
            state: BatteryState::from_u32(state),
            time_to_empty: u64::try_from(time_to_empty).ok().map(Duration::from_secs),
            time_to_full: u64::try_from(time_to_full).ok().map(Duration::from_secs),
            icon_name,
        })
    };

    // Initial state.
    battery.set(read().await?);

    // Subscribe to PropertiesChanged.
    let proxy = zbus::fdo::PropertiesProxy::builder(&conn)
        .destination("org.freedesktop.UPower")
        .map_err(|e| anyhow!("set destination: {e}"))?
        .path("/org/freedesktop/UPower/devices/DisplayDevice")
        .map_err(|e| anyhow!("set path: {e}"))?
        .build()
        .await
        .context("build properties proxy")?;

    let mut changes = proxy.receive_properties_changed().await?;
    while changes.next().await.is_some() {
        battery.set(read().await?);
    }
    Ok(())
}

#[must_use]
pub fn service() -> UpowerService {
    UpowerService
}

pub fn battery() -> impl Signal<Item = Battery> {
    registry::with(|r| {
        r.get::<UpowerHandles>()
            .expect("upower::service() not registered")
            .battery
            .signal_cloned()
    })
}
