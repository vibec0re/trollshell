//! Thin wrapper around `gdk::Monitor` carrying just the metadata bars need.

use gtk::gdk;
use gtk::gdk::prelude::MonitorExt;

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

    /// Underlying `gdk::Monitor` for direct GTK calls (e.g. layer-shell).
    #[must_use]
    pub fn gdk(&self) -> &gdk::Monitor {
        &self.inner
    }
}
