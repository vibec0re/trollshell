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
//! `WarningLevel` additionally drives a self-toast: `spawn_warning_level_watcher`
//! watches for a rising severity edge (entering `Low` or `Critical`/`Action`)
//! and posts via [`crate::notifications::post_local`] — see `warning_toast`
//! for the crossing/dedup rules (#237), and `WarningWatch` for why it reads
//! the raw property states rather than [`Battery`]. [`is_critical`] exposes
//! that same `Critical`/`Action` severity split so other `WarningLevel`
//! consumers (the `trollshell` battery chip's emergency pulse) agree with the
//! toast instead of carrying their own threshold (#656).
//!
//! [`on_battery`] tracks a separate property — `OnBattery` on the *manager*
//! object (`/org/freedesktop/UPower`, `org.freedesktop.UPower`), not the
//! device — since it's correct even on desktops with no battery (#230).

use crate::notifications::Urgency;
use futures_signals::map_ref;
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

        // Two of these subscriptions are also read raw by the warning watcher
        // below, which needs to tell "not loaded yet" from a loaded value —
        // something the folded `Battery` (whose `Loading` arm writes the field's
        // default) cannot say. Cloning a `PropertySignal` shares one tracking
        // task; it does not open a second subscription.
        let percentage = display_device_prop::<f64>("Percentage");
        let warning_level = display_device_prop::<u32>("WarningLevel");

        bind_prop_field(percentage.clone(), 0.0, writer.clone(), |b, v| {
            b.percentage = v;
        });
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
        bind_prop_field(warning_level.clone(), 0, writer, |b, v| {
            b.warning_level = WarningLevel::from_u32(v);
        });

        spawn_warning_level_watcher(warning_level, percentage);

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
/// critical-urgency "Battery critical" toast in `warning_toast` (i.e.
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
/// `Loaded`). That only holds if `None` is what the first *loaded* value
/// meets, which is [`WarningWatch`]'s job: it is the only caller, and it
/// does not count `PropState::Loading` as an observation. Otherwise only a
/// **rising** edge — climbing to a higher severity tier — toasts; falling back (charger plugged in) or sitting at
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

/// One low-battery toast, as [`WarningWatch::observe`] decided it.
#[derive(Debug, PartialEq)]
struct WarningToast {
    summary: &'static str,
    /// `"N% remaining"`, or empty when the toast was held for
    /// [`PERCENTAGE_HOLD`] and `Percentage` still had not loaded. An empty
    /// body gives a summary-only card (the toast overlay draws no body line
    /// for it), so it reads "Battery low" rather than claiming a figure.
    body: String,
    urgency: Urgency,
}

/// The edge detector behind the low-battery toast, fed the **raw** property
/// states rather than the folded [`Battery`].
///
/// That is the whole fix for the toast re-firing on every shell restart.
/// `Battery` folds `PropState::Loading` into each field's default, so its
/// `warning_level` starts out as `WarningLevel::Unknown` before `UPower` has
/// answered anything — and the watcher that used to read it took that
/// placeholder as its first observation, seeded the baseline at tier 0, and
/// then saw the first real `Loaded(Low)` as a rising edge. The "a restart
/// while Low doesn't re-announce" rule on [`warning_toast`] was therefore
/// never in force for the one path that runs in production. Here `Loading`
/// is not an observation at all, so the baseline is seeded by the first
/// value `UPower` actually reported.
///
/// `Stale(v)` *is* an observation: `hytte_bus` only produces it after a
/// `Loaded` (a bus reconnect keeps the last value), so it carries the same
/// level the baseline already holds and is silent by [`warning_toast`]'s own
/// rule, while a level that moved during the outage is judged on its own
/// edge when the reconnect delivers it.
#[derive(Debug, Default)]
struct WarningWatch {
    /// The last level `UPower` reported; `None` until it has reported one.
    baseline: Option<WarningLevel>,
    /// A rising edge seen while `Percentage` was still `Loading`: the toast is
    /// held here and posted by the first `observe` that has the number
    /// (#1559), because a card that says "Battery low" with no figure is
    /// never revised once the figure arrives. The hold is bounded:
    /// [`run_warning_watch`] posts it without a body through [`Self::expire`]
    /// once [`PERCENTAGE_HOLD`] has passed, since a `Percentage` whose `Get`
    /// keeps failing stays `Loading` for as long as it fails.
    pending: Option<WarningLevel>,
}

/// How long a held low-battery toast waits for `Percentage` before it is
/// posted without one.
///
/// When both `Get`s work they land one system-bus round trip apart, so a held
/// toast normally waits milliseconds. When `Percentage`'s first `Get` fails,
/// `hytte_bus` retries it on its ramp (250 ms, 500 ms, 1 s, 2 s, …, capped at
/// 30 s) and never gives up, leaving it `Loading` the whole time. Five seconds
/// covers the first four retries (3.75 s of waiting plus their round trips),
/// and it matches `hytte_bus`'s own line between a blip and an outage
/// (`STREAK_IS_NO_LONGER_A_BLIP`, about 4 s of failures). Past that the figure
/// is not coming soon, and a low-battery warning must not wait on it: five
/// seconds is nothing against the minutes between `UPower`'s warning levels.
const PERCENTAGE_HOLD: Duration = Duration::from_secs(5);

/// The toast for a held level, or `None` for a level that is not a warning.
fn toast_for(held: WarningLevel, body: String) -> Option<WarningToast> {
    let urgency = warning_toast(Some(WarningLevel::Unknown), held)?;
    let summary = match held {
        WarningLevel::Critical | WarningLevel::Action => "Battery critical",
        _ => "Battery low",
    };
    Some(WarningToast {
        summary,
        body,
        urgency,
    })
}

impl WarningWatch {
    /// The hold ran out: post the held toast without a body, if one is held.
    fn expire(&mut self) -> Option<WarningToast> {
        toast_for(self.pending.take()?, String::new())
    }

    /// Feed one combined emission; returns the toast to post, if any.
    ///
    /// `percentage` only shapes the body. It is read as its own state rather
    /// than off `Battery` for the same reason as `level`: `WarningLevel` can
    /// land before `Percentage` does, and `Battery.percentage` is `0.0` until
    /// then — which used to put "0% remaining" on a `Low` toast.
    fn observe(
        &mut self,
        level: &PropState<u32>,
        percentage: &PropState<f64>,
    ) -> Option<WarningToast> {
        let (PropState::Loaded(raw) | PropState::Stale(raw)) = level else {
            return None;
        };
        let level = WarningLevel::from_u32(*raw);
        let edge = warning_toast(self.baseline, level);
        self.baseline = Some(level);
        if edge.is_some() {
            self.pending = Some(level);
        } else if self
            .pending
            .is_some_and(|held| warning_tier(level) < warning_tier(held))
        {
            // The level fell back before the percentage arrived (charger
            // plugged in): the condition the held toast announced is over.
            self.pending = None;
        }
        let (PropState::Loaded(p) | PropState::Stale(p)) = percentage else {
            return None;
        };
        toast_for(self.pending.take()?, format!("{p:.0}% remaining"))
    }
}

/// Drive a [`WarningWatch`] off the two property signals and hand each toast
/// it decides on to `post`. Returns when either signal ends.
///
/// It also owns the hold's clock: a toast [`WarningWatch`] is holding for
/// `Percentage` is posted without a body once [`PERCENTAGE_HOLD`] has passed
/// since the edge, unless an emission posted or dropped it first.
///
/// The signals and the sink are injected so the tests run the production
/// fold — the combination, the ordering, the seed, the hold — without a bus
/// or the notifications daemon.
async fn run_warning_watch<L, P, Post>(level: L, percentage: P, mut post: Post)
where
    L: Signal<Item = PropState<u32>>,
    P: Signal<Item = PropState<f64>>,
    Post: FnMut(WarningToast),
{
    use futures_util::StreamExt;

    let mut watch = WarningWatch::default();
    let states = map_ref! {
        let level = level,
        let percentage = percentage => (level.clone(), percentage.clone())
    }
    .to_stream();
    let mut states = std::pin::pin!(states);
    let mut deadline: Option<tokio::time::Instant> = None;
    loop {
        let next = match deadline {
            Some(at) => {
                if let Ok(next) = tokio::time::timeout_at(at, states.next()).await {
                    next
                } else {
                    deadline = None;
                    if let Some(toast) = watch.expire() {
                        post(toast);
                    }
                    continue;
                }
            }
            None => states.next().await,
        };
        let Some((level, percentage)) = next else {
            return;
        };
        if let Some(toast) = watch.observe(&level, &percentage) {
            post(toast);
        }
        // Armed by the edge that starts a hold and kept when a higher level
        // replaces the held one, so the bound runs from the first edge.
        if watch.pending.is_none() {
            deadline = None;
        } else if deadline.is_none() {
            deadline = Some(tokio::time::Instant::now() + PERCENTAGE_HOLD);
        }
    }
}

/// Watch `UPower`'s `WarningLevel` for rising edges and self-post a toast via
/// [`crate::notifications::post_local`] (normal-urgency entering `Low`,
/// critical-urgency entering `Critical`/`Action`) — see [`WarningWatch`] for
/// the seed and [`warning_toast`] for the edge rules.
///
/// Runs on the hytte-tokio runtime, not the GTK thread; `post_local` is
/// cross-thread-safe (reaches the notifications daemon's `SHARED` handle),
/// so this is safe regardless of service registration order. A supervisor
/// restart builds a fresh [`WarningWatch`], which re-seeds silently from the
/// current value — the same rule as a shell restart.
fn spawn_warning_level_watcher(level: PropertySignal<u32>, percentage: PropertySignal<f64>) {
    spawn_supervised("upower-warning", move || {
        let level = level.clone();
        let percentage = percentage.clone();
        async move {
            run_warning_watch(level.signal(), percentage.signal(), |toast| {
                crate::notifications::post_local(
                    "Battery",
                    toast.summary,
                    &toast.body,
                    toast.urgency,
                );
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

/// `on_battery_snapshot`, reachable from outside this crate.
///
/// `on_battery_snapshot` stays `pub(crate)` — its own doc above is the single
/// source of truth for the contract (degrades to AC on every unknown case,
/// never to the stretched cadence) — and every in-crate poller (`places`,
/// `wifiscan`, `netconn`, `app_usage`) wraps it with its own private
/// `fn on_battery() -> bool` rather than calling it by name directly, so a
/// grep for the real accessor always lands on one place. `trollshell`'s
/// `config::core_leds` (#1041) is the first battery-aware poller that lives in
/// a *different* crate, so `pub(crate)` cannot reach it from there — this is
/// that identical one-line wrapper, re-exported rather than widening the
/// original. Not to be confused with [`on_battery`] above: that one returns a
/// `Signal` and requires the thread-local registry (the GTK main thread); this
/// one is the any-thread snapshot, for a poller running on a tokio worker.
#[must_use]
pub fn on_battery_now() -> bool {
    on_battery_snapshot()
}

#[cfg(test)]
mod tests {
    use super::{
        PERCENTAGE_HOLD, UpowerShared, Urgency, WarningLevel, WarningToast, WarningWatch,
        is_critical, on_battery_snapshot, run_warning_watch, warning_toast,
    };
    use futures_signals::signal::Mutable;
    use hytte_bus::PropState;
    use hytte_reactive::test_lock::TEST_LOCK;
    use hytte_reactive::{registry, shared};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

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

    // ── The production path: raw property states into `WarningWatch` ──────
    //
    // The tests above drive `warning_toast` with a hand-picked baseline. The
    // one that matters — `None`, i.e. a shell that just started — was never
    // what production fed it: the old watcher read `Battery`, whose
    // `warning_level` is `Unknown` before UPower answers, so the baseline was
    // always seeded `Some(Unknown)` and a restart while Low re-announced.
    // These go through the same `observe` the spawned watcher calls.

    /// `UPower`'s raw `WarningLevel` values (`org.freedesktop.UPower.Device`).
    const NONE: u32 = 1;
    const LOW: u32 = 3;
    const CRITICAL: u32 = 4;

    /// A restart while already Low: the property is `Loading` until the first
    /// `Get` answers, then `Loaded(Low)`. Nothing may toast.
    ///
    /// Falsifiable: treating `Loading` as an observation (seeding the
    /// baseline from it, as reading `Battery`'s defaulted field did) turns
    /// the second `observe` into a rising edge and posts "Battery low".
    #[test]
    fn a_restart_while_low_seeds_from_the_first_loaded_value() {
        let mut watch = WarningWatch::default();
        assert_eq!(
            watch.observe(&PropState::Loading, &PropState::Loading),
            None
        );
        assert_eq!(watch.baseline, None, "Loading must not seed the baseline");
        assert_eq!(
            watch.observe(&PropState::Loaded(LOW), &PropState::Loaded(12.0)),
            None,
            "the first loaded level seeds silently, even when it is Low"
        );
        // …and the seed is live: the next real rise still toasts.
        assert_eq!(
            watch
                .observe(&PropState::Loaded(CRITICAL), &PropState::Loaded(4.0))
                .map(|t| t.urgency),
            Some(Urgency::Critical)
        );
    }

    /// A bus reconnect keeps the last value as `Stale`, then reloads it. Same
    /// level both times: no re-toast.
    #[test]
    fn a_reconnect_at_the_same_level_does_not_retoast() {
        let mut watch = WarningWatch::default();
        let pct = PropState::Loaded(9.0);
        assert_eq!(watch.observe(&PropState::Loaded(NONE), &pct), None);
        assert!(watch.observe(&PropState::Loaded(LOW), &pct).is_some());
        assert_eq!(watch.observe(&PropState::Stale(LOW), &pct), None);
        assert_eq!(watch.observe(&PropState::Loaded(LOW), &pct), None);
    }

    /// `WarningLevel` can land before `Percentage`: the toast is held until
    /// the percentage loads, then posted once with its body (#1559). Before,
    /// it went out with an empty body and was never filled in.
    ///
    /// Falsifiable: posting on the edge (the old behaviour) fails the first
    /// `None`; dropping the held edge fails the `Some`.
    #[test]
    fn a_toast_waits_for_a_percentage_that_has_not_loaded() {
        let mut watch = WarningWatch::default();
        assert_eq!(
            watch.observe(&PropState::Loaded(NONE), &PropState::Loading),
            None
        );
        assert_eq!(
            watch.observe(&PropState::Loaded(LOW), &PropState::Loading),
            None,
            "held, not posted bodiless"
        );
        assert_eq!(
            watch.observe(&PropState::Loaded(LOW), &PropState::Loaded(14.4)),
            Some(WarningToast {
                summary: "Battery low",
                body: "14% remaining".to_string(),
                urgency: Urgency::Normal,
            })
        );
        assert_eq!(
            watch.observe(&PropState::Loaded(LOW), &PropState::Loaded(14.0)),
            None,
            "posted once"
        );
    }

    /// A held toast whose condition ended before the percentage arrived is
    /// dropped, not posted late.
    #[test]
    fn a_held_toast_is_dropped_if_the_level_recovers() {
        let mut watch = WarningWatch::default();
        watch.observe(&PropState::Loaded(NONE), &PropState::Loading);
        assert_eq!(
            watch.observe(&PropState::Loaded(LOW), &PropState::Loading),
            None
        );
        assert_eq!(
            watch.observe(&PropState::Loaded(NONE), &PropState::Loading),
            None
        );
        assert_eq!(
            watch.observe(&PropState::Loaded(NONE), &PropState::Loaded(80.0)),
            None
        );
    }

    /// The held level is the toast's severity: Low held, then Critical before
    /// the percentage arrives, posts one critical toast.
    #[test]
    fn a_held_toast_posts_at_the_latest_severity() {
        let mut watch = WarningWatch::default();
        watch.observe(&PropState::Loaded(NONE), &PropState::Loading);
        watch.observe(&PropState::Loaded(LOW), &PropState::Loading);
        watch.observe(&PropState::Loaded(CRITICAL), &PropState::Loading);
        assert_eq!(
            watch
                .observe(&PropState::Loaded(CRITICAL), &PropState::Loaded(4.0))
                .map(|t| (t.summary, t.urgency)),
            Some(("Battery critical", Urgency::Critical))
        );
    }

    /// A held toast must not wait for ever. `Percentage` stays `Loading` for
    /// as long as its `Get` keeps failing (`hytte_bus` retries it on a ramp and
    /// never gives up), and that is the one realistic way to reach the hold:
    /// when both `Get`s work they land milliseconds apart, too soon for the
    /// level to rise in between. The warning must still go out, without the
    /// figure, once the hold runs out (#1574 review).
    ///
    /// Falsifiable: an unbounded hold posts nothing; a bound shorter than
    /// `PERCENTAGE_HOLD` posts before the first check.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn a_held_toast_still_posts_when_the_percentage_never_loads() {
        let level = Mutable::new(PropState::<u32>::Loading);
        let pct = Mutable::new(PropState::<f64>::Loading);
        let posted: Arc<Mutex<Vec<WarningToast>>> = Arc::new(Mutex::new(Vec::new()));
        let task = {
            let posted = posted.clone();
            tokio::spawn(run_warning_watch(
                level.signal_cloned(),
                pct.signal_cloned(),
                move |toast| posted.lock().unwrap().push(toast),
            ))
        };
        let settle = || async {
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        };
        level.set(PropState::Loaded(NONE));
        settle().await;
        level.set(PropState::Loaded(LOW));
        settle().await;

        // The test's own sleeps let the paused clock auto-advance; each one
        // ends at its own deadline, so the watcher's timer fires only once it
        // is due, and `settle` lets it run before anything is asserted.
        tokio::time::sleep(PERCENTAGE_HOLD.saturating_sub(Duration::from_millis(1))).await;
        settle().await;
        assert!(
            posted.lock().unwrap().is_empty(),
            "still holding for the figure just before the bound"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
        settle().await;
        assert_eq!(
            *posted.lock().unwrap(),
            vec![WarningToast {
                summary: "Battery low",
                body: String::new(),
                urgency: Urgency::Normal,
            }],
            "past the bound the warning goes out without a figure rather than not at all"
        );

        // A percentage that lands later does not post a second card.
        pct.set(PropState::Loaded(9.0));
        settle().await;
        assert_eq!(posted.lock().unwrap().len(), 1);

        drop(level);
        drop(pct);
        task.await.expect("watcher task panicked");
    }

    #[test]
    fn the_body_carries_a_loaded_percentage() {
        let mut watch = WarningWatch::default();
        assert_eq!(
            watch.observe(&PropState::Loaded(NONE), &PropState::Loaded(9.6)),
            None
        );
        assert_eq!(
            watch
                .observe(&PropState::Loaded(CRITICAL), &PropState::Stale(9.6))
                .map(|t| (t.summary, t.body)),
            Some(("Battery critical", "10% remaining".to_string()))
        );
    }

    /// The spawned watcher's own fold, end to end: two signals combined, the
    /// seed, the edge, the sink. Each `set` is followed by a yield so the
    /// watcher sees every intermediate state (a `Mutable` signal only reports
    /// the latest value it was polled at).
    #[tokio::test(flavor = "current_thread")]
    async fn run_warning_watch_posts_only_a_real_rising_edge() {
        let level = Mutable::new(PropState::<u32>::Loading);
        let pct = Mutable::new(PropState::<f64>::Loading);
        let posted: Arc<Mutex<Vec<WarningToast>>> = Arc::new(Mutex::new(Vec::new()));

        let task = {
            let posted = posted.clone();
            tokio::spawn(run_warning_watch(
                level.signal_cloned(),
                pct.signal_cloned(),
                move |toast| posted.lock().unwrap().push(toast),
            ))
        };
        let settle = || async {
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
        };

        settle().await;
        // Startup while already Low: silent.
        level.set(PropState::Loaded(LOW));
        settle().await;
        pct.set(PropState::Loaded(14.0));
        settle().await;
        assert!(
            posted.lock().unwrap().is_empty(),
            "a shell (re)start while Low must not announce it"
        );

        // Charger in, then out again: one fresh rising edge, with the body
        // read from the loaded percentage.
        level.set(PropState::Loaded(NONE));
        settle().await;
        pct.set(PropState::Loaded(13.0));
        settle().await;
        level.set(PropState::Loaded(LOW));
        settle().await;

        // Dropping both writers ends both signals, so the fold returns.
        drop(level);
        drop(pct);
        task.await.expect("watcher task panicked");

        assert_eq!(
            *posted.lock().unwrap(),
            vec![WarningToast {
                summary: "Battery low",
                body: "13% remaining".to_string(),
                urgency: Urgency::Normal,
            }]
        );
    }
}
