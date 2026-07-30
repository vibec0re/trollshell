//! `Popup` — an anchored popover hosted on a trigger widget.
//!
//! Wraps `gtk::Popover`. The popover is `set_parent(&trigger)`'d so it
//! positions automatically relative to the trigger's allocation. Click
//! outside dismisses (default Popover behaviour).
//!
//! For popups spawned from a `Bar`, the bar must be built with
//! `Bar::keyboard_interactivity(KeyboardMode::OnDemand)` so the layer
//! surface can grant keyboard focus to the popover.
//!
//! ## Outside-click dismissal on `gtk4-layer-shell`
//!
//! A `gtk::Popover` dismisses on outside-click via a pointer/keyboard
//! *grab* (`set_autohide(true)`). On `gtk4-layer-shell` under some
//! compositors (notably niri) that grab isn't routed to the popover's
//! surface, so clicking outside the popover does nothing — the popover
//! sticks until Escape or a second trigger-click.
//!
//! [`attach_dismiss_catcher`] works around this: while the popover is up it
//! shows a full-screen transparent `Layer::Top` "catcher" window on **every**
//! connected output (below the popover on its home output); a click *or scroll*
//! anywhere on a catcher pops the popover down. Covering every output means a
//! click on a different monitor dismisses too, and handling scroll means a
//! wheel event over the covered output isn't silently swallowed. This is
//! independent of the compositor's grab routing, so outside-click dismissal
//! works regardless. `set_autohide(true)` stays on — where the grab *does*
//! route (e.g. nested popovers, or other compositors) it still dismisses; both
//! paths funnel through `popdown`, so there is no double-dismiss. See
//! `PopupBuilder::dismiss_catcher`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk4_layer_shell::{KeyboardMode, Layer};

use crate::Monitor;
use crate::layer_window::{Anchor, layer_window};

/// Which side of its anchor widget a [`Popup`] points from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Position {
    /// Above the anchor.
    Top,
    /// Below the anchor.
    Bottom,
    /// Left of the anchor.
    Left,
    /// Right of the anchor.
    Right,
}

/// Builder for a [`Popup`]. Start from [`Popup::new`], set the child /
/// position / dismissal behaviour, then [`PopupBuilder::build`].
pub struct PopupBuilder {
    anchor: gtk::Widget,
    child: Option<gtk::Widget>,
    position: Position,
    has_arrow: bool,
    css_class: Option<String>,
    catcher_monitor: Option<Monitor>,
    unparent_on_close: bool,
}

impl PopupBuilder {
    /// Set the popover's content widget.
    #[must_use]
    pub fn child(mut self, child: impl IsA<gtk::Widget>) -> Self {
        self.child = Some(child.upcast());
        self
    }

    /// Choose which side of the anchor the popover points from
    /// (default [`Position::Bottom`]).
    #[must_use]
    pub fn position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }

    /// Toggle the popover's pointing arrow (off by default).
    #[must_use]
    pub fn has_arrow(mut self, on: bool) -> Self {
        self.has_arrow = on;
        self
    }

    /// Add an extra CSS class alongside the built-in `hytte-popup`.
    #[must_use]
    pub fn css_class(mut self, class: impl Into<String>) -> Self {
        self.css_class = Some(class.into());
        self
    }

    /// Give this popover reliable outside-click dismissal via a full-screen
    /// `Layer::Top` catcher, independent of the compositor's autohide grab
    /// routing. `monitor` is the output the popover is hosted on; the catcher
    /// additionally covers every *other* connected output so a click on a
    /// different monitor dismisses too. See the module docs and
    /// [`attach_dismiss_catcher`]. Recommended for any popover hosted on a
    /// `gtk4-layer-shell` surface (bar chips, overlays) under niri.
    #[must_use]
    pub fn dismiss_catcher(mut self, monitor: &Monitor) -> Self {
        self.catcher_monitor = Some(monitor.clone());
        self
    }

    /// Unparent the popover from its anchor once it closes.
    ///
    /// Suits the *transient* pattern — build a fresh popover each time it is
    /// opened (e.g. a per-row edit menu), so nothing accumulates on the anchor
    /// across opens and state hygiene stays trivial. Do **not** combine with
    /// reusing one `Popup` via [`Popup::show`]/[`Popup::toggle`]: once
    /// unparented the popover can't re-present, so a reused handle needs this
    /// left `false` (the default).
    #[must_use]
    pub fn unparent_on_close(mut self, on: bool) -> Self {
        self.unparent_on_close = on;
        self
    }

    /// Build the popover. The popover is parented to the anchor widget;
    /// dropping the returned handle does *not* close it (the parent owns
    /// it via GTK's reference counting).
    #[must_use]
    pub fn build(self) -> Popup {
        let popover = gtk::Popover::new();
        popover.set_parent(&self.anchor);
        popover.set_position(map_position(self.position));
        popover.set_has_arrow(self.has_arrow);
        popover.set_autohide(true);
        popover.add_css_class("hytte-popup");
        if let Some(class) = self.css_class {
            popover.add_css_class(&class);
        }
        if let Some(child) = self.child {
            popover.set_child(Some(&child));
        }
        if let Some(monitor) = self.catcher_monitor {
            attach_dismiss_catcher(&popover, &monitor);
        }
        if self.unparent_on_close {
            popover.connect_closed(gtk::prelude::WidgetExt::unparent);
        }
        Popup { popover }
    }
}

/// Handle to a built popover. Cheap to clone (refcounted `GObject`).
#[derive(Clone)]
pub struct Popup {
    popover: gtk::Popover,
}

impl Popup {
    /// Start building a popover parented to `anchor` (it positions itself
    /// relative to the anchor's allocation).
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(anchor: &impl IsA<gtk::Widget>) -> PopupBuilder {
        PopupBuilder {
            anchor: anchor.clone().upcast(),
            child: None,
            position: Position::Bottom,
            has_arrow: false,
            css_class: None,
            catcher_monitor: None,
            unparent_on_close: false,
        }
    }

    /// Present the popover.
    pub fn show(&self) {
        self.popover.popup();
    }

    /// Dismiss the popover.
    pub fn hide(&self) {
        self.popover.popdown();
    }

    /// Present the popover if hidden, dismiss it if shown.
    pub fn toggle(&self) {
        if self.popover.is_visible() {
            self.popover.popdown();
        } else {
            self.popover.popup();
        }
    }

    /// Underlying `gtk::Popover` for advanced use.
    #[must_use]
    pub fn popover(&self) -> &gtk::Popover {
        &self.popover
    }
}

/// Give an arbitrary `gtk::Popover` reliable outside-click dismissal on
/// `gtk4-layer-shell`, where the popover's own autohide grab is often not
/// routed by the compositor (see the module docs).
///
/// While the popover is up, a full-screen transparent `Layer::Top` window
/// (the "catcher") is shown on **every** connected output — not just the one
/// the popover is on — so a click on a *different* monitor dismisses too.
/// `monitor` is the popover's home output, used only as a fallback if the
/// display can't be enumerated. A press *or scroll* anywhere on a catcher pops
/// the popover down: without the scroll handler a wheel event over the covered
/// output would be silently swallowed with no effect, so scrolling is treated
/// as an outside interaction that dismisses. Because `gtk::Popover` renders as
/// an xdg-popup it stacks reliably above its parent layer-shell surface
/// regardless of present order — so the home-output catcher sits below the
/// popover without relying on compositor-specific sibling-surface ordering. The
/// catchers are created on each show and destroyed when the popover closes *or*
/// is unmapped (which covers dispose-while-mapped on hot-plug teardown), so
/// nothing lingers between opens and no orphan click-eater survives a rebuild.
///
/// Note: the modal drawer used a similar two-surface approach before #109,
/// but switched to a single fullscreen surface because niri does not
/// reliably restack two sibling `Layer::Top` surfaces by present order.
/// The popover case is unaffected — xdg-popup vs. layer-shell is a
/// different surface hierarchy, and the per-monitor catchers are each on a
/// *distinct* output, so they never contend for the same restack.
///
/// `set_autohide` is intentionally left untouched: if the compositor *does*
/// route the grab, that path still dismisses, and because it funnels through
/// the same `popdown()` there is no double-dismiss race.
///
/// Idempotent per popover-show, but call once per popover (e.g. at build
/// time). Works for both `set_parent`/`popup()` popovers and ones driven by
/// `gtk::MenuButton::set_popover`.
pub fn attach_dismiss_catcher(popover: &gtk::Popover, monitor: &Monitor) {
    // Keep autohide on — see the doc comment. The catcher is the reliable
    // path; autohide is the belt-and-suspenders one where the grab routes.
    popover.set_autohide(true);

    let catchers: Rc<RefCell<Vec<gtk::Window>>> = Rc::new(RefCell::new(Vec::new()));
    let monitor = monitor.clone();

    // On show, build + present the catchers *before* the popover's surface
    // finishes mapping so the popover stacks above the home-output one.
    let catchers_for_show = catchers.clone();
    let popover_for_show = popover.clone();
    popover.connect_show(move |_| {
        // Tear down any stale catchers from a previous show first.
        close_catchers(&catchers_for_show);
        let wins: Vec<gtk::Window> = all_monitors(&monitor)
            .iter()
            .map(|m| {
                let win = build_popover_catcher(m, &popover_for_show);
                win.present();
                win
            })
            .collect();
        *catchers_for_show.borrow_mut() = wins;
    });

    // Tear the catchers down on *both* `closed` and `unmap`, funnelling through
    // the same idempotent drain:
    //
    // * `closed` is the semantic close (a catcher click/scroll, a menu-item
    //   action, autohide, or Escape) — the common path.
    // * `unmap` fires on every hide *and* on dispose-while-mapped. `closed` is
    //   not guaranteed to fire when the popover is destroyed out from under a
    //   live show — e.g. a bar chip's menu is up when `monitors_changed` tears
    //   the whole bar down on hot-plug. Without an `unmap` hook the catchers'
    //   `Vec` is merely dropped, but a `gtk::Window` toplevel lives in GTK's
    //   global toplevel list until `destroy()`d, so each survives as an invisible
    //   full-output click-eater with no visible cause.
    //
    // Whichever fires first drains and closes the catchers; the other finds an
    // empty `Vec` and is a no-op — so there is no double-close and no orphan.
    let catchers_for_close = catchers.clone();
    popover.connect_closed(move |_| close_catchers(&catchers_for_close));

    let catchers_for_unmap = catchers;
    popover.connect_unmap(move |_| close_catchers(&catchers_for_unmap));
}

/// Idempotently drain and close every catcher window. Called from both the
/// popover's `closed` and `unmap` handlers; the first to fire takes the windows
/// out of the shared cell and `destroy()`s them (removing each toplevel from
/// GTK's window list), leaving the other a harmless no-op.
fn close_catchers(catchers: &Rc<RefCell<Vec<gtk::Window>>>) {
    // `take()`, not `borrow_mut().drain(..)`: a chained `RefMut` temporary
    // lives for the whole `for`, so each `destroy()` would run with the cell
    // still mutably borrowed. `GtkWidget::destroy` is emitted synchronously
    // from dispose, and this very cell is what a re-entrant teardown reaches
    // for — a `BorrowMutError` there unwinds through a glib callback and
    // aborts the process. Taking first also keeps the idempotence the two
    // callers rely on: the second teardown finds an empty vec.
    for win in catchers.take() {
        win.destroy();
    }
}

/// Every currently-connected monitor, so the catcher covers all outputs.
/// Falls back to just `anchor` if the display can't be enumerated (never
/// expected in practice, but keeps at least the popover's own output covered).
fn all_monitors(anchor: &Monitor) -> Vec<Monitor> {
    let Some(display) = gdk::Display::default() else {
        return vec![anchor.clone()];
    };
    let model = display.monitors();
    let mut out = Vec::new();
    for i in 0..model.n_items() {
        if let Some(obj) = model.item(i)
            && let Ok(m) = obj.downcast::<gdk::Monitor>()
        {
            out.push(Monitor::new(m));
        }
    }
    if out.is_empty() {
        vec![anchor.clone()]
    } else {
        out
    }
}

/// Build a single full-screen transparent catcher window whose only job is to
/// pop `popover` down on any press *or scroll*. `KeyboardMode::None` so it
/// never steals the popover's keyboard focus.
fn build_popover_catcher(monitor: &Monitor, popover: &gtk::Popover) -> gtk::Window {
    let win = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .namespace("hytte-popup-catcher")
        .build();
    win.add_css_class("hytte-popup-catcher");

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_hexpand(true);
    content.set_vexpand(true);
    win.set_child(Some(&content));

    let gesture = gtk::GestureClick::new();
    // Button 0 → any button dismisses.
    gesture.set_button(0);
    let popover_for_click = popover.clone();
    gesture.connect_pressed(move |_, _, _, _| {
        popover_for_click.popdown();
    });
    content.add_controller(gesture);

    // A full-output layer surface intercepts *all* pointer input on its output,
    // including the scroll wheel. Without this a scroll over the covered output
    // would be silently eaten; treat it as an outside interaction and dismiss,
    // matching the click behaviour.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    let popover_for_scroll = popover.clone();
    scroll.connect_scroll(move |_, _, _| {
        popover_for_scroll.popdown();
        glib::Propagation::Proceed
    });
    content.add_controller(scroll);

    win
}

fn map_position(p: Position) -> gtk::PositionType {
    match p {
        Position::Top => gtk::PositionType::Top,
        Position::Bottom => gtk::PositionType::Bottom,
        Position::Left => gtk::PositionType::Left,
        Position::Right => gtk::PositionType::Right,
    }
}

// The full catcher lifecycle (popover show → layer-shell catcher built →
// popover disposed → catcher gone) is compositor-dependent: `build_popover_catcher`
// needs a live `gtk4-layer-shell`, so it can only be live-verified under niri.
//
// What *is* exercisable headlessly (needs a display → gated to `system-tests`,
// run under `xvfb-run`) is the teardown mechanism the fix hinges on: that
// `close_catchers` idempotently drains the shared cell and that `destroy()`ing
// each window actually removes it from GTK's global toplevel list — the very
// thing a bare drop does *not* do, which is why an un-`destroy()`d catcher leaks.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;

    #[gtk::test]
    fn close_catchers_drains_and_destroys_idempotently() {
        // Two plain toplevels (not layer windows — no compositor needed) stand
        // in for the per-monitor catchers; GTK tracks each in its global
        // toplevel list.
        let win_a = gtk::Window::new();
        let win_b = gtk::Window::new();
        win_a.present();
        win_b.present();
        let weak_a = win_a.downgrade();
        let weak_b = win_b.downgrade();

        // The cell is now the only *named* strong ref to each (the locals moved
        // in); GTK's toplevel list holds the other.
        let catchers: Rc<RefCell<Vec<gtk::Window>>> = Rc::new(RefCell::new(vec![win_a, win_b]));

        // First teardown drains + closes every window: destroy drops GTK's ref,
        // the drained windows drop the cell's ref.
        close_catchers(&catchers);
        assert!(
            catchers.borrow().is_empty(),
            "catchers drained on first teardown"
        );

        // A second teardown (the other of closed/unmap) is a harmless no-op.
        close_catchers(&catchers);
        assert!(
            catchers.borrow().is_empty(),
            "second teardown is idempotent"
        );

        // Let any deferred destroy settle, then prove no orphan toplevel survives.
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(
            weak_a.upgrade().is_none() && weak_b.upgrade().is_none(),
            "every catcher window must be destroyed, not left leaking in the toplevel list",
        );
    }

    /// The borrow half of the same teardown (#643): `close_catchers` must not
    /// hold `catchers` borrowed across `destroy()`.
    ///
    /// `GtkWidget::destroy` is emitted **synchronously** from dispose, so a
    /// handler on a catcher runs inside the loop. This test makes that handler
    /// re-enter `close_catchers` on the same cell — the exact shape a real
    /// re-entrant teardown would take. Against the pre-fix
    /// `for win in catchers.borrow_mut().drain(..)`, the inner call hits an
    /// already-live `RefMut` and panics with `BorrowMutError` from inside a
    /// glib callback, which aborts the test binary rather than failing one
    /// test. With `take()` the borrow is over before the first `destroy()`, so
    /// the inner call simply finds an empty vec.
    #[gtk::test]
    fn close_catchers_tolerates_a_reentrant_teardown_from_destroy() {
        let catchers: Rc<RefCell<Vec<gtk::Window>>> = Rc::new(RefCell::new(Vec::new()));

        let win = gtk::Window::new();
        win.present();
        let catchers_for_destroy = catchers.clone();
        win.connect_destroy(move |_| close_catchers(&catchers_for_destroy));
        catchers.borrow_mut().push(win);

        close_catchers(&catchers);
        assert!(
            catchers.borrow().is_empty(),
            "a re-entrant teardown must leave the cell drained, not deadlocked or panicking"
        );
    }
}
