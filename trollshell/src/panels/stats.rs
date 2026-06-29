//! System stats drawer panel — one card per monitored resource
//! (CPU / Memory / Disks / GPU / Services). Each card groups that
//! resource's live rows, history sparkline, and top-consumers list.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, gio};
use hytte::prelude::*;
use hytte::services::app_usage::{self, ProcSample};
use hytte::services::sensors::{self, CpuLoad};
use hytte::services::systemd;
use hytte::ui::MultiSparkline;

use crate::components::cast;
use crate::components::format::fmt_bytes;
use crate::components::history_row::build_history_row;
use crate::components::layout::{finish_page, page_box};
use crate::components::reactive_list::reactive_list;

pub fn panel_stats() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    column.append(build_stats_cpu_card().upcast_ref::<gtk::Widget>());
    column.append(build_stats_memory_card().upcast_ref::<gtk::Widget>());
    column.append(build_stats_disks_card().upcast_ref::<gtk::Widget>());
    column.append(build_stats_gpu_card().upcast_ref::<gtk::Widget>());
    column.append(build_stats_services_group().upcast_ref::<gtk::Widget>());

    finish_page(&column)
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
    let group = adw::PreferencesGroup::builder().title("CPU").build();

    group.add(&build_live_cpu_row());
    group.add(&build_live_per_core_row());
    group.add(&build_live_processes_row());
    group.add(&build_expandable_cpu_history_row());
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
    let group = adw::PreferencesGroup::builder().title("Memory").build();

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

/// Disks card — the per-mount disk expander.
fn build_stats_disks_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Disks").build();
    group.add(&build_live_disk_expander());
    group
}

/// GPU card — live GPU row + usage / VRAM / temp history sparklines. The whole
/// card hides when no GPU is detected (bound to the same `sensors::gpu()`
/// presence signal the live GPU row uses to self-hide); each history row
/// additionally self-hides if its specific metric (load / VRAM / temperature)
/// isn't reported. Intel GPUs are supported as of #150, so this card shows on
/// Arc/iGPU hardware.
fn build_stats_gpu_card() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("GPU").build();

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

fn build_history_cpu_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("CPU");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::cpu(), &row, move |_, c: CpuLoad| {
        spark_clone.push(c.overall);
        value_clone.set_text(&format!("{:.0}%", c.overall * 100.0));
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

    let percore_graph_c = percore_graph.clone();
    let percore_value_c = percore_value.clone();
    bind(sensors::cpu(), &percore_box, move |_, c: CpuLoad| {
        percore_graph_c.push_frame(&c.per_core);
        percore_value_c.set_text(&format!(
            "{} cores \u{00b7} {:.0}%",
            c.per_core.len(),
            c.overall * 100.0
        ));
    });

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

fn build_history_memory_row() -> gtk::Box {
    let (row, spark, value) = build_history_row("Memory");
    spark.set_domain_max(Some(1.0));

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::memory(), &row, move |_, m| {
        if m.total == 0 {
            spark_clone.push(0.0);
            value_clone.set_text("\u{2014}");
        } else {
            let frac = (cast::u64_to_f64(m.used) / cast::u64_to_f64(m.total)).clamp(0.0, 1.0);
            spark_clone.push(frac);
            value_clone.set_text(&format!("{:.0}%", frac * 100.0));
        }
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

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some(l) = state.load
        {
            let pct = l * 100.0;
            spark_clone.push(pct);
            value_clone.set_text(&format!("{pct:.0}%"));
        }
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

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some((used, total)) = state.memory_used_bytes.zip(state.memory_total_bytes)
            && total > 0
        {
            let pct = (cast::u64_to_f64(used) / cast::u64_to_f64(total) * 100.0).clamp(0.0, 100.0);
            spark_clone.push(pct);
            value_clone.set_text(&format!("{pct:.0}%"));
        }
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

    let spark_clone = spark.clone();
    let value_clone = value.clone();
    bind(sensors::gpu(), &row, move |_, g| {
        if let Some(state) = g
            && let Some(t) = state.temperature_celsius
        {
            spark_clone.push(t);
            value_clone.set_text(&format!("{t:.0} \u{00b0}C"));
        }
    });

    row
}

fn build_stats_services_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Services").build();

    bind(
        systemd::failed_units().map(|units| {
            if units.is_empty() {
                "All services running".to_string()
            } else {
                format!("{} failed unit(s)", units.len())
            }
        }),
        &group,
        |g, txt| g.set_description(Some(&txt)),
    );

    group.add(&build_failed_units_expander());
    group
}

fn build_failed_units_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Failed units").build();
    bind(
        systemd::failed_units().map(|u| !u.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );
    bind(
        systemd::failed_units().map(|u| {
            if u.is_empty() {
                "None".to_string()
            } else {
                format!("{} unit(s)", u.len())
            }
        }),
        &expander,
        |r, t| r.set_subtitle(&t),
    );

    reactive_list(
        &expander,
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

    expander
}
