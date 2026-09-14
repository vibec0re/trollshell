//! The plugin's **own drawer page** (#1251) — the native Stats page's cards
//! re-expressed in the wire vocabulary.
//!
//! A click on any bar chip emits
//! [`Effect::OpenPage(Page::PluginSelf)`](hytte_plugin::proto::Page::PluginSelf)
//! and the host resolves that to this tree, keyed by the effect's plugin id.
//! It rides the same render frame as the chips (`View::panel`), so the page and
//! the chip a click came from are always one observation of the machine.
//!
//! # What it mirrors, and what it cannot
//!
//! The native page (`trollshell/src/panels/stats.rs`, 3 165 lines over five
//! cards in three layouts) is the source of truth for the card set, every
//! format string and every hide rule. Four of its five cards are here — CPU,
//! Memory, GPU, Disks — and the fifth, **Services**, is not, for the reason
//! `crate::card::chips` gives about the services chip: failed systemd units are
//! a system-bus client and flapping shell tasks are the shell's own task
//! supervisor, and a plugin process can reach neither. Epic #1248's P3 already
//! has that card staying native.
//!
//! **Two rows genuinely cannot follow, and everything else does.** The
//! `#1295` review's MED 1 found five native rows missing and named nowhere:
//! four had no dependency or wire reason to be absent (a gap in this page, not
//! in what a GTK-free process can reach) and are here now — **Processes**
//! (`stats.rs:576` → `:1182`), **CPU clock** (`:578` → `:1557`), **disk I/O
//! history** (`:610` → `:1293`) and **GPU VRAM history** (`:632`), all sourced
//! from `hytte-sensors` the same way every other row on this page is. The
//! fifth, **Top apps · CPU / RAM** (`:579`, `:596`), is the one genuine gap:
//! `app_usage` (`crates/hytte-services/src/app_usage.rs`) walks `/proc`
//! grouped by systemd app-scope/service cgroup and resolves each app's icon
//! through `gio::AppInfo` — a `hytte-services` module, which a GTK-free plugin
//! process never links, the same shape of gap as the Services card. Not built
//! here, and not planned — see `docs/live-verify.md`'s side-by-side item and
//! #1251.
//!
//! The translation, row by row:
//!
//! | native | here |
//! | --- | --- |
//! | `MultiSparkline` per-core history | **not drawn** — the wire's `Scope` is one trace, and N traces is N nodes; the overall sweep is the one that fits |
//! | `Sparkline` overall CPU history | a [`Scope`](hytte_plugin::display::Scope) |
//! | the `LedMatrix` per-core panel | the P1 lamp row, fitted to the page's own width (`card::PAGE_PX`) rather than the sidebar card's |
//! | "Processes" row | a plain reading, `hytte_sensors::read_process_count` |
//! | "Clock" row (`fmt_hz`) | a plain reading, `hytte_sensors::read_cpu_freq`; hidden with no `cpufreq` governor, same as native |
//! | memory / swap `GtkProgressBar` | a [`LedStrip`](hytte_plugin::display::LedStrip) apiece, with the exact `used / total (pct%)` text beside it |
//! | GPU load (a text suffix natively) | a [`Gauge`](hytte_plugin::display::Gauge) — the one place this page is *more* than the native one, and P1's own choice |
//! | "GPU VRAM" history row | a [`Scope`]; hidden unless both used+total VRAM are reported, same as native |
//! | per-mount `GtkProgressBar` | [`Node::Progress`] with the shell's own `ts-stat-progress` class |
//! | "Disk I/O" history row | a [`Scope`], auto-scaled against the peak rate this session has seen rather than the native row's windowed max (a named simplification, `card::Widgets::disk_io`'s doc) |
//! | Top apps · CPU / RAM | **not drawn** — shell-only, see above |
//!
//! **Why the mounts are `Node::Progress` and not a strip each.** #1251 asks for
//! "progress bars become `Gauge`/`LedStrip`", and the memory and swap bars are
//! exactly that. The mount list is the one place that would be wrong: the
//! preem wrappers hold animation state, a node's id is its reconciler key
//! (#900), and the mount list is **variable-length** — so N strips means N
//! wrappers in the model, rebuilt whenever a filesystem is mounted, which is
//! the shape #900's documentation names as the thing that goes wrong. A
//! `Node::Progress` is stateless, carries the native page's own
//! `.ts-stat-progress` rule verbatim, and is what the native page draws there.
//!
//! # Three layouts, one page
//!
//! The native page has `combined` / `multicolumn` / `split`
//! (`TROLLSHELL_STATS_LAYOUT`), because it is the whole Stats surface and has a
//! grid to lay out. This is one column: the wire has no grid, the drawer is
//! 680 px wide, and `split` exists so a chip can open *its own* page — which
//! here is the same page for all four chips, since a plugin has exactly one
//! `PluginSelf`. Nothing about that is configurable and nothing pretends to be.

use hytte_plugin::nodes;
use hytte_plugin::proto::Node;

use crate::card::{self, Widgets, cls, header, label, percent_text};
use crate::format;
use crate::sample::{Disk, Snapshot};

/// The page's root node id.
pub const ROOT_ID: &str = "stats-panel";

/// Project a snapshot into the drawer page.
///
/// Pure, like [`card::card`] and [`card::chips`]: every branch is decided by
/// `cfg` and by what the snapshot holds, so the whole page is testable from a
/// `Snapshot` literal.
#[must_use]
pub fn panel(cfg: crate::config::Card, snapshot: &Snapshot, widgets: &Widgets) -> Node {
    let mut cards = Vec::new();

    if cfg.cpu {
        cards.push(boxed("stats-panel-cpu", cpu_rows(cfg, snapshot, widgets)));
    }
    if cfg.memory {
        cards.push(boxed(
            "stats-panel-memory",
            card::memory_rows(snapshot, widgets, "stats-panel"),
        ));
    }
    // The GPU card hides itself entirely when there is nothing to read — the
    // native page's `bind(sensors::gpu().map(|g| g.is_some()), &group, …)`,
    // which hides the whole `PreferencesGroup` rather than parking a gauge at
    // zero.
    if cfg.gpu && snapshot.gpu.is_some() {
        cards.push(boxed("stats-panel-gpu", gpu_rows(cfg, snapshot, widgets)));
    }
    if cfg.disk {
        cards.push(boxed("stats-panel-disks", disk_rows(snapshot, widgets)));
    }

    Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: hytte_plugin::proto::Dir::Vertical,
        spacing: 12,
        scroll: false,
        // No `.card` and no `.ts-plugin-card`: the drawer supplies the page
        // chrome (`.ts-plugin-panel` > `.ts-plugin-canvas`), and a panel root
        // that adds its own reads as a card in a card — the SDK's *Styling*
        // docs, same rule the sidebar card follows.
        classes: Vec::new(),
        children: cards,
        tooltip: None,
    }
}

/// One card: a `boxed-list` around its rows, which is the vocabulary's nearest
/// thing to the `adw::PreferencesGroup` every native card is.
///
/// `dense` because these rows are one line of text each and libadwaita's own
/// row min-height is most of the height a dense list would otherwise take
/// (#966) — the page is a monitor, not a settings list.
fn boxed(id: &str, rows: Vec<Node>) -> Node {
    nodes::list(rows)
        .id(id)
        .class("boxed-list")
        .dense(true)
        .build()
}

/// The CPU card: load, the process count, the core count, the lamp row at the
/// page's own pitch, the package temperature, the history sweep and the
/// clock.
fn cpu_rows(cfg: crate::config::Card, snapshot: &Snapshot, widgets: &Widgets) -> Vec<Node> {
    let mut rows = vec![header("CPU", percent_text(snapshot.cpu), "ts-cpu", None)];

    // Processes — native `stats.rs:576` → `:1182`, shown unconditionally like
    // the headline: it carries no cpufreq/hwmon gate of its own, and the `—`
    // fallback is the seed-render state before the first tick (#1295 review
    // MED 1).
    rows.push(header(
        "Processes",
        snapshot
            .processes
            .map_or_else(|| "—".to_owned(), |n| n.to_string()),
        "ts-cpu",
        None,
    ));

    if cfg.per_core {
        // `{} cores` verbatim from the native per-core header row, including
        // its ungrammatical `1 cores` — the native page's own tests pin that
        // spelling, and reading differently from the thing beside it would be
        // worse than reading badly.
        rows.push(header(
            "Per-core",
            format!("{} cores", snapshot.per_core.len()),
            "ts-cpu",
            None,
        ));
        rows.extend(widgets.page_core_nodes(&snapshot.per_core));
    }

    if cfg.temperature {
        rows.push(Node::Row {
            id: Some("stats-panel-cpu-temp-row".to_owned()),
            classes: Vec::new(),
            spacing: 4,
            children: vec![
                widgets.temp_node(
                    "stats-panel-cpu-temp",
                    cls("ts-cpu-temp"),
                    snapshot.cpu_temp_c,
                ),
                label("°C", &["dim-label", "ts-cpu-temp"]),
                Node::Spacer,
            ],
            tooltip: None,
        });
    }

    if cfg.history {
        rows.push(widgets.history_node("stats-panel-cpu-history", cls("ts-cpu")));
    }

    // Clock — native `stats.rs:578` → `:1557`'s collapsed reading
    // (`fmt_hz(f.max_hz)`), hidden with no `cpufreq` governor exactly like the
    // native row (#1295 review MED 1). `snapshot.cpu_clock_hz` is already
    // `None` in that case — see `Sampler::tick`.
    if let Some(clock_hz) = snapshot.cpu_clock_hz {
        rows.push(header("Clock", format::hz(clock_hz), "ts-cpu", None));
    }

    rows
}

/// The GPU card: the adapter's own name, its load, its temperature and the
/// needle.
///
/// Only reached with `snapshot.gpu.is_some()`, so the `else` arms below are the
/// *within*-adapter absences (a vendor with no busy counter, no thermal probe),
/// which the native page renders as a hidden row rather than a dash.
fn gpu_rows(cfg: crate::config::Card, snapshot: &Snapshot, widgets: &Widgets) -> Vec<Node> {
    let Some(gpu) = snapshot.gpu.as_ref() else {
        return Vec::new();
    };
    let mut rows = vec![
        header("GPU", gpu.name.clone(), "ts-gpu", None),
        header("Load", percent_text(gpu.load), "ts-gpu", None),
    ];
    // `{t:.0} °C` — the native GPU row's suffix, which unlike the bar chip's
    // `{c:.0}°` carries the unit letter and a space.
    if cfg.temperature
        && let Some(c) = gpu.temperature_c.filter(|c| c.is_finite())
    {
        rows.push(header(
            "Temperature",
            format!("{c:.0} °C"),
            "ts-gpu-temp",
            None,
        ));
    }
    rows.push(widgets.gpu_node("stats-panel-gpu-load", cls("ts-gpu")));

    // GPU VRAM history — native `stats.rs:632`, hidden unless both used and
    // total VRAM are reported (some vendors expose load/temperature but not
    // memory), the native row's own hide rule (#1295 review MED 1).
    if let Some((used, total)) = gpu
        .memory_used_bytes
        .zip(gpu.memory_total_bytes)
        .filter(|(_, total)| *total > 0)
    {
        let pct = format::fraction(used, total) * 100.0;
        rows.push(header("VRAM", format!("{pct:.0}%"), "ts-gpu", None));
        rows.push(widgets.gpu_vram_node("stats-panel-gpu-vram", cls("ts-gpu")));
    }

    rows
}

/// The Disks card: the `N mount(s)` summary and lamp row (at the page's own
/// pitch, #1295 review LOW 4), one row per mount carrying the exact numbers,
/// then the disk I/O history sweep.
fn disk_rows(snapshot: &Snapshot, widgets: &Widgets) -> Vec<Node> {
    let mut rows = card::page_disk_rows(snapshot, widgets, "stats-panel");
    if snapshot.disks.is_empty() {
        // The native expander renders nothing at all for an empty list, which
        // on a page that is otherwise all rows reads as a broken card; one row
        // saying so is the honest version.
        rows.push(header("No mounts", String::new(), "ts-disk", None));
    } else {
        for (i, disk) in snapshot.disks.iter().enumerate() {
            rows.push(mount_row(i, disk));
        }
    }

    // Disk I/O history — native `stats.rs:610` → `:1293`'s combined rate row.
    // Unconditional like the native one (no vendor/governor hide rule; an
    // idle machine just draws a flat trace at the bottom rail) — #1295 review
    // MED 1.
    if let Some(io) = snapshot.disk_io.as_ref() {
        rows.push(header(
            "Disk I/O",
            format!(
                "\u{2193} {} \u{2191} {}",
                format::rate(io.read_bps),
                format::rate(io.write_bps),
            ),
            "ts-disk",
            Some(format!(
                "total \u{2193} {} \u{2191} {}",
                format::bytes(io.total_read_bytes),
                format::bytes(io.total_write_bytes),
            )),
        ));
        rows.push(widgets.disk_io_node("stats-panel-disk-io", cls("ts-disk")));
    }

    rows
}

/// One mounted filesystem: its path, the native `used / total (pct%)` text, and
/// a progress bar wearing the shell's own `ts-stat-progress` class.
fn mount_row(index: usize, disk: &Disk) -> Node {
    nodes::row(vec![
        label(disk.path.clone(), &["ts-disk"]),
        Node::Spacer,
        label(
            format::used_of_total(disk.used_bytes, disk.total_bytes),
            &["numeric", "ts-disk"],
        ),
        Node::Progress {
            id: None,
            // The native row recomputes the fraction from the byte counts
            // rather than taking `DiskMount.usage`; both are the same quantity
            // and this takes the sampler's sanitised one, which is the value
            // the lamp above it is drawn from — so the bar and the lamp for one
            // mount can never disagree.
            fraction: f64::from(disk.usage.clamp(0.0, 1.0)),
            classes: cls("ts-stat-progress"),
        },
    ])
    .id(format!("stats-panel-mount-{index}"))
    .spacing(6)
    .tooltip(format!(
        "{}: {:.0}%",
        disk.path,
        disk.usage.clamp(0.0, 1.0) * 100.0
    ))
    .build()
}

#[cfg(test)]
mod tests {
    use super::panel;
    use crate::card::Widgets;
    use crate::config::Card;
    use crate::sample::{Disk, Gpu, Memory, Snapshot};
    use hytte_plugin::display::RenderMode;
    use hytte_plugin::display::testing::with_render_mode;
    use hytte_plugin::proto::Node;

    /// The page's skeleton, taken with the host advertising the preem
    /// vocabulary.
    ///
    /// `RenderMode` is process-global and defaults to `Raster` with no live
    /// session, so without this the wrappers lower to `Node::Pixels` and the
    /// golden below would record the *fallback* shape — which is a real shape,
    /// but not the one a preem-speaking shell draws and not the one #865 is
    /// about. `card.rs`'s `preem_ids` does the same for the sidebar card.
    fn skeleton_of(cfg: Card, snapshot: &Snapshot) -> Vec<String> {
        with_render_mode(RenderMode::State, || {
            let mut out = Vec::new();
            skeleton(&panel(cfg, snapshot, &Widgets::default()), &mut out);
            out
        })
    }

    /// A machine with something to say about every card — including, since
    /// the #1295 review's MED 1, the four rows that were missing from it:
    /// a process count, a CPU clock, a disk I/O rate and a GPU VRAM level.
    fn busy() -> Snapshot {
        Snapshot {
            cpu: Some(0.42),
            per_core: vec![0.1, 0.5, 0.9, 0.3],
            cpu_temp_c: Some(61.0),
            gpu: Some(Gpu {
                name: "Test Adapter".to_owned(),
                load: Some(0.37),
                temperature_c: Some(52.0),
                // 4 GiB / 16 GiB — a clean 25%, chosen so the golden's string
                // assertion needs no float-rounding judgment call.
                memory_used_bytes: Some(4_294_967_296),
                memory_total_bytes: Some(17_179_869_184),
            }),
            memory: Some(Memory {
                used: 11_999_999_000,
                total: 33_500_000_000,
                swap_used: 1_073_741_824,
                swap_total: 8_589_934_592,
            }),
            disks: vec![
                Disk {
                    path: "/".to_owned(),
                    used_bytes: 40_000_000_000,
                    total_bytes: 100_000_000_000,
                    usage: 0.4,
                },
                Disk {
                    path: "/home".to_owned(),
                    used_bytes: 730_000_000_000,
                    total_bytes: 1_000_000_000_000,
                    usage: 0.73,
                },
            ],
            processes: Some(287),
            cpu_clock_hz: Some(3_800_000_000.0),
            disk_io: Some(crate::sample::DiskIo {
                read_bps: 2_097_152.0,
                write_bps: 1_048_576.0,
                total_read_bytes: 107_374_182_400,
                total_write_bytes: 53_687_091_200,
            }),
        }
    }

    /// The whole table this page draws for a bar instance: every card's
    /// `boxed-list` and every id inside it.
    ///
    /// Every `Node` kind and every id, in tree order, so the shape of the page
    /// is a literal rather than a description of one.
    fn skeleton(node: &Node, out: &mut Vec<String>) {
        let (kind, id, children): (&str, Option<&str>, Vec<&Node>) = match node {
            Node::Box { id, children, .. } => ("Box", id.as_deref(), children.iter().collect()),
            Node::Row { id, children, .. } => ("Row", id.as_deref(), children.iter().collect()),
            Node::ListBox { id, children, .. } => {
                ("ListBox", id.as_deref(), children.iter().collect())
            }
            Node::Label { id, .. } => ("Label", id.as_deref(), Vec::new()),
            Node::Preem { id, .. } => ("Preem", id.as_deref(), Vec::new()),
            Node::Pixels { id, .. } => ("Pixels", id.as_deref(), Vec::new()),
            Node::Progress { id, .. } => ("Progress", id.as_deref(), Vec::new()),
            Node::Spacer => ("Spacer", None, Vec::new()),
            other => panic!("the page must not use {other:?}"),
        };
        out.push(match id {
            Some(id) => format!("{kind}#{id}"),
            None => kind.to_owned(),
        });
        for child in children {
            skeleton(child, out);
        }
    }

    /// Every `ts-*`/`.ts-stat-progress` class the page carries, in tree order
    /// — the page half of `card::tests::the_bar_renders_four_chips_each_keeping_its_class`
    /// (#1295 review MED 3). `skeleton` deliberately records kind+id only (it
    /// calls itself "the exact node tree", and a class is neither); this is
    /// its sibling for the one thing `skeleton` cannot see.
    fn classes_of(node: &Node, out: &mut Vec<String>) {
        let children: Vec<&Node> = match node {
            Node::Box {
                classes, children, ..
            }
            | Node::Row {
                classes, children, ..
            }
            | Node::ListBox {
                classes, children, ..
            } => {
                out.extend(classes.iter().cloned());
                children.iter().collect()
            }
            Node::Label { classes, .. }
            | Node::Preem { classes, .. }
            | Node::Pixels { classes, .. }
            | Node::Progress { classes, .. } => {
                out.extend(classes.iter().cloned());
                Vec::new()
            }
            Node::Spacer => Vec::new(),
            other => panic!("the page must not use {other:?}"),
        };
        for child in children {
            classes_of(child, out);
        }
    }

    /// **The golden**: the exact node tree a `[bar]`-configured instance draws
    /// for [`busy`].
    ///
    /// Written out rather than computed, so a card that silently stops being
    /// emitted — or an id that drifts away from the reconciler key some other
    /// surface already uses — reds here instead of on glass.
    ///
    /// **Falsified** by deleting any `if cfg.…` arm in [`panel`], by dropping
    /// the swap pair, or by renaming one id.
    #[test]
    fn the_pages_node_tree_is_the_recorded_one() {
        let mut cfg = Card::bar_default();
        // The page draws the lamp row and the sweep regardless of whether the
        // CHIP does — but they ride the same two keys, so the golden is taken
        // with them on, which is also the configuration a reader who turned
        // them on would see.
        cfg.per_core = true;
        cfg.history = true;

        let got = skeleton_of(cfg, &busy());

        assert_eq!(
            got,
            vec![
                "Box#stats-panel",
                // CPU
                "ListBox#stats-panel-cpu",
                "Row", // CPU
                "Label",
                "Spacer",
                "Label",
                "Row", // Processes (#1295 review MED 1)
                "Label",
                "Spacer",
                "Label",
                "Row", // Per-core
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-cores-0",
                "Row#stats-panel-cpu-temp-row",
                "Preem#stats-panel-cpu-temp",
                "Label",
                "Spacer",
                "Preem#stats-panel-cpu-history",
                "Row", // Clock (#1295 review MED 1)
                "Label",
                "Spacer",
                "Label",
                // Memory
                "ListBox#stats-panel-memory",
                "Row",
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-memory-level",
                "Row",
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-swap-level",
                // GPU
                "ListBox#stats-panel-gpu",
                "Row", // GPU (name)
                "Label",
                "Spacer",
                "Label",
                "Row", // Load
                "Label",
                "Spacer",
                "Label",
                "Row", // Temperature
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-gpu-load",
                "Row", // VRAM (#1295 review MED 1)
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-gpu-vram",
                // Disks
                "ListBox#stats-panel-disks",
                "Row", // Disks (N mount(s))
                "Label",
                "Spacer",
                "Label",
                "Row",
                "Preem#stats-panel-disk-lamps",
                "Spacer",
                "Row#stats-panel-mount-0",
                "Label",
                "Spacer",
                "Label",
                "Progress",
                "Row#stats-panel-mount-1",
                "Label",
                "Spacer",
                "Label",
                "Progress",
                "Row", // Disk I/O (#1295 review MED 1)
                "Label",
                "Spacer",
                "Label",
                "Preem#stats-panel-disk-io",
            ],
        );
    }

    /// Every id in the page is unique — a preem node's id is its reconciler key
    /// (#900), and two nodes sharing one is a widget that flickers between two
    /// states rather than a visible error.
    #[test]
    fn every_id_on_the_page_is_unique() {
        let mut cfg = Card::bar_default();
        cfg.per_core = true;
        cfg.history = true;
        let got = skeleton_of(cfg, &busy());
        let ids: Vec<&String> = got.iter().filter(|s| s.contains('#')).collect();
        let mut sorted: Vec<&&String> = ids.iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate ids in {ids:?}");
    }

    /// Every card can be switched off on its own, and switching one off removes
    /// exactly its own `boxed-list`.
    ///
    /// **Falsified** by making any card unconditional.
    #[test]
    fn each_switch_removes_exactly_its_own_card() {
        let lists = |cfg: Card| {
            skeleton_of(cfg, &busy())
                .into_iter()
                .filter(|s| s.starts_with("ListBox#"))
                .collect::<Vec<_>>()
        };

        let all = lists(Card::bar_default());
        assert_eq!(
            all,
            vec![
                "ListBox#stats-panel-cpu",
                "ListBox#stats-panel-memory",
                "ListBox#stats-panel-gpu",
                "ListBox#stats-panel-disks",
            ],
        );

        for (name, off) in [
            (
                "stats-panel-cpu",
                Card {
                    cpu: false,
                    ..Card::bar_default()
                },
            ),
            (
                "stats-panel-memory",
                Card {
                    memory: false,
                    ..Card::bar_default()
                },
            ),
            (
                "stats-panel-gpu",
                Card {
                    gpu: false,
                    ..Card::bar_default()
                },
            ),
            (
                "stats-panel-disks",
                Card {
                    disk: false,
                    ..Card::bar_default()
                },
            ),
        ] {
            let got = lists(off);
            assert!(
                !got.iter().any(|s| s == &format!("ListBox#{name}")),
                "{name}"
            );
            assert_eq!(got.len(), all.len() - 1, "{name} removed more than itself");
        }
    }

    /// A machine with no GPU draws no GPU card at all — not a card with a
    /// needle parked at zero, which is the native page's rule and the one hide
    /// that matters most (it is what makes `gpu = true` free on a GPU-less
    /// box).
    #[test]
    fn a_machine_with_no_gpu_draws_no_gpu_card() {
        let snapshot = Snapshot {
            gpu: None,
            ..busy()
        };
        let got = skeleton_of(Card::bar_default(), &snapshot);
        assert!(
            !got.iter().any(|s| s.contains("stats-panel-gpu")),
            "{got:?}",
        );
    }

    /// A machine with no swap draws no swap row — the native page's
    /// `m.swap_total > 0` rule, and the only place on this page where a row
    /// inside a card disappears on its own.
    #[test]
    fn a_machine_with_no_swap_draws_no_swap_row() {
        let snapshot = Snapshot {
            memory: Some(Memory {
                used: 1,
                total: 2,
                swap_used: 0,
                swap_total: 0,
            }),
            ..busy()
        };
        let got = skeleton_of(Card::bar_default(), &snapshot);
        assert!(
            got.iter().any(|s| s == "Preem#stats-panel-memory-level"),
            "the memory meter is still there: {got:?}",
        );
        assert!(
            !got.iter().any(|s| s == "Preem#stats-panel-swap-level"),
            "{got:?}",
        );
    }

    /// An empty mount list says so rather than rendering an empty card.
    #[test]
    fn no_mounts_is_a_row_that_says_so() {
        let snapshot = Snapshot {
            disks: Vec::new(),
            ..busy()
        };
        let node = panel(Card::bar_default(), &snapshot, &Widgets::default());
        let mut texts = Vec::new();
        collect_text(&node, &mut texts);
        assert!(texts.iter().any(|t| t == "No mounts"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "0 mount(s)"), "{texts:?}");
    }

    /// The exact numbers the page prints for [`busy`] — the mirror of the
    /// native page's own format strings, asserted as literals so a change to
    /// either side of the mirror is visible here (#1026).
    #[test]
    fn the_page_prints_the_native_pages_strings() {
        let mut cfg = Card::bar_default();
        cfg.per_core = true;
        let node = panel(cfg, &busy(), &Widgets::default());
        let mut texts = Vec::new();
        collect_text(&node, &mut texts);

        for want in [
            "CPU",
            "42%",
            "Processes",
            "287",
            "Per-core",
            "4 cores",
            "Clock",
            "3.8 GHz",
            "Memory",
            "11.2 GiB / 31.2 GiB (36%)",
            "Swap",
            "1.0 GiB / 8.0 GiB (12%)",
            "GPU",
            "Test Adapter",
            "Load",
            "37%",
            "Temperature",
            "52 °C",
            "VRAM",
            "25%",
            "Disks",
            "2 mount(s)",
            "/",
            "37.3 GiB / 93.1 GiB (40%)",
            "/home",
            "679.9 GiB / 931.3 GiB (73%)",
            "Disk I/O",
            "\u{2193} 2.0 MiB/s \u{2191} 1.0 MiB/s",
        ] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
    }

    /// The four rows the #1295 review's MED 1 found missing, each hidden
    /// exactly the way the native row it mirrors hides — a machine that
    /// answers `None`/`0` for every one of them draws none of the four.
    ///
    /// **Falsified** by dropping any of the four `if let`/unconditional
    /// pushes MED 1 added to `cpu_rows` / `gpu_rows` / `disk_rows`.
    #[test]
    fn the_four_med_1_rows_hide_exactly_like_their_native_ones() {
        let original = busy();
        let nothing_extra = Snapshot {
            processes: None,
            cpu_clock_hz: None,
            disk_io: None,
            gpu: original.gpu.clone().map(|g| Gpu {
                memory_used_bytes: None,
                memory_total_bytes: None,
                ..g
            }),
            ..original
        };
        let mut texts = Vec::new();
        collect_text(
            &panel(Card::bar_default(), &nothing_extra, &Widgets::default()),
            &mut texts,
        );

        assert!(
            !texts.iter().any(|t| t == "Clock"),
            "no cpufreq governor, no Clock row: {texts:?}",
        );
        assert!(!texts.iter().any(|t| t == "VRAM"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Disk I/O"), "{texts:?}");
        // Processes is the one MED 1 row with no hide rule — it dashes
        // instead, the same "unmeasured, not absent" convention every other
        // seed-render reading on this page follows.
        assert!(texts.iter().any(|t| t == "Processes"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "—"), "{texts:?}");
    }

    fn collect_text(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => {
                for child in children {
                    collect_text(child, out);
                }
            }
            Node::Label { text, .. } | Node::Text { text, .. } => out.push(text.clone()),
            _ => {}
        }
    }

    /// Every `ts-*` class the page carries, in tree order — the page half of
    /// `card::tests::the_bar_renders_four_chips_each_keeping_its_class`, and
    /// the only thing that pins `docs/live-verify.md`'s "the plugin's page
    /// rows must tint". `ts-stat-progress` in particular is not decoration:
    /// it is what makes a mount bar the shell's own pill rather than a
    /// default `GtkProgressBar` (`assets/trollshell/style.css:1899-1913`,
    /// applied by the host at `trollshell/src/plugins/wire_map.rs:596-600`).
    ///
    /// Lifted from the #1295 review's MED 3, adapted to this crate's own
    /// `busy`/`cls` names.
    ///
    /// **Falsified** by dropping any `cls(...)` argument in this module — in
    /// particular `mount_row`'s `cls("ts-stat-progress")` on the
    /// `Node::Progress`.
    #[test]
    fn the_page_keeps_the_shells_own_classes() {
        let mut got = Vec::new();
        classes_of(
            &panel(Card::bar_default(), &busy(), &Widgets::default()),
            &mut got,
        );
        assert!(got.iter().any(|c| c == "ts-stat-progress"), "{got:?}");
        for want in [
            "ts-cpu",
            "ts-cpu-temp",
            "ts-gpu",
            "ts-gpu-temp",
            "ts-memory",
            "ts-disk",
        ] {
            assert!(got.iter().any(|c| c == want), "{want} missing: {got:?}");
        }
    }
}
