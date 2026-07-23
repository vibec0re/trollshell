//! Thin wrapper around `gdk::Monitor` carrying just the metadata bars need.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use gtk::gdk;
use gtk::gdk::prelude::MonitorExt;
use gtk::glib;
use gtk::prelude::ObjectExt;

#[derive(Clone, Debug)]
pub struct Monitor {
    inner: gdk::Monitor,
}

impl Monitor {
    pub(crate) fn new(inner: gdk::Monitor) -> Self {
        Self { inner }
    }

    /// Connector name (e.g. `"DP-1"`, `"eDP-1"`). May be empty on some
    /// drivers; callers should fall back to `model()` or `description()`.
    #[must_use]
    pub fn connector(&self) -> Option<String> {
        self.inner.connector().map(|s| s.to_string())
    }

    /// Free-form description (manufacturer + model).
    #[must_use]
    pub fn description(&self) -> Option<String> {
        self.inner.description().map(|s| s.to_string())
    }

    /// Width and height in logical pixels.
    #[must_use]
    pub fn size(&self) -> (i32, i32) {
        let g = self.inner.geometry();
        (g.width(), g.height())
    }

    /// Signal of this monitor's `(width, height)` in logical pixels. Emits the
    /// current size on subscribe and re-emits on every geometry change.
    ///
    /// A resolution/mode switch (e.g. a kanshi profile change) updates the
    /// *existing* `gdk::Monitor`'s geometry in place — it does **not** emit a
    /// `monitors`-model `items_changed`, so [`App::monitors_changed`] never
    /// fires and a size snapshotted at overlay-install time goes stale (#442).
    /// Long-lived per-monitor sizing should subscribe here (or re-read
    /// [`Monitor::size`] at the point of use) rather than capture once.
    ///
    /// The `notify::geometry` handler is disconnected when the returned signal
    /// is dropped, so a subscription never outlives its consumer on a
    /// persistent `gdk::Monitor` (a monitor that survives a hot-plug rebuild).
    pub fn size_changed(&self) -> impl Signal<Item = (i32, i32)> + 'static {
        let state = Mutable::new(self.size());
        let writer = state.clone();
        let handler = self
            .inner
            .connect_notify_local(Some("geometry"), move |m, _| {
                let g = m.geometry();
                writer.set((g.width(), g.height()));
            });
        // Ride the disconnect guard along in the map closure so it is dropped —
        // and the handler disconnected — exactly when the signal is dropped.
        let guard = GeometryNotifyGuard {
            monitor: self.inner.clone(),
            handler: Some(handler),
        };
        state.signal().map(move |size| {
            let _guard = &guard;
            size
        })
    }

    /// Underlying `gdk::Monitor` for direct GTK calls (e.g. layer-shell).
    #[must_use]
    pub fn gdk(&self) -> &gdk::Monitor {
        &self.inner
    }
}

/// Disconnects a `gdk::Monitor` `notify::geometry` handler when dropped. Owned
/// by the [`Monitor::size_changed`] signal so the handler's lifetime is tied to
/// the subscription — a persistent `gdk::Monitor` doesn't accumulate a live
/// handler per dropped consumer.
struct GeometryNotifyGuard {
    monitor: gdk::Monitor,
    handler: Option<glib::SignalHandlerId>,
}

impl Drop for GeometryNotifyGuard {
    fn drop(&mut self) {
        if let Some(id) = self.handler.take() {
            self.monitor.disconnect(id);
        }
    }
}
