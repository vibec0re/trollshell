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
//! [`attach_dismiss_catcher`] works around this: while the popover is up
//! it shows a full-screen transparent `Layer::Top` "catcher" window
//! underneath it; a click anywhere on the catcher pops the popover down.
//! This is independent of the compositor's grab routing, so outside-click
//! dismissal works regardless. `set_autohide(true)` stays on — where the
//! grab *does* route (e.g. nested popovers, or other compositors) it
//! still dismisses; both paths funnel through `popdown`, so there is no
//! double-dismiss. See `PopupBuilder::dismiss_catcher`.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk4_layer_shell::{KeyboardMode, Layer};

use crate::Monitor;
use crate::layer_window::{Anchor, layer_window};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Position {
    Top,
    Bottom,
    Left,
    Right,
}

pub struct PopupBuilder {
    anchor: gtk::Widget,
    child: Option<gtk::Widget>,
    position: Position,
    has_arrow: bool,
    css_class: Option<String>,
    catcher_monitor: Option<Monitor>,
}

impl PopupBuilder {
    #[must_use]
    pub fn child(mut self, child: impl IsA<gtk::Widget>) -> Self {
        self.child = Some(child.upcast());
        self
    }

    #[must_use]
    pub fn position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }

    #[must_use]
    pub fn has_arrow(mut self, on: bool) -> Self {
        self.has_arrow = on;
        self
    }

    #[must_use]
    pub fn css_class(mut self, class: impl Into<String>) -> Self {
        self.css_class = Some(class.into());
        self
    }

    /// Give this popover reliable outside-click dismissal via a full-screen
    /// `Layer::Top` catcher on `monitor`, independent of the compositor's
    /// autohide grab routing. See the module docs and
    /// [`attach_dismiss_catcher`]. Recommended for any popover hosted on a
    /// `gtk4-layer-shell` surface (bar chips, overlays) under niri.
    #[must_use]
    pub fn dismiss_catcher(mut self, monitor: &Monitor) -> Self {
        self.catcher_monitor = Some(monitor.clone());
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
        Popup { popover }
    }
}

/// Handle to a built popover. Cheap to clone (refcounted `GObject`).
#[derive(Clone)]
pub struct Popup {
    popover: gtk::Popover,
}

impl Popup {
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
        }
    }

    pub fn show(&self) {
        self.popover.popup();
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

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
/// (the "catcher") is shown on `monitor`. A press anywhere on the catcher
/// pops the popover down. Because `gtk::Popover` renders as an xdg-popup
/// it stacks reliably above its parent layer-shell surface regardless of
/// present order — so the catcher sits below the popover without relying
/// on compositor-specific sibling-surface ordering. The catcher is created
/// on each show and destroyed when the popover closes *or* is unmapped (which
/// covers dispose-while-mapped on hot-plug teardown), so nothing lingers
/// between opens and no orphan click-eater survives a rebuild.
///
/// Note: the modal drawer used a similar two-surface approach before #109,
/// but switched to a single fullscreen surface because niri does not
/// reliably restack two sibling `Layer::Top` surfaces by present order.
/// The popover case is unaffected — xdg-popup vs. layer-shell is a
/// different surface hierarchy.
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

    let catcher: Rc<RefCell<Option<gtk::Window>>> = Rc::new(RefCell::new(None));
    let monitor = monitor.clone();

    // On show, build + present the catcher *before* the popover's surface
    // finishes mapping so the popover stacks above it.
    let catcher_for_show = catcher.clone();
    let popover_for_show = popover.clone();
    popover.connect_show(move |_| {
        // Tear down any stale catcher from a previous show first.
        if let Some(old) = catcher_for_show.borrow_mut().take() {
            old.close();
        }
        let win = build_popover_catcher(&monitor, &popover_for_show);
        win.present();
        *catcher_for_show.borrow_mut() = Some(win);
    });

    // Tear the catcher down on *both* `closed` and `unmap`, funnelling through
    // the same idempotent `take()`:
    //
    // * `closed` is the semantic close (a catcher click, a menu-item action,
    //   autohide, or Escape) — the common path.
    // * `unmap` fires on every hide *and* on dispose-while-mapped. `closed` is
    //   not guaranteed to fire when the popover is destroyed out from under a
    //   live show — e.g. a bar chip's menu is up when `monitors_changed` tears
    //   the whole bar down on hot-plug. Without an `unmap` hook the catcher's
    //   `Rc<RefCell<Option<Window>>>` is merely dropped, but a `gtk::Window`
    //   toplevel lives in GTK's global toplevel list until `close()`d, so it
    //   survives as an invisible full-output click-eater with no visible cause.
    //
    // Whichever fires first drains and closes the catcher; the other finds
    // `None` and is a no-op — so there is no double-close and no orphan.
    let catcher_for_close = catcher.clone();
    popover.connect_closed(move |_| close_catcher(&catcher_for_close));

    let catcher_for_unmap = catcher;
    popover.connect_unmap(move |_| close_catcher(&catcher_for_unmap));
}

/// Idempotently drain and close the catcher window. Called from both the
/// popover's `closed` and `unmap` handlers; the first to fire takes the window
/// out of the shared cell and `close()`s it (removing the toplevel from GTK's
/// window list), leaving the other a harmless no-op.
fn close_catcher(catcher: &Rc<RefCell<Option<gtk::Window>>>) {
    if let Some(win) = catcher.borrow_mut().take() {
        win.close();
    }
}

/// Build a single full-screen transparent catcher window whose only job is
/// to pop `popover` down on any press. `KeyboardMode::None` so it never
/// steals the popover's keyboard focus.
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
    let popover = popover.clone();
    gesture.connect_pressed(move |_, _, _, _| {
        popover.popdown();
    });
    content.add_controller(gesture);

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
// `close_catcher` idempotently drains the shared cell and that `close()`ing the
// window actually removes it from GTK's global toplevel list — the very thing a
// bare drop does *not* do, which is why an un-`close()`d catcher leaks.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;

    #[gtk::test]
    fn close_catcher_drains_and_destroys_idempotently() {
        // A plain toplevel (not a layer window — no compositor needed) stands in
        // for the catcher; GTK still tracks it in its global toplevel list.
        let win = gtk::Window::new();
        win.present();
        let weak = win.downgrade();

        // The cell is now the only *named* strong ref (the local `win` moved in);
        // GTK's toplevel list holds the other.
        let catcher: Rc<RefCell<Option<gtk::Window>>> = Rc::new(RefCell::new(Some(win)));

        // First teardown drains + closes: destroy drops GTK's ref, the taken
        // window drops the cell's ref.
        close_catcher(&catcher);
        assert!(
            catcher.borrow().is_none(),
            "catcher drained on first teardown"
        );

        // A second teardown (the other of closed/unmap) is a harmless no-op.
        close_catcher(&catcher);
        assert!(catcher.borrow().is_none(), "second teardown is idempotent");

        // Let any deferred destroy settle, then prove no orphan toplevel survives.
        while gtk::glib::MainContext::default().iteration(false) {}
        assert!(
            weak.upgrade().is_none(),
            "the catcher window must be destroyed, not left leaking in the toplevel list",
        );
    }
}
