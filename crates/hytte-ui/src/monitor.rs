//! Thin wrapper around `gdk::Monitor` carrying just the metadata bars need.

use std::collections::HashSet;

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
    /// On a monitor reached through [`App::monitors`](crate::App::monitors)
    /// or [`App::monitors_changed`](crate::App::monitors_changed), `None`
    /// means GTK has no name for it, never that the name is still in flight:
    /// those list a hot-plugged output only once GTK has applied its first
    /// `done`, which `wl_output` v4 sends after `name` (#1368). Below v4 it
    /// is GTK, not the compositor, that leaves the output nameless — GTK 4.22
    /// ignores `zxdg_output_v1.name` (`gdk/wayland/gdkmonitor-wayland.c:117-122`).
    /// Before #1368, a returning output could be listed nameless and stay
    /// that way until a restart.
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
    /// policy instead of seven: an empty name is not a name. The invariant
    /// holds tree-wide — nothing reads `gdk::Monitor::connector()` raw
    /// (#1177); the display-wide set of names is [`Monitor::attached_connectors`].
    ///
    /// A caller wanting something human-readable for an unnamed output still
    /// falls back to [`description`](Self::description).
    #[must_use]
    pub fn connector(&self) -> Option<String> {
        named_connector(self.inner.connector().map(|s| s.to_string()))
    }

    /// Every connector name of the monitors `display` currently reports,
    /// with the empty-name policy of [`connector`](Self::connector) applied.
    #[must_use]
    pub fn attached_connectors(display: &gdk::Display) -> HashSet<String> {
        use gtk::gdk::prelude::DisplayExt;
        use gtk::prelude::Cast;
        connector_set(
            display
                .monitors()
                .into_iter()
                .filter_map(Result::ok)
                .filter_map(|obj| obj.downcast::<gdk::Monitor>().ok())
                .map(|m| Monitor::new(m).connector()),
        )
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
    /// `monitors`-model `items_changed`, so `App::monitors_changed` never
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

/// The set of real connector names among `names`: [`named_connector`] over
/// each, unnamed outputs dropped. Pure, so the "no empty name in the set"
/// invariant is testable with no display.
fn connector_set(names: impl IntoIterator<Item = Option<String>>) -> HashSet<String> {
    names.into_iter().filter_map(named_connector).collect()
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
    use super::{connector_set, named_connector};

    /// **#1448.** The display-wide set of known connectors never holds an
    /// empty or whitespace name; `plugins::region` reads it for its
    /// "unknown output" log decision.
    #[test]
    fn the_connector_set_never_holds_an_empty_name() {
        let set = connector_set([
            Some("DP-1".to_owned()),
            Some(String::new()),
            Some("  ".to_owned()),
            None,
            Some("HDMI-A-1".to_owned()),
        ]);
        assert_eq!(set.len(), 2, "{set:?}");
        assert!(set.contains("DP-1") && set.contains("HDMI-A-1"));
        assert!(!set.contains(""), "an empty name must not enter the set");
        assert!(!set.contains("  "));
    }

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
