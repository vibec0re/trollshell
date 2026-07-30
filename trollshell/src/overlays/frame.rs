//! Per-monitor OVERLAY-layer window that paints the dark frame around
//! the workspace cutout. Full-screen, click-through, no exclusive zone.
//!
//! Layered above the bar (which is on `Layer::Top`). Bar widgets remain
//! interactive because the frame's input region is empty — every click
//! falls through to the layer below.
//!
//! The frame paints an opaque dark purple (`rgb(28, 6, 44)`, matching
//! `@shell_background` in `style.css`) into the L/R/bottom border regions and
//! carves four rounded inner corners around the workspace cutout. Top inset is
//! the bar's exclusive zone. Using the same opaque fill as the bar means the
//! bar↔frame boundary is seamless. (The frosted-glass/translucent variant is
//! parked on the `experiment/frosted-glass-blur` branch.)
//!
//! The top inset is read live from the bar's own `gtk::Window` at draw time
//! (mirroring `modal::BarGeometry::thickness()`'s "read live, not once"
//! convention) rather than a hardcoded constant — the bar's padding is
//! em-based, so a hardcoded height goes stale the moment text-scale or the
//! configurable bar font-size (#135) pushes it past the 1x baseline (#441).
//!
//! `FRAME_THICKNESS`/`CUTOUT_RADIUS` remain static — match `etc/niri/frame.kdl`
//! struts. If either changes, update both sides.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;
use hytte::ui::{LayerShell, layer_window};

/// A mounted frame overlay for one output.
struct FrameView {
    /// The layer-shell frame window. Closed in [`close_all`]. Its
    /// `bind_visible` apply-loop holds only a `WeakRef` (the #224/#243 fix),
    /// so it frees itself on the next `edge_window_on` emission once the
    /// window drops — no explicit abort needed for it.
    window: gtk::Window,
    /// The sidebar `open_signal` tick loop (spawned raw below — *not* a
    /// `bind`, so no `WeakRef` safety net). Its `JoinHandle` is stored so
    /// [`close_all`] can `.abort()` it on hot-plug; otherwise it would keep
    /// firing (and pinning its `DrawingArea` + `Monitor` clones) against a
    /// torn-down surface, leaking one subscription per rebuild. Mirrors
    /// `sidebar.rs`'s stored-`JoinHandle` teardown.
    sidebar_sub: glib::JoinHandle<()>,
}

thread_local! {
    /// Mounted frame overlays keyed by `Monitor.connector()`. Each entry owns
    /// its layer-shell window and the sidebar tick-loop subscription handle.
    static FRAMES: RefCell<HashMap<String, FrameView>> = RefCell::new(HashMap::new());
}

/// Fallback top inset used only for the brief window before the bar's
/// `gtk::Window` has completed its first layout pass — `gtk_widget_get_height`
/// returns 0 pre-allocation, and 0 would collapse the cutout onto the bar.
/// Once the bar is allocated, [`bar_height`] reads its real height instead.
///
/// Scaled (rather than a flat literal) so the fallback is still a reasonable
/// approximation if the effective font is already larger than the 1x
/// baseline at that point: matches the previous hardcoded `BAR_HEIGHT`
/// (`padding: 6px 12px` (12 vertical) + `min-height: 32px` = 44) at 1x.
const FALLBACK_BAR_HEIGHT: i32 = 44;

/// Frame thickness on left, right, and bottom, in CSS px. Must match the
/// niri `struts` values in `etc/niri/frame.kdl`. `pub(crate)` so
/// `overlays::sidebar`'s integer layout math derives from this single
/// source instead of hand-duplicating the literal (former
/// `FRAME_THICKNESS_I32` + a "keep in sync" comment).
pub(crate) const FRAME_THICKNESS_I32: i32 = 8;

/// [`FRAME_THICKNESS_I32`] as `f64`, for this module's cairo draw math.
/// Lossless: `i32` fits exactly in `f64`'s 53-bit mantissa. `f64::from`
/// would document that better than `as`, but it isn't const-stable yet
/// (`From` isn't a const trait on this MSRV), so a const context needs the
/// cast — hence the explicit allow.
#[allow(clippy::cast_lossless)]
const FRAME_THICKNESS: f64 = FRAME_THICKNESS_I32 as f64;

/// Corner radius for all four corners of the workspace cutout.
const CUTOUT_RADIUS: f64 = 10.0;

/// Mount one frame overlay on `monitor`. `bar` is the bar built for this
/// monitor (built just before this in `main.rs`'s per-monitor loop); its
/// window is read live for the frame's top inset (#441) — see [`bar_height`].
pub fn install(monitor: &Monitor, bar: &BarHandle) {
    let connector = match monitor.connector() {
        Some(c) if !c.is_empty() => c,
        _ => {
            tracing::debug!("frame::install: monitor has no connector; skipping");
            return;
        }
    };
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

    install_draw(&area, monitor.clone(), bar.window().clone());

    // Empty input region: clicks pass through to the bar (Layer::Top below)
    // and to niri's apps (normal layer below that). Set after realize so
    // the surface exists.
    install_click_through(&window);

    // Reactively hide the frame whenever this monitor's active workspace
    // has an edge-spanning window — fullscreen, maximize-to-edges, or a
    // floating window stretched to the output's width. `Layer::Overlay`
    // sits above niri's apps by spec, so without this toggle the frame
    // would paint over those windows.
    //
    // The width fed to the edge-span detection is a *live* signal, not a
    // snapshot: a resolution/mode switch (kanshi profile change) resizes this
    // output without a monitor hot-plug, so a captured width would leave the
    // threshold stale until the next real connect/disconnect (#442). The frame's
    // own cairo cutout already tracks the new size (the anchored layer surface
    // re-allocates and re-runs the draw func on resize); this keeps the
    // edge-span threshold in sync too.
    let mon_w = monitor.size_changed().map(|(w, _)| f64::from(w));
    let visible = niri::edge_window_on(connector.clone(), mon_w).map(|edge| !edge);
    bind_visible(visible, &window);

    // Redraw the frame's cutout each animation frame while the sidebar's
    // revealer is in transition, so the cutout's left edge stays in sync
    // with the slide. Stop ticking once the revealer settles.
    //
    // Spawned raw (not a `bind`), so it has no WeakRef safety net and won't
    // stop when the window drops — the `JoinHandle` is stored in `FrameView`
    // and aborted in `close_all` on hot-plug.
    let area_for_sidebar = area.clone();
    let monitor_for_sidebar = monitor.clone();
    let sidebar_sub = glib::MainContext::default().spawn_local(
        crate::overlays::sidebar::open_signal(monitor).for_each(move |_open| {
            let area = area_for_sidebar.clone();
            let monitor = monitor_for_sidebar.clone();
            area.add_tick_callback(move |a, _clock| {
                a.queue_draw();
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

    // Register the surface + its raw subscription so `close_all` can tear
    // both down on the next monitor hot-plug (re-keys cleanly by connector).
    FRAMES.with(|map| {
        map.borrow_mut().insert(
            connector,
            FrameView {
                window,
                sidebar_sub,
            },
        );
    });
}

/// Close every frame overlay and abort its sidebar tick-loop subscription,
/// dropping the per-monitor entries. Called before rebuilding on hot-plug so
/// a vanished output's frame window + raw subscription don't linger (the
/// subscription has no `WeakRef` safety net, so it must be aborted explicitly —
/// mirrors `sidebar::close_all`).
///
/// Tears down with `destroy()`, not `close()` (#632): a frame overlay that
/// never showed a border on this monitor is still unrealized, and `close()`
/// neither destroys an unrealized window nor drops GTK's internal toplevel
/// reference — only `destroy()` does, and it can't be vetoed by a
/// `close-request` handler.
pub fn close_all() {
    FRAMES.with(|map| {
        // `take()` moves the whole map out (leaving `Default`) and releases
        // the borrow inside the call, rather than holding a `drain()` RefMut
        // across every `destroy()` below (#631) — a borrow held across a GTK
        // call is a latent reentrancy hazard if any emission it triggers is
        // ever synchronous.
        for (_, view) in map.take() {
            // Abort the raw tick-loop first so it can't queue another draw
            // into the surface we're about to destroy, then destroy the
            // window. The `bind_visible` apply-loop rides on the #224/#243
            // WeakRef fix: it frees itself on its next emission once the
            // window drops.
            view.sidebar_sub.abort();
            view.window.destroy();
        }
    });
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

fn install_draw(area: &gtk::DrawingArea, monitor: Monitor, bar_window: gtk::Window) {
    use hytte::gtk::cairo;

    let monitor_for_draw = monitor;
    area.set_draw_func(move |_area, cr: &cairo::Context, width: i32, height: i32| {
        let w = f64::from(width);
        let h = f64::from(height);
        let bar_h = bar_height(&bar_window);

        // Skip if the area is too small to contain the bar + bottom inset.
        if h <= bar_h + FRAME_THICKNESS || w <= 2.0 * FRAME_THICKNESS {
            return;
        }

        // Sidebar's current visible width drives the cutout's left edge.
        // When closed, this is FRAME_THICKNESS (8) — same as before.
        let left_inset = f64::from(crate::overlays::sidebar::current_visible_width(
            &monitor_for_draw,
        ));

        let (cx, cy, cw, ch) = cutout_rect(w, h, left_inset, bar_h);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }

        // Build a path with two sub-paths: the outer "frame region" rect
        // (everything below the bar), and the rounded cutout. Fill with
        // EvenOdd so the cutout is excluded.
        cr.set_fill_rule(cairo::FillRule::EvenOdd);

        // Outer region: from (left_inset, bar_h) to (w, h). Starting at
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
        cr.rectangle(outer_left, bar_h, w - outer_left, h - bar_h);

        // Inner cutout: rounded rect at (cx, cy) of size (cw, ch).
        rounded_rect(cr, cx, cy, cw, ch, CUTOUT_RADIUS);

        // Source: opaque dark purple matching `@shell_background` in
        // style.css — `rgb(28, 6, 44)`. The bar uses the same opaque fill, so
        // the bar↔frame boundary has no seam. Cairo can't read CSS vars, so keep
        // this RGB in sync with @shell_background (alpha 1.0 = opaque shell).
        cr.set_source_rgba(28.0 / 255.0, 6.0 / 255.0, 44.0 / 255.0, 1.0);
        if let Err(e) = cr.fill() {
            tracing::warn!(error = %e, "frame: cairo fill failed");
        }
    });
}

/// The frame's top inset: the bar's real, live allocated height in logical
/// pixels — read fresh from `bar_window` every call, the same "read live, not
/// once" convention `modal::BarGeometry::thickness()` uses for the drawer's
/// perpendicular margin. Replaces the old hardcoded `BAR_HEIGHT` (44), which
/// went stale the moment the bar's em-based padding grew past the 1x baseline
/// (e.g. a larger configurable bar font-size (#135) or GNOME text-scaling)
/// (#441).
///
/// Falls back to [`FALLBACK_BAR_HEIGHT`] (scaled) for the brief window before
/// the bar's `gtk::Window` has completed its first layout pass, where
/// `gtk_widget_get_height` still reports 0.
fn bar_height(bar_window: &gtk::Window) -> f64 {
    let allocated = bar_window.height();
    if allocated > 0 {
        f64::from(allocated)
    } else {
        f64::from(crate::scale::scale(FALLBACK_BAR_HEIGHT))
    }
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
/// left edge starting at `left_inset` px from the screen's left edge and its
/// top edge starting at `bar_h` px (the bar's live height — see
/// [`bar_height`]). Pass `FRAME_THICKNESS` for the default frame-only inset;
/// pass the sidebar's current visible width when the sidebar is open. Returns
/// `(x, y, w, h)` of the cutout's bounding box (corner radius applied at draw
/// time).
fn cutout_rect(width: f64, height: f64, left_inset: f64, bar_h: f64) -> (f64, f64, f64, f64) {
    let x = left_inset;
    let y = bar_h;
    let w = (width - left_inset - FRAME_THICKNESS).max(0.0);
    let h = (height - bar_h - FRAME_THICKNESS).max(0.0);
    (x, y, w, h)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    /// Bar height matching the old hardcoded `BAR_HEIGHT`, for tests that
    /// don't care about scaling — i.e. the 1x-baseline case.
    const BASELINE_BAR_HEIGHT: f64 = 44.0;

    #[test]
    fn cutout_rect_normal_monitor() {
        // 1920x1080: bar 44 (top) + bottom inset N + L/R inset N each.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, FRAME_THICKNESS, BASELINE_BAR_HEIGHT);
        assert_eq!(x, FRAME_THICKNESS);
        assert_eq!(y, BASELINE_BAR_HEIGHT);
        assert_eq!(w, 1920.0 - 2.0 * FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS);
    }

    #[test]
    fn cutout_rect_tiny_monitor_clamps_to_zero() {
        // Pathological tiny monitor: cutout would be negative; clamp to 0
        // to avoid passing negative dimensions into cairo. Use sub-frame
        // dimensions so the clamp engages regardless of FRAME_THICKNESS.
        let (_x, _y, w, h) = cutout_rect(
            FRAME_THICKNESS - 1.0,
            BASELINE_BAR_HEIGHT - 1.0,
            FRAME_THICKNESS,
            BASELINE_BAR_HEIGHT,
        );
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn cutout_rect_with_sidebar_open() {
        // Sidebar fully open at SIDEBAR_WIDTH (320) means the cutout's left
        // edge starts at x = 320 instead of the default FRAME_THICKNESS.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, 320.0, BASELINE_BAR_HEIGHT);
        assert_eq!(x, 320.0);
        assert_eq!(y, BASELINE_BAR_HEIGHT);
        assert_eq!(w, 1920.0 - 320.0 - FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS);
    }

    #[test]
    fn cutout_rect_taller_bar_shifts_cutout_down() {
        // #441: a scaled-up bar (e.g. a larger configurable bar font-size or
        // GNOME text-scaling growing the bar's em-based padding past the 1x
        // baseline) must push the cutout's top edge down by the *real* bar
        // height, not a stale 44 — otherwise the frame's cutout starts under
        // the bar's actual bottom edge (paints over it) or leaves a seam
        // above it.
        let taller_bar = 64.0;
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, FRAME_THICKNESS, taller_bar);
        assert_eq!(x, FRAME_THICKNESS);
        assert_eq!(y, taller_bar);
        assert_eq!(w, 1920.0 - 2.0 * FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - taller_bar - FRAME_THICKNESS);
        // And it must differ from the stale baseline-height cutout — the
        // whole point of deriving it live.
        let (_x2, y2, _w2, h2) = cutout_rect(1920.0, 1080.0, FRAME_THICKNESS, BASELINE_BAR_HEIGHT);
        assert_ne!(y, y2);
        assert_ne!(h, h2);
    }
}
