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

// The #212/#192/#193 fix — this module's whole reason for having
// `on_surface_ready` at all — shipped with nothing testing it: deleting the
// already-mapped branch left every `hytte-ui` test green (#1180 item 8).
//
// What is exercised here is the *timing contract*, which is plain-GTK
// toplevel mechanics and needs no compositor: a persistent layer surface maps
// once and never again, so surface wiring either rides that one map or it
// never happens. A bare `gtk::Window` stands in — the same substitution
// `bar`'s own gated test makes — because the distinction under test
// (is the surface there yet, and did the callback run for this map) is not a
// layer-shell one. What genuinely needs niri, and stays in
// `docs/live-verify.md`, is that the region a caller sets inside `apply` is
// the one the compositor frosts.
//
// Needs a display → `system-tests`, run under `xvfb-run`.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::on_surface_ready;
    use gtk::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Spin the main loop until `done` or the bound runs out, so a test never
    /// hangs on a display that will not map.
    fn settle(done: impl Fn() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            if !gtk::glib::MainContext::default().iteration(false) {
                // Nothing left to dispatch; one more check and give up.
                break;
            }
        }
    }

    /// **#1180 item 8, the #212 half.** Surface wiring runs *within the first
    /// map*, ahead of anything connected after it — not on some later turn of
    /// the loop.
    ///
    /// The order is recorded through a probe: a second `map` handler is
    /// connected **after** `on_surface_ready`, so GTK runs it second, and the
    /// trace has to read `["apply", "map"]`. That is what makes this an
    /// ordering test rather than a "did it run eventually" one — a callback
    /// deferred to an idle handler (a plausible-looking fix for the
    /// not-yet-mapped case) would still run, and would still be too late for
    /// a caller that needs the surface configured before the compositor sees
    /// it mapped.
    ///
    /// **Falsified** by deferring the `apply` inside `on_surface_ready`'s map
    /// handler (`glib::idle_add_local_once`): the trace reads `["map",
    /// "apply"]`.
    #[gtk::test]
    fn surface_wiring_runs_inside_the_first_map() {
        let window = gtk::Window::new();
        let trace: Rc<RefCell<Vec<&'static str>>> = Rc::new(RefCell::new(Vec::new()));

        let for_apply = Rc::clone(&trace);
        on_surface_ready(&window, move |_| for_apply.borrow_mut().push("apply"));

        let for_map = Rc::clone(&trace);
        window.connect_map(move |_| for_map.borrow_mut().push("map"));

        assert!(
            trace.borrow().is_empty(),
            "nothing runs before the window is shown — there is no surface yet",
        );

        window.present();
        settle(|| !trace.borrow().is_empty());

        assert_eq!(
            *trace.borrow(),
            vec!["apply", "map"],
            "the surface callback must run inside the first map emission, before handlers \
             connected after it — not deferred to a later loop turn (#212/#192/#193)",
        );

        window.destroy();
    }

    /// **#1180 item 8, the #193 half** — the five lines whose deletion left
    /// every test in this crate green.
    ///
    /// A persistent layer surface maps synchronously inside the first
    /// `set_visible(true)` and then never remaps, so a caller that wires
    /// *after* that one map has no second map to hook: without the
    /// already-mapped branch its `apply` is never called at all, silently.
    /// That is the exact shape of the #192/#193 frost regressions, where the
    /// blur region was attached after the sole map and did nothing — and the
    /// tell was zero blur log lines, because nothing failed.
    ///
    /// The assertion is deliberately made **before** the loop is spun: the
    /// branch has to apply synchronously, since a caller wiring after the map
    /// gets no other callback to wait for.
    ///
    /// **Falsified** by deleting the `if window.is_mapped() && …` branch from
    /// `on_surface_ready`: `applied` stays 0.
    #[gtk::test]
    fn wiring_an_already_mapped_window_applies_at_once() {
        let window = gtk::Window::new();
        window.present();
        settle(|| window.is_mapped() && window.surface().is_some());
        assert!(
            window.is_mapped() && window.surface().is_some(),
            "the fixture needs a mapped window with a live surface",
        );

        let applied = Rc::new(std::cell::Cell::new(0_u32));
        let for_apply = Rc::clone(&applied);
        on_surface_ready(&window, move |surface| {
            assert!(surface.is_mapped(), "the callback is handed a live surface");
            for_apply.set(for_apply.get() + 1);
        });

        assert_eq!(
            applied.get(),
            1,
            "wiring after the one-and-only map must apply immediately — there is no second \
             map to hook, and a missed apply is silent (#192/#193)",
        );

        // …and exactly once: the immediate call must not also queue one.
        settle(|| false);
        assert_eq!(applied.get(), 1, "and not a second time on the same map");

        window.destroy();
    }
}
