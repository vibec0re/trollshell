//! Regression coverage for the `RefCell`-across-a-GTK-call abort class
//! (#674): a `BorrowMutError` unwinding through a glib/GTK callback aborts
//! the *process*, not just one test — #627/#630/#631/#632/#638/#643 fixed
//! roughly 50 such sites in this binary, and none of them had a test until
//! this file, because `trollshell` had no `system-tests` bucket at all to
//! put one in.
//!
//! This drives `modal.rs`'s two `Mutable`-wake sites #673/#674 name by hand
//! — `show_panel_active`'s `open_state.set(true)` and
//! `wire_retract_finish`'s `open_state.set(false)` — with a hand-rolled
//! *synchronous* `Waker` subscribed to modal's own public
//! `drawer_open_signal`, so a subscriber reentering modal's public API runs
//! genuinely *nested* inside the `Mutable::set` call that woke it — the
//! same shape the historical bug had, not a deferred/safe-by-timing
//! approximation of it. `hytte::prelude::bind`'s own apply-loop can't
//! reproduce this: it's driven by `glib::MainContext::spawn_local`, whose
//! `Waker` only *schedules* a poll for the next main-loop iteration, so a
//! `bind`-based subscriber's reaction always runs safely *after* the
//! triggering call returns and would prove nothing.
//!
//! ## `wake()` must not poll the signal that woke it
//!
//! The obvious design — have `wake()` immediately call `poll_change` on its
//! own signal to see what changed, then react — deadlocks, and not just for
//! the tempting-but-wrong reentrant action (an earlier draft of this file
//! tried routing the reentrant action through `modal::close_all`, which
//! calls `set_neq` on every `open_state`; that's a *second* self-deadlock
//! on top of this one). `futures_signals::Mutable::set` holds its internal
//! `RwLock` **write** guard for the entire `notify()` walk — the guard
//! doesn't drop until `set` returns, after every registered waker has
//! already been woken — and `MutableSignalState::poll_change`'s `Ready`
//! branch needs a **read** lock on that same `RwLock` to fetch the current
//! value. A woken callback that re-polls the very `Mutable` that woke it is
//! therefore always trying to acquire a read lock the current thread
//! already holds as a writer: `std::sync::RwLock` has no reentrant-access
//! detection, so that's an unconditional self-deadlock, verified
//! empirically against this exact `futures-signals` version (it hangs
//! `cargo test` outright, not just this one test — caught here with a
//! bounded `timeout` before landing the fix below).
//!
//! The fix: `wake()`/`wake_by_ref()` only note that *something* changed and
//! run the reentrant action directly — they never touch the signal. Only
//! the test's own top-level code (never nested inside a `Mutable::set`
//! call) polls the signal, once to prime the subscription and once more
//! after each trigger to drain the delivered value and re-arm for the next
//! change.
//!
//! ## Why the reentrant action is `modal::install`
//!
//! `modal::install` is the one `PANELS.borrow_mut()` site reachable without
//! re-touching any `Mutable` (it only *clones* an existing
//! `drawer_open_state` entry, never `.set()`/`.set_neq()`s one) — see #631's
//! note on `install` racing a second `install`/`close_all` for the same key
//! for why this is a real, previously-fixed hazard and not a contrived one.
//!
//! ## Why the `Monitor`/`BarHandle` ride in a `thread_local`, not a field
//!
//! `std::task::Wake`'s only safe conversion to a real `Waker` is
//! `impl<W: Wake + Send + Sync + 'static> From<Arc<W>> for Waker` — and
//! this workspace `forbid`s `unsafe_code`, so the unsafe `RawWaker`
//! alternative (the literal mechanism #663/#674 call "the pure-std
//! `RawWaker` harness") is off the table for this crate. Every GTK type
//! (`Monitor`, `BarHandle`, `gtk::Window`, …) is `!Send` — GTK is
//! thread-affine — so none of them can live in a field of a `Wake` impl.
//! They don't need to: this test is single-threaded end to end, so a
//! `thread_local!` (whose contents carry no `Send`/`Sync` bound at all,
//! precisely because they're pinned to one thread already) is a fully safe
//! way to hand `wake()` a `Monitor`/`BarHandle` to reenter `modal::install`
//! with, without touching either the `Send + Sync` bound or `unsafe_code`.
//!
//! Needs a real display server for a `gtk::Revealer` to actually complete a
//! transition (with `gtk-enable-animations` off, synchronously, inside
//! `set_reveal_child` — verified empirically against this exact
//! gtk4/gtk4-layer-shell version pair before writing this file) and for
//! `gtk4-layer-shell`'s `init_layer_shell` to no-op safely rather than
//! error on a non-Wayland `xvfb-run` X11 backend (also verified) — so this
//! is gated into the `system-tests` bucket, run under `xvfb-run`, like the
//! rest of this bug class in `hytte-ui`.
//!
//! ## One `#[gtk::test]`, one `App::run`, two scenarios
//!
//! Both transitions are exercised from a single `App::run` call rather than
//! two. `hytte_reactive::registry::Registry` is thread-local, and
//! `#[gtk::test]` runs *every* test carrying it on one dedicated, shared
//! thread — so a second `App::new(...).with(plugins::service()).run(...)`
//! call in a second `#[gtk::test]` function panics with "duplicate service
//! registration for `trollshell::plugins::PluginHandles`" the moment it tries
//! to register `plugins::service()` again on that same thread (caught here
//! the same way as the earlier hazards in this file: an actual test run,
//! not a read of the docs). And per the "panic in a function that cannot
//! unwind" abort this file's history is full of, that panic — like any
//! other panic inside the `App::run` body closure — aborts the whole
//! process rather than failing just one test, since it unwinds into the
//! `extern "C"` `activate_trampoline` glib calls into. One `App`/one
//! registration sidesteps the whole question.

#![cfg(feature = "system-tests")]

use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

thread_local! {
    /// The `Monitor`/`BarHandle` the reentrant `wake()` callback below
    /// reinstalls the drawer with. See the module doc's "why a
    /// `thread_local`" section — this exists purely to hand a `!Send` GTK
    /// handle to a `Send + Sync`-bound `Wake` impl without `unsafe`.
    static REENTRY_TARGET: RefCell<Option<(Monitor, BarHandle)>> = const { RefCell::new(None) };
}

/// A `Waker` that runs `reentrant_reinstall` directly from `wake`/
/// `wake_by_ref` — no signal polling in here at all; see the module doc's
/// "`wake()` must not poll the signal that woke it" section for why. Just
/// counts how many times it fired, so the test can assert both the open and
/// the retract transition genuinely drove a reentrant call rather than the
/// waker silently never firing.
struct ReentrantOnWake {
    fired: AtomicUsize,
}

impl Wake for ReentrantOnWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.fired.fetch_add(1, Ordering::SeqCst);
        reentrant_reinstall();
    }
}

/// Reinstall the drawer on the `thread_local`-stashed monitor/bar — the
/// reentrant action driven from inside `wake()`. Panicking here (a
/// `BorrowMutError` out of `PANELS.borrow_mut()`, the regression this test
/// exists to catch) unwinds through the GTK `notify::child-revealed` /
/// `Mutable::set` frames between here and the triggering call, which aborts
/// the whole test process rather than just failing this test — the exact
/// failure mode #663 hit for real.
fn reentrant_reinstall() {
    REENTRY_TARGET.with(|cell| {
        let borrowed = cell.borrow();
        let (monitor, bar) = borrowed
            .as_ref()
            .expect("REENTRY_TARGET set before subscribing the waker");
        trollshell::modal::install(monitor, bar, Edge::Top, 0);
    });
}

/// Drain every value the signal has ready and re-arm the waker for the
/// next change. Must only be called from *outside* any `Mutable::set` call
/// on the signal's own backing `Mutable` — see the module doc.
fn drain_and_rearm<S: Signal<Item = bool>>(mut signal: std::pin::Pin<&mut S>, cx: &mut Context) {
    while let Poll::Ready(Some(_)) = signal.as_mut().poll_change(cx) {}
}

/// Run one open→(subscribe)→trigger→drain cycle against `monitor`'s panel,
/// returning how many times the reentrant waker fired.
///
/// Always starts with a fresh `reentrant_reinstall`, so each scenario gets a
/// clean, closed panel regardless of what an earlier scenario left behind —
/// load-bearing for the retract scenario specifically: chaining "open,
/// reentrantly reinstall (which replaces the panel mid-open with a fresh
/// one whose revealer was never opened), then retract" in one flow makes
/// the retract a silent no-op (`set_reveal_child(false)` on an
/// already-`false` revealer doesn't change anything, so
/// `wire_retract_finish` never fires) — discovered empirically while
/// developing this test.
///
/// `open_first` opens the drawer for real (a plain, non-reentrant call,
/// before the waker subscribes) before `trigger` runs — the retract
/// scenario needs this for the same reason: `dismiss_all` on a
/// never-opened drawer is a no-op.
fn run_scenario(
    ctx: &glib::MainContext,
    monitor: &Monitor,
    open_first: bool,
    trigger: impl FnOnce(),
) -> usize {
    reentrant_reinstall();

    if open_first {
        trollshell::modal::open_plugin_on_focused(None, "system-tests-modal-plugin-pre");
        while ctx.iteration(false) {}
    }

    let waker_state = Arc::new(ReentrantOnWake {
        fired: AtomicUsize::new(0),
    });
    let waker: Waker = Waker::from(Arc::clone(&waker_state));
    let mut cx = Context::from_waker(&waker);
    let mut signal = std::pin::pin!(trollshell::modal::drawer_open_signal(monitor));

    // Prime the subscription: consume the signal's current value (not a
    // real transition), then arm for the first genuine change.
    drain_and_rearm(signal.as_mut(), &mut cx);

    trigger();
    drain_and_rearm(signal.as_mut(), &mut cx);

    // Belt-and-braces: let any deferred idle callback (e.g.
    // `wire_recenter_on_map`'s post-map recompute) settle before the next
    // scenario reinstalls over this one.
    while ctx.iteration(false) {}

    waker_state.fired.load(Ordering::SeqCst)
}

#[gtk::test]
fn modal_tolerates_reentrant_install_from_a_synchronous_open_state_wake() {
    let open_fired = Arc::new(AtomicUsize::new(0));
    let close_fired = Arc::new(AtomicUsize::new(0));
    let open_fired_for_closure = Arc::clone(&open_fired);
    let close_fired_for_closure = Arc::clone(&close_fired);

    let result = App::new("mov.vibec0re.trollshell.test.modal-reentrancy")
        .with(trollshell::plugins::service())
        .run(move |app| {
            // With animations off, `gtk::Revealer::set_reveal_child` completes
            // its transition — and fires `notify::child-revealed` — inside the
            // call itself instead of over a real ~180ms frame-clock-driven
            // animation. That's what makes `wire_retract_finish`'s handler (and
            // the `open_state.set(false)` inside it) run synchronously nested
            // in whatever triggered the retract, matching the shape of the
            // historical GTK-emission-driven reentrancy this module's fixes
            // guard against.
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_enable_animations(false);
            }

            let ctx = glib::MainContext::default();
            let monitors = app.monitors();
            let Some(monitor) = monitors.first().cloned() else {
                // No output under this display server. Not expected under
                // `xvfb-run` (verified to report exactly one), but this test
                // is about RefCell reentrancy, not about environment
                // monitor discovery — don't fail on an unrelated quirk.
                app.quit();
                return;
            };

            let bar = Bar::new(&monitor).show();
            REENTRY_TARGET.with(|cell| {
                *cell.borrow_mut() = Some((monitor.clone(), bar));
            });

            // Scenario 1: `show_panel_active`'s `open_state.set(true)`,
            // reached via `open_plugin_on_focused` → `open_plugin_by_key` →
            // `show_panel_active`. The wake fires nested inside that call
            // and reentrantly reinstalls the very panel it's still in the
            // middle of presenting.
            let fired = run_scenario(&ctx, &monitor, false, || {
                trollshell::modal::open_plugin_on_focused(None, "system-tests-modal-plugin-open");
            });
            open_fired_for_closure.store(fired, Ordering::SeqCst);

            // Scenario 2: `wire_retract_finish`'s `open_state.set(false)`,
            // reached via the revealer's synchronous (animations-off)
            // transition completing inside `set_reveal_child` when
            // `dismiss_all` retracts an open drawer.
            let fired = run_scenario(&ctx, &monitor, true, trollshell::modal::dismiss_all);
            close_fired_for_closure.store(fired, Ordering::SeqCst);

            // Deliberately *not* `modal::close_all()` here: it calls
            // `reset_drawer_open_states`, which `set_neq`s every
            // `open_state`. Scenario 2's `signal`/waker have gone out of
            // scope by now (each `run_scenario` call is self-contained), so
            // this wouldn't hit the same reentrant-wake trap that made an
            // earlier draft of this test call `close_all` unsafe — it's
            // simply not needed: the test process exiting tears down every
            // GTK window regardless.
            REENTRY_TARGET.with(|cell| {
                cell.borrow_mut().take();
            });

            app.quit();
        });

    result.expect("App::run");
    let open_fired = open_fired.load(Ordering::SeqCst);
    let close_fired = close_fired.load(Ordering::SeqCst);
    assert!(
        open_fired >= 1,
        "expected the reentrant `install` to fire from inside \
         `show_panel_active`'s `open_state.set(true)`; got {open_fired} \
         wakes — the waker never observed the open transition, so this run \
         didn't actually exercise the regression"
    );
    assert!(
        close_fired >= 1,
        "expected the reentrant `install` to fire from inside \
         `wire_retract_finish`'s `open_state.set(false)`; got {close_fired} \
         wakes — the waker never observed the retract transition, so this \
         run didn't actually exercise the regression"
    );
}
