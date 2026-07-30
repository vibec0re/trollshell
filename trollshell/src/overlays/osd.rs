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
//! Bootstrap suppression: each of the three subscribed signals fires
//! immediately on install with the service's current snapshot. Without
//! suppression, opening the laptop with the bar starting up would flash
//! three OSDs in a row. We track one `Cell<bool>` per signal and silently
//! consume the FIRST emission; subsequent emissions show the OSD.
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
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;

use crate::components::{cast, focused_output};
use hytte::services::brightness::{self, Brightness};
use hytte::services::pipewire::{self, Source, Volume};
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
    /// Set to `true` while the modal drawer on this monitor is open.
    /// When true, `route_show` skips the OSD to avoid redundant noise
    /// while the user is interacting with the live slider.
    drawer_open: Cell<bool>,
    /// Per-monitor `drawer_open_signal` subscription handle. Unlike the
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
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::debug!("osd::install: monitor has no connector name; skipping");
            return;
        }
    };

    let view = build_osd_view(monitor);

    // Subscribe to the drawer-open state for this monitor so route_show
    // can suppress the OSD while the user is looking at the live slider.
    // The signal is backed by a lazily-allocated `Mutable` keyed by
    // connector, so this works even though `osd::install` runs before
    // `modal::install` during boot.
    //
    // Park the JoinHandle in the view: this is a PER-MONITOR subscription
    // (unlike the module-level singletons), so `close_all` must abort it on
    // teardown or it accumulates one per rebuild.
    let connector_for_sub = connector.clone();
    let drawer_sub = glib::MainContext::default().spawn_local(
        crate::modal::drawer_open_signal(monitor).for_each(move |open| {
            OSDS.with(|map| {
                if let Some(view) = map.borrow().get(&connector_for_sub) {
                    view.drawer_open.set(open);
                }
            });
            std::future::ready(())
        }),
    );
    view.drawer_sub.replace(Some(drawer_sub));

    OSDS.with(|map| {
        map.borrow_mut().insert(connector.clone(), view);
    });

    if !SUBS_INSTALLED.with(Cell::get) {
        SUBS_INSTALLED.with(|c| c.set(true));
        install_subscriptions();
    }
}

/// Close every OSD surface, abort its per-monitor `drawer_open_signal`
/// subscription, cancel any pending hide/fade timers, and drop the
/// per-monitor entries. Called before rebuilding on monitor hot-plug so a
/// vanished output's `OsdView` doesn't linger in [`OSDS`] — otherwise
/// `route_show`'s `map.values().next()` fallback could pop an OSD on a dead
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
        for (_, view) in map.borrow_mut().drain() {
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
        drawer_open: Cell::new(false),
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
/// Bootstrap suppression: each signal's first emission carries the
/// snapshot at install time. We don't want the OSD flashing on
/// startup, so silently consume that first event per signal.
fn install_subscriptions() {
    // ── Volume ────────────────────────────────────────────────────────
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(pipewire::default_sink().for_each(
            move |v: Volume| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                let state = render_volume(v);
                route_show(&state);
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
        let first = Cell::new(true);
        let combined = map_ref! {
            let sources = pipewire::sources() => {
                sources.iter().find(|s| s.is_default).cloned()
            }
        }
        .dedupe_cloned();

        glib::MainContext::default().spawn_local(combined.for_each(
            move |source: Option<Source>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(state) = render_mic(source.as_ref()) {
                    route_show(&state);
                }
                std::future::ready(())
            },
        ));
    }

    // ── Brightness ────────────────────────────────────────────────────
    {
        let first = Cell::new(true);
        glib::MainContext::default().spawn_local(brightness::current().for_each(
            move |b: Option<Brightness>| {
                if first.replace(false) {
                    return std::future::ready(());
                }
                if let Some(b) = b {
                    let state = render_brightness(b);
                    route_show(&state);
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
    OSDS.with(|map| {
        let map = map.borrow();
        if map.is_empty() {
            return;
        }
        let view = target_name
            .as_ref()
            .and_then(|name| map.get(name))
            .or_else(|| map.values().next());
        if let Some(view) = view {
            if view.drawer_open.get() {
                return; // drawer is showing the same control; OSD is redundant
            }
            show(view, state);
        }
    });
}

/// Raise a transient "leave-by" nudge — the public entry point behind the
/// plugin→shell `Effect::RaiseOsd` (#236). Generic and reusable: the caller
/// (any plugin, via the host effect broker) computes the display strings and
/// the shell just shows them. `title`
/// fills the bold `.ts-osd-label` line, `body` the `.ts-osd-value` readout, and
/// `icon` names a symbolic icon (defaulting to [`LEAVE_BY_ICON`] when `None`).
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
    static OSDS: RefCell<HashMap<String, Rc<OsdView>>> =
        RefCell::new(HashMap::new());

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
}
