//! Helpers that spawn a per-binding future on `glib::MainContext`, drive
//! a `Signal`, and apply each emitted value to a GTK widget on the main
//! thread — holding the widget only weakly so a torn-down widget frees.

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
/// (A loop whose widget vanished while it was parked lingers until the next
/// emission wakes it to break; meanwhile it pins no widget subtree — only the
/// signal subscription — so the residual is negligible versus the old
/// strong-clone, which pinned the whole detached subtree forever.)
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    S::Item: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    F: Fn(&W, S::Item) + 'static,
{
    let weak = widget.downgrade();
    glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            apply(&widget, value);
        }
    });
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
    glib::MainContext::default().spawn_local(async move {
        let mut signal = std::pin::pin!(signal);
        while let Some(value) = std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)).await {
            let Some(widget) = weak.upgrade() else { break };
            widget.block_signal(&handler_id);
            apply(&widget, value);
            widget.unblock_signal(&handler_id);
        }
    });
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
}
