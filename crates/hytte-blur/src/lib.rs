//! Client-side `ext-background-effect-v1` blur-region scoping.
//!
//! niri's `ext-background-effect` layer-rule (`blur true`) frosts the **entire**
//! layer-shell surface geometry, not the painted content. trollshell's always-
//! mapped sidebar surface and its fullscreen drawer surface therefore frost a
//! lingering strip / the whole screen (issues #192 / #193). This crate hands
//! niri a *sub-rectangle* — "blur only here" — via the surface's own
//! `set_blur_region` request, scoping the frost to the visible card.
//!
//! ## GTK ↔ Wayland interop (no `unsafe` needed)
//!
//! We call the protocol on **GTK's own `wl_surface`**, over **GTK's own
//! libwayland connection** — not a separate one — or the proxies wouldn't
//! interoperate. `gdk4-wayland` (feature `wayland_crate`) hands us the raw
//! `wayland-client` objects via safe getters: `WaylandSurface::wl_surface()`,
//! `WaylandDisplay::wl_display()`, `WaylandDisplay::wl_compositor()`. We wrap
//! GTK's display backend into a `wayland_client::Connection`, bind the
//! `ext_background_effect_manager_v1` global once (cached per thread), and issue
//! requests on the **same backend** so they flush over GTK's socket. The
//! generated protocol code lives in the dependency crates (`wayland-client` /
//! `wayland-protocols`), so this crate touches only their safe APIs — it needs
//! no `unsafe` and inherits the workspace `unsafe_code = "forbid"`. All of it
//! lives behind the safe [`attach`] / [`SurfaceBlur::set_region`] API;
//! downstream (sidebar/modal) stays `unsafe`-free too.
//!
//! ## Commit timing
//!
//! `set_blur_region` is double-buffered — it applies on the surface's **next
//! `wl_surface.commit`**. We don't commit GTK's surface ourselves (GTK owns
//! its frame lifecycle); instead the caller should `queue_draw()` the window
//! after changing the region, which makes GTK commit on the next frame. We
//! `flush()` the connection so the request reaches the compositor even if no
//! draw is imminent. See [`SurfaceBlur::set_region`].
//!
//! ## Graceful no-op
//!
//! [`attach`] returns `None` when the compositor doesn't advertise the
//! `ext_background_effect_manager_v1` global or the `blur` capability (niri
//! < 26.04, or any non-niri compositor). Callers treat `None` as "no client-
//! side scoping" and fall back to the niri layer-rule blur (the Tier-1
//! `etc/niri/blur.kdl` rules stay in place exactly for this).

mod protocol;

// GTK traits: WidgetExt (surface/display/queue_draw), RootExt, the cast traits.
use gtk::prelude::*;
// `WaylandSurface::wl_surface()` is on this manual extension trait;
// `WaylandDisplay::{wl_display,wl_compositor}()` are inherent methods.
use gdk4_wayland::prelude::WaylandSurfaceExtManual;
use gdk4_wayland::{WaylandDisplay, WaylandSurface};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use protocol::ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1;
use protocol::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

/// A surface-local rectangle, in CSS/logical pixels, matching `gtk::gdk`'s.
pub type Rect = gtk::gdk::Rectangle;

/// Dispatch sink for the throwaway bind queue. We only need it to learn the
/// manager's advertised `capabilities` (the `blur` bit) at bind time; the
/// effect-surface and region objects emit no events.
#[derive(Default)]
struct BlurState {
    /// `true` once the manager's `capabilities` event has advertised the
    /// `blur` capability (bit 0). The only capability defined in v1.
    blur: bool,
}

impl BlurState {
    /// True once the compositor has advertised the `blur` capability.
    fn blur_supported(&self) -> bool {
        self.blur
    }
}

// `registry_queue_init` requires the state to dispatch registry globals; the
// helper drives this, we just need the impl to exist.
impl Dispatch<WlRegistry, GlobalListContents> for BlurState {
    fn event(
        _state: &mut Self,
        _registry: &WlRegistry,
        _event: <WlRegistry as Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for BlurState {
    fn event(
        state: &mut Self,
        _mgr: &ExtBackgroundEffectManagerV1,
        event: <ExtBackgroundEffectManagerV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let protocol::ext_background_effect_manager_v1::Event::Capabilities { flags } = event {
            // `flags` is a `WEnum<Capability>` (a bitfield). Take its raw bits
            // and test the `blur` bit — robust to both known and unknown
            // (future) capability bits being set alongside it.
            use protocol::ext_background_effect_manager_v1::Capability;
            let bits = u32::from(flags);
            state.blur = bits & Capability::Blur.bits() != 0;
        }
    }
}

// The effect-surface object emits no events (`ext_background_effect_surface_v1`
// has only requests), but a `Dispatch` impl is still required to create it.
impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _obj: &ExtBackgroundEffectSurfaceV1,
        _event: <ExtBackgroundEffectSurfaceV1 as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

// `wl_region` and `wl_compositor` emit no events; impls required to use them on
// our queue handle.
impl Dispatch<WlRegion, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _region: &WlRegion,
        _event: <WlRegion as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for BlurState {
    fn event(
        _state: &mut Self,
        _compositor: &WlCompositor,
        _event: <WlCompositor as Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Process-wide (per-thread, GTK main thread) bound `ext-background-effect`
/// manager. The manager is a compositor singleton: binding it once and reusing
/// it across every [`attach`] avoids leaking a fresh `wl_registry` (no
/// destructor) plus an `ext_background_effect_manager_v1` (wayland-rs proxies
/// don't auto-send their destructors on drop) on every drawer open, and avoids
/// re-paying the two blocking round-trips. The wayland objects are `!Send`, so
/// a `thread_local` on the GTK main thread is the correct home; the bound
/// manager intentionally lives for the process lifetime.
struct BlurManager {
    conn: Connection,
    qh: QueueHandle<BlurState>,
    manager: ExtBackgroundEffectManagerV1,
}

thread_local! {
    /// `Some(BlurManager)` once the manager has been bound and advertised the
    /// `blur` capability; `None` if the compositor doesn't expose the global or
    /// the capability (niri < 26.04 / non-niri) — cached either way so an
    /// unsupported compositor isn't re-probed on every [`attach`].
    static MANAGER: std::cell::OnceCell<Option<BlurManager>> = const { std::cell::OnceCell::new() };
}

/// Bind the `ext-background-effect` manager once (first call) and return a
/// reference-friendly view of the cached result. The bind + `capabilities`
/// round-trip happens exactly once per thread; subsequent calls reuse the
/// cache (including the unsupported `None` case). Returns the bits a
/// [`SurfaceBlur`] needs cloned out of the cache so the borrow doesn't escape
/// the `with` closure.
///
/// `wl_display_obj` is GTK's own `wl_display` (so our requests ride GTK's
/// socket and the proxies interoperate with GTK's `wl_surface`).
fn cached_manager(
    wl_display_obj: &wayland_client::protocol::wl_display::WlDisplay,
) -> Option<(
    Connection,
    QueueHandle<BlurState>,
    ExtBackgroundEffectManagerV1,
)> {
    MANAGER.with(|cell| {
        let bound = cell.get_or_init(|| bind_manager(wl_display_obj));
        bound
            .as_ref()
            .map(|m| (m.conn.clone(), m.qh.clone(), m.manager.clone()))
    })
}

/// First-time bind of the `ext-background-effect` manager on GTK's connection.
/// Returns `None` when the global is absent or the `blur` capability isn't
/// advertised — the graceful niri < 26.04 / non-niri no-op.
fn bind_manager(
    wl_display_obj: &wayland_client::protocol::wl_display::WlDisplay,
) -> Option<BlurManager> {
    // Wrap GTK's backend into a Connection so our requests flush over GTK's
    // socket (same fd → the proxies interoperate with GTK's wl_surface).
    let backend = wl_display_obj.backend().upgrade()?;
    let conn = Connection::from_backend(backend);

    // Registry queue used to bind the manager + drain its initial
    // `capabilities` event. Created exactly once and kept alive in the cache;
    // the effect/region objects we later create on `qh` ride the same
    // connection. GTK's own queue drives the surface's frame commits.
    let (globals, mut queue) = registry_queue_init::<BlurState>(&conn).ok()?;
    let qh = queue.handle();

    // v1 only; bind exactly version 1.
    let manager: ExtBackgroundEffectManagerV1 = globals.bind(&qh, 1..=1, ()).ok()?;

    // Round-trip so the `capabilities` event lands in BlurState before we
    // decide whether blur is supported.
    let mut state = BlurState::default();
    queue.roundtrip(&mut state).ok()?;
    if !state.blur_supported() {
        tracing::debug!("ext-background-effect manager bound but blur capability absent");
        return None;
    }

    tracing::debug!("bound ext-background-effect manager (blur capability present)");
    Some(BlurManager { conn, qh, manager })
}

/// A live blur-region handle for one GTK window's surface.
///
/// Holds the `ext_background_effect_surface_v1` plus the bits needed to build
/// `wl_region`s on GTK's connection. Dropping it issues `destroy`, which (per
/// the protocol) removes the effect on the surface's next commit.
///
/// The `conn`/`qh` are clones of the process-wide cached [`BlurManager`]'s; the
/// `compositor`/`surface`/`effect`/`window` are this surface's own. Only the
/// per-surface `effect` is destroyed on drop — the cached manager outlives every
/// `SurfaceBlur`.
pub struct SurfaceBlur {
    conn: Connection,
    qh: QueueHandle<BlurState>,
    compositor: WlCompositor,
    surface: WlSurface,
    effect: ExtBackgroundEffectSurfaceV1,
    /// Kept alive so we can `queue_draw` the GTK window after setting a region,
    /// nudging GTK to commit the surface (which is when the region applies).
    window: gtk::Window,
}

/// Attach a blur-region scope to `window`'s layer-shell surface.
///
/// Returns `None` (a graceful no-op) when:
/// - the window isn't realized on a Wayland display yet, or its `wl_surface`
///   isn't available;
/// - the compositor doesn't expose `ext_background_effect_manager_v1` (niri
///   < 26.04 / non-niri); or
/// - the manager doesn't advertise the `blur` capability.
///
/// Must be called **after** the window is mapped/realized (its `wl_surface`
/// exists). Callers that build the window then `set_visible(true)` should
/// attach right after, or on the window's first `map`.
///
/// The `ext-background-effect` manager is bound once per thread and cached (see
/// [`cached_manager`]); this call only creates the per-surface effect object, so
/// the per-open drawer churn no longer leaks a registry + manager.
pub fn attach(window: &gtk::Window) -> Option<SurfaceBlur> {
    let surface_widget = window.surface()?;
    // GTK is Wayland-only here; downcast the GdkSurface / GdkDisplay to the
    // Wayland variants to reach the raw wayland-client objects. `display()` is
    // on both RootExt and WidgetExt — disambiguate to the widget one.
    let wl_surface = surface_widget
        .downcast_ref::<WaylandSurface>()
        .and_then(WaylandSurface::wl_surface)?;
    let display = WidgetExt::display(window);
    let wl_display = display.downcast_ref::<WaylandDisplay>()?;
    let wl_display_obj = wl_display.wl_display()?;
    // GTK's existing `wl_compositor` (not a fresh bind → no leak).
    let compositor = wl_display.wl_compositor()?;

    // Reuse the cached, process-wide manager (binds + probes on first call).
    // `None` here preserves the graceful niri < 26.04 no-op.
    let (conn, qh, manager) = cached_manager(&wl_display_obj)?;

    let effect = manager.get_background_effect(&wl_surface, &qh, ());

    tracing::debug!("attached ext-background-effect blur scope to layer surface");
    Some(SurfaceBlur {
        conn,
        qh,
        compositor,
        surface: wl_surface,
        effect,
        window: window.clone(),
    })
}

/// Flush GTK's Wayland connection so freshly-issued layer-shell requests reach
/// the compositor *now*, instead of waiting for GTK's own next flush.
///
/// gtk4-layer-shell enqueues requests like `zwlr_layer_surface_v1.set_exclusive_zone`
/// on GTK's libwayland connection and forces a commit, but the bytes only leave
/// the process when that connection is flushed — which GTK normally does on its
/// frame cycle. A surface that has just gone idle (e.g. a sidebar settling
/// closed: revealer collapsed, nothing left to paint) may not produce another
/// frame promptly, so the request can sit unflushed and the compositor never
/// reflows tiles. Pushing the connection explicitly is the same `conn.flush()`
/// that makes [`SurfaceBlur::set_region`] land on close; callers driving the
/// exclusive zone should call this right after `set_exclusive_zone`.
///
/// Works regardless of niri version (it only needs GTK's `wl_display`, not the
/// `ext-background-effect` manager). Safe no-op if `window` isn't realized on a
/// Wayland display yet.
pub fn flush(window: &gtk::Window) {
    if let Some(conn) = gtk_connection(window) {
        let _ = conn.flush();
    }
}

/// Wrap GTK's own libwayland backend into a `Connection` so a `flush()` here
/// pushes the very socket GTK uses (same fd → it carries GTK's queued
/// requests). `None` until the window is realized on a Wayland display.
fn gtk_connection(window: &gtk::Window) -> Option<Connection> {
    let display = WidgetExt::display(window);
    let wl_display = display.downcast_ref::<WaylandDisplay>()?;
    let wl_display_obj = wl_display.wl_display()?;
    let backend = wl_display_obj.backend().upgrade()?;
    Some(Connection::from_backend(backend))
}

impl SurfaceBlur {
    /// Scope the surface's blur to `rect` (surface-local, logical px), or clear
    /// it with `None`.
    ///
    /// Both cases hand niri an explicit `wl_region`: `Some(rect)` covers `rect`;
    /// `None` sends an **empty** region. We deliberately never send a NULL region.
    /// niri distinguishes the two (`src/render_helpers/background_effect.rs`):
    /// a non-NULL but empty region short-circuits to "blur nothing"
    /// (`if rects.is_empty() { return None }`), whereas a NULL region leaves
    /// `has_blur_region` false so niri **reverts to the layer-rule**
    /// `background-effect { blur true }` and frosts the *entire* surface geometry.
    /// On an always-mapped surface (the sidebar) the NULL path therefore re-frosts
    /// the whole still-mapped surface once its content collapses — the lingering
    /// grey strip (#192/#194). Clearing with an empty region suppresses the frost
    /// instead of falling back to it.
    ///
    /// The change is double-buffered and applies on the surface's **next commit**
    /// — we `flush()` the request and `queue_draw()` the window so GTK commits on
    /// its next frame.
    ///
    /// Coordinate note: niri clips the region to the surface size, so an
    /// over-large rect is harmless; an empty/zero rect blurs nothing (the
    /// closed-state case) without reverting to the layer-rule frost.
    pub fn set_region(&self, rect: Option<Rect>) {
        // Always build a region object and set it — never NULL (see doc above).
        // An empty region (no `add`) is the deliberate "clear" path.
        let region = self.compositor.create_region(&self.qh, ());
        if let Some(r) = rect {
            region.add(r.x(), r.y(), r.width(), r.height());
        }
        self.effect.set_blur_region(Some(&region));
        // `set_blur_region` has copy semantics; the region can go now.
        region.destroy();
        // Push the request to the compositor, then nudge GTK to commit the
        // surface (when the double-buffered region actually applies).
        let _ = self.conn.flush();
        self.window.queue_draw();
    }

    /// Scope the surface's blur to the union of `rects` (surface-local, logical
    /// px). The multi-rectangle counterpart of [`Self::set_region`]: it builds a
    /// single `wl_region`, `add`s every rect, and hands it to niri. An **empty
    /// slice** sends an empty (but non-NULL) region — suppressing the frost,
    /// exactly like `set_region(None)` — and never a NULL region (see
    /// [`Self::set_region`]'s note on why NULL would revert to the whole-surface
    /// layer-rule frost).
    ///
    /// Used by the fullscreen frame overlay to frost only its three border
    /// strips ([`frame_border_rects`]), not the workspace cutout (which would
    /// frost the whole screen). Same commit timing as [`Self::set_region`]: the
    /// region is double-buffered, so we `flush()` then `queue_draw()` to land it
    /// on GTK's next frame.
    pub fn set_region_rects(&self, rects: &[Rect]) {
        // Mirror `set_region`'s sequence exactly: one region built on GTK's
        // queue, every rect added, set, destroyed (copy semantics), flushed,
        // then a draw nudge so the double-buffered region commits.
        let region = self.compositor.create_region(&self.qh, ());
        for r in rects {
            region.add(r.x(), r.y(), r.width(), r.height());
        }
        self.effect.set_blur_region(Some(&region));
        region.destroy();
        let _ = self.conn.flush();
        self.window.queue_draw();
    }
}

impl Drop for SurfaceBlur {
    fn drop(&mut self) {
        // Removes the effect on the surface's next commit (per protocol).
        self.effect.destroy();
        let _ = self.conn.flush();
        // Best-effort nudge so the removal commits even if the window stays put.
        if self.surface.is_alive() {
            self.window.queue_draw();
        }
    }
}

/// Compute the surface-local blur rectangle for a sliding panel, given the
/// currently-visible width and the full surface height. Returns `None` (clear
/// the frost) when `visible_width <= 0` — i.e. the panel is closed/collapsed —
/// so no strip lingers. Pure helper, unit-tested; the wayland calls aren't.
#[must_use]
pub fn left_panel_region(visible_width: i32, surface_height: i32) -> Option<Rect> {
    if visible_width <= 0 || surface_height <= 0 {
        return None;
    }
    Some(Rect::new(0, 0, visible_width, surface_height))
}

/// The three border strips of the fullscreen frame overlay — left, right, and
/// bottom — as surface-local rectangles (logical px), for [`SurfaceBlur::set_region_rects`].
///
/// Frosts only the frame's painted border, never the workspace cutout (frosting
/// the cutout would frost the whole screen). The layout mirrors `frame.rs`'s
/// cairo paint: `bar_height` is the top inset (the bar's exclusive zone),
/// `thickness` is the frame strut width, and `left_inset` is the cutout's left
/// edge (the sidebar's current visible width — `thickness` when closed, the
/// sidebar width when open):
///
/// - **left:**   `(0, bar_height, left_inset, height - bar_height)`
/// - **right:**  `(width - thickness, bar_height, thickness, height - bar_height)`
/// - **bottom:** `(left_inset, height - thickness, width - left_inset - thickness, thickness)`
///
/// All dimensions are clamped to `>= 0`, so a degenerate (pre-map / tiny)
/// surface yields empty rects rather than negative ones (niri also clips an
/// over-large rect to the surface, so an empty rect simply blurs nothing).
#[must_use]
pub fn frame_border_rects(
    width: i32,
    height: i32,
    left_inset: i32,
    bar_height: i32,
    thickness: i32,
) -> [Rect; 3] {
    let left_inset = left_inset.max(0);
    let bar_height = bar_height.max(0);
    let thickness = thickness.max(0);
    // Height of the body region below the bar — shared by the L/R strips.
    let body_h = (height - bar_height).max(0);
    [
        // Left strip: screen's left edge → cutout's left edge.
        Rect::new(0, bar_height, left_inset, body_h),
        // Right strip: the fixed-thickness strut on the right edge.
        Rect::new((width - thickness).max(0), bar_height, thickness, body_h),
        // Bottom strip: between the L/R struts, below the cutout.
        Rect::new(
            left_inset,
            (height - thickness).max(0),
            (width - left_inset - thickness).max(0),
            thickness,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_panel_clears_region() {
        // Collapsed sidebar (0 visible width) → no blur region → no strip.
        assert!(left_panel_region(0, 1080).is_none());
        assert!(left_panel_region(-5, 1080).is_none());
    }

    #[test]
    fn zero_height_clears_region() {
        // Pre-map / degenerate height → nothing to blur.
        assert!(left_panel_region(320, 0).is_none());
    }

    #[test]
    fn open_panel_region_hugs_visible_width() {
        let r = left_panel_region(320, 1080).expect("open panel → Some(rect)");
        assert_eq!((r.x(), r.y(), r.width(), r.height()), (0, 0, 320, 1080));
    }

    #[test]
    fn mid_slide_region_tracks_partial_width() {
        // During the open animation the region follows the partial width so the
        // frost slides in with the card rather than snapping.
        let r = left_panel_region(140, 1080).expect("animating → Some(rect)");
        assert_eq!(r.width(), 140);
    }

    fn xywh(r: Rect) -> (i32, i32, i32, i32) {
        (r.x(), r.y(), r.width(), r.height())
    }

    #[test]
    fn frame_border_rects_closed_sidebar() {
        // 1920x1080, bar 44, strut 8, sidebar closed (left_inset == strut).
        let [left, right, bottom] = frame_border_rects(1920, 1080, 8, 44, 8);
        // Left strip is the 8px L-strut below the bar.
        assert_eq!(xywh(left), (0, 44, 8, 1080 - 44));
        // Right strip is the 8px R-strut, flush to the right edge.
        assert_eq!(xywh(right), (1920 - 8, 44, 8, 1080 - 44));
        // Bottom strip spans between the struts, 8px tall, at the bottom edge.
        assert_eq!(xywh(bottom), (8, 1080 - 8, 1920 - 8 - 8, 8));
    }

    #[test]
    fn frame_border_rects_open_sidebar_widens_left_strip() {
        // Sidebar open at 320: the left strip widens to the cutout's left edge
        // and the bottom strip shifts/shrinks to start at it.
        let [left, _right, bottom] = frame_border_rects(1920, 1080, 320, 44, 8);
        assert_eq!(xywh(left), (0, 44, 320, 1080 - 44));
        assert_eq!(xywh(bottom), (320, 1080 - 8, 1920 - 320 - 8, 8));
    }

    #[test]
    fn frame_border_rects_tiny_surface_clamps_to_zero() {
        // Degenerate surface smaller than the insets → no negative dimensions.
        let [left, right, bottom] = frame_border_rects(4, 10, 8, 44, 8);
        // body_h = (10 - 44).max(0) = 0; left_inset clamped to its value but
        // widths/offsets that would go negative clamp to 0.
        assert_eq!(left.height(), 0);
        assert_eq!(right.height(), 0);
        assert_eq!(right.x(), 0); // (4 - 8).max(0)
        assert_eq!(bottom.y(), 2); // (10 - 8).max(0)
        assert_eq!(bottom.width(), 0); // (4 - 8 - 8).max(0)
    }
}
