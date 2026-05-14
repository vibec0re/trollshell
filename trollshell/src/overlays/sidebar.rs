//! Per-monitor pushable left sidebar. Layer-shell window anchored
//! `Left + Top + Bottom`; toggles via `widgets::sidebar_toggle`. When open,
//! reserves space via `exclusive_zone` so niri reflows tiles right; the
//! frame's draw fn reads `current_visible_width` and offsets the workspace
//! cutout's left edge in lockstep with the slide animation.
//!
//! State is per-connector, mirroring `modal::DRAWER_OPEN`. Subscribers (the
//! sidebar surface, the frame draw, future bar-CSS bindings) read
//! `open_signal`; the chip writes via `toggle`.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::prelude::*;

/// Width of the sidebar surface when fully open, in CSS px. Matches the
/// "frame border ~220px" geometry from the spec; the frame's cutout left
/// edge animates from `FRAME_THICKNESS` (8) up to this value while the
/// sidebar reveals.
pub const SIDEBAR_WIDTH: i32 = 220;

/// Frame-strut thickness, duplicated from `frame.rs` so this module stays
/// self-contained. Keep in sync with `frame.rs::FRAME_THICKNESS`.
const FRAME_THICKNESS_I32: i32 = 8;

thread_local! {
    /// Per-connector open/closed bool. Subscribers connect at `install` time
    /// or earlier (e.g., the frame); writers go through `toggle`.
    static SIDEBAR_OPEN: RefCell<HashMap<String, Mutable<bool>>> = RefCell::new(HashMap::new());
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
fn current_visible_width_for_key(_key: &str) -> i32 {
    // Real implementation lands in Task 4 when PANELS exists. For now,
    // always return the frame's default left inset.
    FRAME_THICKNESS_I32
}

/// True when the sidebar's revealer animation is at rest on `monitor`
/// (fully open or fully closed). The frame's tick callback uses this to
/// know when to stop redrawing after the slide finishes.
pub fn is_settled(monitor: &Monitor) -> bool {
    is_settled_for_key(&monitor_key(monitor))
}

/// Internal: keyed lookup used by both the public API and tests.
fn is_settled_for_key(_key: &str) -> bool {
    // Real implementation lands in Task 4. With no panel installed there
    // is nothing animating, so we report settled.
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_is_220() {
        // Frame integration assumes this exact value when computing how
        // much the cutout's left edge moves. Guard against accidental edits.
        assert_eq!(SIDEBAR_WIDTH, 220);
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
