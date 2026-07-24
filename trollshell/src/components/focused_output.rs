//! Single shared "niri's currently focused output" cache (#440).
//!
//! Three consumers each need to know which monitor niri currently has
//! focused, to route a monitor-less event (an OSD toast, a notification
//! toast, a keybind-driven `open-page` `GAction`) onto the right surface:
//! `overlays::osd`, `overlays::notifications`, and `commands`. Each used to
//! hand-roll the same shape — a `thread_local! { static FOCUSED_OUTPUT:
//! RefCell<Option<String>> }` plus its own `niri::focused_output()`
//! subscription writing into it. This module is the one shared cache:
//! [`install`] wires the subscription exactly once for the process
//! lifetime (idempotent — there's no single boot-order-guaranteed call
//! site, so every consumer calls it from its own setup path), and
//! [`current`] reads the latest known value.
//!
//! No bootstrap suppression: unlike the OSD's media/brightness/battery
//! signals, callers want the latest known focused output even before the
//! first niri focus event lands (matching all three prior copies' behavior).

use std::cell::{Cell, RefCell};

use hytte::gtk::glib;
use hytte::prelude::*;
use hytte::services::niri;

thread_local! {
    /// Most recent focused-output connector name (e.g. `"DP-1"`) from
    /// [`hytte::services::niri::focused_output`]. Updated by the
    /// module-level subscription wired in [`install`].
    static FOCUSED_OUTPUT: RefCell<Option<String>> = const { RefCell::new(None) };

    /// Set after the first [`install`] call so the subscription wires
    /// exactly once regardless of how many consumers call it.
    static SUBSCRIBED: Cell<bool> = const { Cell::new(false) };
}

/// Wire the module-level `niri::focused_output()` subscription feeding
/// [`current`], exactly once for the process lifetime. Cheap and safe to
/// call from every consumer's own setup path (`overlays::osd::install`,
/// `overlays::notifications::install`, `commands::install`) — only the
/// first call actually spawns the subscription; later calls no-op.
pub(crate) fn install() {
    if SUBSCRIBED.with(Cell::get) {
        return;
    }
    SUBSCRIBED.with(|c| c.set(true));
    glib::MainContext::default().spawn_local(niri::focused_output().for_each(|out| {
        FOCUSED_OUTPUT.with(|c| *c.borrow_mut() = out);
        std::future::ready(())
    }));
}

/// The most recently seen focused-output connector name, or `None` when
/// unknown (niri startup, no focused workspace) or before [`install`] has
/// ever been called.
pub(crate) fn current() -> Option<String> {
    FOCUSED_OUTPUT.with(|c| c.borrow().clone())
}
