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
//!
//! ## The two sidebars (#1247)
//!
//! Both horizontal insets follow a sidebar: the left edge the left sidebar's
//! visible width, the right edge the right one's (#1158/#1160). Each side is
//! read from its own surface through `sidebar::current_visible_width(side, …)`
//! and both are redrawn from **one** tick loop, armed by a `map_ref!` over the
//! two open signals and broken only when [`all_settled`] says neither revealer
//! is still moving. A side that is closed, empty or never installed reports
//! [`FRAME_THICKNESS_I32`], which is the plain strut — so a shell with no
//! right-mounted plugin draws exactly the frame it drew before this existed.

use std::cell::RefCell;
use std::collections::HashMap;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;
use hytte::ui::{LayerShell, layer_window};

use super::sidebar::{self, Side};

/// A mounted frame overlay for one output.
struct FrameView {
    /// The layer-shell frame window. Closed in [`close_all`]. Its
    /// `bind_visible` apply-loop holds only a `WeakRef` (the #224/#243 fix),
    /// so it frees itself on the next `edge_window_on` emission once the
    /// window drops — no explicit abort needed for it.
    window: gtk::Window,
    /// The sidebar `open_signal_on` tick loop — **both** sides since #1247,
    /// folded into one subscription rather than two (spawned raw below — *not*
    /// a `bind`, so no `WeakRef` safety net). Its `JoinHandle` is stored so
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
    // No `Some(c) if !c.is_empty()` guard needed any more (#1177):
    // `Monitor::connector()` itself folds `Some("")` to `None` since #1180
    // item 6, so this `let-else` already can't see the empty string.
    let Some(connector) = monitor.connector() else {
        tracing::debug!("frame::install: monitor has no connector; skipping");
        return;
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

    // Redraw the frame's cutout each animation frame while *either* sidebar's
    // revealer is in transition, so the cutout's left and right edges stay in
    // sync with the slide. Stop ticking once both revealers have settled.
    //
    // One subscription over both sides (`map_ref!`), not two (#1247): the tick
    // callback it arms already redraws the whole cutout, so a second
    // subscription would only ever arm a redundant twin of the tick the first
    // one armed — and would need its own `JoinHandle` in `FrameView` and its own
    // `abort()` in `close_all` to avoid the leak documented on that field.
    //
    // Spawned raw (not a `bind`), so it has no WeakRef safety net and won't
    // stop when the window drops — the `JoinHandle` is stored in `FrameView`
    // and aborted in `close_all` on hot-plug.
    let area_for_sidebar = area.clone();
    let monitor_for_sidebar = monitor.clone();
    let both_sides = map_ref! {
        let left = sidebar::open_signal_on(Side::Left, monitor),
        let right = sidebar::open_signal_on(Side::Right, monitor) => (*left, *right)
    };
    let sidebar_sub =
        glib::MainContext::default().spawn_local(both_sides.for_each(move |_opens| {
            let area = area_for_sidebar.clone();
            let monitor = monitor_for_sidebar.clone();
            area.add_tick_callback(move |a, _clock| {
                // `a`, the closure's own parameter — never a captured clone of
                // the area this is connected to (#224/#831).
                a.queue_draw();
                if all_settled(
                    sidebar::is_settled(Side::Left, &monitor),
                    sidebar::is_settled(Side::Right, &monitor),
                ) {
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            async {}
        }));

    window.set_visible(true);

    // Register the surface + its raw subscription so `close_all` can tear
    // both down on the next monitor hot-plug (re-keys cleanly by connector).
    // Tail-expression `insert` + an outer `drop`, not a bare `insert(…);`
    // statement (#643): the displaced `FrameView` would otherwise be a
    // temporary of the same statement as the `borrow_mut()` `RefMut`, and
    // statement temporaries drop in reverse creation order — so it would run
    // its drop glue (a `JoinHandle` plus a `gtk::Window` unref) with `FRAMES`
    // still borrowed. A refcount decrement, not a widget teardown: disposing
    // the window needs `destroy()`, which is `close_all`'s job. Same shape and
    // same weak reachability as the annotated `modal::install` site.
    drop(FRAMES.with(|map| {
        map.borrow_mut().insert(
            connector,
            FrameView {
                window,
                sidebar_sub,
            },
        )
    }));
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

        // Each sidebar's current visible width drives the cutout edge on *its*
        // side (#1247). When a side is closed (or, on the right, empty or never
        // installed) its inset is FRAME_THICKNESS (8) — the plain strut, same as
        // before the sidebars existed.
        let (left_inset, right_inset) = sidebar_insets(|side| {
            f64::from(sidebar::current_visible_width(side, &monitor_for_draw))
        });

        let (cx, cy, cw, ch) = cutout_rect(w, h, left_inset, right_inset, bar_h);
        if cw <= 0.0 || ch <= 0.0 {
            return;
        }

        // Build a path with two sub-paths: the outer "frame region" rect
        // (everything below the bar), and the rounded cutout. Fill with
        // EvenOdd so the cutout is excluded.
        cr.set_fill_rule(cairo::FillRule::EvenOdd);

        // Outer region: the band below the bar, clipped out of whichever
        // sidebar regions are currently showing (see [`outer_span`]). The bar
        // area above is left untouched (transparent), so the bar paints its own
        // gradient.
        let (outer_x, outer_w) = outer_span(w, left_inset, right_inset);
        cr.rectangle(outer_x, bar_h, outer_w, h - bar_h);

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

/// The frame's two horizontal insets, each read from **its own** side's sidebar
/// (#1247).
///
/// Split out — taking the per-side width as a closure rather than reading the
/// accessor itself — for one reason: this pairing is the entire bug. Before
/// #1247 both the cutout's right edge and the paint rect's right edge were
/// pinned at `width - FRAME_THICKNESS` while only the left followed a surface,
/// so the 8 px right strut painted *over* an open right sidebar and the tiles
/// niri had just reflowed got no border at all. A geometry test cannot see that
/// — feed `cutout_rect` the wrong number and it dutifully draws the wrong
/// rectangle — so the side↔source mapping is asserted here instead, where
/// swapping either entry for the other side is red.
fn sidebar_insets(width_of: impl Fn(Side) -> f64) -> (f64, f64) {
    (width_of(Side::Left), width_of(Side::Right))
}

/// Whether the frame's redraw tick can stop: **both** sidebars are at rest.
///
/// A conjunction rather than either side's own flag, because one tick callback
/// serves both edges (see `install`'s single `map_ref!` subscription): breaking
/// as soon as the left has settled would freeze the cutout mid-way through a
/// right slide.
fn all_settled(left: bool, right: bool) -> bool {
    left && right
}

/// Horizontal span of the frame's opaque paint rect: `(x, width)`.
///
/// Each end is pulled in to its sidebar's inset **only while that sidebar is
/// showing something** (inset strictly greater than [`FRAME_THICKNESS`]), so the
/// frame's cairo paint never enters that surface's region — the sidebar
/// (`Layer::Top`, below this `Layer::Overlay` frame) shows through naturally.
/// With a side closed its end stays at the screen edge, so the standard 8 px
/// strut paints normally: with both closed this is `(0, width)`, pixel-identical
/// to the pre-sidebar frame.
///
/// Clamped at 0 for the pathological case where two open sidebars are together
/// wider than the output — cairo takes a negative width, but nothing good
/// follows it.
fn outer_span(width: f64, left_inset: f64, right_inset: f64) -> (f64, f64) {
    let left = if left_inset > FRAME_THICKNESS {
        left_inset
    } else {
        0.0
    };
    let right = if right_inset > FRAME_THICKNESS {
        width - right_inset
    } else {
        width
    };
    (left, (right - left).max(0.0))
}

/// Cutout bounds for a monitor of size (`width`, `height`), with the cutout's
/// left edge starting at `left_inset` px from the screen's left edge, its right
/// edge `right_inset` px in from the screen's right edge, and its top edge
/// starting at `bar_h` px (the bar's live height — see [`bar_height`]). Pass
/// `FRAME_THICKNESS` on a side for the default frame-only inset; pass that
/// side's sidebar visible width while it is showing. Returns `(x, y, w, h)` of
/// the cutout's bounding box (corner radius applied at draw time).
fn cutout_rect(
    width: f64,
    height: f64,
    left_inset: f64,
    right_inset: f64,
    bar_h: f64,
) -> (f64, f64, f64, f64) {
    let x = left_inset;
    let y = bar_h;
    let w = (width - left_inset - right_inset).max(0.0);
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

    /// Both sides closed — the inset a sidebar reports when it is shut, empty,
    /// or never installed. The pre-sidebar geometry.
    const SHUT: f64 = FRAME_THICKNESS;

    /// A sidebar's open width, standing for `SIDEBAR_WIDTH` at the 1x baseline.
    const OPEN_W: f64 = 320.0;

    /// A **different** open width for the other side, so every "which side did
    /// this come from" assertion below has a distinguishable answer.
    const OPEN_W_2: f64 = 448.0;

    #[test]
    fn cutout_rect_normal_monitor() {
        // 1920x1080: bar 44 (top) + bottom inset N + L/R inset N each.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, SHUT, SHUT, BASELINE_BAR_HEIGHT);
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
            SHUT,
            SHUT,
            BASELINE_BAR_HEIGHT,
        );
        assert_eq!(w, 0.0);
        assert_eq!(h, 0.0);
    }

    #[test]
    fn cutout_rect_with_sidebar_open() {
        // Sidebar fully open at SIDEBAR_WIDTH (320) means the cutout's left
        // edge starts at x = 320 instead of the default FRAME_THICKNESS.
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, OPEN_W, SHUT, BASELINE_BAR_HEIGHT);
        assert_eq!(x, OPEN_W);
        assert_eq!(y, BASELINE_BAR_HEIGHT);
        assert_eq!(w, 1920.0 - OPEN_W - FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS);
    }

    // ── The right sidebar (#1247) ────────────────────────────────────────────

    /// The mirror of `cutout_rect_with_sidebar_open`: an open **right** sidebar
    /// pulls the cutout's right edge in by its width, so the tiles niri has
    /// reflowed to `width - 320` get a border there instead of the frame's 8 px
    /// strut painting on top of the sidebar.
    ///
    /// The left edge is untouched — an open right sidebar does not move it.
    ///
    /// **Falsification (run):** restoring `cutout_rect`'s
    /// `width - left_inset - FRAME_THICKNESS` (the pre-#1247 spelling, i.e.
    /// right-blind) turns the width assertion red.
    #[test]
    fn cutout_rect_with_the_right_sidebar_open() {
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, SHUT, OPEN_W, BASELINE_BAR_HEIGHT);
        assert_eq!(x, FRAME_THICKNESS, "the left edge stays at the plain strut");
        assert_eq!(y, BASELINE_BAR_HEIGHT);
        assert_eq!(w, 1920.0 - FRAME_THICKNESS - OPEN_W);
        assert_eq!(h, 1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS);
        // The cutout's right edge, i.e. where the frame's border is drawn.
        assert_eq!(x + w, 1920.0 - OPEN_W);
    }

    /// Both open at once — the tiles are squeezed between two sidebars and the
    /// cutout is inset on both sides by *different* amounts.
    ///
    /// The two widths differ on purpose: with one number for both, a cutout
    /// keyed to the wrong side is indistinguishable from a correct one.
    #[test]
    fn cutout_rect_with_both_sidebars_open() {
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, OPEN_W, OPEN_W_2, BASELINE_BAR_HEIGHT);
        assert_eq!(x, OPEN_W, "the left edge follows the LEFT sidebar");
        assert_eq!(y, BASELINE_BAR_HEIGHT);
        assert_eq!(
            x + w,
            1920.0 - OPEN_W_2,
            "and the right edge follows the RIGHT one"
        );
        assert_eq!(w, 1920.0 - OPEN_W - OPEN_W_2);
        assert_eq!(h, 1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS);
    }

    /// A closed/empty/never-installed right sidebar costs nothing: the cutout is
    /// **identical** to the pre-#1247 geometry, asserted against the left-only
    /// cases rather than re-derived, so the two can't drift.
    #[test]
    fn a_shut_right_sidebar_leaves_the_pre_1247_cutout_untouched() {
        assert_eq!(
            cutout_rect(1920.0, 1080.0, SHUT, SHUT, BASELINE_BAR_HEIGHT),
            (
                FRAME_THICKNESS,
                BASELINE_BAR_HEIGHT,
                1920.0 - 2.0 * FRAME_THICKNESS,
                1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS
            )
        );
        assert_eq!(
            cutout_rect(1920.0, 1080.0, OPEN_W, SHUT, BASELINE_BAR_HEIGHT),
            (
                OPEN_W,
                BASELINE_BAR_HEIGHT,
                1920.0 - OPEN_W - FRAME_THICKNESS,
                1080.0 - BASELINE_BAR_HEIGHT - FRAME_THICKNESS
            )
        );
    }

    /// Toggling the right sidebar never moves the cutout's **left** edge, at any
    /// left inset — the property the left half of the frame keeps across the
    /// whole of #1247.
    #[test]
    fn a_right_toggle_does_not_move_the_left_edge() {
        for left in [SHUT, OPEN_W, OPEN_W_2] {
            let shut = cutout_rect(1920.0, 1080.0, left, SHUT, BASELINE_BAR_HEIGHT);
            let open = cutout_rect(1920.0, 1080.0, left, OPEN_W, BASELINE_BAR_HEIGHT);
            assert_eq!(shut.0, left);
            assert_eq!(open.0, left);
            assert_ne!(shut.2, open.2, "…but the width does change");
        }
    }

    /// Two sidebars wider than the output leave no cutout rather than a negative
    /// one — the horizontal twin of `cutout_rect_tiny_monitor_clamps_to_zero`,
    /// reachable for the first time now that both ends move (a big font on a
    /// small output, #737's widening on both sides at once).
    #[test]
    fn two_oversized_sidebars_clamp_the_cutout_to_zero() {
        let (_x, _y, w, _h) = cutout_rect(600.0, 1080.0, 400.0, 400.0, BASELINE_BAR_HEIGHT);
        assert_eq!(w, 0.0);
    }

    /// #1247's own keying: the left inset comes from the **left** sidebar and
    /// the right inset from the **right** one.
    ///
    /// The geometry tests above cannot see this — hand `cutout_rect` the left
    /// width twice and it draws a perfectly consistent, perfectly wrong
    /// rectangle. This is the assertion that fails for it.
    ///
    /// **Falsification (run):** making `sidebar_insets` read
    /// `(width_of(Side::Left), width_of(Side::Left))` — i.e. keying the right
    /// inset to the left sidebar's width, the exact mutation #1247 asks for —
    /// turns this red.
    #[test]
    fn each_inset_is_read_from_its_own_side() {
        let (left, right) = sidebar_insets(|side| match side {
            Side::Left => OPEN_W,
            Side::Right => OPEN_W_2,
        });
        assert_eq!(left, OPEN_W, "the left inset must come from Side::Left");
        assert_eq!(
            right, OPEN_W_2,
            "the right inset must come from Side::Right"
        );
    }

    // ── The paint rect (`outer_span`) ────────────────────────────────────────

    /// Both sidebars shut: the opaque band spans the whole output, so the plain
    /// 8 px L/R struts paint exactly as they did before either sidebar existed.
    #[test]
    fn outer_span_with_both_sidebars_shut_spans_the_output() {
        assert_eq!(outer_span(1920.0, SHUT, SHUT), (0.0, 1920.0));
    }

    /// An open sidebar pulls its end of the band in to the sidebar's own edge,
    /// so the frame's cairo paint never enters that surface's region.
    #[test]
    fn outer_span_stops_at_each_open_sidebars_edge() {
        assert_eq!(
            outer_span(1920.0, OPEN_W, SHUT),
            (OPEN_W, 1920.0 - OPEN_W),
            "left open"
        );
        // The right-hand case is #1247: before it, the band ran to `width` and
        // the 8 px right strut painted on top of the sidebar.
        assert_eq!(
            outer_span(1920.0, SHUT, OPEN_W),
            (0.0, 1920.0 - OPEN_W),
            "right open"
        );
        assert_eq!(
            outer_span(1920.0, OPEN_W, OPEN_W_2),
            (OPEN_W, 1920.0 - OPEN_W - OPEN_W_2),
            "both open, and each end follows its own side"
        );
    }

    /// Overlapping sidebars produce an empty band, never a negative width — a
    /// negative `cairo_rectangle` width is a path going backwards, and with
    /// `FillRule::EvenOdd` that silently inverts which region gets filled.
    #[test]
    fn outer_span_clamps_rather_than_going_negative() {
        let (_x, w) = outer_span(600.0, 400.0, 400.0);
        assert_eq!(w, 0.0);
    }

    // ── The redraw tick (#1247) ──────────────────────────────────────────────

    /// One tick callback serves both cutout edges, so it may only stop when
    /// **both** revealers are at rest.
    ///
    /// **Falsification (run):** dropping either term from [`all_settled`] (the
    /// pre-#1247 spelling was the left one alone) turns the matching case red —
    /// on glass that is a cutout frozen part-way through the other side's slide
    /// until something else queues a draw.
    #[test]
    fn the_redraw_tick_stops_only_once_both_sides_have_settled() {
        assert!(all_settled(true, true));
        assert!(!all_settled(true, false), "the right is still sliding");
        assert!(!all_settled(false, true), "the left is still sliding");
        assert!(!all_settled(false, false));
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
        let (x, y, w, h) = cutout_rect(1920.0, 1080.0, SHUT, SHUT, taller_bar);
        assert_eq!(x, FRAME_THICKNESS);
        assert_eq!(y, taller_bar);
        assert_eq!(w, 1920.0 - 2.0 * FRAME_THICKNESS);
        assert_eq!(h, 1080.0 - taller_bar - FRAME_THICKNESS);
        // And it must differ from the stale baseline-height cutout — the
        // whole point of deriving it live.
        let (_x2, y2, _w2, h2) = cutout_rect(1920.0, 1080.0, SHUT, SHUT, BASELINE_BAR_HEIGHT);
        assert_ne!(y, y2);
        assert_ne!(h, h2);
    }
}
