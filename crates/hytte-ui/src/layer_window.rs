//! Thin wrapper around `gtk4-layer-shell` that yields a configured
//! `gtk::Window` ready to host shell content.
//!
//! `Bar` (next module) is layered on top of this. Consumers wanting a
//! non-`Bar` layer surface (e.g. an OSD or a wallpaper) can use
//! `LayerWindow` directly.

use crate::Monitor;
use gtk::gdk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge as LsEdge, KeyboardMode, Layer, LayerShell};

/// Which screen edge(s) a layer surface is pinned to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    /// Pin to the top edge.
    Top,
    /// Pin to the bottom edge.
    Bottom,
    /// Pin to the left edge.
    Left,
    /// Pin to the right edge.
    Right,
}

/// Per-edge gaps (logical pixels) between a layer surface and the screen
/// edges it is anchored to.
#[derive(Clone, Copy, Debug, Default)]
pub struct Margin {
    /// Gap from the top edge.
    pub top: i32,
    /// Gap from the right edge.
    pub right: i32,
    /// Gap from the bottom edge.
    pub bottom: i32,
    /// Gap from the left edge.
    pub left: i32,
}

/// Builder for a configured layer-shell `gtk::Window`. Start from
/// [`layer_window`].
pub struct LayerWindowBuilder {
    monitor: Monitor,
    layer: Layer,
    anchors: Vec<Anchor>,
    margin: Margin,
    namespace: String,
    exclusive: bool,
    keyboard_mode: Option<KeyboardMode>,
}

impl LayerWindowBuilder {
    /// Add a screen edge to anchor to (repeatable; anchoring to opposite
    /// edges stretches the surface across that axis).
    #[must_use]
    pub fn anchor(mut self, edge: Anchor) -> Self {
        self.anchors.push(edge);
        self
    }

    /// Set the gaps between the surface and its anchored edges.
    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    /// Choose the layer-shell layer (background/bottom/top/overlay) the
    /// surface sits on.
    #[must_use]
    pub fn layer(mut self, layer: Layer) -> Self {
        self.layer = layer;
        self
    }

    /// Set the layer-shell namespace (a compositor-visible surface tag).
    #[must_use]
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    /// Reserve an exclusive zone so tiled windows don't overlap the surface
    /// (used by bars).
    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    /// Set how the surface accepts keyboard focus.
    #[must_use]
    pub fn keyboard_mode(mut self, mode: KeyboardMode) -> Self {
        self.keyboard_mode = Some(mode);
        self
    }

    /// Construct the `gtk::Window`, wire up layer-shell, but don't show.
    ///
    /// # Surface lifecycle (map-once)
    ///
    /// A persistent layer surface maps **synchronously** inside the first
    /// `set_visible(true)` and then, for the life of the window, never remaps
    /// — the underlying `gdk::Surface` is created once and reused. Two timing
    /// footguns follow:
    ///
    /// * Reading `window.surface()` *before* that first `set_visible(true)`
    ///   returns `None` — there is no surface yet.
    /// * Wiring anything onto the surface (an input region for click-through,
    ///   a blur region, …) *after* the window has already mapped silently
    ///   never applies, because there is no second `map` to hook.
    ///
    /// This is the exact shape of the #192/#193 frost regressions, where blur
    /// was attached after the one-and-only map and did nothing. Use
    /// [`on_surface_ready`] to run surface-touching code at the right moment
    /// regardless of whether the surface has mapped yet.
    #[must_use]
    pub fn build(self) -> gtk::Window {
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(self.layer);
        window.set_namespace(Some(&self.namespace));
        window.set_monitor(Some(self.monitor.gdk()));

        for anchor in &self.anchors {
            window.set_anchor(map_edge(*anchor), true);
        }

        window.set_margin(LsEdge::Top, self.margin.top);
        window.set_margin(LsEdge::Right, self.margin.right);
        window.set_margin(LsEdge::Bottom, self.margin.bottom);
        window.set_margin(LsEdge::Left, self.margin.left);

        if self.exclusive {
            window.auto_exclusive_zone_enable();
        }

        if let Some(mode) = self.keyboard_mode {
            window.set_keyboard_mode(mode);
        }

        window
    }
}

/// Start building a layer-shell `gtk::Window` on `monitor`.
#[must_use]
pub fn layer_window(monitor: &Monitor) -> LayerWindowBuilder {
    LayerWindowBuilder {
        monitor: monitor.clone(),
        layer: Layer::Top,
        anchors: Vec::new(),
        margin: Margin::default(),
        namespace: String::from("hytte"),
        exclusive: false,
        keyboard_mode: None,
    }
}

/// Run `apply` with the window's `gdk::Surface` as soon as it exists, and
/// again on every subsequent (re)map.
///
/// Layer surfaces built by [`LayerWindowBuilder::build`] map synchronously on
/// the first `set_visible(true)` and — for a persistent surface — never remap
/// (see that method's *Surface lifecycle* note); a window toggled via
/// `set_visible` instead remaps on each show. This helper covers both cases: it
/// hooks `map` (so a not-yet-mapped or repeatedly-remapping window applies on
/// every map) and, if the window is *already* mapped when called, applies once
/// immediately. So it works whether it is wired before or after the surface
/// first appears — sidestepping the map-once timing footgun behind the #192/#193
/// frost regressions, where surface wiring ran after the sole map and silently
/// did nothing.
///
/// `apply` receives the live `gdk::Surface` — set an input region for
/// click-through, a blur region, etc. It may run more than once (every map), so
/// keep it idempotent.
pub fn on_surface_ready<F>(window: &gtk::Window, apply: F)
where
    F: Fn(&gdk::Surface) + 'static,
{
    let apply = std::rc::Rc::new(apply);
    let on_map = apply.clone();
    window.connect_map(move |w| {
        if let Some(surface) = w.surface() {
            on_map(&surface);
        } else {
            tracing::warn!("on_surface_ready: window mapped without a surface");
        }
    });
    if window.is_mapped()
        && let Some(surface) = window.surface()
    {
        apply(&surface);
    }
}

fn map_edge(a: Anchor) -> LsEdge {
    match a {
        Anchor::Top => LsEdge::Top,
        Anchor::Bottom => LsEdge::Bottom,
        Anchor::Left => LsEdge::Left,
        Anchor::Right => LsEdge::Right,
    }
}
