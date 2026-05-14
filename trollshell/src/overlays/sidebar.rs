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
            .map(|_p| SIDEBAR_WIDTH)
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
#[allow(clippy::too_many_lines)]
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);
    let open_state = sidebar_open_state(&key);

    // Layer-shell window: anchored Left + Top + Bottom so the surface
    // spans the full screen height and exclusive_zone reserves on the
    // single (Left) edge — well-defined push semantics.
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
    // Fixed surface width so niri sees a stable 320-wide column when open.
    window.set_size_request(SIDEBAR_WIDTH, -1);

    // Revealer drives the slide animation. SlideRight pushes the card out
    // from the screen's left edge, in time with niri's tile reflow.
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(0);
    revealer.set_reveal_child(false);
    revealer.set_halign(gtk::Align::Fill);
    revealer.set_valign(gtk::Align::Fill);

    // Card — vertical box that holds the placeholder label (Phase 1) and
    // future content (Phase 2+). Fills the entire 320-px surface: no margins
    // so there is no gap around the dark area. The bar (Layer::Top, mapped
    // after sidebar) naturally paints over y=0..44, so no top margin is needed.
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("ts-sidebar");
    card.set_halign(gtk::Align::Fill);
    card.set_hexpand(true);
    card.set_valign(gtk::Align::Fill);
    card.set_vexpand(true);

    let placeholder = gtk::Label::new(Some("sidebar"));
    placeholder.add_css_class("ts-sidebar-placeholder");
    placeholder.set_halign(gtk::Align::Center);
    placeholder.set_valign(gtk::Align::Center);
    placeholder.set_vexpand(true);
    card.append(&placeholder);

    revealer.set_child(Some(&card));
    window.set_child(Some(&revealer));
    window.set_visible(false);

    // After the close animation finishes, drop the exclusive zone and hide
    // the surface. (When the open animation finishes, child_revealed flips
    // to true — we don't need to do anything.) Cross-check open_state so
    // that a rapid open→close→open doesn't let the stale close-completion
    // tear down state the re-open has already set.
    let open_state_for_notify = open_state.clone();
    let window_for_settled = window.clone();
    revealer.connect_child_revealed_notify(move |r| {
        if !r.is_child_revealed() && !open_state_for_notify.get() {
            window_for_settled.set_exclusive_zone(0);
            window_for_settled.set_visible(false);
        }
    });

    // Drive open/close transitions from the shared mutable. Toggle flips
    // the bool; this subscription does the surface work. Capture the
    // JoinHandle so close_all can abort the subscription before closing the
    // window, preventing a zombie subscription from firing into a dead window
    // after a hot-plug cycle.
    let window_for_open = window.clone();
    let revealer_for_open = revealer.clone();
    let subscription =
        glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
            if open {
                window_for_open.set_visible(true);
                window_for_open.present();
                window_for_open.set_exclusive_zone(SIDEBAR_WIDTH - FRAME_THICKNESS_I32);
                revealer_for_open.set_reveal_child(true);
            } else {
                // Start the close animation. Surface stays visible + zone stays
                // reserved until the revealer reports it has fully collapsed
                // (see connect_child_revealed_notify above) — otherwise niri
                // reclaims the space while the card is still on-screen.
                revealer_for_open.set_reveal_child(false);
            }
            async {}
        }));

    // ESC → close. Bound to the sidebar window so it fires when the
    // sidebar has keyboard focus (KeyboardMode::OnDemand).
    let key_ctrl = gtk::EventControllerKey::new();
    let monitor_for_esc = monitor.clone();
    key_ctrl.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            toggle(&monitor_for_esc);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);

    // Stash the panel so accessors can find it.
    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key.clone(),
            SidebarPanel {
                window: window.clone(),
                revealer: revealer.clone(),
                open_state: open_state.clone(),
                subscription,
            },
        );
    });
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
