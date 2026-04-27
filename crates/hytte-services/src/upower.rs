//! Battery state via `UPower`.
//!
//! Subscribes to `org.freedesktop.UPower.Device` properties on
//! `/org/freedesktop/UPower/devices/DisplayDevice` (the aggregated battery —
//! one entry covering all batteries on the system).
//!
//! Each tracked field (`Percentage`, `State`, `IconName`, `TimeToEmpty`,
//! `TimeToFull`) gets its own [`hytte_bus::property`] subscription.  Changes
//! are coalesced into the shared [`Battery`] via parallel `for_each` tasks
//! that each update only their slice of the state (same pattern as
//! `power_profiles`).

use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{property, BusKind, PropState};
use hytte_reactive::{registry, Service};
use std::time::Duration;

const UPOWER_NAME: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";

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

    #[allow(clippy::too_many_lines)]
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = UpowerHandles::default();
        let writer = handles.battery.clone();

        // ── Percentage ────────────────────────────────────────────────────────
        let percentage_signal = property::<f64>(UPOWER_NAME)
            .bus(BusKind::System)
            .at_path(DISPLAY_DEVICE_PATH)
            .iface(DEVICE_IFACE)
            .name("Percentage")
            .start();

        // ── State ─────────────────────────────────────────────────────────────
        let state_signal = property::<u32>(UPOWER_NAME)
            .bus(BusKind::System)
            .at_path(DISPLAY_DEVICE_PATH)
            .iface(DEVICE_IFACE)
            .name("State")
            .start();

        // ── TimeToEmpty ───────────────────────────────────────────────────────
        let time_to_empty_signal = property::<i64>(UPOWER_NAME)
            .bus(BusKind::System)
            .at_path(DISPLAY_DEVICE_PATH)
            .iface(DEVICE_IFACE)
            .name("TimeToEmpty")
            .start();

        // ── TimeToFull ────────────────────────────────────────────────────────
        let time_to_full_signal = property::<i64>(UPOWER_NAME)
            .bus(BusKind::System)
            .at_path(DISPLAY_DEVICE_PATH)
            .iface(DEVICE_IFACE)
            .name("TimeToFull")
            .start();

        // ── IconName ──────────────────────────────────────────────────────────
        let icon_name_signal = property::<String>(UPOWER_NAME)
            .bus(BusKind::System)
            .at_path(DISPLAY_DEVICE_PATH)
            .iface(DEVICE_IFACE)
            .name("IconName")
            .start();

        // ── Coalesce into Battery ─────────────────────────────────────────────

        let percentage_writer = writer.clone();
        rt.spawn(async move {
            percentage_signal
                .signal()
                .for_each(move |s| {
                    let pct = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => 0.0,
                    };
                    percentage_writer.lock_mut().percentage = pct;
                    std::future::ready(())
                })
                .await;
        });

        let state_writer = writer.clone();
        rt.spawn(async move {
            state_signal
                .signal()
                .for_each(move |s| {
                    let raw = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => 0,
                    };
                    state_writer.lock_mut().state = BatteryState::from_u32(raw);
                    std::future::ready(())
                })
                .await;
        });

        let tte_writer = writer.clone();
        rt.spawn(async move {
            time_to_empty_signal
                .signal()
                .for_each(move |s| {
                    let secs = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => 0,
                    };
                    tte_writer.lock_mut().time_to_empty =
                        u64::try_from(secs).ok().filter(|&s| s > 0).map(Duration::from_secs);
                    std::future::ready(())
                })
                .await;
        });

        let ttf_writer = writer.clone();
        rt.spawn(async move {
            time_to_full_signal
                .signal()
                .for_each(move |s| {
                    let secs = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => 0,
                    };
                    ttf_writer.lock_mut().time_to_full =
                        u64::try_from(secs).ok().filter(|&s| s > 0).map(Duration::from_secs);
                    std::future::ready(())
                })
                .await;
        });

        let icon_writer = writer.clone();
        rt.spawn(async move {
            icon_name_signal
                .signal()
                .for_each(move |s| {
                    let name = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => String::new(),
                    };
                    icon_writer.lock_mut().icon_name = name;
                    std::future::ready(())
                })
                .await;
        });

        handles
    }
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
