//! Per-monitor pushable sidebars — **one per [`Side`]** since #1158/#1160.
//! Each is a layer-shell window on `Layer::Top` anchored `<side> + Top +
//! Bottom`: the left one (toggled by `widgets::sidebar_toggle` and the
//! `toggle-sidebar` action) carries the built-in calendar/tasks cards plus the
//! three `Sidebar*` plugin regions, and the right one (the `toggle-sidebar-right`
//! action, no chip) carries the three `SidebarRight*` regions and nothing else.
//!
//! Everything below describes **both** unless it says otherwise — layer,
//! exclusive-zone machinery, revealer, scroller, settle re-assert and reflow
//! nudge are shared code, and the four things that differ are the methods on
//! [`Side`]. The one behavioural asymmetry is that the right surface is not
//! mapped at all until a card shows on it ([`wire_non_empty`]); the left, never
//! being empty, is mapped at `install` exactly as it always was.
//!
//! ## Persistence + z-order
//!
//! The surface is created **once** at install time and stays alive for the
//! process lifetime. `widgets::sidebar_toggle` only flips the open state
//! `Mutable`; the surface itself is never hidden, recreated, or re-presented.
//! Within `Layer::Top`, z-order is fixed by surface creation order — install
//! runs before `Bar::new().show()`, so the sidebar sits **below** the bar.
//! Re-presenting the sidebar on each open used to bump it above the bar and
//! produced a ~44 px overlap on the bar's bottom edge.
//!
//! ## Animation + exclusive zone
//!
//! `GtkRevealer` (`SlideRight`) animates the card's allocated width between 0
//! and the card's open width on open/close. The exclusive zone is set
//! **explicitly** from the open-state subscription ([`open_width`] open, `0`
//! closed) rather than driven by `auto_exclusive_zone_enable()` — the auto path
//! failed to reclaim space cleanly on close (the bar stayed pushed even after
//! the revealer settled at 0 width), so we drive it directly. Niri snaps tiles +
//! the bar to the new value immediately; the revealer's slide is cosmetic.
//!
//! The open zone is **measured, not assumed** (#737). [`SIDEBAR_WIDTH`] is a
//! design-baseline literal; what the surface actually paints is the revealer
//! child's natural width, floored at `scale(SIDEBAR_WIDTH)`. The card's padding
//! is `em`-based and its children's minimum widths are too, and
//! `set_size_request` only sets a *minimum* — so above the 1x baseline the card
//! measures wider than 320. Committing a hardcoded 320 then reserves a narrower
//! strip than the surface paints: the sidebar overhangs the tile niri put beside
//! it and swallows the window's left border and rounded corners, which niri
//! draws just outside the window geometry in the 8 px strut gap
//! (`etc/niri/frame.kdl`). [`open_width`] reads the live measurement instead —
//! the same "read live, not once" rule `frame.rs` applies to the bar's height
//! (#441), and what this subsystem's own design spec asked for in the first
//! place ("read revealer allocation",
//! `docs/superpowers/specs/2026-05-14-sidebar-design.md`).
//!
//! `set_exclusive_zone` only mutates gtk4-layer-shell's *pending* state — it
//! applies on the surface's next `wl_surface.commit`, which GTK only issues
//! when it draws. On close the settled card draws nothing, so the
//! `exclusive_zone = 0` could sit uncommitted and niri would keep the tile
//! pushed (a wallpaper gap on the left). `drive_exclusive_zone_on_settle`
//! re-asserts the zone once the revealer settles and forces a GTK frame so the
//! final committed surface state carries it (#194).
//!
//! Committing `exclusive_zone = 0` makes niri stop *reserving* the strip (tiles
//! reflow to full width), but the persistent surface still overlays on top. That
//! overlay is transparent (`.ts-sidebar-surface { background: transparent }`) and
//! the collapsed card paints nothing, so it's invisible. (Historically a grey
//! strip lingered here: the shell scoped a niri `background-effect` frost to the
//! card via the client-side `hytte-blur` protocol, and niri re-frosted the whole
//! still-mapped surface whenever that scoping lapsed — #192/#194. The
//! frosted-glass experiment was retired in #312, so there is no frost to leak.)
//!
//! The surface does **not** shrink back below `SIDEBAR_WIDTH` once opened — a GTK
//! toplevel won't re-measure under a prior allocation (`win_width=320 /
//! surface_width=320` closed-after-open, vs. `0 / 1` never-opened). That is
//! harmless: a transparent, click-through, zone-0 overlay of any width is
//! invisible. `drive_exclusive_zone_on_settle` still re-asserts `exclusive_zone
//! = 0` + flushes on settle so niri reliably reclaims the strut; its card-floor
//! relax is now a belt-and-braces no-op (the toplevel won't actually deflate, and
//! it no longer needs to).
//!
//! ## Scrolling
//!
//! The card stack sits in a [`gtk::ScrolledWindow`] ([`build_scroller`], #965).
//! Without it a card taller than the surface simply overflowed: GTK allocates a
//! widget at least its minimum height, the layer surface's height is the
//! compositor's to give, and the excess was drawn past the bottom edge with no
//! way to reach it — a 12-agent hive card hid every card below it (the pet, in
//! Mara's case).
//!
//! What stops the stack's height from driving the surface's is
//! `vscrollbar_policy = Automatic`: with it, a scroller wrapping 12 × 60 px of
//! rows measures `(min 58, nat 720)` vertically, and with `Never` it measures
//! `(720, 720)` — the bare stack's own numbers. (58 px is GTK's floor for a
//! scroller whose scrollbar may appear; `min-content-height` is `-1` either way
//! and is *not* what does this.) That minimum is the whole fix: the toplevel
//! stops being asked for a height the compositor cannot give, and the excess
//! becomes scroll.
//!
//! The scroller is spliced **between** the `AdwClamp` and the card
//! ([`build_clamped_scroller`], which `install` and the tests share so the tests
//! measure the shipped nesting), so the revealer's child — the thing
//! [`open_width`] measures — is the same `AdwClamp` it always was, and the
//! numbers reaching it through the scroller are the card's own; see
//! [`build_scroller`] for why `hscrollbar_policy = Never` is what makes that
//! true.
//!
//! The surface's own allocation is what bounds the viewport, and it follows the
//! compositor for free: anchored `Top + Bottom` with `exclusive_zone = 0`, the
//! height we are configured at is already the work area minus the bar, on every
//! output and across every mode switch. Nothing here needs to compute or track
//! it. A `max-content-height` cap was tried (#965's first round) and measured
//! inert on this surface — it moves only the *natural* measure, which nothing on
//! a both-edges-anchored axis reads: with the toplevel height held at 300, caps
//! of `-1`, `300` and a nonsensical `100` all produced the same
//! `scroller_h=300 stack_h=720 vadjustment(upper=720, page=300)`. It was
//! deleted rather than kept as decoration.
//!
//! ## Frame integration
//!
//! The frame overlay (`Layer::Overlay`, above the bar) reads
//! [`current_visible_width`] each animation tick and shifts its cutout's
//! left edge to match — the sidebar surface (below the frame) shows
//! through the cutout. [`open_width`] is the authority on both sides, so the
//! cutout's left edge can't disagree with the strip niri reserved.
//!
//! State is per-connector, mirroring `modal::DRAWER_OPEN`. Subscribers (the
//! sidebar surface, the frame draw, future bar-CSS bindings) read
//! `open_signal`; the chip writes via `toggle`.
//!
//! ## Post-close niri reflow (#1129)
//!
//! Committing `exclusive_zone = 0` makes niri stop *reserving* the strip, but
//! niri only *reflows* a workspace's columns on a change **about** them, not
//! on a change to the space around them — so a column that sat flush against
//! the old reserved edge is left exactly where it was, now partly off screen
//! with nothing pushing it back. `drive_exclusive_zone_on_settle`'s settle
//! re-assert nudges niri to reflow the monitor's active workspace right after
//! it commits the closed (`0`) zone — never on open, where niri already
//! reflows its own reserve, and never on a redundant closed→closed re-assert;
//! see [`should_reflow_after_close`] for the exact edge and
//! `hytte_services::niri::reflow_workspace` for the nudge itself (the same
//! move-to-first/move-to-last/move-to-first chain
//! `hytte-plugin-niri-layouts`'s `apply` sends after resizing columns, for
//! the same reason).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, cairo, gdk, glib};
use hytte::prelude::*;
use hytte::services::niri;
use hytte::ui::{Anchor, Layer, LayerShell, layer_window};

use super::frame;
use crate::components::monitor_key::{is_fallback_key, monitor_key};
use crate::scale::scale;

/// **Design-baseline** width of the sidebar surface when fully open, in CSS px,
/// authored at the 1x baseline `crate::scale` documents (font 11pt @ 96 DPI).
/// Matches the "frame border ~320px" geometry from the spec; the frame's cutout
/// left edge animates from [`frame::FRAME_THICKNESS_I32`] (8) up to the open
/// width while the sidebar reveals.
///
/// This is the **floor**, not the final width: it is `scale()`d into the card's
/// `set_size_request` and the `AdwClamp` bounds, and the surface still measures
/// wider than that floor whenever a child's minimum width demands it. Everything
/// that has to agree with what the surface *paints* — the exclusive zone and the
/// frame's cutout left edge — goes through [`open_width`], never through this
/// constant (#737).
pub const SIDEBAR_WIDTH: i32 = 320;

/// Which screen edge a sidebar surface lives on (#1158/#1160).
///
/// The two sides are **mirrors**: same layer, same revealer, same exclusive-zone
/// machinery, same card CSS — only the anchors, the slide direction, the three
/// plugin regions and the namespace differ. Everything keyed per monitor in this
/// module is keyed per `(Side, connector)` since #1160, so the two windows'
/// open states, surfaces and settle timers never touch.
///
/// The one asymmetry is deliberate and is [`Side::Right`]'s whole point: the
/// left sidebar carries the built-in calendar/tasks cards and is therefore never
/// empty, so it is mounted unconditionally; the right one holds nothing but
/// plugin regions, so it is not mapped at all until a card shows there (see
/// [`install_side`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Side {
    /// The original sidebar: anchored `Left + Top + Bottom`, always mounted.
    Left,
    /// The #1158 mirror: anchored `Right + Top + Bottom`, hidden while empty.
    Right,
}

impl Side {
    /// The three layer-shell anchors this side's surface takes.
    ///
    /// Split out as a pure function so the anchor set is assertable without a
    /// live compositor — `gtk4-layer-shell`'s `set_anchor` is write-only from
    /// Rust, so a test on a built window could not read it back.
    fn anchors(self) -> [Anchor; 3] {
        match self {
            Side::Left => [Anchor::Left, Anchor::Top, Anchor::Bottom],
            Side::Right => [Anchor::Right, Anchor::Top, Anchor::Bottom],
        }
    }

    /// Which way the card slides in. The revealer slides **away from** the
    /// anchored edge, so the left card slides right and the right card slides
    /// left; a `SlideRight` right-hand card would grow off the screen.
    fn transition(self) -> gtk::RevealerTransitionType {
        match self {
            Side::Left => gtk::RevealerTransitionType::SlideRight,
            Side::Right => gtk::RevealerTransitionType::SlideLeft,
        }
    }

    /// Which end of the surface the collapsed revealer parks against — the
    /// anchored edge, so the card grows inward from it.
    fn halign(self) -> gtk::Align {
        match self {
            Side::Left => gtk::Align::Start,
            Side::Right => gtk::Align::End,
        }
    }

    /// The layer-shell namespace prefix, which is also what a niri
    /// `layer-rule` matches on — so the two surfaces can carry different
    /// blur/opacity rules.
    fn namespace(self, key: &str) -> String {
        match self {
            Side::Left => format!("hytte-sidebar-{key}"),
            Side::Right => format!("hytte-sidebar-right-{key}"),
        }
    }

    /// The extra CSS class this side's **surface** carries on top of
    /// `.ts-sidebar-surface`, or `None` for the left (which carries only the
    /// shared one, exactly as it always has).
    fn surface_class(self) -> Option<&'static str> {
        match self {
            Side::Left => None,
            Side::Right => Some("ts-sidebar-right-surface"),
        }
    }

    /// The extra CSS class this side's **card** carries on top of `.ts-sidebar`.
    /// Both sides keep `.ts-sidebar` so every existing card/padding rule applies
    /// unchanged; the twin is the hook a skin mirrors paddings/radii through.
    fn card_class(self) -> Option<&'static str> {
        match self {
            Side::Left => None,
            Side::Right => Some("ts-sidebar-right"),
        }
    }
}

thread_local! {
    /// Per-`(side, connector)` open/closed bool. Subscribers connect at
    /// `install` time or earlier (e.g., the frame); writers go through `toggle`.
    ///
    /// This is **user intent**, not what is on screen: the right sidebar's
    /// surface additionally requires a card to show there (see
    /// [`effective_open`]), and its toggle refuses to set this at all while it
    /// is empty ([`toggle_right_on_focused`]).
    static SIDEBAR_OPEN: RefCell<HashMap<(Side, String), Mutable<bool>>> =
        RefCell::new(HashMap::new());
}

thread_local! {
    /// Per-`(side, connector)` sidebar surface handle. Populated by `install`;
    /// read by `current_visible_width_for_key` and `is_settled_for_key`.
    static PANELS: RefCell<HashMap<(Side, String), SidebarPanel>> = RefCell::new(HashMap::new());
}

struct SidebarPanel {
    window: gtk::Window,
    revealer: gtk::Revealer,
    open_state: Mutable<bool>,
    subscription: glib::JoinHandle<()>,
    /// Forwards this monitor's open/close edge to the plugin host's visibility
    /// aggregate (#288). Aborted in [`close_all`] before the monitor is forgotten
    /// so it can't re-add a hot-unplugged connector after teardown.
    visibility_subscription: glib::JoinHandle<()>,
    /// **Right only** (#1160): mirrors `plugins::sidebar_right_non_empty` into
    /// [`Self::non_empty`] and latches the surface's one-and-only `set_visible`
    /// on the first card. `None` on the left, which is never empty. Aborted in
    /// [`close_all`] with its siblings so it cannot map a surface that is being
    /// destroyed.
    non_empty_subscription: Option<glib::JoinHandle<()>>,
    /// Folds the open intent and [`Self::non_empty`] into the value the zone /
    /// revealer / input-region / plugin-visibility subscriptions all act on
    /// ([`effective_open`], #1160). Aborted in [`close_all`] before
    /// `open_state.set(false)` so a teardown edge cannot travel back out through
    /// the derived chain.
    effective_subscription: glib::JoinHandle<()>,
    /// Live exclusive-zone settle timer ([`drive_exclusive_zone_on_settle`]), if
    /// one is armed. Cancelled in [`close_all`]: a tick armed for a close that
    /// gets interrupted by `close_all` (window closed mid-slide → the revealer
    /// can never settle, its frame clock having stopped) would otherwise loop on
    /// the main context forever, keeping the window/revealer clones alive.
    zone_tick: Rc<RefCell<Option<glib::SourceId>>>,
    /// Whether this side currently has **any** card to show on this connector
    /// (#1160) — mirrored out of `plugins::sidebar_right_non_empty` for the
    /// right side, and pinned at `true` for the left (whose built-in
    /// calendar/tasks cards mean it is never empty).
    ///
    /// Read synchronously by [`toggle_right_on_focused`], which is why this is a
    /// `Mutable` parked here rather than only a signal: a toggle has to decide
    /// *now* whether it is a no-op, and a signal's value only arrives on the
    /// next main-context poll.
    non_empty: Mutable<bool>,
}

fn sidebar_open_state(side: Side, key: &str) -> Mutable<bool> {
    SIDEBAR_OPEN.with(|map| {
        map.borrow_mut()
            .entry((side, key.to_string()))
            .or_insert_with(|| Mutable::new(false))
            .clone()
    })
}

/// What the surface actually shows: the user's open intent **and** something to
/// show (#1160).
///
/// Pure, and split out for the same reason [`open_width_from_natural`] and
/// [`should_reflow_after_close`] are: it is the whole "hidden entirely when
/// empty" rule, and it is assertable without a widget tree.
///
/// The left side passes `non_empty = true` unconditionally, so this is the
/// identity there and its behaviour is untouched.
fn effective_open(open: bool, non_empty: bool) -> bool {
    open && non_empty
}

/// Signal that emits the sidebar open/closed state for `monitor`. Backed by
/// [`SIDEBAR_OPEN`] so callers can subscribe before `install` has run for
/// this monitor (e.g., the frame wires up during early bootstrap).
pub fn open_signal(monitor: &Monitor) -> impl Signal<Item = bool> + 'static {
    sidebar_open_state(Side::Left, &monitor_key(monitor)).signal()
}

/// Flip the **left** sidebar's open state for `monitor`. Bar chip calls this on
/// click. The right sidebar has no chip; it is reached through
/// [`toggle_right_on_focused`] (the `toggle-sidebar-right` `GAction`) and by
/// `Esc` while focused.
pub fn toggle(monitor: &Monitor) {
    toggle_side(Side::Left, monitor);
}

/// Flip one side's open state for `monitor`.
///
/// Used by the bar chip (left) and by each surface's own `Esc` handler, which
/// must close the side it is mounted on rather than always the left.
fn toggle_side(side: Side, monitor: &Monitor) {
    let state = sidebar_open_state(side, &monitor_key(monitor));
    let now = state.get();
    state.set(!now);
}

/// Command-surface entry point (no `&Monitor` in hand): flip the sidebar on
/// the `preferred` connector if one is installed there, else on any installed
/// sidebar. Backs the `toggle-sidebar` `GAction` driven by a niri keybind —
/// `preferred` is niri's focused output. Looks the connector up in the live
/// [`PANELS`] map (not [`SIDEBAR_OPEN`]) so it targets a real installed surface
/// and never conjures a dangling open-state entry for a nonexistent monitor.
pub fn toggle_on_focused(preferred: Option<&str>) {
    if let Some(key) = installed_key(Side::Left, preferred) {
        let state = sidebar_open_state(Side::Left, &key);
        state.set(!state.get());
    }
}

/// The [`toggle_on_focused`] twin for the **right** sidebar, backing the
/// `toggle-sidebar-right` `GAction` (#1160).
///
/// Differs from the left in exactly one way, and it is the epic's "hidden
/// entirely when empty" rule: a toggle that would **open** a connector's right
/// sidebar while it has no card does nothing and says so once at `debug!`.
/// Flipping the state anyway would leave the user pressing a keybind that
/// produces no visible change *and* an open flag that silently decides the
/// surface's fate the moment an unrelated plugin dials in — the sidebar would
/// appear to open by itself. The read is synchronous off the panel's mirrored
/// `non_empty` (see [`SidebarPanel::non_empty`]), so the decision is made
/// against what is on screen now, not against a value that lands on the next
/// poll.
///
/// A **close always goes through**, empty or not (#1244 review, finding 1).
/// Refusing on emptiness alone produced the very bug the refusal exists to
/// prevent, from the other direction: open the sidebar while a card is there,
/// let the card go away — the plugin exits, or renders an empty tree (#1039's
/// documented "nothing to show right now"), or grows a `hidden_on` entry for
/// this output (#1050) — and the surface collapses with `open` still latched
/// `true`. The dismissing press was a no-op, and the next card to arrive slid
/// the sidebar open by itself. A departures board with no departures empties
/// the sidebar on its own schedule, so this needs no crash to reach.
///
/// Closing an empty sidebar is never surprising and it clears the latch, which
/// is why the guard is `!non_empty && !open` rather than `!non_empty`. The
/// alternative — clearing `SIDEBAR_OPEN` from [`wire_non_empty`] when the feed
/// goes false — would throw away intent the user may want back after a plugin
/// restart, and put a second writer on [`SIDEBAR_OPEN`].
pub fn toggle_right_on_focused(preferred: Option<&str>) {
    let Some(key) = installed_key(Side::Right, preferred) else {
        tracing::debug!(
            preferred,
            "toggle-sidebar-right: no right sidebar installed; ignoring"
        );
        return;
    };
    let non_empty = PANELS.with(|panels| {
        panels
            .borrow()
            .get(&(Side::Right, key.clone()))
            .is_some_and(|p| p.non_empty.get())
    });
    let state = sidebar_open_state(Side::Right, &key);
    if !non_empty && !state.get() {
        tracing::debug!(
            monitor = %key,
            "toggle-sidebar-right: no plugin card mounted on the right sidebar here; ignoring"
        );
        return;
    }
    state.set(!state.get());
}

/// The connector a side-toggle targets: `preferred` (niri's focused output) when
/// a surface for that side is installed there, else any installed one.
///
/// Looks the connector up in the live [`PANELS`] map (not [`SIDEBAR_OPEN`]) so it
/// targets a real installed surface and never conjures a dangling open-state
/// entry for a nonexistent monitor — and filters by `side`, so a `Side::Right`
/// toggle on a shell where only the left is mounted resolves to `None` rather
/// than to the left sidebar's connector.
fn installed_key(side: Side, preferred: Option<&str>) -> Option<String> {
    PANELS.with(|panels| {
        let panels = panels.borrow();
        preferred
            .filter(|k| panels.contains_key(&(side, (*k).to_owned())))
            .map(str::to_string)
            .or_else(|| {
                panels
                    .keys()
                    .find(|(s, _)| *s == side)
                    .map(|(_, key)| key.clone())
            })
    })
}

/// The sidebar's real open width on one surface, in logical px: the natural
/// width of the revealer's child (the `AdwClamp` wrapping the card), floored at
/// the scaled design baseline.
///
/// Measured rather than assumed (#737). The card's padding is `em`-based, its
/// children's minimum widths grow with the effective font too, and
/// `set_size_request` only sets a *minimum* — so above the 1x baseline the card
/// measures wider than [`SIDEBAR_WIDTH`], and committing the bare constant as
/// the exclusive zone reserves a narrower strip than the surface paints.
///
/// Deliberately measures the revealer's **child**, not the revealer and not the
/// window:
///
/// * `GtkRevealer`'s measure multiplies the sliding orientation by the animation
///   position, so measuring the revealer would under-report mid-slide.
/// * `window.width()` is the size the compositor last configured us at: it lags
///   the content by at least a frame, and — per the module note on the surface
///   never re-measuring below a prior allocation — it stays at the widest value
///   this session ever reached, so it over-reports after a wide card goes away.
///
/// The child's natural width is what the toplevel asks for, and (the surface
/// being anchored Left+Top+Bottom, so niri leaves the width to the client) that
/// is what the surface settles at. It is also stable across the whole slide,
/// which is what lets the zone be committed at the *start* of the open
/// transition rather than a frame after it finishes.
///
/// The `max` floor keeps the value monotone: before the first layout pass (where
/// `measure` can report 0) it is the scaled baseline. It never *masks* a wider
/// card — GTK guarantees `natural >= minimum`, so a card whose minimum exceeds
/// the `AdwClamp`'s maximum is still reported at its true width.
fn open_width(revealer: &gtk::Revealer) -> i32 {
    let natural = revealer.child().map_or(0, |child| {
        let (minimum, natural, _, _) = child.measure(gtk::Orientation::Horizontal, -1);
        // `natural.max(minimum)`, not a bare `natural`: GTK is meant to guarantee
        // `natural >= minimum`, but this child is an `AdwClamp` whose entire job
        // is to cap the natural width at `maximum_size` — and a card whose own
        // minimum exceeds that cap is still allocated, and paints, at its
        // minimum. Taking the max keeps the answer on the side of what is
        // actually painted regardless of which of the two the clamp reports.
        natural.max(minimum)
    });
    open_width_from_natural(natural)
}

/// The floor half of [`open_width`], split out so it is unit-testable without a
/// live widget tree (mirroring `scale::scale_with_factor`).
fn open_width_from_natural(natural: i32) -> i32 {
    natural.max(scale(SIDEBAR_WIDTH))
}

/// Currently visible width of the sidebar card on `monitor`, in CSS px — the
/// measured [`open_width`] while open. Returns `frame::FRAME_THICKNESS_I32` when
/// the sidebar is closed, hasn't been installed yet, or the per-monitor panel is
/// missing. The frame uses this to compute its cutout's left edge each animation
/// tick.
pub fn current_visible_width(monitor: &Monitor) -> i32 {
    current_visible_width_for_key(&monitor_key(monitor))
}

/// Internal: keyed lookup used by both the public API and tests.
fn current_visible_width_for_key(key: &str) -> i32 {
    // Copy the revealer out and measure with no `PANELS` borrow live (#643) —
    // same shape, and the same reason, as `is_settled_for_key` just below:
    // `open_width` walks a whole widget subtree's measure vfuncs, and this runs
    // from `frame.rs`'s per-frame draw while `install`/`close_all` hold the
    // `borrow_mut()` counterparties. (The `open_state.get()` filter reads a
    // `Mutable`, not GTK, so it is fine inside the borrow.)
    let revealer = PANELS.with(|panels| {
        panels
            .borrow()
            .get(&(Side::Left, key.to_owned()))
            .filter(|p| p.open_state.get())
            .map(|p| p.revealer.clone())
    });
    revealer.map_or(frame::FRAME_THICKNESS_I32, |r| open_width(&r))
}

/// True when the sidebar's revealer animation is at rest on `monitor`
/// (fully open or fully closed). The frame's tick callback uses this to
/// know when to stop redrawing after the slide finishes.
pub fn is_settled(monitor: &Monitor) -> bool {
    is_settled_for_key(&monitor_key(monitor))
}

/// Internal: keyed lookup used by both the public API and tests.
fn is_settled_for_key(key: &str) -> bool {
    // Copy the two handles out and read them with no `PANELS` borrow live
    // (#643). `is_child_revealed()` is only a property *getter*, so it cannot
    // emit — but the sweep's definition is "any borrow across a GTK call" and
    // deliberately does not carve out getters, and this runs from `frame.rs`'s
    // per-frame tick callback while `install`/`close_all` hold the `borrow_mut()`
    // counterparties. Cheaper to settle it than to keep the exemption as
    // folklore. (`current_visible_width_for_key` just above reads
    // `open_state.get()` — a `Mutable`, not GTK — so it needs nothing.)
    let handles = PANELS.with(|panels| {
        panels
            .borrow()
            .get(&(Side::Left, key.to_owned()))
            .map(|p| (p.revealer.clone(), p.open_state.get()))
    });
    handles.is_none_or(|(revealer, open)| revealer.is_child_revealed() == open)
}

/// Build the sidebar surface for one monitor, mount it as a layer-shell
/// window, and wire its open-state subscription. Mirrors `modal::install`
/// in shape; called from `main.rs` per monitor — must run **before** the
/// bar's `Bar::new().show()` so the sidebar surface stays below the bar
/// in z-order (`Layer::Top` orders by creation, not by re-commit).
pub fn install(monitor: &Monitor) {
    install_side(monitor, Side::Left);
}

/// The [`install`] twin for the **right** sidebar (#1158/#1160) — called from
/// `main.rs` right beside it, per monitor.
///
/// Same surface, same machinery, mirrored anchors; the one behavioural
/// difference is that this surface is not mapped until a plugin card shows on
/// this connector's right side, and its toggle is a no-op until then.
pub fn install_right(monitor: &Monitor) {
    install_side(monitor, Side::Right);
}

/// Build one side's surface. [`install`]/[`install_right`] are the two call
/// sites; everything that differs between them is read off `side`.
fn install_side(monitor: &Monitor, side: Side) {
    let key = monitor_key(monitor);
    let open_state = sidebar_open_state(side, &key);

    // "Is there anything to show on this side, on this monitor?" (#1160). The
    // left sidebar carries the built-in calendar/tasks cards, so it is never
    // empty and this is a constant `true` — which makes `effective_open` the
    // identity there and leaves its behaviour byte-identical to pre-#1160.
    let non_empty = Mutable::new(side == Side::Left);

    let window = build_sidebar_window(monitor, &key, side);
    let revealer = build_revealer(side);
    let card = build_card(monitor, side);
    // The clamp → scroller → card nesting (#965), built by the same helper the
    // tests build it with so they measure what ships.
    let clamp = build_clamped_scroller(&card);
    revealer.set_child(Some(&clamp));
    window.set_child(Some(&revealer));

    // The surface's input region, wired BEFORE the first `set_visible` — see
    // [`wire_input_region`] for the whole #212 argument. Ordering is the point:
    // every line that touches the surface has to be in place before the one map
    // this window will ever get.
    wire_input_region(&window, &open_state, &non_empty);

    // Present the surface ONCE — toggle goes through the revealer + open_state,
    // never through set_visible/present. See module-level note on z-order.
    //
    // The left maps here, at install, and stays alive for the process lifetime.
    // The right maps the first time a card shows on this connector and then
    // stays alive too (see [`wire_non_empty`]): a *persistent* layer surface is
    // the design this module documents at the top, and re-presenting one on each
    // toggle is what bumped it above the bar in the first place.
    if side == Side::Left {
        window.set_visible(true);
    }

    // Slot holding the currently-armed settle timer so `close_all` can cancel it
    // before tearing the surface down (see field docs on SidebarPanel).
    let zone_tick: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    // The last exclusive zone actually committed on this surface, seeded at 0
    // to match what `build_sidebar_window` committed above (#1129) — read by
    // [`should_reflow_after_close`] so the post-close niri reflow nudge fires
    // on a genuine open→closed transition and not on a redundant
    // closed→closed re-assert.
    let last_zone: Rc<Cell<i32>> = Rc::new(Cell::new(0));

    // What the surface acts on: the user's intent AND something to show (#1160).
    // For the left this tracks `open_state` exactly (`non_empty` is a constant
    // `true`), so every subscriber below sees the same edges it always did.
    let effective = Mutable::new(effective_open(open_state.get(), non_empty.get()));

    let subscription = wire_open_subscription(
        &window, &revealer, &card, &effective, &zone_tick, &last_zone, &key,
    );
    wire_escape(&window, monitor.clone(), side);

    // Forward this monitor's sidebar open/close edge to the plugin host so
    // out-of-process plugin cards mounted here can park their pollers while the
    // sidebar is hidden (#288) — e.g. the departures plugin's own poller.
    // Kept as its own lightweight subscription (rather than folded into the
    // zone-driving `wire_open_subscription`) so that dense function keeps
    // its argument budget; the host ORs this across monitors before pushing
    // `SlotVisibility`. The initial `false` on subscribe seeds this monitor's flag.
    //
    // Since #1160 the two sides feed **separate** aggregates and a plugin's push
    // follows the aggregate of the sidebar its own mount is on (the #1221 review's
    // LOW 5): a right-mounted card is not on screen because the *left* sidebar
    // opened. It is the **effective** open that is forwarded, not the raw intent,
    // so a right sidebar that is open-but-empty reports hidden — which is what it
    // is.
    let visibility_subscription = {
        let key = key.clone();
        glib::MainContext::default().spawn_local(effective.signal().for_each(move |open| {
            match side {
                Side::Left => crate::plugins::set_sidebar_visibility(&key, open),
                Side::Right => crate::plugins::set_sidebar_right_visibility(&key, open),
            }
            std::future::ready(())
        }))
    };

    // The right side's "does this connector have a card?" feed, mirrored into
    // `non_empty` (#1160). Left unwired on the left, whose `non_empty` is the
    // constant `true` seeded above.
    let non_empty_subscription = (side == Side::Right).then(|| {
        wire_non_empty(
            &window,
            &non_empty,
            crate::plugins::sidebar_right_non_empty(monitor),
        )
    });

    // Fold intent × content into the value every subscriber above acts on.
    // Spawned **after** them so its first emission finds them already listening,
    // and after the non-empty feed so the surface has had its chance to map.
    let effective_subscription = {
        let effective = effective.clone();
        let open_state = open_state.clone();
        glib::MainContext::default().spawn_local(
            map_ref! {
                let open = open_state.signal(),
                let has_card = non_empty.signal() => effective_open(*open, *has_card)
            }
            .for_each(move |eff| {
                effective.set_neq(eff);
                std::future::ready(())
            }),
        )
    };

    // `drop(…with(|…| …insert(…)))`, not a bare `insert(…);` statement (#643,
    // mirroring the annotated `modal::install` site). `insert` returns the
    // displaced `SidebarPanel`; as a bare statement that value is a temporary
    // of the *same* statement as the `borrow_mut()` `RefMut`, and statement
    // temporaries drop in reverse creation order — so it would run its drop
    // glue with `PANELS` still borrowed. Tail-expression + outer `drop` moves
    // that past the borrow.
    //
    // Stating the mechanism precisely, because the cluster has muddled it
    // before: this is a *refcount decrement*, not a widget teardown. GTK holds
    // its own reference to a mapped toplevel, so dropping the Rust `gtk::Window`
    // handle does not dispose the window (that needs `destroy()`, which
    // `close_all` calls). What actually runs here is `JoinHandle`/`Mutable`
    // drop glue plus two GObject unrefs. Reachable only if `install` ran twice
    // for one key without an intervening `close_all`, which `main.rs` currently
    // prevents — weak, like the rest of the `install` group, and converted for
    // the same reason: not holding the borrow costs nothing.
    drop(PANELS.with(|panels| {
        panels.borrow_mut().insert(
            (side, key),
            SidebarPanel {
                window,
                revealer,
                open_state,
                subscription,
                visibility_subscription,
                non_empty_subscription,
                effective_subscription,
                zone_tick,
                non_empty,
            },
        )
    }));
}

/// Wire the surface's input region so a closed (or empty) sidebar's persistent
/// layer-shell surface doesn't swallow pointer events, and wire it **before**
/// the window's first `set_visible` (#212).
///
/// The ordering is the whole function. A layer surface maps **synchronously**
/// inside its one `set_visible(true)` and — for a persistent surface — never
/// remaps (`hytte_ui::LayerWindowBuilder::build`'s *Surface lifecycle* note), so
/// surface wiring that runs after that map silently never applies. That is the
/// exact shape of the #192/#193/#212 frost regressions, where blur was attached
/// after the one-and-only map and did nothing, and it is why this goes through
/// [`hytte::ui::on_surface_ready`] rather than a bare post-`set_visible` call:
///
/// * On the **left**, the map happens at `install` and the helper's
///   already-mapped branch fires immediately — the same instant the pre-#1160
///   code applied its region.
/// * On the **right**, the map is latched to the first card ([`wire_non_empty`])
///   and can be minutes away, so there is nothing to apply a region *to* at
///   install time (`window.surface()` is `None`); the helper's `connect_map`
///   hook is what carries it across that gap. This surface is the first one in
///   the tree where the difference between the two spellings is observable at
///   all.
///
/// It is also the one place a blur region would be attached if the
/// frosted-glass experiment (#312) ever came back — one call, both sides, before
/// the map.
///
/// Split out of [`install_side`] on [`wire_non_empty`]'s precedent, and for the
/// same reason: `install_side` needs a live `Monitor` and a layer surface, so no
/// test reaches it, and replacing this block with the pre-#1160
/// `apply_input_passthrough(&window, true)` left the whole suite green (#1244
/// review, finding 2). Taking a bare `gtk::Window` makes the *ordering* itself
/// assertable — see `gtk_tests::the_input_region_is_wired_before_the_first_map`.
///
/// Applies `!effective_open(…)`: passthrough while the sidebar is closed **or**
/// empty, a full region while it is actually showing something. At map time on
/// the right that is always `true` (the toggle refuses to open an empty
/// sidebar), and on the left it is `true` because nothing is open at install.
fn wire_input_region(window: &gtk::Window, open: &Mutable<bool>, non_empty: &Mutable<bool>) {
    let open = open.clone();
    let non_empty = non_empty.clone();
    on_map_or_now(window, move |window| {
        let showing = effective_open(open.get(), non_empty.get());
        apply_input_passthrough(window, !showing);
    });
}

/// Run `apply` with the **window** as soon as its surface exists, and again on
/// every subsequent map — [`hytte::ui::on_surface_ready`] with the argument
/// flipped from `&gdk::Surface` to `&gtk::Window`.
///
/// A three-line adapter with two jobs. The obvious one: [`apply_input_passthrough`]
/// (and `frame::install_click_through` before it) is written against the window,
/// because that is what it has to re-ask for a surface handle on each call.
///
/// The one that earns it a name: it is the **seam this module's map-timing
/// contract is assertable through** (#1244 review, finding 2). `install_side`
/// needs a live `Monitor` and a layer surface, so nothing in CI reaches it, and
/// replacing its `on_surface_ready` block with a bare pre-map
/// `apply_input_passthrough` left all 1033 tests green — the #1180-item-8 shape
/// one layer up, on the contract the right sidebar's whole design leans on.
/// Taking a plain closure and a bare `gtk::Window` lets
/// `gtk_tests::the_input_region_is_wired_before_the_first_map` stamp
/// `window.is_mapped()` from inside the callback and pin *when* it ran, which no
/// assertion on the input region itself could do: **GDK exposes no getter for a
/// surface's input region**, so what it applied stays a code-path argument plus
/// a live-verify item. That the wiring is deferred at all is now held by a test.
fn on_map_or_now(window: &gtk::Window, apply: impl Fn(&gtk::Window) + 'static) {
    let window_for_apply = window.clone();
    hytte::ui::on_surface_ready(window, move |_surface| apply(&window_for_apply));
}

/// Mirror `source` ("does this side have a card on this connector?") into
/// `non_empty`, and **map** `window` the first time it says yes (#1160).
///
/// Three decisions live here, and all three are the epic's:
///
/// * **The surface is not created until there is something to put on it.** A
///   shell with no right-mounted plugin — which is every shell until #1161's nix
///   option lands, and most shells after — never presents this window at all, so
///   there is no transparent 320 px overlay on the right edge, no exclusive zone
///   and nothing for niri to composite. That is what "hidden entirely when
///   empty" means: not a blank slab that happens to paint nothing.
/// * **The map is latched, not mirrored.** Once mapped, the surface stays mapped
///   for the process lifetime even if every right-hand plugin disconnects. This
///   is the same persistent-surface rule the left sidebar has followed since
///   #212 and the module header's z-order note is about: a layer surface is
///   created inside its one `set_visible(true)` and `Layer::Top` orders by
///   creation, so re-presenting one on each empty↔non-empty cycle would re-stack
///   it — the ~44 px bar overlap that note records. An already-mapped right
///   sidebar with nothing to show is invisible for exactly the reasons a closed
///   one is: collapsed revealer, transparent surface, zero exclusive zone, empty
///   input region.
/// * **`set_visible` is the last thing wired.** `install_side` has already run
///   `on_surface_ready` by the time this can fire, so the input region (and any
///   future blur region) rides the map rather than missing it — the #192/#193
///   footgun `on_surface_ready` exists for.
///
/// Split out of `install_side` so the map behaviour is assertable: `install_side`
/// needs a live `Monitor` and a layer surface and no test reaches it, while this
/// takes a bare `gtk::Window` and any `bool` signal.
fn wire_non_empty(
    window: &gtk::Window,
    non_empty: &Mutable<bool>,
    source: impl Signal<Item = bool> + 'static,
) -> glib::JoinHandle<()> {
    let non_empty = non_empty.clone();
    let window = window.clone();
    let mapped = Cell::new(false);
    glib::MainContext::default().spawn_local(source.for_each(move |has_card| {
        non_empty.set_neq(has_card);
        if has_card && !mapped.replace(true) {
            tracing::debug!("sidebar: right surface mapping — first card on this output");
            window.set_visible(true);
        }
        std::future::ready(())
    }))
}

/// Layer-shell window anchored `<side> + Top + Bottom` — full screen height,
/// `exclusive_zone` reserves on that single side edge for well-defined push
/// semantics. **No `set_size_request`** — the window's natural width is
/// driven by the revealer's allocated child width, which animates between
/// 0 (closed) and `SIDEBAR_WIDTH` (open). The zone itself is set explicitly
/// from the open subscription, not via auto — see module-level note.
///
/// The two sides differ in exactly four things, all read off [`Side`]: the
/// horizontal anchor ([`Side::anchors`]), the namespace (so a niri `layer-rule`
/// can address one surface without the other, [`Side::namespace`]), the surface
/// CSS class ([`Side::surface_class`]) and — outside this function — the
/// revealer's slide direction. Layer, exclusivity, keyboard mode and the seeded
/// zero zone are identical, which is what "a mirror of the left" means.
fn build_sidebar_window(monitor: &Monitor, key: &str, side: Side) -> gtk::Window {
    let mut builder = layer_window(monitor).layer(Layer::Top);
    for anchor in side.anchors() {
        builder = builder.anchor(anchor);
    }
    let window = builder
        .namespace(side.namespace(key))
        .exclusive(false)
        .keyboard_mode(KeyboardMode::OnDemand)
        .build();
    window.add_css_class("ts-sidebar-surface");
    if let Some(class) = side.surface_class() {
        window.add_css_class(class);
    }
    window.set_exclusive_zone(0);
    window
}

/// Revealer that pushes the card out from the anchored screen edge in time with
/// niri's tile reflow — `SlideRight` off the left edge, `SlideLeft` off the
/// right one ([`Side::transition`]). The 180 ms duration matches the modal
/// drawer's slide so every surface feels like one design system.
fn build_revealer(side: Side) -> gtk::Revealer {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(side.transition());
    revealer.set_transition_duration(180);
    revealer.set_reveal_child(false);
    revealer.set_halign(side.halign());
    revealer.set_valign(gtk::Align::Fill);
    revealer
}

/// The revealer's child exactly as [`install`] mounts it: `AdwClamp` →
/// [`build_scroller`] → card (#965).
///
/// Split out of `install` **so the tests can build the shipped nesting**.
/// `install` needs a live `Monitor` and a layer surface, so no test reaches it —
/// and while this clamp was assembled inline there, splicing the bare card into
/// it (the sidebar with no scroller at all, i.e. #965 fully un-fixed) left all
/// 14 tests in this module green. They pinned the helper's *configuration* and
/// nothing pinned where the helper was *mounted*. `gtk_tests::scrolled_tree`
/// now goes through here, and asserts the clamp's child really is the scroller.
///
/// Scaled, like the card's own floor in `build_card`: an unscaled 320 cap over a
/// card whose `em` padding and children grew with the font would try to tighten
/// the card below its own minimum every time text-scaling is above the 1x
/// baseline (#737). At 1x `scale` is a no-op, so this is the same 320.
fn build_clamped_scroller(card: &gtk::Box) -> adw::Clamp {
    adw::Clamp::builder()
        .maximum_size(scale(SIDEBAR_WIDTH))
        .tightening_threshold(scale(SIDEBAR_WIDTH))
        .child(&build_scroller(card))
        .build()
}

/// Vertical viewport for the card stack (#965): the cards scroll inside the
/// surface instead of overflowing past its bottom edge.
///
/// Mounted between the `AdwClamp` and the card ([`build_clamped_scroller`]),
/// which is what keeps it invisible to the width machinery — [`open_width`]
/// measures the revealer's child, and that child is still the same clamp. The
/// reason the clamp's *numbers* are also unchanged is the policy pair, and it is
/// worth stating precisely because #737's contract rides on it:
///
/// * `vscrollbar_policy = Automatic` is **the fix**, and the only line here
///   whose mutation is red. It is what decouples the scroller's vertical
///   *minimum* from its child's: wrapping 12 × 60 px of rows it measures
///   `(min 58, nat 720)`, and with `Never` it measures `(720, 720)` — the bare
///   stack's own numbers, i.e. exactly the overflow #965 is about. (58 px is
///   GTK's floor for a scroller whose scrollbar may appear; `min-content-height`
///   is `-1` in both cases and has nothing to do with it.) The vertical mirror
///   of the `Never` below: there we *want* the child's minimum to pass through,
///   here we want it stopped. `gtk_tests::a_tall_card_scrolls_…` is the guard.
/// * `hscrollbar_policy = Never` is what makes `GtkScrolledWindow` propagate its
///   child's **minimum** width straight through instead of substituting its own
///   `min-content-width`. That minimum is the entire horizontal contract of this
///   subsystem: `build_card`'s `set_size_request` floor and any card whose own
///   minimum exceeds the clamp's cap both reach [`open_width`] as a *minimum*,
///   never as a natural (the clamp's whole job is to cap the natural at
///   `scale(SIDEBAR_WIDTH)`). Switching this to `Automatic` would drop the card's
///   minimum on the floor and hand back the bare baseline — re-introducing
///   exactly the under-reserved exclusive zone #737 fixed, where the surface
///   painted wider than the strip niri had reserved and overhung the tile beside
///   it. `gtk_tests::wide_card_reserves_what_it_paints` is the guard.
/// * `propagate_natural_width` carries the child's natural through as well, so
///   the pair leaves *both* halves of `natural.max(minimum)` reading the card.
///   (Belt and braces on this tree specifically: the clamp above caps the
///   natural at the baseline anyway, so only the minimum can ever exceed it —
///   flipping this one to `false` was measured too, and no test here moves.)
/// * `overlay_scrolling` asks for the vertical scrollbar as an *indicator* drawn
///   over the content rather than a widget allocated beside it — the sidebar has
///   no room for a gutter, and #965 asked for it so the open width can't jump by
///   a scrollbar's width the moment a card grows tall. Stated explicitly even
///   though it is GTK's default, because it is a contract of this surface rather
///   than a preference. It is **not**, however, what protects the width here:
///   flipping it to `false` was measured against both width tests (realized and
///   unrealized, GTK 4.22) and changed nothing, so this line is intent, and
///   `Never` above is the guard.
///
/// `propagate_natural_height` is the vertical half of the same idea: the
/// scroller reports the stack's real height as its natural rather than
/// collapsing to `min-content-height`. Like its horizontal twin it is measured
/// **inert on this surface** — flipping it to `false` moves no test — and for
/// the same kind of reason: the height axis here belongs to the compositor (the
/// toplevel is anchored `Top + Bottom`), so nothing reads the vertical natural.
/// Kept because it is the truthful answer to "how tall would this like to be",
/// which stops being unobservable the moment this tree is mounted anywhere that
/// is not a both-edges-anchored layer surface. What actually fixes #965 is the
/// *minimum* the first bullet removes, not this.
fn build_scroller(card: &gtk::Box) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .overlay_scrolling(true)
        .child(card)
        .build()
}

/// Card has a fixed `SIDEBAR_WIDTH` so when the revealer is fully expanded
/// the surface settles at exactly that width — and at exactly 0 when fully
/// collapsed. No margins so there's no gap around the dark area. The bar
/// (`Layer::Top`, mapped after sidebar) naturally paints over y=0..44 in
/// the overlap, so no top margin is needed.
///
/// `set_size_request` only sets the **minimum** width, so a child with a
/// pathological natural width (e.g. a long calendar event title, or an
/// `AdwActionRow` subtitle that doesn't wrap) would otherwise push the
/// card — and the layer-shell surface above it — past `SIDEBAR_WIDTH`,
/// visually overlapping niri tiles, the bar, and the frame. The
/// `AdwClamp` wrapping this card in `install` caps the natural width at
/// `scale(SIDEBAR_WIDTH)`; see also `components::layout::finish_page` for the
/// same belt-and-suspenders pattern in the drawer.
///
/// That cap binds the **natural** width only. A child whose *minimum* width
/// exceeds it still widens the card — no clamp can allocate a child below its
/// own minimum — which is exactly why the exclusive zone is measured rather than
/// assumed (#737): the belt-and-suspenders pair caps the common case, and
/// [`open_width`] makes the reserved strip match whatever gets through anyway.
///
/// This `scale(SIDEBAR_WIDTH)` floor is the **open-state** floor and is set here
/// only as the initial value: [`drive_exclusive_zone_on_settle`] relaxes it to
/// `0` once the sidebar settles closed (and [`wire_open_subscription`] restores
/// it on open). Without that, the 320px minimum pins the *persistent*
/// layer-shell surface at full width even when the revealer is collapsed, so the
/// closed surface keeps covering the left edge and niri never reflows the tile
/// (#194). It is `scale`d (#737) because `set_size_request` is exactly the
/// imperative-pixel case `crate::scale` exists for — an unscaled 320 shrinks
/// relative to the `em`-based card padding as the font grows. What the surface
/// ends up painting is still the *measured* width ([`open_width`]), which the
/// floor only bounds from below.
fn build_card(monitor: &Monitor, side: Side) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    card.add_css_class("ts-sidebar");
    // The #1160 twin: both sides keep `.ts-sidebar` (so every padding/colour
    // rule authored for the sidebar applies to both without being written
    // twice), and the right one additionally carries `.ts-sidebar-right` as the
    // hook a skin mirrors paddings/radii through.
    if let Some(class) = side.card_class() {
        card.add_css_class(class);
    }
    card.set_size_request(scale(SIDEBAR_WIDTH), -1);
    card.set_halign(gtk::Align::Fill);
    card.set_hexpand(false);
    card.set_valign(gtk::Align::Fill);
    // vexpand so the card stretches to the full sidebar height — needed
    // for the spacer below to actually have slack to absorb, which is
    // what anchors the bottom plugin region to the bottom edge.
    card.set_vexpand(true);

    // Plugin mount: `Mount::SidebarLead` / `Mount::SidebarRightLead` — the
    // *leading* plugin *region* (#301), mounted at the very TOP of the sidebar,
    // ABOVE the built-in cards. This is the only region whose cards render above
    // calendar/tasks; the after-tasks `SidebarTop` region (below) cannot. This is
    // where the weather card lives now (#290 migrated it out-of-process; see
    // `trollshell-plugin-weather`). Empty until a plugin dials in.
    card.append(&match side {
        Side::Left => crate::plugins::sidebar_lead_slot(monitor),
        Side::Right => crate::plugins::sidebar_right_lead_slot(monitor),
    });

    // The built-in cards are the **left** sidebar's, and stay there (#1158: the
    // right side is a mirror of the left's *shape*, not a second copy of its
    // contents — a second calendar would be two views of one service fighting
    // for the same screen). The right card is three plugin regions and the flex
    // gap between them, which is exactly why it is hidden entirely while those
    // regions are empty.
    if side == Side::Left {
        card.append(&crate::widgets::calendar::widget(monitor));
        card.append(&crate::widgets::tasks::widget(monitor));
    }

    // Plugin mount: `Mount::SidebarTop` / `Mount::SidebarRightTop` — a *region*
    // holding N out-of-process widget-plugin cards (#274), sorted by each
    // plugin's manifest `order`. Reconciled *after* the built-in cards but above
    // the flex gap (plugins must not shove calendar/tasks down). Empty until a
    // plugin dials in.
    card.append(&match side {
        Side::Left => crate::plugins::sidebar_top_slot(monitor),
        Side::Right => crate::plugins::sidebar_right_top_slot(monitor),
    });

    // Flex gap: eats whatever vertical space the calendar + tasks
    // didn't claim, so the bottom plugin region settles against the
    // bottom edge of the sidebar instead of floating in the middle.
    //
    // Still true inside the scroller's viewport (#965): a `GtkViewport`
    // allocates its child `max(child natural, viewport)`, so a stack shorter
    // than the surface is stretched to the full viewport and this gap still has
    // the slack it needs (`gtk_tests::a_short_card_still_fills_the_viewport`).
    // Once the stack outgrows the surface the gap collapses to 0 and the bottom
    // region rides at the end of the scroll — which is the point: it is
    // reachable there, where before it was drawn off the screen.
    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    card.append(&spacer);

    // Plugin mount: `Mount::SidebarBottom` / `Mount::SidebarRightBottom` — the
    // bottom plugin *region* (#274), reconciled below everything. This is where
    // the departures board lives now (#289 migrated it out-of-process; see
    // `trollshell-plugin-departures`).
    card.append(&match side {
        Side::Left => crate::plugins::sidebar_bottom_slot(monitor),
        Side::Right => crate::plugins::sidebar_right_bottom_slot(monitor),
    });
    card
}

/// The three widgets [`drive_exclusive_zone_on_settle`]/`reassert_if_settled`
/// act on together, bundled into one clonable value so those two functions
/// stay under clippy's `too_many_arguments` now that #1129 added the reflow
/// tracking (`last_zone`, `key`) alongside them.
#[derive(Clone)]
struct ZoneSurface {
    window: gtk::Window,
    revealer: gtk::Revealer,
    card: gtk::Box,
}

/// Drive open/close transitions from the shared mutable. The surface stays
/// alive across toggles (see module note on z-order); we flip the revealer,
/// the exclusive zone, AND the surface's input region in lockstep. Niri
/// snaps tiles + bar to the new zone immediately; the revealer's slide is
/// cosmetic.
///
/// Returns the `JoinHandle` so `close_all` can abort the subscription
/// before closing the window — prevents a zombie subscription firing into
/// a dead window after a hot-plug cycle.
fn wire_open_subscription(
    window: &gtk::Window,
    revealer: &gtk::Revealer,
    card: &gtk::Box,
    open_state: &Mutable<bool>,
    zone_tick: &Rc<RefCell<Option<glib::SourceId>>>,
    last_zone: &Rc<Cell<i32>>,
    key: &str,
) -> glib::JoinHandle<()> {
    let window = window.clone();
    let revealer = revealer.clone();
    let card = card.clone();
    let zone_tick = zone_tick.clone();
    let last_zone = last_zone.clone();
    let key = key.to_owned();
    let open_state_for_zone = open_state.clone();
    glib::MainContext::default().spawn_local(open_state.signal().for_each(move |open| {
        // Restore the card's full-width floor the moment we start opening, so the
        // revealer slides a full-width card in. The floor is relaxed to 0 on
        // each settled-close (see `drive_exclusive_zone_on_settle`) so the closed
        // toplevel can re-measure below it and the wl_surface deflates; we only
        // relax on *settle*, so the close slide still shows a full-width card
        // sliding out. (#194)
        //
        // Ordered *before* the zone: `open_width` measures the card, so the floor
        // has to be back in place for that measurement to describe the open
        // surface rather than the relaxed-closed one (#737).
        if open {
            card.set_size_request(scale(SIDEBAR_WIDTH), -1);
        }
        // Reserve exactly what the surface will paint, measured now — not the
        // bare `SIDEBAR_WIDTH` literal, which under-reserves above the 1x
        // baseline and lets the sidebar overhang the tile (#737).
        window.set_exclusive_zone(if open { open_width(&revealer) } else { 0 });
        // Push the layer-shell request to niri NOW. gtk4-layer-shell enqueues the
        // `set_exclusive_zone` request on GTK's wayland connection but the bytes
        // only leave the process on GTK's next flush; a sidebar settling closed
        // may not produce another frame, so the zero zone could sit unflushed and
        // niri would keep the tile pushed (grey wallpaper gap). Flush GDK's
        // display connection explicitly so the zone release lands (#194).
        WidgetExt::display(&window).flush();
        revealer.set_reveal_child(open);
        apply_input_passthrough(&window, !open);
        // Re-assert the exclusive zone once the revealer settles so niri reliably
        // reclaims the strip on close (#194). `set_exclusive_zone` only mutates
        // gtk4-layer-shell's PENDING state; it commits on GTK's next frame. When
        // the sidebar settles closed (transparent card, revealer collapsed to 0)
        // GTK has no reason to draw again, so the `exclusive_zone = 0` can sit
        // uncommitted — the compositor keeps the tile pushed and a wallpaper gap
        // shows. We tick until settle, then re-set the zone + force a GTK frame
        // (queue_draw) so the FINAL committed surface state carries the right
        // zone. NEEDS LIVE NIRI RE-TEST.
        drive_exclusive_zone_on_settle(
            &ZoneSurface {
                window: window.clone(),
                revealer: revealer.clone(),
                card: card.clone(),
            },
            &open_state_for_zone,
            &zone_tick,
            &last_zone,
            &key,
            open,
        );
        async {}
    }))
}

/// Re-arm a frame-cadence (~60 Hz) slide timer in `slot`, first cancelling
/// whatever timer `slot` currently holds. `step` runs each tick and returns the
/// next [`glib::ControlFlow`]; when it yields `Break` the timer clears `slot`.
///
/// Because re-arming removes the slot's previous timer, at most one timer per
/// slot is ever live, and `slot` only ever holds a *live* id (the owner clears
/// it on `Break`). That invariant is what lets [`close_all`] cancel an in-flight
/// settle tick by removing the stored id — without it, a tick armed for a close
/// that is then interrupted by `close_all` (window closed mid-slide → the
/// revealer's frame clock stops, so it can never settle and the tick's own
/// settle/bail conditions never trip) would loop on the main context forever,
/// holding the window/revealer clones alive (a zombie surface per hot-plug). It
/// also means `close_all` never removes a stale (already-finished, possibly
/// reused) source id.
fn rearm_slide_tick<F>(slot: &Rc<RefCell<Option<glib::SourceId>>>, mut step: F)
where
    F: FnMut() -> glib::ControlFlow + 'static,
{
    // The slot only holds a live timer (the owner clears it on Break), so a
    // present id is safe to remove here.
    let previous = slot.borrow_mut().take();
    if let Some(previous) = previous {
        previous.remove();
    }
    let slot_for_clear = slot.clone();
    let id = glib::timeout_add_local(Duration::from_millis(16), move || {
        let flow = step();
        if matches!(flow, glib::ControlFlow::Break) {
            // We are the slot's only live timer (re-arm removes predecessors),
            // so clearing unconditionally can't drop a successor's id.
            slot_for_clear.borrow_mut().take();
        }
        flow
    });
    *slot.borrow_mut() = Some(id);
}

/// Re-assert the layer-shell exclusive zone — and **deflate the card's width
/// floor** — after the revealer settles, then force a GTK frame so the final
/// surface state actually commits (#194).
///
/// `set_exclusive_zone` (called once in [`wire_open_subscription`]) only mutates
/// gtk4-layer-shell's *pending* state — it applies on the surface's next
/// `wl_surface.commit`, which GTK only issues when it draws a frame. On OPEN the
/// revealer's slide and the now-visible card keep GTK drawing, so the open zone
/// commits naturally.
///
/// The re-assert also re-*measures* ([`open_width`], #737) rather than replaying
/// the value [`wire_open_subscription`] committed at the start of the slide, so
/// a card that grew while the sidebar was opening (a plugin card dialling in, a
/// long calendar title arriving) still ends up with a zone that matches what the
/// settled surface paints.
///
/// The card-floor relax is a vestige of an earlier (disproven) hypothesis. A
/// live `RUST_LOG` capture showed the closed toplevel + `wl_surface` staying at
/// `win_width=320 / surface_width=320` once opened (vs. `0 / 1` never-opened),
/// and the theory was that `build_card`'s `set_size_request` floor pinned the
/// surface full-width so it covered a grey strip. Relaxing that
/// floor to `0` on settled-close did **not** shrink the surface (a GTK toplevel
/// won't re-measure under a prior allocation); the strip was actually a niri
/// `background-effect` frost of the whole still-mapped surface, since retired
/// with the frosted-glass experiment (#312). The floor relax is kept as harmless
/// belt-and-braces; it no longer matters, since a transparent, zone-0 overlay is
/// invisible at any width.
///
/// We tick at frame cadence until the revealer settles at the `open` target,
/// then re-set the zone + floor and `queue_resize()` / `queue_draw()` the window.
/// `queue_draw` schedules a GTK frame whose commit carries the pending
/// layer-shell state. Re-asserting at the *settled* moment guarantees the final
/// committed zone matches the final open-state, even if the last mid-slide commit
/// predated it.
///
/// A mid-animation re-toggle is detected via `open_state` and the stale timer
/// bails so two timers don't fight; the timer id is parked in `tick_slot` so
/// [`close_all`] can cancel it on teardown.
///
/// Since #1129, the settle re-assert is also where niri gets nudged to
/// reflow this monitor's active workspace: see [`should_reflow_after_close`]
/// for the exact condition and `last_zone`'s role in it.
fn drive_exclusive_zone_on_settle(
    surface: &ZoneSurface,
    open_state: &Mutable<bool>,
    tick_slot: &Rc<RefCell<Option<glib::SourceId>>>,
    last_zone: &Rc<Cell<i32>>,
    key: &str,
    open: bool,
) {
    // Helper: when the revealer has reached the target, deflate/restore the card
    // floor, lock in the zone, and force a commit. Returns whether it settled
    // (i.e. the caller can stop).
    fn reassert_if_settled(
        surface: &ZoneSurface,
        last_zone: &Cell<i32>,
        key: &str,
        open: bool,
    ) -> bool {
        let ZoneSurface {
            window,
            revealer,
            card,
        } = surface;
        if revealer.is_child_revealed() != open {
            return false;
        }
        // Relax the card's min-width floor to 0 when settled-closed so the
        // collapsed revealer can let the toplevel re-measure below the floor and
        // the persistent Top surface deflates to ~0 width; restore the
        // `scale(SIDEBAR_WIDTH)` floor when open (the AdwClamp still caps the
        // ceiling). (#194)
        let floor = if open { scale(SIDEBAR_WIDTH) } else { 0 };
        card.set_size_request(floor, -1);
        // The zone and the floor used to be one value; they are not the same
        // number any more (#737). The floor is the scaled *baseline*; the zone is
        // what the card actually measures with that floor applied, which is
        // larger whenever a child's minimum demands more. Reusing the floor here
        // is what reserved a 320 strip under a wider surface. Measured after the
        // `set_size_request` above so it sees the restored floor.
        let zone = if open { open_width(revealer) } else { 0 };
        // DIAGNOSTIC (#194): logs the settled surface geometry once per settle
        // (the re-assert), not per animation frame. NOTE: the persistent toplevel
        // stays at win_width=320 / surface_width=320 once opened (a GTK toplevel
        // won't re-measure below a prior allocation, even with the card floor
        // relaxed) — that is EXPECTED and fine. The historical grey strip was a
        // niri `background-effect` frost of this whole still-mapped surface, gone
        // now that the frosted-glass experiment is retired (#312).
        tracing::debug!(
            open,
            set_exclusive_zone = zone,
            card_floor = floor,
            revealed = revealer.is_child_revealed(),
            win_width = window.width(),
            surface_width = ?window.surface().map(|s| s.width()),
            "sidebar: exclusive-zone re-assert + card-floor deflate on settle",
        );
        window.set_exclusive_zone(zone);
        // Force a re-measure so the relaxed floor shrinks the toplevel, then a
        // GTK frame so the pending layer-shell state commits even when the
        // settled surface would otherwise draw nothing further, then flush so the
        // zone-release bytes actually leave the process (a settled-closed surface
        // may not produce another frame on its own).
        window.queue_resize();
        window.queue_draw();
        WidgetExt::display(window).flush();

        // #1129: niri stops *reserving* the strip the moment this commit
        // lands, but it only *reflows* the columns on a change about the
        // columns themselves — so a tile that was flush against the old
        // reserved edge is left exactly where it was, partly off screen.
        // Nudge it only on the genuine open→closed edge (never on open,
        // where niri already reflows its own reserve as part of opening —
        // and never on a redundant closed→closed re-assert, which a spurious
        // `open_state.set(false)` or a second immediate-settle emission for
        // an unchanged signal could otherwise re-fire).
        let previous_zone = last_zone.get();
        last_zone.set(zone);
        if should_reflow_after_close(open, previous_zone, zone)
            && let Some(workspace) = niri::active_workspace_id(key)
        {
            niri::reflow_workspace(workspace);
        }
        true
    }

    if reassert_if_settled(surface, last_zone, key, open) {
        return;
    }
    let surface = surface.clone();
    let open_state = open_state.clone();
    let last_zone = last_zone.clone();
    let key = key.to_owned();
    rearm_slide_tick(tick_slot, move || {
        // A re-toggle started a fresh tick for the new target; bail out.
        if open_state.get() != open {
            return glib::ControlFlow::Break;
        }
        if reassert_if_settled(&surface, &last_zone, &key, open) {
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Whether an exclusive-zone settle should trigger the post-close niri
/// reflow nudge (#1129).
///
/// Split out purely so it's unit-testable without a live widget tree or a
/// registered niri service — the same reason [`open_width_from_natural`] is
/// split out of [`open_width`].
///
/// `true` only on a genuine open→closed transition: `!open` (never on open —
/// niri already reflows its own reserve as part of opening, so the nudge
/// would be redundant there even if it were harmless) **and** the zone
/// actually flipped from something reserved (`previous_zone != 0`) to
/// nothing (`new_zone == 0`). A closed→closed re-assert — `previous_zone`
/// already `0` — must not re-fire: `close_all` unconditionally
/// `open_state.set(false)`s on teardown, and a second immediate-settle
/// emission for a value that didn't change is otherwise indistinguishable
/// from a real close at this call site.
fn should_reflow_after_close(open: bool, previous_zone: i32, new_zone: i32) -> bool {
    !open && new_zone == 0 && previous_zone != 0
}

/// Toggle the surface's input region so the closed sidebar's persistent
/// layer-shell surface doesn't swallow pointer events in the (revealer-
/// shrunk) sidebar region. `passthrough == true` → empty region (clicks
/// fall through to niri tiles below); `false` → full surface accepts
/// input. Mirrors the click-through pattern in `frame::install_click_through`.
fn apply_input_passthrough(window: &gtk::Window, passthrough: bool) {
    let Some(surface) = window.surface() else {
        return;
    };
    if passthrough {
        let empty = cairo::Region::create();
        surface.set_input_region(Some(&empty));
    } else {
        surface.set_input_region(None);
    }
}

/// ESC → close. Bound to the sidebar window so it fires when the sidebar has
/// keyboard focus (`KeyboardMode::OnDemand`).
///
/// Closes **this** surface's side (#1160) — an `Esc` on the focused right
/// sidebar must not close the left one behind it.
fn wire_escape(window: &gtk::Window, monitor: Monitor, side: Side) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(move |_, k, _, _| {
        if k == gdk::Key::Escape {
            toggle_side(side, &monitor);
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

/// Close every sidebar surface and drop the per-monitor entries. Called
/// before rebuilding bars on hot-plug, so stale layer-shell windows don't
/// linger after a monitor disappears.
///
/// Tears down with `destroy()`, not `close()` (#632): a sidebar never opened
/// on this monitor is still unrealized, and `close()` neither destroys an
/// unrealized window nor drops GTK's internal toplevel reference — only
/// `destroy()` does, and it can't be vetoed by a `close-request` handler.
pub fn close_all() {
    PANELS.with(|panels| {
        // `take()` moves the whole map out (leaving `Default`) and releases
        // the borrow inside the call, rather than holding a `drain()` RefMut
        // across every `destroy()` below (#631) — a borrow held across a GTK
        // call is a latent reentrancy hazard if any emission it triggers is
        // ever synchronous.
        for ((side, key), panel) in panels.take() {
            // Abort the subscription and drop the refresh timer first so neither
            // can dispatch into the (about to be closed) window. Then reset the
            // bool so any other subscribers see the closed state, and finally
            // tear down the surface.
            panel.subscription.abort();
            // The two #1160 derived feeds, aborted here for the same reason: the
            // non-empty feed could otherwise `set_visible(true)` a surface being
            // destroyed, and the effective fold would carry the
            // `open_state.set(false)` below back into the (aborted) zone and
            // visibility subscriptions.
            if let Some(handle) = panel.non_empty_subscription {
                handle.abort();
            }
            panel.effective_subscription.abort();
            // Abort the visibility subscription BEFORE forgetting the monitor, so
            // the `open_state.set(false)` below can't fire it and re-add the
            // connector we're about to forget (#288).
            panel.visibility_subscription.abort();
            // Drop this monitor from the plugin-host visibility aggregate (#288):
            // the subscriptions are aborted (so the `false` edge below won't reach
            // the host), and on a true hot-unplug this monitor is gone — so if it
            // held the only open sidebar, `visible` must drop to false. Since
            // #1160 there is one aggregate per side, so this drops the monitor
            // from the one this surface fed.
            match side {
                Side::Left => crate::plugins::forget_sidebar_visibility(&key),
                Side::Right => crate::plugins::forget_sidebar_right_visibility(&key),
            }
            // Cancel any in-flight settle timer. With the subscription aborted it
            // can't be re-armed, and without this an interrupted close tick would
            // loop forever (the closed window's revealer can never settle),
            // holding the surface alive across the hot-plug.
            if let Some(id) = panel.zone_tick.borrow_mut().take() {
                id.remove();
            }
            panel.open_state.set(false);
            panel.window.destroy();
        }
    });
    // SIDEBAR_OPEN is keyed per-monitor and deliberately survives a rebuild
    // for connector-named monitors (see the module doc's "State is
    // per-connector" note). But a connector-less monitor's fallback key is
    // the now-defunct GdkMonitor pointer: the next rebuild mints a
    // *different* pointer, so that entry can never be looked up again. Left
    // un-pruned it's a pure leak — one stale `Mutable` per hot-plug cycle
    // for every connector-less monitor.
    SIDEBAR_OPEN.with(|map| map.borrow_mut().retain(|(_, key), _| !is_fallback_key(key)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_is_320() {
        // The design baseline the card floor, the AdwClamp cap and `frame.rs`'s
        // `cutout_rect_with_sidebar_open` case are all authored against. Guard
        // against accidental edits. (What the surface *paints* is the measured
        // `open_width`, which this only bounds from below — see #737.)
        assert_eq!(SIDEBAR_WIDTH, 320);
    }

    /// The floor half of [`open_width`]: a card that measures narrower than the
    /// scaled baseline (or hasn't been laid out yet, where `measure` reports 0)
    /// still reserves the full baseline strip.
    #[test]
    fn open_width_floors_at_the_scaled_baseline() {
        // Headless: GTK isn't initialized, so `scale` is an exact no-op and the
        // floor is the bare literal (see `scale::no_op_at_default`).
        assert_eq!(open_width_from_natural(0), SIDEBAR_WIDTH);
        assert_eq!(open_width_from_natural(120), SIDEBAR_WIDTH);
        assert_eq!(open_width_from_natural(SIDEBAR_WIDTH), SIDEBAR_WIDTH);
    }

    /// The half #737 is actually about: when the card measures **wider** than the
    /// baseline — `em` padding and child minimums both grow with the effective
    /// font, and `set_size_request` only sets a minimum — the reserved zone must
    /// follow the measurement, not the literal. Falsified by the pre-#737 code,
    /// which committed `SIDEBAR_WIDTH` unconditionally and so reserved a 320 px
    /// strip under a surface painting 344, overhanging the tile beside it by 24.
    #[test]
    fn open_width_follows_a_card_wider_than_the_baseline() {
        assert_eq!(open_width_from_natural(344), 344);
        assert!(open_width_from_natural(SIDEBAR_WIDTH + 1) > SIDEBAR_WIDTH);
    }

    #[test]
    fn sidebar_open_state_is_keyed_per_connector() {
        let a = sidebar_open_state(Side::Left, "DP-1");
        let b = sidebar_open_state(Side::Left, "DP-1");
        let c = sidebar_open_state(Side::Left, "HDMI-A-1");
        // Same key → same Mutable handle (clone of the Arc inside).
        a.set(true);
        assert!(b.get());
        // Different key → independent state.
        assert!(!c.get());
    }

    // ── The right sidebar (#1158/#1160) ──────────────────────────────────────

    /// The whole geometric difference between the two surfaces: the horizontal
    /// anchor. Both keep `Top + Bottom` (full working height), so the right one
    /// really is a mirror rather than a differently-shaped surface.
    ///
    /// Asserted as a **set membership**, not as an index, because
    /// `LayerWindowBuilder::anchor` is order-insensitive — an assertion on
    /// position would fail for a reordering that changes nothing on screen.
    ///
    /// **Falsification (run):** flipping `Side::Right`'s anchor to
    /// `Anchor::Left` turns this red on the "anchors its own edge" assertion.
    #[test]
    fn each_side_anchors_its_own_edge_plus_top_and_bottom() {
        for (side, own, other) in [
            (Side::Left, Anchor::Left, Anchor::Right),
            (Side::Right, Anchor::Right, Anchor::Left),
        ] {
            let anchors = side.anchors();
            assert!(
                anchors.contains(&own),
                "{side:?} must anchor its own edge ({own:?}); got {anchors:?}"
            );
            assert!(
                !anchors.contains(&other),
                "{side:?} must not anchor the opposite edge ({other:?}); got {anchors:?} — a \
                 surface anchored to both horizontal edges spans the whole output and the \
                 exclusive zone stops meaning one strip"
            );
            assert!(
                anchors.contains(&Anchor::Top) && anchors.contains(&Anchor::Bottom),
                "{side:?} must keep the full-height Top+Bottom anchoring; got {anchors:?}"
            );
        }
    }

    /// The card slides **away from** its anchored edge. A `SlideRight` card on
    /// the right-hand surface would grow off the screen edge rather than into
    /// the workspace.
    #[test]
    fn each_side_slides_inward_from_its_own_edge() {
        assert_eq!(
            Side::Left.transition(),
            gtk::RevealerTransitionType::SlideRight
        );
        assert_eq!(
            Side::Right.transition(),
            gtk::RevealerTransitionType::SlideLeft
        );
        assert_eq!(Side::Left.halign(), gtk::Align::Start);
        assert_eq!(Side::Right.halign(), gtk::Align::End);
    }

    /// The two surfaces take different layer-shell namespaces, so a niri
    /// `layer-rule` (blur, opacity, shadow) can address one without the other —
    /// and so two surfaces on one connector cannot collide on a name.
    #[test]
    fn the_two_sides_take_different_namespaces() {
        assert_eq!(Side::Left.namespace("DP-1"), "hytte-sidebar-DP-1");
        assert_eq!(Side::Right.namespace("DP-1"), "hytte-sidebar-right-DP-1");
        assert_ne!(Side::Left.namespace("DP-1"), Side::Right.namespace("DP-1"));
    }

    /// The right side adds a CSS twin on top of the shared classes; the left
    /// keeps exactly what it always had, so no existing `.ts-sidebar` rule has
    /// to be re-specified.
    #[test]
    fn only_the_right_side_adds_a_css_twin() {
        assert_eq!(Side::Left.card_class(), None);
        assert_eq!(Side::Left.surface_class(), None);
        assert_eq!(Side::Right.card_class(), Some("ts-sidebar-right"));
        assert_eq!(
            Side::Right.surface_class(),
            Some("ts-sidebar-right-surface")
        );
    }

    /// "Hidden entirely when empty", as one value: an open right sidebar with
    /// nothing on it reveals nothing, reserves nothing and reports itself
    /// hidden to its plugins.
    ///
    /// **Falsification (run):** replacing `open && non_empty` with a bare
    /// `open` turns the third case red.
    #[test]
    fn effective_open_needs_both_intent_and_content() {
        assert!(effective_open(true, true));
        assert!(!effective_open(false, true));
        assert!(!effective_open(true, false));
        assert!(!effective_open(false, false));
    }

    /// The left sidebar's `non_empty` is a constant `true` (it carries the
    /// built-in calendar/tasks cards), so [`effective_open`] is the identity
    /// there — which is the whole argument that #1160 leaves the left side's
    /// behaviour untouched.
    #[test]
    fn the_left_side_is_unaffected_by_the_emptiness_gate() {
        for open in [false, true] {
            assert_eq!(effective_open(open, true), open);
        }
    }

    /// Per-`(side, connector)` keying: the same connector's two sidebars hold
    /// independent open state, so a right toggle cannot move the left one.
    ///
    /// **Falsification (run):** keying [`SIDEBAR_OPEN`] by the connector alone
    /// turns this red on the `!left.get()` assertion.
    #[test]
    fn the_two_sides_hold_independent_open_state_per_connector() {
        let left = sidebar_open_state(Side::Left, "DP-9");
        let right = sidebar_open_state(Side::Right, "DP-9");
        right.set(true);
        assert!(
            !left.get(),
            "flipping the right sidebar on DP-9 must not open the left one on the same output"
        );
        assert!(right.get());
        // …and the same handle comes back for the same (side, key) pair.
        assert!(sidebar_open_state(Side::Right, "DP-9").get());
        assert!(!sidebar_open_state(Side::Left, "DP-9").get());
        // Leave the thread-local as we found it: these run in one process.
        right.set(false);
    }

    /// When no sidebar surface has been installed yet (or the connector is
    /// unknown), `current_visible_width` must return `frame::FRAME_THICKNESS_I32`
    /// so the frame's cutout draws at its default left edge.
    #[test]
    fn current_visible_width_defaults_to_frame_thickness_when_no_panel() {
        // No PANELS map yet, no install() call — the frame might query us
        // during early bootstrap. Use a fake monitor key directly via the
        // private fallback path.
        assert_eq!(
            current_visible_width_for_key("nonexistent"),
            frame::FRAME_THICKNESS_I32
        );
    }

    #[test]
    fn is_settled_defaults_to_true_when_no_panel() {
        // Same situation: no panel installed → nothing animating → settled.
        assert!(is_settled_for_key("nonexistent"));
    }

    // ── Post-close niri reflow (#1129) ───────────────────────────────────────

    /// The one case the nudge exists for: a settle that just committed the
    /// closed (`0`) zone, coming from a previously-reserved one.
    #[test]
    fn reflows_on_a_genuine_open_to_closed_transition() {
        assert!(should_reflow_after_close(false, 320, 0));
    }

    /// Mutation guard ("fire twice"): a closed→closed re-assert — the
    /// previous commit was *already* `0` — must not re-fire. This is what
    /// keeps a spurious `close_all` `open_state.set(false)`, or a second
    /// immediate-settle emission for a value that didn't change, from
    /// nudging niri a second time for the same close.
    #[test]
    fn does_not_reflow_when_already_closed() {
        assert!(!should_reflow_after_close(false, 0, 0));
    }

    /// Mutation guard ("fire on open"): opening must never nudge, even in the
    /// contrived case where the computed zone happens to land on `0` — this
    /// is what falsifies a mutation that drops the `!open` term and keeps
    /// only the zone comparison.
    #[test]
    fn does_not_reflow_on_open() {
        assert!(!should_reflow_after_close(true, 0, 320));
        assert!(!should_reflow_after_close(true, 320, 0));
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::time::Duration;

    use super::{
        PANELS, SIDEBAR_WIDTH, Side, SidebarPanel, build_clamped_scroller, build_revealer,
        effective_open, on_map_or_now, open_width, sidebar_open_state, toggle_right_on_focused,
        wire_non_empty,
    };
    use crate::scale::scale;
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, glib};

    /// Rows in the stand-in hive card: Mara's live-test case, the one that
    /// pushed the pet card off the bottom of the screen (#963 → #965).
    const HIVE_ROWS: i32 = 12;
    /// Height of one such row, in px. `HIVE_ROWS * ROW_HEIGHT` has to exceed
    /// [`SURFACE_HEIGHT`] by enough that the last row is unambiguously below
    /// the viewport.
    const ROW_HEIGHT: i32 = 60;
    /// Stand-in for the layer surface's usable height (a short display, or a
    /// tall one with a lot of cards above this one).
    const SURFACE_HEIGHT: i32 = 300;

    /// Run the GTK main loop until it has nothing left to dispatch, so a queued
    /// resize/allocation actually happens. Same helper, same reason, as
    /// `widgets::mpris`'s geometry tests.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Drive the GTK main loop until `done()` holds, or `ms` of wall clock has
    /// passed.
    ///
    /// Needed on top of [`pump`] for anything that only takes effect on a
    /// **frame**: a scroll queues an allocation on the viewport, and the queue
    /// is drained by a `GdkFrameClock` tick. `iteration(true)` blocks until a
    /// source is ready, which is what lets that tick arrive — a spin on
    /// `iteration(false)` starves the clock and measures the test's own loop
    /// (the scroll then looks like it simply never happened). The deadline is
    /// what guarantees termination when the frame the test wants never comes.
    /// Same shape, and the same reason, as `plugins::region`'s `pump_for`.
    fn pump_until(ms: u64, done: impl Fn() -> bool) {
        let expired = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = expired.clone();
        gtk::glib::timeout_add_local_once(Duration::from_millis(ms), move || flag.set(true));
        while !expired.get() && !done() {
            gtk::glib::MainContext::default().iteration(true);
        }
    }

    /// The `.ts-sidebar` card stack `build_card` builds, minus the parts that
    /// need a live `Monitor` and the service registry (its calendar/tasks/plugin
    /// slots).
    ///
    /// `content_min` is the minimum width of a stand-in child, standing for
    /// whatever a real card's contents demand — an `em`-padded calendar grid, a
    /// plugin card, a long event title. `rows` stacks that many fixed-height
    /// blocks below it, standing for a list-shaped card: the agents hive, a
    /// departures board with a full timetable.
    fn card(content_min: i32, rows: i32) -> gtk::Box {
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("ts-sidebar");
        card.set_size_request(scale(SIDEBAR_WIDTH), -1);
        card.set_valign(gtk::Align::Fill);
        card.set_vexpand(true);
        if content_min > 0 {
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.set_size_request(content_min, -1);
            card.append(&content);
        }
        for _ in 0..rows {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
            row.set_size_request(-1, ROW_HEIGHT);
            card.append(&row);
        }
        card
    }

    /// The **pre-#965** tree: revealer → `AdwClamp` → `child`, with no scroller
    /// in between. Kept for exactly one job — the unscrolled control in
    /// [`the_scroller_changes_no_width_measurement`] — so the width the shipped
    /// tree measures is pinned against what the tree before this change
    /// measured, rather than only against itself. Everything else goes through
    /// [`scrolled_tree`].
    fn revealer_over(child: &impl IsA<gtk::Widget>) -> gtk::Revealer {
        let clamp = adw::Clamp::builder()
            .maximum_size(scale(SIDEBAR_WIDTH))
            .tightening_threshold(scale(SIDEBAR_WIDTH))
            .child(child)
            .build();
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        revealer.set_child(Some(&clamp));
        revealer
    }

    /// The **shipped** tree: revealer → `AdwClamp` → `build_scroller` → card
    /// (#965), assembled by `install`'s own [`build_clamped_scroller`] rather
    /// than re-spelled here — so these tests measure both the scroller's
    /// configuration *and* where it is mounted. While the clamp was assembled
    /// inline in `install`, splicing the bare card into it (the fix reverted)
    /// left every test in this module green.
    ///
    /// Hands back the scroller too, by asking the clamp for its child: a
    /// mounting that stops going through the scroller fails right here, at the
    /// seam, rather than somewhere downstream.
    fn scrolled_tree(card: &gtk::Box) -> (gtk::Revealer, gtk::ScrolledWindow) {
        let clamp = build_clamped_scroller(card);
        let scroller = clamp
            .child()
            .and_then(|child| child.downcast::<gtk::ScrolledWindow>().ok())
            .expect(
                "install mounts the card stack inside a ScrolledWindow inside the clamp (#965)",
            );
        let revealer = gtk::Revealer::new();
        revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
        revealer.set_child(Some(&clamp));
        (revealer, scroller)
    }

    /// [`scrolled_tree`] for the width tests, which only need the revealer.
    fn tree(content_min: i32) -> gtk::Revealer {
        scrolled_tree(&card(content_min, 0)).0
    }

    /// A card whose contents fit inside the baseline reserves exactly the
    /// baseline — the 1x case, which stays pixel-identical to the constant this
    /// replaced. Compared against `scale(SIDEBAR_WIDTH)` rather than the bare
    /// literal because `#[gtk::test]` *does* initialize GTK, so `scale` is only
    /// a no-op if the harness' font happens to sit at the baseline.
    #[gtk::test]
    fn narrow_card_reserves_the_baseline() {
        adw::init().expect("libadwaita init");
        let revealer = tree(0);
        revealer.set_reveal_child(true);
        assert_eq!(open_width(&revealer), scale(SIDEBAR_WIDTH));
    }

    /// #737's regression: a card whose contents demand more than the baseline
    /// paints wider than the baseline (`set_size_request` is a *minimum*, and
    /// `AdwClamp` cannot tighten a child below its own minimum), so the zone
    /// derived from it has to be wider too. The pre-#737 code committed
    /// `SIDEBAR_WIDTH` here — reserving a 320 px strip under a surface painting
    /// 100 px more, which overhung the tile beside it and covered that window's
    /// left border and rounded corners.
    #[gtk::test]
    fn wide_card_reserves_what_it_paints() {
        adw::init().expect("libadwaita init");
        // Authored relative to the floor so the case stays meaningful (rather
        // than trivially satisfied) whatever font the harness runs with.
        let wide = scale(SIDEBAR_WIDTH) + 100;
        let revealer = tree(wide);
        revealer.set_reveal_child(true);
        let got = open_width(&revealer);
        assert!(
            got >= wide,
            "a card whose contents demand {wide} px paints {wide} px; the exclusive zone must \
             reserve at least that, not the {} px baseline (got {got})",
            scale(SIDEBAR_WIDTH)
        );
    }

    /// #965: a card taller than the surface must **scroll**, not overflow.
    ///
    /// Mara's live-test case (#963): a hive with 12 agents grew the card past
    /// the screen and everything below it — the pet card — was cut off with no
    /// way to reach it. The pre-#965 tree had no scroller at all, so GTK
    /// allocated the stack its full minimum height and the surface simply
    /// clipped the excess.
    ///
    /// Asserts the rectangle and the hit, never `is_visible()` (#851/#838): the
    /// last row is `visible` in both states here — that flag is orthogonal to
    /// being on-screen, which is the whole reason this bug shipped invisible to
    /// a visibility-based test.
    #[gtk::test]
    fn a_tall_card_scrolls_instead_of_hiding_the_cards_below_it() {
        adw::init().expect("libadwaita init");
        let stack = card(0, HIVE_ROWS);
        let last = stack.last_child().expect("the stand-in hive card has rows");
        let (revealer, scroller) = scrolled_tree(&stack);
        revealer.set_reveal_child(true);

        let window = gtk::Window::new();
        window.set_child(Some(&revealer));
        window.set_default_size(scale(SIDEBAR_WIDTH), SURFACE_HEIGHT);
        window.present();
        pump();

        // Every measurement is taken in the scroller's coordinate space: it is
        // the widget whose allocation clips (a `GtkScrolledWindow` viewport is
        // `GTK_OVERFLOW_HIDDEN`), so "inside the viewport" is a statement about
        // this rectangle and no other.
        let viewport = f64::from(scroller.height());
        let centre_of = |b: &gtk::graphene::Rect| {
            (
                f64::from(b.x()) + f64::from(b.width()) / 2.0,
                f64::from(b.y()) + f64::from(b.height()) / 2.0,
            )
        };
        let hits_last = |x: f64, y: f64| {
            scroller
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == last || w.is_ancestor(&last))
        };

        let before = last
            .compute_bounds(&scroller)
            .expect("the last row is a descendant of the scroller");
        let (bx, by) = centre_of(&before);
        let (before_top, before_hit) = (f64::from(before.y()), hits_last(bx, by));
        let visible_before = last.is_visible();

        let vadj = scroller.vadjustment();
        // `set_value` clamps to `upper - page_size`, i.e. the bottom of the
        // scrollable range — asking for `upper` is asking to scroll to the end.
        vadj.set_value(vadj.upper());
        // The scroll only lands on a frame (it queues an allocation on the
        // viewport), so this waits for one rather than spinning.
        pump_until(2000, || {
            last.compute_bounds(&scroller)
                .is_some_and(|b| f64::from(b.y()) < viewport)
        });
        let (value, upper, page) = (vadj.value(), vadj.upper(), vadj.page_size());

        let after = last
            .compute_bounds(&scroller)
            .expect("the last row is still a descendant of the scroller");
        let (ax, ay) = centre_of(&after);
        let (after_top, after_bottom) =
            (f64::from(after.y()), f64::from(after.y() + after.height()));
        let after_hit = hits_last(ax, ay);
        let visible_after = last.is_visible();
        // Measured here, on a *realized* tree with a live scrollbar, because
        // that is the production case: the surface is mapped whenever
        // `open_width` runs. `the_scroller_changes_no_width_measurement` pins
        // the same number on unrealized trees, where a scrollbar that only
        // materializes on realize would go unnoticed.
        let width_while_scrolling = open_width(&revealer);

        window.set_child(None::<&gtk::Widget>);
        window.destroy();

        let content = f64::from(ROW_HEIGHT * HIVE_ROWS);
        assert!(
            viewport > 0.0 && viewport < content,
            "test setup: the viewport is {viewport} px for {content} px of cards — the case only \
             means something when the stack is taller than the surface"
        );
        assert!(
            visible_before && visible_after,
            "test setup: the last row must be `visible` in BOTH states (before={visible_before}, \
             after={visible_after}) — that is what makes the assertions below statements about \
             geometry rather than about the visible flag, which is orthogonal to being on-screen \
             and is why this bug shipped (#851/#838)"
        );
        assert!(
            before_top >= viewport && !before_hit,
            "before scrolling, the last card must lie below the {viewport} px viewport and be \
             unreachable there (top={before_top}, picked={before_hit})"
        );
        assert!(
            after_top >= 0.0 && after_bottom <= viewport,
            "after scrolling to the bottom, the last card must lie inside the {viewport} px \
             viewport — this is the #965 bug: with no scroller it never gets there \
             (top={after_top}, bottom={after_bottom}; vadjustment value={value} upper={upper} \
             page={page})"
        );
        assert!(
            after_hit,
            "a click at the centre of the scrolled-to last card must land on it \
             (bounds y={after_top}..{after_bottom} in a {viewport} px viewport)"
        );
        assert_eq!(
            width_while_scrolling,
            scale(SIDEBAR_WIDTH),
            "a mapped, actively scrolling sidebar must still reserve exactly the baseline: a \
             scrollbar allocated beside the content instead of drawn over it would widen the \
             exclusive zone the moment a card grew tall (#737/#965)"
        );
    }

    /// The width invariant #965 must not disturb: the measured open width is
    /// **identical** with and without the scroller, for a short card and for the
    /// 12-row hive alike.
    ///
    /// Both halves matter and they fail to different mutations:
    ///
    /// * tall == short is the one the issue asks for — vertical content must not
    ///   leak into the horizontal measurement (a non-overlay scrollbar appearing
    ///   only once the content overflows would do exactly that).
    /// * scrolled == unscrolled is the stronger statement, and the one that
    ///   would catch a scrollbar allocated *unconditionally* (which the
    ///   tall-vs-short half cannot see, being equally wrong on both sides): it
    ///   pins the number to what the pre-#965 tree measured rather than merely
    ///   to itself.
    ///
    /// Both halves are measured on **unrealized** trees, which is what
    /// `a_tall_card_scrolls_instead_of_hiding_the_cards_below_it`'s closing
    /// width assertion complements: that one measures a mapped surface with a
    /// live scrollbar.
    #[gtk::test]
    fn the_scroller_changes_no_width_measurement() {
        adw::init().expect("libadwaita init");
        let wide = scale(SIDEBAR_WIDTH) + 100;
        let mut widths = Vec::new();
        for (content_min, rows) in [(0, 0), (0, HIVE_ROWS), (wide, 0), (wide, HIVE_ROWS)] {
            let scrolled = scrolled_tree(&card(content_min, rows)).0;
            // The control stays a hand-built **pre-#965** tree — the clamp over
            // the bare card — which is the whole point of it.
            let plain = revealer_over(&card(content_min, rows));
            scrolled.set_reveal_child(true);
            plain.set_reveal_child(true);
            let (got, want) = (open_width(&scrolled), open_width(&plain));
            assert_eq!(
                got, want,
                "a {content_min} px-minimum card with {rows} rows measures {want} px without the \
                 scroller and {got} px with it — the scroller must be invisible to the exclusive \
                 zone (#737/#965)"
            );
            widths.push(got);
        }
        assert_eq!(
            widths[0], widths[1],
            "a short card and a {HIVE_ROWS}-row card must reserve the same width; stacking cards \
             is a vertical fact and must not widen the sidebar (#965)"
        );
        assert_eq!(
            widths[2], widths[3],
            "same for a card already wider than the baseline: {} px vs {} px",
            widths[2], widths[3]
        );
    }

    /// The flex-gap contract `build_card` documents survives the viewport: a
    /// stack shorter than the surface is still stretched to the full viewport
    /// height, so its `vexpand` spacer keeps the bottom plugin region pinned to
    /// the bottom edge instead of letting it float mid-sidebar.
    #[gtk::test]
    fn a_short_card_still_fills_the_viewport() {
        adw::init().expect("libadwaita init");
        let stack = card(0, 1);
        let (revealer, scroller) = scrolled_tree(&stack);
        revealer.set_reveal_child(true);
        let window = gtk::Window::new();
        window.set_child(Some(&revealer));
        window.set_default_size(scale(SIDEBAR_WIDTH), SURFACE_HEIGHT);
        window.present();
        pump();
        let (stack_h, viewport_h) = (stack.height(), scroller.height());
        window.set_child(None::<&gtk::Widget>);
        window.destroy();
        assert!(
            viewport_h > ROW_HEIGHT,
            "test setup: the viewport ({viewport_h} px) must be taller than the one {ROW_HEIGHT} \
             px row in the stack"
        );
        assert_eq!(
            stack_h, viewport_h,
            "a short card must still be allocated the whole viewport, or the sidebar's flex gap \
             stops anchoring the bottom plugin region to the bottom edge"
        );
    }

    /// [`open_width`] measures the revealer's **child**, so it is the same
    /// number before, during and after the slide. Measuring the revealer itself
    /// would multiply by the animation position and report the baseline floor
    /// here — precisely the stale 320 this fix exists to stop committing, and it
    /// would make the zone depend on *when* during the transition it was read.
    #[gtk::test]
    fn open_width_is_independent_of_the_slide_position() {
        adw::init().expect("libadwaita init");
        let wide = scale(SIDEBAR_WIDTH) + 100;
        let revealer = tree(wide);
        // Never revealed: the revealer's own horizontal measure is 0 here.
        let collapsed = open_width(&revealer);
        revealer.set_reveal_child(true);
        assert_eq!(collapsed, open_width(&revealer));
        assert!(collapsed >= wide, "got {collapsed}");
    }

    // ── The right sidebar's map rule and its toggle (#1158/#1160) ────────────

    /// A stand-in for the layer surface. A bare `gtk::Window` is the same
    /// substitution `hytte-ui`'s own `on_surface_ready` tests make, and for the
    /// same reason: what is under test — has this toplevel been mapped yet —
    /// is plain GTK toplevel mechanics, not a layer-shell one. What genuinely
    /// needs niri (the surface really landing on the right edge, below the bar,
    /// with the strip reserved) is in `docs/live-verify.md`.
    ///
    /// Deliberately never presented by the test itself: every `is_mapped()`
    /// assertion below is then a statement about [`wire_non_empty`] and nothing
    /// else.
    fn surface() -> gtk::Window {
        let window = gtk::Window::new();
        // A default size so the toplevel is mappable at all on the headless
        // display; nothing here reads it.
        window.set_default_size(scale(SIDEBAR_WIDTH), 300);
        window
    }

    /// Drive the main loop until `done` holds or `ms` of wall clock passes —
    /// the module's own `pump_until`, re-stated here because mapping a toplevel
    /// lands on a frame rather than on a plain iteration.
    fn settle(ms: u64, done: impl Fn() -> bool) {
        let expired = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = expired.clone();
        gtk::glib::timeout_add_local_once(Duration::from_millis(ms), move || flag.set(true));
        while !expired.get() && !done() {
            gtk::glib::MainContext::default().iteration(true);
        }
    }

    /// The headline of #1158's "hidden entirely when empty": with no card on
    /// this output's right side, the surface is **never mapped** — not mapped
    /// and transparent, not mapped and zero-width. Asserted through
    /// `is_mapped()` on the window, never through `is_visible()` on a widget,
    /// which is orthogonal to being on screen (#851).
    ///
    /// **Falsification (run):** dropping the `if has_card` guard in
    /// [`wire_non_empty`] (mapping unconditionally on the first emission) turns
    /// this red — `the right sidebar must not be mapped while its regions are
    /// empty`.
    #[gtk::test]
    fn an_empty_right_sidebar_is_never_mapped() {
        adw::init().expect("libadwaita init");
        let window = surface();
        let non_empty = Mutable::new(false);
        let source = Mutable::new(false);
        let handle = wire_non_empty(&window, &non_empty, source.signal());

        // Give the subscription every chance to map: it has emitted (`false`),
        // and the loop has run to quiescence.
        settle(300, || false);

        let mapped = window.is_mapped();
        handle.abort();
        window.destroy();
        assert!(
            !mapped,
            "the right sidebar must not be mapped while its regions are empty — a shell with no \
             right-mounted plugin should put no surface on screen at all (#1158)"
        );
        assert!(!non_empty.get());
    }

    /// …and it maps the moment a card shows, so a plugin placed right actually
    /// has something to paint on.
    ///
    /// **Falsification (run):** deleting the `window.set_visible(true)` inside
    /// [`wire_non_empty`] turns this red.
    #[gtk::test]
    fn the_right_sidebar_maps_on_its_first_card() {
        adw::init().expect("libadwaita init");
        let window = surface();
        let non_empty = Mutable::new(false);
        let source = Mutable::new(false);
        let handle = wire_non_empty(&window, &non_empty, source.signal());
        settle(300, || false);
        assert!(!window.is_mapped(), "test setup: starts unmapped");

        source.set(true);
        settle(2000, || window.is_mapped());

        let (mapped, mirrored) = (window.is_mapped(), non_empty.get());
        handle.abort();
        window.destroy();
        assert!(
            mapped,
            "the first card on this output's right side must map the surface (#1160)"
        );
        assert!(
            mirrored,
            "the non-empty flag must mirror the region feed — `toggle_right_on_focused` reads it \
             synchronously to decide whether a toggle is a no-op"
        );
    }

    /// The map is a **latch**: a right sidebar whose last plugin disconnects
    /// stays mapped rather than unmapping and re-mapping on the next one.
    ///
    /// This is deliberate and is the left sidebar's own rule (#212, and this
    /// module's z-order note): `Layer::Top` orders by surface creation, so a
    /// re-present would re-stack the surface above the bar. An empty
    /// already-mapped right sidebar is invisible the same way a closed one is.
    #[gtk::test]
    fn a_right_sidebar_that_empties_again_stays_mapped() {
        adw::init().expect("libadwaita init");
        let window = surface();
        let non_empty = Mutable::new(false);
        let source = Mutable::new(true);
        let handle = wire_non_empty(&window, &non_empty, source.signal());
        settle(2000, || window.is_mapped());
        assert!(window.is_mapped(), "test setup: mapped by the first card");

        source.set(false);
        settle(300, || false);

        let (mapped, mirrored) = (window.is_mapped(), non_empty.get());
        handle.abort();
        window.destroy();
        assert!(
            mapped,
            "the surface must stay mapped once created — re-presenting a Layer::Top surface \
             re-stacks it above the bar (#212)"
        );
        assert!(
            !mirrored,
            "the flag still has to follow the feed, so the toggle goes back to being a no-op"
        );
    }

    /// The #212 ordering, as a test rather than as prose (#1244 review,
    /// finding 2).
    ///
    /// A persistent layer surface maps synchronously inside its one
    /// `set_visible(true)` and never remaps, so anything that touches the
    /// surface has to be wired **before** that call or it silently never
    /// applies. [`install_side`] wires the input region through
    /// [`on_map_or_now`] for exactly that reason, and on the right sidebar the
    /// gap between wiring and map is unbounded — the surface waits for its first
    /// card.
    ///
    /// Nothing held that. Replacing `install_side`'s wiring with the pre-#1160
    /// bare `apply_input_passthrough(&window, true)` — deleting the map hook the
    /// right surface depends on — left 843 hermetic and 1033 gated tests green.
    /// `hytte-ui`'s own tests pin the *helper's* contract; this pins that this
    /// module's wiring defers.
    ///
    /// Asserts both halves, because only the pair distinguishes "deferred" from
    /// "ran twice": nothing applies while the window has no surface, and what
    /// does apply, applies with the window **mapped**. A bare `gtk::Window`
    /// stands in, the same substitution `hytte-ui` makes — "has this toplevel
    /// been mapped yet" is plain GTK mechanics.
    ///
    /// What it deliberately does **not** assert is the input region itself: GDK
    /// exposes no getter for one, so what was applied stays a code-path argument
    /// plus a live-verify item. *When* it was applied is the part that was
    /// unheld, and it is the part that regressed in #192/#193/#212.
    ///
    /// **Falsification (run):** making [`on_map_or_now`] call `apply`
    /// immediately (the pre-#1160 eager shape) turns this red on the
    /// before-the-map assertion.
    #[gtk::test]
    fn the_input_region_is_wired_before_the_first_map() {
        adw::init().expect("libadwaita init");
        let window = surface();
        // `Option<bool>`: `None` = never ran, `Some(mapped)` = ran, with the
        // window's map state at that moment. One cell answers both halves.
        let stamp: std::rc::Rc<std::cell::Cell<Option<bool>>> =
            std::rc::Rc::new(std::cell::Cell::new(None));
        let stamp_for_apply = stamp.clone();
        on_map_or_now(&window, move |w| stamp_for_apply.set(Some(w.is_mapped())));

        // Wired, but the window has never been shown: there is no surface to
        // touch yet, so nothing must have run. This is the assertion an eager
        // apply fails — and the one that makes the `Some(true)` below mean
        // "deferred to the map" rather than "ran at some point".
        settle(200, || false);
        assert_eq!(
            stamp.get(),
            None,
            "nothing may be applied to the surface before the window is shown — there is no \
             surface yet (`window.surface()` is None), so an eager apply is a silent no-op that \
             never happens again (#212)"
        );

        window.set_visible(true);
        settle(2000, || stamp.get().is_some());
        let ran = stamp.get();
        window.destroy();
        assert_eq!(
            ran,
            Some(true),
            "the wiring must run on the window's one map, with the window mapped — that is the \
             only moment a persistent layer surface offers (#192/#193/#212)"
        );
    }

    /// A `SidebarPanel` with no live subscriptions, for the [`PANELS`]-driven
    /// toggle tests. The two `JoinHandle`s are inert futures: nothing here
    /// exercises the zone/visibility machinery, only the toggle's own decision.
    fn panel(side: Side, key: &str, non_empty: bool) -> SidebarPanel {
        let idle = || glib::MainContext::default().spawn_local(std::future::ready(()));
        SidebarPanel {
            window: surface(),
            revealer: build_revealer(side),
            open_state: sidebar_open_state(side, key),
            subscription: idle(),
            visibility_subscription: idle(),
            non_empty_subscription: None,
            effective_subscription: idle(),
            zone_tick: std::rc::Rc::new(std::cell::RefCell::new(None)),
            non_empty: Mutable::new(non_empty),
        }
    }

    /// Install both sides' panels for one connector and hand back a guard that
    /// clears [`PANELS`] again — these tests share one thread-local with every
    /// other test in this binary.
    fn with_panels(key: &str, right_non_empty: bool, body: impl FnOnce()) {
        PANELS.with(|panels| {
            let mut panels = panels.borrow_mut();
            panels.insert((Side::Left, key.to_owned()), panel(Side::Left, key, true));
            panels.insert(
                (Side::Right, key.to_owned()),
                panel(Side::Right, key, right_non_empty),
            );
        });
        body();
        PANELS.with(|panels| panels.borrow_mut().clear());
        sidebar_open_state(Side::Left, key).set(false);
        sidebar_open_state(Side::Right, key).set(false);
    }

    /// A toggle aimed at an empty right sidebar does **nothing** — it does not
    /// flip the open state, so the surface cannot "open by itself" later when an
    /// unrelated plugin dials in.
    ///
    /// **Falsification (run):** deleting the `if !non_empty { … return; }` guard
    /// in [`toggle_right_on_focused`] turns this red.
    #[gtk::test]
    fn a_toggle_on_an_empty_right_sidebar_is_a_no_op() {
        adw::init().expect("libadwaita init");
        with_panels("DP-EMPTY", false, || {
            let right = sidebar_open_state(Side::Right, "DP-EMPTY");
            assert!(!right.get(), "test setup: starts closed");
            toggle_right_on_focused(Some("DP-EMPTY"));
            assert!(
                !right.get(),
                "a right-sidebar toggle on an output with no right-mounted card must not flip the \
                 open state — otherwise the surface springs open the moment any plugin arrives"
            );
        });
    }

    /// …but an **open** right sidebar can always be dismissed, even after its
    /// last card has gone away (#1244 review, finding 1).
    ///
    /// The full round trip the reviewer's probe describes, because the bug is
    /// only visible at the end of it: open with a card present → the card goes
    /// away (the plugin exits, renders an empty tree, or hides on this output)
    /// → the user presses the keybind to dismiss it → a card comes back. With
    /// the guard keyed on emptiness alone the third step was a no-op and the
    /// fourth slid the sidebar open by itself — exactly the "opens by itself"
    /// failure the refusal exists to prevent, reached from the other side.
    ///
    /// This is the case the other two toggle tests could not see:
    /// `a_toggle_on_an_empty_right_sidebar_is_a_no_op` pins the refusal at
    /// `open == false`, `effective_open_needs_both_intent_and_content` pins the
    /// fold, and they never meet at `open == true && non_empty == false`.
    ///
    /// **Falsification (run):** restoring the unconditional `if !non_empty { …
    /// return; }` turns this red on "must be dismissable".
    #[gtk::test]
    fn an_open_right_sidebar_can_be_closed_after_its_last_card_leaves() {
        adw::init().expect("libadwaita init");
        // `with_panels`'s `right_non_empty = false` is the *after* state: the
        // card is already gone by the time the user reaches for the keybind.
        with_panels("DP-EMPTIED", false, || {
            let right = sidebar_open_state(Side::Right, "DP-EMPTIED");
            // Opened while a card was still there.
            right.set(true);

            toggle_right_on_focused(Some("DP-EMPTIED"));
            assert!(
                !right.get(),
                "an open right sidebar whose last card went away must be dismissable — otherwise \
                 the open intent stays latched with nothing on screen to explain it"
            );

            // The half that makes it a bug rather than a cosmetic refusal: a
            // card coming back must NOT re-open a sidebar the user dismissed.
            // `effective_open(open, non_empty)` is what the surface acts on, so
            // this is the value that would have slid it open.
            assert!(
                !effective_open(right.get(), true),
                "a card returning must not re-open a sidebar the user closed"
            );

            // …and the refusal is still in force for a fresh open attempt.
            toggle_right_on_focused(Some("DP-EMPTIED"));
            assert!(
                !right.get(),
                "with the latch cleared, an empty right sidebar refuses to open again"
            );
        });
    }

    /// …and it is a real toggle once there is a card.
    #[gtk::test]
    fn a_toggle_on_a_non_empty_right_sidebar_opens_it() {
        adw::init().expect("libadwaita init");
        with_panels("DP-FULL", true, || {
            let right = sidebar_open_state(Side::Right, "DP-FULL");
            toggle_right_on_focused(Some("DP-FULL"));
            assert!(right.get(), "a right sidebar with a card must open");
            toggle_right_on_focused(Some("DP-FULL"));
            assert!(!right.get(), "…and close again");
        });
    }

    /// The two sides are independent surfaces: a right toggle must leave the
    /// left sidebar exactly as it was, open or closed.
    ///
    /// **Falsification (run):** keying [`super::SIDEBAR_OPEN`] by the connector
    /// alone (dropping the `Side` half) turns this red on both assertions.
    #[gtk::test]
    fn a_right_toggle_leaves_the_left_sidebar_alone() {
        adw::init().expect("libadwaita init");
        with_panels("DP-BOTH", true, || {
            let left = sidebar_open_state(Side::Left, "DP-BOTH");
            let right = sidebar_open_state(Side::Right, "DP-BOTH");

            // Closed left stays closed.
            toggle_right_on_focused(Some("DP-BOTH"));
            assert!(right.get());
            assert!(!left.get(), "a right toggle must not open the left sidebar");

            // Open left stays open — the interesting direction, since "both
            // sidebars open at once" is one of the epic's live-verify items.
            left.set(true);
            toggle_right_on_focused(Some("DP-BOTH"));
            assert!(!right.get(), "test setup: the right one closed again");
            assert!(
                left.get(),
                "a right toggle must not close an open left sidebar — the two surfaces are \
                 independent and are meant to be usable together"
            );
        });
    }

    /// A right toggle on a shell where only the left sidebar is installed
    /// resolves to nothing at all, rather than falling back to the left
    /// connector's entry and flipping the wrong surface.
    #[gtk::test]
    fn a_right_toggle_with_no_right_surface_installed_does_nothing() {
        adw::init().expect("libadwaita init");
        PANELS.with(|panels| {
            panels.borrow_mut().insert(
                (Side::Left, "DP-LEFTONLY".to_owned()),
                panel(Side::Left, "DP-LEFTONLY", true),
            );
        });
        let left = sidebar_open_state(Side::Left, "DP-LEFTONLY");
        // No preferred output either, so the "any installed surface" fallback is
        // the path under test.
        toggle_right_on_focused(None);
        let opened = left.get();
        PANELS.with(|panels| panels.borrow_mut().clear());
        left.set(false);
        assert!(
            !opened,
            "`toggle_right_on_focused`'s fallback must only consider Side::Right surfaces"
        );
    }
}
