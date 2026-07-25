//! System stats drawer panel — the combined view (CPU + Memory + Disks +
//! GPU + Services cards, stacked in one page) restored in #508 after #307
//! split it into five single-card pages. Every resource bar chip (and the
//! self-hiding failed-units services chip) opens this same page; the
//! per-resource cards are the `build_stats_*` builders below, appended in
//! order by [`panel_stats`].
//!
//! #516 adds scroll-to-section precision on top of the #508 restore, without
//! re-splitting the page: each resource chip stashes a [`StatsSection`] via
//! [`set_scroll_target`] just before it opens/re-shows the drawer, and the
//! combined page — wrapped in a `ScrolledWindow` here so five stacked cards
//! don't run past screen height — scrolls its own target card to the top.
//! See [`StatsSection`] for how `crate::modal` triggers the (re)scroll.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, gio, glib};
use hytte::prelude::*;
use hytte::services::app_usage::{self, ProcSample};
use hytte::services::sensors::{self, CpuFreq, CpuLoad};
use hytte::services::systemd;
use hytte::ui::MultiSparkline;

use crate::components::cast;
use crate::components::format::{fmt_bytes, fmt_hz, fmt_rate};
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box};
use crate::components::monitor_key::monitor_key;
use crate::components::reactive_list::reactive_list;

/// One card in the combined Stats page (#516). Named after the resource chip
/// that scrolls to it, in the same top-to-bottom order [`panel_stats`] stacks
/// the cards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StatsSection {
    Cpu,
    Memory,
    Disks,
    Gpu,
    Services,
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
/// GPU / services bar chips. All five cards stack in one `ts-popup-column`,
/// one click, scroll to see all (pre-#307 shape).
///
/// Wrapped in a `gtk::ScrolledWindow` (mirroring `connections.rs`/`wifi.rs`'s
/// #84 pattern) so five stacked cards can't push the drawer past screen
/// height. A `"stats"`-prefixed `gio::SimpleActionGroup` carrying one
/// `"scroll"` action is inserted on the returned page widget: activating it
/// re-applies the [`StatsSection`] pending in [`PENDING_SCROLL`] for the monitor
/// named by the action's string parameter (#542). Routing the trigger through a
/// widget-local action (rather than a cross-module registry keyed by monitor) is
/// what lets `crate::modal` poke *this specific monitor's* built page instance
/// armed with only the `gtk::Widget` it already gets back from
/// `gtk::Stack::child_by_name` — this page never needs to know which monitor
/// it's on, only which key it was handed at activation.
pub fn panel_stats() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let cpu_card = build_stats_cpu_card();
    let memory_card = build_stats_memory_card();
    let disks_card = build_stats_disks_card();
    let gpu_card = build_stats_gpu_card();
    let services_card = build_stats_services_group();

    column.append(cpu_card.upcast_ref::<gtk::Widget>());
    column.append(memory_card.upcast_ref::<gtk::Widget>());
    column.append(disks_card.upcast_ref::<gtk::Widget>());
    column.append(gpu_card.upcast_ref::<gtk::Widget>());
    column.append(services_card.upcast_ref::<gtk::Widget>());

    // Soft default (flagged for review, same spirit as the disk-I/O row's
    // own "soft defaults" caveat further down this file): 560 leaves a
    // couple of cards visible on a typical laptop panel while still
    // requiring a scroll to reach the rest, the same trade-off
    // connections.rs's 480 and wifi.rs's 240 caps make.
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    scrolled.set_max_content_height(crate::scale::scale(560));
    scrolled.set_child(Some(&column));

    let sections: Vec<(StatsSection, gtk::Widget)> = vec![
        (StatsSection::Cpu, cpu_card.upcast()),
        (StatsSection::Memory, memory_card.upcast()),
        (StatsSection::Disks, disks_card.upcast()),
        (StatsSection::Gpu, gpu_card.upcast()),
        (StatsSection::Services, services_card.upcast()),
    ];
    let scrolled_for_action = scrolled.clone();
    let column_for_action = column.clone();
    let action_group = gio::SimpleActionGroup::new();
    // The activation carries the target monitor's key as its string parameter
    // (#542): `PENDING_SCROLL` is keyed per monitor, and `crate::modal` hands in
    // the key of whichever monitor's drawer is (re)showing this page.
    let scroll_action = gio::SimpleAction::new("scroll", Some(glib::VariantTy::STRING));
    scroll_action.connect_activate(move |_, param| {
        let Some(key) = param.and_then(glib::Variant::str) else {
            return;
        };
        apply_scroll(&scrolled_for_action, &column_for_action, &sections, key);
    });
    action_group.add_action(&scroll_action);

    let page = finish_page(&scrolled);
    page.insert_action_group("stats", Some(&action_group));
    page
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
    column: &gtk::Box,
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
    scroll_card_to_top_when_ready(scrolled.clone(), column.clone(), card.clone(), 1);
}

/// Deferred scroll worker for [`apply_scroll`]: on the next main-loop idle, land
/// `card`'s top edge at the top of `scrolled`'s viewport. If `card`'s bounds
/// don't resolve yet (`compute_bounds` → `None`, an unsettled allocation),
/// re-arm up to `retries` more idles before giving up (#542).
fn scroll_card_to_top_when_ready(
    scrolled: gtk::ScrolledWindow,
    column: gtk::Box,
    card: gtk::Widget,
    retries: u32,
) {
    glib::idle_add_local_once(move || {
        let Some(bounds) = card.compute_bounds(&column) else {
            if retries > 0 {
                scroll_card_to_top_when_ready(scrolled, column, card, retries - 1);
            }
            return;
        };
        let vadj = scrolled.vadjustment();
        let lower = vadj.lower();
        let upper = (vadj.upper() - vadj.page_size()).max(lower);
        vadj.set_value(f64::from(bounds.y()).clamp(lower, upper));
    });
}

/// Wrap a bare history-sparkline `gtk::Box` in a `gtk::ListBoxRow` so it joins
/// an `AdwPreferencesGroup`'s boxed-list in source order with the standard
/// separators. A non-`GtkListBoxRow` child added to a group otherwise renders
/// *below* the boxed-list and out of order (cf. the adw-routing gotcha; the
/// same fix PR #149 used for the per-interface network rows).
fn history_row_wrapper(child: &gtk::Box) -> gtk::ListBoxRow {
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
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    let collapsed_once_for_bind = collapsed_once.clone();
    bind(signal, &expander, move |_, list| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            expander_for_bind.remove(&row);
        }
        // Collapsed summary: the heaviest entry's display name, or an em-dash.
        let subtitle = list.first().map_or_else(
            || "\u{2014}".to_string(),
            |s| {
                format!(
                    "{} \u{00b7} {}",
                    sample_display_name(s, &mut meta_cache.borrow_mut()),
                    value(s)
                )
            },
        );
        expander_for_bind.set_subtitle(&subtitle);

        let mut new_rows = Vec::with_capacity(list.len());
        for s in &list {
            let row = adw::ActionRow::builder().activatable(false).build();
            // Markup off: scope ids are untrusted — adversarial names could
            // otherwise inject Pango markup into the title (cf. #30).
            row.set_use_markup(false);
            row.set_title(&sample_display_name(s, &mut meta_cache.borrow_mut()));
            if s.procs > 1 {
                row.set_subtitle(&format!("{} processes", s.procs));
            }

            // Prefix icon: cached from the app-id, or sensible fallbacks.
            let icon: gio::Icon = if let Some(app_id) = s.app_id.as_deref() {
                resolve_app_meta(app_id, &mut meta_cache.borrow_mut())
                    .and_then(|m| m.icon)
                    .unwrap_or_else(|| {
                        gio::ThemedIcon::new("application-x-executable-symbolic")
                            .upcast::<gio::Icon>()
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
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;

        // Re-assert collapsed state once on first non-empty population.
        // libadwaita can render the expander open when rows arrive after the
        // initial set_expanded(false) on an empty widget (async row-population
        // race — #131).  We fire exactly once so the user's subsequent
        // manual expand/collapse is never overridden.
        if !collapsed_once_for_bind.get() && !list.is_empty() {
            expander_for_bind.set_expanded(false);
            collapsed_once_for_bind.set(true);
        }
    });

    expander
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

fn build_live_per_core_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder().title("Per-core").build();
    row.set_activatable(false);
    row.set_selectable(false);
    bind(
        sensors::cpu().map(|c| format!("{} cores", c.per_core.len())),
        &row,
        |r, t| r.set_subtitle(&t),
    );

    let cores_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    cores_row.add_css_class("ts-cores-row");
    cores_row.set_margin_top(4);
    cores_row.set_margin_bottom(4);
    cores_row.set_hexpand(true);
    cores_row.set_valign(gtk::Align::Center);

    let core_bars: Rc<RefCell<Vec<gtk::ProgressBar>>> = Rc::new(RefCell::new(Vec::new()));
    let cores_row_for_bind = cores_row.clone();
    let bars_for_bind = core_bars.clone();
    bind(sensors::cpu(), &cores_row, move |_, c: CpuLoad| {
        let mut bars = bars_for_bind.borrow_mut();
        if bars.len() != c.per_core.len() {
            while let Some(child) = cores_row_for_bind.first_child() {
                cores_row_for_bind.remove(&child);
            }
            bars.clear();
            for _ in 0..c.per_core.len() {
                let col = gtk::Box::new(gtk::Orientation::Vertical, 0);
                col.set_hexpand(true);
                col.set_halign(gtk::Align::Center);
                let bar = gtk::ProgressBar::new();
                bar.add_css_class("ts-core-bar");
                bar.set_orientation(gtk::Orientation::Vertical);
                bar.set_inverted(true);
                bar.set_valign(gtk::Align::End);
                col.append(&bar);
                cores_row_for_bind.append(&col);
                bars.push(bar);
            }
        }
        for (bar, load) in bars.iter().zip(c.per_core.iter()) {
            bar.set_fraction(load.clamp(0.0, 1.0));
            bar.set_tooltip_text(Some(&format!("{:.0}%", load * 100.0)));
        }
    });

    row.add_suffix(&cores_row);
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

/// Services group — flattened per #311: no group description and no
/// `Failed units` expander wrapper (both duplicated the count already shown
/// on the bar chip, and the expander hid the one thing this flyout is opened
/// to see). The failed-unit `ActionRow`s render straight into the group, so
/// the flyout *is* the list, matching the other stats panels' pattern of
/// showing their primary content directly rather than behind a titled row.
/// If every unit recovers while the panel is open, the list just goes empty
/// (the chip that opens this panel self-hides at zero failed units anyway).
fn build_stats_services_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();

    reactive_list(
        &group,
        systemd::failed_units(),
        |unit: &systemd::FailedUnit| {
            let row = adw::ActionRow::builder()
                .title(&unit.name)
                .activatable(false)
                .build();
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
