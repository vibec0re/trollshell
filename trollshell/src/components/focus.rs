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
//! niri's window focus sticks, then re-arms `OnDemand` so popovers keep
//! working. `OnDemand` only grabs on a fresh pointer press, so re-arming it
//! without a click can't re-steal focus.
//!
//! # Re-arm timing: event-driven, not a fixed timeout
//!
//! The hard part is *when* to re-arm. niri's `focus_window` IPC is async
//! and travels a different path than the keyboard-mode change, so a fixed
//! delay races it: re-arm too early and the bar re-grabs focus before the
//! window focus has landed; that race is exactly why #12 "works sometimes"
//! (the original code re-armed after a flat 150ms). Instead we re-arm the
//! moment niri *confirms* the focus change, by watching the live
//! `niri::focused_window` signal (which the niri service drives off the
//! compositor's event stream). A generous timeout stays as a safety net so
//! the bar **always** ends up back in `OnDemand` even if the confirmation
//! never arrives (focus failed, window closed, workspace had no window,
//! …). Whichever fires first re-arms and cancels the other.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use hytte::futures_signals::signal::SignalExt;
use hytte::gtk::{
    self, glib,
    glib::{JoinHandle, SourceId},
    prelude::*,
};
use hytte::services::niri;
use hytte::ui::{KeyboardMode, LayerShell};

/// Safety-net timeout: how long the host bar may sit at
/// `KeyboardMode::None` before `OnDemand` is force-restored, if niri never
/// confirms the focus change via the [`niri::focused_window`] signal. The
/// event-driven path normally re-arms well before this; it only exists so a
/// focus that never lands (failed IPC, closed window, empty workspace)
/// can't leave the bar stuck without a keyboard grab. Deliberately generous
/// — correctness (always re-arming) matters more than shaving milliseconds
/// off the rare miss.
const REARM_TIMEOUT: Duration = Duration::from_millis(600);

/// What a pill click is trying to focus, used to decide when niri has
/// *confirmed* the change so the bar can re-arm its keyboard grab.
#[derive(Clone, Copy)]
pub(crate) enum FocusTarget {
    /// A specific window: confirmed when [`niri::focused_window`] reports a
    /// window with this id.
    Window(u64),
    /// A workspace switch, which has no single target window id: confirmed
    /// when the focused window simply *changes* (or the timeout fires).
    WorkspaceSwitch,
}

/// Make the layer-shell surface hosting `widget` momentarily relinquish its
/// keyboard focus, so a niri focus change triggered by the same click lands
/// on the target window/workspace rather than on the bar, then re-arm
/// `OnDemand` once niri confirms the change (or the safety-net timeout
/// fires). See the module docs. No-op if `widget` isn't rooted in a window
/// yet (shouldn't happen for a mapped bar pill).
pub(crate) fn yield_to_niri_focus(widget: &impl IsA<gtk::Widget>, target: FocusTarget) {
    let Some(window) = widget
        .root()
        .and_then(|root| root.downcast::<gtk::Window>().ok())
    else {
        return;
    };
    window.set_keyboard_mode(KeyboardMode::None);

    // A re-arm fires exactly once. The signal watcher and the timeout both
    // race to call it; the first wins, re-arms `OnDemand`, and tears down
    // the loser (aborts the watcher future / removes the timeout source) so
    // neither the subscription nor the timer leaks.
    let pending: Rc<RefCell<Option<Pending>>> = Rc::new(RefCell::new(None));

    let watcher = {
        let window = window.clone();
        let pending = pending.clone();
        glib::MainContext::default().spawn_local(async move {
            // signal_cloned() replays the current focused window first; that
            // initial value is the pre-click state, not a confirmation. Skip
            // it so a workspace switch waits for a genuine *change*, and a
            // window click only matches once niri reports the new focus.
            let mut first = true;
            niri::focused_window()
                .for_each(|focused| {
                    let confirmed = if first {
                        first = false;
                        // A window click whose target is already the focused
                        // window is already in the desired end state.
                        matches!(target, FocusTarget::Window(id) if focused.as_ref().map(|w| w.id) == Some(id))
                    } else {
                        match target {
                            FocusTarget::Window(id) => {
                                focused.as_ref().map(|w| w.id) == Some(id)
                            }
                            FocusTarget::WorkspaceSwitch => true,
                        }
                    };
                    if confirmed {
                        rearm(&window, &pending);
                    }
                    std::future::ready(())
                })
                .await;
        })
    };

    let timeout = {
        let window = window.clone();
        let pending = pending.clone();
        glib::timeout_add_local_once(REARM_TIMEOUT, move || {
            rearm(&window, &pending);
        })
    };

    *pending.borrow_mut() = Some(Pending { watcher, timeout });
}

/// The two cancellable handles a re-arm tears down: the focused-window
/// signal watcher and the safety-net timeout.
struct Pending {
    watcher: JoinHandle<()>,
    timeout: SourceId,
}

/// Restore `OnDemand` on the host bar and cancel the other (losing) re-arm
/// path. Idempotent: only the first caller finds `pending` populated; later
/// calls (e.g. a stray signal emission after the timeout already fired) see
/// `None` and no-op.
fn rearm(window: &gtk::Window, pending: &Rc<RefCell<Option<Pending>>>) {
    let Some(p) = pending.borrow_mut().take() else {
        return;
    };
    window.set_keyboard_mode(KeyboardMode::OnDemand);
    // Cancel the loser. `abort()` on our own JoinHandle is safe to call
    // from within the watcher future (it just flags the task to stop on its
    // next poll); `SourceId::remove` drops the timer if it hasn't fired.
    p.watcher.abort();
    p.timeout.remove();
}
