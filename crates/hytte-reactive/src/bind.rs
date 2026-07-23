//! Helpers that spawn a per-binding future on `glib::MainContext`, drive
//! a `Signal`, and apply each emitted value to a GTK widget on the main
//! thread — holding the widget only weakly so a torn-down widget frees.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use futures_signals::signal::Signal;
use gtk::glib;
use gtk::prelude::*;

/// Spawn a future on the GTK main loop that drives `signal` and applies
/// each emitted value to `widget` via `apply`.
///
/// The apply-loop holds only a [`glib::WeakRef`] to `widget`, so the binding
/// never keeps the widget alive by itself. While the widget is in use its
/// parent chain (window → boxes → chips) holds it strong, so every emission
/// upgrades the weak ref and applies exactly as before. Once the last strong
/// ref is dropped — e.g. a bar or drawer is torn down on monitor hot-plug —
/// the next emission upgrades to `None`, the loop `break`s, and both the task
/// and its underlying signal subscription are released.
///
/// The apply-loop is *also* aborted eagerly from the widget's `destroy` signal,
/// so a widget torn down while its signal is parked releases the task (and its
/// subscription) immediately rather than lingering until the next emission wakes
/// it to break. The weak upgrade stays as the guarantee — it covers the sliver
/// between destroy and abort, and any widget freed without emitting `destroy` —
/// so the #224 free-on-drop behaviour is unchanged; the abort just trims the
/// residual to zero in the common teardown path.
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    S::Item: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    F: Fn(&W, S::Item) + 'static,
{
    let weak = widget.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            apply(&widget, value);
        }
    });
    abort_on_destroy(widget, handle);
}

/// Abort a bind's parked apply-loop the moment `widget` is destroyed, rather
/// than leaving it alive until the next emission wakes it to break on a dead
/// weak ref (see [`bind`]). Because the closure captures only the
/// [`glib::JoinHandle`] — never the widget — it introduces no strong ref that
/// could keep the widget alive.
fn abort_on_destroy<W: IsA<gtk::Widget>>(widget: &W, handle: glib::JoinHandle<()>) {
    widget.connect_destroy(move |_| handle.abort());
}

/// Bind a string-producing signal to a `gtk::Label`'s text.
pub fn bind_text<S>(signal: S, label: &gtk::Label)
where
    S: Signal + 'static,
    S::Item: AsRef<str> + 'static,
{
    bind(signal, label, |w, v| w.set_text(v.as_ref()));
}

/// Bind a bool signal to a widget's `visible` property.
pub fn bind_visible<W, S>(signal: S, widget: &W)
where
    W: IsA<gtk::Widget> + Clone + 'static,
    S: Signal<Item = bool> + 'static,
{
    bind(signal, widget, W::set_visible);
}

/// Bind a bool signal to whether `class` is present on the widget.
pub fn bind_class<W, S>(signal: S, widget: &W, class: &str)
where
    W: IsA<gtk::Widget> + Clone + 'static,
    S: Signal<Item = bool> + 'static,
{
    let class = class.to_owned();
    bind(signal, widget, move |w, v| {
        if v {
            w.add_css_class(&class);
        } else {
            w.remove_css_class(&class);
        }
    });
}

/// Two-way bind: signal drives a writable widget property while the user
/// can still drive that property themselves. The user-event handler is
/// blocked across each signal-driven `apply`, so programmatic state
/// mirroring never re-enters the user handler.
///
/// `connect_user` is invoked once at bind time. It must wire a user-event
/// handler (e.g. `connect_active_notify`, `connect_value_changed`,
/// `connect_toggled`) and return its [`glib::SignalHandlerId`]. The bind
/// future blocks that handler around every `apply` call and unblocks it
/// after.
///
/// Lifetime is tied to the widget the same way [`bind`] is: the apply-loop
/// holds only a [`glib::WeakRef`], so it applies while the widget is alive and
/// frees itself (breaking the loop) once the widget's last strong ref drops.
pub fn bind_two_way<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
) where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
{
    let handler_id = connect_user(widget);
    let weak = widget.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            widget.block_signal(&handler_id);
            apply(&widget, value);
            widget.unblock_signal(&handler_id);
        }
    });
    abort_on_destroy(widget, handle);
}

/// How long after the user releases a drag a signal-driven `apply` stays
/// suppressed. A continuous poller (e.g. the mpris ~250 ms position poll bound
/// to a seek slider) needs a cycle or two to reflect the value the user just
/// committed, so without this settle window the first post-release poll would
/// still report the *pre-drag* value and snap the thumb back under the finger.
/// 600 ms ≈ 2–3 mpris poll cycles.
const DRAG_SETTLE: Duration = Duration::from_millis(600);

/// Pure suppression predicate for [`bind_two_way_drag_safe`]: a signal-driven
/// apply is suppressed while a pointer/touch grab is active (`grabbed`), or
/// while a grab ended less than `settle` ago. Split out with no GTK so the
/// drag-suppression logic is unit-testable without a display server.
fn drag_suppresses(
    grabbed: bool,
    last_release: Option<Instant>,
    now: Instant,
    settle: Duration,
) -> bool {
    grabbed || matches!(last_release, Some(t) if now.saturating_duration_since(t) < settle)
}

/// Drag-safe two-way bind for **continuous** widgets — a `gtk::Scale`/`Range`
/// whose value a poller keeps writing while the user can also drag it.
///
/// Identical to [`bind_two_way`] but adds *grab suppression*: while the user is
/// actively dragging the widget — and for a short [`DRAG_SETTLE`] window after
/// they let go — signal-driven `apply` calls are dropped. Without this, a
/// continuous poller (e.g. the mpris ~250 ms position poll bound to a seek
/// slider) writes the widget's value mid-drag and yanks the thumb back out from
/// under the user's finger; the poller and the user fight over the widget. Once
/// the grab ends and the settle window elapses, the next emission applies as
/// normal and the widget reconciles with the true state.
///
/// Grab state is tracked with a capture-phase [`gtk::EventControllerLegacy`] —
/// raw button/touch press and release events. Unlike a `GestureClick`, a legacy
/// controller does not participate in gesture claiming, so it is *not* cancelled
/// mid-drag when the range's own drag gesture claims the sequence: press sets
/// the grab, release clears it, for the whole grab regardless of who claims. The
/// controller is owned by the widget, so it lives and dies with it, and it only
/// observes (returns [`glib::Propagation::Proceed`]) — the widget's own handlers
/// still run.
///
/// Prefer plain [`bind_two_way`] for **discrete** widgets (switches, toggles,
/// spin rows): they have no drag to protect and no fighting poller, so the extra
/// controller and suppression would only add overhead.
pub fn bind_two_way_drag_safe<S, W, V, Apply, Connect>(
    signal: S,
    widget: &W,
    apply: Apply,
    connect_user: Connect,
) where
    S: Signal<Item = V> + 'static,
    V: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    Apply: Fn(&W, V) + 'static,
    Connect: FnOnce(&W) -> glib::SignalHandlerId,
{
    let handler_id = connect_user(widget);

    // Grab state, written by the capture-phase controller and read by the
    // apply-loop below. `grabbed` is true between press and release; a fresh
    // release stamps `last_release` so the poller stays suppressed through the
    // brief window it takes the daemon to reflect the committed value.
    let grabbed = Rc::new(Cell::new(false));
    let last_release: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));

    let controller = gtk::EventControllerLegacy::new();
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let grabbed = grabbed.clone();
        let last_release = last_release.clone();
        controller.connect_event(move |_, event| {
            match event.event_type() {
                gtk::gdk::EventType::ButtonPress | gtk::gdk::EventType::TouchBegin => {
                    grabbed.set(true);
                }
                gtk::gdk::EventType::ButtonRelease
                | gtk::gdk::EventType::TouchEnd
                | gtk::gdk::EventType::TouchCancel => {
                    grabbed.set(false);
                    last_release.set(Some(Instant::now()));
                }
                _ => {}
            }
            // Observe only — never consume; the widget's own drag handling runs.
            glib::Propagation::Proceed
        });
    }
    widget.add_controller(controller);

    let weak = widget.downgrade();
    let handle = glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            if drag_suppresses(
                grabbed.get(),
                last_release.get(),
                Instant::now(),
                DRAG_SETTLE,
            ) {
                // The user is mid-drag (or just released) — drop this value so we
                // don't fight them. The next emission after the grab settles (the
                // poller keeps ticking) applies and reconciles.
                continue;
            }
            widget.block_signal(&handler_id);
            apply(&widget, value);
            widget.unblock_signal(&handler_id);
        }
    });
    abort_on_destroy(widget, handle);
}

// Pure drag-suppression logic — no GTK, so it runs in the default (hermetic)
// test bucket. The actual pointer-grab tracking is display-dependent (a real
// `gtk::Scale` drag) and is covered by manual live-verify, not here.
#[cfg(test)]
mod drag_suppress_tests {
    use super::{DRAG_SETTLE, drag_suppresses};
    use std::time::{Duration, Instant};

    #[test]
    fn active_grab_always_suppresses() {
        let now = Instant::now();
        // No prior release, but the grab is live → suppress regardless.
        assert!(drag_suppresses(true, None, now, DRAG_SETTLE));
        assert!(drag_suppresses(true, Some(now), now, DRAG_SETTLE));
    }

    #[test]
    fn idle_with_no_release_applies() {
        let now = Instant::now();
        // Not grabbed and never released → the poller value flows through.
        assert!(!drag_suppresses(false, None, now, DRAG_SETTLE));
    }

    #[test]
    fn release_within_settle_suppresses() {
        let released = Instant::now();
        let now = released + Duration::from_millis(300);
        // 300 ms < 600 ms settle → still suppressed so the daemon can catch up.
        assert!(drag_suppresses(false, Some(released), now, DRAG_SETTLE));
    }

    #[test]
    fn release_past_settle_applies_again() {
        let released = Instant::now();
        let now = released + Duration::from_millis(900);
        // 900 ms ≥ 600 ms settle → the poller resumes and the widget reconciles.
        assert!(!drag_suppresses(false, Some(released), now, DRAG_SETTLE));
    }

    #[test]
    fn settle_boundary_is_exclusive() {
        let released = Instant::now();
        // Exactly at the settle window: `< settle` is false → not suppressed.
        assert!(!drag_suppresses(
            false,
            Some(released),
            released + DRAG_SETTLE,
            DRAG_SETTLE
        ));
    }
}

// These exercise live GTK widgets, so they need a display server — gated into
// the `system-tests` bucket rather than run by default.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;
    use futures_signals::signal::Mutable;
    use gtk::glib;
    use std::cell::Cell;
    use std::rc::Rc;

    /// A signal emission applies the value, and the user-event handler is
    /// NOT re-fired while the apply runs.
    #[gtk::test]
    fn signal_apply_does_not_refire_user_handler() {
        let ctx = glib::MainContext::default();

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(state.signal(), &switch, gtk::Switch::set_active, move |w| {
            let counter = user_calls_for_handler.clone();
            w.connect_active_notify(move |_| counter.set(counter.get() + 1))
        });

        // Pump until the initial Mutable emission applies.
        while ctx.iteration(false) {}

        // Drive a state change. apply() will call set_active(true), which
        // would normally fire active-notify. The handler must stay blocked.
        state.set(true);
        while ctx.iteration(false) {}

        assert_eq!(
            user_calls.get(),
            0,
            "user handler must not fire during signal-driven apply"
        );
        assert!(switch.is_active(), "apply did set the property");
    }

    /// Dropping the last strong ref to a bound widget frees it: the next
    /// signal emission wakes the parked apply-loop, its `WeakRef` upgrade
    /// returns `None`, and the loop breaks. On the old strong-clone/`for_each`
    /// code the future pinned the widget forever, so `weak.upgrade()` would
    /// still be `Some` — this is the #224 regression test.
    #[gtk::test]
    fn dropping_bound_widget_frees_it_on_next_emission() {
        let ctx = glib::MainContext::default();

        let label = gtk::Label::new(None);
        let weak = label.downgrade();
        let state = Mutable::new(String::from("a"));

        bind_text(state.signal_cloned(), &label);
        while ctx.iteration(false) {}
        assert_eq!(
            weak.upgrade().map(|l| l.text().to_string()),
            Some(String::from("a")),
            "bound label applied the initial value"
        );

        // Drop the only strong ref we hold. A weakly-held bind future must not
        // keep the widget alive.
        drop(label);

        // Emit again: this wakes the parked apply-loop, which upgrades the
        // (now-dangling) weak ref, gets `None`, and breaks — releasing it.
        state.set(String::from("b"));
        while ctx.iteration(false) {}

        assert!(
            weak.upgrade().is_none(),
            "bound widget must be freed once its last strong ref is dropped"
        );
    }

    /// A genuine user action still fires the user handler — the block is
    /// released between applies.
    #[gtk::test]
    fn user_event_still_fires_after_apply() {
        let ctx = glib::MainContext::default();

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(state.signal(), &switch, gtk::Switch::set_active, move |w| {
            let counter = user_calls_for_handler.clone();
            w.connect_active_notify(move |_| counter.set(counter.get() + 1))
        });

        while ctx.iteration(false) {}

        // Simulate a user-driven flip by toggling active directly. Because
        // the signal hasn't emitted, the handler must NOT be blocked.
        switch.set_active(true);
        while ctx.iteration(false) {}

        assert_eq!(
            user_calls.get(),
            1,
            "user-driven set_active must fire the user handler exactly once"
        );
    }

    /// With no active grab, `bind_two_way_drag_safe` applies signal values to
    /// the widget exactly like `bind_two_way` — the drag guard only kicks in
    /// while the user is dragging. (The suppression path itself needs synthetic
    /// pointer events, so it's live-verified on a real `gtk::Scale` drag.)
    #[gtk::test]
    fn drag_safe_applies_when_not_grabbed() {
        let ctx = glib::MainContext::default();

        let scale = gtk::Scale::new(
            gtk::Orientation::Horizontal,
            Some(&gtk::Adjustment::new(0.0, 0.0, 1.0, 0.01, 0.1, 0.0)),
        );
        let state = Mutable::new(0.0_f64);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way_drag_safe(
            state.signal(),
            &scale,
            gtk::prelude::RangeExt::set_value,
            move |s| {
                let counter = user_calls_for_handler.clone();
                s.connect_value_changed(move |_| counter.set(counter.get() + 1))
            },
        );

        while ctx.iteration(false) {}

        // Idle (no grab): a signal emission applies to the widget, and the user
        // handler stays blocked across the programmatic apply.
        state.set(0.75);
        while ctx.iteration(false) {}

        assert!(
            (gtk::prelude::RangeExt::value(&scale) - 0.75).abs() < f64::EPSILON,
            "idle drag-safe bind must apply the signal value"
        );
        assert_eq!(
            user_calls.get(),
            0,
            "user handler must not fire during signal-driven apply"
        );
    }
}
