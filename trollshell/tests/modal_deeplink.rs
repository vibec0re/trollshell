//! Regression coverage for #799: a deep-link that fires with **no drawer open
//! on any monitor** must open its target rather than silently doing nothing.
//!
//! `modal::switch_active` is the deep-link entry point — "put `target` in front
//! of the user" for a caller that has no `&Monitor` in hand. Its v1 shape only
//! *swapped the page of drawers that were already open*, so reaching it with
//! every drawer closed was a documented no-op: nothing opened, nothing moved, no
//! feedback. That failure mode is indistinguishable from a broken keybind, which
//! is why #799 wanted it closed before the command surface (#219) grows more
//! monitor-less callers than `components::deep_link_row` (a row that, by
//! construction, only exists inside an already-open drawer).
//!
//! ## Why this is a `system-tests` integration test and not a unit test
//!
//! The branch is only reachable with a real drawer *mounted and closed*, and
//! `modal`'s panel set is a private thread-local of `gtk::Window`s built by
//! `modal::install`. On a bare test thread `PANELS` is empty, which is the
//! *"no drawer mounted anywhere"* case — where the fixed code and the old code
//! behave identically (both no-op), so a hermetic unit test could not be
//! falsified by reverting the fix. Per `tests/overlay_reentrancy.rs`'s rule
//! about tests that cannot be demonstrated failing, this is therefore driven
//! against a real `App` + `Bar` + drawer under a display server instead.
//!
//! Falsified the intended way: reverting `switch_active` to its pre-#799 body
//! leaves the drawer closed and fails the `drawer_open` assertion below.
//!
//! ## Shape
//!
//! One `#[gtk::test]`, one `App::run`, for the reason both sibling files
//! document at length: `hytte_reactive::registry::Registry` is thread-local and
//! `#[gtk::test]` shares one thread across every test in a binary, so a second
//! `App::new(…).with(…)` here would panic on duplicate service registration —
//! and a panic inside `App::run`'s body closure unwinds into glib's
//! `extern "C"` activate trampoline, aborting the process instead of failing one
//! test. For the same reason nothing in the closure asserts: the outcome is
//! recorded into atomics and every `assert!` runs after `App::run` returns.
//!
//! Needs a display server (`xvfb-run`) for the layer-shell surface and the
//! revealer, hence the `system-tests` gate.

#![cfg(feature = "system-tests")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use hytte::services::{calendar, clock};
use trollshell::modal::{self, Page};

/// The drawer-open state `monitor`'s panel currently reports, read through
/// `modal`'s own public signal.
///
/// A `Mutable`'s signal always delivers its current value on the first poll
/// regardless of whether anything changed, so one poll of a fresh subscription
/// is a plain read. `Waker::noop()` is enough because this never polls a second
/// time — nothing here needs to be woken for a *later* change, unlike the
/// sibling reentrancy tests whose whole subject is the wake.
fn drawer_open(monitor: &Monitor) -> bool {
    let mut cx = Context::from_waker(Waker::noop());
    let mut signal = std::pin::pin!(modal::drawer_open_signal(monitor));
    match signal.as_mut().poll_change(&mut cx) {
        Poll::Ready(Some(open)) => open,
        Poll::Ready(None) => panic!("drawer_open_signal ended before delivering a value"),
        Poll::Pending => {
            panic!("a fresh Mutable signal must deliver its current value on the first poll")
        }
    }
}

#[gtk::test]
fn switch_active_opens_the_target_when_no_drawer_is_open_anywhere() {
    let reached = Arc::new(AtomicBool::new(false));
    let closed_before = Arc::new(AtomicBool::new(false));
    let open_after = Arc::new(AtomicBool::new(false));

    let reached_in = Arc::clone(&reached);
    let closed_before_in = Arc::clone(&closed_before);
    let open_after_in = Arc::clone(&open_after);

    let result = App::new("mov.vibec0re.trollshell.test.modal-deeplink")
        // The whole service set this test's one page needs. `plugins` because
        // `build_pages_stack` eagerly adds the per-monitor plugin drawer child
        // (`plugins::plugin_panel_slot`, #349 PR2) for *every* drawer, so
        // `modal::install` needs it regardless of which page is shown;
        // `calendar` + `clock` because `Page::Calendar` builds
        // `widgets::calendar::widget_for_drawer` and its on-show hook calls
        // `calendar::refresh()`. `Page::Calendar` is chosen precisely because
        // that is the entire list — every other page reaches further into the
        // service registry for no benefit here. As in `overlay_reentrancy.rs`,
        // none of these have a live daemon under `xvfb-run`; their handles just
        // stay empty, and empty is all this test reads.
        .with(trollshell::plugins::service())
        .with(clock::service())
        .with(calendar::service())
        .run(move |app| {
            // With animations off, `gtk::Revealer::set_reveal_child` completes
            // its transition inside the call rather than over a frame-clock
            // animation, so the open settles within the drain below.
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_enable_animations(false);
            }

            let ctx = glib::MainContext::default();
            let monitors = app.monitors();
            let Some(monitor) = monitors.first().cloned() else {
                // No output under this display server — not expected under
                // `xvfb-run` (verified to report exactly one). Leave `reached`
                // false and let the assertion outside the closure report it,
                // rather than panicking in here where it would abort.
                app.quit();
                return;
            };

            let bar = Bar::new(&monitor).show();
            modal::install(&monitor, &bar, Edge::Top, 0);
            while ctx.iteration(false) {}

            // Precondition, recorded rather than assumed: `install` mounts the
            // drawer *closed*. If this ever stopped holding, the deep-link below
            // would take the ordinary in-place-swap path and the test would pass
            // without exercising #799's branch at all.
            closed_before_in.store(!drawer_open(&monitor), Ordering::SeqCst);

            // The deep-link, fired with every drawer closed. Note that
            // `components::focused_output::install()` is deliberately *not*
            // called: its cache reads `None`, which is the "niri's focused
            // output is unknown" case, and `open_on_focused`'s any-mounted-
            // drawer fallback is what has to carry the link the rest of the way.
            modal::switch_active(Page::Calendar);
            while ctx.iteration(false) {}

            open_after_in.store(drawer_open(&monitor), Ordering::SeqCst);
            reached_in.store(true, Ordering::SeqCst);

            app.quit();
        });

    result.expect("App::run");
    assert!(
        reached.load(Ordering::SeqCst),
        "the test body never ran — no monitor was reported by the display \
         server, so nothing about #799 was exercised"
    );
    assert!(
        closed_before.load(Ordering::SeqCst),
        "`modal::install` left the drawer open, so the deep-link below took the \
         ordinary in-place-swap path instead of #799's no-drawer-open branch"
    );
    assert!(
        open_after.load(Ordering::SeqCst),
        "`switch_active` with no drawer open anywhere left every drawer closed — \
         the deep-link was a silent no-op (#799)"
    );
}
