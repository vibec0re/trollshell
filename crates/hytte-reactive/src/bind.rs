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
pub fn bind_visible<S>(signal: S, widget: &impl IsA<gtk::Widget>)
where
    S: Signal<Item = bool> + 'static,
{
    bind(signal, &widget.clone().upcast::<gtk::Widget>(), |w, v| {
        w.set_visible(v);
    });
}

/// Bind a bool signal to whether `class` is present on the widget.
pub fn bind_class<S>(signal: S, widget: &impl IsA<gtk::Widget>, class: &str)
where
    S: Signal<Item = bool> + 'static,
{
    let class = class.to_owned();
    bind(
        signal,
        &widget.clone().upcast::<gtk::Widget>(),
        move |w, v| {
            if v {
                w.add_css_class(&class);
            } else {
                w.remove_css_class(&class);
            }
        },
    );
}
