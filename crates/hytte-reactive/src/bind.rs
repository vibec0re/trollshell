//! Helpers that spawn a per-binding future on `glib::MainContext`, drive
//! a `Signal` to completion, and apply each emitted value to a GTK widget
//! on the main thread.

use futures_signals::signal::{Signal, SignalExt};
use gtk::glib;
use gtk::prelude::*;

/// Spawn a future on the GTK main loop that drives `signal` and applies
/// each emitted value to `widget` via `apply`.
///
/// The widget is cloned (cheap — GTK widgets are reference-counted). The
/// future lives as long as the underlying signal source; widget cleanup
/// drops the closure when the widget is collected and the next emission
/// observes a no-op.
pub fn bind<S, W, F>(signal: S, widget: &W, apply: F)
where
    S: Signal + 'static,
    S::Item: 'static,
    W: IsA<gtk::Widget> + Clone + 'static,
    F: Fn(&W, S::Item) + 'static,
{
    let widget = widget.clone();
    glib::MainContext::default().spawn_local(async move {
        signal
            .for_each(move |value| {
                apply(&widget, value);
                std::future::ready(())
            })
            .await;
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
/// Lifetime is tied to the widget the same way `bind` is: a cheap clone
/// keeps the future alive for as long as the widget is referenced.
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
    let widget = widget.clone();
    let handler_id = connect_user(&widget);
    glib::MainContext::default().spawn_local(async move {
        signal
            .for_each(move |value| {
                widget.block_signal(&handler_id);
                apply(&widget, value);
                widget.unblock_signal(&handler_id);
                std::future::ready(())
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_signals::signal::Mutable;
    use gtk::glib;
    use std::rc::Rc;
    use std::cell::Cell;

    /// A signal emission applies the value, and the user-event handler is
    /// NOT re-fired while the apply runs.
    #[gtk::test]
    fn signal_apply_does_not_refire_user_handler() {
        let ctx = glib::MainContext::default();

        let switch = gtk::Switch::new();
        let state = Mutable::new(false);
        let user_calls = Rc::new(Cell::new(0u32));

        let user_calls_for_handler = user_calls.clone();
        bind_two_way(
            state.signal(),
            &switch,
            gtk::Switch::set_active,
            move |w| {
                let counter = user_calls_for_handler.clone();
                w.connect_active_notify(move |_| counter.set(counter.get() + 1))
            },
        );

        // Pump until the initial Mutable emission applies.
        while ctx.iteration(false) {}

        // Drive a state change. apply() will call set_active(true), which
        // would normally fire active-notify. The handler must stay blocked.
        state.set(true);
        while ctx.iteration(false) {}

        assert_eq!(user_calls.get(), 0,
            "user handler must not fire during signal-driven apply");
        assert!(switch.is_active(), "apply did set the property");
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
        bind_two_way(
            state.signal(),
            &switch,
            gtk::Switch::set_active,
            move |w| {
                let counter = user_calls_for_handler.clone();
                w.connect_active_notify(move |_| counter.set(counter.get() + 1))
            },
        );

        while ctx.iteration(false) {}

        // Simulate a user-driven flip by toggling active directly. Because
        // the signal hasn't emitted, the handler must NOT be blocked.
        switch.set_active(true);
        while ctx.iteration(false) {}

        assert_eq!(user_calls.get(), 1,
            "user-driven set_active must fire the user handler exactly once");
    }
}
