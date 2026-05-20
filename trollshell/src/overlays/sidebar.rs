//! Per-monitor pushable left sidebar. Layer-shell window anchored
//! `Left + Top + Bottom` on `Layer::Top`; toggles via `widgets::sidebar_toggle`.
//! When open, reserves space via `exclusive_zone` so niri reflows tiles right;
//! the frame's draw fn reads `current_visible_width` and offsets the outer rect's
//! left edge so it never paints over the sidebar's region.
//!
//! The sidebar sits on `Layer::Top` (below the frame's `Layer::Overlay`). The
//! frame carves out the sidebar's region from its own paint, so the sidebar's
//! surface shows through regardless of compositor stacking order. Fullscreen
//! apps on `Layer::Top`'s level naturally cover the sidebar — no explicit hide
//! logic needed.
//!
//! State is per-connector, mirroring `modal::DRAWER_OPEN`. Subscribers (the
//! sidebar surface, the frame draw, future bar-CSS bindings) read
//! `open_signal`; the chip writes via `toggle`.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::ui::{layer_window, Anchor, Layer, LayerShell};

/// Width of the sidebar surface when fully open, in CSS px. Matches the
/// "frame border ~320px" geometry from the spec; the frame's cutout left
/// edge animates from `FRAME_THICKNESS` (8) up to this value while the
/// sidebar reveals.
pub const SIDEBAR_WIDTH: i32 = 320;

/// Frame-strut thickness, duplicated from `frame.rs` so this module stays
/// self-contained. Keep in sync with `frame.rs::FRAME_THICKNESS`.
const FRAME_THICKNESS_I32: i32 = 8;

thread_local! {
    /// Per-connector open/closed bool. Subscribers connect at `install` time
    /// or earlier (e.g., the frame); writers go through `toggle`.
    static SIDEBAR_OPEN: RefCell<HashMap<String, Mutable<bool>>> = RefCell::new(HashMap::new());
}

thread_local! {
    /// Per-connector sidebar surface handle. Populated by `install`;
    /// read by `current_visible_width_for_key` and `is_settled_for_key`.
    static PANELS: RefCell<HashMap<String, SidebarPanel>> = RefCell::new(HashMap::new());
}

struct SidebarPanel {
    window: gtk::Window,
    revealer: gtk::Revealer,
    open_state: Mutable<bool>,
    subscription: glib::JoinHandle<()>,
    /// Natural horizontal size of the surface, measured once after the
    /// widget tree is built. `set_size_request(SIDEBAR_WIDTH, -1)` is a
    /// *minimum* — the calendar widget's natural width plus
    /// `.ts-sidebar`'s 12 px padding regularly lifts the surface past
    /// 320. Using this measured value for both `exclusive_zone` and
    /// `current_visible_width` keeps niri's tile inset and the frame's
    /// cutout aligned with whatever the surface actually committed to.
    surface_width: i32,
}

fn monitor_key(m: &Monitor) -> String {
    m.connector()
        .unwrap_or_else(|| format!("monitor:{:p}", m.gdk()))
}

fn sidebar_open_state(key: &str) -> Mutable<bool> {
    SIDEBAR_OPEN.with(|map| {
        map.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| Mutable::new(false))
            .clone()
    })
}

/// Signal that emits the sidebar open/closed state for `monitor`. Backed by
/// [`SIDEBAR_OPEN`] so callers can subscribe before `install` has run for
/// this monitor (e.g., the frame wires up during early bootstrap).
pub fn open_signal(monitor: &Monitor) -> impl Signal<Item = bool> + 'static {
    sidebar_open_state(&monitor_key(monitor)).signal()
}

/// Flip the open state for `monitor`. Bar chip calls this on click.
pub fn toggle(monitor: &Monitor) {
    let state = sidebar_open_state(&monitor_key(monitor));
    let now = state.get();
    state.set(!now);
}

/// Currently visible width of the sidebar card on `monitor`, in CSS px.
/// Returns `FRAME_THICKNESS` when the sidebar is closed, hasn't been
/// installed yet, or the per-monitor panel is missing. The frame uses
/// this to compute its cutout's left edge each animation tick.
pub fn current_visible_width(monitor: &Monitor) -> i32 {
    current_visible_width_for_key(&monitor_key(monitor))
}

/// Internal: keyed lookup used by both the public API and tests.
fn current_visible_width_for_key(key: &str) -> i32 {
    PANELS.with(|panels| {
        panels
            .borrow()
            .get(key)
            .filter(|p| p.open_state.get())
            .map(|p| p.surface_width)
            .unwrap_or(FRAME_THICKNESS_I32)
    })
}

/// True when the sidebar's revealer animation is at rest on `monitor`
/// (fully open or fully closed). The frame's tick callback uses this to
/// know when to stop redrawing after the slide finishes.
pub fn is_settled(monitor: &Monitor) -> bool {
    is_settled_for_key(&monitor_key(monitor))
}

/// Internal: keyed lookup used by both the public API and tests.
fn is_settled_for_key(key: &str) -> bool {
    PANELS.with(|panels| {
        panels
            .borrow()
            .get(key)
            .map(|p| p.revealer.is_child_revealed() == p.open_state.get())
            .unwrap_or(true)
    })
}

/// Build the sidebar surface for one monitor, mount it as a hidden
/// layer-shell window, and wire its open-state subscription. Mirrors
/// `modal::install` in shape; called from `main.rs` per monitor.
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);
    let open_state = sidebar_open_state(&key);

    let window = build_sidebar_window(monitor, &key);
    let revealer = build_revealer();
    let card = build_card(monitor);
    revealer.set_child(Some(&card));
    window.set_child(Some(&revealer));
    window.set_visible(false);

    // Measure the natural surface width now that the widget tree is wired
    // up. Used for both `exclusive_zone` (niri tile inset matches the
    // surface's actual right edge given the 8 px frame strut) and
    // `current_visible_width` (frame cutout left edge aligns). Floor at
    // SIDEBAR_WIDTH so smaller-than-expected natural widths still reveal
    // the spec'd 320 px column.
    let (_, nat_w, _, _) = window.measure(gtk::Orientation::Horizontal, -1);
    let surface_width = nat_w.max(SIDEBAR_WIDTH);

    wire_close_finish(&revealer, &window, &open_state);
    let subscription = wire_open_subscription(&window, &revealer, &open_state, surface_width);
    wire_escape(&window, monitor.clone());

    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key,
            SidebarPanel {
                window,
                revealer,
                open_state,
                subscription,
                surface_width,
            },
        );
    });
}

/// Layer-shell window anchored Left + Top + Bottom — full screen height,
/// `exclusive_zone` reserves on the single Left edge for well-defined push
/// semantics. Fixed `SIDEBAR_WIDTH` so niri sees a stable column on open.
fn build_sidebar_window(monitor: &Monitor, key: &str) -> gtk::Window {
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Left)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .namespace(format!("hytte-sidebar-{key}"))
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .build();
    window.add_css_class("ts-sidebar-surface");
    window.set_size_request(SIDEBAR_WIDTH, -1);
    window
}

/// SlideRight revealer that pushes the card out from the screen's left edge
/// in time with niri's tile reflow.
fn build_revealer() -> gtk::Revealer {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(0);
    revealer.set_reveal_child(false);
    revealer.set_halign(gtk::Align::Fill);
    revealer.set_valign(gtk::Align::Fill);
    revealer
}

/// Card fills the entire surface — no margins so there's no gap around the
/// dark area. The bar (Layer::Top, mapped after sidebar) naturally paints
/// over y=0..44, so no top margin is needed.
fn build_card(monitor: &Monitor) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("ts-sidebar");
    card.set_halign(gtk::Align::Fill);
    card.set_hexpand(true);
    card.set_valign(gtk::Align::Fill);
    card.set_vexpand(true);

    card.append(&crate::widgets::calendar::widget(monitor));
    card.append(&crate::widgets::departures::widget());
    card
}

/// After the close animation finishes, drop the exclusive zone and hide the
/// surface. Cross-check `open_state` so a rapid open→close→open doesn't let
/// the stale close-completion tear down state the re-open already set.
fn wire_close_finish(revealer: &gtk::Revealer, window: &gtk::Window, open_state: &Mutable<bool>) {
    let open_state = open_state.clone();
    let window = window.clone();
    revealer.connect_child_revealed_notify(move |r| {
        if !r.is_child_revealed() && !open_state.get() {
            window.set_exclusive_zone(0);
            window.set_visible(false);
        }
    });
}

/// Drive open/close transitions from the shared mutable. Returns the
/// `JoinHandle` so `close_all` can abort the subscription before closing the
/// window — prevents a zombie subscription firing into a dead window after a
/// hot-plug cycle.
fn wire_open_subscription(
    window: &gtk::Window,
    revealer: &gtk::Revealer,
    open_state: &Mutable<bool>,
    surface_width: i32,
) -> glib::JoinHandle<()> {
    let window = window.clone();
    let revealer = revealer.clone();
    glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
        if open {
            // Order matters: set `exclusive_zone` in the LayerShell state
            // *before* the surface is created via set_visible + present.
            // gtk4-layer-shell stores the value and applies it to the
            // surface's initial commit, so niri sees the correct
            // reservation on the first configure. Setting it after
            // present() leaves the first commit at the default (0), and
            // the subsequent update isn't reliably honored — that was the
            // "works only the first time" symptom of the previous lifecycle.
            window.set_exclusive_zone(surface_width + FRAME_THICKNESS_I32);
            window.set_visible(true);
            window.present();
            revealer.set_reveal_child(true);
            hytte::services::departures::refresh();
        } else {
            // Start the close animation. Surface stays visible + zone stays
            // reserved until the revealer reports it has fully collapsed
            // (see `wire_close_finish` above) — otherwise niri reclaims the
            // space while the card is still on-screen.
            revealer.set_reveal_child(false);
        }
        async {}
    }))
}

/// ESC → close. Bound to the sidebar window so it fires when the sidebar has
/// keyboard focus (`KeyboardMode::OnDemand`).
fn wire_escape(window: &gtk::Window, monitor: Monitor) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            toggle(&monitor);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

/// Close every sidebar surface and drop the per-monitor entries. Called
/// before rebuilding bars on hot-plug, so stale layer-shell windows don't
/// linger after a monitor disappears.
pub fn close_all() {
    PANELS.with(|panels| {
        for (_, panel) in panels.borrow_mut().drain() {
            // Abort the subscription first so it cannot dispatch into the
            // (about to be closed) window. Then reset the bool so any other
            // subscribers see the closed state, and finally tear down the
            // surface.
            panel.subscription.abort();
            panel.open_state.set(false);
            panel.window.close();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_is_320() {
        // Frame integration assumes this exact value when computing how
        // much the cutout's left edge moves. Guard against accidental edits.
        assert_eq!(SIDEBAR_WIDTH, 320);
    }

    #[test]
    fn sidebar_open_state_is_keyed_per_connector() {
        let a = sidebar_open_state("DP-1");
        let b = sidebar_open_state("DP-1");
        let c = sidebar_open_state("HDMI-A-1");
        // Same key → same Mutable handle (clone of the Arc inside).
        a.set(true);
        assert!(b.get());
        // Different key → independent state.
        assert!(!c.get());
    }

    /// When no sidebar surface has been installed yet (or the connector is
    /// unknown), `current_visible_width` must return `FRAME_THICKNESS_I32`
    /// so the frame's cutout draws at its default left edge.
    #[test]
    fn current_visible_width_defaults_to_frame_thickness_when_no_panel() {
        // No PANELS map yet, no install() call — the frame might query us
        // during early bootstrap. Use a fake monitor key directly via the
        // private fallback path.
        assert_eq!(current_visible_width_for_key("nonexistent"), FRAME_THICKNESS_I32);
    }

    #[test]
    fn is_settled_defaults_to_true_when_no_panel() {
        // Same situation: no panel installed → nothing animating → settled.
        assert!(is_settled_for_key("nonexistent"));
    }
}
