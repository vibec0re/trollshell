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
use hytte_bus::{property, BusKind, PropState, PropertySignal};
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

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = UpowerHandles::default();
        let writer = handles.battery.clone();

        bind_prop_field(rt, display_device_prop::<f64>("Percentage"), 0.0, writer.clone(),
            |b, v| b.percentage = v);
        bind_prop_field(rt, display_device_prop::<u32>("State"), 0, writer.clone(),
            |b, v| b.state = BatteryState::from_u32(v));
        bind_prop_field(rt, display_device_prop::<i64>("TimeToEmpty"), 0, writer.clone(),
            |b, v| b.time_to_empty = secs_to_duration(v));
        bind_prop_field(rt, display_device_prop::<i64>("TimeToFull"), 0, writer.clone(),
            |b, v| b.time_to_full = secs_to_duration(v));
        bind_prop_field(rt, display_device_prop::<String>("IconName"), String::new(), writer,
            |b, v| b.icon_name = v);

        handles
    }
}

fn display_device_prop<T>(name: &'static str) -> PropertySignal<T>
where
    T: Clone + Send + Sync + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    property::<T>(UPOWER_NAME)
        .bus(BusKind::System)
        .at_path(DISPLAY_DEVICE_PATH)
        .iface(DEVICE_IFACE)
        .name(name)
        .start()
}

fn secs_to_duration(secs: i64) -> Option<Duration> {
    u64::try_from(secs).ok().filter(|&s| s > 0).map(Duration::from_secs)
}

fn bind_prop_field<T>(
    rt: &tokio::runtime::Handle,
    prop: PropertySignal<T>,
    default: T,
    writer: Mutable<Battery>,
    apply: impl Fn(&mut Battery, T) + Send + 'static,
) where
    T: Clone + Send + Sync + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    rt.spawn(async move {
        prop.signal()
            .for_each(move |s| {
                let v = match s {
                    PropState::Loaded(v) | PropState::Stale(v) => v,
                    PropState::Loading => default.clone(),
                };
                apply(&mut writer.lock_mut(), v);
                std::future::ready(())
            })
            .await;
    });
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
