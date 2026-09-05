use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use hytte::adw;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, gdk, glib, graphene, prelude::*};
use hytte::prelude::*;
use hytte::services::calendar;
use hytte::services::clipboard;
use hytte::services::notifications;
use hytte::ui::{Anchor, Edge, Layer, LayerEdge, LayerShell, layer_window, on_surface_ready};

use crate::components::layout::DRAWER_MAX_WIDTH_WIDE;
use crate::components::monitor_key::{is_fallback_key, monitor_key};
use crate::components::visibility_gate::GateRegistry;
use crate::scale::scale;

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
/// `assets/trollshell/style.css`:
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

/// Chrome between the surface edge and the visible `.ts-drawer` card along the
/// **vertical** axis, summed over both ends — what a card *height* budget has
/// to hand back to the stylesheet. Derived from
/// `assets/trollshell/style.css`: `.ts-drawer` `margin-top: 0px` +
/// `margin-bottom: 20px`. `.ts-modal` carries no vertical padding (only
/// `padding-left`/`padding-right`), and `.ts-drawer-content`'s
/// [`DRAWER_FLARE_RADIUS`] inset is a start/end margin, so neither adds to the
/// vertical total.
///
/// These are the same two CSS properties whichever edge the bar sits on — for
/// a Top/Bottom bar the vertical axis happens to be the *perpendicular* one
/// (the axis [`BarGeometry::perpendicular_margin`] also eats into), for a
/// Left/Right bar it is the *main* one. See
/// [`BarGeometry::available_card_height`]. Keep in sync with the stylesheet,
/// exactly like the `CARD_CHROME_MAIN_*` constants above.
const CARD_CHROME_VERTICAL: i32 = 20;

/// Floor for [`BarGeometry::available_card_height`], in logical pixels.
///
/// Deliberately *not* run through [`scale`]: the budget it floors is measured
/// in live logical pixels off the monitor, not in design-baseline units, and
/// mixing the two spaces is the exact double-counting mistake #701 was about.
/// Chosen to match the smallest scroller cap the shell ships (`wifi.rs`'s 240),
/// i.e. the smallest viewport anyone has judged usable here.
const MIN_CARD_HEIGHT: i32 = 240;

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
    /// The monitor this drawer is on. Its size is re-read live (via
    /// [`Monitor::size`]) each time the card is positioned — never snapshotted
    /// — so a resolution/mode switch (kanshi profile change) that resizes the
    /// output without a hot-plug doesn't leave the centering clamp stale (#442).
    /// Since #600 it is the *fallback* for [`BarGeometry::main_extent`] rather
    /// than its primary source; the liveness requirement is unchanged.
    monitor: Monitor,
    /// The bar's layer-shell window, measured live at open time for both its
    /// real thickness (the perpendicular axis: height for Top/Bottom, width for
    /// Left/Right) and its main-axis extent (see [`BarGeometry::main_extent`]).
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

    /// Vertical room a drawer card actually has on this monitor, in live
    /// logical pixels: the screen height, less whatever this drawer's geometry
    /// reserves on that axis, less the card's own vertical chrome
    /// ([`CARD_CHROME_VERTICAL`]).
    ///
    /// This is what the Stats page's `ScrolledWindow` cap is derived from
    /// (#701). It used to be a hardcoded 560 design-baseline px pushed through
    /// [`scale`] — a number derived from nothing on the actual screen, so the
    /// drawer scrolled on a 1440-px-tall output with half the screen below the
    /// card sitting empty. Scaling can't rescue such a constant either:
    /// [`scale`] tracks font size and the page *content* rides the very same
    /// factor, so the ratio content-height ÷ cap is font-invariant.
    ///
    /// Which term is reserved depends on the axis the bar runs along, hence
    /// the [`BarGeometry::horizontal`] split:
    ///
    /// * **Top/Bottom bar** — the vertical axis is the *perpendicular* one, so
    ///   [`BarGeometry::perpendicular_margin`] (the bar's offset plus its
    ///   measured thickness) is the reservation. The main-axis margin runs
    ///   left↔right there and is correctly no part of it.
    /// * **Left/Right bar** — the vertical axis is instead the *main* one. The
    ///   bar's own thickness is a *width* reservation and rightly contributes
    ///   nothing, but `main_margin` does: [`BarGeometry::main_layer_edge`]
    ///   resolves to `Bottom` for a vertical bar, so the centering margin
    ///   [`reposition_card`] applies there lifts the card off the screen bottom
    ///   and is a genuine height reservation. This branch used to reserve zero,
    ///   returning a budget too large by exactly that margin (#793 item 2).
    ///
    /// That second case is unreachable while `main.rs`'s `BAR_EDGE` is
    /// `Edge::Top`. It is fixed anyway because the arithmetic's own tests would
    /// otherwise stand as a passing assertion of the wrong number for whoever
    /// first ships a left/right bar — see
    /// [`tests::clamp_card_height_reserves_a_vertical_bars_main_margin_not_its_thickness`].
    ///
    /// `main_margin` comes from [`live_main_margin`], the same expression
    /// [`reposition_card`] solves, so the budget and the placement cannot
    /// disagree about it. It reflects the card's allocation from the previous
    /// pass rather than the one this cap is about to produce — the caller runs
    /// before `reposition_card`, and `set_max_content_height` only *queues* a
    /// resize regardless — which is the same converging loop
    /// [`wire_recenter_on_map`]'s post-map recompute already drives. On the
    /// unanchored path (`open_by_key`) it is 0: the card sits flush with the
    /// bar's trailing edge and nothing is reserved.
    ///
    /// The drawer surface itself is fullscreen with `exclusive_zone(-1)` (see
    /// [`build_drawer_window`]), so the monitor's own height is the right
    /// starting point in both cases: no other exclusive zone shrinks it.
    ///
    /// Both terms are read **live** on every call, never snapshotted at
    /// install — the same #442 rule [`BarGeometry::main_extent`] already
    /// follows. A kanshi profile change resizes the output in place without a
    /// hot-plug, and the bar's CSS-driven thickness can change under us too.
    /// That liveness is why the value is *pushed into* the already-built page
    /// on show (see [`apply_stats_max_height`]) instead of being handed to
    /// [`build_page`], which builds once and could only ever capture a stale
    /// number.
    fn available_card_height(&self, main_margin: i32) -> i32 {
        let (_, monitor_height) = self.monitor.size();
        let reserved = if self.horizontal() {
            self.perpendicular_margin()
        } else {
            main_margin
        };
        clamp_card_height(monitor_height, reserved, CARD_CHROME_VERTICAL)
    }

    /// True when the bar runs horizontally (Top/Bottom) so the drawer
    /// positions along the X axis; false for vertical bars (Left/Right).
    fn horizontal(&self) -> bool {
        matches!(self.edge, Edge::Top | Edge::Bottom)
    }

    /// Extent of the strip the drawer centers within, along the bar's *main*
    /// axis: the bar surface's live width (Top/Bottom) or height (Left/Right).
    ///
    /// Deliberately the **bar surface**, not the monitor (#600). The chip
    /// coordinate that drives the centering math comes from [`trigger_center`],
    /// which is a point *inside the bar's own layer surface* — so the extent it
    /// is solved against has to be that same surface's, or the two spaces drift
    /// apart the moment anything reserves an exclusive zone on the bar's
    /// leading main-axis edge. The sidebar does exactly that (it commits
    /// `exclusive_zone = SIDEBAR_WIDTH` while open, see `overlays::sidebar`),
    /// and a bar anchored to *both* main-axis edges gets shifted **and**
    /// narrowed by it — which put the drawer a sidebar's width to the left of
    /// its chip. Reading the bar's live extent self-corrects for any surface
    /// that reserves leading space, without `modal` knowing which one it is.
    ///
    /// Still a **live** read on every call, never a value snapshotted at
    /// install: the bar is anchored to both main-axis edges, so the compositor
    /// reconfigures it on a resolution/mode switch (kanshi profile change)
    /// exactly as it does the monitor — #442's staleness fix survives. Falls
    /// back to the live monitor size when the bar window has no allocation yet
    /// (belt and braces: the bar maps at startup, long before any drawer opens,
    /// which is the same assumption [`BarGeometry::thickness`] already makes).
    ///
    /// Known limitation: the card is laid out from the *screen's* trailing edge
    /// (the drawer surface is fullscreen with `exclusive_zone(-1)`), so this
    /// assumes nothing reserves space on the bar's **trailing** main-axis edge.
    /// Nothing in trollshell does — the bar reserves on its own perpendicular
    /// edge and the sidebar on the left — and Wayland gives a client no way to
    /// learn its surface's absolute position, so the leading/trailing split of
    /// `monitor - bar` is not recoverable. If a trailing-edge reserving surface
    /// ever lands, the card would sit flush with the bar's trailing edge rather
    /// than sliding under it.
    fn main_extent(&self) -> i32 {
        let (mon_w, mon_h) = self.monitor.size();
        let (bar, monitor) = if self.horizontal() {
            (self.bar_window.width(), mon_w)
        } else {
            (self.bar_window.height(), mon_h)
        };
        if bar > 0 { bar } else { monitor }
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
    /// The combined (and multicolumn) Stats page. Chips open this in the
    /// `combined`/`multicolumn` layouts; `build_page` picks `panel_stats` vs
    /// `panel_stats_multicolumn` off [`crate::panels::stats::stats_layout`].
    Stats,
    /// The five per-resource Stats pages (#307's split, restored in #508
    /// alongside `Stats`). Only reached in the `split` layout: the resource
    /// chips target their own variant instead of `Stats`, and the plugin wire
    /// `Stats` page maps to `StatsCpu`. Inert in the other two layouts.
    StatsCpu,
    StatsMemory,
    StatsGpu,
    StatsDisks,
    StatsServices,
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
    /// (#50, item 5 of #42). The combined/multicolumn Stats page carries the
    /// CPU and Memory cards' top-apps expanders; in the `split` layout (#508)
    /// those expanders live on the per-resource `StatsCpu` / `StatsMemory`
    /// pages instead. Listing all three keeps the gate correct in every layout
    /// (only the layout's actual pages are ever registered/visible, so the
    /// unused variants never match at runtime).
    fn uses_app_usage(self) -> bool {
        matches!(self, Self::Stats | Self::StatsCpu | Self::StatsMemory)
    }

    /// Pages whose content is backed by the `mpris` service's position
    /// poller (the seek bar's `position_us`, sampled at 4 Hz while a player
    /// is Playing). Used to gate that poller on whether the Media panel is
    /// actually visible (#228): it's the only consumer of `position_us`
    /// (`panels/media.rs`'s position label + seek fraction).
    fn uses_mpris_position(self) -> bool {
        matches!(self, Self::Media)
    }

    /// Every drawer page. The single source for reverse lookups
    /// ([`Page::from_stack_name`]); the string mapping still lives only in
    /// [`Page::stack_name`], so a page's token is defined in exactly one place.
    const ALL: [Self; 20] = [
        Self::Media,
        Self::Network,
        Self::Vpn,
        Self::Connections,
        Self::Bluetooth,
        Self::Stats,
        Self::StatsCpu,
        Self::StatsMemory,
        Self::StatsGpu,
        Self::StatsDisks,
        Self::StatsServices,
        Self::Audio,
        Self::Power,
        Self::PowerMenu,
        Self::Notifications,
        Self::Appearance,
        Self::Displays,
        Self::Clipboard,
        Self::Calendar,
        Self::Settings,
    ];

    fn stack_name(self) -> &'static str {
        match self {
            Self::Media => "media",
            Self::Network => "network",
            Self::Vpn => "vpn",
            Self::Connections => "connections",
            Self::Bluetooth => "bluetooth",
            Self::Stats => "stats",
            Self::StatsCpu => "stats-cpu",
            Self::StatsMemory => "stats-memory",
            Self::StatsGpu => "stats-gpu",
            Self::StatsDisks => "stats-disks",
            Self::StatsServices => "stats-services",
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

    /// Reverse of [`Page::stack_name`]: resolve a `Page` from its stable
    /// stack-name token (e.g. `"power-menu"`). The command surface's
    /// `open-page` `GAction` uses this to turn a niri keybind's string argument
    /// into a `Page`. Returns `None` for an unknown token.
    #[must_use]
    pub fn from_stack_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.stack_name() == name)
    }
}

/// The fixed stack-child name of the per-monitor plugin drawer page (#349 PR2).
/// The `__` prefix keeps it out of the [`Page::stack_name`] keyspace (which
/// `stack_names_are_unique_and_nonempty` guards for `Page` only), so it can never
/// collide with a built-in page token.
const PLUGIN_STACK_CHILD: &str = "__plugin";

/// What a drawer is currently showing: a built-in [`Page`], or a plugin's own
/// panel (keyed by plugin id). Keeps `Page` `Copy` and untouched (#349 PR2) — the
/// plugin concept lives only here, not in the 19-variant `Page` enum the shell's
/// ~60 call sites pass by value. A plugin panel drives none of the
/// netconn/stats/media pollers, so those gates match only `Builtin`.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Active {
    Builtin(Page),
    Plugin(String),
}

impl Active {
    /// The stack-child name to make visible for this active target: the page's
    /// own token for a built-in, or the single fixed plugin-child name for any
    /// plugin panel (there is one plugin child per drawer; the active selection
    /// picks which plugin's tree it shows — see `plugins::plugin_panel_slot`).
    fn stack_name(&self) -> &str {
        match self {
            Self::Builtin(p) => p.stack_name(),
            Self::Plugin(_) => PLUGIN_STACK_CHILD,
        }
    }

    /// The built-in page this target shows, if any — `None` for a plugin panel.
    /// Used by the poller gates (which only built-in pages drive) and the
    /// same-page retract compare in [`toggle`].
    fn builtin(&self) -> Option<Page> {
        match self {
            Self::Builtin(p) => Some(*p),
            Self::Plugin(_) => None,
        }
    }
}

/// Per-monitor drawer handle. Internally owns a single fullscreen layer-shell
/// window (a transparent click-catching background plus the card overlaid on
/// top — one surface, no cross-surface restacking for niri to mishandle, #109)
/// and a `GtkRevealer` that slides the card out of the bar's far edge
/// (direction chosen per `BarGeometry`).
struct ModalPanel {
    window: gtk::Window,
    revealer: gtk::Revealer,
    stack: gtk::Stack,
    /// The visible `.ts-drawer` card — a `gtk::Overlay` whose custom-drawn
    /// background paints the concave-flare silhouette (#34) behind the page
    /// content. Measured (post-map) for its real allocated size so the *card*,
    /// not the transparent surface, centers under the trigger chip.
    card: gtk::Overlay,
    /// What the drawer is currently showing (a built-in [`Page`] or a plugin
    /// panel), or `None` when closed (#349 PR2 widened this from `Option<Page>`).
    current: RefCell<Option<Active>>,
    /// The `.ts-modal` positioner box overlaid on the fullscreen window. It
    /// carries the chrome padding and is aligned to the bar's perp+trailing
    /// corner; its GTK margins (set per-open) place the card the same distance
    /// from the screen edge the old content-sized drawer surface did — so the
    /// centering math is unchanged, only the margin *sink* moved here from the
    /// window. The `revealer` lives inside it.
    positioner: gtk::Box,
    /// The bar this drawer hangs off — its edge/offset/thickness drive the
    /// drawer's anchoring and perpendicular margin.
    geometry: BarGeometry,
    /// The chip the drawer was opened from, or `None` for the chipless entry
    /// points (`open_by_key` / `open_plugin_by_key`), which anchor the card
    /// flush with the bar's trailing edge. See [`TriggerAnchor`].
    anchor: RefCell<Option<TriggerAnchor>>,
    /// Emits `true` while the drawer is open (between `show_panel` and the
    /// retract animation finishing). Consumers — e.g. the bar — bind CSS
    /// classes to this so the seam between bar and drawer can restyle.
    open_state: Mutable<bool>,
}

/// What the open drawer's card is centered on: the bar chip that opened it,
/// plus the last main-axis center that chip resolved to.
///
/// Holding the **widget**, not just the number, is what makes the card
/// re-centerable (#612). The center from [`trigger_center`] is a point in the
/// *bar surface's* coordinate space (#600), and that space is not stable while
/// the drawer is open: anything that reserves leading main-axis space — the
/// sidebar's `exclusive_zone`, see `overlays::sidebar` — both shifts and
/// narrows the bar, moving every chip inside it. So the recorded number goes
/// stale on a zone change even though the chip never moved on screen, and only
/// a re-measure against the live bar recovers the truth. Weak so a hot-plug
/// that tears the bar down isn't kept alive by a drawer handle; `center` is the
/// last successful measure, used as the fallback if the chip is gone (a bar
/// teardown takes the drawer with it via `close_all`, so that is close to
/// unreachable — but a stale number still beats none).
struct TriggerAnchor {
    /// The chip, weakly. `None` on upgrade → fall back to `center`.
    widget: glib::WeakRef<gtk::Widget>,
    /// Last main-axis center this chip measured to, in the bar surface's
    /// coordinate space. Refreshed by [`live_center`] on every re-measure.
    center: i32,
}

/// Identifies one global "is a matching drawer page visible on any monitor"
/// gate. Add a variant + arm in [`GateId::matches`] to gate another poller;
/// [`recompute_gates`] and the three named signal accessors below stay
/// one-line each.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GateId {
    /// [`Page::uses_netconn`] (Connections / Network) — gates the netconn
    /// `ss` poller (#50).
    Netconn,
    /// [`Page::uses_app_usage`] (Stats) — gates the `app_usage` `/proc` poller
    /// (#50, item 5 of #42).
    Stats,
    /// [`Page::uses_mpris_position`] (Media) — gates the mpris per-player
    /// `Position` pollers (#228).
    Media,
}

impl GateId {
    fn matches(self, page: Page) -> bool {
        match self {
            GateId::Netconn => page.uses_netconn(),
            GateId::Stats => page.uses_app_usage(),
            GateId::Media => page.uses_mpris_position(),
        }
    }
}

thread_local! {
    /// `Rc<ModalPanel>`, not a bare `ModalPanel` (#643): every entry point below
    /// needs a whole `&ModalPanel` to drive a run of GTK calls
    /// (`set_stack_page` / `show_panel` / `set_reveal_child` / `on_page_show`),
    /// and the only way to do that without holding this cell borrowed across all
    /// of it is to clone a handle out first. `close_all` and `install` are the
    /// `borrow_mut()` counterparties that make a held shared borrow a hazard;
    /// a `BorrowMutError` unwinding through a glib callback aborts the process.
    /// Mirrors `overlays::osd`'s `OSDS`.
    static PANELS: RefCell<HashMap<String, Rc<ModalPanel>>> = RefCell::new(HashMap::new());
    /// Per-connector drawer-open state, decoupled from `ModalPanel` lifetime
    /// so subscribers (OSD, bar CSS) can wire up before `install` runs and
    /// survive bar rebuilds on hot-plug.
    static DRAWER_OPEN: RefCell<HashMap<String, Mutable<bool>>> = RefCell::new(HashMap::new());
    /// One [`GateId`] gate per poller-parking consumer, `true` while some
    /// monitor's drawer is currently showing a page matching that gate's
    /// [`GateId::matches`] predicate. Global (not per-monitor) because each
    /// backing service is a single global poller; recomputed by
    /// [`recompute_gates`] on every page show/swap/retract. Was three
    /// hand-rolled `NETCONN_VISIBLE`/`STATS_VISIBLE`/`MEDIA_VISIBLE`
    /// `Mutable`s + one `recompute_*_visible` fn apiece before #443 — see
    /// #228's PR body for the original follow-up note.
    static GATES: GateRegistry<GateId> = GateRegistry::new();
}

/// Recompute every [`GateId`] gate from the live panel set. Called after
/// every transition that changes a panel's `current` page (open, in-place
/// page swap, deep-link switch, retract-finish). Each gate's `set` is a
/// no-op-free notify so we recompute unconditionally and let it dedupe.
fn recompute_gates() {
    let pages: Vec<Page> = PANELS.with(|panels| {
        panels
            .borrow()
            .values()
            .filter_map(|p| p.current.borrow().as_ref().and_then(Active::builtin))
            .collect()
    });
    GATES.with(|gates| {
        for id in [GateId::Netconn, GateId::Stats, GateId::Media] {
            gates.set(id, pages.iter().copied().any(|p| id.matches(p)));
        }
    });
}

/// Signal that emits `true` while a netconn-backed drawer page
/// ([`Page::uses_netconn`]: Connections / Network) is visible on any monitor.
/// Wired in `main.rs` to `netconn::set_active` so the always-on `ss` poller
/// parks when those panels are hidden (#50).
pub fn netconn_visible_signal() -> impl Signal<Item = bool> + 'static {
    GATES.with(|gates| gates.mutable(GateId::Netconn)).signal()
}

/// Signal that emits `true` while the Stats drawer page
/// ([`Page::uses_app_usage`]) is visible on any monitor. Wired in `main.rs` to
/// `app_usage::set_active` so the always-on `/proc` poller parks when that panel
/// is hidden (#50, item 5 of #42).
pub fn stats_visible_signal() -> impl Signal<Item = bool> + 'static {
    GATES.with(|gates| gates.mutable(GateId::Stats)).signal()
}

/// Signal that emits `true` while the Media drawer page
/// ([`Page::uses_mpris_position`]) is visible on any monitor. Wired in
/// `main.rs` to `mpris::set_active` so the always-on per-player `Position`
/// pollers park when that panel is hidden (#228).
pub fn media_visible_signal() -> impl Signal<Item = bool> + 'static {
    GATES.with(|gates| gates.mutable(GateId::Media)).signal()
}

fn drawer_open_state(key: &str) -> Mutable<bool> {
    DRAWER_OPEN.with(|map| {
        map.borrow_mut()
            .entry(key.to_string())
            .or_insert_with(|| Mutable::new(false))
            .clone()
    })
}

/// Every mounted drawer, as owned handles with the [`PANELS`] borrow already
/// released. The read half of the #643 sweep for this module: every caller that
/// drives GTK over the panel set takes this snapshot instead of iterating
/// `panels.borrow().values()` across those calls, so no GTK emission can ever
/// find `PANELS` borrowed. Cloning an `Rc` per drawer is a refcount bump.
fn live_panels() -> Vec<Rc<ModalPanel>> {
    PANELS.with(|panels| panels.borrow().values().map(Rc::clone).collect())
}

/// One mounted drawer by connector key, as an owned handle with the [`PANELS`]
/// borrow already released. Single-key counterpart to [`live_panels`].
fn live_panel(key: &str) -> Option<Rc<ModalPanel>> {
    PANELS.with(|panels| panels.borrow().get(key).map(Rc::clone))
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
    let geometry = BarGeometry {
        edge,
        offset,
        monitor: monitor.clone(),
        bar_window: bar.window().clone(),
    };

    // One fullscreen `Layer::Top` surface (#109): an `Overlay` whose MAIN child
    // is a transparent click-catcher box and whose OVERLAY child is the
    // positioner carrying the card. Z-order is handled INSIDE GTK (overlay
    // children paint above the main child), so there's no cross-surface
    // restacking within `Layer::Top` for niri to mishandle — the previous
    // two-surface design (separate catcher + drawer windows) relied on niri
    // honoring present-order restacking within a layer, which it does not, so
    // outside clicks never reached the catcher's gesture.
    let window = build_drawer_window(monitor, &key, &geometry);
    wire_escape(&window, key.clone());

    let revealer = build_revealer(&geometry);
    let (card, content) = build_drawer_card(&geometry);
    let stack = build_pages_stack();

    content.append(&stack);
    revealer.set_child(Some(&card));

    // Transparent click-catching background: any press retracts the drawer.
    let catcher_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    catcher_box.add_css_class("ts-modal-catcher");
    catcher_box.set_hexpand(true);
    catcher_box.set_vexpand(true);
    let catcher_gesture = gtk::GestureClick::new();
    catcher_gesture.set_button(0);
    let catcher_key = key.clone();
    catcher_gesture.connect_pressed(move |_, _, _, _| {
        retract_by_key(&catcher_key);
    });
    catcher_box.add_controller(catcher_gesture);

    // Positioner: the `.ts-modal` chrome-bearing box, aligned to the bar's
    // perpendicular + trailing-main corner. Because the overlay is fullscreen,
    // a GTK margin on this box (with the trailing-edge alignment) lands the
    // card the same distance from the screen edge the old layer-shell
    // `set_margin` did — so the centering math is byte-identical (only the
    // margin sink moved window → positioner). Holds the revealer → card.
    let positioner = build_positioner(&geometry);
    positioner.append(&revealer);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&catcher_box));
    overlay.add_overlay(&positioner);
    window.set_child(Some(&overlay));

    wire_retract_finish(&revealer, key.clone());
    wire_recenter_on_map(&window, key.clone());
    wire_recenter_on_bar_geometry(&geometry.bar_window, key.clone());

    window.set_visible(false);

    // `insert` returns the previous `ModalPanel` for this key, if any. As a
    // bare statement, `panels.borrow_mut().insert(...);` would drop that
    // returned value as a temporary of the *same* statement as the
    // `borrow_mut()` RefMut — and statement temporaries drop in reverse
    // creation order, so the old `ModalPanel` (and its `gtk::Window`) would
    // be dropped *before* the RefMut, i.e. while `PANELS` is still borrowed
    // (#631). Wrapping in `drop(...)` moves that drop to after `with`
    // returns, once the borrow has already ended. Reachable only if
    // `install` ran twice for one key without an intervening `close_all`,
    // which `main.rs` currently prevents — weakest of the sweep, kept for
    // the same reason as the others.
    drop(PANELS.with(|panels| {
        panels.borrow_mut().insert(
            key.clone(),
            Rc::new(ModalPanel {
                window,
                revealer,
                stack,
                card,
                current: RefCell::new(None),
                positioner,
                geometry,
                anchor: RefCell::new(None),
                open_state: drawer_open_state(&key),
            }),
        )
    }));
}

/// The single fullscreen drawer surface (#109): anchored to all four screen
/// edges so the inner `Overlay` fills the screen, holding both the transparent
/// click-catcher and the card. Transparent (`.ts-modal-catcher`, not
/// `.ts-modal` — the chrome padding lives on the positioner inside). Ignores
/// other layer-shell exclusive zones (`exclusive_zone(-1)`) so the positioner's
/// perpendicular margin is measured from the true screen edge — the bar's own
/// exclusive reservation would otherwise stack with our margin and push the
/// drawer off the bar. Card positioning is done with GTK margins on the
/// positioner box (set live in `show_panel`), since this surface no longer
/// moves.
fn build_drawer_window(monitor: &Monitor, key: &str, _geometry: &BarGeometry) -> gtk::Window {
    let window = layer_window(monitor)
        .layer(Layer::Top)
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .namespace(format!("hytte-modal-{key}"))
        .build();
    window.add_css_class("ts-modal-catcher");
    window.set_exclusive_zone(-1);
    window
}

/// Build the `.ts-modal` positioner box overlaid on the fullscreen window.
/// It carries the chrome padding and is aligned to the bar's perpendicular +
/// *trailing*-main corner so a GTK margin places the card the same distance
/// from the screen edge the old content-sized drawer surface did. The main-axis
/// floor (was on the drawer window) lives here now: `AdwClamp` inside each page
/// caps width at 680; the 360 floor keeps sparse pages from collapsing. Scaled
/// with the font (#114) so the floor grows consistently with the cap. The
/// `revealer` is appended into it by the caller.
fn build_positioner(geometry: &BarGeometry) -> gtk::Box {
    let positioner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    positioner.add_css_class("ts-modal");
    let floor = scale(360);
    if geometry.horizontal() {
        // Horizontal bar: hug the trailing (right) main edge, align
        // perpendicularly to the bar's near/far side.
        positioner.set_halign(gtk::Align::End);
        positioner.set_valign(geometry.perpendicular_align());
        positioner.set_size_request(floor, -1);
    } else {
        // Vertical bar: hug the trailing (bottom) main edge.
        positioner.set_valign(gtk::Align::End);
        positioner.set_halign(geometry.perpendicular_align());
        positioner.set_size_request(-1, floor);
    }
    positioner
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
/// is `@shell_background` — a fixed dark color that matches the bar and the
/// sidebar (fixes the light-mode regression from #44 where `@window_bg_color`
/// painted the drawer near-white). The fill is a flat `@shell_background`,
/// matching `.ts-sidebar` exactly (#106 — removed the 0.82× top-darkening
/// gradient that made the drawer's top edge read darker than the bar).
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

    // Fill: flat `@shell_background` so the drawer matches the sidebar
    // (`.ts-sidebar`, flat `@shell_background`) and the bar's base color.
    // Previously a vertical gradient darkened the top to 0.82×, making the
    // top edge — flush against the bar — read noticeably darker (#106).
    let base_rgb = [
        f64::from(base.red()),
        f64::from(base.green()),
        f64::from(base.blue()),
    ];
    cr.set_source_rgba(base_rgb[0], base_rgb[1], base_rgb[2], 1.0);
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

/// Construct the page widget for `page` — the single registry mapping a `Page`
/// to its panel constructor. The exhaustive `match` (no wildcard arm) is the
/// compile-time guarantee that adding a `Page` variant forces a build arm here,
/// so a new page can never silently skip lazy registration.
///
/// Called by [`ensure_page`] on a page's first activation. Every panel
/// constructor is side-effect-free at build time — `bind*` delivers current
/// signal state on subscribe, and per-page on-show work (clipboard/calendar
/// refresh, notification dismissal) is driven by [`on_page_show`], not
/// construction — so deferring a build to first open loses nothing. This holds
/// for every page, the combined Stats page included: all sparkline history
/// lives in the `sensors` service (the scalar `*_history()` rings plus the per-core
/// `cpu_per_core_history()` / `cpu_freq_per_core_history()`), so a lazily-built
/// page backfills instantly via `Sparkline::set_samples` /
/// `MultiSparkline::set_frames`. The CPU page's clock aggregate and its two
/// per-core `MultiSparkline`s were the last holdout, hoisted in #338 (#336 had
/// already done Memory/Disks/GPU and the overall CPU-load line); `EAGER_PAGES`
/// is now empty, so this function is only ever reached lazily.
fn build_page(page: Page) -> gtk::Widget {
    use crate::panels;
    use crate::panels::stats::StatsLayout;

    match page {
        Page::Media => panels::panel_media(),
        Page::Network => panels::panel_network(),
        Page::Vpn => panels::panel_vpn(),
        Page::Connections => panels::panel_connections(),
        Page::Bluetooth => panels::panel_bluetooth(),
        // The single Stats page renders combined or multicolumn per the runtime
        // layout (#508); `split` never opens `Page::Stats` (chips target the
        // per-resource variants), so combined is the harmless fallback there.
        Page::Stats => match crate::panels::stats::stats_layout() {
            StatsLayout::Multicolumn => panels::panel_stats_multicolumn(),
            StatsLayout::Combined | StatsLayout::Split => panels::panel_stats(),
        },
        Page::StatsCpu => panels::panel_stats_cpu(),
        Page::StatsMemory => panels::panel_stats_memory(),
        Page::StatsGpu => panels::panel_stats_gpu(),
        Page::StatsDisks => panels::panel_stats_disks(),
        Page::StatsServices => panels::panel_stats_services(),
        Page::Audio => panels::panel_audio(),
        Page::Power => panels::panel_power(),
        Page::PowerMenu => panels::panel_power_menu(),
        Page::Notifications => panels::panel_notifications(),
        Page::Appearance => panels::panel_appearance(),
        Page::Displays => panels::panel_displays(),
        Page::Clipboard => panels::panel_clipboard(),
        Page::Calendar => panels::panel_calendar(),
        Page::Settings => panels::panel_settings(),
    }
}

/// Ensure `page`'s widget exists in `stack`, building it on first request
/// (the build-on-first-open hook, #231). [`gtk::Stack::child_by_name`] is the
/// source of truth for "is this page built" — no separate `Page`→widget map to
/// leak or dangle across hot-plug, since a lazily-built child dies with its
/// stack (mirroring how the old eager pages array was consumed into the stack
/// and never held elsewhere). Idempotent.
///
/// Must run *before* any `set_visible_child_name` for `page`, or GTK silently
/// no-ops the switch (warns, shows nothing); [`set_stack_page`] is the single
/// choke point that guarantees it.
fn ensure_page(stack: &gtk::Stack, page: Page) {
    if stack.child_by_name(page.stack_name()).is_none() {
        let widget = build_page(page);
        stack.add_named(&widget, Some(page.stack_name()));
    }
}

/// Switch `panel`'s drawer stack to `active` (a built-in [`Page`] or the plugin
/// panel), building a built-in page on first use. The **single choke point**
/// every visibility-changing path routes through
/// ([`open`]/[`toggle`]/[`switch_active`]/[`show_panel_active`]), so no route can
/// reach `set_visible_child_name` for an unbuilt page. The plugin child is added
/// eagerly in [`build_pages_stack`] and always exists, so it needs no
/// build-on-first-open (#349 PR2).
fn set_stack_active(panel: &ModalPanel, active: &Active) {
    match active {
        Active::Builtin(page) => ensure_page(&panel.stack, *page),
        Active::Plugin(_) => {}
    }
    panel.stack.set_visible_child_name(active.stack_name());
}

/// Thin built-in wrapper over [`set_stack_active`] — the four historical callers
/// that only ever switch to a `Page` (`switch_active`, `toggle`'s swap + open
/// branches, `show_panel`) keep working unchanged.
fn set_stack_page(panel: &ModalPanel, page: Page) {
    set_stack_active(panel, &Active::Builtin(page));
}

/// Per-active side-effects on show: a built-in page runs its [`on_page_show`]
/// hook; a plugin panel publishes the active selection so every per-monitor
/// plugin drawer child reconciles that plugin's tree (#349 PR2).
fn on_active_show(panel: &ModalPanel, active: &Active) {
    match active {
        Active::Builtin(page) => on_page_show(panel, *page),
        Active::Plugin(id) => crate::plugins::set_active_panel(Some(id)),
    }
}

/// `hhomogeneous`/`vhomogeneous` off so the stack reports the *visible* child's
/// natural size — without this, sparse pages (Calendar, `PowerMenu`) render at
/// the size of the largest mounted page (Stats / Audio). With homogeneous
/// sizing off the stack sizes to whichever child is visible, so pages need not
/// all exist for a stable measure — which is exactly what makes lazy building
/// size-neutral: each page still measures to its own natural size on show.
///
/// No page is built eagerly any more: [`EAGER_PAGES`] is empty. Every page —
/// the CPU stats page included, since #338 hoisted its clock aggregate + the two
/// per-core `MultiSparkline` series into the `sensors` service (joining
/// Memory/Disks/GPU + the overall CPU-load line, hoisted in #336) — builds
/// lazily on first activation via [`ensure_page`]/[`set_stack_page`] and
/// backfills instantly via `Sparkline::set_samples` / `MultiSparkline::set_frames`.
/// This drops the startup cost from `19×N` panels (one full set per monitor) to
/// `0` — the stack starts empty and grows a child per page on first open. Nothing
/// is visible until the drawer first opens, which is itself a `set_stack_page`
/// call, so the first-open page is built then.
///
/// [`EAGER_PAGES`] is the single source for which pages skip the lazy path;
/// [`tests::eager_pages_is_empty`] tripwires it so a future edit can't silently
/// grow this set without revisiting the reasoning above.
const EAGER_PAGES: [Page; 0] = [];

fn build_pages_stack() -> gtk::Stack {
    let stack = gtk::Stack::new();
    stack.set_vexpand(false);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(140);
    stack.set_interpolate_size(true);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    for page in EAGER_PAGES {
        ensure_page(&stack, page);
    }

    // The one eagerly-added child: the per-monitor plugin drawer page (#349
    // PR2). Harmless — it's an empty region until a plugin panel is active, and
    // with `hhomogeneous`/`vhomogeneous` off it contributes zero size while not
    // visible. It is NOT a `Page` (so it sidesteps `build_page`'s exhaustive
    // match), added once under the fixed `PLUGIN_STACK_CHILD` name and never
    // rebuilt. `plugin_panel_slot()` only needs `PluginHandles` registered,
    // which happens at `App::with(plugins::service())` before any monitor build.
    stack.add_named(
        &crate::plugins::plugin_panel_slot(),
        Some(PLUGIN_STACK_CHILD),
    );
    stack
}

/// When the retract animation finishes, hide the drawer surface and clear the
/// open state for downstream subscribers. The click-catcher is part of the same
/// surface now, so hiding the window hides both.
fn wire_retract_finish(revealer: &gtk::Revealer, key: String) {
    revealer.connect_child_revealed_notify(move |r| {
        if r.is_child_revealed() {
            return;
        }
        // Post-#627, `close_all` drains every entry out of `PANELS` before
        // closing any window, so a synchronous `child-revealed` emission
        // during one of those closes now always lands on the `None` branch
        // (this key is already gone). Benign: `close_all`'s own tail performs
        // everything the `Some` arm below would have — `set_visible(false)`
        // on a window that's being dropped anyway, `*current = None`,
        // `open_state.set(false)`, and (unconditionally)
        // `set_active_panel(None)` — so doing nothing here just skips
        // redundant work, not a missed teardown.
        //
        // The lookup is a `.cloned()` handle rather than a `Ref` held over the
        // body (#643): `set_visible(false)` is a GTK call, and `open_state
        // .set(false)` wakes every drawer-open subscriber — `futures-signals`
        // calls `Waker::wake()` synchronously from `notify`. Both are paths a
        // re-entrant `PANELS` borrow could arrive on.
        let panel = PANELS.with(|panels| panels.borrow().get(&key).map(Rc::clone));
        let closed_plugin_panel = panel.is_some_and(|panel| {
            panel.window.set_visible(false);
            let was_plugin = matches!(&*panel.current.borrow(), Some(Active::Plugin(_)));
            *panel.current.borrow_mut() = None;
            panel.open_state.set(false);
            was_plugin
        });
        // Clear the plugin-panel selection only when *this* drawer was the one
        // showing a plugin panel. `active_panel_id` is a single global selection
        // shared by every monitor's plugin child, but drawers are per-monitor and
        // can be open at once — clearing unconditionally would blank a plugin
        // panel still open on another monitor when an unrelated drawer closes
        // here (#349 PR2).
        if closed_plugin_panel {
            crate::plugins::set_active_panel(None);
        }
        recompute_gates();
    });
}

/// Close and remove all drawers (called before rebuilding bars on hot-plug).
///
/// Drains `PANELS` into a local `Vec` and drops the borrow *before* calling
/// `gtk::Window::destroy` on each entry, rather than destroying from inside
/// the `borrow_mut()` (#627). `wire_retract_finish`'s `child-revealed`
/// handler takes its own `borrow()` of the same `RefCell`; if `destroy()`
/// ever triggered that signal synchronously, running it under an active
/// `borrow_mut()` would panic, and a panic unwinding through a glib callback
/// aborts the process. Whether `gtk_window_destroy`/`gtk_revealer_unmap` can
/// emit `notify::child-revealed` synchronously is not verified either way —
/// draining first removes the dependency on that question entirely, which is
/// the point: not holding the borrow is free, so there's no need to resolve
/// it first.
///
/// Uses `destroy()`, not `close()` (#632): a drawer the user never opened on
/// this monitor is still unrealized when a hot-plug tears it down, and
/// `gtk_window_close` is a *request* routed through `close-request` that does
/// not drop GTK's internal toplevel reference — `gtk_window_destroy` is the
/// call documented to do that, and unlike `close()` it can't be vetoed by a
/// future `close-request` handler.
pub fn close_all() {
    let panels: Vec<_> =
        PANELS.with(|panels| panels.borrow_mut().drain().map(|(_, v)| v).collect());
    for panel in panels {
        panel.window.destroy();
    }
    reset_drawer_open_states();
    // A monitor teardown that held the open plugin panel must clear the
    // selection too, so the v1 "hot-unplug just closes the plugin page with the
    // drawer" default holds (#349 PR2).
    //
    // This clears the *selection*; it is no longer also the thing that releases
    // the panel's preem renderer instances (#903). It cannot be: the destroys
    // above have already aborted every drawer child's render subscription, and a
    // `Mutable::set` here only wakes a task for the next `glib::MainContext`
    // iteration — reached after this function has returned — so nothing is left
    // to receive the `None`, whichever side of the destroys it is broadcast from.
    // `plugins::region`'s panel child now releases its scope from its own
    // `connect_destroy` instead, which is independent of who is subscribed.
    crate::plugins::set_active_panel(None);
    // No panels left → no netconn/stats page visible; park the pollers.
    recompute_gates();
}

/// The [`DRAWER_OPEN`] half of [`close_all`]: every drawer window is gone, so
/// clear each state to `false`, then drop the entries that can never be reused.
///
/// **Clear (#618).** `close_all` closes the windows but used to leave the
/// `Mutable`s alone, so a monitor whose drawer was open when a hot-plug /
/// kanshi profile switch fired kept reading `true` with no drawer attached.
/// That silently poisoned every subscriber for the rest of the session:
/// `overlays::osd` treats `drawer_open` as "the drawer already shows this
/// control, the OSD would be redundant" and suppressed volume/mic/brightness
/// feedback on that output, and `main.rs`'s `bind_class` kept the bar's
/// squared-off `drawer-open` seam corner. The overlay rebuild didn't rescue it:
/// a fresh `OsdView` starts at `false`, but subscribing to a `Mutable` delivers
/// its current value immediately, so the new view was handed the stale `true`
/// right back. Written with `set_neq` so the monitors that were already closed
/// — every one of them, in the common case — don't emit a redundant tick.
///
/// **Prune.** `DRAWER_OPEN` is keyed per-monitor and deliberately survives a
/// rebuild for connector-named monitors (so a subscriber wired up before the
/// rebuild — OSD, bar CSS — keeps working after it). But a connector-less
/// monitor's fallback key is the now-defunct `GdkMonitor` pointer: the next
/// rebuild mints a *different* pointer, so that entry can never be looked up
/// again. Left un-pruned it's a pure leak — one stale `Mutable` per hot-plug
/// cycle for every connector-less monitor.
///
/// Clear runs *before* the prune, and that ordering is load-bearing. #618
/// proposed the reverse — prune, then clear the survivors, to save a wake on
/// entries about to be dropped — but in `futures-signals` 0.3.34 that strands
/// subscribers on the stale `true` permanently:
///
/// * A signal's `MutableSignalState` holds a strong `Arc` to the shared state,
///   but is **not** counted in `senders` — only live `Mutable` handles are.
///   Dropping the *last* sender notifies with `has_changed = false` and then
///   wipes the waker list, while `poll_change` reads
///   `if is_changed() { … } else if senders == 0 { Poll::Ready(None) }` — a
///   permanent end-of-stream.
/// * `close_all`'s `PANELS` drain has already dropped every
///   `ModalPanel::open_state` before this runs, so pruning an entry straight
///   off the map — #618's rejected ordering, and the shape this function had
///   before #631 — would drop that key's *only remaining* sender while its
///   value was still `true`, unrecoverably: a `set_neq` loop afterwards can no
///   longer reach an entry already out of the map, and its wakers are gone
///   regardless. Clearing first latches `changed` before the senders reach
///   zero, so the last poll delivers `false` and *then* the stream ends.
///   That's still why clear has to run before prune.
///
/// **#631 changed *where* the last-sender drop happens, not whether clear
/// precedes prune.** The implementation below clones every `Mutable` into a
/// local `states` `Vec` before touching any of them (to get `set_neq` off the
/// borrowed map — see the function's own comment). That clone is a second
/// sender for the rest of the function, so the map's copy is never the last
/// survivor: `retain` dropping a pruned key's map-entry is now an ordinary
/// decrement, and the real last-sender drop happens only when `states` itself
/// goes out of scope at the end of the function — after both the reset and
/// the prune, with no `DRAWER_OPEN` borrow held at all. That's strictly safer
/// than the pre-#631 shape, whose last-sender drop happened *at* the `retain`
/// call, inside the active `borrow_mut()`.
///
/// The fallback keys this prunes do have a live subscriber, so this is not
/// hypothetical: `main.rs` binds the bar's `drawer-open` class for every
/// monitor, and it's only `overlays::osd::install` that skips the unnamed ones.
/// It goes unobserved today purely because that bar window is being destroyed
/// anyway — not because nobody is listening.
fn reset_drawer_open_states() {
    // Collect the handles before calling `set_neq` on any of them (#631):
    // `set_neq` notifies via `Waker::wake()`, and holding `DRAWER_OPEN`
    // borrowed across that call is a latent reentrancy hazard if a woken
    // subscriber ever runs synchronously and turns around to call
    // `drawer_open_state`/`drawer_open_signal` — both of which also borrow
    // `DRAWER_OPEN`. `Mutable` is a cheap cloneable handle, so this is just
    // moving where the clone happens, not adding real cost.
    let states: Vec<Mutable<bool>> =
        DRAWER_OPEN.with(|map| map.borrow().values().cloned().collect());
    for state in &states {
        state.set_neq(false);
    }
    DRAWER_OPEN.with(|map| map.borrow_mut().retain(|key, _| !is_fallback_key(key)));
}

/// Swap every currently-open panel's visible page to `target`. Drawer pages
/// are built once, monitor-agnostically, so an in-page deep-link callback
/// (e.g. a Settings row → Wallpaper) doesn't have a handy `&Monitor`. This
/// helper walks the per-monitor panel set and:
/// - For each panel currently showing a page (`current.is_some()`), swaps
///   the stack child to `target` (crossfade + height) without retracting.
/// - For closed panels, does nothing — we only switch what's actually open.
///
/// If no drawer is open on *any* monitor, the deep-link falls back to
/// **opening** `target` on niri's focused output rather than doing nothing
/// (#799). The v1 behaviour here was a documented no-op — fine while the only
/// caller was [`crate::components::deep_link_row`], a row that by construction
/// lives inside an already-open drawer, but wrong for every monitor-less caller
/// the command surface keeps growing (#219): a silent no-op is indistinguishable
/// from a broken keybind, so it gets reported against the binding rather than
/// against this.
///
/// **Why niri's focused output, and not a "primary" monitor.** The issue framed
/// the choice as primary-vs-pointer-focus; the codebase settles it. GTK4 has no
/// primary monitor to ask for — `gdk_display_get_primary_monitor` did not
/// survive GTK3 — so "primary" would have to be *invented* here, as the first
/// entry of `app.monitors()` or the first key of [`PANELS`] (i.e. `HashMap`
/// iteration order). Both are arbitrary and neither is stable across a hot-plug.
/// niri, by contrast, *tells us* which output is focused, and
/// [`crate::components::focused_output`] is already the shell's single cache of
/// it — the same routing every other monitor-less entry point uses (`commands`'
/// `open-page`/`power-menu` verbs, `plugins::effects`' `Effect::OpenPage`, the
/// OSD and the notification toasts). Pointer focus is the weaker tie-break for
/// this caller: it needs its own GDK tracking, and it disagrees with keyboard
/// focus exactly when the user has parked the mouse on a screen they are *not*
/// working on — the wrong answer for a keybind-driven verb.
///
/// The fallback inherits [`open_on_focused`]'s own degradation: an unknown
/// focused output (niri startup, no focused workspace, or the cache never
/// installed) or one with no drawer mounted still opens *a* mounted drawer, so
/// the deep-link lands somewhere instead of going quiet again. It is a genuine
/// no-op only when no drawer is mounted anywhere — i.e. no bars exist at all.
pub fn switch_active(target: Page) {
    // Snapshot the handles first, then act with no `PANELS` borrow live (#643):
    // `set_stack_page` and `on_page_show` are runs of GTK calls, and holding the
    // shared borrow across them means any synchronous emission that re-enters
    // `PANELS` — `close_all`'s and `install`'s `borrow_mut()` are the
    // counterparties — panics, fatally, from inside a glib callback. Cloning an
    // `Rc` per open drawer is a refcount bump.
    let mut switched = false;
    for panel in live_panels() {
        if panel.current.borrow().is_some() {
            set_stack_page(&panel, target);
            *panel.current.borrow_mut() = Some(Active::Builtin(target));
            on_page_show(&panel, target);
            switched = true;
        }
    }
    recompute_gates();
    if switched {
        return;
    }
    // Nothing was open to jump within → open `target` instead. `open_by_key`
    // recomputes the gates itself once the page is showing, so the call above
    // stays exactly where it was for the common (switched) path rather than
    // moving into a branch.
    let focused = crate::components::focused_output::current();
    open_on_focused(focused.as_deref(), target);
}

/// Begin the retract animation on every open drawer. Used by drawer-content
/// callbacks (e.g. the power-menu action rows) that don't carry a monitor
/// handle but want the drawer to close after their action fires.
pub fn dismiss_all() {
    // Snapshot before revealing (#643): `set_reveal_child(false)` starts the
    // retract, and `wire_retract_finish`'s `child-revealed` handler — which
    // reads `PANELS` itself — is what finishes it.
    for panel in live_panels() {
        panel.revealer.set_reveal_child(false);
    }
}

/// Begin the retract animation. The notify-child-revealed handler finishes
/// the teardown (hiding the single surface, catcher and card together) when it
/// ends.
fn retract_by_key(key: &str) {
    // Bind-then-act (#643): an `if let Some(panel) = panels.borrow().get(key)`
    // keeps the scrutinee's `Ref` alive for the whole then-block, i.e. across
    // `set_reveal_child` — whose completion handler re-reads `PANELS`.
    if let Some(panel) = live_panel(key) {
        panel.revealer.set_reveal_child(false);
    }
}

pub fn open(monitor: &Monitor, page: Page) {
    open_by_key(&monitor_key(monitor), page);
}

/// Open the drawer to `page` on the monitor whose connector is `key`. Shared
/// by [`open`] (the monitor-driven notification/toast path) and the
/// command-surface helpers below. No bar-chip context here, so the drawer
/// anchors flush with the bar's trailing main-axis edge (`anchor` = `None`,
/// main margin = 0). No-op if no drawer is mounted for `key`.
fn open_by_key(key: &str, page: Page) {
    // Resolve to an owned handle first (#643): `show_panel` presents the
    // surface and reveals the card — a long run of GTK calls, plus an
    // `open_state.set(true)` that wakes subscribers synchronously — none of
    // which may run under a live `PANELS` borrow.
    if let Some(panel) = live_panel(key) {
        *panel.anchor.borrow_mut() = None;
        show_panel(&panel, page);
    }
    recompute_gates();
}

/// Command-surface entry point (no `&Monitor` in hand): open `page` on the
/// `preferred` connector if a drawer is mounted there, else on any mounted
/// drawer. Backs the `open-page` / `power-menu` `GActions` driven by niri
/// keybinds — `preferred` is niri's focused output. Falls back to any panel so
/// an unknown/absent focused output still opens *a* drawer rather than
/// silently no-op'ing. No-op only when no drawers are mounted at all.
pub fn open_on_focused(preferred: Option<&str>, page: Page) {
    let key = PANELS.with(|panels| {
        let panels = panels.borrow();
        preferred
            .filter(|k| panels.contains_key(*k))
            .map(str::to_string)
            .or_else(|| panels.keys().next().cloned())
    });
    if let Some(key) = key {
        open_by_key(&key, page);
    }
}

/// Open the drawer to a plugin's **own** panel on the monitor whose connector is
/// `key` (#349 PR2). The plugin analogue of [`open_by_key`]: no bar-chip context,
/// so the drawer anchors flush with the bar's trailing edge (`anchor` = `None`,
/// main margin = 0). The active selection is published *before*
/// presenting so the per-monitor plugin drawer child reconciles the panel tree as
/// early as possible (risk #1 in the PR: measure-before-show). No-op if no drawer
/// is mounted for `key`.
fn open_plugin_by_key(key: &str, plugin_id: &str) {
    // Owned handle, no live `PANELS` borrow (#643) — as in [`open_by_key`],
    // plus `set_active_panel`, which publishes into the plugin host.
    if let Some(panel) = live_panel(key) {
        *panel.anchor.borrow_mut() = None;
        // Publish the selection BEFORE presenting so the child reconciles first
        // (`show_panel_active` also publishes it via `on_active_show`; idempotent).
        crate::plugins::set_active_panel(Some(plugin_id));
        show_panel_active(&panel, Active::Plugin(plugin_id.to_owned()));
    }
    recompute_gates();
}

/// Command-surface analogue of [`open_on_focused`] for a plugin's own panel
/// (#349 PR2): open `plugin_id`'s panel on the `preferred` connector if a drawer
/// is mounted there, else on any mounted drawer. Called from the plugin effect
/// broker when a plugin emits `Effect::OpenPage(Page::PluginSelf)`. No-op only
/// when no drawers are mounted at all.
pub fn open_plugin_on_focused(preferred: Option<&str>, plugin_id: &str) {
    let key = PANELS.with(|panels| {
        let panels = panels.borrow();
        preferred
            .filter(|k| panels.contains_key(*k))
            .map(str::to_string)
            .or_else(|| panels.keys().next().cloned())
    });
    if let Some(key) = key {
        open_plugin_by_key(&key, plugin_id);
    }
}

/// Toggle the drawer on `monitor` to the given `page`, centering the drawer
/// card under `trigger` along the bar's main axis:
/// - Same page open → start retract.
/// - Different page open → swap stack child in place (crossfade + size);
///   the drawer surface keeps its existing position.
/// - Closed → reposition under `trigger`, present surface, reveal.
pub fn toggle(monitor: &Monitor, page: Page, trigger: &impl IsA<gtk::Widget>) {
    toggle_inner(monitor, page, trigger, true);
}

/// Like [`toggle`], but never retracts when `page` is already the drawer's
/// visible page — it re-runs the on-show hook instead. Used by chips that
/// route into a page shared with other triggers (#516: the five combined-
/// Stats resource chips all pass `Page::Stats`), so a click from a
/// *different* chip while that shared page is already open jumps within it
/// (`on_page_show`'s `Page::Stats` arm re-applies the pending scroll target)
/// rather than closing the drawer. Re-clicking the *same* chip lands here
/// too — it just re-applies the same target, which is a harmless no-op.
pub fn toggle_keep_open(monitor: &Monitor, page: Page, trigger: &impl IsA<gtk::Widget>) {
    toggle_inner(monitor, page, trigger, false);
}

/// Shared implementation behind [`toggle`]/[`toggle_keep_open`]; `retract_on_same`
/// picks which of the two the "page already open" branch takes.
fn toggle_inner(
    monitor: &Monitor,
    page: Page,
    trigger: &impl IsA<gtk::Widget>,
    retract_on_same: bool,
) {
    let key = monitor_key(monitor);
    // Owned handle before any of the three arms run (#643): every one of them
    // drives GTK — `set_reveal_child`, `set_stack_page`, `on_page_show`,
    // `trigger_center`'s measure, `show_panel` — and the `Ref` a
    // `panels.borrow().get(&key)` produces would be live across all of it. This
    // is the busiest entry point in the module (every bar chip lands here), so
    // it is also the one most worth not making conditional on whether GTK
    // happens to emit synchronously today.
    if let Some(panel) = live_panel(&key) {
        let current = panel.current.borrow().clone();
        match current {
            // Same built-in page already open → retract (`toggle`) or re-run
            // the on-show hook in place (`toggle_keep_open`). A plugin panel
            // showing is never the same as a built-in `page`, so it falls to
            // the swap arm.
            Some(Active::Builtin(p)) if p == page => {
                if retract_on_same {
                    panel.revealer.set_reveal_child(false);
                } else {
                    on_page_show(&panel, page);
                }
            }
            Some(_) => {
                set_stack_page(&panel, page);
                *panel.current.borrow_mut() = Some(Active::Builtin(page));
                on_page_show(&panel, page);
            }
            None => {
                // Build + set the visible child first so `measure` reflects the
                // target page's natural size, not whatever was last shown, and
                // so `show_panel`'s margin measure below sees the real page.
                // `show_panel` re-sets it (idempotent).
                set_stack_page(&panel, page);
                // Record the chip, then let `show_panel` place the card off it.
                // The placement is best-effort at this point (a pre-map measure
                // can underestimate the card); the window-map handler recomputes
                // from the real allocation once the surface is mapped.
                set_anchor(&panel, trigger.upcast_ref());
                show_panel(&panel, page);
            }
        }
    }
    // Swap/open may have changed which page is visible; the same-page retract
    // branch is recomputed later by `wire_retract_finish`. Idempotent.
    recompute_gates();
}

/// Present the drawer on `page`, positioned off whatever [`ModalPanel::anchor`]
/// currently holds (set by the caller — a chip for [`toggle_inner`], `None` for
/// the chipless entry points, which land the card flush with the bar's trailing
/// main-axis edge).
fn show_panel(panel: &ModalPanel, page: Page) {
    show_panel_active(panel, Active::Builtin(page));
}

/// The generalized present path (#349 PR2): show `active` — a built-in [`Page`]
/// or a plugin panel. Identical to the old `show_panel` except it routes through
/// [`set_stack_active`]/[`on_active_show`], so it needs only `stack_name` +
/// on-show + margins, none of which are `Page`-specific.
fn show_panel_active(panel: &ModalPanel, active: Active) {
    set_stack_active(panel, &active);
    on_active_show(panel, &active);
    *panel.current.borrow_mut() = Some(active);
    panel.open_state.set(true);

    reposition_card(panel);

    panel.window.set_visible(true);
    panel.window.present();
    panel.revealer.set_reveal_child(true);
}

/// Place the card for the drawer's current [`ModalPanel::anchor`], from live
/// geometry only. The single positioning path: [`show_panel_active`] runs it at
/// open, [`wire_recenter_on_map`] re-runs it once the surface has a real
/// allocation, and [`wire_recenter_on_bar_geometry`] re-runs it whenever the bar
/// surface is reconfigured under an open drawer (#612).
///
/// Both margins sink onto the `positioner` box (the fullscreen surface no longer
/// moves) — because the overlay is fullscreen, a GTK margin on the
/// perpendicular+trailing-aligned positioner is equivalent to the old
/// layer-shell `set_margin` on the content-sized drawer surface.
///
/// * Perpendicular: bar offset + live-measured thickness, replacing the old
///   hardcoded 59. The bar window is mapped by open time.
/// * Main axis: [`main_margin_for_center`] off the anchor's *re-measured*
///   center, or 0 (flush with the bar's trailing edge) with no anchor.
///
/// Nothing here is cached between calls, which is the whole point: every input
/// — bar extent, bar thickness, chip position, card allocation — is re-read, so
/// repeating the call is how a geometry change is absorbed.
fn reposition_card(panel: &ModalPanel) {
    set_widget_margin(
        &panel.positioner,
        panel.geometry.perpendicular_layer_edge(),
        panel.geometry.perpendicular_margin(),
    );
    set_widget_margin(
        &panel.positioner,
        panel.geometry.main_layer_edge(),
        live_main_margin(panel),
    );
}

/// The main-axis margin the card should carry right now: solved from the anchor
/// chip's live center, or 0 when the drawer has no anchor at all (the
/// `open_by_key` path, where the card sits flush with the bar's trailing edge).
///
/// Extracted so [`reposition_card`] and [`apply_stats_max_height`] share one
/// expression (#793 item 2). For a Left/Right bar this margin is applied to
/// `Bottom`, which makes it a height reservation as well as a placement — and a
/// height budget solved from a *different* number than the placement uses is
/// precisely the bug that shows up as a card overflowing the screen.
fn live_main_margin(panel: &ModalPanel) -> i32 {
    live_center(panel).map_or(0, |center| main_margin_for_center(panel, center))
}

/// Record `trigger` as the chip this drawer is centered on. `None` (clearing any
/// previous anchor) if the chip isn't rooted, which preserves the pre-#612
/// behaviour of that case: no center, so the card lands flush with the bar's
/// trailing edge rather than being centered on a made-up coordinate.
fn set_anchor(panel: &ModalPanel, trigger: &gtk::Widget) {
    // Measure before borrowing (#643) — `trigger_center` is a run of GTK calls.
    let anchor = trigger_center(panel, trigger).map(|center| TriggerAnchor {
        widget: trigger.downgrade(),
        center,
    });
    *panel.anchor.borrow_mut() = anchor;
}

/// The anchor chip's main-axis center, **re-measured against the live bar**, or
/// `None` when the drawer has no anchor at all.
///
/// The re-measure is what #612 turns on: [`trigger_center`] returns a
/// bar-surface-relative coordinate, and the bar surface moves and resizes under
/// an open drawer whenever something reserves leading main-axis space (the
/// sidebar's exclusive zone). A chip pinned to the bar's *trailing* end doesn't
/// move on screen when that happens, but its bar-relative center does — so
/// re-solving the old number against the new extent would slide the card off the
/// chip in the opposite direction. Re-measuring both halves in the same pass is
/// the only combination that holds for leading-, center- and trailing-placed
/// chips alike.
///
/// Falls back to the recorded center if the chip is gone or unrooted, and
/// refreshes the record on every successful measure.
fn live_center(panel: &ModalPanel) -> Option<i32> {
    // Copy both halves out before touching GTK (#643): `trigger_center` is a run
    // of GTK calls and the refresh below needs the cell mutably, so neither may
    // run under a borrow taken for the other.
    let (trigger, recorded) = {
        let anchor = panel.anchor.borrow();
        let anchor = anchor.as_ref()?;
        (anchor.widget.upgrade(), anchor.center)
    };
    let measured = trigger.and_then(|widget| trigger_center(panel, &widget));
    if let Some(center) = measured
        && let Some(anchor) = panel.anchor.borrow_mut().as_mut()
    {
        anchor.center = center;
    }
    Some(measured.unwrap_or(recorded))
}

/// Set a GTK margin on `widget` corresponding to a layer-shell [`LayerEdge`].
/// The shell runs LTR, so `Left`→`margin_start` and `Right`→`margin_end`. Used
/// to position the `.ts-modal` positioner within the fullscreen overlay the
/// same way `set_margin` positioned the old content-sized drawer surface.
fn set_widget_margin(widget: &impl IsA<gtk::Widget>, edge: LayerEdge, margin: i32) {
    // `LayerEdge` (gtk4_layer_shell's `Edge`) is `#[non_exhaustive]`, so a
    // wildcard arm is required even though the four screen edges are the only
    // meaningful values. `Right` maps to `margin_end` (LTR) via that wildcard.
    match edge {
        LayerEdge::Top => widget.set_margin_top(margin),
        LayerEdge::Bottom => widget.set_margin_bottom(margin),
        LayerEdge::Left => widget.set_margin_start(margin),
        _ => widget.set_margin_end(margin),
    }
}

/// Recompute the main-axis margin from the *real* card allocation once the
/// surface is mapped. `measure` can return 0 before the surface is realized
/// (per the original centering docstring), flooring the drawer width and
/// shifting the card. `connect_map` fires *before* the first size-allocate,
/// so the recompute is deferred to the next main-loop idle — by then the card
/// has a real allocation (`card.width()`/`card.height()`, which include the
/// card's borders and padding). Fires on every map.
///
/// Also (#516) re-applies any pending Stats-page scroll target here. A fresh
/// drawer open runs [`on_page_show`] *before* `window.present()` (see
/// [`show_panel_active`]), so that first attempt may fire against an
/// allocation that isn't settled yet; re-triggering it once the window is
/// genuinely mapped guarantees a correct final position regardless — the
/// same settled-frame guarantee the margin recompute below needs. Both
/// triggers land on the same idempotent idle-deferred computation
/// (`panels::stats`'s own `"stats.scroll"` action handler), so firing twice
/// is harmless.
fn wire_recenter_on_map(window: &gtk::Window, key: String) {
    window.connect_map(move |_| {
        let key = key.clone();
        glib::idle_add_local_once(move || {
            // Owned handle, no live `PANELS` borrow (#643): `apply_stats_scroll`
            // activates a `GAction`, and `main_margin_for_center` measures the
            // card — both GTK, both under a shared borrow before this.
            let Some(panel) = live_panel(&key) else {
                return;
            };
            apply_stats_scroll(&panel);
            reposition_card(&panel);
        });
    });
}

/// Re-place the card when the **bar's** surface geometry changes underneath an
/// already-open drawer (#612, the deferred half of #604).
///
/// The drawer's own surface can't notice: it is anchored to all four screen
/// edges with `exclusive_zone(-1)` precisely so nothing reserving space moves
/// it, so it is never reconfigured. The bar is the surface that *does* move —
/// [`BarGeometry::main_extent`] already leans on that, reading the bar's live
/// extent so the centering math self-corrects for whatever reserves leading
/// space (#600). This is the same observation applied one step earlier: watch
/// that extent for *changes*, not just sample it at open.
///
/// Reachable in practice only through a non-pointer trigger — the drawer's
/// fullscreen click-catcher retracts on any click, so no ordinary pointer path
/// toggles the sidebar with the drawer still up. `commands.rs`'s
/// `toggle-sidebar` `GAction` on a niri keybind is that trigger, and it is why
/// this is wired at all rather than left as a shrug.
///
/// **Deliberately not a sidebar subscription.** #604 avoided a `modal` →
/// sidebar coupling and #612 asked that the fix keep avoiding it; hooking the
/// bar's own `GdkSurface::layout` keeps `modal` ignorant of *which* surface
/// reserved space, exactly as `main_extent` is. Any future leading-edge
/// reserver gets the same treatment for free.
///
/// Mechanics, since the signal choice is load-bearing:
/// * `GdkSurface::layout` is emitted from the frame clock's layout phase, and
///   for a Wayland toplevel the backend's `compute_size` always returns FALSE,
///   so the emission is never skipped — a compositor configure (which is what an
///   exclusive-zone change delivers) always reaches it. `GdkSurface::width` is
///   *not* a notifying property, so `notify::width` would silently never fire.
/// * `GtkNative` connects its own `layout` handler at realize, i.e. before this
///   one, so GTK has already re-allocated the bar by the time we run. The idle
///   deferral is belt-and-braces on top of that (and mirrors
///   [`wire_recenter_on_map`], which needs it for the same settled-frame
///   reason).
/// * The signal also fires for plain relayouts at unchanged size (a clock label
///   growing a digit), so the reported size is diffed and unchanged sizes cost
///   one integer compare.
fn wire_recenter_on_bar_geometry(bar_window: &gtk::Window, key: String) {
    // `on_surface_ready` runs on every map, and the bar's layer surface maps
    // exactly once — but latch anyway rather than rely on that, since a second
    // run would stack a second `layout` handler on the same surface.
    let wired = Cell::new(false);
    let last_size = Rc::new(Cell::new((0, 0)));
    on_surface_ready(bar_window, move |surface| {
        if wired.replace(true) {
            return;
        }
        let key = key.clone();
        let last_size = Rc::clone(&last_size);
        surface.connect_layout(move |_, width, height| {
            if last_size.replace((width, height)) == (width, height) {
                return;
            }
            let key = key.clone();
            glib::idle_add_local_once(move || {
                // Owned handle, no live `PANELS` borrow (#643) — `reposition_card`
                // measures the card and the chip.
                let Some(panel) = live_panel(&key) else {
                    return;
                };
                // Read-then-act, no `current` borrow held across the GTK below.
                // A closed drawer needs nothing: the next open positions from
                // scratch off the live geometry anyway.
                let is_open = panel.current.borrow().is_some();
                if is_open {
                    reposition_card(&panel);
                }
            });
        });
    });
}

/// The trigger chip's center along the bar's *main axis*, **relative to the
/// bar's own layer surface** (the chip's root): X for horizontal (Top/Bottom)
/// bars, Y for vertical (Left/Right) bars. `None` if the chip isn't rooted yet
/// (shouldn't happen for a mapped bar widget).
///
/// This is *not* a screen coordinate, and the two only coincide while the bar
/// surface starts at the screen origin. An exclusive zone on the bar's leading
/// main-axis edge (an open sidebar) shifts the surface, and the doc comment
/// here used to claim "screen coordinates" — which is what hid #600: the
/// margin math solved this bar-relative point against the full monitor width.
/// Whatever consumes this must pair it with [`BarGeometry::main_extent`], the
/// extent of the same surface, never `monitor.size()`.
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
/// transparent surface) centers under `center` — the chip midpoint from
/// [`trigger_center`], a point in the **bar surface's** coordinate space.
///
/// The card is inset from the trailing surface edge by `chrome_main_end`
/// (`.ts-modal` padding + `.ts-drawer` margin on that side), so the card's
/// trailing edge sits at `main_extent - main_margin - chrome_main_end` and
/// its center at `main_extent - main_margin - chrome_main_end -
/// card_extent/2`. Solving `card_center == center`:
///
/// ```text
/// main_margin = main_extent - center - chrome_main_end - card_extent/2
/// ```
///
/// where `main_extent` is [`BarGeometry::main_extent`] — the bar surface's live
/// main-axis size, i.e. the extent of the very space `center` is measured in.
/// It used to be the monitor's size, which is #600: with an open sidebar
/// reserving 320 px on the left, the bar surface starts at x=320 and a chip at
/// screen x=420 reports `center = 100`, so solving against 1920 instead of 1600
/// landed the card 320 px left of its chip. Both terms are **re-read here**,
/// never snapshotted at install, so a resolution/mode switch (kanshi profile
/// change) that resizes the output without a hot-plug can't leave the clamp
/// stale (#442). `card_extent` is the card's real allocated size on the main
/// axis (its border box; CSS borders + padding included). Clamped to
/// `[0, main_extent - card_footprint]` so the card can't fall off either end —
/// near the trailing/leading edge it collapses to flush.
fn main_margin_for_center(panel: &ModalPanel, center: i32) -> i32 {
    let geometry = &panel.geometry;
    // Live bar-surface extent (monitor size as fallback), not a captured
    // snapshot (#442) — and the bar's, not the monitor's, so it matches the
    // space `center` was measured in (#600).
    let main_extent = geometry.main_extent();

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
    // Upper bound is the *wide* Stats clamp (#508), not `DRAWER_MAX_WIDTH`, so
    // the multicolumn Stats page (which `finish_page_clamped`s to
    // `DRAWER_MAX_WIDTH_WIDE`) still centers under its trigger chip instead of
    // being measured as if it were 680 wide. Every other page measures well
    // under 680, so widening the ceiling is a no-op for them — the clamp only
    // ever bites on a pathological over-request, and now that ceiling covers the
    // one page that legitimately reaches past 680.
    let card_extent = card_extent.clamp(scale(360), scale(DRAWER_MAX_WIDTH_WIDE));

    clamp_main_margin(
        main_extent,
        center,
        card_extent,
        geometry.chrome_main_start(),
        geometry.chrome_main_end(),
    )
}

/// Pure clamp arithmetic behind [`main_margin_for_center`], split out so the
/// centering math is unit-testable without a GTK surface. Given the live
/// `main_extent` (the bar surface's main-axis size, see
/// [`BarGeometry::main_extent`]), the trigger `center` *in that same space*,
/// the measured `card_extent`, and the leading/trailing chrome, returns the
/// main-axis margin that centers the card under `center`, clamped so the card
/// never falls off either end.
///
/// Isolating this makes both failure modes concrete. #442: for a fixed
/// center/card/chrome, a stale `main_extent` (the old resolution) yields a
/// different margin than the live one — exactly the misplacement a snapshotted
/// size caused after a mode switch. #600: feeding the *monitor's* extent while
/// `center` is bar-surface-relative displaces the card by whatever an open
/// sidebar reserves on the leading edge.
fn clamp_main_margin(
    main_extent: i32,
    center: i32,
    card_extent: i32,
    chrome_start: i32,
    chrome_end: i32,
) -> i32 {
    let card_footprint = card_extent + chrome_start + chrome_end;
    let desired = main_extent - center - chrome_end - card_extent / 2;
    let max = (main_extent - card_footprint).max(0);
    desired.clamp(0, max)
}

/// Pure arithmetic behind [`BarGeometry::available_card_height`] (#701), split
/// out so the budget is unit-testable without a live monitor or a mapped bar
/// surface — the same reason [`clamp_main_margin`] is split out.
///
/// `bar_reserved` is what the drawer's geometry takes off the vertical axis:
/// the bar's offset plus its measured thickness for a Top/Bottom bar, and the
/// card's main-axis centering margin for a Left/Right one — whose
/// [`BarGeometry::main_layer_edge`] is `Bottom`, making that margin a *height*
/// reservation rather than the zero this used to document (#793 item 2).
/// `chrome` is the card's own vertical margin total ([`CARD_CHROME_VERTICAL`]).
///
/// Floors at [`MIN_CARD_HEIGHT`] so degenerate inputs can't yield a nonsense
/// cap. Two of them are reachable: `Monitor::size` reports `(0, 0)` for a
/// monitor GDK hasn't configured yet, and a bar thick enough to swallow the
/// screen would otherwise hand `gtk::ScrolledWindow::set_max_content_height` a
/// zero or negative value (where negative means "no maximum" — the silent
/// opposite of what a shrinking screen should do). Below the floor a cap has
/// stopped being a useful safety net anyway, so this clamps *up*; the
/// fullscreen drawer surface clips whatever still doesn't fit, exactly as it
/// did before. Subtraction is saturating so a pathological `bar_reserved`
/// can't wrap in a debug build.
fn clamp_card_height(monitor_height: i32, bar_reserved: i32, chrome: i32) -> i32 {
    monitor_height
        .saturating_sub(bar_reserved)
        .saturating_sub(chrome)
        .max(MIN_CARD_HEIGHT)
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

/// Per-page side-effects that should run whenever a page becomes visible
/// (initial open OR cross-fade swap from another page). Add a match arm
/// here when a new page needs an on-show fetch.
fn on_page_show(panel: &ModalPanel, page: Page) {
    match page {
        Page::Clipboard => clipboard::refresh(),
        Page::Calendar => calendar::refresh(),
        // Opening the Notifications drawer = the user has seen them.
        // Dismiss all active toasts (move to history); the bell counter
        // bound to active.len() drops to zero.
        Page::Notifications => notifications::dismiss_all(),
        // #701 then #516, in that order. First re-derive the Stats page's
        // scroll cap from *this* monitor's live geometry — every show is the
        // point where a kanshi mode switch or a bar-thickness change would
        // have invalidated the last value. Then (re)land the page on whichever
        // resource chip's card is currently pending. Both fire on every
        // open/swap/re-show of the page — including the "already open"
        // `toggle_keep_open` path, which is exactly the case that needs it (the
        // stack's visible child doesn't change, so nothing else would trigger a
        // rescroll).
        //
        // What lets the scroll see the new cap is the *defer*, not this
        // ordering (#793 item 3). This comment used to claim the viewport "has
        // to be sized before the deep-link scroll decides where the target card
        // lands", which is not how GTK works: `set_max_content_height` only
        // queues a resize, so a synchronous `apply_stats_scroll` would read the
        // pre-cap layout no matter which of the two ran first. It is safe
        // because `panels::stats`'s `apply_scroll` defers a full main-loop idle
        // tick (`glib::idle_add_local_once`) plus one bounded retry (#542), by
        // which point the queued resize has been laid out. So: do not delete
        // that idle on the strength of this ordering. The ordering buys nothing
        // on its own, and dropping the defer silently breaks the deep link.
        Page::Stats => {
            apply_stats_max_height(panel);
            apply_stats_scroll(panel);
        }
        _ => {}
    }
}

/// Trigger `panel`'s built Stats page to re-apply its own pending scroll
/// target (#516). A no-op if the page was never opened on this monitor yet
/// (not in `panel.stack` — `gtk::Stack::child_by_name` returns `None`, e.g.
/// the drawer just opened straight to a different page).
///
/// Routes through `gtk::Widget::activate_action` on the page widget itself
/// rather than a monitor-keyed registry here: `panels::stats::panel_stats`
/// installs a `"stats"`-prefixed action group carrying a `"scroll"` action on
/// the widget it returns, so this only needs the `gtk::Widget` `crate::panels`
/// already handed back — no need for `modal.rs` to track per-monitor
/// Stats-page state of its own. This monitor's key rides along as the action's
/// string parameter (#542), since `panels::stats` keys its pending-scroll map
/// per monitor. An `Err` here would mean that action group somehow wasn't
/// installed, which shouldn't happen for the real Stats page; logged rather than
/// ignored so a future regression there isn't silent.
fn apply_stats_scroll(panel: &ModalPanel) {
    let Some(widget) = panel.stack.child_by_name(Page::Stats.stack_name()) else {
        return;
    };
    let key = monitor_key(&panel.geometry.monitor);
    if let Err(e) = widget.activate_action("stats.scroll", Some(&key.to_variant())) {
        tracing::debug!(error = %e, "modal: stats scroll action activation failed");
    }
}

/// Push this monitor's live [`BarGeometry::available_card_height`] into
/// `panel`'s built Stats page as its `ScrolledWindow` cap (#701). A no-op if
/// the page was never opened on this monitor yet, exactly like
/// [`apply_stats_scroll`].
///
/// Rides the same `"stats"` action group `apply_stats_scroll` uses, for the
/// same reason: `crate::panels::stats` builds its pages monitor-agnostically
/// and hands back only a `gtk::Widget`, so a per-monitor number has to travel
/// as an action parameter rather than a `build_page` argument. Doing it *here*
/// — on every show — is what keeps the cap correct across monitors, kanshi
/// mode switches and bar-thickness changes without anything having to
/// invalidate a cached value.
///
/// The parameter is plain logical pixels; the receiving side must **not** run
/// it through [`scale`]. That was the #701 bug in miniature — the old constant
/// was a design-baseline number that legitimately needed scaling, this one is
/// already measured on the real screen, and scaling it again would double-count
/// the font factor.
///
/// Refresh granularity is deliberately per-*show*, not per-frame: a geometry
/// change while the Stats drawer is already open (a kanshi mode switch mid-
/// drawer — [`wire_recenter_on_bar_geometry`] re-*places* the card there, but
/// does not re-push this) leaves the previous cap standing until the next
/// show. Bounded and self-healing, and the cap is a safety net rather than the
/// layout, so it isn't worth a second trigger; if that window ever matters,
/// the fix is one call to this from that handler's `is_open` branch.
fn apply_stats_max_height(panel: &ModalPanel) {
    let Some(widget) = panel.stack.child_by_name(Page::Stats.stack_name()) else {
        return;
    };
    // Only a Left/Right bar's main-axis margin lands on the vertical axis, and
    // `available_card_height` ignores this argument for a Top/Bottom bar — so
    // skip the solve there rather than pay `live_main_margin`'s GTK measure on
    // the one bar edge the shell actually ships (`main.rs`'s `BAR_EDGE`).
    let main_margin = if panel.geometry.horizontal() {
        0
    } else {
        live_main_margin(panel)
    };
    let height = panel.geometry.available_card_height(main_margin);
    if let Err(e) = widget.activate_action("stats.max-height", Some(&height.to_variant())) {
        tracing::debug!(error = %e, "modal: stats max-height action activation failed");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_CHROME_VERTICAL, DRAWER_OPEN, EAGER_PAGES, MIN_CARD_HEIGHT, Page, Signal,
        clamp_card_height, clamp_main_margin, close_all, drawer_open_state,
        reset_drawer_open_states,
    };
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    // These are pure-logic guards for the lazy drawer-page registry (#231).
    // The two properties that _cannot_ be regressed silently are covered by the
    // compiler, not a test:
    //   * `build_page` is an exhaustive `match Page { … }` with no wildcard, so
    //     adding a `Page` variant fails to compile until it has a build arm —
    //     a new page can never skip lazy registration.
    //   * `set_stack_active` is the only caller of `stack.set_visible_child_name`
    //     and always `ensure_page`s a built-in first (the plugin child is added
    //     eagerly and always exists), so no route reaches an unbuilt page.
    //     `set_stack_page` is a thin built-in wrapper over it (#349 PR2).
    // What's left to guard at runtime is the *stack-name keyspace* that
    // `ensure_page` uses with `gtk::Stack::child_by_name` to decide "already
    // built?". GTK-level idempotence of `ensure_page` needs a display server;
    // the trollshell binary has no `system-tests`/xvfb harness, so that stays
    // build- + live-verify-covered.

    /// Tripwire: growing the enum without updating `ALL` (and consciously
    /// revisiting the lazy registry that keys off `stack_name`) should trip
    /// here. `ALL` backs the `from_stack_name` reverse lookup used by
    /// deep-links and the niri command surface.
    #[test]
    fn all_has_stable_count() {
        // 15 core pages + the 5 per-resource `Stats*` split variants (#508
        // restored #307's split alongside the combined page).
        assert_eq!(Page::ALL.len(), 20);
    }

    /// Each page's `stack_name` is the key `ensure_page` hands to
    /// `child_by_name`. If two pages shared a token, the lazy path would treat
    /// the second as already built and never construct it. Guard every token is
    /// distinct and non-empty.
    #[test]
    fn stack_names_are_unique_and_nonempty() {
        let mut seen = HashSet::new();
        for page in Page::ALL {
            let name = page.stack_name();
            assert!(!name.is_empty(), "{page:?} has an empty stack name");
            assert!(seen.insert(name), "duplicate stack name {name:?}");
        }
        assert_eq!(seen.len(), Page::ALL.len());
    }

    /// `stack_name` ⇄ `from_stack_name` round-trips for every page, so the
    /// reverse lookup resolves exactly the pages the registry can build and
    /// `ALL` stays consistent with the `stack_name` mapping.
    #[test]
    fn stack_name_round_trips() {
        for page in Page::ALL {
            assert_eq!(Page::from_stack_name(page.stack_name()), Some(page));
        }
    }

    /// An unknown token resolves to `None` — no accidental catch-all that would
    /// map a bad deep-link to some default page.
    #[test]
    fn from_stack_name_rejects_unknown() {
        assert_eq!(Page::from_stack_name("does-not-exist"), None);
    }

    /// Centering a mid-screen chip: the card's center lands on `center`. With
    /// `chrome_end` inset, the margin is `screen - center - chrome_end -
    /// card/2`, well inside the clamp bounds.
    #[test]
    fn clamp_main_margin_centers_mid_screen() {
        // 1920 wide, chip centered at 960, 680-wide card, 20/15 chrome.
        // desired = 1920 - 960 - 15 - 340 = 605; footprint = 680+20+15 = 715;
        // max = 1920 - 715 = 1205; 605 is within [0, 1205].
        assert_eq!(clamp_main_margin(1920, 960, 680, 20, 15), 605);
    }

    /// The card can't fall off the trailing edge: a chip near the leading
    /// screen edge would want a margin larger than `max`, so it clamps flush.
    #[test]
    fn clamp_main_margin_clamps_to_trailing_edge() {
        // center = 0 → desired = 1920 - 0 - 15 - 340 = 1565, but
        // max = 1920 - 715 = 1205, so it clamps down to 1205.
        assert_eq!(clamp_main_margin(1920, 0, 680, 20, 15), 1205);
    }

    /// The card can't fall off the leading edge: a chip past the trailing edge
    /// would want a negative margin, so it clamps to 0 (flush).
    #[test]
    fn clamp_main_margin_clamps_to_leading_edge() {
        // center = 1920 → desired = 1920 - 1920 - 15 - 340 = -355 → 0.
        assert_eq!(clamp_main_margin(1920, 1920, 680, 20, 15), 0);
    }

    /// The #442 crux: a stale `main_extent` (the pre-mode-switch resolution)
    /// yields a *different* margin than the live one for the same chip and card
    /// — which is exactly the misplacement a size snapshotted at install caused
    /// after a kanshi mode switch. Re-reading it live on every call fixes it;
    /// #600 only changed *which* live surface it is read from (the bar's, which
    /// the compositor reconfigures on a mode switch just like the monitor),
    /// never the liveness itself.
    #[test]
    fn clamp_main_margin_tracks_screen_extent() {
        // Same chip (center = 960) and card (680, chrome 20/15) on two screen
        // widths. The old (1920) and new (2560) resolutions place the card's
        // trailing margin differently — proving the value flows from the live
        // `main_extent`, so a stale one misplaces the card.
        let at_1920 = clamp_main_margin(1920, 960, 680, 20, 15);
        let at_2560 = clamp_main_margin(2560, 960, 680, 20, 15);
        assert_ne!(at_1920, at_2560);
        // +640 wider screen shifts the margin from the trailing edge by +640.
        assert_eq!(at_2560 - at_1920, 640);
    }

    /// #600: with the sidebar open, `center` is measured inside a bar surface
    /// that the 320 px leading exclusive zone has both shifted and narrowed.
    /// Solving against the *bar's* extent keeps the card under the chip's real
    /// screen position; solving against the monitor's (the old behaviour) puts
    /// it a full sidebar width to the left.
    #[test]
    fn clamp_main_margin_accounts_for_leading_reservation() {
        // 1920 monitor, sidebar reserving 320 on the left → the bar surface
        // spans [320, 1920), 1600 wide. A chip at screen x = 960 therefore
        // reports center = 640 from `trigger_center`.
        const MONITOR: i32 = 1920;
        const BAR: i32 = 1600;
        const CHIP_SCREEN_X: i32 = 960;
        const CENTER: i32 = CHIP_SCREEN_X - (MONITOR - BAR);
        // The card is laid out from the *screen's* trailing edge (the drawer
        // surface is fullscreen, `exclusive_zone(-1)`), so a margin `m` puts
        // the card's center here:
        let card_center = |m: i32| MONITOR - m - 15 - 680 / 2;

        assert_eq!(
            card_center(clamp_main_margin(BAR, CENTER, 680, 20, 15)),
            CHIP_SCREEN_X
        );
        // Pre-fix: the monitor's extent against a bar-relative center.
        assert_eq!(
            card_center(clamp_main_margin(MONITOR, CENTER, 680, 20, 15)),
            CHIP_SCREEN_X - 320
        );
    }

    /// The sidebar-*closed* common case must not regress: with nothing
    /// reserving leading space the bar surface spans the whole monitor, so
    /// `main_extent` is the monitor extent and the card centers under the chip
    /// for every position the clamp doesn't bite at.
    #[test]
    fn clamp_main_margin_centers_without_reservation() {
        const MONITOR: i32 = 1920;
        const MAX: i32 = MONITOR - (680 + 20 + 15);
        for center in [400, 640, 960, 1200, 1500] {
            let margin = clamp_main_margin(MONITOR, center, 680, 20, 15);
            assert!(margin > 0 && margin < MAX, "clamp bit at center {center}");
            assert_eq!(MONITOR - margin - 15 - 680 / 2, center);
        }
    }

    /// Screen-space center of the card for a given main-axis margin, at the
    /// 1920 px / 680 px card / 15 px trailing-chrome geometry the two #612 tests
    /// below share. The card is laid out from the *screen's* trailing edge (the
    /// drawer surface is fullscreen with `exclusive_zone(-1)`), so this is what
    /// a margin actually means on screen.
    fn card_center_at(margin: i32) -> i32 {
        1920 - margin - 15 - 680 / 2
    }

    /// #612, the trailing-chip case — the one a "just re-run the math with the
    /// new extent" fix gets wrong.
    ///
    /// The drawer can stay open across a sidebar toggle (only via the
    /// `toggle-sidebar` keybind: the fullscreen click-catcher eats every pointer
    /// path), so the card has to be re-placed. The crux is *which* inputs get
    /// refreshed. A chip pinned near the bar's trailing end does not move on
    /// screen when a 320 px leading zone shifts and narrows the bar — but its
    /// bar-relative center, which is what `trigger_center` reports, drops by
    /// exactly that 320. Re-solving the *recorded* center against the *new*
    /// extent therefore moves a card that should have stayed put; only
    /// re-measuring the chip too (what `live_center` does) holds.
    #[test]
    fn recenter_on_zone_change_remeasures_the_trigger() {
        const MONITOR: i32 = 1920;
        const SIDEBAR: i32 = 320;
        const BAR_OPEN: i32 = MONITOR - SIDEBAR;
        // 420 px in from the screen's right edge, in both sidebar states.
        const CHIP_SCREEN_X: i32 = MONITOR - 420;

        // Sidebar closed: the bar spans the monitor, so bar-relative == screen.
        let before = clamp_main_margin(MONITOR, CHIP_SCREEN_X, 680, 20, 15);
        assert_eq!(card_center_at(before), CHIP_SCREEN_X);

        // Sidebar open: bar surface is [320, 1920), so the unmoved chip now
        // reports a center 320 lower. Same card, same screen position.
        let remeasured = clamp_main_margin(BAR_OPEN, CHIP_SCREEN_X - SIDEBAR, 680, 20, 15);
        assert_eq!(card_center_at(remeasured), CHIP_SCREEN_X);
        assert_eq!(
            remeasured, before,
            "the card must not move when the chip didn't"
        );

        // The half-fix: fresh extent, stale center. Lands 320 px of chip
        // displacement onto the card — here clamped flush, 65 px off the chip.
        let stale = clamp_main_margin(BAR_OPEN, CHIP_SCREEN_X, 680, 20, 15);
        assert_ne!(stale, before);
        assert_ne!(card_center_at(stale), CHIP_SCREEN_X);
    }

    /// #612, the leading-chip case — the one that proves the *extent* still has
    /// to be re-read as well, so the fix is "re-measure both", not "re-measure
    /// the chip instead".
    ///
    /// A chip pinned near the bar's leading end keeps the same bar-relative
    /// center across the zone change (the bar's own layout doesn't move it) but
    /// slides 320 px right on screen with the surface. The card has to follow,
    /// and it is the narrowed extent that carries it there.
    #[test]
    fn recenter_on_zone_change_tracks_the_bar_extent() {
        const MONITOR: i32 = 1920;
        const SIDEBAR: i32 = 320;
        const BAR_OPEN: i32 = MONITOR - SIDEBAR;
        // 400 px in from the bar's leading edge, in both sidebar states.
        const CHIP_BAR_X: i32 = 400;

        let before = clamp_main_margin(MONITOR, CHIP_BAR_X, 680, 20, 15);
        assert_eq!(card_center_at(before), CHIP_BAR_X);

        let after = clamp_main_margin(BAR_OPEN, CHIP_BAR_X, 680, 20, 15);
        assert_eq!(card_center_at(after), CHIP_BAR_X + SIDEBAR);
        assert_eq!(
            after - before,
            -SIDEBAR,
            "the card must follow the chip's new screen position"
        );
    }

    /// The old hardcoded Stats scroll cap (`stats_scrolled(&grid, 560)`),
    /// kept here only as the number the #701 tests measure against. It is not
    /// referenced by any non-test code any more.
    const OLD_STATS_CAP: i32 = 560;

    /// A typical top bar: 0 offset, ~34 logical px thick. The bar's own
    /// `thickness()` is a live measure, so this stands in for it.
    const TOP_BAR: i32 = 34;

    /// #701, the headline case: on a real output the budget is the screen, not
    /// a constant. A 1440-tall panel gives the card ~1386 px — two and a half
    /// times the 560 the page used to clamp itself to, which is why the drawer
    /// scrolled permanently with the bottom half of the screen empty.
    #[test]
    fn clamp_card_height_uses_the_whole_screen() {
        assert_eq!(
            clamp_card_height(1440, TOP_BAR, CARD_CHROME_VERTICAL),
            1440 - TOP_BAR - CARD_CHROME_VERTICAL
        );
        assert!(clamp_card_height(1440, TOP_BAR, CARD_CHROME_VERTICAL) > OLD_STATS_CAP * 2);
    }

    /// Every output the shell plausibly runs on clears the old cap by a wide
    /// margin — including a 768-px netbook panel. That is the bug in one
    /// assertion: no supported screen ever *wanted* 560.
    #[test]
    fn clamp_card_height_beats_the_old_cap_on_every_real_output() {
        for height in [768, 800, 900, 1080, 1200, 1440, 1600, 2160] {
            let budget = clamp_card_height(height, TOP_BAR, CARD_CHROME_VERTICAL);
            assert!(
                budget > OLD_STATS_CAP,
                "{height}px output still capped below the old constant ({budget})"
            );
        }
    }

    /// The #442 liveness crux, restated for the height axis: a stale monitor
    /// height yields a different budget than the live one, so the value has to
    /// be re-read per show (which is what `apply_stats_max_height` does) rather
    /// than captured at `build_page` time. The budget tracks the screen 1:1.
    #[test]
    fn clamp_card_height_tracks_monitor_height() {
        let at_1080 = clamp_card_height(1080, TOP_BAR, CARD_CHROME_VERTICAL);
        let at_1440 = clamp_card_height(1440, TOP_BAR, CARD_CHROME_VERTICAL);
        assert_ne!(at_1080, at_1440);
        assert_eq!(at_1440 - at_1080, 360);
    }

    /// …and the bar's live thickness the same way: a fatter bar (a CSS change,
    /// or a bar inset from the screen edge) gives the card exactly that much
    /// less room.
    #[test]
    fn clamp_card_height_tracks_bar_thickness() {
        let thin = clamp_card_height(1440, 34, CARD_CHROME_VERTICAL);
        let thick = clamp_card_height(1440, 74, CARD_CHROME_VERTICAL);
        assert_eq!(thin - thick, 40);
    }

    /// A Left/Right bar's *thickness* is a width reservation and must never
    /// come off the height — subtracting the perpendicular margin on that axis
    /// too would silently short the budget by a bar's width. What *does* come
    /// off is the card's main-axis centering margin, because
    /// `BarGeometry::main_layer_edge` resolves to `Bottom` for a vertical bar.
    ///
    /// This test previously asserted only the `bar_reserved == 0` case and
    /// described it as the whole rule for a vertical bar, matching the shape
    /// `available_card_height` had. That is the number #793 item 2 objected to:
    /// it passes, and it stands as a green assertion that a left/right bar's
    /// card may use the full screen height — a cap that overflows by the
    /// centering margin for whoever first ships one.
    #[test]
    fn clamp_card_height_reserves_a_vertical_bars_main_margin_not_its_thickness() {
        // Unanchored drawer (the `open_by_key` path): margin 0, so the card
        // really does get the whole screen minus chrome. The one case the old
        // assertion was right about, kept.
        assert_eq!(
            clamp_card_height(1440, 0, CARD_CHROME_VERTICAL),
            1440 - CARD_CHROME_VERTICAL
        );
        // Anchored under a chip partway along the bar: that margin lifts the
        // card off the screen bottom, so it is height the card cannot use.
        let main_margin = 300;
        assert_eq!(
            clamp_card_height(1440, main_margin, CARD_CHROME_VERTICAL),
            1440 - main_margin - CARD_CHROME_VERTICAL
        );
        // Which of the two numbers a vertical bar actually passes is decided in
        // `available_card_height`, and that needs a live monitor plus a mapped
        // bar surface, so it can't be reached from here. This pins the
        // arithmetic it feeds; `clamp_card_height_tracks_bar_thickness` pins
        // the Top/Bottom side of the same choice.
    }

    /// Degenerate input #1: `Monitor::size` reports `(0, 0)` for a monitor GDK
    /// hasn't configured yet. Without the floor this hands
    /// `set_max_content_height` a *negative* value, which GTK reads as "no
    /// maximum" — the silent opposite of a safety net.
    #[test]
    fn clamp_card_height_floors_on_unconfigured_monitor() {
        assert_eq!(
            clamp_card_height(0, TOP_BAR, CARD_CHROME_VERTICAL),
            MIN_CARD_HEIGHT
        );
    }

    /// Degenerate input #2: a bar thick enough to swallow the screen (a tiny
    /// output, or a bar mid-reconfigure reporting a nonsense allocation).
    /// Clamps up to the floor rather than to 0 or below.
    #[test]
    fn clamp_card_height_floors_on_a_screen_swallowing_bar() {
        assert_eq!(
            clamp_card_height(400, 600, CARD_CHROME_VERTICAL),
            MIN_CARD_HEIGHT
        );
        assert_eq!(
            clamp_card_height(240, TOP_BAR, CARD_CHROME_VERTICAL),
            MIN_CARD_HEIGHT
        );
    }

    /// The floor is a floor, never a ceiling, and the result is always a
    /// usable positive cap. Swept across the whole plausible input space plus
    /// both saturating extremes, so no combination can produce a zero, a
    /// negative, or an overflow panic in a debug build.
    ///
    /// The second assertion here used to be `budget == raw.max(MIN_CARD_HEIGHT)`
    /// with `raw` recomputed from the same two `saturating_sub`s — the function
    /// body restated character for character, so it could not fail for any
    /// input (#793 item 4). It is replaced rather than deleted, by properties
    /// that are *consequences* of the implementation instead of a copy of it,
    /// each falsified by a different plausible regression:
    ///
    /// * **Never below the floor** — catches a dropped `.max(MIN_CARD_HEIGHT)`.
    /// * **Never larger than the screen it caps** — the "floor is not a
    ///   ceiling" half. Above the floor the budget must not exceed
    ///   `monitor_height`; a `+` where a `-` belongs fails here and nowhere
    ///   else. Asserted only for `bar_reserved >= 0`: a negative reservation
    ///   would mean the bar handing height *back*, which is not a physically
    ///   meaningful input, and `saturating_sub` legitimately grows the budget
    ///   past the screen for one.
    /// * **Monotone in both arguments** — more screen never yields a smaller
    ///   cap, more reservation never yields a larger one. A swapped
    ///   `saturating_sub` operand order satisfies both assertions above on the
    ///   swept inputs and fails only this one.
    #[test]
    fn clamp_card_height_is_always_a_usable_positive_cap() {
        const HEIGHTS: [i32; 11] = [i32::MIN, -1, 0, 1, 100, 240, 300, 768, 1440, 4320, i32::MAX];
        const RESERVATIONS: [i32; 6] = [i32::MIN, 0, 34, 200, 5000, i32::MAX];

        for height in HEIGHTS {
            for reserved in RESERVATIONS {
                let budget = clamp_card_height(height, reserved, CARD_CHROME_VERTICAL);
                assert!(
                    budget >= MIN_CARD_HEIGHT,
                    "budget {budget} fell below the floor at ({height}, {reserved})"
                );
                assert!(
                    reserved < 0 || budget <= height.max(MIN_CARD_HEIGHT),
                    "budget {budget} exceeds the {height}px screen it is a cap \
                     for, and the floor is not what raised it (reserved {reserved})"
                );
            }
        }

        // Monotone in the screen height. Compares two real outputs against each
        // other rather than either against a recomputed expectation, which is
        // what keeps this independent of the function body.
        for a in HEIGHTS {
            for b in HEIGHTS {
                let (lo, hi) = (a.min(b), a.max(b));
                for reserved in RESERVATIONS {
                    assert!(
                        clamp_card_height(lo, reserved, CARD_CHROME_VERTICAL)
                            <= clamp_card_height(hi, reserved, CARD_CHROME_VERTICAL),
                        "a taller screen ({hi} over {lo}) shrank the cap at \
                         reserved {reserved}"
                    );
                }
            }
        }

        // …and monotone (non-increasing) in what the geometry reserves.
        for a in RESERVATIONS {
            for b in RESERVATIONS {
                let (lo, hi) = (a.min(b), a.max(b));
                for height in HEIGHTS {
                    assert!(
                        clamp_card_height(height, hi, CARD_CHROME_VERTICAL)
                            <= clamp_card_height(height, lo, CARD_CHROME_VERTICAL),
                        "reserving more ({hi} over {lo}) grew the cap at \
                         height {height}"
                    );
                }
            }
        }
    }

    /// Tripwire for `build_pages_stack`'s eager set (#231): as of #338 every
    /// Stats page's sparkline history is hoisted into the `sensors` service (the
    /// CPU page's clock aggregate + per-core `MultiSparkline`s were the last
    /// holdout; #336 did Memory/Disks/GPU + the overall CPU-load line), so no
    /// page needs to build eagerly — `EAGER_PAGES` is empty and every page takes
    /// the lazy path. If this fails, a page was silently re-added to
    /// `EAGER_PAGES` without updating the doc comments on
    /// `build_page`/`build_pages_stack` explaining why it can't build lazily.
    #[test]
    fn eager_pages_is_empty() {
        assert_eq!(EAGER_PAGES, [] as [Page; 0]);
    }

    /// #618: after a hot-plug teardown no drawer window exists, so every
    /// surviving `DRAWER_OPEN` state must read `false`. Leaving a connector's
    /// entry latched at `true` made `overlays::osd` suppress the volume / mic /
    /// brightness OSD on that output for the rest of the session (it reads the
    /// state as "the drawer already shows this control"), and kept the bar's
    /// `drawer-open` seam class applied with no drawer attached.
    ///
    /// Drives [`reset_drawer_open_states`] rather than [`close_all`]: the rest
    /// of `close_all` closes GTK windows and reaches into the plugin service
    /// registry (`plugins::set_active_panel` panics if `plugins::service()`
    /// isn't registered), neither of which exists without a display + a booted
    /// `App`. This is the whole `DRAWER_OPEN` half of it, unchanged.
    #[test]
    fn reset_clears_surviving_drawer_open_states() {
        let open = drawer_open_state("DP-1");
        let already_closed = drawer_open_state("HDMI-A-1");
        // A connector-less monitor: keyed by GdkMonitor pointer, so its entry
        // gets pruned — but a subscriber still holds this handle.
        let headless = drawer_open_state("monitor:0x1234");
        open.set(true);
        headless.set(true);

        reset_drawer_open_states();

        assert!(!open.get(), "connector state stayed latched at true");
        assert!(!already_closed.get());
        assert!(
            !headless.get(),
            "a pruned entry's live handle stayed latched at true"
        );

        DRAWER_OPEN.with(|map| {
            let map = map.borrow();
            // Connector keys survive the rebuild (subscribers wired up before
            // it must keep working); the unreusable fallback key is pruned.
            assert!(map.contains_key("DP-1"));
            assert!(map.contains_key("HDMI-A-1"));
            assert!(!map.contains_key("monitor:0x1234"));
        });

        // The crux of the OSD chain: a fresh subscriber re-minting this key
        // after the rebuild is handed `false`, not the stale `true`. (A
        // `Mutable`'s signal re-delivers its current value on subscribe, which
        // is why rebuilding the overlay alone never fixed this.)
        assert!(!drawer_open_state("DP-1").get());
    }

    /// Companion to the above, pinning [`close_all`] *itself* rather than the
    /// helper: deleting the `reset_drawer_open_states()` call from `close_all`
    /// would leave the helper-level test green, since the helper would still be
    /// correct in isolation. This is the test that actually fails if the fix is
    /// unwired.
    ///
    /// `close_all` can't run to completion off a booted `App` — it reaches
    /// `plugins::set_active_panel`, which `.expect()`s the plugin service out of
    /// the thread-local registry. That's an ordinary unwinding panic (no profile
    /// sets `panic = "abort"`) raised *after* the `DRAWER_OPEN` reset, and
    /// `PANELS` is empty on a test thread so the drain above it touches no GTK.
    /// Catching it is therefore enough to observe the reset. The caught panic's
    /// message shows up in this test's captured output — that's expected, not a
    /// failure. (No `set_hook` to silence it: the panic hook is process-global
    /// and would race the other tests running in parallel.)
    ///
    /// Degrades gracefully in both directions: if the plugin service ever stops
    /// panicking here, `catch_unwind` simply returns `Ok` and the assert still
    /// holds; if the reset is ever reordered *after* the panicking call, this
    /// fails loudly, which is the point.
    #[test]
    fn close_all_resets_drawer_open_state() {
        // A key of its own, so this can't interact with the helper-level test
        // above when libtest runs both on one thread (`--test-threads=1`), where
        // the `DRAWER_OPEN` thread-local is shared.
        let open = drawer_open_state("DP-9");
        open.set(true);

        let _ = std::panic::catch_unwind(close_all);

        assert!(
            !open.get(),
            "close_all left the drawer-open state latched — is it still calling \
             reset_drawer_open_states()?"
        );
    }

    /// A `Waker` whose `wake` re-enters `DRAWER_OPEN` exactly the way a real
    /// subscriber (OSD, bar CSS) would if woken synchronously: it calls
    /// [`drawer_open_state`], which needs its own `borrow_mut()` of the same
    /// thread-local `RefCell`. Built via `std::task::Wake` rather than a raw
    /// `RawWaker`/`RawWakerVTable` — the workspace forbids `unsafe_code`
    /// project-wide, and `Wake` is the safe, stable way to get a `Waker` from
    /// an `Arc` since Rust 1.68.
    struct ReentrantWaker;

    impl Wake for ReentrantWaker {
        fn wake(self: Arc<Self>) {
            // The key doesn't matter — any key hits the same `DRAWER_OPEN`
            // `RefCell`. What matters is that this runs synchronously, from
            // inside whatever called `Waker::wake`.
            let _ = drawer_open_state("reentrant-probe");
        }
    }

    /// #631, the one site in the sweep with no GTK involved at all: this
    /// function's reentrancy trigger is `futures-signals`' `Waker::wake()`,
    /// called synchronously from `Mutable::set_neq`'s internal `notify` when
    /// a subscriber is registered. That makes it hermetically testable with
    /// nothing but `std::task` — no display, no `App`, no new dependency —
    /// unlike the other eight sites in this sweep, which all need a live GTK
    /// window to prove anything either way.
    ///
    /// This is also the one regression in the sweep that would be silent:
    /// every other test in this module would keep passing while this
    /// function quietly went back to holding `DRAWER_OPEN` borrowed across
    /// `set_neq`, reopening the reentrancy hazard (and, transitively,
    /// weakening the #618 guarantee the surrounding doc comment argues for).
    ///
    /// Mechanics: register `ReentrantWaker` on a fresh signal for one key,
    /// set that key's value so `reset_drawer_open_states`'s own `set_neq`
    /// call is a real change (a no-op `set_neq` never notifies, so this
    /// wouldn't trigger anything), then run the function under
    /// `catch_unwind`. Pre-#631, the outer `borrow_mut()` was still held
    /// when `set_neq` woke `ReentrantWaker` synchronously, and its nested
    /// `drawer_open_state` call hit `BorrowMutError`. Post-#631, `set_neq`
    /// runs with no `DRAWER_OPEN` borrow active at all, so the wake's
    /// re-borrow succeeds.
    ///
    /// Falsified by temporarily reverting `reset_drawer_open_states` to the
    /// pre-#631 shape and rerunning: this doesn't just fail the one
    /// `assert!` — `Mutable`'s internal state uses a `std::sync::Mutex`
    /// alongside the borrow, so the `BorrowMutError` panic unwinds through it
    /// mid-notify and poisons it. `catch_unwind` still catches that first
    /// panic, but the poisoned `Mutable` is still sitting in `DRAWER_OPEN`
    /// afterwards, and when the worker thread later tears down its
    /// thread-locals, dropping it re-panics on the poison — from inside a
    /// destructor, where an escaping panic is fatal. The whole test binary
    /// aborts (`SIGABRT`), not just this test. A stronger falsification than
    /// a clean assertion failure, and the same class of outcome the rest of
    /// this sweep guards against by a different route: an inner panic
    /// escaping through a context that cannot unwind.
    #[test]
    fn reset_drawer_open_states_does_not_reenter_the_borrow() {
        let state = drawer_open_state("reentrant-trigger");
        state.set(true);

        let waker = Waker::from(Arc::new(ReentrantWaker));
        let mut cx = Context::from_waker(&waker);
        let mut sig = Box::pin(state.signal());
        // The first poll of a fresh signal always delivers the current value
        // regardless of `changed`, so it takes a second poll (which sees no
        // further change) to actually register the waker for the *next*
        // notify — the one `reset_drawer_open_states`'s `set_neq` will fire.
        loop {
            match sig.as_mut().poll_change(&mut cx) {
                Poll::Ready(Some(_)) => {}
                Poll::Ready(None) => panic!("signal ended before the waker could register"),
                Poll::Pending => break,
            }
        }

        let result = std::panic::catch_unwind(reset_drawer_open_states);

        assert!(
            result.is_ok(),
            "reset_drawer_open_states panicked — a synchronously-woken subscriber \
             re-borrowed DRAWER_OPEN while reset_drawer_open_states was still \
             holding it"
        );
    }
}
