//! Let a bar pill hand keyboard focus to the niri window/workspace it
//! focuses, instead of to the bar itself.
//!
//! The bar is a `gtk4-layer-shell` surface built `KeyboardMode::OnDemand`
//! (`main.rs`) so popovers spawned from bar chips can receive keyboard
//! input. Under niri that carries a side effect: clicking *anywhere* on an
//! on-demand surface hands keyboard focus to that surface. So the same
//! click that asks niri to `focus_window(id)` also focuses the *bar*, and
//! the bar wins — focus lands on the bar instead of the clicked window, so
//! window-switching from the bar silently doesn't work (issue #12). The
//! tell is that focus snaps to the bar specifically; a failed IPC call
//! would have left focus on the previous window.
//!
//! [`yield_to_niri_focus`] breaks the tie: right after the focus request it
//! drops the host surface to `KeyboardMode::None`, releasing the grab so
//! niri's window focus sticks, then re-arms `OnDemand` a beat later so
//! popovers keep working. `OnDemand` only grabs on a fresh pointer press,
//! so re-arming it without a click can't re-steal focus. Releasing and
//! focusing converge on the same end state regardless of which the
//! compositor processes first: both "release the bar's grab" and "focus
//! the window" must happen, and once neither competes the keyboard follows
//! the focused window.

use std::time::Duration;

use hytte::gtk::{self, glib, prelude::*};
use hytte::ui::{KeyboardMode, LayerShell};

/// How long the host bar sits at `KeyboardMode::None` before `OnDemand` is
/// restored. Long enough for the `None` request and the `focus_window` /
/// `focus_workspace` IPC to both settle in the compositor; short enough
/// that a popover opened immediately after a pill click still gets its
/// keyboard.
const REARM_AFTER: Duration = Duration::from_millis(150);

/// Make the layer-shell surface hosting `widget` momentarily relinquish its
/// keyboard focus, so a niri focus change triggered by the same click lands
/// on the target window/workspace rather than on the bar. See the module
/// docs. No-op if `widget` isn't rooted in a window yet (shouldn't happen
/// for a mapped bar pill).
pub(crate) fn yield_to_niri_focus(widget: &impl IsA<gtk::Widget>) {
    let Some(window) = widget
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
    else {
        return;
    };
    window.set_keyboard_mode(KeyboardMode::None);
    glib::timeout_add_local_once(REARM_AFTER, move || {
        window.set_keyboard_mode(KeyboardMode::OnDemand);
    });
}
