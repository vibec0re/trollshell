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
use hytte_reactive::{Service, registry, shared, spawn_supervised};
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

/// Cross-thread mirror of the `OnBattery` handle, for the background pollers
/// that pick their cadence from it (#505).
///
/// This has to be a [`shared`] bag rather than a thread-local registry read:
/// the pollers that consume it (`netconn`, `app_usage`, `wifiscan`, `places`)
/// run their loops inside `spawn_supervised`, i.e. on **tokio worker
/// threads**, and [`registry`] is a `thread_local!` that only the GTK main
/// thread ever populates. A `registry::with` from a worker thread sees a
/// freshly-defaulted empty `Registry` and reports "no upower" forever — which
/// is exactly the bug #526 shipped (see [`on_battery_snapshot`]).
pub(crate) struct UpowerShared {
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

        // Publish the OnBattery handle on the cross-thread path too — the
        // cadence pollers read it from tokio workers, where the thread-local
        // registry is empty. See `UpowerShared`.
        shared::insert(UpowerShared {
            on_battery: handles.on_battery.clone(),
        });

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

/// Best-effort "are we on battery?" snapshot, readable from **any** thread.
///
/// This is the one accessor the battery-aware pollers use to pick their
/// cadence (#505). It reads the [`UpowerShared`] bag rather than the
/// thread-local registry, because every consumer runs inside
/// `spawn_supervised` — on a tokio worker thread, where a `registry::with`
/// read sees an empty registry and would report AC forever regardless of the
/// real power state.
///
/// # Degrades to AC, never to the slow cadence
///
/// Returns `false` ("on AC", i.e. **normal** cadence) whenever the true state
/// isn't known. That covers every degenerate case, and each one is deliberate:
///
/// - **`upower::service()` not registered at all** — e.g. a build that leaves
///   it out, or `enableRecommendedServices = false` so `UPower` isn't running.
///   `shared::get` returns `None`.
/// - **`UPower` absent from the bus** — the service is registered but the
///   `OnBattery` property never resolves, so `bind_on_battery` leaves the
///   handle at its `false` default (`PropState::Loading` maps to `false`).
/// - **Desktop with no battery** — `UPower` answers `OnBattery = false`. This is
///   why the manager property is the right source and `BatteryState::Unknown`
///   on the display device is not: a batteryless machine reports `Unknown`
///   state but is unambiguously on AC.
/// - **Startup ordering** — `main.rs` registers `wifiscan`/`places` *before*
///   `upower`, so a poll loop can run before upower's handles are published.
///   Reads before that point say AC, then track the real value.
///
/// Biasing the unknown case toward AC means the worst failure is a poller
/// staying at full rate on battery (a little extra power draw), never a
/// poller stuck at the stretched cadence on a machine that has no battery at
/// all — which would be a visible, hard-to-diagnose staleness bug.
pub(crate) fn on_battery_snapshot() -> bool {
    shared::get::<UpowerShared>().is_some_and(|s| s.on_battery.get())
}

#[cfg(test)]
mod tests {
    use super::{
        UpowerShared, Urgency, WarningLevel, is_critical, on_battery_snapshot, warning_toast,
    };
    use futures_signals::signal::Mutable;
    use hytte_reactive::test_lock::TEST_LOCK;
    use hytte_reactive::{registry, shared};

    /// Everything about the battery-aware cadence snapshot (#505), in **one**
    /// test on purpose: `hytte_reactive::shared` is a process-global map, so
    /// two `#[test]` fns mutating it would run concurrently on libtest's
    /// thread pool and flake against each other. One test = one thread = a
    /// deterministic sequence.
    ///
    /// That only protects this test from *itself*, though — the map is
    /// process-global, not module-global, so any other crate's tests that
    /// clear it (e.g. `places::tests` in this same crate, via
    /// `with_seeded_config`) race this one too unless they take the same
    /// lock. Hence `TEST_LOCK` (#777): this test used to take no lock at
    /// all, which is exactly the gap that let it interleave with
    /// `places::tests`' own (previously private, now retired) lock and
    /// produce `places.rs`'s `NotRunning` flake in CI.
    #[test]
    fn on_battery_snapshot_contract() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry::reset_for_tests();

        // ── Degenerate: upower never registered ──────────────────────────
        // A desktop with `enableRecommendedServices = false`, or any build
        // that leaves `upower::service()` out. Must read as AC (`false`) so
        // pollers stay at their *normal* cadence — never the stretched one.
        assert!(
            !on_battery_snapshot(),
            "no upower registered must degrade to AC, not to the slow cadence"
        );

        // ── Registered, on AC ────────────────────────────────────────────
        // Also the batteryless-desktop case: UPower answers OnBattery=false
        // even though the display device's BatteryState is Unknown.
        let on_battery = Mutable::new(false);
        shared::insert(UpowerShared {
            on_battery: on_battery.clone(),
        });
        assert!(!on_battery_snapshot());

        // ── Registered, on battery ───────────────────────────────────────
        on_battery.set(true);
        assert!(on_battery_snapshot());

        // ── Reachable from another thread ────────────────────────────────
        // The regression pin for #526: every consumer of this snapshot runs
        // its poll loop under `spawn_supervised`, i.e. off the GTK main
        // thread. The original implementation read the `thread_local!`
        // registry, which is only ever populated on the main thread — so from
        // a worker it reported AC forever and the whole feature was a no-op.
        // `std::thread` rather than a tokio worker here: `thread_local!`
        // behaves identically for both, and this needs no runtime features.
        let seen = std::thread::spawn(on_battery_snapshot)
            .join()
            .expect("snapshot thread panicked");
        assert!(
            seen,
            "snapshot must be readable off the GTK main thread — the pollers \
             that consume it all run on tokio workers"
        );

        // ── Live flip is observed, not latched at startup ─────────────────
        on_battery.set(false);
        assert!(!on_battery_snapshot());

        // Leave the bag holding `true` right before the reset: if
        // `reset_for_tests` ever stopped clearing the shared map, the stale
        // bag would still answer `true` here and the assert below would
        // catch it. Resetting straight from a `false` bag can't distinguish
        // "map cleared" from "map intact and still false" — both read as
        // `false` (#738).
        on_battery.set(true);
        registry::reset_for_tests();
        assert!(
            !on_battery_snapshot(),
            "reset must clear the shared bag back to the AC default"
        );
    }

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
