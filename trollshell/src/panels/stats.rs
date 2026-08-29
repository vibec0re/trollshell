//! System stats drawer panel — three runtime-selectable layouts of the same
//! five per-resource cards (CPU / Memory / GPU / Disks / Services), chosen by
//! the `TROLLSHELL_STATS_LAYOUT` env var (read once via [`stats_layout`]):
//!
//! - **`combined`** — all five cards stacked in one page
//!   ([`panel_stats`]); every resource chip opens it. This is the #508 restore
//!   of the pre-#307 shape.
//! - **`multicolumn`** (default) — the same five cards in a 2-column grid
//!   ([`panel_stats_multicolumn`]) under a wider Stats-specific clamp; chips
//!   still open the one `Page::Stats`.
//! - **`split`** — @kaesaecracker's five per-chip single-card pages
//!   ([`panel_stats_cpu`] … [`panel_stats_services`]) restored from #307's
//!   `e78d11e` alongside the combined page; each chip opens its own page.
//!
//! The per-resource cards are the `build_stats_*` builders below; every layout
//! shares them. The option is wired as a nix `programs.trollshell.stats.layout`
//! enum → `TROLLSHELL_STATS_LAYOUT` session var (#508; see the #508 thread for
//! the "first pure-UI-layout knob" philosophy note, and #307/#518 for the
//! split↔combined lineage).
//!
//! #516 adds scroll-to-section precision to the combined and multicolumn
//! layouts: each resource chip stashes a [`StatsSection`] via
//! [`set_scroll_target`] just before it opens/re-shows the drawer, and the page
//! — wrapped in a `ScrolledWindow` here so stacked cards don't run past screen
//! height — scrolls its own target card to the top. The mechanism is
//! `compute_bounds`-based and container-agnostic, so it works against both the
//! combined page's `gtk::Box` column and the multicolumn `gtk::Grid`. See
//! [`StatsSection`] for how `crate::modal` triggers the (re)scroll.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, gio, glib};
use hytte::prelude::*;
use hytte::reactive::health::{self, TaskHealth, TaskState};
use hytte::services::app_usage::{self, ProcSample};
use hytte::services::sensors::{self, CpuFreq, CpuLoad};
use hytte::services::systemd;
use hytte::ui::{MultiSparkline, PixelSurface};
use hytte_preem::{ColorMap, DisplayStyle, Fill, LedMatrix};

use crate::components::cast;
use crate::components::format::{fmt_bytes, fmt_hz, fmt_rate};
use crate::components::history_row::build_history_row;
use crate::components::layout::{
    DRAWER_MAX_WIDTH_WIDE, finish_page, finish_page_clamped, page_box, page_grid,
};
use crate::components::markup;
use crate::components::monitor_key::monitor_key;
use crate::components::reactive_list::reactive_list;

/// One card in the combined Stats page (#516). Named after the resource chip
/// that scrolls to it, in the same top-to-bottom order [`panel_stats`] stacks
/// the cards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatsSection {
    Cpu,
    Memory,
    Gpu,
    Disks,
    Services,
}

/// Which Stats-drawer layout the shell was launched with (#508). Chosen once at
/// startup by the `TROLLSHELL_STATS_LAYOUT` env var via [`stats_layout`]; the
/// chips, the drawer routing (`crate::modal::build_page`), the centering clamp,
/// and the plugin wire mapping all branch on it. See the module docs for what
/// each layout renders.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatsLayout {
    /// All five cards stacked in one page — today's/pre-#307 shape.
    Combined,
    /// The five cards in a 2-column grid under a wider clamp (default).
    Multicolumn,
    /// Five per-chip single-card pages (#307's split, restored in #508).
    Split,
}

/// Parse a raw `TROLLSHELL_STATS_LAYOUT` value into a [`StatsLayout`].
///
/// `None` (unset) resolves to [`StatsLayout::Multicolumn`] silently — multicolumn
/// is the documented default and, like `TROLLSHELL_RECORD_AUDIO` /
/// `TROLLSHELL_WEATHER_CITY`, an unset override reads as its default rather than
/// a misconfiguration. A present-but-unrecognized value returns `Err(other)` so
/// [`stats_layout`] can warn once and fall back to multicolumn.
fn parse_stats_layout(raw: Option<&str>) -> Result<StatsLayout, &str> {
    match raw {
        None | Some("multicolumn") => Ok(StatsLayout::Multicolumn),
        Some("combined") => Ok(StatsLayout::Combined),
        Some("split") => Ok(StatsLayout::Split),
        Some(other) => Err(other),
    }
}

/// The Stats-drawer layout for this process, read once from
/// `TROLLSHELL_STATS_LAYOUT` and cached (#508). The value is process-constant —
/// a session env var — so it's resolved on first use and reused everywhere
/// (chips, drawer routing, the plugin wire mapping) rather than re-read per
/// click. A present-but-unrecognized value warns once (via the `OnceLock`
/// init running exactly once) and falls back to [`StatsLayout::Multicolumn`].
pub fn stats_layout() -> StatsLayout {
    use std::sync::OnceLock;
    static LAYOUT: OnceLock<StatsLayout> = OnceLock::new();
    *LAYOUT.get_or_init(|| {
        let raw = std::env::var("TROLLSHELL_STATS_LAYOUT").ok();
        match parse_stats_layout(raw.as_deref()) {
            Ok(layout) => layout,
            Err(other) => {
                tracing::warn!(
                    value = %other,
                    "TROLLSHELL_STATS_LAYOUT unrecognized (expected combined/multicolumn/split); \
                     using multicolumn",
                );
                StatsLayout::Multicolumn
            }
        }
    })
}

thread_local! {
    /// The card a resource chip wants the combined Stats page to land on next,
    /// keyed per monitor (#516; per-monitor since #542). Set by
    /// [`set_scroll_target`] synchronously just before the chip calls
    /// `crate::modal::toggle`/`toggle_keep_open`; read (not consumed — a stale
    /// value is harmless, and re-reading lets both the window-map path and the
    /// already-open re-show path apply the same target) by the `"stats.scroll"`
    /// action each built page installs on itself in [`panel_stats`], with the
    /// target monitor's key handed in as the action's parameter. Keying per
    /// monitor keeps one monitor's chip choice from leaking onto another
    /// monitor's page, and lets a keybind/notification open on a monitor whose
    /// chip never ran land at the top rather than inherit a neighbour's section.
    static PENDING_SCROLL: RefCell<HashMap<String, StatsSection>> =
        RefCell::new(HashMap::new());
}

/// Stash which card the next Stats-page open/re-show on `monitor` should scroll
/// to (#516). Called by a resource chip's click handler immediately before it
/// opens or re-shows the combined Stats page; consumed by that monitor's page's
/// own `"stats.scroll"` action (wired in [`panel_stats`]) the next time
/// `crate::modal` triggers it.
pub fn set_scroll_target(monitor: &Monitor, section: StatsSection) {
    PENDING_SCROLL.with(|m| {
        m.borrow_mut().insert(monitor_key(monitor), section);
    });
}

/// The combined stats flyout — opened from any of the CPU / memory / disk /
/// GPU / services bar chips when `TROLLSHELL_STATS_LAYOUT=combined`. All five
/// cards stack in one `ts-popup-column`, one click, scroll to see all
/// (pre-#307 shape).
///
/// Wrapped in a `gtk::ScrolledWindow` (mirroring `connections.rs`/`wifi.rs`'s
/// #84 pattern) so five stacked cards can't push the drawer past screen
/// height. A `"stats"`-prefixed `gio::SimpleActionGroup` carrying a `"scroll"`
/// and a `"max-height"` action is inserted on the returned page widget by
/// [`install_stats_actions`]: `"scroll"` re-applies the [`StatsSection`]
/// pending in [`PENDING_SCROLL`] for the monitor named by its string parameter
/// (#542), and `"max-height"` sets the viewport cap from the live per-monitor
/// budget `crate::modal` measures (#701). Routing both through widget-local
/// actions (rather than a cross-module registry keyed by monitor) is what lets
/// `crate::modal` poke *this specific monitor's* built page instance armed with
/// only the `gtk::Widget` it already gets back from
/// `gtk::Stack::child_by_name` — this page never needs to know which monitor
/// it's on, only what it was handed at activation.
pub fn panel_stats() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let cpu_card = build_stats_cpu_card();
    let memory_card = build_stats_memory_card();
    let gpu_card = build_stats_gpu_card();
    let disks_card = build_stats_disks_card();
    let services_card = build_stats_services_card();

    column.append(cpu_card.upcast_ref::<gtk::Widget>());
    column.append(memory_card.upcast_ref::<gtk::Widget>());
    column.append(gpu_card.upcast_ref::<gtk::Widget>());
    column.append(disks_card.upcast_ref::<gtk::Widget>());
    column.append(services_card.upcast_ref::<gtk::Widget>());

    // No cap here (#701): the viewport ceiling is whatever room this monitor
    // actually has, pushed in by `crate::modal` on every show via the
    // `"max-height"` action. The old 560 was a "soft default (flagged for
    // review)" in the same spirit as the disk-I/O row's own soft-defaults
    // caveat further down this file — and it was flagged for exactly the right
    // reason: five stacked cards clear 560 comfortably, so the drawer scrolled
    // permanently while the bottom half of the screen sat empty.
    let scrolled = stats_scrolled(&column);

    let sections: Vec<(StatsSection, gtk::Widget)> = vec![
        (StatsSection::Cpu, cpu_card.upcast()),
        (StatsSection::Memory, memory_card.upcast()),
        (StatsSection::Gpu, gpu_card.upcast()),
        (StatsSection::Disks, disks_card.upcast()),
        (StatsSection::Services, services_card.upcast()),
    ];

    let page = finish_page(&scrolled);
    install_stats_actions(
        &page,
        &scrolled,
        column.upcast_ref::<gtk::Widget>(),
        sections,
    );
    page
}

/// The multicolumn stats flyout (#508) — the same five cards as [`panel_stats`]
/// laid out in a 2-column [`page_grid`]: CPU | Memory, GPU | Disks, then
/// Services spanning the last row. See [`attach_stats_cards`] for why that is
/// the arrangement.
/// Opened from the resource chips when `TROLLSHELL_STATS_LAYOUT` is
/// `multicolumn` (default) or unset; the chips still target the single
/// `Page::Stats` (this replaces [`panel_stats`] for that page in multicolumn
/// mode — see `crate::modal::build_page`).
///
/// Uses a wider Stats-specific clamp ([`DRAWER_MAX_WIDTH_WIDE`]) via
/// [`finish_page_clamped`] so two side-by-side history graphs each keep a usable
/// width instead of squeezing to ~330px inside the global `DRAWER_MAX_WIDTH`
/// (680) cap — the answer to #508's "the panel got *smaller*" complaint.
///
/// The #516 scroll-to-section deep-links survive: [`install_stats_actions`]
/// takes the `gtk::Grid` as its `compute_bounds` coordinate parent (the
/// mechanism is container-agnostic), so a chip click still lands its card at the
/// top of the shared `ScrolledWindow`.
pub fn panel_stats_multicolumn() -> gtk::Widget {
    let grid = page_grid();

    let cpu_card = build_stats_cpu_card();
    let memory_card = build_stats_memory_card();
    let gpu_card = build_stats_gpu_card();
    let disks_card = build_stats_disks_card();
    let services_card = build_stats_services_card();

    attach_stats_cards(
        &grid,
        cpu_card.upcast_ref::<gtk::Widget>(),
        memory_card.upcast_ref::<gtk::Widget>(),
        gpu_card.upcast_ref::<gtk::Widget>(),
        disks_card.upcast_ref::<gtk::Widget>(),
        services_card.upcast_ref::<gtk::Widget>(),
    );
    install_gpu_hidden_reflow(
        &grid,
        gpu_card.upcast_ref::<gtk::Widget>(),
        disks_card.upcast_ref::<gtk::Widget>(),
    );

    // Same live per-monitor cap as the combined page (#701) — see
    // [`panel_stats`]. Two columns roughly halve the stacked height, so on any
    // normal output this layout should now never scroll at all; the cap stays
    // as the safety net it was always meant to be, for a very tall failed-units
    // list or a genuinely small panel. The old 560 claimed to be "usually
    // slack", which was simply false: the CPU card alone eats most of that
    // budget, and this grid stacks three rows.
    let scrolled = stats_scrolled(&grid);

    let sections: Vec<(StatsSection, gtk::Widget)> = vec![
        (StatsSection::Cpu, cpu_card.upcast()),
        (StatsSection::Memory, memory_card.upcast()),
        (StatsSection::Gpu, gpu_card.upcast()),
        (StatsSection::Disks, disks_card.upcast()),
        (StatsSection::Services, services_card.upcast()),
    ];

    let page = finish_page_clamped(&scrolled, DRAWER_MAX_WIDTH_WIDE);
    install_stats_actions(&page, &scrolled, grid.upcast_ref::<gtk::Widget>(), sections);
    page
}

/// Attach the five Stats cards to the multicolumn grid, in the plain
/// two-column flow: `CPU | Memory`, `GPU | Disks`, then Services alone across
/// the last row.
///
/// ## Why the CPU span is gone again (#857)
///
/// #702 gave CPU a two-column span on one premise, recorded in its own comment:
/// CPU was "the one card whose content is a *strip* rather than a stack — the
/// per-core bars — so it's the one that actually wants the page width". That
/// premise died with #861, which replaced the 64-progressbar strip with a
/// single compact LED panel. A card whose widest content is now a ~180 px
/// surface does not want 1080 px, and annikahannig saw the result on glass:
/// "default looks weird now. Too much free space. I guess this can now be
/// regular two column flexbox".
///
/// Un-spanning restores **#582's bar-chip reading order** — CPU, Memory, GPU,
/// Disks, Services, left-to-right then down — which is the order the bar's
/// always-visible resource chips render in (`main.rs`'s resource `group`) and
/// which `stats_section_declaration_order_matches_bar` pins on the enum. #702
/// had traded that order away knowingly ("a deliberate trade annikahannig
/// picked on #702 (option A)"); with the span's reason gone, the trade has
/// nothing left to buy, so the order comes back rather than staying as drift.
/// These are the exact coordinates #582 landed, restored verbatim.
///
/// Services keeps the last row to itself for the reason #582 gave it: the
/// failed-units list can grow tall and has no natural column partner. The grid
/// stays **three** rows either way, so the drawer gets no taller than the
/// spanning layout it replaces.
///
/// Split out from [`panel_stats_multicolumn`] so `stats_grid_tests` can assert
/// the **real** coordinates rather than a copy of them — the shipping builder
/// binds every card to a `sensors::*` signal and so needs a registered
/// `Registry` to run, while five bare widgets need nothing.
fn attach_stats_cards(
    grid: &gtk::Grid,
    cpu: &gtk::Widget,
    memory: &gtk::Widget,
    gpu: &gtk::Widget,
    disks: &gtk::Widget,
    services: &gtk::Widget,
) {
    grid.attach(cpu, 0, 0, 1, 1);
    grid.attach(memory, 1, 0, 1, 1);
    grid.attach(gpu, 0, 1, 1, 1);
    grid.attach(disks, 1, 1, 1, 1);
    grid.attach(services, 0, 2, 2, 1);
}

/// Keep Disks out of column 1 while the GPU card is hidden (#571).
///
/// GPU is the only one of the five cards that self-hides *as a card* — its own
/// bind to `sensors::gpu()`'s presence signal, in [`build_stats_gpu_card`].
/// (Services never hides; only its "Flapping shell tasks" subgroup does, which
/// shrinks the card rather than vacating a cell.) Left at the fixed attach,
/// that self-hide would empty row 1's column 0 and strand Disks in column 1 —
/// and the gap would not collapse, because the grid is column-homogeneous and
/// rows 0 and 2 still occupy column 0. It would simply sit there as a hole.
///
/// **The reflow survives the #857 rearrangement unchanged**, for the reason
/// #702 already recorded: it only ever touches the *column*, never `set_row`.
/// All it requires is that GPU and Disks share a row in columns 0 and 1 —
/// whichever row that happens to be — and they still do (row 1, as under both
/// #582 and #702). What it needs from the *rest* of the grid is that column 0
/// stay occupied in some other row so the column keeps its width, and both
/// rows 0 (CPU) and 2 (Services' span) do that.
///
/// Tracks the GPU card's own `visible` property directly rather than
/// re-deriving the same condition from `sensors::gpu()`, so this can never
/// drift from whatever actually controls the card's presence; applied once
/// immediately for the first render, then kept live via `notify::visible`.
/// Moves the `GtkGridLayoutChild` rather than re-attaching.
fn install_gpu_hidden_reflow(grid: &gtk::Grid, gpu: &gtk::Widget, disks: &gtk::Widget) {
    let disks_layout_child = grid
        .layout_manager()
        .expect("gtk::Grid always installs a GtkGridLayout")
        .layout_child(disks)
        .downcast::<gtk::GridLayoutChild>()
        .expect("a GtkGrid's layout children are GtkGridLayoutChild");
    let reflow_disks_column = move |gpu_visible: bool| {
        disks_layout_child.set_column(i32::from(gpu_visible));
    };
    reflow_disks_column(gpu.is_visible());
    gpu.connect_visible_notify(move |gpu| reflow_disks_column(gpu.is_visible()));
}

/// The vertically-scrolling wrapper shared by the combined and multicolumn
/// Stats pages (#508). `propagate_natural_height` lets a short stack report its
/// own size so the drawer isn't padded out to the cap when it doesn't need to
/// be.
///
/// Deliberately built **uncapped** (#701). The viewport ceiling is not a
/// property of the page — it's a property of the monitor the drawer happens to
/// be on — so it arrives later, through the `"max-height"` action
/// [`install_stats_actions`] installs, which `crate::modal` activates from
/// `on_page_show` with that monitor's live budget. Every route that makes this
/// page visible runs `on_page_show` synchronously before the surface is
/// presented, so the cap is in place by the first frame; until then this
/// behaves like the uncapped `single_card_page`, i.e. content-sized inside a
/// fullscreen surface.
fn stats_scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_child(Some(child));
    scrolled
}

/// Install the `"stats"`-prefixed `gio::SimpleActionGroup` on `page`, shared by
/// the combined and multicolumn Stats pages (#508/#516/#542/#701). Two actions,
/// both driven by `crate::modal` from `on_page_show` because both depend on
/// *which monitor's* drawer is showing this monitor-agnostically built page:
///
/// - `"scroll"` (string param) — re-apply the [`StatsSection`] pending in
///   [`PENDING_SCROLL`] for the monitor named by the parameter (#542).
///   `coord_parent` is the widget the cards' `compute_bounds` is taken
///   relative to: the combined page's column `gtk::Box`, or the multicolumn
///   `gtk::Grid`. The deep-link scroll is container-agnostic, so either works.
/// - `"max-height"` (int32 param) — set the `ScrolledWindow` viewport cap to
///   the vertical room that monitor actually has (#701).
///
/// The `"max-height"` parameter is applied **verbatim**, with no
/// `crate::scale::scale` call. It arrives as live logical pixels measured off
/// `gdk::Monitor::geometry` (see `modal::BarGeometry::available_card_height`),
/// not as a design-baseline constant, and the page content it caps already
/// rides the font factor — scaling here would double-count it, which is the
/// mistake that made the old hardcoded 560 unfixable by tuning.
fn install_stats_actions(
    page: &gtk::Widget,
    scrolled: &gtk::ScrolledWindow,
    coord_parent: &gtk::Widget,
    sections: Vec<(StatsSection, gtk::Widget)>,
) {
    let scrolled_for_action = scrolled.clone();
    let parent_for_action = coord_parent.clone();
    let action_group = gio::SimpleActionGroup::new();

    let scroll_action = gio::SimpleAction::new("scroll", Some(glib::VariantTy::STRING));
    scroll_action.connect_activate(move |_, param| {
        let Some(key) = param.and_then(glib::Variant::str) else {
            return;
        };
        apply_scroll(&scrolled_for_action, &parent_for_action, &sections, key);
    });
    action_group.add_action(&scroll_action);

    let scrolled_for_height = scrolled.clone();
    let height_action = gio::SimpleAction::new("max-height", Some(glib::VariantTy::INT32));
    height_action.connect_activate(move |_, param| {
        let Some(height) = param.and_then(glib::Variant::get::<i32>) else {
            return;
        };
        scrolled_for_height.set_max_content_height(height);
    });
    action_group.add_action(&height_action);

    page.insert_action_group("stats", Some(&action_group));
}

/// Wrap a single stats card into a drawer page — #307's split shape, restored
/// alongside the combined/multicolumn pages in #508 and selected by
/// `TROLLSHELL_STATS_LAYOUT=split`. Each per-resource panel is one card in a
/// `ts-popup-column`, so the icon-per-resource flyouts stay visually identical
/// to the cards they share with the other two layouts. No `ScrolledWindow` or
/// scroll action: a single card is short and each chip opens its own page.
fn single_card_page(card: &gtk::Widget) -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);
    column.append(card);
    finish_page(&column)
}

/// CPU stats flyout — opened from the CPU bar chip in `split` layout.
pub fn panel_stats_cpu() -> gtk::Widget {
    single_card_page(build_stats_cpu_card().upcast_ref::<gtk::Widget>())
}

/// Memory stats flyout — opened from the memory bar chip in `split` layout.
pub fn panel_stats_memory() -> gtk::Widget {
    single_card_page(build_stats_memory_card().upcast_ref::<gtk::Widget>())
}

/// GPU stats flyout — opened from the GPU bar chip in `split` layout.
pub fn panel_stats_gpu() -> gtk::Widget {
    single_card_page(build_stats_gpu_card().upcast_ref::<gtk::Widget>())
}

/// Disks stats flyout — opened from the disk bar chip in `split` layout.
pub fn panel_stats_disks() -> gtk::Widget {
    single_card_page(build_stats_disks_card().upcast_ref::<gtk::Widget>())
}

/// Services flyout — opened from the services bar chip in `split` layout.
pub fn panel_stats_services() -> gtk::Widget {
    single_card_page(build_stats_services_card().upcast_ref::<gtk::Widget>())
}

/// Resolve the [`StatsSection`] pending for `key`'s monitor (if any) against
/// `sections` and scroll `scrolled` so that card's top edge lands at the top of
/// the viewport. Deferred one main-loop idle tick past this call so a
/// just-mapped or just-swapped-to page's layout is settled before
/// `compute_bounds` reads it — the same "allocation isn't ready before
/// map/tick" guarantee `modal.rs`'s own post-map recenter math relies on
/// (#212-family lore). No-ops quietly if nothing is pending for that monitor,
/// the target isn't one of this page's cards, or the page isn't laid out yet
/// (e.g. the drawer was closed again before the idle tick ran).
fn apply_scroll(
    scrolled: &gtk::ScrolledWindow,
    coord_parent: &gtk::Widget,
    sections: &[(StatsSection, gtk::Widget)],
    key: &str,
) {
    let Some(target) = PENDING_SCROLL.with(|m| m.borrow().get(key).copied()) else {
        return;
    };
    let Some((_, card)) = sections.iter().find(|(section, _)| *section == target) else {
        return;
    };

    // One bounded retry (#542): on a first-ever build-on-swap the page's
    // allocation can still be unsettled on the first idle tick, so
    // `compute_bounds` returns `None` and the scroll silently no-ops. Re-arm one
    // more idle in that case — capped at a single retry so a genuinely off-page
    // target can't spin the main loop forever.
    scroll_card_to_top_when_ready(scrolled.clone(), coord_parent.clone(), card.clone(), 1);
}

/// Deferred scroll worker for [`apply_scroll`]: on the next main-loop idle, land
/// `card`'s top edge at the top of `scrolled`'s viewport. If `card`'s bounds
/// don't resolve yet (`compute_bounds` → `None`, an unsettled allocation),
/// re-arm up to `retries` more idles before giving up (#542).
fn scroll_card_to_top_when_ready(
    scrolled: gtk::ScrolledWindow,
    coord_parent: gtk::Widget,
    card: gtk::Widget,
    retries: u32,
) {
    glib::idle_add_local_once(move || {
        let Some(bounds) = card.compute_bounds(&coord_parent) else {
            if retries > 0 {
                scroll_card_to_top_when_ready(scrolled, coord_parent, card, retries - 1);
            }
            return;
        };
        let vadj = scrolled.vadjustment();
        let lower = vadj.lower();
        let upper = (vadj.upper() - vadj.page_size()).max(lower);
        vadj.set_value(f64::from(bounds.y()).clamp(lower, upper));
    });
}

/// Wrap a bare full-width widget — a history-sparkline `gtk::Box`, or the
/// per-core LED panel's `gtk::Box` — in a `gtk::ListBoxRow` so it joins an
/// `AdwPreferencesGroup`'s boxed-list in source order with the standard
/// separators. A non-`GtkListBoxRow` child added to a group otherwise renders
/// *below* the boxed-list and out of order (cf. the adw-routing gotcha; the
/// same fix PR #149 used for the per-interface network rows).
fn history_row_wrapper(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_hexpand(true);
    row.set_child(Some(child));
    row
}

/// CPU card — live CPU + per-core + processes, CPU history sparkline, and the
/// CPU top-apps expander. Processes (a system-load metric) lives here; this is
/// the one placement Mara didn't pin (flagged in the PR for relocation).
fn build_stats_cpu_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    group.add(&build_live_cpu_row());
    group.add(&build_live_per_core_row());
    group.add(&history_row_wrapper(&build_per_core_leds_row()));
    group.add(&build_live_processes_row());
    group.add(&build_expandable_cpu_history_row());
    group.add(&build_expandable_cpu_clock_row());
    group.add(&build_top_apps_expander(
        "Top apps \u{00b7} CPU",
        app_usage::top_by_cpu(),
        |s| format!("{:.0}%", s.cpu_frac * 100.0),
    ));

    group
}

/// Memory card — live memory + swap, memory history sparkline, and the RAM
/// top-apps expander. The swap row self-hides when no swap is configured.
fn build_stats_memory_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    group.add(&build_live_memory_row());
    group.add(&build_live_swap_row());
    group.add(&history_row_wrapper(&build_history_memory_row()));
    group.add(&build_top_apps_expander(
        "Top apps \u{00b7} RAM",
        app_usage::top_by_mem(),
        |s| fmt_bytes(s.mem_bytes),
    ));

    group
}

/// Disks card — the per-mount capacity expander plus a live disk-I/O
/// throughput history row (aggregate read+write rate across physical disks).
fn build_stats_disks_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    group.add(&build_live_disk_expander());
    group.add(&history_row_wrapper(&build_history_disk_io_row()));
    group
}

/// GPU card — live GPU row + usage / VRAM / temp history sparklines. The whole
/// card hides when no GPU is detected (bound to the same `sensors::gpu()`
/// presence signal the live GPU row uses to self-hide); each history row
/// additionally self-hides if its specific metric (load / VRAM / temperature)
/// isn't reported. Intel GPUs are supported as of #150, so this card shows on
/// Arc/iGPU hardware.
fn build_stats_gpu_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    // Hide the entire card when no GPU is present.
    bind(
        sensors::gpu().map(|g| g.is_some()),
        &group,
        gtk::prelude::WidgetExt::set_visible,
    );

    group.add(&build_live_gpu_row());
    group.add(&history_row_wrapper(&build_history_gpu_usage_row()));
    group.add(&history_row_wrapper(&build_history_gpu_vram_row()));
    group.add(&history_row_wrapper(&build_history_gpu_temp_row()));

    group
}

/// Cached desktop app metadata resolved from `gio::AppInfo`.
///
/// Note: `gio::DesktopAppInfo` is not available in gio 0.22 bindings, so we
/// use the `gio::AppInfo` interface (the abstract interface) via
/// `gio::AppInfo::all()`, which returns all installed applications with their
/// ids, display names, and icons. We scan this list lazily (once per new
/// app-id) and cache the result for the lifetime of the expander widget.
#[derive(Clone)]
struct AppMeta {
    display_name: String,
    icon: Option<gio::Icon>,
}

/// Resolve display name and icon for an app-id via a layered `gio::AppInfo`
/// lookup.
///
/// Tries the following strategies in order, stopping at the first hit:
///
/// 1. **Exact id match** — `AppInfo::all()` entry whose id equals
///    `<app_id>.desktop` (or its lowercase variant). Fast for well-behaved
///    desktop files.
/// 2. **Case-insensitive id containment** — scans for an entry whose desktop
///    file id (without the `.desktop` suffix) case-insensitively contains, or
///    is contained by, the app-id. Catches reverse-DNS mismatches such as
///    `org.gnome.Nautilus.desktop` for app-id `org.gnome.Nautilus`, and
///    NixOS wrapper names like `firefox-unwrapped` matching `firefox.desktop`.
/// 3. **Executable basename match** — entry whose executable file stem
///    case-insensitively equals the app-id. Catches cgroup scope leaves like
///    `app-firefox.scope`→`firefox` when the desktop file is `Firefox.desktop`
///    with executable `/usr/bin/firefox`.
///
/// All three layers scan `AppInfo::all()` (or share the same pre-fetched list)
/// and cache the result so the work happens at most once per unique app-id per
/// expander lifetime.
///
/// Note: `gio::DesktopAppInfo::search` and `startup_wm_class` are not
/// available in the gio 0.22 bindings used here, so the above heuristics
/// approximate their behaviour using the `AppInfo` abstract interface.
fn resolve_app_meta(
    app_id: &str,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) -> Option<AppMeta> {
    if let Some(cached) = meta_cache.get(app_id) {
        return cached.clone();
    }

    let app_id_lower = app_id.to_lowercase();

    // Fetch all installed apps once for this lookup.
    let all = gio::AppInfo::all();

    // Layer 1: exact id match (fast path).
    let exact = format!("{app_id}.desktop");
    let exact_lower = format!("{app_id_lower}.desktop");
    let hit = all.iter().find(|info| {
        info.id()
            .is_some_and(|id| id == exact.as_str() || id == exact_lower.as_str())
    });

    // Layer 2: case-insensitive id containment.
    // Strips the `.desktop` suffix and checks if the stem contains the app-id
    // or vice-versa (handles reverse-DNS and wrapper-name mismatches).
    let hit = hit.or_else(|| {
        all.iter().find(|info| {
            info.id().is_some_and(|id| {
                let stem = id
                    .as_str()
                    .strip_suffix(".desktop")
                    .unwrap_or(id.as_str())
                    .to_lowercase();
                stem.contains(app_id_lower.as_str())
                    || app_id_lower.as_str().contains(stem.as_str())
            })
        })
    });

    // Layer 3: executable basename match.
    // Catches cases where the desktop file uses a different name but the
    // binary matches (e.g. `firefox` binary → `Firefox.desktop`).
    let hit = hit.or_else(|| {
        all.iter().find(|info| {
            let exe = info.executable();
            exe.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.to_lowercase() == app_id_lower.as_str())
        })
    });

    let meta = hit.map(|info| AppMeta {
        display_name: info.display_name().to_string(),
        icon: info.icon(),
    });
    meta_cache.insert(app_id.to_string(), meta.clone());
    meta
}

/// A collapsible "Top apps" list (CPU or RAM) bound to an [`app_usage`] signal.
/// `value` formats each row's right-hand value. Mirrors
/// [`build_live_disk_expander`]'s drain-and-rebuild pattern.
///
/// Each row gets a leading icon resolved from the app-id via `gio::AppInfo`.
/// Icons and display names are cached per app-id (one `AppInfo::all()` scan per
/// unique app-id per expander lifetime). The name field is rendered with markup
/// off so an adversarial scope id can't inject Pango markup (cf. #30).
///
/// The "System" bucket (all non-app-scope PIDs) gets a `computer-symbolic` icon.
fn build_top_apps_expander(
    title: &str,
    signal: impl Signal<Item = Vec<ProcSample>> + 'static,
    value: fn(&ProcSample) -> String,
) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title(title).build();
    // The child rows below have had markup off since #30, but the expander's
    // *own* collapsed subtitle is built from the same untrusted display name
    // and was left unguarded (#753).
    markup::plain_text(&expander);
    expander.set_expanded(false);

    // Metadata cache: app-id → AppMeta (None = no desktop file found).
    // Lives for the lifetime of this expander's bind closure.
    let meta_cache: Rc<RefCell<HashMap<String, Option<AppMeta>>>> =
        Rc::new(RefCell::new(HashMap::new()));

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    // One-shot guard: collapse the expander the first time rows actually arrive.
    // We must not call set_expanded(false) on every tick — that would fight the
    // user's manual expand/collapse.  The build-time set_expanded(false) above
    // covers the empty-expander case; this guard fires once on first population.
    let collapsed_once: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let rows_for_bind = rows_track.clone();
    let collapsed_once_for_bind = collapsed_once.clone();
    bind(signal, &expander, move |expander, list| {
        rebuild_top_apps(
            expander,
            &rows_for_bind,
            &meta_cache,
            &collapsed_once_for_bind,
            value,
            &list,
        );
    });

    expander
}

/// One rebuild pass of a "Top apps" expander: tear down whatever the previous
/// pass left in `rows_track`, refresh the collapsed summary, and rebuild one
/// [`adw::ActionRow`] per entry of `list`.
///
/// A free function taking its two cells, the one-shot `collapsed_once` guard
/// and the `value` formatter explicitly, mirroring `panels/connections.rs`'s
/// `bind_connections_group`/`rebuild_connections` pair (#828) — the body used
/// to sit inline inside [`build_top_apps_expander`]'s apply closure, where
/// nothing could call it and nothing could reach the cells to observe them.
/// Extracting it changes no behaviour (the closure is now a single forwarding
/// call) and is what lets the colocated `#[gtk::test]` at the bottom of this
/// file drive the cells directly: re-entering through the bound `Mutable`
/// instead would be *deferred* to the next main-context iteration, because
/// `bind` polls its signal from a `glib::MainContext` task, so it could never
/// reproduce the synchronous re-entry this borrow discipline exists for
/// (#674).
///
/// `expander` is fed the `&adw::ExpanderRow` that `bind` hands its apply
/// closure, **not** a strong clone captured from the enclosing scope. The
/// captured-clone form pinned the expander for the life of the binding and so
/// defeated `bind`'s `WeakRef` contract (`hytte-reactive/src/bind.rs:16-22`);
/// #830 recorded it here as a separate bug from #674 (it had to stay put for
/// that PR's byte-identical-extract argument to hold), and #831 fixed it here
/// along with the other eleven sites of the same shape. The colocated
/// `top_apps_binding_does_not_pin_expander` test is the regression guard.
fn rebuild_top_apps(
    expander: &adw::ExpanderRow,
    rows_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
    meta_cache: &Rc<RefCell<HashMap<String, Option<AppMeta>>>>,
    collapsed_once: &Rc<Cell<bool>>,
    value: fn(&ProcSample) -> String,
    list: &[ProcSample],
) {
    // `take()` ends the borrow before the first `remove()`; a chained
    // `borrow_mut().drain(..)` would hold it for the whole loop, and a
    // re-entrant borrow from a synchronous emission panics fatally through
    // the glib callback (#643).
    for row in rows_track.take() {
        expander.remove(&row);
    }
    // Collapsed summary: the heaviest entry's display name, or an em-dash.
    let subtitle = list.first().map_or_else(
        || "\u{2014}".to_string(),
        |s| {
            // Resolve first, format second. An argument-position `RefMut` is a
            // temporary of the whole *enclosing expression*, not of the
            // sub-expression that made it, so inlining this back into the
            // `format!` would leave `meta_cache` borrowed while the
            // caller-supplied `value(s)` runs — #643's spelling 4, at the one
            // site in this function #663 did not rewrite (#832). Latent rather
            // than live (`value` is a capture-free `fn` pointer and both
            // production instantiations are pure formatters), but a `value`
            // that ever reached back into this cell would abort the process
            // through the glib callback rather than fail a render.
            let name = sample_display_name(s, &mut meta_cache.borrow_mut());
            format!("{name} \u{00b7} {}", value(s))
        },
    );
    expander.set_subtitle(&subtitle);

    let mut new_rows = Vec::with_capacity(list.len());
    for s in list {
        let row = adw::ActionRow::builder().activatable(false).build();
        // Markup off: scope ids are untrusted — adversarial names could
        // otherwise inject Pango markup into the title (cf. #30).
        markup::plain_text(&row);
        // Resolve first, set second: an argument-position `RefMut` is a
        // temporary of the whole statement, so inlining this would hold
        // `meta_cache` borrowed across `set_title` (#643).
        let title = sample_display_name(s, &mut meta_cache.borrow_mut());
        row.set_title(&title);
        if s.procs > 1 {
            row.set_subtitle(&format!("{} processes", s.procs));
        }

        // Prefix icon: cached from the app-id, or sensible fallbacks.
        let icon: gio::Icon = if let Some(app_id) = s.app_id.as_deref() {
            // Same statement-temporary rule: bind the lookup so the
            // `RefMut` is gone before the fallback icon is constructed.
            let cached =
                resolve_app_meta(app_id, &mut meta_cache.borrow_mut()).and_then(|m| m.icon);
            cached.unwrap_or_else(|| {
                gio::ThemedIcon::new("application-x-executable-symbolic").upcast::<gio::Icon>()
            })
        } else {
            // System bucket: generic computer icon.
            gio::ThemedIcon::new("computer-symbolic").upcast::<gio::Icon>()
        };
        let img = gtk::Image::from_gicon(&icon);
        img.set_icon_size(gtk::IconSize::Normal);
        img.set_valign(gtk::Align::Center);
        row.add_prefix(&img);

        let label = gtk::Label::new(Some(&value(s)));
        label.set_valign(gtk::Align::Center);
        row.add_suffix(&label);
        expander.add_row(&row);
        new_rows.push(row);
    }
    *rows_track.borrow_mut() = new_rows;

    // Re-assert collapsed state once on first non-empty population.
    // libadwaita can render the expander open when rows arrive after the
    // initial set_expanded(false) on an empty widget (async row-population
    // race — #131).  We fire exactly once so the user's subsequent
    // manual expand/collapse is never overridden.
    if !collapsed_once.get() && !list.is_empty() {
        expander.set_expanded(false);
        collapsed_once.set(true);
    }
}

/// Human-readable display name for a sample. If an app-id is set, looks up
/// the display name via the `AppMeta` cache (falling back to the raw app-id
/// on a cache miss). Returns `s.name` for the "System" bucket.
fn sample_display_name(
    s: &ProcSample,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) -> String {
    s.app_id.as_deref().map_or_else(
        || s.name.clone(),
        |id| resolve_app_meta(id, meta_cache).map_or_else(|| id.to_string(), |m| m.display_name),
    )
}

fn build_live_cpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("CPU").build();
    bind(
        sensors::cpu().map(|c| format!("{:.0}%", c.overall * 100.0)),
        &row,
        |r, t| r.set_subtitle(&t),
    );
    let temp_label = gtk::Label::new(None);
    temp_label.set_valign(gtk::Align::Center);
    bind(
        sensors::cpu_temp().map(|t| match t.package_celsius {
            Some(c) => format!("{c:.0} \u{00b0}C"),
            None => String::new(),
        }),
        &temp_label,
        move |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&temp_label);
    row
}

/// Per-core header row: title and live core count only. The LED panel is a
/// *separate* full-width row ([`build_per_core_leds_row`]) rather than this
/// row's suffix, because a suffix cannot be shrunk below its minimum — the
/// original sin behind #702, and the reason the panel stays out of a suffix
/// even though its own minimum is now 0 px.
fn build_live_per_core_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Per-core").build();
    row.set_activatable(false);
    row.set_selectable(false);
    bind(
        sensors::cpu().map(|c| format!("{} cores", c.per_core.len())),
        &row,
        |r, t| r.set_subtitle(&t),
    );
    row
}

// ── The per-core LED panel's look (#857) ─────────────────────────────────────

/// How the per-core LED panel is dressed this session (#857).
///
/// Four session env vars rather than four hard-coded constants, for the same
/// reason `TROLLSHELL_STATS_LAYOUT` is one (#508): this is a pure look-and-feel
/// choice with no right answer, CI cannot judge it, and the shell has no config
/// DSL to put it in. Annika's issue asked for a shape, a fill and a colour
/// option; an env var each lets all of `vfd|lcd|oled|crt` × `style|heat|
/// rainbow|transpride|#rrggbb` be tried on real glass by restarting the shell
/// instead of rebuilding it.
///
/// The skin and the colour map are **independent axes** (see the `hytte-preem`
/// `color_map` docs): `…_STYLE=crt` with `…_COLOR=heat` gives heat-mapped lamps
/// *through* the tube's scanlines, not one instead of the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CoreLeds {
    /// The kit skin — the panel's physical character.
    style: DisplayStyle,
    /// The colour axis — what colour each lamp lights in.
    color: ColorMap,
    /// A pinned row count, or `None` for the automatic `rect` shape.
    rows: Option<usize>,
    /// What a ragged last row's leftover slots look like.
    fill: Fill,
}

impl Default for CoreLeds {
    fn default() -> Self {
        Self {
            // VFD: near-black field with a phosphor halo off every lit lamp —
            // the "blinken lichten" look, and the skin whose glow reads best
            // against the drawer's dark card.
            style: DisplayStyle::Vfd,
            // Heat: the panel's whole job is "which core is busy", and a
            // level-driven ramp answers that at a glance in a way a single ink
            // cannot. `style` is one env var away for anyone who wants the
            // accent-tinted single ink back.
            color: ColorMap::Heat,
            rows: None,
            fill: Fill::Spare,
        }
    }
}

/// Parse `TROLLSHELL_CORE_LEDS_STYLE`. `None`/unset is the default skin;
/// an unrecognized value returns `Err(other)` so the caller can warn once.
fn parse_core_leds_style(raw: Option<&str>) -> Result<DisplayStyle, &str> {
    match raw {
        None => Ok(CoreLeds::default().style),
        Some(v) => DisplayStyle::ALL
            .into_iter()
            .find(|s| s.name() == v)
            .ok_or(v),
    }
}

/// Parse `TROLLSHELL_CORE_LEDS_COLOR`: one of the kit's named maps, or an
/// `#rrggbb` / `rrggbb` literal for Annika's `(r, g, b)` option.
fn parse_core_leds_color(raw: Option<&str>) -> Result<ColorMap, &str> {
    let Some(v) = raw else {
        return Ok(CoreLeds::default().color);
    };
    if let Some(map) = ColorMap::ALL.into_iter().find(|m| m.name() == v) {
        return Ok(map);
    }
    parse_hex_rgb(v).ok_or(v)
}

/// `#rrggbb` or bare `rrggbb` → an [`ColorMap::Rgb`]. Case-insensitive; any
/// other length or a non-hex digit is `None`.
fn parse_hex_rgb(raw: &str) -> Option<ColorMap> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok();
    Some(ColorMap::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

/// Parse `TROLLSHELL_CORE_LEDS_ROWS`: `rect` (or unset) for the automatic
/// shape, or a positive row count. `0` is rejected rather than silently
/// clamped — it is a typo, not an intent.
///
/// `rect` is Annika's word for it from #857, and since her second pass on the
/// same issue ("rectangle for led view would be still more preem tho") it now
/// keeps its promise: the automatic shape is [`LedMatrix::wide`]'s wide
/// rectangle rather than the near-square panel #861 shipped. Still automatic
/// rather than a pinned default row count, because a fixed `rows = 4` reads
/// well at 64 cores and absurdly at 4 — see [`LedMatrix::wide`].
fn parse_core_leds_rows(raw: Option<&str>) -> Result<Option<usize>, &str> {
    match raw {
        None | Some("rect") => Ok(None),
        Some(v) => match v.parse::<usize>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(v),
        },
    }
}

/// Parse `TROLLSHELL_CORE_LEDS_FILL`: `spare` (unset default) or `blank`.
fn parse_core_leds_fill(raw: Option<&str>) -> Result<Fill, &str> {
    match raw {
        None | Some("spare") => Ok(Fill::Spare),
        Some("blank") => Ok(Fill::Blank),
        Some(other) => Err(other),
    }
}

/// Resolve one env var through its parser, warning once on an unrecognized
/// value and falling back to `fallback`.
///
/// Split out so all four knobs warn in the same shape, and so the parsers
/// themselves stay pure (and testable) functions of a `&str`.
fn core_leds_var<T>(
    name: &str,
    parse: impl FnOnce(Option<&str>) -> Result<T, &str>,
    expected: &str,
    fallback: T,
) -> T {
    let raw = std::env::var(name).ok();
    match parse(raw.as_deref()) {
        Ok(v) => v,
        Err(other) => {
            tracing::warn!(
                var = %name,
                value = %other,
                expected = %expected,
                "per-core LED panel option unrecognized; using the default",
            );
            fallback
        }
    }
}

/// The per-core LED panel's dressing for this process, read once from the
/// environment and cached — the [`stats_layout`] pattern, for the same reason
/// (a session env var is process-constant, so re-reading it per redraw would
/// buy nothing and cost a `getenv` a second).
fn core_leds() -> CoreLeds {
    use std::sync::OnceLock;
    static LEDS: OnceLock<CoreLeds> = OnceLock::new();
    *LEDS.get_or_init(|| {
        let default = CoreLeds::default();
        CoreLeds {
            style: core_leds_var(
                "TROLLSHELL_CORE_LEDS_STYLE",
                parse_core_leds_style,
                "vfd/lcd/oled/crt",
                default.style,
            ),
            color: core_leds_var(
                "TROLLSHELL_CORE_LEDS_COLOR",
                parse_core_leds_color,
                "style/rainbow/transpride/heat/#rrggbb",
                default.color,
            ),
            rows: core_leds_var(
                "TROLLSHELL_CORE_LEDS_ROWS",
                parse_core_leds_rows,
                "rect or a positive row count",
                default.rows,
            ),
            fill: core_leds_var(
                "TROLLSHELL_CORE_LEDS_FILL",
                parse_core_leds_fill,
                "spare/blank",
                default.fill,
            ),
        }
    })
}

/// The largest on-screen box the per-core LED panel may occupy, in logical px
/// (#857).
///
/// The panel is rasterised at its own small buffer size and blown up by a whole
/// integer factor, so this is a *budget*, not a size: [`core_panel_scale`]
/// picks the biggest factor that still fits inside it. Both bounds earn their
/// keep — the height stops a small-core box from filling the CPU card with
/// lamps, and the width stops a deliberately wide shape (a `…_ROWS=3` strip on
/// a 64-core box is 247 px at 1×) from being scaled past the card and then
/// letterboxed back down, which would resample the fixed dot grid the kit
/// exists to keep whole (#839/#843).
const CORE_PANEL_MAX_W: usize = 280;
/// Height half of [`CORE_PANEL_MAX_W`]'s budget box.
const CORE_PANEL_MAX_H: usize = 104;

/// The integer upscale factor for a `buf_w`×`buf_h` LED-panel buffer: the
/// largest whole factor whose result still fits the
/// [`CORE_PANEL_MAX_W`]×[`CORE_PANEL_MAX_H`] budget, and never less than 1.
///
/// Whole factors only, because `PixelSurface` paints nearest-neighbour: a
/// fractional blow-up would duplicate some lamp rows and not others, and the
/// panel would shimmer as the core count changed. Across the shapes
/// [`core_led_matrix`] produces it keeps every machine's panel a comparable
/// on-glass size rather than letting a 4-core box render a postage stamp next
/// to a 64-core box's full panel — with the #857 rectangle default the widths
/// land in a 96–279 px band (1 core → 96, 4 → 245, 8 → 147, 16 → 279, 32 →
/// 252, 64 → 181, 128 → 247) and the heights in a 49–96 px one.
///
/// The heights are shorter than the near-square panel's used to be, and that is
/// the point of the change rather than a side effect: a rectangle is short, and
/// the CPU card was showing "too much free space" as a tall square in a card
/// that no longer needs the page width.
fn core_panel_scale(buf_w: usize, buf_h: usize) -> u32 {
    // An empty buffer has nothing to scale — and `MAX / 1` would otherwise
    // hand back a nonsense factor of 104 for a surface that renders nothing.
    if buf_w == 0 || buf_h == 0 {
        return 1;
    }
    let by_w = CORE_PANEL_MAX_W / buf_w;
    let by_h = CORE_PANEL_MAX_H / buf_h;
    u32::try_from(by_w.min(by_h).max(1)).unwrap_or(1)
}

/// Build the [`LedMatrix`] for `cores` lamps, in the session's configured skin,
/// colour map and fill ([`core_leds`]).
///
/// The shape is the wide-rectangle [`LedMatrix::wide`] unless
/// `TROLLSHELL_CORE_LEDS_ROWS` pinned a row count, in which case the columns
/// fall out of it — which is where [`Fill`] starts to matter, since 64 lamps on
/// 3 rows needs 22 columns and leaves the last row two slots short.
fn core_led_matrix(cores: usize) -> LedMatrix {
    core_led_matrix_for(core_leds(), cores)
}

/// [`core_led_matrix`] with the dressing passed in rather than read from the
/// environment.
///
/// Split out so the shape choice is testable at all: [`core_leds`] resolves
/// four env vars into a process-wide `OnceLock`, so a test that drove it would
/// depend on the ambient environment *and* on which test ran first. A `CoreLeds`
/// value has neither problem.
fn core_led_matrix_for(cfg: CoreLeds, cores: usize) -> LedMatrix {
    let base = match cfg.rows {
        Some(rows) => LedMatrix::new(cfg.style, cores.max(1).div_ceil(rows), rows),
        None => LedMatrix::wide(cfg.style, cores),
    };
    base.color(cfg.color).fill(cfg.fill)
}

/// The hover readout for the LED panel.
///
/// The `FlowBox` this replaced put a bare `"42%"` tooltip on each bar, which
/// could say *how* busy a bar was but never *which core* it was — you had to
/// count bars. One panel-wide summary that names the busiest core is strictly
/// more informative in aggregate, at the cost of the per-lamp readout; a
/// pointer-precise per-lamp tooltip would need the inverse of
/// `PixelSurface`'s letterbox transform and is left as a follow-up.
fn core_panel_tooltip(per_core: &[f64]) -> String {
    if per_core.is_empty() {
        return "No per-core data".to_string();
    }
    let sum: f64 = per_core.iter().sum();
    let cores = u64::try_from(per_core.len()).unwrap_or(u64::MAX);
    let avg = sum / cast::u64_to_f64(cores) * 100.0;
    // `partial_cmp` rather than `total_cmp` would have to handle NaN; the
    // fold below simply never adopts a NaN as the new maximum, so a garbage
    // sample can't win the "busiest" title.
    let (busiest, peak) =
        per_core
            .iter()
            .enumerate()
            .fold((0usize, f64::NEG_INFINITY), |(bi, bv), (i, &v)| {
                if v > bv { (i, v) } else { (bi, bv) }
            });
    format!(
        "{} cores \u{00b7} avg {avg:.0}% \u{00b7} max {:.0}% (core {busiest})",
        per_core.len(),
        peak.max(0.0) * 100.0,
    )
}

/// The row container the LED panel sits in.
///
/// The `gtk::Box` carries the row's padding, not the surface: `PixelSurface`
/// computes its letterbox against the widget allocation rather than the CSS
/// content box, so CSS padding set on the surface itself would be painted over.
///
/// Split out from [`build_per_core_leds_row`] so `led_panel_layout_tests` can
/// measure the **real** arrangement rather than a copy of it — that function
/// binds `sensors::cpu()` inline and needs a registered `Registry` to run.
fn core_panel_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("ts-cores-row");
    // The *box* centres, and the surface inside it does not expand. Both
    // halves matter, and getting either wrong inflates the CPU card:
    // `GtkBoxLayout`'s opposite-orientation measure hands an **expanding**
    // child all the space it is measuring for, and an aspect-locked
    // height-for-width surface handed 400 px of width answers "then I am 400 px
    // tall". Centring at the box level instead means the box asks its child for
    // a natural 93 px, gets a 93 px height back, and spends the slack on
    // centring — which is what `a_wide_card_cannot_inflate_the_row_height`
    // pins. (Measured: with `hexpand` on the surface, a 400 px card produced a
    // 400 px-tall row.)
    row.set_halign(gtk::Align::Center);
    row.set_hexpand(true);
    row
}

/// The raster surface the LED panel draws into, configured but not yet bound.
///
/// **Never expanding**, for the reason [`core_panel_row`] spells out: an
/// expanding `PixelSurface` is handed the card's whole width at measure time
/// and, being aspect-locked, answers with a matching height. It asks for its
/// natural buffer size and nothing more; [`core_panel_row`] does the centring.
///
/// See [`core_panel_row`] for why this is a separate constructor.
fn core_panel_surface() -> PixelSurface {
    let panel = PixelSurface::new();
    panel.set_halign(gtk::Align::Center);
    panel.set_valign(gtk::Align::Center);
    panel
}

/// The per-core strip: an LED panel — one lamp per core, each lit to that
/// core's load ("Blinken Lichten", #857).
///
/// # It cannot repeat #702
///
/// The `gtk::FlowBox` of 64 vertical `ProgressBar`s this replaced was the
/// second answer to #702, whose first answer — a single-line `gtk::Box` — had a
/// *minimum* width of `n·8 + (n−1)·4` = **764 px at 64 cores**, because every
/// bar carried a hard `min-width: 8px` CSS floor and 64 floors add up. No
/// container could negotiate that down, and through `page_grid`'s homogeneous
/// columns it doubled the whole drawer's minimum width.
///
/// The panel's minimum width at 64 cores is **0 px**, on any core count, for
/// two independent reasons:
///
/// 1. It is **one widget**, not `n`. There are no per-child floors to add up —
///    the lamps are pixels in a buffer, not widgets in a container.
/// 2. `PixelSurface::measure` returns `(0, natural, -1, -1)`: its minimum is
///    hard-coded to `0` on both axes so CSS/layout can shrink it freely, and
///    its *natural* size (181×49 px at 64 cores in the default rectangle,
///    times [`core_panel_scale`]) is only ever a request. The `ts-cores-row`
///    wrapper carries the row's padding and a `min-height`, deliberately never
///    a `min-width`.
///
/// Under-allocated, the surface letterboxes the buffer down rather than forcing
/// the container wider — so the failure mode is a smaller panel, not a wider
/// drawer.
///
/// # Redraw
///
/// Signal-driven, no animation timer: `sensors::cpu()` ticks once a second and
/// each tick rasterises one frame (~116 µs on VFD at 64 cores, measured
/// `--release`) and swaps the texture. Same-size swaps take
/// `PixelSurface`'s `queue_draw` path rather than `queue_resize`.
fn build_per_core_leds_row() -> gtk::Box {
    let row = core_panel_row();
    let panel = core_panel_surface();
    row.append(&panel);

    bind(sensors::cpu(), &panel, move |panel, c: CpuLoad| {
        let levels: Vec<f32> = c
            .per_core
            .iter()
            .map(|&load| cast::f64_to_f32(load))
            .collect();
        let frame = core_led_matrix(levels.len()).render(&levels);
        panel.set_scale(core_panel_scale(frame.width(), frame.height()));
        panel.set_pixels(
            u32::try_from(frame.width()).unwrap_or(0),
            u32::try_from(frame.height()).unwrap_or(0),
            frame.data(),
        );
        panel.set_tooltip_text(Some(&core_panel_tooltip(&c.per_core)));
    });

    row
}

fn build_live_memory_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Memory").build();
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                "—".to_string()
            } else {
                let pct = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)) * 100.0;
                format!("{} / {} ({pct:.0}%)", fmt_bytes(m.used), fmt_bytes(m.total))
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.total == 0 {
                0.0
            } else {
                let frac = cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total);
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}

fn build_live_swap_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Swap").build();

    // Hide entirely when no swap is configured.
    bind(
        sensors::memory().map(|m| m.swap_total > 0),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                String::new()
            } else {
                let pct = (cast::u64_to_f64(m.swap_used) / cast::u64_to_f64(m.swap_total)) * 100.0;
                format!(
                    "{} / {} ({pct:.0}%)",
                    fmt_bytes(m.swap_used),
                    fmt_bytes(m.swap_total)
                )
            }
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("ts-stat-progress");
    bar.set_valign(gtk::Align::Center);
    bind(
        sensors::memory().map(|m| {
            if m.swap_total == 0 {
                0.0
            } else {
                let frac = cast::u64_to_f64(m.swap_used) / cast::u64_to_f64(m.swap_total);
                frac.clamp(0.0, 1.0)
            }
        }),
        &bar,
        gtk::ProgressBar::set_fraction,
    );
    row.add_suffix(&bar);

    row
}

fn build_live_processes_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Processes").build();
    let count_label = gtk::Label::new(None);
    count_label.set_valign(gtk::Align::Center);
    bind(
        sensors::process_count().map(|n| format!("{n}")),
        &count_label,
        |label, txt| label.set_text(&txt),
    );
    row.add_suffix(&count_label);
    row
}

fn build_live_gpu_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("GPU").build();

    // Hide when no GPU detected.
    bind(
        sensors::gpu().map(|g| g.is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => state.name.clone(),
            None => String::new(),
        }),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let suffix = gtk::Label::new(None);
    suffix.set_valign(gtk::Align::Center);
    bind(
        sensors::gpu().map(|g| match g {
            Some(state) => match state.temperature_celsius {
                Some(t) => format!("{t:.0} \u{00b0}C"),
                None => match state.load {
                    Some(l) => format!("{:.0}%", l * 100.0),
                    None => String::new(),
                },
            },
            None => String::new(),
        }),
        &suffix,
        |label, txt| {
            label.set_text(&txt);
            label.set_visible(!txt.is_empty());
        },
    );
    row.add_suffix(&suffix);

    row
}

fn build_live_disk_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Disk").build();
    bind(
        sensors::disk().map(|d| format!("{} mount(s)", d.mounts.len())),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    reactive_list(
        &expander,
        sensors::disk().map(|d| d.mounts),
        |m: &sensors::DiskMount| {
            let row = adw::ActionRow::builder()
                .title(&m.path)
                .activatable(false)
                .build();
            // A mount path is whatever the filesystem is mounted at, `&`
            // included (#753).
            markup::plain_text(&row);
            let frac = if m.total_bytes > 0 {
                cast::u64_to_f64(m.used_bytes) / cast::u64_to_f64(m.total_bytes)
            } else {
                0.0
            };
            let pct = frac * 100.0;
            let label = gtk::Label::new(Some(&format!(
                "{} / {} ({pct:.0}%)",
                fmt_bytes(m.used_bytes),
                fmt_bytes(m.total_bytes),
            )));
            label.set_valign(gtk::Align::Center);
            row.add_suffix(&label);
            let bar = gtk::ProgressBar::new();
            bar.add_css_class("ts-stat-progress");
            bar.set_valign(gtk::Align::Center);
            bar.set_fraction(frac.clamp(0.0, 1.0));
            row.add_suffix(&bar);
            row
        },
        None::<fn() -> adw::ActionRow>,
    );

    expander
}

/// Disk I/O throughput history row, mirroring the network traffic row
/// ([`crate::panels::network::traffic`]): a full-width auto-scaling
/// [`Sparkline`] of the aggregate `read + write` rate, a `↓ read ↑ write`
/// current-rate line, a `min … · max …` line over the graph window, and a
/// `total ↓ … ↑ …` cumulative-since-boot line.
///
/// Soft defaults (flagged for review, both mirroring the network row):
/// - **aggregate** across physical disks, not per-device;
/// - **one combined series** (read + write summed into the graph), with the
///   read/write split shown in the value labels rather than as two lines.
fn build_history_disk_io_row() -> gtk::Box {
    // Same window the Sparkline keeps (see `build_history_row`), so the min/max
    // labels describe exactly the samples on screen.
    const WINDOW: usize = 60;

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    outer.set_hexpand(true);

    let (top_row, spark, top_value) = build_history_row("Disk I/O");
    // Throughput has no fixed ceiling — auto-scale to the window max.
    spark.set_domain_max(None);
    // Units live in the detail rows below, so the right-hand slot is redundant
    // (mirrors the network per-interface row).
    top_value.set_visible(false);
    top_row.set_hexpand(true);
    outer.append(&top_row);

    // Current ↓ read ↑ write rate.
    let rate_label = gtk::Label::new(None);
    rate_label.add_css_class("ts-stat-value");
    rate_label.set_xalign(0.0);
    rate_label.set_margin_start(crate::scale::scale(88));
    outer.append(&rate_label);

    // min / max of the combined rate over the graph window.
    let minmax_label = gtk::Label::new(None);
    minmax_label.add_css_class("ts-stat-value");
    minmax_label.set_xalign(0.0);
    minmax_label.set_margin_start(crate::scale::scale(88));
    outer.append(&minmax_label);

    // Cumulative read / write since boot.
    let total_label = gtk::Label::new(None);
    total_label.add_css_class("ts-stat-value");
    total_label.set_xalign(0.0);
    total_label.set_margin_start(crate::scale::scale(88));
    total_label.set_margin_bottom(4);
    outer.append(&total_label);

    // Our own ring of the combined-rate samples, so we can label min/max over
    // the window (the Sparkline doesn't expose its buffer).
    let window: Rc<RefCell<VecDeque<f64>>> = Rc::new(RefCell::new(VecDeque::with_capacity(WINDOW)));

    bind(sensors::disk_io_history(), &outer, move |_, h| {
        spark.set_samples(&h);
    });
    bind(sensors::disk_io(), &outer, move |_, io| {
        let combined = io.read_bps + io.write_bps;
        {
            let mut w = window.borrow_mut();
            if w.len() == WINDOW {
                w.pop_front();
            }
            w.push_back(combined);
        }

        rate_label.set_text(&format!(
            "\u{2193} {} \u{2191} {}",
            fmt_rate(io.read_bps),
            fmt_rate(io.write_bps),
        ));

        let (mut min, mut max) = (f64::INFINITY, 0.0_f64);
        for &v in window.borrow().iter() {
            min = min.min(v);
            max = max.max(v);
        }
        if !min.is_finite() {
            min = 0.0;
        }
        minmax_label.set_text(&format!(
            "min {} \u{00b7} max {}",
            fmt_rate(min),
            fmt_rate(max),
        ));

        total_label.set_text(&format!(
            "total \u{2193} {} \u{2191} {}",
            fmt_bytes(io.total_read_bytes),
            fmt_bytes(io.total_write_bytes),
        ));
    });

    outer
}

fn build_history_cpu_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("CPU");
    spark.set_domain_max(Some(1.0));

    let value_clone = value.clone();
    bind(sensors::cpu(), &row, move |_, c: CpuLoad| {
        value_clone.set_text(&format!("{:.0}%", c.overall * 100.0));
    });
    // History now lives in the sensors service (#231): snapshot the ring into
    // the sparkline rather than pushing per-emit, so it's shared across monitors
    // and survives a lazily-rebuilt page.
    bind(sensors::cpu_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

/// Expandable CPU history row: collapsed shows the overall `Sparkline`;
/// expanded switches to the per-core [`MultiSparkline`] (overall is hidden).
///
/// A [`gtk::Stack`] with `vhomogeneous(false)` and a crossfade transition
/// lets the card grow in height when the user expands. The collapsed page is
/// produced by [`build_history_cpu_row`]; the expanded page inlines the former
/// `build_history_per_core_row` content so both graphs share one row slot.
///
/// Activating the row (clicking) toggles between states. Expanded state is
/// **not** persisted across drawer open/close — each rebuild starts collapsed.
///
/// Returns a [`gtk::ListBoxRow`] so it slots into the [`adw::PreferencesGroup`]
/// boxed-list in source order (same routing fix as [`history_row_wrapper`]).
fn build_expandable_cpu_history_row() -> gtk::ListBoxRow {
    // ── Collapsed page: overall CPU sparkline ──────────────────────────────
    let overall_box = build_history_cpu_row();

    // ── Expanded page: per-core MultiSparkline ─────────────────────────────
    // Per-core load is a fraction in 0..=1, so the domain is fixed (same as
    // the overall sparkline) rather than auto-scaled.
    let percore_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    percore_box.add_css_class("ts-history-row");
    percore_box.set_hexpand(true);

    let percore_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let percore_name = gtk::Label::new(Some("Per-core history"));
    percore_name.add_css_class("ts-stat-name");
    percore_name.set_xalign(0.0);
    percore_name.set_hexpand(true);
    percore_header.append(&percore_name);

    let percore_value = gtk::Label::new(None);
    percore_value.add_css_class("ts-stat-value");
    percore_value.set_xalign(1.0);
    percore_header.append(&percore_value);
    percore_box.append(&percore_header);

    let percore_graph = MultiSparkline::new(60);
    percore_graph.set_domain_max(Some(1.0));
    percore_graph.widget().set_hexpand(true);
    percore_box.append(percore_graph.widget());

    // History now lives in the sensors service (#338): snapshot the per-core
    // ring set into the MultiSparkline via `set_frames` rather than pushing a
    // frame per emit, so it's shared across monitors and backfills a lazily
    // (re)built page instantly. The value label still reads the live snapshot.
    let percore_value_c = percore_value.clone();
    bind(sensors::cpu(), &percore_box, move |_, c: CpuLoad| {
        percore_value_c.set_text(&format!(
            "{} cores \u{00b7} {:.0}%",
            c.per_core.len(),
            c.overall * 100.0
        ));
    });
    let percore_graph_c = percore_graph.clone();
    bind(
        sensors::cpu_per_core_history(),
        &percore_box,
        move |_, h| {
            percore_graph_c.set_frames(&h);
        },
    );

    // ── Stack: vhomogeneous(false) so the card grows on expand ───────────
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_hexpand(true);
    stack.add_named(&overall_box, Some("overall"));
    stack.add_named(&percore_box, Some("percore"));
    // Default to collapsed; not persisted across rebuilds.
    stack.set_visible_child_name("overall");

    // ── Chevron: trailing affordance showing collapsed/expanded state ─────
    let chevron = gtk::Image::from_icon_name("pan-end-symbolic");
    chevron.set_valign(gtk::Align::Center);
    chevron.set_icon_size(gtk::IconSize::Normal);

    // ── Outer box: hosts stack + chevron, receives the GestureClick ───────
    // adw::PreferencesGroup's internal GtkListBox does NOT activate plain
    // GtkListBoxRows on click (only its own Adw row types), so we cannot rely
    // on connect_activate.  A GestureClick on the content widget fires directly
    // on pointer release, independent of list activation.
    let outer_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    outer_box.set_hexpand(true);
    outer_box.append(&stack);
    outer_box.append(&chevron);

    let gesture = gtk::GestureClick::new();
    let stack_for_gesture = stack.clone();
    let chevron_for_gesture = chevron.clone();
    gesture.connect_released(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let expanded = stack_for_gesture.visible_child_name().as_deref() == Some("percore");
        if expanded {
            stack_for_gesture.set_visible_child_name("overall");
            chevron_for_gesture.set_icon_name(Some("pan-end-symbolic"));
        } else {
            stack_for_gesture.set_visible_child_name("percore");
            chevron_for_gesture.set_icon_name(Some("pan-down-symbolic"));
        }
    });
    outer_box.add_controller(gesture);

    // ── Row wrapper: NOT activatable (gesture handles click) ──────────────
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_hexpand(true);
    row.set_child(Some(&outer_box));

    row
}

/// Collapsed page for [`build_expandable_cpu_clock_row`]: aggregate CPU clock
/// `Sparkline`. Per #214's locked design, the aggregate is the **maximum**
/// current frequency across cores (`CpuFreq::max_hz`), and the graph is
/// normalized against `max_ceiling_hz` (the highest `cpuinfo_max_freq`) so the
/// axis is a fixed 0→max-clock domain that shows headroom rather than
/// auto-scaling to the live window.
fn build_history_cpu_clock_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Clock");
    spark.set_domain_max(Some(1.0));

    let value_clone = value.clone();
    bind(sensors::cpu_freq(), &row, move |_, f: CpuFreq| {
        value_clone.set_text(&fmt_hz(f.max_hz));
    });
    // History (the max_ceiling_hz-normalized aggregate clock) is hoisted into
    // the sensors service (#338): snapshot the ring rather than pushing per-emit,
    // so it's shared across monitors and survives a lazily-rebuilt page.
    bind(sensors::cpu_freq_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

/// Expandable CPU-clock history row: collapsed shows the aggregate (max
/// across cores) clock `Sparkline`; expanded switches to the per-core
/// [`MultiSparkline`], each series normalized to the shared `max_ceiling_hz`.
///
/// This is #214, the UI half completing the cpufreq work merged in #241 —
/// same locked design as #210's CPU-usage row (Mara: reusable expand
/// mechanism, new row in the existing CPU card, not a separate card). The
/// Stack/chevron/`GestureClick` toggle here is a deliberate **parallel** of
/// [`build_expandable_cpu_history_row`] rather than a shared helper: the two
/// rows' collapsed/expanded content (sources, normalization, value
/// formatting) differ enough that factoring out just the toggle shell would
/// leave two near-identical stubs calling into it, without shrinking the
/// per-row bind logic that's the actual bulk of each function. If a third
/// expandable metric shows up, that's the point to extract the shared shell.
///
/// Graceful-degrade: when there's no cpufreq governor (VMs, missing
/// `/sys/.../cpufreq`), `sensors::cpu_freq()` publishes an empty `CpuFreq`
/// (`max_ceiling_hz == 0.0`); this row hides entirely rather than showing a
/// flat/meaningless graph, mirroring the GPU card's and swap row's self-hide.
///
/// Returns a [`gtk::ListBoxRow`] so it slots into the [`adw::PreferencesGroup`]
/// boxed-list in source order (same routing fix as [`history_row_wrapper`]).
fn build_expandable_cpu_clock_row() -> gtk::ListBoxRow {
    // ── Collapsed page: aggregate (max) clock sparkline ────────────────────
    let overall_box = build_history_cpu_clock_row();

    // ── Expanded page: per-core MultiSparkline ─────────────────────────────
    // Each core's current frequency normalized against the shared
    // max_ceiling_hz, so the domain stays a fixed 0..=1 (same axis as the
    // collapsed page) rather than auto-scaling.
    let percore_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    percore_box.add_css_class("ts-history-row");
    percore_box.set_hexpand(true);

    let percore_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let percore_name = gtk::Label::new(Some("Per-core clock"));
    percore_name.add_css_class("ts-stat-name");
    percore_name.set_xalign(0.0);
    percore_name.set_hexpand(true);
    percore_header.append(&percore_name);

    let percore_value = gtk::Label::new(None);
    percore_value.add_css_class("ts-stat-value");
    percore_value.set_xalign(1.0);
    percore_header.append(&percore_value);
    percore_box.append(&percore_header);

    let percore_graph = MultiSparkline::new(60);
    percore_graph.set_domain_max(Some(1.0));
    percore_graph.widget().set_hexpand(true);
    percore_box.append(percore_graph.widget());

    // Per-core clock history (each core normalized to the shared
    // max_ceiling_hz) is hoisted into the sensors service (#338): snapshot the
    // ring set via `set_frames` rather than pushing per-emit. The value label
    // still reads the live snapshot.
    let percore_value_c = percore_value.clone();
    bind(sensors::cpu_freq(), &percore_box, move |_, f: CpuFreq| {
        percore_value_c.set_text(&format!(
            "{} cores \u{00b7} {}",
            f.per_core.len(),
            fmt_hz(f.max_hz)
        ));
    });
    let percore_graph_c = percore_graph.clone();
    bind(
        sensors::cpu_freq_per_core_history(),
        &percore_box,
        move |_, h| {
            percore_graph_c.set_frames(&h);
        },
    );

    // ── Stack: vhomogeneous(false) so the card grows on expand ───────────
    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_hexpand(true);
    stack.add_named(&overall_box, Some("overall"));
    stack.add_named(&percore_box, Some("percore"));
    // Default to collapsed; not persisted across rebuilds.
    stack.set_visible_child_name("overall");

    // ── Chevron: trailing affordance showing collapsed/expanded state ─────
    let chevron = gtk::Image::from_icon_name("pan-end-symbolic");
    chevron.set_valign(gtk::Align::Center);
    chevron.set_icon_size(gtk::IconSize::Normal);

    // ── Outer box: hosts stack + chevron, receives the GestureClick ───────
    // adw::PreferencesGroup's internal GtkListBox does NOT activate plain
    // GtkListBoxRows on click (only its own Adw row types), so we cannot rely
    // on connect_activate.  A GestureClick on the content widget fires directly
    // on pointer release, independent of list activation.
    let outer_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    outer_box.set_hexpand(true);
    outer_box.append(&stack);
    outer_box.append(&chevron);

    let gesture = gtk::GestureClick::new();
    let stack_for_gesture = stack.clone();
    let chevron_for_gesture = chevron.clone();
    gesture.connect_released(move |gesture, _, _, _| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let expanded = stack_for_gesture.visible_child_name().as_deref() == Some("percore");
        if expanded {
            stack_for_gesture.set_visible_child_name("overall");
            chevron_for_gesture.set_icon_name(Some("pan-end-symbolic"));
        } else {
            stack_for_gesture.set_visible_child_name("percore");
            chevron_for_gesture.set_icon_name(Some("pan-down-symbolic"));
        }
    });
    outer_box.add_controller(gesture);

    // ── Row wrapper: NOT activatable (gesture handles click) ──────────────
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    row.set_hexpand(true);
    row.set_child(Some(&outer_box));

    // Graceful-degrade: hide the whole row when no cpufreq governor is
    // present (empty CpuFreq → max_ceiling_hz == 0.0), same convention as the
    // GPU card / swap row self-hides above.
    bind(
        sensors::cpu_freq().map(|f| f.max_ceiling_hz > 0.0),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    row
}

fn build_history_memory_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Memory");
    spark.set_domain_max(Some(1.0));

    let value_clone = value.clone();
    bind(sensors::memory(), &row, move |_, m| {
        if m.total == 0 {
            value_clone.set_text("\u{2014}");
        } else {
            let frac = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)).clamp(0.0, 1.0);
            value_clone.set_text(&format!("{:.0}%", frac * 100.0));
        }
    });
    bind(sensors::memory_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

fn build_history_gpu_usage_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("GPU usage");
    // Usage is a percentage — fix the scale to 0..=100 % (unlike temp, which
    // auto-scales). We push raw percent (0..=100), so the domain max is 100.
    spark.set_domain_max(Some(100.0));

    // Hide unless GPU is present with a load reading. Intel iGPUs report load
    // without a temperature, so this gate keys off `load` (not temp).
    bind(
        sensors::gpu().map(|g| g.and_then(|s| s.load).is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some(l) = state.load
        {
            value_clone.set_text(&format!("{:.0}%", l * 100.0));
        }
    });
    bind(sensors::gpu_load_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

fn build_history_gpu_vram_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("GPU VRAM");
    // VRAM is a percentage of total memory — fix the scale to 0..=100 % like
    // GPU usage (we push raw percent, so the domain max is 100).
    spark.set_domain_max(Some(100.0));

    // Hide unless GPU is present with both used + total VRAM readings (some
    // GPUs report load/temp but not memory).
    bind(
        sensors::gpu().map(|g| {
            g.and_then(|s| s.memory_used_bytes.zip(s.memory_total_bytes))
                .is_some()
        }),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some((used, total)) = state.memory_used_bytes.zip(state.memory_total_bytes)
            && total > 0
        {
            let pct = (cast::u64_to_f64(used) / cast::u64_to_f64(total) * 100.0).clamp(0.0, 100.0);
            value_clone.set_text(&format!("{pct:.0}%"));
        }
    });
    bind(sensors::gpu_vram_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

fn build_history_gpu_temp_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("GPU temp");
    spark.set_domain_max(None);

    // Hide unless GPU is present with a temperature reading.
    bind(
        sensors::gpu().map(|g| g.and_then(|s| s.temperature_celsius).is_some()),
        &row,
        gtk::prelude::WidgetExt::set_visible,
    );

    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some(t) = state.temperature_celsius
        {
            value_clone.set_text(&format!("{t:.0} \u{00b0}C"));
        }
    });
    bind(sensors::gpu_temp_history(), &row, move |_, h| {
        spark.set_samples(&h);
    });

    row
}

/// Services card — two boxed lists of "what on this box is broken", stacked:
/// systemd's failed units ([`build_failed_units_group`]) and the shell's own
/// flapping supervised tasks ([`build_flapping_tasks_group`], #722).
///
/// The flapping list is folded in here rather than given a page — or a
/// control-center tab — of its own because #721's health table is a
/// process-global static *inside the shell*: a control-center view would link,
/// compile, and report zero flapping tasks forever. This card already means
/// "what on this box is broken", and a shell task that keeps panicking belongs
/// next to a systemd unit that keeps failing.
///
/// Both halves are silent while everything is healthy, so on a healthy box this
/// card renders as nothing at all — exactly what the failed-units group did on
/// its own before #722.
fn build_stats_services_card() -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.append(&build_failed_units_group());
    card.append(&build_flapping_tasks_group());
    card
}

/// Failed systemd units — the first half of the Services card, flattened per
/// #311: no group description and no `Failed units` expander wrapper (both
/// duplicated the count already shown on the bar chip, and the expander hid the
/// one thing this flyout is opened to see). The failed-unit `ActionRow`s render
/// straight into the group, so the flyout *is* the list, matching the other
/// stats panels' pattern of showing their primary content directly rather than
/// behind a titled row. If every unit recovers while the panel is open, the
/// list just goes empty.
fn build_failed_units_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    reactive_list(
        &group,
        systemd::failed_units(),
        |unit: &systemd::FailedUnit| {
            let row = adw::ActionRow::builder()
                .title(&unit.name)
                .activatable(false)
                .build();
            // Unit names and, especially, free-form `Description=` text are
            // arbitrary — an `&` there would blank the row this flyout exists
            // to show (#753).
            markup::plain_text(&row);
            let subtitle = if unit.description.is_empty() {
                unit.sub_state.clone()
            } else {
                unit.description.clone()
            };
            row.set_subtitle(&subtitle);

            let pill = gtk::Label::new(Some("failed"));
            pill.set_valign(gtk::Align::Center);
            pill.add_css_class("ts-pill-error");
            row.add_suffix(&pill);

            row
        },
        None::<fn() -> adw::ActionRow>,
    );

    group
}

/// Flapping supervised tasks — the shell's own half of the Services card
/// (#722), bound to the health surface `hytte_reactive::health` publishes
/// (#721). One row per *live* supervisor whose panic streak is non-zero (see
/// [`is_flapping`]).
///
/// Unlike its sibling this group carries a title and hides itself outright
/// while nothing is flapping. An untitled empty group renders as nothing, but a
/// *titled* empty one would leave a permanent header sitting on a healthy card;
/// hiding also keeps the card's own inter-group spacing honest. The title is a
/// literal, so it needs no [`markup::escape`] — an `AdwPreferencesGroup`'s
/// title label is hardcoded `use-markup="True"` with no property to flip.
///
/// `TaskHealth::name` is explicitly non-unique — `sensors` supervises four
/// tasks under one label, `upower` three — so a service with two flapping tasks
/// shows two identically-titled rows, told apart by their numbers. That is
/// deliberate: `TaskId` has no accessor to disambiguate them with, and
/// collapsing them onto the name would report one task's restarts as another's.
///
/// Entries are live, not historical (#721): a task whose supervisor has stopped
/// has no row at all, and nothing here survives a shell restart. This answers
/// "is anything flapping *right now*", which is the question the bar chip that
/// leads here also answers.
fn build_flapping_tasks_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Flapping shell tasks")
        .build();

    bind(
        flapping_tasks().map(|tasks| !tasks.is_empty()),
        &group,
        gtk::prelude::WidgetExt::set_visible,
    );

    reactive_list(
        &group,
        flapping_tasks(),
        |task: &TaskHealth| {
            let row = adw::ActionRow::builder()
                .title(task.name)
                .activatable(false)
                .build();
            // Task labels are `&'static str` literals today and the subtitle is
            // generated here, but keeping both literal by construction costs one
            // call and survives a future `name` that interpolates (#753).
            markup::plain_text(&row);
            row.set_subtitle(&flapping_subtitle(
                task.state,
                task.runs,
                task.panics,
                task.consecutive_panics,
                task.backoff,
            ));

            let pill = gtk::Label::new(Some("flapping"));
            pill.set_valign(gtk::Align::Center);
            pill.add_css_class("ts-pill-error");
            row.add_suffix(&pill);

            row
        },
        None::<fn() -> adw::ActionRow>,
    );

    group
}

/// The supervised tasks worth showing, in `health::signal`'s own order (the
/// order supervision started).
///
/// `dedupe_cloned` because the health signal is chatty relative to this view:
/// it emits on *every* supervisor transition, including the start-up burst of
/// one event per service task, and almost none of those change the flapping
/// list. Subscribing twice (visibility + rows) is fine — `signal_cloned` fans
/// out — and keeps each binding reading the filter it actually cares about.
fn flapping_tasks() -> impl Signal<Item = Vec<TaskHealth>> {
    health::signal()
        .map(|tasks| {
            tasks
                .into_iter()
                .filter(|task| is_flapping(task.consecutive_panics))
                .collect::<Vec<_>>()
        })
        .dedupe_cloned()
}

/// Whether a supervised task's panic streak makes it worth surfacing (#722).
///
/// The field is `TaskHealth::consecutive_panics` — panics since the last run
/// that stayed up long enough for the supervisor to call it healthy (30 s by
/// default), so it resets on the very verdict that resets the backoff. Zero
/// means "not flapping *now*" whatever the lifetime `panics` total says, which
/// is the whole reason #721 published both numbers: a task that crashed once an
/// hour ago and has been fine since is not a diagnostic.
///
/// Shared with `crate::widgets::services`, whose chip has to appear on exactly
/// the tasks this group renders: the two predicates drifting apart would leave
/// the group unreachable in `split` layout, where that chip is the only route
/// to this card.
pub(crate) fn is_flapping(consecutive_panics: u32) -> bool {
    consecutive_panics > 0
}

/// The subtitle for one flapping task's row: what it is doing now, how hard it
/// is flapping, and how much of that is recent.
///
/// Takes the fields rather than a `&TaskHealth` so it stays testable —
/// `TaskId`'s inner `u64` is private and it has no constructor, so a
/// `TaskHealth` cannot be built outside `hytte-reactive` at all (#721 kept that
/// API deliberately minimal, and #722 is not the reason to widen it).
///
/// The lifetime total is spelled out only when it differs from the streak;
/// otherwise "3 panics in a row · 3 total" says the same thing twice.
fn flapping_subtitle(
    state: TaskState,
    runs: u32,
    panics: u32,
    consecutive_panics: u32,
    backoff: Duration,
) -> String {
    let mut parts = Vec::with_capacity(4);
    parts.push(match state {
        // `backoff` is the supervisor's live capped-exponential delay (1 s →
        // 30 s) and reads ZERO while a run is actually in flight, so a
        // sub-second value has no countdown worth printing.
        TaskState::Restarting if backoff.as_secs() > 0 => {
            format!("Restarting in {}s", backoff.as_secs())
        }
        TaskState::Restarting => "Restarting".to_owned(),
        TaskState::Running => "Running".to_owned(),
    });
    let panic_word = if consecutive_panics == 1 {
        "panic"
    } else {
        "panics"
    };
    parts.push(format!("{consecutive_panics} {panic_word} in a row"));
    if panics > consecutive_panics {
        parts.push(format!("{panics} total"));
    }
    let run_word = if runs == 1 { "run" } else { "runs" };
    parts.push(format!("{runs} {run_word}"));
    parts.join(" \u{00b7} ")
}

#[cfg(test)]
mod tests {
    use super::{
        CORE_PANEL_MAX_H, CORE_PANEL_MAX_W, ColorMap, CoreLeds, DisplayStyle, Duration, Fill,
        StatsLayout, StatsSection, TaskState, core_led_matrix_for, core_panel_scale,
        core_panel_tooltip, flapping_subtitle, is_flapping, parse_core_leds_color,
        parse_core_leds_fill, parse_core_leds_rows, parse_core_leds_style, parse_hex_rgb,
        parse_stats_layout,
    };

    /// The [`StatsSection`] declaration order is the panel's canonical
    /// resource order and must agree with the always-visible bar chip order
    /// (`main.rs`'s CPU/Memory/GPU/Disk/Services resource `group`, #571).
    /// Every `sections` vec in this file (and the multicolumn grid
    /// coordinates) is built by hand rather than derived from the enum, so
    /// this can't catch every drift on its own — but a fieldless enum's
    /// implicit discriminants follow declaration order, so pinning them here
    /// is a cheap, GTK-free tripwire against an accidental reshuffle of the
    /// enum itself falling back out of sync with the bar.
    #[test]
    fn stats_section_declaration_order_matches_bar() {
        assert_eq!(StatsSection::Cpu as u8, 0);
        assert_eq!(StatsSection::Memory as u8, 1);
        assert_eq!(StatsSection::Gpu as u8, 2);
        assert_eq!(StatsSection::Disks as u8, 3);
        assert_eq!(StatsSection::Services as u8, 4);
    }

    /// Unset resolves to the documented `multicolumn` default, silently — no
    /// warn (the `stats_layout` accessor only warns on a present-but-unrecognized
    /// value). Matches the repo's `TROLLSHELL_RECORD_AUDIO` / `_WEATHER_CITY`
    /// convention that an unset override reads as its default (#508, #566).
    #[test]
    fn parse_unset_is_multicolumn() {
        assert_eq!(parse_stats_layout(None), Ok(StatsLayout::Multicolumn));
    }

    /// Each recognized token maps to its layout.
    #[test]
    fn parse_known_tokens() {
        assert_eq!(
            parse_stats_layout(Some("combined")),
            Ok(StatsLayout::Combined)
        );
        assert_eq!(
            parse_stats_layout(Some("multicolumn")),
            Ok(StatsLayout::Multicolumn)
        );
        assert_eq!(parse_stats_layout(Some("split")), Ok(StatsLayout::Split));
    }

    /// A present-but-unrecognized value is returned as `Err` so `stats_layout`
    /// can warn once and fall back to multicolumn — case-sensitive, no fuzzy
    /// match.
    #[test]
    fn parse_unknown_is_err() {
        assert_eq!(parse_stats_layout(Some("Combined")), Err("Combined"));
        assert_eq!(parse_stats_layout(Some("grid")), Err("grid"));
        assert_eq!(parse_stats_layout(Some("")), Err(""));
    }

    // ── The per-core LED panel (#857) ────────────────────────────────────────

    /// **The #702 non-regression, stated as a number.** The panel the drawer
    /// actually asks for at 64 cores is 93 × 1 = **93 px** wide, against the
    /// 764 px hard minimum the pre-#702 bar strip had — and unlike that strip
    /// this is a *natural* size, not a floor: `PixelSurface::measure` hard-codes
    /// its minimum to 0.
    ///
    /// Falsified by growing the kit's cell metrics (`CELL`/`GAP`/`PAD`) or by
    /// letting `core_panel_scale` blow the panel past the budget box.
    #[test]
    fn the_panel_is_a_rectangle_at_64_cores() {
        let m = core_led_matrix_for(CoreLeds::default(), 64);
        assert_eq!((m.cols(), m.rows()), (16, 4), "64 cores is a 16x4 panel");
        assert_eq!((m.width(), m.height()), (181, 49));
        assert_eq!(core_panel_scale(m.width(), m.height()), 1);
        assert!(
            m.width() > m.height(),
            "the default shape must be a rectangle, not #861's square"
        );
        assert!(
            m.width() < 764 / 4,
            "the panel must stay a small fraction of #702's 764 px strip"
        );
    }

    /// The rectangle default holds across **every** core count the shell can
    /// meet, not just the 64-thread box that motivated it — this is the table
    /// in the #857 PR body, checked rather than claimed.
    ///
    /// The invariant that matters is `cols > rows`: a pinned `rows = 4` would
    /// satisfy it at 64 and fail it at 4 (a 1×4 column), which is exactly why
    /// the shape is computed rather than configured. One lamp is the only count
    /// with no rectangle available.
    ///
    /// Falsified by pointing [`core_led_matrix_for`]'s `None` arm back at
    /// `LedMatrix::rect` — 4, 16 and 64 cores all go square.
    #[test]
    fn every_core_count_gets_a_rectangle() {
        // (cores, cols, rows) — the shipping table.
        let table = [
            (1usize, 1usize, 1usize),
            (2, 2, 1),
            (4, 4, 1),
            (8, 4, 2),
            (12, 6, 2),
            (16, 8, 2),
            (32, 11, 3),
            (64, 16, 4),
            (128, 22, 6),
        ];
        for (cores, cols, rows) in table {
            let m = core_led_matrix_for(CoreLeds::default(), cores);
            assert_eq!(
                (m.cols(), m.rows()),
                (cols, rows),
                "{cores} cores drifted off the shipping shape"
            );
            assert!(
                m.cols() * m.rows() >= cores,
                "{cores} cores lost a lamp in a {cols}x{rows} panel"
            );
            if cores >= 2 {
                assert!(
                    m.cols() > m.rows(),
                    "{cores} cores: {cols}x{rows} is not wider than it is tall"
                );
                assert!(
                    m.width() > m.height(),
                    "{cores} cores: the {cols}x{rows} buffer is not a rectangle"
                );
            }
        }
    }

    /// A pinned `TROLLSHELL_CORE_LEDS_ROWS` still overrides the automatic
    /// shape — including into a shape that is *worse* than the default, which
    /// is the user's call to make.
    ///
    /// Falsified by dropping [`core_led_matrix_for`]'s `Some(rows)` arm: the
    /// override silently becomes a no-op and 4 cores stay 4x1.
    #[test]
    fn a_pinned_row_count_beats_the_automatic_shape() {
        let pinned = CoreLeds {
            rows: Some(4),
            ..CoreLeds::default()
        };
        assert_eq!(
            {
                let m = core_led_matrix_for(pinned, 4);
                (m.cols(), m.rows())
            },
            (1, 4),
            "the pinned row count must win, even at 1x4"
        );
        assert_eq!(
            {
                let m = core_led_matrix_for(pinned, 64);
                (m.cols(), m.rows())
            },
            (16, 4)
        );
    }

    /// Whatever the core count and whatever the shape, the *requested* panel
    /// fits the budget box — so no machine can make the CPU card demand a
    /// width or height the drawer has to grow to.
    ///
    /// Falsified by taking the max instead of the min of the two scale bounds,
    /// or by dropping either bound.
    #[test]
    fn every_core_count_fits_the_budget_box() {
        for cores in 1..=512usize {
            for shape in [None, Some(1usize), Some(2), Some(3)] {
                let m = core_led_matrix_for(
                    CoreLeds {
                        rows: shape,
                        ..CoreLeds::default()
                    },
                    cores,
                );
                let scale = usize::try_from(core_panel_scale(m.width(), m.height()))
                    .expect("u32 fits usize");
                assert!(scale >= 1, "{cores}/{shape:?} scaled to nothing");
                // A 1x panel that is *already* wider than the budget is fine —
                // its minimum is still 0 and it letterboxes down. What must
                // never happen is the scale making it worse.
                if scale > 1 {
                    assert!(
                        m.width() * scale <= CORE_PANEL_MAX_W
                            && m.height() * scale <= CORE_PANEL_MAX_H,
                        "{cores} cores / rows {shape:?}: {}x{} at {scale}x escapes the budget",
                        m.width(),
                        m.height()
                    );
                }
            }
        }
    }

    /// The scale never *shrinks* the buffer — a fractional or sub-1 factor
    /// would resample the fixed dot grid, which is the one thing #839/#843 buy
    /// us. A panel too big for the budget stays at 1x and letterboxes instead.
    ///
    /// Falsified by dropping the `.max(1)`.
    #[test]
    fn the_scale_never_shrinks_the_panel() {
        assert_eq!(
            core_panel_scale(1000, 1000),
            1,
            "an oversized panel stays 1x"
        );
        assert_eq!(core_panel_scale(0, 0), 1, "a degenerate buffer stays 1x");
        // The small-machine shapes get the blow-up that keeps them legible.
        assert!(core_panel_scale(16, 16) > 1, "a 1-core panel is blown up");
        assert!(core_panel_scale(30, 30) > 1, "a 4-core panel is blown up");
    }

    /// The hover readout names the core count, the average and the busiest
    /// core — the thing the per-bar `"42%"` tooltip could never say.
    ///
    /// Falsified by folding on `>=` instead of `>` (the busiest index becomes
    /// the last tied core, not the first) or by summing instead of averaging.
    #[test]
    fn the_tooltip_names_the_busiest_core() {
        assert_eq!(core_panel_tooltip(&[]), "No per-core data");
        assert_eq!(
            core_panel_tooltip(&[0.0, 0.5, 1.0, 0.5]),
            "4 cores \u{00b7} avg 50% \u{00b7} max 100% (core 2)"
        );
        assert_eq!(
            core_panel_tooltip(&[0.02]),
            "1 cores \u{00b7} avg 2% \u{00b7} max 2% (core 0)"
        );
        // A garbage sample can't win the "busiest" title or produce a negative
        // reading — the panel is fed straight off /proc and has been wrong
        // before.
        let odd = core_panel_tooltip(&[f64::NAN, 0.3]);
        assert!(odd.contains("(core 1)"), "NaN took the busiest slot: {odd}");
        assert!(
            core_panel_tooltip(&[-1.0]).contains("max 0%"),
            "a negative load reported below zero"
        );
    }

    /// The env knobs resolve to the documented defaults when unset, accept
    /// every value the warning text advertises, and reject anything else so
    /// `core_leds_var` can warn rather than silently pick something.
    #[test]
    fn the_led_panel_env_knobs_parse() {
        let default = CoreLeds::default();
        // Unset is the default, silently — the `TROLLSHELL_STATS_LAYOUT` rule.
        assert_eq!(parse_core_leds_style(None), Ok(default.style));
        assert_eq!(parse_core_leds_color(None), Ok(default.color));
        assert_eq!(parse_core_leds_rows(None), Ok(default.rows));
        assert_eq!(parse_core_leds_fill(None), Ok(default.fill));

        // Every skin the warning names, by the kit's own `name()` vocabulary.
        for style in DisplayStyle::ALL {
            assert_eq!(parse_core_leds_style(Some(style.name())), Ok(style));
        }
        assert_eq!(parse_core_leds_style(Some("plasma")), Err("plasma"));

        // Every named map, plus the `#rrggbb` literal.
        for map in ColorMap::ALL {
            assert_eq!(parse_core_leds_color(Some(map.name())), Ok(map));
        }
        assert_eq!(
            parse_core_leds_color(Some("#9b59b6")),
            Ok(ColorMap::Rgb(0x9b, 0x59, 0xb6))
        );
        assert_eq!(parse_core_leds_color(Some("puce")), Err("puce"));
        // `rgb` is `ColorMap::name`'s output but not an input: it carries no
        // components, so accepting it would mean inventing a colour.
        assert_eq!(parse_core_leds_color(Some("rgb")), Err("rgb"));

        assert_eq!(parse_core_leds_rows(Some("rect")), Ok(None));
        assert_eq!(parse_core_leds_rows(Some("3")), Ok(Some(3)));
        assert_eq!(
            parse_core_leds_rows(Some("0")),
            Err("0"),
            "0 rows is a typo"
        );
        assert_eq!(parse_core_leds_rows(Some("-2")), Err("-2"));
        assert_eq!(parse_core_leds_rows(Some("many")), Err("many"));

        assert_eq!(parse_core_leds_fill(Some("spare")), Ok(Fill::Spare));
        assert_eq!(parse_core_leds_fill(Some("blank")), Ok(Fill::Blank));
        assert_eq!(parse_core_leds_fill(Some("none")), Err("none"));
    }

    /// The `#rrggbb` literal: with or without the hash, either case, and
    /// nothing else — a short, long or non-hex string is rejected rather than
    /// silently truncated.
    #[test]
    fn hex_colours_parse_both_ways() {
        assert_eq!(
            parse_hex_rgb("#ff8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_hex_rgb("ff8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(
            parse_hex_rgb("FF8800"),
            Some(ColorMap::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(parse_hex_rgb("#f80"), None, "short hex is not accepted");
        assert_eq!(parse_hex_rgb("#ff8800ff"), None, "alpha is not accepted");
        assert_eq!(parse_hex_rgb("#gg8800"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    /// The flapping filter keys on the *streak*, and its boundary is 1, not
    /// some "enough restarts to be worth mentioning" threshold: the supervisor
    /// has already applied that judgement by resetting the streak on any run
    /// that stayed up 30 s. A non-zero streak means the task is panicking now.
    #[test]
    fn only_a_live_panic_streak_counts_as_flapping() {
        assert!(
            !is_flapping(0),
            "a task with no panics since its last healthy run is not flapping, whatever its \
             lifetime total"
        );
        assert!(
            is_flapping(1),
            "one panic since the last healthy run counts"
        );
        assert!(is_flapping(9));
    }

    /// While restarting, the countdown leads — it is the "when does this come
    /// back" the row is opened to answer — and doubles as a severity reading
    /// (the supervisor's delay caps at 30 s).
    #[test]
    fn flapping_subtitle_leads_with_the_backoff_while_restarting() {
        assert_eq!(
            flapping_subtitle(TaskState::Restarting, 7, 5, 3, Duration::from_secs(4)),
            "Restarting in 4s \u{00b7} 3 panics in a row \u{00b7} 5 total \u{00b7} 7 runs"
        );
    }

    /// A run is in flight, so there is no delay to count down — but the streak
    /// is still live (the run has not yet lasted long enough to reset it), which
    /// is precisely why the row is still on screen.
    #[test]
    fn flapping_subtitle_says_running_when_a_run_is_in_flight() {
        assert_eq!(
            flapping_subtitle(TaskState::Running, 12, 11, 3, Duration::ZERO),
            "Running \u{00b7} 3 panics in a row \u{00b7} 11 total \u{00b7} 12 runs"
        );
    }

    /// A sub-second backoff has no whole second to print; say the state and
    /// drop the countdown rather than claiming "Restarting in 0s".
    #[test]
    fn flapping_subtitle_drops_a_sub_second_countdown() {
        assert_eq!(
            flapping_subtitle(TaskState::Restarting, 2, 1, 1, Duration::from_millis(400)),
            "Restarting \u{00b7} 1 panic in a row \u{00b7} 2 runs"
        );
    }

    /// The lifetime total is redundant while every panic is part of the current
    /// streak — the common case for a task that has only ever flapped.
    #[test]
    fn flapping_subtitle_omits_a_total_that_equals_the_streak() {
        assert_eq!(
            flapping_subtitle(TaskState::Running, 4, 3, 3, Duration::ZERO),
            "Running \u{00b7} 3 panics in a row \u{00b7} 4 runs"
        );
    }

    /// Whatever the core count, the rectangular panel has a lamp for every
    /// core and no wholly empty row — the shape contract the CPU card
    /// depends on, checked from the shell side of the boundary too so a kit
    /// change that lost lamps would fail here as well as in `hytte-preem`.
    #[test]
    fn the_panel_has_a_lamp_for_every_core() {
        for cores in 1..=512usize {
            let m = core_led_matrix_for(CoreLeds::default(), cores);
            assert!(
                m.cols() * m.rows() >= cores,
                "{cores} cores lost lamps in a {}x{} panel",
                m.cols(),
                m.rows()
            );
            assert!(
                (m.rows() - 1) * m.cols() < cores,
                "{cores} cores left a spare row in a {}x{} panel",
                m.cols(),
                m.rows()
            );
        }
    }
}

/// The `RefCell`-across-a-GTK-call abort class (#674) for this file.
///
/// ## Why there *is* a production change alongside this test
///
/// Unlike `widgets/workspaces.rs`'s `update_workspaces` or `widgets/tasks.rs`'s
/// `rebuild_list`, this file's rebuild pass used to live **inline inside
/// [`build_top_apps_expander`]'s `bind` apply closure**, with both of its cells
/// created as closure captures. Nothing could call it and nothing could reach
/// the cells. So the body was hoisted into [`rebuild_top_apps`] and the closure
/// became a single forwarding call — a mechanical extract, provable as such:
/// diffing the moved body against `origin/main`'s closure body after
/// normalising one indentation level, the three capture→parameter renames and
/// `for s in &list` → `for s in list` yields **nothing**. The exact command is
/// in the PR that landed this.
///
/// Driving the loop through the bound signal instead would not have worked:
/// `bind` polls its signal from a `glib::MainContext` task, so a
/// `Mutable::set` issued from inside an apply wakes that task for the *next*
/// main-context iteration. The re-entry would be deferred, no borrow would
/// still be live, and the test would pass against the unfixed code too (#828's
/// finding).
///
/// ## Why the probe is `destroy`
///
/// PR #817 (`overlays/notifications.rs`) found that a removed widget's
/// `destroy` can be silently deferred when something outside the cell under
/// test still holds it — its toast cards kept a focusable dismiss button alive
/// past `vbox.remove()`, so `destroy` never fired *inside* the call and the
/// probe had to become `unmap`. Probe choice is per-site and empirical.
///
/// That trap does not apply here, and it was checked rather than assumed: the
/// `adw::ExpanderRow` this test builds is never added to any window, so there
/// is no `GtkRoot` and no focus-widget chain to retain a removed row past
/// `expander.remove()` — `destroy` fires off pure refcounting, as it does for
/// `panels/connections.rs`'s other-users bucket, which uses the same
/// `add_row`/`remove` pair on an `adw::ExpanderRow`. The test asserts this
/// rather than trusting it: `fired_inside == Some(true)` is the anti-vacuity
/// guard, and it is the only reason a probe that never re-enters would be
/// caught instead of shipping a decorative green.
///
/// ## The second cell, `meta_cache`: two of its three sites are NOT covered
///
/// `meta_cache` is a different hazard shape from `rows_track` — short borrows
/// *inside* the loop rather than a take/write-back — and #663 fixed it at two
/// sites (the third is #832's, covered; see the section below). Both of
/// #663's were worked through for a test here and dropped, for reasons
/// specific to each rather than a blanket "looks safe":
///
/// - **`row.set_title(&title)`.** Pre-#663 this was
///   `row.set_title(&sample_display_name(s, &mut meta_cache.borrow_mut()))`,
///   holding the `RefMut` across a real GTK setter. `set_title` on an
///   `AdwPreferencesRow` *does* emit `notify::title` synchronously, so that
///   would be a usable probe **if** the row were a widget a test could attach
///   a handler to beforehand. It isn't: the row is `build`-ed a few lines
///   earlier in the *same* call, never reused across calls (every pass tears
///   the previous rows down and builds fresh ones), and it is not parented to
///   `expander` until after `set_title` has already returned. There is no
///   point at which an external caller can hold that exact widget and arm a
///   probe on it. This is the identical structural wall `widgets/calendar.rs`
///   hit with `on_day_clicked`'s `flash_row_highlight` (see its own note);
///   reaching it would need `rebuild_top_apps` restructured so row
///   construction and row population are separately callable — a production
///   change this work deliberately does not make.
///
/// - **The icon fallback.** Pre-#663 this was
///   `resolve_app_meta(app_id, &mut meta_cache.borrow_mut()).and_then(…).unwrap_or_else(…)`,
///   holding the `RefMut` across `gio::ThemedIcon::new(…)`. That is a plain
///   `GObject` constructor: it emits no signal, invokes no callback, and
///   cannot re-enter anything. Reverting it is unobservable by construction,
///   so a test would be theatre — the same verdict #758 reached for the four
///   revealer `close_all`s.
///
/// Confirmed by measurement, not just by reading: with the `set_title` site
/// reverted to its pre-#663 inlined form, the test below still passes. That is
/// the point — the borrow *is* live across `set_title` (the samples this test
/// feeds have `app_id: None`, but `borrow_mut()` is taken either way), and
/// nothing re-enters it. Uncovered, not accidentally covered.
///
/// ## The third `meta_cache` site — #663 missed it, #832 fixed it, and it *is*
/// covered
///
/// The collapsed-summary expression in [`rebuild_top_apps`] used to be
///
/// ```text
/// format!("{} · {}", sample_display_name(s, &mut meta_cache.borrow_mut()), value(s))
/// ```
///
/// An argument-position `RefMut` is a temporary of the whole enclosing
/// expression, so it was **still alive when `value(s)` was called** — #643's
/// spelling 4, at a site #663 did not rewrite. #830 recorded it here rather
/// than fixing it, because that PR's whole argument rested on the extracted
/// body being byte-identical to the closure it came from; #832 fixed it, and
/// it is now `let`-bound like its two siblings.
///
/// Unlike those two siblings this one **is** falsifiable, and that is the
/// difference worth naming: the call-out is `value`, a *caller-supplied*
/// `fn(&ProcSample) -> String` parameter, so a test can pass its own. It
/// cannot capture (bare `fn` pointer, which is exactly why the production
/// instantiations are safe), so
/// [`top_apps_summary_releases_meta_cache_before_calling_value`] hands the
/// cell over through a thread-local and has the probe report
/// `try_borrow_mut().is_ok()`. Against the argument-position form the first
/// observation is `false`; against the `let`-bound form every observation is
/// `true`. The probe reports rather than panics on purpose — a real
/// `borrow_mut()` there would abort the test binary instead of failing one
/// test, which is the production failure mode but a poor assertion.
///
/// Needs a real display server (`adw::ExpanderRow`/`adw::ActionRow` have to be
/// constructible and actually run GTK's dispose machinery), hence the
/// `system-tests` gate, like the rest of this bug class.
#[cfg(all(test, feature = "system-tests"))]
mod reentrancy_tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use hytte::services::app_usage::ProcSample;

    use super::{AppMeta, rebuild_top_apps};

    type Rows = Rc<RefCell<Vec<adw::ActionRow>>>;
    type MetaCache = Rc<RefCell<HashMap<String, Option<AppMeta>>>>;

    thread_local! {
        /// The cell [`probing_value`] inspects, and what it saw on each call.
        /// A thread-local rather than a capture because `value` is a bare
        /// `fn` pointer — see [`probing_value`].
        static PROBE_CACHE: RefCell<Option<MetaCache>> = const { RefCell::new(None) };
        static PROBE_SAW: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    }

    /// A `value` formatter that reaches back into `meta_cache` — the thing
    /// #832 says the argument-position `RefMut` would have aborted on.
    ///
    /// `value` is `fn(&ProcSample) -> String`, so it cannot capture the cell;
    /// production is safe for exactly that reason. The cell is handed over
    /// through [`PROBE_CACHE`] instead. It *reports*
    /// `try_borrow_mut().is_ok()` rather than taking a real `borrow_mut()`:
    /// the latter is the production failure mode (a `BorrowMutError` panic
    /// unwinding through a glib callback aborts the process) but would take
    /// the whole test binary down instead of failing this one test.
    fn probing_value(s: &ProcSample) -> String {
        PROBE_CACHE.with_borrow(|slot| {
            if let Some(cache) = slot.as_ref() {
                let free = cache.try_borrow_mut().is_ok();
                PROBE_SAW.with_borrow_mut(|seen| seen.push(free));
            }
        });
        format!("{:.0}%", s.cpu_frac * 100.0)
    }

    /// The formatter `build_stats_cpu_card` passes for the "Top apps · CPU"
    /// expander, copied verbatim so the test drives a real production `value`
    /// rather than a stub. Must be a plain `fn` — the parameter is a bare
    /// function pointer, which is exactly why it can't smuggle test state in.
    fn cpu_value(s: &ProcSample) -> String {
        format!("{:.0}%", s.cpu_frac * 100.0)
    }

    /// `count` "System"-bucket samples (`app_id: None`).
    ///
    /// No app-id on purpose: an `app_id: Some(_)` sample sends
    /// `resolve_app_meta` through `gio::AppInfo::all()`, making the test's
    /// result depend on which desktop files the host happens to have
    /// installed. `sample_display_name`/`resolve_app_meta` still take
    /// `meta_cache.borrow_mut()` on the `None` path — the borrow is taken
    /// before the callee looks at `app_id` — so the cell's borrow discipline
    /// is still exercised, it just stays empty.
    ///
    /// The second sample carries `procs: 2`, which is the branch that also
    /// sets a row subtitle.
    fn samples(count: u32) -> Vec<ProcSample> {
        (1..=count)
            .map(|n| ProcSample {
                name: format!("proc{n:03}"),
                app_id: None,
                cpu_frac: f64::from(n) / 100.0,
                mem_bytes: u64::from(n) * 1024,
                procs: n,
            })
            .collect()
    }

    /// The expander and the two cells `build_top_apps_expander` builds,
    /// exactly as it builds them, plus the one-shot collapse guard. No
    /// registry, no `App`, no `/proc` poller: every piece of state
    /// `rebuild_top_apps` touches now arrives through its own parameters.
    fn fresh() -> (adw::ExpanderRow, Rows, MetaCache, Rc<Cell<bool>>) {
        adw::init().expect("libadwaita init");
        (
            adw::ExpanderRow::builder().title("Top apps").build(),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(RefCell::new(HashMap::new())),
            Rc::new(Cell::new(false)),
        )
    }

    /// `rows_track`: `expander.remove(&row)` drops the expander's reference
    /// and the loop-owned `row` binding drops its own at the end of that
    /// iteration — the last strong ref, so `GtkWidget::destroy` fires
    /// **synchronously** from dispose.
    ///
    /// The handler re-enters `rebuild_top_apps` on the same cells. Against the
    /// pre-#663 `for row in rows_track.borrow_mut().drain(..)` the inner call
    /// hits a live `RefMut` and aborts the whole test binary with
    /// `BorrowMutError` rather than failing one test — #663's SIGABRT, the
    /// failure mode #674 exists for. With `take()` the cell is free for the
    /// whole call, so the inner call finds an empty `Vec`.
    ///
    /// `rebuild_top_apps` never diffs by identity — every call unconditionally
    /// tears down whatever the last call left in `rows_track` and rebuilds
    /// fresh from `list` — so passing the *same* samples twice still exercises
    /// the full take/remove/rebuild path.
    #[gtk::test]
    fn rebuild_top_apps_tolerates_a_reentrant_rebuild_from_a_removed_row_destroy() {
        let (expander, rows, meta, collapsed) = fresh();
        let seed = samples(2);
        rebuild_top_apps(&expander, &rows, &meta, &collapsed, cpu_value, &seed);
        assert_eq!(
            rows.borrow().len(),
            2,
            "both samples must be rendered after the seeding call"
        );

        // True only while the outer `rebuild_top_apps` is on the stack, so the
        // handler can record whether it ran inside the call or was deferred.
        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let row2 = rows.borrow()[1].clone();
        {
            let expander = expander.clone();
            let rows = Rc::clone(&rows);
            let meta = Rc::clone(&meta);
            let collapsed = Rc::clone(&collapsed);
            let seed = seed.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            row2.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_top_apps(&expander, &rows, &meta, &collapsed, cpu_value, &seed);
            });
        }
        // Drop our clone before the removing pass: while it lives the row has
        // a second strong ref, `remove()` won't dispose it, and `destroy`
        // never fires — the test would pass vacuously.
        drop(row2);

        in_outer.set(true);
        rebuild_top_apps(&expander, &rows, &meta, &collapsed, cpu_value, &seed);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed row's `destroy` must fire synchronously inside `rebuild_top_apps`; \
             `adw::ExpanderRow` holds its rows in an internal list box, so if that keeps one \
             alive past the removal loop — or if GTK ever defers the emission — this test proves \
             nothing about the borrow discipline"
        );
        assert_eq!(
            rows.borrow().len(),
            2,
            "the outer call's write-back must still land: re-entry may not leave the cell holding \
             the inner call's rows or an empty Vec"
        );
    }

    /// #832: the collapsed-summary line must resolve the display name into a
    /// local *before* calling `value`, so `meta_cache` is no longer borrowed
    /// when the caller-supplied formatter runs.
    ///
    /// One sample means [`probing_value`] is called exactly twice — once for
    /// the collapsed summary, once for the row's suffix label — and both must
    /// find the cell free. Against the pre-#832 argument-position form the
    /// *first* observation is `false`, because an argument-position `RefMut`
    /// is a temporary of the whole enclosing `format!`. `saw.len() >= 2` is
    /// the anti-vacuity guard: a probe that never ran would otherwise satisfy
    /// `all()` trivially.
    #[gtk::test]
    fn top_apps_summary_releases_meta_cache_before_calling_value() {
        let (expander, rows, meta, collapsed) = fresh();
        PROBE_SAW.with_borrow_mut(Vec::clear);
        PROBE_CACHE.with_borrow_mut(|slot| *slot = Some(Rc::clone(&meta)));

        rebuild_top_apps(
            &expander,
            &rows,
            &meta,
            &collapsed,
            probing_value,
            &samples(1),
        );

        // Unhook before asserting: a later test in this module driving
        // `cpu_value` must not keep appending to `PROBE_SAW`.
        PROBE_CACHE.with_borrow_mut(|slot| *slot = None);
        let saw = PROBE_SAW.with_borrow(Clone::clone);

        assert!(
            saw.len() >= 2,
            "the probing `value` must have run for both the collapsed summary and the row label; \
             got {} observation(s), so this test proves nothing",
            saw.len()
        );
        assert!(
            saw.iter().all(|&free| free),
            "`meta_cache` must be free whenever `value` runs, but the observations were {saw:?}; \
             a `false` means the summary's `RefMut` was still live across `value(s)` (#832) — in \
             production that is a `BorrowMutError` through a glib callback, i.e. a process abort"
        );
    }
}

/// #831 regression coverage for this file's two widget-pinning `bind` call
/// sites, in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&W` `bind` hands it rather than a strong clone
/// captured from the enclosing scope, or the binding keeps the widget alive
/// for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
///
/// Only `build_top_apps_expander` is reachable from a test: it takes its
/// signal as a parameter, so a `Mutable` stands in for the service. The other
/// site in this file, `build_per_core_bars_row`, reads `sensors::cpu()`
/// inline and would need a registered `Registry` (or an extraction into a
/// `bind_*` helper, a production restructure this work does not make), so it
/// is fixed but uncovered — as are the nine sites in the other ten files.
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::app_usage::ProcSample;

    use super::build_top_apps_expander;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    fn cpu_value(s: &ProcSample) -> String {
        format!("{:.0}%", s.cpu_frac * 100.0)
    }

    /// Falsified by reintroducing the `expander_for_bind` strong clone the
    /// apply closure used to capture: with it, `drop(expander)` is not the
    /// last strong ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn top_apps_binding_does_not_pin_expander() {
        adw::init().expect("libadwaita init");
        let samples: Mutable<Vec<ProcSample>> = Mutable::new(Vec::new());
        let expander = build_top_apps_expander("Top apps", samples.signal_cloned(), cpu_value);
        let weak = expander.downgrade();
        pump();

        drop(expander);

        assert!(
            weak.upgrade().is_none(),
            "build_top_apps_expander must not pin its expander: a strong clone captured by the \
             apply closure (rather than taking the closure's own `&adw::ExpanderRow` argument \
             from `bind`) would keep this alive for the life of the binding, defeating #224's \
             WeakRef contract"
        );
    }
}

/// The per-core LED panel's **layout** contract (#857), which is pure GTK
/// geometry and therefore needs a display server.
///
/// [`build_per_core_leds_row`] itself binds `sensors::cpu()` inline and would
/// need a registered `Registry` to run (the same limitation `pin_tests`
/// documents), so these drive its two constructors — [`core_panel_row`] and
/// [`core_panel_surface`] — directly. That is the point of the split: the
/// arrangement under test is the arrangement that ships, not a copy of it that
/// can drift.
///
/// Two properties, both of which a plain unit test cannot reach because both
/// are answers GTK gives, not arithmetic this file does:
///
/// 1. the row's **minimum width is 0** with a 64-core panel in it — the #702
///    non-regression, measured rather than asserted in prose;
/// 2. an over-wide card cannot inflate the row's **height** request, which an
///    aspect-locked height-for-width raster widget absolutely can do if it is
///    allowed to fill.
#[cfg(all(test, feature = "system-tests"))]
mod led_panel_layout_tests {
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use hytte::ui::PixelSurface;

    use super::{CORE_PANEL_MAX_H, core_panel_row, core_panel_surface};

    /// The buffer dimensions a 64-core panel rasterises to under the #857
    /// rectangle default — a 16×4 grid.
    const PANEL_64: (u32, u32) = (181, 49);

    /// A surface holding the 181×49 buffer a 64-core rectangular panel
    /// rasterises to, arranged exactly as the shipping row arranges it.
    fn panel_64() -> PixelSurface {
        let (w, h) = PANEL_64;
        let len = usize::try_from(w * h * 4).expect("the 64-core buffer fits usize");
        let panel = core_panel_surface();
        panel.set_pixels(w, h, &vec![0xff_u8; len]);
        panel
    }

    /// **The #702 non-regression, measured.** The row holding a 64-core panel
    /// reports a minimum width of **0 px** — against the 764 px the pre-#702
    /// bar strip demanded — so no container is ever forced wider by it, while
    /// the row still *wants* the panel's natural 93 px.
    ///
    /// Falsified by putting a CSS `min-width` back on `.ts-cores-row`, or by
    /// giving the surface a `set_size_request`.
    #[gtk::test]
    fn the_panel_row_has_no_minimum_width() {
        adw::init().expect("libadwaita init");
        let row = core_panel_row();
        row.append(&panel_64());
        let (min_w, nat_w, _, _) = row.measure(gtk::Orientation::Horizontal, -1);
        assert_eq!(min_w, 0, "the LED panel row grew a minimum width");
        let want = i32::try_from(PANEL_64.0).expect("the buffer width fits i32");
        assert!(
            nat_w >= want,
            "the row should still want the panel's {want} px, got {nat_w}"
        );
    }

    /// A card far wider than the panel must not make the row taller: a filling
    /// surface would request `400 * 49 / 181` px of height and swallow the
    /// CPU card.
    ///
    /// Falsified by putting `set_hexpand(true)` back on the surface — which is
    /// how this was first written, and measured 400 px.
    #[gtk::test]
    fn a_wide_card_cannot_inflate_the_row_height() {
        adw::init().expect("libadwaita init");
        let budget = i32::try_from(CORE_PANEL_MAX_H).expect("the budget fits i32");
        let row = core_panel_row();
        row.append(&panel_64());
        let (_, nat_h, _, _) = row.measure(gtk::Orientation::Vertical, 400);
        assert!(
            nat_h <= budget,
            "a 400 px card stretched the 181x49 panel to {nat_h} px tall"
        );
    }
}

/// The multicolumn grid's arrangement (#857): the two-column flow that replaced
/// #702's spanning CPU card, and the #571 GPU-hidden reflow that has to keep
/// working underneath it.
///
/// Gated on `system-tests` because `gtk::Grid`'s layout children only exist
/// once GTK is initialised. Drives [`attach_stats_cards`] and
/// [`install_gpu_hidden_reflow`] — the real production functions — over five
/// bare `gtk::Label`s, so the shipping coordinates are what gets asserted
/// rather than a second copy of them that can drift.
#[cfg(all(test, feature = "system-tests"))]
mod stats_grid_tests {
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    use super::{StatsSection, attach_stats_cards, install_gpu_hidden_reflow};

    /// A grid wired by the shipping attach function, plus the five widgets in
    /// [`StatsSection`] declaration order.
    fn stats_grid() -> (gtk::Grid, [gtk::Widget; 5]) {
        let grid = gtk::Grid::new();
        grid.set_column_homogeneous(true);
        let cards: [gtk::Widget; 5] =
            std::array::from_fn(|i| gtk::Label::new(Some(&i.to_string())).upcast());
        let [cpu, memory, gpu, disks, services] = &cards;
        attach_stats_cards(&grid, cpu, memory, gpu, disks, services);
        (grid, cards)
    }

    /// `(column, row, column_span, row_span)` of `w` in `grid`.
    fn cell(grid: &gtk::Grid, w: &gtk::Widget) -> (i32, i32, i32, i32) {
        let lc = grid
            .layout_manager()
            .expect("gtk::Grid always installs a GtkGridLayout")
            .layout_child(w)
            .downcast::<gtk::GridLayoutChild>()
            .expect("a GtkGrid's layout children are GtkGridLayoutChild");
        (lc.column(), lc.row(), lc.column_span(), lc.row_span())
    }

    /// The columns occupied in `row` by the currently-visible cards.
    fn occupied(grid: &gtk::Grid, cards: &[gtk::Widget; 5], row: i32) -> Vec<i32> {
        let mut cols: Vec<i32> = cards
            .iter()
            .filter(|w| w.is_visible())
            .map(|w| cell(grid, w))
            .filter(|(_, r, _, _)| *r == row)
            .flat_map(|(c, _, span, _)| c..c + span)
            .collect();
        cols.sort_unstable();
        cols
    }

    /// **The #857 ask, pinned.** CPU no longer spans both columns, and the grid
    /// reads left-to-right then down in #582's bar-chip order — CPU, Memory,
    /// GPU, Disks, Services — which is the order the bar's own resource chips
    /// render in and the order `StatsSection` declares.
    ///
    /// Falsified by any one of the five `grid.attach` lines: restoring #702's
    /// `(0, 0, 2, 1)` CPU span breaks the span assertion *and* reshuffles the
    /// reading order to CPU, GPU, Disks, Memory, Services.
    #[gtk::test]
    fn the_grid_is_a_two_column_flow_in_bar_chip_order() {
        adw::init().expect("libadwaita init");
        let (grid, cards) = stats_grid();
        let [cpu, memory, gpu, disks, services] = &cards;

        assert_eq!(cell(&grid, cpu), (0, 0, 1, 1), "CPU must not span (#857)");
        assert_eq!(cell(&grid, memory), (1, 0, 1, 1));
        assert_eq!(cell(&grid, gpu), (0, 1, 1, 1));
        assert_eq!(cell(&grid, disks), (1, 1, 1, 1));
        assert_eq!(
            cell(&grid, services),
            (0, 2, 2, 1),
            "Services keeps the last row to itself (#582)"
        );

        // Reading order is row-major over the cells, and must equal the
        // `StatsSection` declaration order that
        // `stats_section_declaration_order_matches_bar` pins to the bar.
        let sections = [
            StatsSection::Cpu,
            StatsSection::Memory,
            StatsSection::Gpu,
            StatsSection::Disks,
            StatsSection::Services,
        ];
        let mut by_position: Vec<(i32, i32, StatsSection)> = cards
            .iter()
            .zip(sections)
            .map(|(w, s)| {
                let (col, row, _, _) = cell(&grid, w);
                (row, col, s)
            })
            .collect();
        by_position.sort_by_key(|(row, col, _)| (*row, *col));
        let reading_order: Vec<StatsSection> = by_position.into_iter().map(|(_, _, s)| s).collect();
        assert_eq!(
            reading_order,
            sections.to_vec(),
            "the grid must read in bar-chip order"
        );
    }

    /// **The #571 reflow, still intact under #857's arrangement.** With every
    /// card shown, both columns of every row are occupied. When the GPU card
    /// self-hides, Disks moves into the column GPU vacated rather than being
    /// stranded in column 1 with a hole to its left.
    ///
    /// The invariant is stated as *"every row's occupied columns form a
    /// prefix"* rather than as Disks' column number, because that is the thing
    /// the reflow exists to protect: a gap is only a hole when something sits
    /// to the right of it. A trailing empty cell (row 1's column 1, once Disks
    /// has moved left) is not a hole.
    ///
    /// Falsified by deleting either the immediate `reflow_disks_column(...)`
    /// call in [`install_gpu_hidden_reflow`] or its `connect_visible_notify`.
    #[gtk::test]
    fn hiding_the_gpu_card_leaves_no_hole() {
        adw::init().expect("libadwaita init");
        let (grid, cards) = stats_grid();
        let [_, _, gpu, disks, _] = &cards;
        install_gpu_hidden_reflow(&grid, gpu, disks);

        assert_eq!(occupied(&grid, &cards, 0), vec![0, 1], "row 0 full");
        assert_eq!(occupied(&grid, &cards, 1), vec![0, 1], "row 1 full");
        assert_eq!(occupied(&grid, &cards, 2), vec![0, 1], "Services spans");

        gpu.set_visible(false);
        assert_eq!(
            cell(&grid, disks).0,
            0,
            "Disks must reflow into the column GPU vacated"
        );
        for row in 0..=2 {
            let cols = occupied(&grid, &cards, row);
            let prefix: Vec<i32> =
                (0..i32::try_from(cols.len()).expect("at most two columns")).collect();
            assert_eq!(
                cols, prefix,
                "row {row} has a hole with the GPU card hidden"
            );
        }
        // Column 0 stays occupied in the rows either side of the reflowed
        // Disks, which is what keeps the column-homogeneous grid's column 0
        // width meaningful.
        assert!(occupied(&grid, &cards, 0).contains(&0));
        assert!(occupied(&grid, &cards, 2).contains(&0));

        // And it reverses when a GPU appears.
        gpu.set_visible(true);
        assert_eq!(cell(&grid, disks).0, 1, "Disks must return to column 1");
        assert_eq!(occupied(&grid, &cards, 1), vec![0, 1]);
    }
}
