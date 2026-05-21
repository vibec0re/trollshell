//! Per-monitor pushable left sidebar. Layer-shell window anchored
//! `Left + Top + Bottom` on `Layer::Top`; toggles via `widgets::sidebar_toggle`.
//!
//! ## Persistence + z-order
//!
//! The surface is created **once** at install time and stays alive for the
//! process lifetime. `widgets::sidebar_toggle` only flips the open state
//! `Mutable`; the surface itself is never hidden, recreated, or re-presented.
//! Within `Layer::Top`, z-order is fixed by surface creation order — install
//! runs before `Bar::new().show()`, so the sidebar sits **below** the bar.
//! Re-presenting the sidebar on each open used to bump it above the bar and
//! produced a ~44 px overlap on the bar's bottom edge.
//!
//! ## Animation + exclusive zone
//!
//! `GtkRevealer` (`SlideRight`) animates the card's allocated width between 0
//! and `SIDEBAR_WIDTH` on open/close. The exclusive zone is set **explicitly**
//! from the open-state subscription (`SIDEBAR_WIDTH` open, `0` closed) rather
//! than driven by `auto_exclusive_zone_enable()` — the auto path failed to
//! reclaim space cleanly on close (the bar stayed pushed even after the
//! revealer settled at 0 width), so we drive it directly. Niri snaps tiles +
//! the bar to the new value immediately; the revealer's slide is cosmetic.
//!
//! ## Frame integration
//!
//! The frame overlay (`Layer::Overlay`, above the bar) reads
//! [`current_visible_width`] each animation tick and shifts its cutout's
//! left edge to match — the sidebar surface (below the frame) shows
//! through the cutout. `SIDEBAR_WIDTH` is the authoritative width on both
//! sides.
//!
//! State is per-connector, mirroring `modal::DRAWER_OPEN`. Subscribers (the
//! sidebar surface, the frame draw, future bar-CSS bindings) read
//! `open_signal`; the chip writes via `toggle`.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, cairo, gdk, glib};
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
/// Returns `FRAME_THICKNESS_I32` when the sidebar is closed, hasn't been
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
            .map_or(FRAME_THICKNESS_I32, |_| SIDEBAR_WIDTH)
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
            .is_none_or(|p| p.revealer.is_child_revealed() == p.open_state.get())
    })
}

/// Build the sidebar surface for one monitor, mount it as a layer-shell
/// window, and wire its open-state subscription. Mirrors `modal::install`
/// in shape; called from `main.rs` per monitor — must run **before** the
/// bar's `Bar::new().show()` so the sidebar surface stays below the bar
/// in z-order (`Layer::Top` orders by creation, not by re-commit).
pub fn install(monitor: &Monitor) {
    let key = monitor_key(monitor);
    let open_state = sidebar_open_state(&key);

    let window = build_sidebar_window(monitor, &key);
    let revealer = build_revealer();
    let card = build_card(monitor);
    let clamp = adw::Clamp::builder()
        .maximum_size(SIDEBAR_WIDTH)
        .tightening_threshold(SIDEBAR_WIDTH)
        .child(&card)
        .build();
    revealer.set_child(Some(&clamp));
    window.set_child(Some(&revealer));
    // Present the surface ONCE, here at install. Stays alive for the
    // process lifetime — toggle goes through the revealer + open_state,
    // never through set_visible/present. See module-level note on z-order.
    window.set_visible(true);
    // Start clickthrough — the persistent surface keeps a full input
    // region by default even after the revealer collapses to 0 width,
    // so without this the closed sidebar's region still swallows clicks.
    apply_input_passthrough(&window, false);

    let subscription = wire_open_subscription(&window, &revealer, &open_state);
    wire_escape(&window, monitor.clone());

    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key,
            SidebarPanel {
                window,
                revealer,
                open_state,
                subscription,
            },
        );
    });
}

/// Layer-shell window anchored Left + Top + Bottom — full screen height,
/// `exclusive_zone` reserves on the single Left edge for well-defined push
/// semantics. **No `set_size_request`** — the window's natural width is
/// driven by the revealer's allocated child width, which animates between
/// 0 (closed) and `SIDEBAR_WIDTH` (open). The zone itself is set explicitly
/// from the open subscription, not via auto — see module-level note.
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
    window.set_exclusive_zone(0);
    window
}

/// `SlideRight` revealer that pushes the card out from the screen's left edge
/// in time with niri's tile reflow. The 180 ms duration matches the modal
/// drawer's slide so both surfaces feel like one design system.
fn build_revealer() -> gtk::Revealer {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(180);
    revealer.set_reveal_child(false);
    revealer.set_halign(gtk::Align::Start);
    revealer.set_valign(gtk::Align::Fill);
    revealer
}

/// Card has a fixed `SIDEBAR_WIDTH` so when the revealer is fully expanded
/// the surface settles at exactly that width — and at exactly 0 when fully
/// collapsed. No margins so there's no gap around the dark area. The bar
/// (`Layer::Top`, mapped after sidebar) naturally paints over y=0..44 in
/// the overlap, so no top margin is needed.
///
/// `set_size_request` only sets the **minimum** width, so a child with a
/// pathological natural width (e.g. a long calendar event title, or an
/// `AdwActionRow` subtitle that doesn't wrap) would otherwise push the
/// card — and the layer-shell surface above it — past `SIDEBAR_WIDTH`,
/// visually overlapping niri tiles, the bar, and the frame. The
/// `AdwClamp` wrapping this card in `install` caps the natural width at
/// `SIDEBAR_WIDTH`; see also `components::layout::finish_page` for the
/// same belt-and-suspenders pattern in the drawer.
fn build_card(monitor: &Monitor) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("ts-sidebar");
    card.set_size_request(SIDEBAR_WIDTH, -1);
    card.set_halign(gtk::Align::Fill);
    card.set_hexpand(false);
    card.set_valign(gtk::Align::Fill);
    // vexpand so the card stretches to the full sidebar height — needed
    // for the spacer below to actually have slack to absorb, which is
    // what anchors the departures widget to the bottom edge.
    card.set_vexpand(true);

    card.append(&crate::widgets::calendar::widget(monitor));
    card.append(&crate::widgets::tasks::widget(monitor));

    // Flex gap: eats whatever vertical space the calendar + tasks
    // didn't claim, so the departures widget settles against the
    // bottom edge of the sidebar instead of floating in the middle.
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    card.append(&spacer);

    card.append(&crate::widgets::departures::widget());
    card
}

/// Drive open/close transitions from the shared mutable. The surface stays
/// alive across toggles (see module note on z-order); we flip the revealer,
/// the exclusive zone, AND the surface's input region in lockstep. Niri
/// snaps tiles + bar to the new zone immediately; the revealer's slide is
/// cosmetic.
///
/// Returns the `JoinHandle` so `close_all` can abort the subscription
/// before closing the window — prevents a zombie subscription firing into
/// a dead window after a hot-plug cycle.
fn wire_open_subscription(
    window: &gtk::Window,
    revealer: &gtk::Revealer,
    open_state: &Mutable<bool>,
) -> glib::JoinHandle<()> {
    let window = window.clone();
    let revealer = revealer.clone();
    glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
        window.set_exclusive_zone(if open { SIDEBAR_WIDTH } else { 0 });
        revealer.set_reveal_child(open);
        apply_input_passthrough(&window, !open);
        if open {
            hytte::services::departures::refresh();
        }
        async {}
    }))
}

/// Toggle the surface's input region so the closed sidebar's persistent
/// layer-shell surface doesn't swallow pointer events in the (revealer-
/// shrunk) sidebar region. `passthrough == true` → empty region (clicks
/// fall through to niri tiles below); `false` → full surface accepts
/// input. Mirrors the click-through pattern in `frame::install_click_through`.
fn apply_input_passthrough(window: &gtk::Window, passthrough: bool) {
    let Some(surface) = window.surface() else {
        return;
    };
    if passthrough {
        let empty = cairo::Region::create();
        surface.set_input_region(Some(&empty));
    } else {
        surface.set_input_region(None);
    }
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
