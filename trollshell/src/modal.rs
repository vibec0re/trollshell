use std::cell::RefCell;
use std::collections::HashMap;

use hytte::adw;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, graphene, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar;
use hytte::services::clipboard;
use hytte::services::notifications;
use hytte::ui::{Anchor, Edge, Layer, LayerEdge, LayerShell, layer_window};

/// Drawer's max content width (`AdwClamp.maximum_size` in `components::layout::finish_page`).
/// Used to clamp the per-trigger margin so the card never falls off-screen left.
const DRAWER_MAX_WIDTH: i32 = 680;

/// Concave flare radius for the drawer's two top corners — where the card
/// sweeps *outward* to meet the bar so it "grows out of the bar" instead of
/// floating below it (#34). The card body is inset by this much from its top
/// (widest) extent, so the page content carries a matching left/right margin.
/// Tunable: bigger = more pronounced flare. Drawn by [`draw_drawer_silhouette`].
/// Kept close to the shell's other corner radii (frame cutout/panels ~10–14)
/// so the drawer junction matches the rest of the UI (#44).
const DRAWER_FLARE_RADIUS: i32 = 12;

/// Convex corner radius for the drawer's two bottom corners.
const DRAWER_CORNER_RADIUS: f64 = 14.0;

/// Chrome between the layer-shell surface edge and the visible `.ts-drawer`
/// card, on the *leading* side of the bar's main axis. Derived from
/// `trollshell/style.css`:
/// - Top/Bottom bars (horizontal axis): `.ts-modal` `padding-left: 20px`
///   plus `.ts-drawer` `margin-left: 0`.
/// - Left/Right bars (vertical axis): `.ts-drawer` `margin-top: 0`
///   (`.ts-modal` has no vertical padding).
///
/// The card centering math must account for this so the *card* — not the
/// transparent surface — lands centered under the trigger chip. Keep these
/// in sync with the stylesheet.
const CARD_CHROME_MAIN_START_HORIZONTAL: i32 = 20;
const CARD_CHROME_MAIN_START_VERTICAL: i32 = 0;

/// Chrome between the surface edge and the card on the *trailing* side of the
/// bar's main axis (the side the drawer hugs):
/// - Horizontal: `.ts-modal` `padding-right: 5px` + `.ts-drawer`
///   `margin-right: 10px` = 15.
/// - Vertical: `.ts-drawer` `margin-bottom: 20px` = 20.
const CARD_CHROME_MAIN_END_HORIZONTAL: i32 = 15;
const CARD_CHROME_MAIN_END_VERTICAL: i32 = 20;

/// Where the bar sits, so the drawer can anchor to the bar's *actual* edge
/// with a perpendicular margin derived from the bar's real offset + measured
/// thickness — instead of the old hardcoded `Anchor::Top` + `margin.top = 59`
/// (where 59 was a guessed top-bar height that breaks the moment the bar is
/// inset, moved to another edge, or sits under another exclusive surface).
///
/// `thickness` is read live from the (by-open-time mapped) bar window rather
/// than measured once, so CSS-driven height changes and hot-plug rebuilds
/// stay correct.
#[derive(Clone)]
struct BarGeometry {
    /// The bar's screen edge.
    edge: Edge,
    /// The bar's own margin on `edge` (gap between the screen edge and the
    /// bar). Usually 0 for a flush bar.
    offset: i32,
    /// Monitor size in logical pixels, captured at install. Used to clamp the
    /// card on-screen along the main axis.
    mon_w: i32,
    mon_h: i32,
    /// The bar's layer-shell window, measured at open time for its real
    /// thickness (height for Top/Bottom, width for Left/Right).
    bar_window: gtk::Window,
}

impl BarGeometry {
    /// Bar thickness along the axis perpendicular to the bar, read from the
    /// live (mapped) bar window.
    fn thickness(&self) -> i32 {
        match self.edge {
            Edge::Top | Edge::Bottom => self.bar_window.height(),
            Edge::Left | Edge::Right => self.bar_window.width(),
        }
    }

    /// Distance from the screen edge the drawer anchors to, out to the
    /// drawer's near edge: the bar's offset plus its measured thickness.
    /// Replaces the literal `59`.
    fn perpendicular_margin(&self) -> i32 {
        self.offset + self.thickness()
    }

    /// True when the bar runs horizontally (Top/Bottom) so the drawer
    /// positions along the X axis; false for vertical bars (Left/Right).
    fn horizontal(&self) -> bool {
        matches!(self.edge, Edge::Top | Edge::Bottom)
    }

    /// Chrome between the surface edge and the card on the side the drawer
    /// hugs (where its main-axis margin is anchored).
    fn chrome_main_end(&self) -> i32 {
        if self.horizontal() {
            CARD_CHROME_MAIN_END_HORIZONTAL
        } else {
            CARD_CHROME_MAIN_END_VERTICAL
        }
    }

    /// Chrome between the surface edge and the card on the opposite side.
    fn chrome_main_start(&self) -> i32 {
        if self.horizontal() {
            CARD_CHROME_MAIN_START_HORIZONTAL
        } else {
            CARD_CHROME_MAIN_START_VERTICAL
        }
    }

    /// The perpendicular anchor + main-axis anchor the drawer surface uses.
    /// The main-axis anchor is the *trailing* edge so the under-the-chip
    /// margin is uniformly measured from there: horizontal bars hug the right
    /// and slide along X; vertical bars hug the bottom and slide along Y.
    fn anchors(&self) -> (Anchor, Anchor) {
        match self.edge {
            Edge::Top => (Anchor::Top, Anchor::Right),
            Edge::Bottom => (Anchor::Bottom, Anchor::Right),
            Edge::Left => (Anchor::Left, Anchor::Bottom),
            Edge::Right => (Anchor::Right, Anchor::Bottom),
        }
    }

    /// Layer-shell edge carrying the perpendicular (bar-thickness) margin.
    fn perpendicular_layer_edge(&self) -> LayerEdge {
        match self.edge {
            Edge::Top => LayerEdge::Top,
            Edge::Bottom => LayerEdge::Bottom,
            Edge::Left => LayerEdge::Left,
            Edge::Right => LayerEdge::Right,
        }
    }

    /// Layer-shell edge carrying the main-axis (under-the-chip) margin — the
    /// trailing edge: horizontal bars hug the right, vertical bars the bottom.
    /// The margin is the distance from this edge back to the card.
    fn main_layer_edge(&self) -> LayerEdge {
        if self.horizontal() {
            LayerEdge::Right
        } else {
            LayerEdge::Bottom
        }
    }

    /// Revealer slide direction so the card pulls out of the bar's far edge.
    fn slide(&self) -> gtk::RevealerTransitionType {
        match self.edge {
            Edge::Top => gtk::RevealerTransitionType::SlideDown,
            Edge::Bottom => gtk::RevealerTransitionType::SlideUp,
            Edge::Left => gtk::RevealerTransitionType::SlideRight,
            Edge::Right => gtk::RevealerTransitionType::SlideLeft,
        }
    }

    /// Alignment that pins the card against the bar so it slides out of the
    /// bar's far edge rather than floating mid-surface.
    fn perpendicular_align(&self) -> gtk::Align {
        match self.edge {
            Edge::Top | Edge::Left => gtk::Align::Start,
            Edge::Bottom | Edge::Right => gtk::Align::End,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Page {
    Media,
    Network,
    Vpn,
    Connections,
    Bluetooth,
    Stats,
    Audio,
    Power,
    PowerMenu,
    Notifications,
    Appearance,
    Displays,
    Clipboard,
    Calendar,
    Settings,
}

impl Page {
    /// Pages whose content is backed by the `netconn` service (the active-
    /// connections list / counters). Used to gate netconn's always-on `ss`
    /// poller on whether one of these is actually visible (#50): the
    /// Connections drill-down page and the Network panel (which shows a
    /// netconn-derived "N sockets" subtitle + live group).
    fn uses_netconn(self) -> bool {
        matches!(self, Self::Connections | Self::Network)
    }

    /// Pages whose content is backed by the `app_usage` service (the
    /// most-expensive-apps top-N CPU/RAM lists). Used to gate `app_usage`'s
    /// always-on `/proc` poller on whether one of these is actually visible
    /// (#50, item 5 of #42): only the Stats panel reads those lists.
    fn uses_app_usage(self) -> bool {
        matches!(self, Self::Stats)
    }

    fn stack_name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Network => "network",
            Self::Vpn => "vpn",
            Self::Connections => "connections",
            Self::Bluetooth => "bluetooth",
            Self::Stats => "stats",
            Self::Audio => "audio",
            Self::Power => "power",
            Self::PowerMenu => "power-menu",
            Self::Notifications => "notifications",
            Self::Appearance => "appearance",
            Self::Displays => "displays",
            Self::Clipboard => "clipboard",
            Self::Calendar => "calendar",
            Self::Settings => "settings",
        }
    }
}

/// Per-monitor drawer handle. Internally owns the layer-shell window, a
/// `GtkRevealer` that slides the card out of the bar's far edge (direction
/// chosen per `BarGeometry`), and a persistent fullscreen click-catcher
/// that's toggled alongside the drawer.
struct ModalPanel {
    window: gtk::Window,
    revealer: gtk::Revealer,
    stack: gtk::Stack,
    /// The visible `.ts-drawer` card — a `gtk::Overlay` whose custom-drawn
    /// background paints the concave-flare silhouette (#34) behind the page
    /// content. Measured (post-map) for its real allocated size so the *card*,
    /// not the transparent surface, centers under the trigger chip.
    card: gtk::Overlay,
    current: RefCell<Option<Page>>,
    catcher: gtk::Window,
    /// The bar this drawer hangs off — its edge/offset/thickness drive the
    /// drawer's anchoring and perpendicular margin.
    geometry: BarGeometry,
    /// Main-axis center (X for horizontal bars, Y for vertical) of the chip
    /// that opened the drawer, in screen coordinates. Stashed at open time so
    /// the window-map handler can recompute the margin once the card has a
    /// real allocation (`measure` can return 0 before the surface is mapped).
    pending_center: RefCell<Option<i32>>,
    /// Emits `true` while the drawer is open (between `show_panel` and the
    /// retract animation finishing). Consumers — e.g. the bar — bind CSS
    /// classes to this so the seam between bar and drawer can restyle.
    open_state: Mutable<bool>,
}

thread_local! {
    static PANELS: RefCell<HashMap<String, ModalPanel>> = RefCell::new(HashMap::new());
    /// Per-connector drawer-open state, decoupled from `ModalPanel` lifetime
    /// so subscribers (OSD, bar CSS) can wire up before `install` runs and
    /// survive bar rebuilds on hot-plug.
    static DRAWER_OPEN: RefCell<HashMap<String, Mutable<bool>>> = RefCell::new(HashMap::new());
    /// `true` while a [`Page::uses_netconn`] page is the visible drawer page on
    /// *any* monitor. Drives [`netconn_visible_signal`] so the netconn `ss`
    /// poller can park while no one's looking (#50). Global (not per-monitor)
    /// because the netconn service is global; recomputed by
    /// [`recompute_netconn_visible`] on every page show/swap/retract.
    static NETCONN_VISIBLE: Mutable<bool> = Mutable::new(false);
    /// `true` while a [`Page::uses_app_usage`] page (the Stats panel) is the
    /// visible drawer page on *any* monitor. Drives [`stats_visible_signal`] so
    /// the app_usage `/proc` poller can park while no one's looking (#50, item 5
    /// of #42). Global (not per-monitor) because the app_usage service is
    /// global; recomputed by [`recompute_stats_visible`] on every page
    /// show/swap/retract.
    static STATS_VISIBLE: Mutable<bool> = Mutable::new(false);
}

/// Recompute [`NETCONN_VISIBLE`] from the live panel set: `true` iff some
/// monitor's drawer is currently showing a [`Page::uses_netconn`] page. Called
/// after every transition that changes a panel's `current` page (open, in-place
/// page swap, deep-link switch, retract-finish). `Mutable::set` is a no-op-free
/// notify so we recompute unconditionally and let it dedupe.
fn recompute_netconn_visible() {
    let visible = PANELS.with(|panels| {
        panels
            .borrow()
            .values()
            .any(|p| p.current.borrow().is_some_and(Page::uses_netconn))
    });
    NETCONN_VISIBLE.with(|m| {
        if m.get() != visible {
            m.set(visible);
        }
    });
}

/// Signal that emits `true` while a netconn-backed drawer page
/// ([`Page::uses_netconn`]: Connections / Network) is visible on any monitor.
/// Wired in `main.rs` to `netconn::set_active` so the always-on `ss` poller
/// parks when those panels are hidden (#50).
pub fn netconn_visible_signal() -> impl Signal<Item = bool> + 'static {
    NETCONN_VISIBLE.with(|m| m.signal())
}

/// Recompute [`STATS_VISIBLE`] from the live panel set: `true` iff some
/// monitor's drawer is currently showing a [`Page::uses_app_usage`] page (the
/// Stats panel). Called after every transition that changes a panel's `current`
/// page (open, in-place page swap, deep-link switch, retract-finish).
/// `Mutable::set` is a no-op-free notify so we recompute unconditionally and let
/// it dedupe.
fn recompute_stats_visible() {
    let visible = PANELS.with(|panels| {
        panels
            .borrow()
            .values()
            .any(|p| p.current.borrow().is_some_and(Page::uses_app_usage))
    });
    STATS_VISIBLE.with(|m| {
        if m.get() != visible {
            m.set(visible);
        }
    });
}

/// Signal that emits `true` while the Stats drawer page
/// ([`Page::uses_app_usage`]) is visible on any monitor. Wired in `main.rs` to
/// `app_usage::set_active` so the always-on `/proc` poller parks when that panel
/// is hidden (#50, item 5 of #42).
pub fn stats_visible_signal() -> impl Signal<Item = bool> + 'static {
    STATS_VISIBLE.with(|m| m.signal())
}

fn drawer_open_state(key: &str) -> Mutable<bool> {
    DRAWER_OPEN.with(|map| {
        map.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| Mutable::new(false))
            .clone()
    })
}

fn monitor_key(m: &Monitor) -> String {
    m.connector()
        .unwrap_or_else(|| format!("monitor:{:p}", m.gdk()))
}

/// Build the drawer for one monitor and mount it as a layer-shell window.
///
/// `bar` is the bar this drawer hangs off; its `edge` and `offset` (the
/// bar's own margin on that edge) are plumbed through so the drawer anchors
/// to the bar's *actual* edge with a perpendicular margin derived from the
/// bar's real offset + measured thickness. Must be called after the bar is
/// built so the bar window exists to be measured at open time.
pub fn install(monitor: &Monitor, bar: &BarHandle, edge: Edge, offset: i32) {
    let key = monitor_key(monitor);
    let (mon_w, mon_h) = monitor.size();
    let geometry = BarGeometry {
        edge,
        offset,
        mon_w,
        mon_h,
        bar_window: bar.window().clone(),
    };

    // Build the catcher FIRST so that within `Layer::Top` the catcher's
    // surface is committed before the drawer's. Within the same layer,
    // most Wayland compositors stack the most-recently-mapped surface on
    // top, so `show_panel` re-maps catcher → drawer on every open to keep
    // the drawer above its catcher.
    let catcher = build_catcher(monitor, key.clone());

    let window = build_drawer_window(monitor, &key, &geometry);
    wire_escape(&window, key.clone());

    let revealer = build_revealer(&geometry);
    let (card, content) = build_drawer_card(&geometry);
    let stack = build_pages_stack();

    content.append(&stack);
    revealer.set_child(Some(&card));
    window.set_child(Some(&revealer));

    wire_retract_finish(&revealer, key.clone());
    wire_recenter_on_map(&window, key.clone());
    window.set_visible(false);

    PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key.clone(),
            ModalPanel {
                window,
                revealer,
                stack,
                card,
                current: RefCell::new(None),
                catcher,
                geometry,
                pending_center: RefCell::new(None),
                open_state: drawer_open_state(&key),
            },
        );
    });
}

/// Drawer surface, anchored to the bar's *actual* edge. Content-driven
/// natural size; ignores other layer-shell exclusive zones (sets
/// `exclusive_zone(-1)`) so its perpendicular margin is measured from the
/// true screen edge — the bar's own exclusive reservation would otherwise
/// stack with our margin and push the drawer off the bar. The perpendicular
/// margin is set live in `show_panel` from `geometry.perpendicular_margin()`
/// (bar offset + measured thickness), not a hardcoded constant; the
/// main-axis margin (under the chip) is set per-open.
fn build_drawer_window(monitor: &Monitor, key: &str, geometry: &BarGeometry) -> gtk::Window {
    let (anchor_perp, anchor_main) = geometry.anchors();
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(anchor_perp)
        .anchor(anchor_main)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .namespace(format!("hytte-modal-{key}"))
        .build();
    window.add_css_class("ts-modal");
    window.set_exclusive_zone(-1);
    // AdwClamp inside each page caps width at 680; 360 floor keeps sparse
    // pages from collapsing. niri honors live surface-size commits so the
    // drawer grows/shrinks as pages switch. The floor applies on the bar's
    // main axis (width for horizontal bars, height for vertical).
    if geometry.horizontal() {
        window.set_size_request(360, -1);
    } else {
        window.set_size_request(-1, 360);
    }
    window
}

fn wire_escape(window: &gtk::Window, key: String) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            retract_by_key(&key);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

/// Revealer pinned against the bar so the card pulls out of the bar's far
/// edge rather than floating mid-surface. Slide direction + alignment match
/// the bar's edge (`SlideDown`/`Start` for a top bar, `SlideUp`/`End` for a
/// bottom bar, and the left/right cases). Size animates on page swaps.
fn build_revealer(geometry: &BarGeometry) -> gtk::Revealer {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(geometry.slide());
    revealer.set_transition_duration(180);
    revealer.set_reveal_child(false);
    if geometry.horizontal() {
        revealer.set_valign(geometry.perpendicular_align());
        revealer.set_vexpand(false);
    } else {
        revealer.set_halign(geometry.perpendicular_align());
        revealer.set_hexpand(false);
    }
    revealer
}

/// Build the drawer card: a custom-drawn silhouette that grows out of the bar
/// with a concave flare at its top corners (#34), with the page content
/// overlaid on top. Returns the `gtk::Overlay` (the measured "card") and the
/// content `gtk::Box` the pages stack mounts into.
///
/// Why custom-draw and not CSS: the flare is a *concave* curve (the corners
/// sweep outward to meet the bar). CSS `border-radius` only rounds corners
/// *inward* (convex), so the shape is painted in cairo here — mirroring the
/// [`crate::overlays::frame`] precedent. The content box is inset by
/// `DRAWER_FLARE_RADIUS` (its start/end margin) so the wings extend beyond it,
/// up to the bar; the overlay is measured from that content so the drawn
/// background tracks the page's natural size.
fn build_drawer_card(geometry: &BarGeometry) -> (gtk::Overlay, gtk::Box) {
    let card = gtk::Overlay::new();
    card.add_css_class("ts-drawer");
    if geometry.horizontal() {
        card.set_valign(geometry.perpendicular_align());
        card.set_vexpand(false);
    } else {
        card.set_halign(geometry.perpendicular_align());
        card.set_hexpand(false);
    }

    // No hexpand/vexpand: a gtk::Overlay always allocates its main child the
    // overlay's full size, so the DrawingArea fills the card regardless —
    // and setting expand here would propagate up and inflate the card past
    // its natural width, shifting it off-centre (#44). The `.ts-drawer-bg`
    // class carries `color: @shell_background` so the draw func can read the
    // fill color via `color()` — fixed to the always-dark shell surface color
    // so the drawer matches the bar regardless of the system theme (#90).
    let bg = gtk::DrawingArea::new();
    bg.add_css_class("ts-drawer-bg");
    bg.set_draw_func(|area, cr: &gtk::cairo::Context, width: i32, height: i32| {
        draw_drawer_silhouette(cr, f64::from(width), f64::from(height), area.color());
    });
    card.set_child(Some(&bg));

    // Repaint when the system light/dark preference flips.  The fill color
    // (`@shell_background`) is fixed and not theme-derived, so this is
    // technically a no-op, but retained as a cheap safety net in case the
    // variable is ever tied back to an Adwaita token.
    let bg_weak = bg.downgrade();
    adw::StyleManager::default().connect_dark_notify(move |_| {
        if let Some(bg) = bg_weak.upgrade() {
            bg.queue_draw();
        }
    });

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("ts-drawer-content");
    content.set_margin_start(DRAWER_FLARE_RADIUS);
    content.set_margin_end(DRAWER_FLARE_RADIUS);
    card.add_overlay(&content);
    // Size the overlay (and thus the drawn background) to the content + its
    // flare-inset margins, not the zero-natural-size DrawingArea.
    card.set_measure_overlay(&content, true);

    (card, content)
}

/// Paint the drawer card silhouette into `cr` for a `w`×`h` allocation: a card
/// flush to the bar along its top edge, its two top corners flaring *outward*
/// (concave, `DRAWER_FLARE_RADIUS`) to meet the bar, and ordinary convex
/// (`DRAWER_CORNER_RADIUS`) bottom corners. Mirrors the cairo approach in
/// [`crate::overlays::frame`].
///
/// `base` is read from the drawing area's `.ts-drawer-bg` CSS `color`, which
/// is now `@shell_background` — a fixed dark color that matches the bar
/// regardless of the system theme (fixes the light-mode regression from #44
/// where `@window_bg_color` painted the drawer near-white). The gradient
/// shades `base` by 0.82 at the top and fades to `base` at the bottom.
fn draw_drawer_silhouette(cr: &gtk::cairo::Context, w: f64, h: f64, base: gdk::RGBA) {
    use std::f64::consts::{FRAC_PI_2, PI};

    let rf = f64::from(DRAWER_FLARE_RADIUS);
    let rc = DRAWER_CORNER_RADIUS;
    // Degenerate allocation (pre-map / collapsed) — nothing sensible to draw.
    if w <= 2.0 * rf || h <= rf + rc {
        return;
    }

    // Outline, clockwise from the top-left wing tip. The top edge runs flush
    // along the bar's bottom; the two top corners curve outward (`arc_negative`)
    // up into the bar; the bottom two are ordinary convex corners (`arc`).
    cr.new_path();
    cr.move_to(0.0, 0.0);
    cr.line_to(w, 0.0);
    cr.arc_negative(w, rf, rf, -FRAC_PI_2, -PI); // top-right concave flare
    cr.line_to(w - rf, h - rc);
    cr.arc(w - rf - rc, h - rc, rc, 0.0, FRAC_PI_2); // bottom-right convex
    cr.line_to(rf + rc, h);
    cr.arc(rf + rc, h - rc, rc, FRAC_PI_2, PI); // bottom-left convex
    cr.line_to(rf, rf);
    cr.arc_negative(0.0, rf, rf, 0.0, -FRAC_PI_2); // top-left concave flare
    cr.close_path();

    // Fill: a vertical gradient from a slightly-shaded `@shell_background` at
    // the top to the base at the bottom. The fill is always dark (the drawer
    // always matches the always-dark bar via `@shell_background`), so the
    // old light-mode 1.04 lift branch is gone — only the 0.82 dark shade
    // remains. Edge is a faint white hairline, matching the dark surface.
    let base_rgb = [
        f64::from(base.red()),
        f64::from(base.green()),
        f64::from(base.blue()),
    ];
    let top = [
        (base_rgb[0] * 0.82).clamp(0.0, 1.0),
        (base_rgb[1] * 0.82).clamp(0.0, 1.0),
        (base_rgb[2] * 0.82).clamp(0.0, 1.0),
    ];

    let fill = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    fill.add_color_stop_rgba(0.0, top[0], top[1], top[2], 1.0);
    fill.add_color_stop_rgba(1.0, base_rgb[0], base_rgb[1], base_rgb[2], 1.0);
    if let Err(e) = cr.set_source(&fill) {
        tracing::warn!(error = %e, "drawer: failed to set gradient source");
        return;
    }
    if let Err(e) = cr.fill_preserve() {
        tracing::warn!(error = %e, "drawer: cairo fill failed");
        return;
    }

    // Faint hairline edge (was `border: 1px solid @borders`): white at 8% alpha.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
    cr.set_line_width(1.0);
    if let Err(e) = cr.stroke() {
        tracing::warn!(error = %e, "drawer: cairo stroke failed");
    }
}

/// `hhomogeneous`/`vhomogeneous` off so the stack reports the *visible*
/// child's natural size — without this, sparse pages (Calendar, `PowerMenu`)
/// render at the size of the largest mounted page (Stats / Audio).
fn build_pages_stack() -> gtk::Stack {
    use crate::panels;

    let stack = gtk::Stack::new();
    stack.set_vexpand(false);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    stack.set_interpolate_size(true);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    let pages: [(Page, gtk::Widget); 15] = [
        (Page::Media, panels::panel_media()),
        (Page::Network, panels::panel_network()),
        (Page::Vpn, panels::panel_vpn()),
        (Page::Connections, panels::panel_connections()),
        (Page::Bluetooth, panels::panel_bluetooth()),
        (Page::Stats, panels::panel_stats()),
        (Page::Audio, panels::panel_audio()),
        (Page::Power, panels::panel_power()),
        (Page::PowerMenu, panels::panel_power_menu()),
        (Page::Notifications, panels::panel_notifications()),
        (Page::Appearance, panels::panel_appearance()),
        (Page::Displays, panels::panel_displays()),
        (Page::Clipboard, panels::panel_clipboard()),
        (Page::Calendar, panels::panel_calendar()),
        (Page::Settings, panels::panel_settings()),
    ];
    for (page, widget) in pages {
        stack.add_named(&widget, Some(page.stack_name()));
    }
    stack
}

/// When the retract animation finishes, hide the drawer + catcher and clear
/// the open state for downstream subscribers.
fn wire_retract_finish(revealer: &gtk::Revealer, key: String) {
    revealer.connect_child_revealed_notify(move |r| {
        if r.is_child_revealed() {
            return;
        }
        PANELS.with(|panels| {
            let panels = panels.borrow();
            let Some(panel) = panels.get(&key) else {
                return;
            };
            panel.window.set_visible(false);
            panel.catcher.set_visible(false);
            *panel.current.borrow_mut() = None;
            panel.open_state.set(false);
        });
        recompute_netconn_visible();
        recompute_stats_visible();
    });
}

/// Close and remove all drawers (called before rebuilding bars on hot-plug).
pub fn close_all() {
    PANELS.with(|panels| {
        for (_, panel) in panels.borrow_mut().drain() {
            panel.catcher.close();
            panel.window.close();
        }
    });
    // No panels left → no netconn/stats page visible; park the pollers.
    recompute_netconn_visible();
    recompute_stats_visible();
}

/// Swap every currently-open panel's visible page to `target`. Drawer pages
/// are built once, monitor-agnostically, so an in-page deep-link callback
/// (e.g. a Settings row → Wallpaper) doesn't have a handy `&Monitor`. This
/// helper walks the per-monitor panel set and:
/// - For each panel currently showing a page (`current.is_some()`), swaps
///   the stack child to `target` (crossfade + height) without retracting.
/// - For closed panels, does nothing — we only switch what's actually open.
///
/// If the user has no drawer open when this fires, the call is a no-op.
/// Future work could open `target` on the primary monitor in that case;
/// for v1 we treat the deep-link as "in this drawer, jump there".
pub fn switch_active(target: Page) {
    PANELS.with(|panels| {
        for panel in panels.borrow().values() {
            if panel.current.borrow().is_some() {
                panel.stack.set_visible_child_name(target.stack_name());
                *panel.current.borrow_mut() = Some(target);
                on_page_show(target);
            }
        }
    });
    recompute_netconn_visible();
    recompute_stats_visible();
}

/// Begin the retract animation on every open drawer. Used by drawer-content
/// callbacks (e.g. the power-menu action rows) that don't carry a monitor
/// handle but want the drawer to close after their action fires.
pub fn dismiss_all() {
    PANELS.with(|panels| {
        for panel in panels.borrow().values() {
            panel.revealer.set_reveal_child(false);
        }
    });
}

/// Begin the retract animation. The notify-child-revealed handler finishes
/// the teardown (hiding the surface + closing the catcher) when it ends.
fn retract_by_key(key: &str) {
    PANELS.with(|panels| {
        if let Some(panel) = panels.borrow().get(key) {
            panel.revealer.set_reveal_child(false);
        }
    });
}

pub fn open(monitor: &Monitor, page: Page) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let Some(panel) = panels.get(&key) else {
            return;
        };
        // No bar chip context here (called from a notification toast click);
        // anchor the drawer flush with the bar's trailing main-axis edge.
        *panel.pending_center.borrow_mut() = None;
        show_panel(panel, page, 0);
    });
    recompute_netconn_visible();
    recompute_stats_visible();
}

/// Toggle the drawer on `monitor` to the given `page`, centering the drawer
/// card under `trigger` along the bar's main axis:
/// - Same page open → start retract.
/// - Different page open → swap stack child in place (crossfade + size);
///   the drawer surface keeps its existing position.
/// - Closed → reposition under `trigger`, present surface, reveal.
pub fn toggle(monitor: &Monitor, page: Page, trigger: &impl IsA<gtk::Widget>) {
    let key = monitor_key(monitor);
    PANELS.with(|panels| {
        let panels = panels.borrow();
        let Some(panel) = panels.get(&key) else {
            return;
        };
        let current = *panel.current.borrow();
        match current {
            Some(p) if p == page => {
                panel.revealer.set_reveal_child(false);
            }
            Some(_) => {
                panel.stack.set_visible_child_name(page.stack_name());
                *panel.current.borrow_mut() = Some(page);
                on_page_show(page);
            }
            None => {
                // Set the visible child first so `measure` reflects the
                // target page's natural size, not whatever was last shown.
                // `show_panel` re-sets it (idempotent).
                panel.stack.set_visible_child_name(page.stack_name());
                let chip_center = trigger_center(panel, trigger.upcast_ref());
                *panel.pending_center.borrow_mut() = chip_center;
                // Best-effort margin now (may use a pre-map measure that
                // underestimates); the window-map handler recomputes from the
                // real card allocation once the surface is mapped.
                let margin = chip_center.map_or(0, |c| main_margin_for_center(panel, c));
                show_panel(panel, page, margin);
            }
        }
    });
    // Swap/open may have changed which page is visible; the same-page retract
    // branch is recomputed later by `wire_retract_finish`. Idempotent.
    recompute_netconn_visible();
    recompute_stats_visible();
}

/// Present the drawer on `page` at `main_margin` pixels from the bar's
/// trailing main-axis edge (screen right for horizontal bars, screen top for
/// vertical). Also sets the perpendicular margin live from the bar's real
/// offset + thickness. Show the catcher before the drawer so that within
/// `Layer::Top` the drawer's surface commits most recently and stacks above
/// the catcher.
fn show_panel(panel: &ModalPanel, page: Page, main_margin: i32) {
    panel.stack.set_visible_child_name(page.stack_name());
    *panel.current.borrow_mut() = Some(page);
    panel.open_state.set(true);
    on_page_show(page);

    // Perpendicular margin: bar offset + live-measured thickness, replacing
    // the old hardcoded 59. The bar window is mapped by open time.
    panel.window.set_margin(
        panel.geometry.perpendicular_layer_edge(),
        panel.geometry.perpendicular_margin(),
    );
    panel
        .window
        .set_margin(panel.geometry.main_layer_edge(), main_margin);

    panel.catcher.set_visible(true);
    panel.catcher.present();

    panel.window.set_visible(true);
    panel.window.present();
    panel.revealer.set_reveal_child(true);
}

/// Recompute the main-axis margin from the *real* card allocation once the
/// surface is mapped. `measure` can return 0 before the surface is realized
/// (per the original centering docstring), flooring the drawer width and
/// shifting the card. `connect_map` fires *before* the first size-allocate,
/// so the recompute is deferred to the next main-loop idle — by then the card
/// has a real allocation (`card.width()`/`card.height()`, which include the
/// card's borders and padding). Fires on every map.
fn wire_recenter_on_map(window: &gtk::Window, key: String) {
    window.connect_map(move |_| {
        let key = key.clone();
        glib::idle_add_local_once(move || {
            PANELS.with(|panels| {
                let panels = panels.borrow();
                let Some(panel) = panels.get(&key) else {
                    return;
                };
                let Some(center) = *panel.pending_center.borrow() else {
                    return;
                };
                let margin = main_margin_for_center(panel, center);
                panel
                    .window
                    .set_margin(panel.geometry.main_layer_edge(), margin);
            });
        });
    });
}

/// The trigger chip's center along the bar's *main axis*, in screen
/// coordinates: X for horizontal (Top/Bottom) bars, Y for vertical
/// (Left/Right) bars. `None` if the chip isn't rooted yet (shouldn't happen
/// for a mapped bar widget).
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn trigger_center(panel: &ModalPanel, trigger: &gtk::Widget) -> Option<i32> {
    trigger.root().and_then(|root| {
        let mid = graphene::Point::new(trigger.width() as f32 / 2.0, trigger.height() as f32 / 2.0);
        trigger
            .compute_point(root.upcast_ref::<gtk::Widget>(), &mid)
            .map(|p| {
                if panel.geometry.horizontal() {
                    p.x() as i32
                } else {
                    p.y() as i32
                }
            })
    })
}

/// Main-axis margin (distance from the bar's trailing edge — screen right for
/// horizontal bars, screen top for vertical) so the *visible card* (not the
/// transparent surface) centers under `center` (a screen-coordinate point on
/// the main axis).
///
/// The card is inset from the trailing surface edge by `chrome_main_end`
/// (`.ts-modal` padding + `.ts-drawer` margin on that side), so the card's
/// trailing edge sits at `screen_extent - main_margin - chrome_main_end` and
/// its center at `screen_extent - main_margin - chrome_main_end -
/// card_extent/2`. Solving `card_center == center`:
///
/// ```text
/// main_margin = screen_extent - center - chrome_main_end - card_extent/2
/// ```
///
/// where `screen_extent` is `mon_w` (horizontal) or `mon_h` (vertical) and
/// `card_extent` is the card's real allocated size on the main axis (its
/// border box; CSS borders + padding included). Clamped to
/// `[0, screen_extent - card_footprint]` so the card can't fall off either
/// end — near the trailing/leading screen edge it collapses to flush.
fn main_margin_for_center(panel: &ModalPanel, center: i32) -> i32 {
    let geometry = &panel.geometry;
    let screen_extent = if geometry.horizontal() {
        geometry.mon_w
    } else {
        geometry.mon_h
    };

    // Card's real allocated main-axis size once mapped (its border box —
    // CSS borders + padding included). The eager call from `toggle` runs
    // before the surface is allocated, so `width()`/`height()` can be 0;
    // fall back to the card's *natural* measure there. The post-map idle
    // recompute then lands the final position from the real allocation.
    let orientation = if geometry.horizontal() {
        gtk::Orientation::Horizontal
    } else {
        gtk::Orientation::Vertical
    };
    let allocated = if geometry.horizontal() {
        panel.card.width()
    } else {
        panel.card.height()
    };
    let card_extent = if allocated > 0 {
        allocated
    } else {
        let (_, nat, _, _) = panel.card.measure(orientation, -1);
        nat
    };
    let card_extent = card_extent.clamp(360, DRAWER_MAX_WIDTH);

    let chrome_end = geometry.chrome_main_end();
    let chrome_start = geometry.chrome_main_start();
    let card_footprint = card_extent + chrome_start + chrome_end;

    let desired = screen_extent - center - chrome_end - card_extent / 2;
    let max = (screen_extent - card_footprint).max(0);
    desired.clamp(0, max)
}

/// Signal that emits `true` while the drawer on `monitor` is open (the
/// retract animation hasn't completed yet), and `false` when it's closed.
/// Backed by [`DRAWER_OPEN`] so callers can subscribe before `install` has
/// run for this monitor — needed because the OSD and bar both wire up
/// during synchronous boot, while modal panels are built later inside the
/// `monitors_changed` task.
pub fn drawer_open_signal(monitor: &Monitor) -> impl Signal<Item = bool> + 'static {
    drawer_open_state(&monitor_key(monitor)).signal()
}

/// Full-screen transparent layer-shell window that closes the drawer on any
/// press. Built once at install time and kept alive for the panel's
/// lifetime; visibility tracks the drawer.
fn build_catcher(monitor: &Monitor, modal_key: String) -> gtk::Window {
    let win = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::None)
        .namespace("hytte-modal-catcher")
        .build();
    win.add_css_class("ts-modal-catcher");

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    content.set_hexpand(true);
    content.set_vexpand(true);
    win.set_child(Some(&content));

    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    let modal_key_for_press = modal_key;
    gesture.connect_pressed(move |_, _, _, _| {
        retract_by_key(&modal_key_for_press);
    });
    content.add_controller(gesture);

    // Start hidden; `show_panel` toggles visibility alongside the drawer.
    win.set_visible(false);
    win
}

/// Per-page side-effects that should run whenever a page becomes visible
/// (initial open OR cross-fade swap from another page). Add a match arm
/// here when a new page needs an on-show fetch.
fn on_page_show(page: Page) {
    match page {
        Page::Clipboard => clipboard::refresh(),
        Page::Calendar => calendar::refresh(),
        // Opening the Notifications drawer = the user has seen them.
        // Dismiss all active toasts (move to history); the bell counter
        // bound to active.len() drops to zero.
        Page::Notifications => notifications::dismiss_all(),
        _ => {}
    }
}
