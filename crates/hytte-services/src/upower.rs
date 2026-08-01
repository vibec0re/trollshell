//! Battery state via `UPower`.
//!
//! Subscribes to `org.freedesktop.UPower.Device` properties on
//! `/org/freedesktop/UPower/devices/DisplayDevice` (the aggregated battery —
//! one entry covering all batteries on the system).
//!
//! Each tracked field (`Percentage`, `State`, `IconName`, `TimeToEmpty`,
//! `TimeToFull`, `WarningLevel`) gets its own [`hytte_bus::property`]
//! subscription.  Changes are coalesced into the shared [`Battery`] via
//! parallel `for_each` tasks that each update only their slice of the state
//! (same pattern as `power_profiles`).
//!
//! `WarningLevel` additionally drives a self-toast: [`spawn_warning_level_watcher`]
//! watches for a rising severity edge (entering `Low` or `Critical`/`Action`)
//! and posts via [`crate::notifications::post_local`] — see [`warning_toast`]
//! for the crossing/dedup rules (#237). [`is_critical`] exposes that same
//! `Critical`/`Action` severity split so other `WarningLevel` consumers (the
//! `trollshell` battery chip's emergency pulse) agree with the toast instead
//! of carrying their own threshold (#656).
//!
//! [`on_battery`] tracks a separate property — `OnBattery` on the *manager*
//! object (`/org/freedesktop/UPower`, `org.freedesktop.UPower`), not the
//! device — since it's correct even on desktops with no battery (#230).

use crate::notifications::Urgency;
use futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte_bus::{BusKind, PropState, PropertySignal, property};
use hytte_reactive::{Service, registry, spawn_supervised};
use std::time::Duration;

const UPOWER_NAME: &str = "org.freedesktop.UPower";
const DISPLAY_DEVICE_PATH: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
const DEVICE_IFACE: &str = "org.freedesktop.UPower.Device";
const MANAGER_PATH: &str = "/org/freedesktop/UPower";

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

/// `UPower`'s `WarningLevel` enum (`org.freedesktop.UPower.Device.WarningLevel`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WarningLevel {
    Unknown,
    None,
    /// UPS-only: discharging but not yet at a warning threshold.
    Discharging,
    Low,
    Critical,
    Action,
}

impl WarningLevel {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::None,
            2 => Self::Discharging,
            3 => Self::Low,
            4 => Self::Critical,
            5 => Self::Action,
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
    /// `UPower`'s own low-battery severity, independent of `percentage`.
    pub warning_level: WarningLevel,
}

impl Default for Battery {
    fn default() -> Self {
        Self {
            percentage: 0.0,
            state: BatteryState::Unknown,
            time_to_empty: None,
            time_to_full: None,
            icon_name: String::new(),
            warning_level: WarningLevel::Unknown,
        }
    }
}

#[doc(hidden)]
pub struct UpowerHandles {
    pub(crate) battery: Mutable<Battery>,
    pub(crate) on_battery: Mutable<bool>,
}

impl Default for UpowerHandles {
    fn default() -> Self {
        Self {
            battery: Mutable::new(Battery::default()),
            on_battery: Mutable::new(false),
        }
    }
}

impl Service for UpowerService {
    type Handles = UpowerHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = UpowerHandles::default();
        let writer = handles.battery.clone();

        bind_prop_field(
            display_device_prop::<f64>("Percentage"),
            0.0,
            writer.clone(),
            |b, v| b.percentage = v,
        );
        bind_prop_field(
            display_device_prop::<u32>("State"),
            0,
            writer.clone(),
            |b, v| b.state = BatteryState::from_u32(v),
        );
        bind_prop_field(
            display_device_prop::<i64>("TimeToEmpty"),
            0,
            writer.clone(),
            |b, v| b.time_to_empty = secs_to_duration(v),
        );
        bind_prop_field(
            display_device_prop::<i64>("TimeToFull"),
            0,
            writer.clone(),
            |b, v| b.time_to_full = secs_to_duration(v),
        );
        bind_prop_field(
            display_device_prop::<String>("IconName"),
            String::new(),
            writer.clone(),
            |b, v| b.icon_name = v,
        );
        bind_prop_field(
            display_device_prop::<u32>("WarningLevel"),
            0,
            writer.clone(),
            |b, v| b.warning_level = WarningLevel::from_u32(v),
        );

        spawn_warning_level_watcher(writer);

        bind_on_battery(
            manager_prop::<bool>("OnBattery"),
            handles.on_battery.clone(),
        );

        handles
    }
}

fn display_device_prop<T>(name: &'static str) -> PropertySignal<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    property::<T>(BusKind::System, UPOWER_NAME)
        .at_path(DISPLAY_DEVICE_PATH)
        .iface(DEVICE_IFACE)
        .name(name)
        .start()
}

/// The manager object (`/org/freedesktop/UPower`, `org.freedesktop.UPower`)
/// carries system-wide properties like `OnBattery` — distinct from the
/// per-device iface `display_device_prop` targets.
fn manager_prop<T>(name: &'static str) -> PropertySignal<T>
where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    property::<T>(BusKind::System, UPOWER_NAME)
        .at_path(MANAGER_PATH)
        .iface(UPOWER_NAME)
        .name(name)
        .start()
}

fn bind_on_battery(prop: PropertySignal<bool>, writer: Mutable<bool>) {
    spawn_supervised("upower", move || {
        let prop = prop.clone();
        let writer = writer.clone();
        async move {
            prop.signal()
                .for_each(move |s| {
                    let v = match s {
                        PropState::Loaded(v) | PropState::Stale(v) => v,
                        PropState::Loading => false,
                    };
                    writer.set(v);
                    std::future::ready(())
                })
                .await;
        }
    });
}

fn secs_to_duration(secs: i64) -> Option<Duration> {
    u64::try_from(secs)
        .ok()
        .filter(|&s| s > 0)
        .map(Duration::from_secs)
}

fn bind_prop_field<T>(
    prop: PropertySignal<T>,
    default: T,
    writer: Mutable<Battery>,
    apply: impl Fn(&mut Battery, T) + Clone + Send + 'static,
) where
    T: Clone
        + Send
        + Sync
        + 'static
        + TryFrom<zbus::zvariant::OwnedValue, Error = zbus::zvariant::Error>
        + for<'v> TryFrom<zbus::zvariant::Value<'v>, Error = zbus::zvariant::Error>,
{
    spawn_supervised("upower", move || {
        let prop = prop.clone();
        let writer = writer.clone();
        let default = default.clone();
        let apply = apply.clone();
        async move {
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
        }
    });
}

/// Coarse severity used to detect a genuine *rising* `WarningLevel` edge.
/// `Unknown`, `None`, and `Discharging` (UPS-only, not a warning by itself)
/// all collapse to "not warning" — only `Low` and `Critical`/`Action` are
/// toast-worthy tiers.
fn warning_tier(level: WarningLevel) -> u8 {
    match level {
        WarningLevel::Unknown | WarningLevel::None | WarningLevel::Discharging => 0,
        WarningLevel::Low => 1,
        WarningLevel::Critical | WarningLevel::Action => 2,
    }
}

/// Whether `level` sits at the same "critical" severity tier that drives the
/// critical-urgency "Battery critical" toast in [`warning_toast`] (i.e.
/// `Critical` or `Action`). Exposed so other consumers of `WarningLevel` —
/// e.g. the battery chip's emergency pulse in `trollshell` — key off the
/// exact same severity split the toast uses instead of inventing a second,
/// driftable one (#656).
#[must_use]
pub fn is_critical(level: WarningLevel) -> bool {
    warning_tier(level) >= warning_tier(WarningLevel::Critical)
}

/// Decide whether moving from `baseline` (the last-observed level, or `None`
/// if `next` is the very first observation) to `next` should post a toast,
/// and at what urgency.
///
/// `baseline = None` never toasts — it only seeds the baseline, so a shell
/// restart while the battery is already `Low`/`Critical` doesn't re-announce
/// a state the user is already living with (see #237 triage: naively
/// toasting on "any transition into Low/Critical" would re-fire on every
/// restart, since the property emits its current value as the first
/// `Loaded`). Otherwise only a **rising** edge — climbing to a higher
/// severity tier — toasts; falling back (charger plugged in) or sitting at
/// the same tier stays silent. A fast drain that skips a tier (jumping
/// straight from "fine" to `Critical`/`Action`) still fires the
/// higher-severity toast once.
fn warning_toast(baseline: Option<WarningLevel>, next: WarningLevel) -> Option<Urgency> {
    let prev_tier = warning_tier(baseline?);
    let next_tier = warning_tier(next);
    if next_tier <= prev_tier {
        return None;
    }
    match next_tier {
        1 => Some(Urgency::Normal),
        2 => Some(Urgency::Critical),
        _ => None,
    }
}

/// Watch `battery`'s `warning_level` for rising edges and self-post a toast
/// via [`crate::notifications::post_local`] (normal-urgency entering `Low`,
/// critical-urgency entering `Critical`/`Action`) — see [`warning_toast`] for
/// the dedup/baseline rules.
///
/// Runs on the hytte-tokio runtime, not the GTK thread; `post_local` is
/// cross-thread-safe (reaches the notifications daemon's `SHARED` handle),
/// so this is safe regardless of service registration order.
fn spawn_warning_level_watcher(battery: Mutable<Battery>) {
    spawn_supervised("upower-warning", move || {
        let battery = battery.clone();
        async move {
            let mut baseline: Option<WarningLevel> = None;
            battery
                .signal_ref(|b| b.warning_level)
                .dedupe()
                .for_each(move |level| {
                    if let Some(urgency) = warning_toast(baseline, level) {
                        let percentage = battery.lock_ref().percentage;
                        let summary = match level {
                            WarningLevel::Critical | WarningLevel::Action => "Battery critical",
                            _ => "Battery low",
                        };
                        let body = format!("{percentage:.0}% remaining");
                        crate::notifications::post_local("Battery", summary, &body, urgency);
                    }
                    baseline = Some(level);
                    std::future::ready(())
                })
                .await;
        }
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

/// Whether the system is currently running on battery power, per `UPower`'s
/// manager-level `OnBattery` property. `false` until the first `Loaded`
/// (covers both "on AC" and "not yet known").
pub fn on_battery() -> impl Signal<Item = bool> {
    registry::with(|r| {
        r.get::<UpowerHandles>()
            .expect("upower::service() not registered")
            .on_battery
            .signal()
    })
}

#[cfg(test)]
mod tests {
    use super::{Urgency, WarningLevel, is_critical, warning_toast};

    #[test]
    fn is_critical_matches_the_toasts_critical_tier() {
        // Pins the exact split `battery.rs`'s emergency pulse relies on
        // (#656): only Critical/Action count, same as the toast's
        // `Urgency::Critical` branch in `warning_toast`.
        assert!(!is_critical(WarningLevel::Unknown));
        assert!(!is_critical(WarningLevel::None));
        assert!(!is_critical(WarningLevel::Discharging));
        assert!(!is_critical(WarningLevel::Low));
        assert!(is_critical(WarningLevel::Critical));
        assert!(is_critical(WarningLevel::Action));
    }

    #[test]
    fn first_observation_never_toasts() {
        // The baseline seed (shell startup) must stay silent even if the
        // battery is already Low/Critical — see #237 triage.
        assert_eq!(warning_toast(None, WarningLevel::Low), None);
        assert_eq!(warning_toast(None, WarningLevel::Critical), None);
        assert_eq!(warning_toast(None, WarningLevel::Action), None);
        assert_eq!(warning_toast(None, WarningLevel::None), None);
    }

    #[test]
    fn rising_edge_into_low_toasts_normal() {
        assert_eq!(
            warning_toast(Some(WarningLevel::None), WarningLevel::Low),
            Some(Urgency::Normal)
        );
        assert_eq!(
            warning_toast(Some(WarningLevel::Discharging), WarningLevel::Low),
            Some(Urgency::Normal)
        );
    }

    #[test]
    fn rising_edge_into_critical_or_action_toasts_critical() {
        assert_eq!(
            warning_toast(Some(WarningLevel::Low), WarningLevel::Critical),
            Some(Urgency::Critical)
        );
        assert_eq!(
            warning_toast(Some(WarningLevel::Critical), WarningLevel::Action),
            None // same tier — no re-toast
        );
        assert_eq!(
            warning_toast(Some(WarningLevel::Low), WarningLevel::Action),
            Some(Urgency::Critical)
        );
    }

    #[test]
    fn fast_drain_skipping_low_still_toasts_critical() {
        // A quick drain can jump straight from "fine" to Critical/Action
        // without UPower ever reporting Low in between.
        assert_eq!(
            warning_toast(Some(WarningLevel::None), WarningLevel::Critical),
            Some(Urgency::Critical)
        );
    }

    #[test]
    fn sitting_at_the_same_level_does_not_retoast() {
        assert_eq!(
            warning_toast(Some(WarningLevel::Low), WarningLevel::Low),
            None
        );
        assert_eq!(
            warning_toast(Some(WarningLevel::Critical), WarningLevel::Critical),
            None
        );
    }

    #[test]
    fn falling_back_is_silent() {
        // Charger plugged in: level improves, no toast, but it does become
        // the new baseline for the next rising edge.
        assert_eq!(
            warning_toast(Some(WarningLevel::Critical), WarningLevel::Low),
            None
        );
        assert_eq!(
            warning_toast(Some(WarningLevel::Low), WarningLevel::None),
            None
        );
    }

    #[test]
    fn re_arming_after_a_fall_toasts_again() {
        // Low -> None (charger) -> Low again should toast the second time:
        // it's a fresh rising edge from the new baseline.
        let after_charge = WarningLevel::None;
        assert_eq!(
            warning_toast(Some(after_charge), WarningLevel::Low),
            Some(Urgency::Normal)
        );
    }
}
