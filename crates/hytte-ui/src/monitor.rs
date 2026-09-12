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
    /// answer `Some("")`, and this doc used to say so ("may be empty on some
    /// drivers; callers should fall back") and leave the handling to seven
    /// call sites, which grew two different policies. Six filtered the empty
    /// string out — `fullscreen::install` and `overlays::consent::install`
    /// with `.filter(|c| !c.is_empty())`, `plugins::region::named_connector`
    /// with its own helper, and the frame / OSD / toast overlay installers
    /// with a `Some(c) if !c.is_empty()` arm that skips the output and logs.
    ///
    /// **`components::monitor_key` is the one that did not**, and it is the
    /// one that mattered: it is the key behind `modal::DRAWER_OPEN`,
    /// `overlays::sidebar::SIDEBAR_OPEN` and `panels::stats::PANELS`, so on a
    /// driver answering `Some("")` every unnamed output collided on the key
    /// `""` instead of taking its own `monitor:{ptr}` fallback — one
    /// drawer-open flag and one sidebar-open flag shared between two
    /// monitors, and the second output's stats panel displacing the first's.
    /// (**PR #1199 review, LOW 1** corrects this paragraph: it used to name
    /// the toast/OSD/frame maps as unfiltered too, and say two outputs shared
    /// a toast surface and an OSD. They filter, and an unnamed output gets
    /// **no** such overlay installed rather than a shared one.) Nor was the
    /// `""` entry ever pruned: `is_fallback_key("")` is `false`, so it
    /// survived every `close_all` retain.
    ///
    /// Folding it here rather than at the call sites is what makes that one
    /// policy instead of seven: an empty name is not a name, and no caller
    /// that reads the name *through this type* has to remember to ask. The
    /// filters still at those call sites are idempotent against this and
    /// harmless; they are simply no longer load-bearing, and #1177 owns
    /// removing them.
    ///
    /// **It is not yet true that no caller can forget** (**PR #1199 review,
    /// LOW 2**): `plugins::region::attached_connectors` reads
    /// `gdk::Monitor::connector()` off the GDK object directly, so it never
    /// passes through here and can still put `""` in the attached-connector
    /// set. The consequence there is one cosmetic log decision ("is this
    /// region's connector one this display knows about"), which is why that
    /// site is a cleanup for #1177 rather than a fix here — but the sentence
    /// above is scoped to callers that go through [`Monitor`] deliberately,
    /// not out of optimism.
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
    /// is read rather than at each of seven call sites.
    ///
    /// `Some("")` is what GDK answers for an output some drivers do not name,
    /// and it is the value that used to reach `components::monitor_key`
    /// unfiltered — where it became the map key `""`, shared by *every*
    /// unnamed output on the machine: one `DRAWER_OPEN` flag, one
    /// `SIDEBAR_OPEN` flag and one stats `PANELS` entry between them, instead
    /// of a per-monitor `monitor:{ptr}` fallback each. (The frame, OSD and
    /// toast installers filtered it themselves and still do — an unnamed
    /// output gets no such overlay at all; **PR #1199 review, LOW 1**
    /// corrected this doc, which used to name them here too.)
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
