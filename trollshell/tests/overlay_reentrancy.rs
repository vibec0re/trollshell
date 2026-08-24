//! The overlay half of the `RefCell`-across-a-GTK-call abort class (#674).
//!
//! `tests/modal_reentrancy.rs` (#732) covers the two `Mutable`-wake sites
//! inside `modal.rs`. This file covers the one remaining site in the sweep
//! that has the same *provable* shape and lives in a different file:
//! `overlays::sidebar::close_all`.
//!
//! ## Why this site and not the other eight `close_all`s
//!
//! The sweep (#627/#630/#631/#632/#638/#643 → #644/#663/#673) converted
//! roughly fifty borrow-across-a-call sites. A regression test can only
//! *prove* it guards one of them if the code under test itself contains a
//! synchronous callback that re-enters the same cell — otherwise reverting
//! the fix changes nothing observable and the "test" is decoration.
//!
//! `sidebar::close_all` is the one overlay `close_all` that clears that bar:
//! its loop body calls `panel.open_state.set(false)`, and `futures-signals`
//! invokes `Waker::wake()` **synchronously** from `Mutable::set`'s `notify`.
//! So a subscriber woken by that `set` runs *inside* whatever borrow of
//! `PANELS` the loop is holding, and any public sidebar reader it calls
//! (`current_visible_width` / `is_settled`, which is precisely what
//! `overlays::frame`'s per-frame tick callback calls) takes a shared
//! `PANELS.borrow()`. Before #644 the loop was
//! `for (key, panel) in panels.borrow_mut().drain()` — a live `RefMut` for
//! the whole loop — so that shared borrow was a `BorrowMutError`, i.e. a
//! panic unwinding out of a glib/GTK frame, i.e. `SIGABRT`. After #644 it is
//! `for (key, panel) in panels.take()`, which moves the map out and drops
//! the borrow before the loop body ever runs.
//!
//! The other eight are *deliberately not tested here*, with reasons, so the
//! next person doesn't re-derive this:
//!
//! * `frame::close_all`, `osd::close_all`, `notifications::close_all` — the
//!   same `drain()`→`take()` conversion, but their loop bodies are nothing
//!   but `abort()` + `window.destroy()`. Nothing in `frame.rs`/`osd.rs`/
//!   `notifications.rs` connects a handler to a signal `destroy()` emits
//!   that touches `FRAMES`/`OSDS`/`TOAST_WINDOWS`, and a test crate cannot
//!   attach one (the windows are private to those modules). Reverting those
//!   three to `borrow_mut().drain()` leaves every reachable assertion green,
//!   which is exactly the "test you cannot demonstrate failing" that #674
//!   says to skip rather than pad the count with.
//! * `consent::close_all` / `consent::request`'s supersede site and
//!   `prompt::close_prompt` — the Form B (`if let Some(x) = …borrow_mut()
//!   .take()`) conversions. Same problem, and worse: `window.close()`'s
//!   `close-request` emission has no handler in either module, and neither
//!   module's window handlers touch `CONSENT_WINDOW`/`PROMPT_WINDOW` at all
//!   (consent's resolve closure holds its own `gtk::Window` clone; prompt's
//!   `dismiss` runs from a button, not from teardown).
//! * The four `install` insert-drop sites (`modal`, `sidebar`, `frame`,
//!   `osd`, #644/#673). Their own comments already state the mechanism: what
//!   drops inside the borrow is a *refcount decrement*, not a widget
//!   teardown — GTK keeps its own reference to a mapped toplevel, so
//!   dropping the Rust handle emits nothing. There is no reentrant path to
//!   provoke even in principle.
//! * `modal::reset_drawer_open_states` (#631) — already covered, hermetically,
//!   by `reset_drawer_open_states_does_not_reenter_the_borrow` in `modal.rs`'s
//!   own `mod tests`.
//! * `overlays::notifications::apply_emission` (#673's headline) and
//!   `components::reactive_list` (#663, backs eight panels) — both private
//!   (`fn` / `pub(crate) fn`), so an external integration-test crate cannot
//!   call them at all, and neither is reachable through a public entry point
//!   without a live D-Bus notification daemon / a registered service feeding
//!   the signal they render from.
//!
//! ## Shape
//!
//! Same harness as `tests/modal_reentrancy.rs`, and the two constraints it
//! documents apply here verbatim, so only the deltas are restated:
//!
//! * **`wake()` must never touch the `Mutable` that woke it.** `Mutable::set`
//!   holds its internal `RwLock` **write** guard for the whole `notify()`
//!   walk, and `std::sync::RwLock` has no reentrant-access detection, so a
//!   woken callback that calls `get()`/`set()`/`signal()` on that same
//!   `Mutable` self-deadlocks. This is why the reentrant action is
//!   `current_visible_width`/`is_settled` and *not* `sidebar::install` or
//!   `sidebar::toggle`: `install` calls `open_state.signal()` (write lock)
//!   and `toggle` calls `state.get()` (read lock), both on the very
//!   `Mutable` mid-`set`. `current_visible_width`/`is_settled` only reach
//!   `open_state` through `PANELS`, and post-fix `PANELS` has already been
//!   emptied by `take()` when the wake fires — so the `map`/`filter` closure
//!   never runs and the `Mutable` is never touched. Pre-fix they never get
//!   that far either: the `borrow()` panics first. Deadlock-free in both
//!   directions, which is what makes this the usable probe.
//! * **`Waker` via `std::task::Wake`, `!Send` GTK handles via a
//!   `thread_local!`.** The workspace `forbid`s `unsafe_code`, so the raw
//!   `RawWaker` route is unavailable and `Arc<W: Wake + Send + Sync>` is the
//!   only way to a `Waker`; `Monitor` is `!Send`, so it rides in a
//!   `thread_local!` instead of a field. See `modal_reentrancy.rs` for the
//!   long version.
//! * **One `App::run`.** `hytte_reactive::registry::Registry` is
//!   thread-local and `#[gtk::test]` shares one thread across every test in
//!   a binary, so a second `App::new(…).with(…)` in the same binary panics
//!   on duplicate service registration — and that panic, raised inside
//!   `App::run`'s body closure, aborts the process rather than failing one
//!   test. This binary therefore has exactly one `#[gtk::test]`. (It is a
//!   *separate* test binary from `modal_reentrancy.rs`, which is why both
//!   can call `App::run`: cargo runs each integration-test target as its own
//!   process.)
//!
//! Needs a real display server (`xvfb-run`) for the layer-shell surface and
//! the revealer, hence the `system-tests` gate.

#![cfg(feature = "system-tests")]

use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, glib};
use hytte::prelude::*;
use hytte::services::{calendar, clock, tasks};
use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use trollshell::overlays::sidebar;

thread_local! {
    /// The `Monitor` the reentrant `wake()` callback below reads the sidebar
    /// geometry for. `Monitor` is `!Send`, and a `Wake` impl must be
    /// `Send + Sync`; a `thread_local!` carries no such bound because it is
    /// already pinned to one thread. This test is single-threaded end to end
    /// (`#[gtk::test]` + GTK's thread affinity), so that is sound and needs
    /// no `unsafe`.
    static REENTRY_MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
}

/// A `Waker` that re-enters `overlays::sidebar`'s public readers directly
/// from `wake`/`wake_by_ref`, counting how many times it fired so the test
/// can prove the run actually exercised the site instead of silently never
/// waking.
struct ReentrantSidebarReader {
    fired: AtomicUsize,
}

impl Wake for ReentrantSidebarReader {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.fired.fetch_add(1, Ordering::SeqCst);
        reentrant_sidebar_read();
    }
}

/// The reentrant action: read the sidebar's live geometry the way
/// `overlays::frame`'s tick callback does.
///
/// `frame.rs`'s `install_draw` tick calls exactly this pair every frame
/// while the sidebar slides, and `sidebar::close_all` is the `borrow_mut()`
/// counterparty on the `PANELS` cell both of them read — so this is the real
/// pairing the sweep was about, not a contrived one. Both take a shared
/// `PANELS.borrow()`; a `BorrowMutError` here unwinds through
/// `Mutable::set`'s `notify` and out through the glib `activate` trampoline
/// this whole closure runs under, which aborts the process rather than
/// failing one test — the failure mode #663 hit for real.
fn reentrant_sidebar_read() {
    REENTRY_MONITOR.with(|cell| {
        let borrowed = cell.borrow();
        let monitor = borrowed
            .as_ref()
            .expect("REENTRY_MONITOR set before the waker is armed");
        // Results deliberately unused: what is under test is that these calls
        // can *run at all* from inside `close_all`'s loop, not what they
        // return (post-teardown they necessarily report "closed").
        let _ = sidebar::current_visible_width(monitor);
        let _ = sidebar::is_settled(monitor);
    });
}

/// Drain every value the signal has ready and re-arm the waker for the next
/// change. Must only be called from *outside* any `Mutable::set` on the
/// signal's own backing `Mutable` — see the module doc.
fn drain_and_rearm<S: Signal<Item = bool>>(mut signal: std::pin::Pin<&mut S>, cx: &mut Context) {
    while let Poll::Ready(Some(_)) = signal.as_mut().poll_change(cx) {}
}

#[gtk::test]
fn sidebar_close_all_tolerates_a_reentrant_panels_read_from_the_open_state_wake() {
    let fired = Arc::new(AtomicUsize::new(0));
    let fired_for_closure = Arc::clone(&fired);

    let result = App::new("mov.vibec0re.trollshell.test.overlay-reentrancy")
        // `sidebar::install` → `build_card` mounts three plugin regions plus
        // the calendar and tasks cards, and those bind straight to service
        // accessors that `.expect()` their handles out of the registry. These
        // four are the whole set the sidebar's construction path needs
        // (`plugins` for the three `Mount::Sidebar*` slots, `calendar`+`clock`
        // for the calendar card, `tasks` for the task list) — every other
        // service `main.rs` registers is reached only from bar chips or drawer
        // pages this test never builds. They have no live daemon under
        // `xvfb-run`; that is fine, their handles just stay empty, and empty
        // is all this test reads.
        .with(trollshell::plugins::service())
        .with(clock::service())
        .with(calendar::service())
        .with(tasks::service())
        .run(move |app| {
            // With animations off, `gtk::Revealer::set_reveal_child` finishes
            // its transition inside the call instead of over a frame-clock
            // animation, so the sidebar's open subscription settles within the
            // `ctx.iteration` drain below rather than several frames later.
            if let Some(settings) = gtk::Settings::default() {
                settings.set_gtk_enable_animations(false);
            }

            let ctx = glib::MainContext::default();
            let monitors = app.monitors();
            let Some(monitor) = monitors.first().cloned() else {
                // No output under this display server. Not expected under
                // `xvfb-run`, but this test is about `RefCell` reentrancy, not
                // about monitor discovery — don't fail on an unrelated quirk.
                app.quit();
                return;
            };

            REENTRY_MONITOR.with(|cell| {
                *cell.borrow_mut() = Some(monitor.clone());
            });

            // Mount the sidebar for real: `PANELS` is private and `install` is
            // the only thing that populates it, so there is no shortcut to the
            // state `close_all` iterates.
            sidebar::install(&monitor);

            // Open it before tearing it down. Not strictly required to make
            // `close_all`'s `open_state.set(false)` notify (`Mutable::set`
            // notifies unconditionally, unlike `set_neq`), but it is the real
            // hot-plug-with-the-sidebar-open shape: it arms the revealer, the
            // exclusive zone, and the settle tick that `close_all` then has to
            // unwind — so the loop body under test runs with every branch live.
            sidebar::toggle(&monitor);
            while ctx.iteration(false) {}

            let waker_state = Arc::new(ReentrantSidebarReader {
                fired: AtomicUsize::new(0),
            });
            let waker: Waker = Waker::from(Arc::clone(&waker_state));
            let mut cx = Context::from_waker(&waker);
            let mut signal = std::pin::pin!(sidebar::open_signal(&monitor));

            // Prime the subscription: a fresh signal's first poll always
            // delivers the current value, so drain to `Pending` to actually
            // register the waker for the *next* notify — the one `close_all`
            // fires.
            drain_and_rearm(signal.as_mut(), &mut cx);

            // The site under test. Pre-#644 this held `PANELS.borrow_mut()`
            // for the whole loop, so the `PANELS.borrow()` the wake below
            // performs was a `BorrowMutError`.
            sidebar::close_all();

            drain_and_rearm(signal.as_mut(), &mut cx);

            // Let any deferred idle work (the aborted subscriptions' final
            // poll, the cancelled settle tick) settle before the process
            // tears the display connection down.
            while ctx.iteration(false) {}

            fired_for_closure.store(waker_state.fired.load(Ordering::SeqCst), Ordering::SeqCst);

            REENTRY_MONITOR.with(|cell| {
                cell.borrow_mut().take();
            });

            app.quit();
        });

    result.expect("App::run");
    let fired = fired.load(Ordering::SeqCst);
    assert!(
        fired >= 1,
        "expected the reentrant `PANELS` read to fire from inside \
         `sidebar::close_all`'s `open_state.set(false)`; got {fired} wakes — \
         the waker never observed the close transition, so this run did not \
         actually exercise the regression"
    );
}
