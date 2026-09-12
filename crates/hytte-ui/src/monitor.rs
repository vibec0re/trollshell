//! Thin wrapper around `gdk::Monitor` carrying just the metadata bars need.

use futures_signals::signal::{Mutable, Signal, SignalExt};
use gtk::gdk;
use gtk::gdk::prelude::MonitorExt;
use gtk::glib;
use gtk::prelude::ObjectExt;

/// A connected output. Thin, cheaply-cloned wrapper around `gdk::Monitor`
/// exposing just the metadata bars and overlays need (connector, geometry,
/// and a geometry-change signal).
#[derive(Clone, Debug)]
pub struct Monitor {
    inner: gdk::Monitor,
}

impl Monitor {
    pub(crate) fn new(inner: gdk::Monitor) -> Self {
        Self { inner }
    }

    /// Connector name (e.g. `"DP-1"`, `"eDP-1"`), or `None` for an output
    /// this driver does not name — **including one it names with the empty
    /// string**.
    ///
    /// That fold is the whole of #1180 item 6. GDK's own `connector()` can
    /// answer `Some("")`, this doc used to say so ("may be empty on some
    /// drivers; callers should fall back") and leave the handling to six call
    /// sites, which grew three different policies: `fullscreen::install`,
    /// `overlays::consent::install` and `plugins::region::named_connector`
    /// each filtered the empty string out, while `components::monitor_key`
    /// and the toast/OSD/frame overlay maps did not — so on a driver that
    /// answers `Some("")` every unnamed output collided on one key `""`
    /// instead of taking its own fallback, and two outputs shared one
    /// drawer-open flag, one toast surface and one OSD.
    ///
    /// Folding it here rather than at the call sites is what makes that one
    /// policy instead of six: an empty name is not a name, and no caller can
    /// now be written that forgets to ask. The filters still at those call
    /// sites are idempotent against this and harmless; they are simply no
    /// longer load-bearing.
    ///
    /// A caller wanting something human-readable for an unnamed output still
    /// falls back to [`description`](Self::description).
    #[must_use]
    pub fn connector(&self) -> Option<String> {
        named_connector(self.inner.connector().map(|s| s.to_string()))
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

/// The empty-connector policy, once (#1180 item 6): a name that is empty (or
/// only whitespace) is not a name.
///
/// A free function over the already-read `Option<String>` rather than a method
/// on [`Monitor`], for `plugins::region::named_connector`'s reason — the
/// helper this replaces: it keeps the decision pure, so it is testable with no
/// display at all, which is the only environment CI's hermetic bucket has.
fn named_connector(connector: Option<String>) -> Option<String> {
    connector.filter(|name| !name.trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::named_connector;

    /// **#1180 item 6.** One empty-connector policy, applied where the name
    /// is read rather than at each of six call sites.
    ///
    /// `Some("")` is what GDK answers for an output some drivers do not name,
    /// and it is the value that used to reach `components::monitor_key`
    /// unfiltered — where it became the map key `""`, shared by *every*
    /// unnamed output on the machine: one drawer-open flag, one toast surface
    /// and one OSD between them, instead of a per-monitor fallback each.
    ///
    /// **Falsified** by dropping the filter from [`named_connector`]: the
    /// empty and whitespace cases come back `Some`.
    #[test]
    fn an_empty_connector_name_is_not_a_name() {
        assert_eq!(
            named_connector(Some("DP-1".to_owned())),
            Some("DP-1".to_owned()),
            "a real connector passes through untouched",
        );
        assert_eq!(named_connector(None), None, "GDK's own None stays None");
        assert_eq!(
            named_connector(Some(String::new())),
            None,
            "an empty name must degrade exactly like no name at all",
        );
        assert_eq!(
            named_connector(Some("   ".to_owned())),
            None,
            "…and so must one that is only whitespace: no compositor answers to it",
        );
    }
}
