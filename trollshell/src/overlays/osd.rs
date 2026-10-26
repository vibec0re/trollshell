//! Transient on-screen-display popup for volume, microphone, and brightness
//! changes.
//!
//! When the user pokes a media or brightness key (the actual keybinds land
//! in #31), the relevant service signal emits a new value. This widget owns
//! one layer-shell window pinned Top-Center on the primary monitor and
//! pops it up briefly to show the new state, then auto-hides after 1.5s.
//!
//! Latest signal wins: if volume changes and brightness changes 200ms
//! later, the OSD switches kind immediately and resets its hide timer.
//!
//! Bootstrap suppression: each of the subscribed signals fires immediately on
//! install with the service's current snapshot, and the services start from a
//! placeholder (no backlight, no sinks, no sources) until their backend
//! reports — so the first *real* reading can arrive as a later emission.
//! Without suppression, opening the laptop with the bar starting up would
//! flash three OSDs in a row. Volume, microphone and brightness each swallow
//! emissions until their first real reading, which seeds silently
//! (`StartupGuard`); every emission after it shows the OSD. Battery keeps
//! its own seeded-baseline diff (`detect_battery_event`).
//!
//! Drawer suppression: the OSD stays quiet only while the drawer on its own
//! monitor shows the very control that changed — volume and microphone under
//! the Audio page, brightness under the Power page's slider
//! (`drawer_shows_control`). Battery alerts and a plugin's leave-by nudge
//! are never swallowed: no drawer page is a substitute for them.
//!
//! Multi-monitor: `install` mounts one layer-shell window per monitor,
//! keyed by `Monitor.connector()`. Module-level signal subscriptions
//! run exactly once across all mounts; `route_show` picks the OSD on
//! niri's focused output, falling back to the first mounted OSD when
//! the focused output is unknown or missing from the map.
//!
//! CSS hooks (intentionally bar-prefixed `ts-`):
//! - window root: `.ts-osd`
//! - inner card: `.ts-osd-card`
//! - header (icon + label/value column): `.ts-osd-header`
//! - icon: `.ts-osd-icon`
//! - kind label: `.ts-osd-label`
//! - value readout: `.ts-osd-value`
//! - progress bar: `.ts-osd-progress`
//! - kind modifier: `.volume`, `.mic`, `.brightness`, `.battery`, `.leave-by`
//! - state modifier: `.muted` (when applicable)
//! - shown modifier: `.shown` (toggled by Rust to drive CSS transitions)

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;

use crate::components::{cast, focused_output};
use crate::modal::Page;
use hytte::services::brightness::{self, Brightness};
use hytte::services::pipewire::{self, Sink, Source, Volume};
use hytte::services::upower::{self, Battery, BatteryState};
use hytte::ui::layer_window;

/// How long the OSD stays visible after the latest event, in ms. Each
/// new event resets the timer.
const HIDE_AFTER_MS: u32 = 1500;

/// Dwell for the leave-by nudge (#236). Much longer than [`HIDE_AFTER_MS`] —
/// it's a "get up and leave" alert you should have time to read and act on,
/// not a transient volume/brightness readout.
const LEAVE_BY_HIDE_AFTER_MS: u32 = 6000;

/// The symbolic icon a leave-by nudge falls back to when the plugin doesn't
/// name one in its `Effect::RaiseOsd`.
const LEAVE_BY_ICON: &str = "appointment-soon-symbolic";

/// Distance from top of screen to the OSD window's top edge.
/// `TOP_MARGIN` (48) + the CSS `.shown` `margin-top` (40) = **88 px** from the
/// screen top, keeping the resting card position unchanged while the larger
/// CSS travel makes it fly in from above.
const TOP_MARGIN: i32 = 48;

#[derive(Clone, Copy, Debug)]
enum Kind {
    Volume,
    Mic,
    Brightness,
    Battery,
    /// A plugin-raised "leave-by" nudge (#236) — the generic
    /// [`nudge`] entry point behind `Effect::RaiseOsd`.
    LeaveBy,
}

impl Kind {
    fn css_class(self) -> &'static str {
        match self {
            Self::Volume => "volume",
            Self::Mic => "mic",
            Self::Brightness => "brightness",
            Self::Battery => "battery",
            Self::LeaveBy => "leave-by",
        }
    }

    /// How long this kind lingers before auto-hiding. The volume/brightness
    /// OSDs are a glance ([`HIDE_AFTER_MS`]); a leave-by nudge is a
    /// get-up-and-go alert, so it dwells much longer ([`LEAVE_BY_HIDE_AFTER_MS`]).
    fn hide_after_ms(self) -> u32 {
        match self {
            Self::LeaveBy => LEAVE_BY_HIDE_AFTER_MS,
            _ => HIDE_AFTER_MS,
        }
    }
}

/// Mutable widgets owned by the OSD; rebuilt content swaps icon name,
/// progress fraction, label/value text, and kind/muted CSS classes
/// in place.
struct OsdView {
    window: gtk::Window,
    card: gtk::Box,
    icon: gtk::Image,
    label: gtk::Label, // kind name: "Volume" / "Microphone" / "Brightness"
    value: gtk::Label, // percent / "Muted"
    progress: gtk::ProgressBar,
    /// Pending hide timeout. Held so each new event can cancel and
    /// re-arm it (latest-wins debounce).
    timeout: Cell<Option<glib::SourceId>>,
    /// Pending fade-out → `set_visible(false)` timeout. Cancelled if a
    /// new event arrives mid-fade-out.
    fade_out_timeout: Cell<Option<glib::SourceId>>,
    /// CSS modifier classes currently set on the card so we can clean
    /// them up on each update without growing a leaky class list.
    current_kind: Cell<Option<&'static str>>,
    current_muted: Cell<bool>,
    /// The built-in page the modal drawer on this monitor is showing, `None`
    /// while it is closed (or showing no built-in page). `route_show` skips
    /// an OSD the page already shows the live control for
    /// ([`drawer_shows_control`]) — and only that one.
    drawer_page: Cell<Option<Page>>,
    /// Per-monitor `drawer_page_signal` subscription handle. Unlike the
    /// module-level singletons this is one-per-install, so it MUST be aborted
    /// on teardown — parked here and `.abort()`ed in [`close_all`], else it
    /// accumulates a dangling subscription per monitor rebuild.
    drawer_sub: RefCell<Option<glib::JoinHandle<()>>>,
}

/// Mount one OSD on `monitor` and lazily wire the module-level signal
/// subscriptions (volume / mic / brightness / focused-output) on the
/// first call. Subsequent calls only insert into the per-monitor map;
/// subscriptions stay singletons.
pub fn install(monitor: &Monitor) {
    // No `Some(c) if !c.is_empty()` guard needed any more (#1177):
    // `Monitor::connector()` itself folds `Some("")` to `None` since #1180
    // item 6, so this `let-else` already can't see the empty string.
    let Some(connector) = monitor.connector() else {
        tracing::warn!(
            description = ?monitor.description(),
            "osd::install: monitor has no connector name; skipping"
        );
        return;
    };

    let view = build_osd_view(monitor);

    // Subscribe to the page the drawer on this monitor shows so route_show
    // can suppress an OSD while the user is looking at that control's live
    // slider. The signal is backed by a lazily-allocated `Mutable` keyed by
    // connector, so this works even though `osd::install` runs before
    // `modal::install` during boot.
    //
    // Park the JoinHandle in the view: this is a PER-MONITOR subscription
    // (unlike the module-level singletons), so `close_all` must abort it on
    // teardown or it accumulates one per rebuild.
    let connector_for_sub = connector.clone();
    let drawer_sub = glib::MainContext::default().spawn_local(
        crate::modal::drawer_page_signal(monitor).for_each(move |page| {
            OSDS.with(|map| {
                if let Some(view) = map.borrow().get(&connector_for_sub) {
                    view.drawer_page.set(page);
                }
            });
            std::future::ready(())
        }),
    );
    view.drawer_sub.replace(Some(drawer_sub));

    // Tail-expression `insert` + an outer `drop`, not a bare `insert(…);`
    // statement (#643): the displaced `Rc<OsdView>` would otherwise be a
    // temporary of the same statement as the `borrow_mut()` `RefMut`, and
    // statement temporaries drop in reverse creation order — so it would run
    // its drop glue with `OSDS` still borrowed. At install time `OSDS` holds
    // the only strong `Rc` (`route_show`'s clone is short-lived and on the same
    // thread), so that drop would reach the whole `OsdView`: the `SourceId`
    // slots, the `drawer_sub` `JoinHandle`, and a `gtk::Window` unref. Still a
    // refcount decrement rather than a dispose (that needs `destroy()`, which
    // is `close_all`'s job), and reachable only on a double `install` for one
    // connector — same weak-but-free standard as the rest of the group.
    drop(OSDS.with(|map| map.borrow_mut().insert(connector.clone(), view)));

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}

/// Close every OSD surface, abort its per-monitor `drawer_page_signal`
/// subscription, cancel any pending hide/fade timers, and drop the
/// per-monitor entries. Called before rebuilding on monitor hot-plug so a
/// vanished output's `OsdView` doesn't linger in `OSDS` — otherwise
/// `route_show`'s unknown-focus fallback could pop an OSD on a dead
/// surface, and the un-aborted drawer subscription would leak per rebuild.
///
/// The module-level singletons (volume / mic / brightness / battery /
/// focused-output) keep running: they route by connector each emission, so a
/// fresh `install` re-keys the map and they self-heal. `SUBS_INSTALLED` stays
/// set so they wire exactly once for the process lifetime.
///
/// Tears down with `destroy()`, not `close()` (#632): an OSD that never
/// popped on this monitor is still unrealized, and `close()` neither
/// destroys an unrealized window nor drops GTK's internal toplevel
/// reference — only `destroy()` does, and it can't be vetoed by a
/// `close-request` handler.
pub fn close_all() {
    OSDS.with(|map| {
        // `take()` moves the whole map out (leaving `Default`) and releases
        // the borrow inside the call, rather than holding a `drain()` RefMut
        // across every `destroy()` below (#631) — a borrow held across a GTK
        // call is a latent reentrancy hazard if any emission it triggers is
        // ever synchronous.
        for (_, view) in map.take() {
            if let Some(sub) = view.drawer_sub.borrow_mut().take() {
                sub.abort();
            }
            // Cancel pending timers so they can't fire (holding a live `Rc`
            // clone of the view) into a destroyed window after teardown.
            if let Some(id) = view.timeout.take() {
                id.remove();
            }
            if let Some(id) = view.fade_out_timeout.take() {
                id.remove();
            }
            view.window.destroy();
        }
    });
}

/// Construct one `OsdView` for `monitor`. Pure widget construction —
/// signal wiring lives in [`install_subscriptions`] and runs once
/// regardless of monitor count.
fn build_osd_view(monitor: &Monitor) -> Rc<OsdView> {
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .margin(Margin {
            top: TOP_MARGIN,
            ..Margin::default()
        })
        .namespace("hytte-osd")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    window.add_css_class("ts-osd");
    install_click_through(&window);
    window.set_visible(false);

    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("ts-osd-card");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    header.add_css_class("ts-osd-header");

    let icon = gtk::Image::new();
    icon.add_css_class("ts-osd-icon");
    icon.set_pixel_size(crate::scale::scale(32));
    header.append(&icon);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.set_hexpand(true);
    let label = gtk::Label::new(None);
    label.add_css_class("ts-osd-label");
    label.set_xalign(0.0);
    column.append(&label);
    let value = gtk::Label::new(None);
    value.add_css_class("ts-osd-value");
    value.set_xalign(0.0);
    column.append(&value);
    header.append(&column);

    card.append(&header);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("ts-osd-progress");
    card.append(&progress);

    window.set_child(Some(&card));

    Rc::new(OsdView {
        window,
        card,
        icon,
        label,
        value,
        progress,
        timeout: Cell::new(None),
        fade_out_timeout: Cell::new(None),
        current_kind: Cell::new(None),
        current_muted: Cell::new(false),
        drawer_page: Cell::new(None),
        drawer_sub: RefCell::new(None),
    })
}

/// Set an empty input region on the OSD's surface so pointer events fall
/// through to whatever's underneath — the card is display-only (an
/// `Image`, two `Label`s, a `ProgressBar`) and takes no keyboard, so
/// nothing inside it ever needs a click. Without this, the card's
/// top-center footprint is a pointer black-hole for the ~1.8s it's shown.
///
/// The OSD is shown/hidden repeatedly via `set_visible` (see [`show`]),
/// so its surface remaps on every show and `connect_map` re-fires each
/// time. Mirrors the surface-timing shape of
/// `sidebar::wire_blur_attach` — `connect_map` plus an `is_mapped`
/// fallback for the (here moot, since we wire before the first show)
/// case where the surface is already mapped.
fn install_click_through(window: &gtk::Window) {
    use hytte::gtk::cairo;

    // `on_surface_ready` runs this on the first map and on every subsequent
    // remap (the OSD remaps on each `set_visible`), and immediately if the
    // surface is already up — see its docs for the map-once lore this folds in.
    hytte::ui::on_surface_ready(window, |surface| {
        surface.set_input_region(Some(&cairo::Region::create()));
    });
}

/// Wire the four module-level signal subscriptions exactly once on the
/// first [`install`] call, regardless of monitor count.
///
/// Bootstrap suppression: we don't want the OSD flashing on startup, so
/// volume, microphone and brightness each swallow emissions until their
/// first real reading, which seeds silently ([`StartupGuard`]); battery
/// seeds its diff baseline from its first emission.
fn install_subscriptions() {
    // ── Volume (default sink) ─────────────────────────────────────────
    //
    // Derived from `sinks()` the way the microphone below is from
    // `sources()`, not read off `default_sink()`: that handle collapses "no
    // default sink known yet" into `Volume::default()`, a real-looking 0 %,
    // so the startup guard could not tell its placeholder from a reading.
    // The value is the one the backend publishes there — the default sink's
    // averaged volume and its mute — and `VolumeGate` keeps another sink's
    // change from re-firing it. The default sink's *id* rides along with the
    // value: switching the default between two present sinks changes the
    // value without the user touching a volume key, so [`VolumeGate`] seeds
    // silently on an id change and only shows a change on the same sink.
    {
        let mut gate = VolumeGate::default();
        let volume = pipewire::sinks().map(|sinks| default_volume(&sinks));

        glib::MainContext::default().spawn_local(volume.for_each(
            move |reading: Option<(u32, Volume)>| {
                if let Some(v) = gate.admit(reading) {
                    route_show(&render_volume(v));
                }
                std::future::ready(())
            },
        ));
    }

    // ── Microphone (default source) ───────────────────────────────────
    //
    // No `default_source()` signal is exposed, so derive it locally by
    // looking for `is_default=true` in `sources()`. Wrapped in
    // `dedupe_cloned` so identical snapshots don't re-fire after
    // unrelated source list changes (e.g. a USB mic plug event when
    // the system mic is still the default).
    {
        let mut guard = StartupGuard::default();
        let combined = map_ref! {
            let sources = pipewire::sources() => {
                sources.iter().find(|s| s.is_default).cloned()
            }
        }
        .dedupe_cloned();

        glib::MainContext::default().spawn_local(combined.for_each(
            move |source: Option<Source>| {
                if guard.admits(source.is_some())
                    && let Some(state) = render_mic(source.as_ref())
                {
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // ── Brightness ────────────────────────────────────────────────────
    //
    // `None` until the service's first sysfs read finds a backlight.
    {
        let mut guard = StartupGuard::default();
        glib::MainContext::default().spawn_local(brightness::current().for_each(
            move |b: Option<Brightness>| {
                if guard.admits(b.is_some())
                    && let Some(b) = b
                {
                    route_show(&render_brightness(b));
                }
                std::future::ready(())
            },
        ));
    }

    // ── Battery ───────────────────────────────────────────────────────
    //
    // Edge + threshold detection. The first emission seeds a baseline
    // silently; subsequent emissions diff against it via
    // detect_battery_event.
    {
        let first = Cell::new(true);
        let last_battery: RefCell<Option<Battery>> = RefCell::new(None);
        glib::MainContext::default().spawn_local(upower::battery().for_each(
            move |batt: Battery| {
                if first.replace(false) {
                    *last_battery.borrow_mut() = Some(batt);
                    return std::future::ready(());
                }
                let prev_snapshot = last_battery.borrow().clone();
                if let Some(event) = detect_battery_event(prev_snapshot.as_ref(), &batt) {
                    let state = render_battery(event, &batt);
                    route_show(&state);
                }
                *last_battery.borrow_mut() = Some(batt);
                std::future::ready(())
            },
        ));
    }

    // ── Focused output ────────────────────────────────────────────────
    //
    // Wires the shared `components::focused_output` cache used by
    // `route_show` (idempotent — see its docs).
    focused_output::install();
}

/// Route `state` to the OSD on the focused monitor. Falls back to the
/// first mounted OSD when the focused output is unknown or not in the
/// map (e.g. niri startup, monitor disconnect).
fn route_show(state: &State) {
    let target_name: Option<String> = focused_output::current();
    // Clone the `Rc` out and let the borrow end at this `let` (#643). `show()`
    // is a run of GTK calls (`set_visible`, `present`, label/icon setters, timer
    // arming), and `install`/`close_all` are the `borrow_mut()` counterparties
    // on this cell — a shared borrow held across all that panics on any
    // re-entry, and a `BorrowMutError` through a glib callback aborts the
    // process. `OSDS` already stores `Rc<OsdView>`, so this is just a refcount
    // bump.
    let view = OSDS.with(|map| {
        let map = map.borrow();
        super::consent::output_for(&map, target_name.as_deref()).map(Rc::clone)
    });
    if let Some(view) = view {
        if drawer_shows_control(state.kind, view.drawer_page.get()) {
            return; // the drawer is showing this very control; OSD is redundant
        }
        show(&view, state);
    }
}

/// Whether the drawer page `shown` on an OSD's monitor already shows the live
/// control an OSD of `kind` would report — the one case [`route_show`]
/// swallows it as redundant.
///
/// Volume and microphone have their sliders on the Audio page, brightness on
/// the Power page; any other page (or none) hides nothing. Battery events and
/// leave-by nudges are alerts, not readouts of a slider the user is holding:
/// no page stands in for "critical battery" or "leave now", so they always
/// show.
fn drawer_shows_control(kind: Kind, shown: Option<Page>) -> bool {
    match kind {
        Kind::Volume | Kind::Mic => shown == Some(Page::Audio),
        Kind::Brightness => shown == Some(Page::Power),
        Kind::Battery | Kind::LeaveBy => false,
    }
}

/// The startup guard for one OSD signal: it swallows every emission up to
/// and including the first real reading, which seeds the baseline silently.
///
/// A service handle starts from a placeholder until its backend reports —
/// `brightness::current()` is `None`, and the default sink and source this
/// module derives are `None` while their lists are empty — so the first
/// *real* reading can land as the second emission, or later. The guard this
/// replaced swallowed exactly the first emission whatever it carried, which
/// let that reading through as a change and flashed an OSD at startup. Once
/// seeded the guard admits everything; a later placeholder (a device gone)
/// renders nothing at the call site, as before.
#[derive(Debug, Default)]
struct StartupGuard {
    seeded: bool,
}

impl StartupGuard {
    /// Feed one emission; `real` is whether it carries a reading rather than
    /// the placeholder. `true` when the emission may be shown.
    fn admits(&mut self, real: bool) -> bool {
        if self.seeded {
            return true;
        }
        self.seeded = real;
        false
    }
}

/// Decides which default-sink readings the volume OSD shows.
///
/// A reading is the default sink's id plus its volume and mute. The gate
/// keeps the last reading it saw and admits a new one only when it is on the
/// **same sink** and the value differs: the first reading seeds (the startup
/// rule [`StartupGuard` documents]), and a reading on a *different* sink
/// seeds too, because a switch between two present sinks changes the value
/// with no volume key involved. A volume change on the new sink right after
/// the switch compares against the seeded value, so it still shows. A
/// placeholder (`None`, no default sink) shows nothing and clears the
/// baseline, so whatever comes back next seeds: no volume key can reach the
/// default sink while there is none, and `PipeWire` reuses freed global ids,
/// so a sink returning under the old id is not evidence it is the same sink.
///
/// [`StartupGuard` documents]: StartupGuard
#[derive(Debug, Default)]
struct VolumeGate {
    last: Option<(u32, Volume)>,
}

impl VolumeGate {
    /// Feed one emission; the volume to show, if any.
    fn admit(&mut self, reading: Option<(u32, Volume)>) -> Option<Volume> {
        let Some((id, volume)) = reading else {
            // No default sink: whatever comes back next seeds, even under a
            // recycled id.
            self.last = None;
            return None;
        };
        match self.last.replace((id, volume)) {
            Some((last_id, last_volume)) if last_id == id && last_volume != volume => Some(volume),
            _ => None,
        }
    }
}

/// Raise a transient "leave-by" nudge — the public entry point behind the
/// plugin→shell `Effect::RaiseOsd` (#236). Generic and reusable: the caller
/// (any plugin, via the host effect broker) computes the display strings and
/// the shell just shows them. `title`
/// fills the bold `.ts-osd-label` line, `body` the `.ts-osd-value` readout, and
/// `icon` names a symbolic icon (defaulting to `LEAVE_BY_ICON` when `None`).
/// Routed onto the focused output like every other kind, with the longer
/// leave-by dwell. GTK-main-thread-only (the effect broker runs there).
pub fn nudge(title: &str, body: &str, icon: Option<&str>) {
    let state = State {
        kind: Kind::LeaveBy,
        icon: icon.unwrap_or(LEAVE_BY_ICON).to_owned(),
        // Leave-by carries no meaningful fraction; the `.leave-by` CSS blanks the
        // progress bar entirely, so this value is never actually shown.
        fraction: 0.0,
        label: title.to_owned(),
        value: body.to_owned(),
        muted: false,
    };
    route_show(&state);
}

thread_local! {
    /// Mounted OSD instances keyed by `gtk::Monitor.connector()`.
    /// `connector()` matches niri's `Workspace.output` (KMS connector
    /// names like `"DP-1"`, `"eDP-1"`).
    static OSDS: RefCell<BTreeMap<String, Rc<OsdView>>> =
        const { RefCell::new(BTreeMap::new()) };

    /// Set after the first `install()` call to ensure module-level
    /// signal subscriptions are wired exactly once across all
    /// per-monitor mounts.
    static SUBS_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Rendered OSD state — what to display once a signal fires.
struct State {
    kind: Kind,
    /// Named symbolic icon. `String` (not `&'static str`) so a plugin-supplied
    /// [`nudge`] icon can flow in alongside the built-in kinds' static names.
    icon: String,
    fraction: f64,
    /// Kind name shown in `.ts-osd-label` ("Volume" / "Microphone" /
    /// "Brightness" / a leave-by title). `String` so the dynamic
    /// plugin-computed nudge title (#236) can live here.
    label: String,
    /// Percent / "Muted" text shown in `.ts-osd-value`.
    value: String,
    muted: bool,
}

fn volume_icon(v: &Volume) -> &'static str {
    if v.muted {
        return "audio-volume-muted-symbolic";
    }
    let pct = pct(v.linear);
    match pct {
        0..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

fn mic_icon(s: &Source) -> &'static str {
    if s.muted {
        return "microphone-sensitivity-muted-symbolic";
    }
    let pct = pct(s.volume);
    if pct >= 50 {
        "microphone-sensitivity-high-symbolic"
    } else {
        "microphone-sensitivity-medium-symbolic"
    }
}

const LOW_THRESHOLD: f64 = 20.0;
const CRITICAL_THRESHOLD: f64 = 10.0;
const IMMINENT_THRESHOLD: f64 = 5.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatteryEvent {
    PluggedIn,
    Unplugged,
    LowBattery,
    CriticalBattery,
    ImminentShutdown,
}

fn detect_battery_event(prev: Option<&Battery>, curr: &Battery) -> Option<BatteryEvent> {
    let prev = prev?;

    if curr.state == BatteryState::Unknown || prev.state == BatteryState::Unknown {
        return None;
    }

    if prev.state == BatteryState::Discharging && curr.state == BatteryState::Charging {
        return Some(BatteryEvent::PluggedIn);
    }
    if prev.state == BatteryState::Charging && curr.state == BatteryState::Discharging {
        return Some(BatteryEvent::Unplugged);
    }

    if curr.state == BatteryState::Discharging && prev.state == BatteryState::Discharging {
        if prev.percentage > IMMINENT_THRESHOLD && curr.percentage <= IMMINENT_THRESHOLD {
            return Some(BatteryEvent::ImminentShutdown);
        }
        if prev.percentage > CRITICAL_THRESHOLD && curr.percentage <= CRITICAL_THRESHOLD {
            return Some(BatteryEvent::CriticalBattery);
        }
        if prev.percentage > LOW_THRESHOLD && curr.percentage <= LOW_THRESHOLD {
            return Some(BatteryEvent::LowBattery);
        }
    }

    None
}

/// The default sink's id, volume and mute out of the sink list — the value
/// the backend publishes as `pipewire::default_sink()` — or `None` while no
/// sink is the default, which `default_sink()` instead reports as a
/// real-looking `Volume::default()`. The id is what [`VolumeGate`] uses to tell
/// a volume change from a switch of default sink.
fn default_volume(sinks: &[Sink]) -> Option<(u32, Volume)> {
    sinks.iter().find(|s| s.is_default).map(|s| {
        (
            s.id,
            Volume {
                linear: s.volume,
                muted: s.muted,
            },
        )
    })
}

fn render_volume(v: Volume) -> State {
    let icon = volume_icon(&v);
    // Boosted volume can exceed 100%; show the true value in the label
    // while still clamping the progress bar to [0.0, 1.0].
    let pct = cast::f64_to_u32_trunc((v.linear * 100.0).round());
    let value = if v.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    State {
        kind: Kind::Volume,
        icon: icon.to_owned(),
        fraction: clamp01(v.linear),
        label: "Volume".to_owned(),
        value,
        muted: v.muted,
    }
}

fn render_mic(source: Option<&Source>) -> Option<State> {
    let s = source?;
    let icon = mic_icon(s);
    let pct = pct(s.volume);
    let value = if s.muted {
        "Muted".to_string()
    } else {
        format!("{pct}%")
    };
    Some(State {
        kind: Kind::Mic,
        icon: icon.to_owned(),
        fraction: clamp01(s.volume),
        label: "Microphone".to_owned(),
        value,
        muted: s.muted,
    })
}

fn render_brightness(b: Brightness) -> State {
    let pct = pct(b.level);
    State {
        kind: Kind::Brightness,
        icon: "display-brightness-symbolic".to_owned(),
        fraction: clamp01(b.level),
        label: "Brightness".to_owned(),
        value: format!("{pct}%"),
        muted: false,
    }
}

fn render_battery(event: BatteryEvent, batt: &Battery) -> State {
    let (icon, label, value) = match event {
        BatteryEvent::PluggedIn => (
            // Adwaita 49+ moved this to the `legacy/` set; `battery-good-charging`
            // is the closest equivalent that resolves cleanly in modern themes.
            "battery-good-charging-symbolic",
            "Charging",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::Unplugged => (
            "battery-symbolic",
            "On battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::LowBattery => (
            "battery-low-symbolic",
            "Low battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::CriticalBattery => (
            "battery-caution-symbolic",
            "Critical battery",
            format!("{:.0}%", batt.percentage),
        ),
        BatteryEvent::ImminentShutdown => (
            "battery-caution-symbolic",
            "Battery very low",
            format!("{:.0}%", batt.percentage),
        ),
    };
    State {
        kind: Kind::Battery,
        icon: icon.to_owned(),
        fraction: (batt.percentage / 100.0).clamp(0.0, 1.0),
        label: label.to_owned(),
        value,
        muted: false,
    }
}

/// Populate the OSD view with `state`, mark it visible, and arm the
/// auto-hide timeout. If a previous timeout was still pending, cancel
/// it so the OSD stays visible for another full `HIDE_AFTER_MS`.
fn show(view: &Rc<OsdView>, state: &State) {
    view.icon.set_icon_name(Some(state.icon.as_str()));
    view.progress.set_fraction(state.fraction);
    view.label.set_text(&state.label);
    view.value.set_text(&state.value);

    // Swap kind modifier class.
    let new_kind = state.kind.css_class();
    if let Some(old) = view.current_kind.replace(Some(new_kind))
        && old != new_kind
    {
        view.card.remove_css_class(old);
    }
    view.card.add_css_class(new_kind);

    // Toggle muted modifier.
    let was_muted = view.current_muted.replace(state.muted);
    if state.muted && !was_muted {
        view.card.add_css_class("muted");
    } else if !state.muted && was_muted {
        view.card.remove_css_class("muted");
    }

    // Make the window visible, then defer the .shown class flip by one
    // frame so GTK4's CSS engine sees the transition's "from" state
    // (opacity 0, margin-top 0) before the "to" state. Without the
    // idle defer, the same-frame visibility-and-class flip skips the
    // transition entirely.
    //
    // Cancel any in-flight fade-out: a new event landed before the
    // previous fade-out timer fired, so we keep the window visible
    // and re-arm.
    if let Some(prev) = view.fade_out_timeout.take() {
        prev.remove();
    }
    view.window.set_visible(true);
    let card_for_idle = view.card.clone();
    glib::idle_add_local_once(move || {
        card_for_idle.add_css_class("shown");
    });

    // Reset auto-hide. When the timer fires we DON'T set_visible(false)
    // directly; instead remove the .shown class to start the CSS fade,
    // then schedule a second timer to flip visibility once the
    // transition has finished.
    if let Some(prev) = view.timeout.take() {
        prev.remove();
    }
    let view_for_timeout = view.clone();
    let id = glib::timeout_add_local_once(
        Duration::from_millis(u64::from(state.kind.hide_after_ms())),
        move || {
            view_for_timeout.timeout.set(None);
            view_for_timeout.card.remove_css_class("shown");
            // Wait for the 280ms CSS transition + 20ms safety buffer
            // before actually hiding the layer-shell window.
            let view_for_fade = view_for_timeout.clone();
            let fade_id = glib::timeout_add_local_once(Duration::from_millis(300), move || {
                view_for_fade.fade_out_timeout.set(None);
                view_for_fade.window.set_visible(false);
            });
            view_for_timeout.fade_out_timeout.set(Some(fade_id));
        },
    );
    view.timeout.set(Some(id));
}

fn pct(linear: f64) -> u32 {
    cast::f64_to_u32_trunc((clamp01(linear) * 100.0).round())
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hytte::services::pipewire::{Source, Volume};

    fn vol(linear: f64, muted: bool) -> Volume {
        Volume { linear, muted }
    }

    fn src(volume: f64, muted: bool) -> Source {
        Source {
            id: 0,
            name: String::new(),
            description: String::new(),
            volume,
            muted,
            is_default: false,
        }
    }

    #[test]
    fn volume_icon_muted() {
        assert_eq!(volume_icon(&vol(0.5, true)), "audio-volume-muted-symbolic");
    }

    #[test]
    fn volume_icon_low_band() {
        assert_eq!(volume_icon(&vol(0.0, false)), "audio-volume-low-symbolic");
        assert_eq!(volume_icon(&vol(0.33, false)), "audio-volume-low-symbolic");
    }

    #[test]
    fn volume_icon_medium_band() {
        assert_eq!(
            volume_icon(&vol(0.5, false)),
            "audio-volume-medium-symbolic"
        );
        assert_eq!(
            volume_icon(&vol(0.66, false)),
            "audio-volume-medium-symbolic"
        );
    }

    #[test]
    fn volume_icon_high_band() {
        assert_eq!(volume_icon(&vol(0.67, false)), "audio-volume-high-symbolic");
        assert_eq!(volume_icon(&vol(1.0, false)), "audio-volume-high-symbolic");
    }

    #[test]
    fn mic_icon_muted() {
        assert_eq!(
            mic_icon(&src(0.5, true)),
            "microphone-sensitivity-muted-symbolic"
        );
    }

    #[test]
    fn mic_icon_high_band() {
        assert_eq!(
            mic_icon(&src(0.5, false)),
            "microphone-sensitivity-high-symbolic"
        );
        assert_eq!(
            mic_icon(&src(1.0, false)),
            "microphone-sensitivity-high-symbolic"
        );
    }

    #[test]
    fn mic_icon_medium_band() {
        assert_eq!(
            mic_icon(&src(0.0, false)),
            "microphone-sensitivity-medium-symbolic"
        );
        assert_eq!(
            mic_icon(&src(0.49, false)),
            "microphone-sensitivity-medium-symbolic"
        );
    }

    fn batt(percentage: f64, state: BatteryState) -> Battery {
        Battery {
            percentage,
            state,
            ..Battery::default()
        }
    }

    #[test]
    fn detect_plug_in() {
        let prev = batt(50.0, BatteryState::Discharging);
        let curr = batt(50.0, BatteryState::Charging);
        assert!(matches!(
            detect_battery_event(Some(&prev), &curr),
            Some(BatteryEvent::PluggedIn)
        ));
    }

    #[test]
    fn detect_unplug() {
        let prev = batt(80.0, BatteryState::Charging);
        let curr = batt(80.0, BatteryState::Discharging);
        assert!(matches!(
            detect_battery_event(Some(&prev), &curr),
            Some(BatteryEvent::Unplugged)
        ));
    }

    #[test]
    fn detect_low_threshold_cross() {
        let prev = batt(22.0, BatteryState::Discharging);
        let curr = batt(19.0, BatteryState::Discharging);
        assert!(matches!(
            detect_battery_event(Some(&prev), &curr),
            Some(BatteryEvent::LowBattery)
        ));
    }

    #[test]
    fn detect_critical_threshold_cross() {
        let prev = batt(11.0, BatteryState::Discharging);
        let curr = batt(9.0, BatteryState::Discharging);
        assert!(matches!(
            detect_battery_event(Some(&prev), &curr),
            Some(BatteryEvent::CriticalBattery)
        ));
    }

    #[test]
    fn detect_imminent_shutdown_threshold_cross() {
        let prev = batt(6.0, BatteryState::Discharging);
        let curr = batt(4.0, BatteryState::Discharging);
        assert!(matches!(
            detect_battery_event(Some(&prev), &curr),
            Some(BatteryEvent::ImminentShutdown)
        ));
    }

    #[test]
    fn no_event_on_steady_discharge() {
        let prev = batt(50.0, BatteryState::Discharging);
        let curr = batt(49.0, BatteryState::Discharging);
        assert!(detect_battery_event(Some(&prev), &curr).is_none());
    }

    #[test]
    fn no_event_on_unknown_state() {
        let prev = batt(50.0, BatteryState::Unknown);
        let curr = batt(50.0, BatteryState::Charging);
        assert!(detect_battery_event(Some(&prev), &curr).is_none());
    }

    // ── Which drawer page silences which OSD ─────────────────────────────

    /// Every drawer state the OSD can be handed: closed, then each page.
    fn every_drawer_state() -> impl Iterator<Item = Option<Page>> {
        std::iter::once(None).chain(Page::ALL.into_iter().map(Some))
    }

    /// Volume and microphone are silenced by the Audio page — where both of
    /// their sliders live — and by nothing else. Before the fix *any* open
    /// page silenced them, so the volume keys gave no feedback with Calendar
    /// open.
    ///
    /// **Falsification:** `Kind::Volume | Kind::Mic => shown.is_some()` (the
    /// pre-fix "any page" rule) reds on the first non-Audio page.
    #[test]
    fn volume_and_mic_are_quiet_only_under_the_audio_page() {
        for kind in [Kind::Volume, Kind::Mic] {
            for shown in every_drawer_state() {
                assert_eq!(
                    drawer_shows_control(kind, shown),
                    shown == Some(Page::Audio),
                    "{kind:?} with the drawer on {shown:?}",
                );
            }
        }
    }

    /// Brightness is silenced only by the Power page, which carries the
    /// brightness slider.
    #[test]
    fn brightness_is_quiet_only_under_the_power_page() {
        for shown in every_drawer_state() {
            assert_eq!(
                drawer_shows_control(Kind::Brightness, shown),
                shown == Some(Page::Power),
                "Brightness with the drawer on {shown:?}",
            );
        }
    }

    /// A battery alert (critical, imminent shutdown, plug/unplug) and a
    /// plugin's leave-by nudge show whatever the drawer shows — the Power
    /// page included, which shows the charge but raises no alert.
    ///
    /// **Falsification:** route `Kind::Battery` with `Kind::Brightness` → the
    /// Power page swallows a critical-battery OSD and this reds.
    #[test]
    fn battery_and_leave_by_are_never_silenced() {
        for kind in [Kind::Battery, Kind::LeaveBy] {
            for shown in every_drawer_state() {
                assert!(
                    !drawer_shows_control(kind, shown),
                    "{kind:?} swallowed with the drawer on {shown:?}",
                );
            }
        }
    }

    // ── The startup guard ────────────────────────────────────────────────

    fn sink(name: &str, volume: f64, muted: bool, is_default: bool) -> Sink {
        Sink {
            id: 0,
            name: name.to_owned(),
            description: String::new(),
            volume,
            muted,
            is_default,
        }
    }

    /// The volume the OSD reads is the default sink's, and a list with no
    /// default — the empty list a fresh handle starts from included — is the
    /// placeholder, not a 0 % reading. That is what lets the startup guard
    /// tell the two apart.
    #[test]
    fn default_volume_is_none_until_a_sink_is_the_default() {
        assert_eq!(
            default_volume(&[]),
            None,
            "the handle's startup placeholder"
        );
        assert_eq!(default_volume(&[sink("hdmi", 0.4, false, false)]), None);
        assert_eq!(
            default_volume(&[
                sink("hdmi", 0.4, false, false),
                sink("speakers", 0.7, true, true),
            ]),
            Some((0, vol(0.7, true))),
        );
        assert_eq!(
            default_volume(&[sink("silent", 0.0, false, true)]),
            Some((0, vol(0.0, false))),
            "a real 0 % default sink is a reading",
        );
    }

    fn reading(id: u32, linear: f64) -> (u32, Volume) {
        (id, vol(linear, false))
    }

    /// The gate's answers for a run of emissions, in order.
    fn gated(readings: &[Option<(u32, Volume)>]) -> Vec<Option<Volume>> {
        let mut gate = VolumeGate::default();
        readings.iter().map(|&r| gate.admit(r)).collect()
    }

    /// Switching the default between two present sinks at different volumes
    /// is not a volume change: it seeds silently. A change on the new sink
    /// right after still shows, and the same sink's change shows as before.
    #[test]
    fn a_default_sink_switch_seeds_silently_and_a_change_after_it_shows() {
        assert_eq!(
            gated(&[
                None,                  // placeholder
                Some(reading(1, 0.3)), // first real reading seeds
                Some(reading(1, 0.4)), // same sink, new value: shows
                Some(reading(2, 0.8)), // switch to sink 2: silent
                Some(reading(2, 0.8)), // repeat: nothing
                Some(reading(2, 0.9)), // change on the new sink: shows
                None,                  // device gone: nothing
                Some(reading(2, 0.9)), // back: seeds
                Some(reading(1, 0.4)), // switch back: silent
            ]),
            vec![
                None,
                None,
                Some(vol(0.4, false)),
                None,
                None,
                Some(vol(0.9, false)),
                None,
                None,
                None,
            ],
        );
    }

    /// Mute is part of the value: a mute toggle on the same sink shows.
    #[test]
    fn a_mute_toggle_on_the_same_sink_shows() {
        let mut gate = VolumeGate::default();
        assert_eq!(gate.admit(Some(reading(1, 0.5))), None);
        assert_eq!(gate.admit(Some((1, vol(0.5, true)))), Some(vol(0.5, true)));
    }

    /// A default sink that goes away and comes back seeds again, whatever
    /// its id says: `PipeWire` reuses freed global ids, so a different node
    /// (a Bluetooth profile switch re-creates its node) can return under the
    /// old one at a different volume, with no volume key pressed.
    #[test]
    fn a_sink_coming_back_after_none_seeds_silently() {
        assert_eq!(
            gated(&[Some(reading(1, 0.3)), None, Some(reading(1, 0.6))]),
            vec![None, None, None],
        );
    }

    /// The id the gate keys on is the *default* sink's own, read through
    /// `default_volume` from a real sink list. Every other fixture sink is
    /// id 0 and the gate tests hand-build their readings, so without this a
    /// `default_volume` that returned a constant id passed every test while
    /// the switch OSD came back (#1572 review).
    #[test]
    fn a_default_switch_through_default_volume_seeds_silently() {
        let at = |id, name: &str, volume, is_default| Sink {
            id,
            ..sink(name, volume, false, is_default)
        };
        let speakers = [at(41, "hdmi", 0.4, false), at(42, "speakers", 0.7, true)];
        let hdmi = [at(41, "hdmi", 0.4, true), at(42, "speakers", 0.7, false)];
        assert_eq!(default_volume(&hdmi).map(|(id, _)| id), Some(41));
        let mut gate = VolumeGate::default();
        assert_eq!(gate.admit(default_volume(&speakers)), None, "seeds");
        assert_eq!(
            gate.admit(default_volume(&hdmi)),
            None,
            "a switch is not a volume change"
        );
    }

    /// What `guard` answers for each emission in `real`, in order.
    fn admitted(real: &[bool]) -> Vec<bool> {
        let mut guard = StartupGuard::default();
        real.iter().map(|&r| guard.admits(r)).collect()
    }

    /// The startup flash: the handle's placeholder arrives first, then the
    /// backend's first real reading. That reading seeds the baseline; only the
    /// change after it shows.
    ///
    /// **Falsification:** the pre-fix "swallow exactly the first emission"
    /// guard admits the second emission here — the first real reading — and
    /// this reds.
    #[test]
    fn the_first_real_reading_after_a_placeholder_seeds_silently() {
        assert_eq!(admitted(&[false, true, true]), [false, false, true]);
        assert_eq!(
            admitted(&[false, false, true, true]),
            [false, false, false, true]
        );
    }

    /// A service that already had its reading when the OSD subscribed: the
    /// first emission is real and seeds, exactly as the old guard did.
    #[test]
    fn a_real_first_emission_seeds_like_before() {
        assert_eq!(admitted(&[true, true, true]), [false, true, true]);
    }

    /// Once seeded the guard steps aside: a later placeholder (a device gone)
    /// is admitted — the call site renders nothing for it — and the reading
    /// after it shows, as every emission after the first did before.
    #[test]
    fn a_seeded_guard_admits_everything_after() {
        assert_eq!(admitted(&[true, false, true]), [false, true, true]);
    }

    /// #1565: with niri's focus unknown, a volume OSD takes the output an
    /// `open-page` keybind opens Audio on (the first sorted connector), and
    /// the "drawer already shows it" suppression reads *that* output's drawer.
    /// The map stands in for `OSDS`, each value for a view's `drawer_page`.
    ///
    /// **Falsification:** `first_key_value` → `last_key_value` in
    /// `consent::output_for` puts the OSD on a closed-drawer output, so the
    /// volume OSD shows over the open Audio drawer and this reds. Reverting
    /// `OSDS` to a `HashMap` does not compile: `output_for` takes a `BTreeMap`.
    #[test]
    fn suppression_reads_the_drawer_output_when_focus_is_unknown() {
        use std::collections::BTreeMap;
        let connectors = ["HDMI-A-2", "eDP-1", "DP-3", "HDMI-A-1", "DP-2", "DP-1"];
        for rotation in 0..connectors.len() {
            let mut order = connectors.to_vec();
            order.rotate_left(rotation);
            let map: BTreeMap<String, Option<Page>> = order
                .iter()
                .map(|c| ((*c).to_owned(), (*c == "DP-1").then_some(Page::Audio)))
                .collect();
            let shown = *crate::overlays::consent::output_for(&map, None).expect("a mounted output");
            assert_eq!(shown, Some(Page::Audio), "the OSD must read DP-1's drawer");
            assert!(drawer_shows_control(Kind::Volume, shown));
        }
    }
}
