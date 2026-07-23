//! Fire a fresh scan when the sidebar opens, scoped to the widget that asked
//! for it.
//!
//! The sidebar calendar and tasks widgets each want a fresh data scan the
//! moment the user opens the sidebar, so they never show up-to-60-second-stale
//! data on open. Both subscribe to the per-monitor
//! [`sidebar::open_signal`](crate::overlays::sidebar::open_signal), which is
//! backed by a *persistent* per-connector `Mutable` that outlives any single
//! widget. The widgets themselves are rebuilt on every `monitors_changed`
//! emission (`sidebar::build_card` runs on each `install`, after `close_all`
//! drops the old card), so a raw `spawn_local` subscription that isn't tied to
//! the widget accumulates one dead loop — and one redundant `refresh()` per
//! open — for every dock/undock cycle (#439).
//!
//! [`on_open`] routes the subscription through [`bind`], whose apply-loop holds
//! only a `WeakRef` to the anchor. When the card is torn down the anchor's last
//! strong ref drops, the next open-state emission upgrades to `None`, and the
//! loop breaks — releasing the subscription. This is the same weak-ref safety
//! net every sibling widget bind already relies on.

use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

use crate::overlays::sidebar;

/// Call `refresh` on each rising edge of `monitor`'s sidebar-open state
/// (closed → open), for as long as `anchor` is alive.
///
/// The subscription's lifetime is tied to `anchor` via [`bind`], so a widget
/// rebuilt on monitor hot-plug drops its old subscription instead of leaking it
/// (#439). Edge-triggered via a `Cell` so the initial `false` replay from
/// `signal()` doesn't fire a `refresh` against a still-closed sidebar.
pub(crate) fn on_open<W>(monitor: &Monitor, anchor: &W, refresh: impl Fn() + 'static)
where
    W: IsA<gtk::Widget> + Clone + 'static,
{
    let last_open = Rc::new(Cell::new(false));
    bind(sidebar::open_signal(monitor), anchor, move |_, open| {
        let prev = last_open.replace(open);
        if open && !prev {
            refresh();
        }
    });
}
