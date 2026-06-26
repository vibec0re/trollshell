//! Per-monitor OVERLAY-layer window that paints the dark frame around
//! the workspace cutout. Full-screen, click-through, no exclusive zone.
//!
//! Layered above the bar (which is on `Layer::Top`). Bar widgets remain
//! interactive because the frame's input region is empty — every click
//! falls through to the layer below.
//!
//! The frame paints a translucent dark purple (`rgba(28, 6, 44, 0.90)`,
//! matching `@shell_background` in `style.css`) into the L/R/bottom border
//! regions and carves four rounded inner corners around the workspace cutout.
//! Top inset is the bar's exclusive zone. Using the same 0.90 alpha as the
//! bar means the bar↔frame boundary is seamless (uniform frost).
//!
//! ## Border frost (client protocol)
//!
//! The frame surface is translucent *and* frosted — but only along its three
//! border strips (left / right / bottom), never the workspace cutout (frosting
//! the cutout would frost the whole screen). We hand niri a three-rectangle
//! blur region via the client `ext-background-effect` `set_blur_region` request
//! ([`hytte::blur`]), so the frost hugs the painted border. The region tracks
//! the sidebar slide: the left strip's width follows the cutout's left edge
//! ([`sidebar::current_visible_width`]). `None` on niri < 26.04 — the frame
//! stays translucent-but-unblurred there (the bar/sidebar layer-rule fallback
//! in `etc/niri/blur.kdl` does not cover the frame). See #189 / #194.
//!
//! Visual constants — match `etc/niri/frame.kdl` struts and the
//! post-restyle bar geometry from `style.css`. If any of these change,
//! update both sides.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::blur::SurfaceBlur;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;
use hytte::ui::{LayerShell, layer_window};

use crate::components::cast;
use crate::overlays::sidebar;

/// Bar height after restyle: `padding: 6px 12px` (12 vertical) + `min-height: 32px` = 44.
/// Top inset of the frame (= top of the workspace cutout).
const BAR_HEIGHT: f64 = 44.0;

/// Frame thickness on left, right, and bottom. Must match the niri
/// `struts` values in `etc/niri/frame.kdl`.
const FRAME_THICKNESS: f64 = 8.0;

/// Corner radius for all four corners of the workspace cutout.
const CUTOUT_RADIUS: f64 = 10.0;

/// Mount one frame overlay on `monitor`.
pub fn install(monitor: &Monitor) {
    let connector = monitor.connector().unwrap_or_default();
    let (mon_w, _mon_h) = monitor.size();
    let mon_w = f64::from(mon_w);

    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .namespace("hytte-frame")
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .build();
    // Span the full output ignoring the bar's exclusive zone. Default is 0
    // ("don't reserve, but be pushed by other surfaces' zones"), which would
    // offset our surface down by the bar's height — leaving a visible gap
    // between the bar's bottom and the frame's top. -1 means "ignore".
    window.set_exclusive_zone(-1);
    window.add_css_class("ts-frame");

    // Transparent drawing area — fills the layer-shell surface.
    let area = gtk::DrawingArea::new();
    area.set_hexpand(true);
    area.set_vexpand(true);
    window.set_child(Some(&area));

    install_draw(&area, monitor.clone());

    // Empty input region: clicks pass through to the bar (Layer::Top below)
    // and to niri's apps (normal layer below that). Set after realize so
    // the surface exists.
    install_click_through(&window);

    // Client-side blur-region scope (#189): frost only the frame's three border
    // strips, not the cutout. The wl_surface exists only once mapped, so attach
    // on first `map` and seed the border region. `None` on niri < 26.04 → the
    // frame stays translucent-but-unblurred (no layer-rule fallback covers it).
    // Shared so the resize handler and the sidebar-slide tick can re-send it.
    let blur: Rc<RefCell<Option<SurfaceBlur>>> = Rc::new(RefCell::new(None));
    wire_blur_attach(&window, &blur, monitor);

    // Re-send the border region whenever the surface is resized (e.g. monitor
    // mode change), since the right/bottom strips are anchored to (w, h).
    let blur_for_resize = blur.clone();
    let window_for_resize = window.clone();
    let monitor_for_resize = monitor.clone();
    area.connect_resize(move |_area, _w, _h| {
        if let Some(sb) = blur_for_resize.borrow().as_ref() {
            send_frame_blur(sb, &window_for_resize, &monitor_for_resize);
        }
    });

    // Reactively hide the frame whenever this monitor's active workspace
    // has an edge-spanning window — fullscreen, maximize-to-edges, or a
    // floating window stretched to the output's width. `Layer::Overlay`
    // sits above niri's apps by spec, so without this toggle the frame
    // would paint over those windows.
    let visible = niri::edge_window_on(connector, mon_w).map(|edge| !edge);
    bind_visible(visible, &window);

    // Redraw the frame's cutout each animation frame while the sidebar's
    // revealer is in transition, so the cutout's left edge stays in sync
    // with the slide — and re-send the border blur region each tick so the
    // left strip's frost tracks the cutout's moving left edge. Stop ticking
    // once the revealer settles.
    let area_for_sidebar = area.clone();
    let monitor_for_sidebar = monitor.clone();
    let window_for_sidebar = window.clone();
    let blur_for_sidebar = blur.clone();
    glib::MainContext::default().spawn_local(
        crate::overlays::sidebar::open_signal(monitor).for_each(move |_open| {
            let area = area_for_sidebar.clone();
            let monitor = monitor_for_sidebar.clone();
            let window = window_for_sidebar.clone();
            let blur = blur_for_sidebar.clone();
            area.add_tick_callback(move |a, _clock| {
                a.queue_draw();
                if let Some(sb) = blur.borrow().as_ref() {
                    send_frame_blur(sb, &window, &monitor);
                }
                if crate::overlays::sidebar::is_settled(&monitor) {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            async {}
        }),
    );

    window.set_visible(true);
}

/// Set an empty input region on the window's surface so every pointer
/// event falls through to the layer below. Layer-shell does not give
/// us this directly; we go through the underlying `GdkSurface` once
/// it's realized.
fn install_click_through(window: &gtk::Window) {
    use hytte::gtk::cairo;

    window.connect_realize(|w| {
        if let Some(surface) = w.surface() {
            // An empty cairo region == no pointer area == fully click-through.
            let empty = cairo::Region::create();
            surface.set_input_region(Some(&empty));
        } else {
            tracing::warn!("frame: window has no surface at realize");
        }
    });
}

/// Attach the [`SurfaceBlur`] once the layer surface is mapped (its `wl_surface`
/// exists) and seed the border region. Mirrors the sidebar's `wire_blur_attach`:
/// the frame surface is persistent (mapped once at install) but is *hidden* (not
/// destroyed) on a fullscreen window, so on a remap the previous handle is bound
/// to a now-dead `wl_surface`. We therefore **drop the stale handle on each map,
/// then re-attach + re-seed** rather than guarding with `is_some` (which would
/// keep the dead handle forever). Idempotent on a single map.
fn wire_blur_attach(
    window: &gtk::Window,
    blur: &Rc<RefCell<Option<SurfaceBlur>>>,
    monitor: &Monitor,
) {
    let blur = blur.clone();
    let monitor = monitor.clone();
    window.connect_map(move |w| {
        // Defer to idle so the wl_surface is fully realized before we bind.
        let w = w.clone();
        let blur = blur.clone();
        let monitor = monitor.clone();
        glib::idle_add_local_once(move || {
            // Drop any handle bound to a previous (now-destroyed) surface so a
            // remap rebinds against the live wl_surface.
            blur.borrow_mut().take();
            if let Some(sb) = hytte::blur::attach(&w) {
                send_frame_blur(&sb, &w, &monitor);
                *blur.borrow_mut() = Some(sb);
                tracing::debug!("frame: attached client blur-region scope (border strips)");
            } else {
                tracing::debug!(
                    "frame: client blur-region unavailable (niri < 26.04?); border not frosted"
                );
            }
        });
    });
}

/// Send the current border blur region to niri: read the surface size, take the
/// sidebar's current visible width as the cutout's left edge, and frost the
/// three border strips ([`hytte::blur::frame_border_rects`]). No-op-friendly —
/// callers only invoke it when `blur` is `Some` (niri >= 26.04).
fn send_frame_blur(sb: &SurfaceBlur, window: &gtk::Window, monitor: &Monitor) {
    let (w, h) = surface_size(window);
    let left_inset = sidebar::current_visible_width(monitor);
    // BAR_HEIGHT / FRAME_THICKNESS are integral f64 design constants (44.0 / 8.0);
    // round to the i32 the region rects use. No raw `as` (pedantic gate).
    let bar_height = cast::f64_to_i32_round(BAR_HEIGHT);
    let thickness = cast::f64_to_i32_round(FRAME_THICKNESS);
    let rects = hytte::blur::frame_border_rects(w, h, left_inset, bar_height, thickness);
    sb.set_region_rects(&rects);
}

/// The frame surface's size in logical px (full output). Falls back to the
/// window's allocated size before the surface reports one.
fn surface_size(window: &gtk::Window) -> (i32, i32) {
    window.surface().map_or_else(
        || (window.width(), window.height()),
        |s| (s.width(), s.height()),
    )
}

fn install_draw(area: &gtk::DrawingArea, monitor: Monitor) {
    use hytte::gtk::cairo;

    let monitor_for_draw = monitor;
    area.set_draw_func(move |_area, cr: &cairo::Context, width: i32, height: i32| {
        let w = f64::from(width);
        let h = f64::from(height);

        // Skip if the area is too small to contain the bar + bottom inset.
        if h <= BAR_HEIGHT + FRAME_THICKNESS || w <= 2.0 * FRAME_THICKNESS {
            return;
        }

        // Sidebar's current visible width drives the cutout's left edge.
        // When closed, this is FRAME_THICKNESS (8) — same as before.
        let left_inset = f64::from(crate::overlays::sidebar::current_visible_width(
            &monitor_for_draw,
        ));

        let (cx, cy, cw, ch) = cutout_rect(w, h, left_inset);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }

        // Build a path with two sub-paths: the outer "frame region" rect
        // (everything below the bar), and the rounded cutout. Fill with
        // EvenOdd so the cutout is excluded.
        cr.set_fill_rule(cairo::FillRule::EvenOdd);

        // Outer region: from (left_inset, BAR_HEIGHT) to (w, h). Starting at
        // left_inset (instead of 0) means the frame's cairo paint never enters
        // the sidebar's region — the sidebar's surface (Layer::Top, below this
        // Layer::Overlay frame) shows through naturally. When the sidebar is
        // closed, left_inset = FRAME_THICKNESS (8) and this is identical to the
        // previous "from 0" rect minus the now-empty L-strut. Bar area above is
        // left untouched (transparent), so the bar paints its own gradient.
        // When the sidebar is open (left_inset > FRAME_THICKNESS), start the
        // outer paint rect at left_inset so the frame's gradient never enters
        // the sidebar's region — letting the sidebar surface (Layer::Top, below
        // this Layer::Overlay frame) show through. When closed (left_inset ==
        // FRAME_THICKNESS), start at 0 so the standard 8px L-strut paints
        // normally — same visual as before the sidebar feature.
        let outer_left = if left_inset > FRAME_THICKNESS {
            left_inset
        } else {
            0.0
        };
        cr.rectangle(outer_left, BAR_HEIGHT, w - outer_left, h - BAR_HEIGHT);

        // Inner cutout: rounded rect at (cx, cy) of size (cw, ch).
        rounded_rect(cr, cx, cy, cw, ch, CUTOUT_RADIUS);

        // Source: translucent dark purple matching `@shell_background` in
        // style.css — `rgba(28, 6, 44, 0.90)`. The bar also uses 0.90 alpha,
        // so the bar↔frame boundary has no opacity seam. Cairo can't read CSS
        // vars, so keep this RGB + alpha in sync with @shell_background.
        cr.set_source_rgba(28.0 / 255.0, 6.0 / 255.0, 44.0 / 255.0, 0.90);
        if let Err(e) = cr.fill() {
            tracing::warn!(error = %e, "frame: cairo fill failed");
        }
    });
}

/// Trace a closed rounded-rectangle sub-path of size (`rw`, `rh`) at (`rx`, `ry`)
/// with corner radius `radius`, on the given cairo context. Does not stroke or fill.
#[allow(clippy::many_single_char_names)]
fn rounded_rect(cr: &gtk::cairo::Context, rx: f64, ry: f64, rw: f64, rh: f64, radius: f64) {
    use std::f64::consts::PI;
    let r = radius.min(rw / 2.0).min(rh / 2.0);
    cr.new_sub_path();
    cr.arc(rx + rw - r, ry + r, r, -PI / 2.0, 0.0); // top-right
    cr.arc(rx + rw - r, ry + rh - r, r, 0.0, PI / 2.0); // bottom-right
    cr.arc(rx + r, ry + rh - r, r, PI / 2.0, PI); // bottom-left
    cr.arc(rx + r, ry + r, r, PI, 1.5 * PI); // top-left
    cr.close_path();
}

/// Cutout bounds for a monitor of size (`width`, `height`), with the cutout's
/// left edge starting at `left_inset` px from the screen's left edge. Pass
/// `FRAME_THICKNESS` for the default frame-only inset; pass the sidebar's
/// current visible width when the sidebar is open. Returns `(x, y, w, h)` of
/// the cutout's bounding box (corner radius applied at draw time).
fn cutout_rect(width: f64, height: f64, left_inset: f64) -> (f64, f64, f64, f64) {
    let x = left_inset;
    let y = BAR_HEIGHT;
    let w = (width - left_inset - FRAME_THICKNESS).max(0.0);
    let h = (height - BAR_HEIGHT - FRAME_THICKNESS).max(0.0);
    (x, y, w, h)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn cutout_rect_normal_monitor() {
        // 1920x1080: bar 44 (top) + bottom inset N + L/R inset N each.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, FRAME_THICKNESS);
        assert_eq!(x, FRAME_THICKNESS);
        assert_eq!(y, BAR_HEIGHT);
        assert_eq!(w, 1920.0 - 2.0 * FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - BAR_HEIGHT - FRAME_THICKNESS);
    }

    #[test]
    fn cutout_rect_tiny_monitor_clamps_to_zero() {
        // Pathological tiny monitor: cutout would be negative; clamp to 0
        // to avoid passing negative dimensions into cairo. Use sub-frame
        // dimensions so the clamp engages regardless of FRAME_THICKNESS.
        let (_x, _y, w, h) = cutout_rect(FRAME_THICKNESS - 1.0, BAR_HEIGHT - 1.0, FRAME_THICKNESS);
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn cutout_rect_with_sidebar_open() {
        // Sidebar fully open at SIDEBAR_WIDTH (320) means the cutout's left
        // edge starts at x = 320 instead of the default FRAME_THICKNESS.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, 320.0);
        assert_eq!(x, 320.0);
        assert_eq!(y, BAR_HEIGHT);
        assert_eq!(w, 1920.0 - 320.0 - FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - BAR_HEIGHT - FRAME_THICKNESS);
    }
}
