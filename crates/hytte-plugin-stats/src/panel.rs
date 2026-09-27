//! The plugin's **own drawer page** (#1251) — the native Stats page's cards
//! re-expressed in the wire vocabulary, and since #1252 in the native page's
//! own **layout and widgets**: two columns of `boxed-list` cards, one-line
//! "label … value" rows, `Progress` bars and flat history lines. **No preem.**
//!
//! A click on any bar chip emits
//! [`Effect::OpenPage(Page::PluginSelf)`](hytte_plugin::proto::Page::PluginSelf)
//! and the host resolves that to this tree, keyed by the effect's plugin id.
//! It rides the same render frame as the chips (`View::panel`), so the page and
//! the chip a click came from are always one observation of the machine.
//!
//! # Why it stopped being a preem page (#1252)
//!
//! #1251 drew this page with the retro kit — a seven-segment temperature, a
//! gauge needle for GPU load, LED strips for memory and swap, phosphor scopes
//! for every history — in one narrow column. Next to the native page it read
//! as a different program, and Annika said so ("typ ultra dogshit compared
//! to [the native page] … maybe not use preem widgets here?"). The sidebar
//! card is the preem surface and stays one (`crate::card::card`, which she
//! does like); **this** page mirrors `trollshell/src/panels/stats.rs`'s
//! multicolumn layout card for card, with the widgets that page uses:
//!
//! | native (`panels/stats.rs`) | here |
//! | --- | --- |
//! | `page_grid`, `CPU \| Memory` / `GPU \| Disks` | a **homogeneous** horizontal [`Node::Box`] (the grid is column-homogeneous too) of two vertical column boxes — CPU over GPU, Memory over Disk; the wire has no grid, and two columns of independent height is what the grid draws once the Services row is gone |
//! | `adw::PreferencesGroup` per card | a `boxed-list` [`Node::ListBox`] (not `dense`: the native rows keep libadwaita's row height), also classed `ts-page-card` so the shell's plugin-page flattening of `boxed-list` leaves this page's cards their card surface |
//! | `AdwActionRow` / `slim_row` (title, subtitle, suffix) | a [`Node::Row`] classed `header`, so libadwaita's own `row > box.header` rule (12 px margins, 50 px rows) applies to it exactly as to `slim_row`'s box; title, then the subtitle as a `subtitle` label **on the same line** (a two-line title column cannot be vertically centred from the wire), a [`Node::Spacer`], then the suffix |
//! | `build_history_row`'s `[name 80px \| Sparkline \| value 80px]` | a `ts-history-row` [`Node::Row`] of a `ts-stat-name` label, a [`Node::Sparkline`] — the **same** `hytte_ui::Sparkline` — and a `ts-stat-value` label; the 80 px columns are the `ts-stat-col` rule, since a node cannot call `set_size_request` |
//! | memory / swap / per-mount `GtkProgressBar` (`ts-stat-progress`) | [`Node::Progress`] with the same class |
//! | the Disk `AdwExpanderRow`, collapsed | a [`Node::Expander`], collapsed until clicked (the plugin holds the flag — see [`DISKS_EXPANDER_ID`]) |
//! | Disk I/O's `↓ … ↑ …` / `min … · max …` / `total ↓ … ↑ …` lines, indented 88 px | three `ts-stat-value` labels under the line, indented by the `ts-stat-detail` rule |
//! | `build_top_apps_expander`'s "Top apps · CPU" / "Top apps · RAM" `AdwExpanderRow`s, last in the CPU and Memory cards, collapsed, the heaviest app's `name · value` dimmed beside the chevron | a [`Node::Expander`] each, in the same two places, collapsed until clicked (the plugin holds both flags — [`TOP_APPS_CPU_ID`], [`TOP_APPS_RAM_ID`]), the same `name · value` or `—` in a 20-character ellipsizing `dim-label` [`Node::Text`] pinned right of the title |
//! | `rebuild_top_apps`' `slim_row` per app: name (20 characters), `N processes` when a group has more than one, the `42%` / `1.2 GiB` suffix | a `ts-history-row` [`Node::Row`] per app, at most [`hytte_sensors::app_usage::TOP_N`] (the walker's own cap): the name as a 20-character ellipsizing `Text`, the `N processes` subtitle on the same line, a [`Node::Spacer`], then the value — **no icon** (below) |
//!
//! The history lines keep [`HISTORY_LEN`] samples, the native rows' 60, on the
//! native rows' axes: load, memory, GPU usage and VRAM on a fixed `0..=1`, the
//! clock normalised to the highest `cpuinfo_max_freq` (the native row's fixed
//! "0→max-clock" domain), GPU temperature and the disk I/O rate auto-scaled —
//! which for disk I/O is now exactly the native row's windowed max rather than
//! #1251's session-peak simplification, because the line auto-scales over the
//! same sixty samples the native `Sparkline` holds.
//!
//! Against a shell too old to draw a [`Node::Sparkline`] (it is negotiated,
//! generation 7), every line degrades to a `Progress` bar at its newest
//! sample's level — `hytte_plugin::nodes::sparkline`'s fallback — so even the
//! old-shell page carries no preem.
//!
//! # What it cannot mirror
//!
//! - **Services** (the fifth card): failed systemd units are a system-bus
//!   client and flapping shell tasks are the shell's own task supervisor; a
//!   plugin process can reach neither. Epic #1248 has that card staying
//!   native.
//! - **Top apps' icons and display names.** The rows themselves are here
//!   (#1419 item 3), sampled in this process by the native page's own walker
//!   (`crate::top_apps`, a crate-private module). What is not: native resolves
//!   each row's icon and display name through `gio::DesktopAppInfo`, which a
//!   GTK-free plugin cannot call. The icon column is **pending #1419's
//!   question** (read the `.desktop` file in the plugin, or a wire node the
//!   shell resolves), so a row carries no icon node at all rather than a
//!   placeholder. A row names its group by its raw key — the scope's app id,
//!   the unit name, `System` — so on niri a row reads `niri-firefox` where
//!   native reads `Firefox`; most rows differ, not only apps with no
//!   `.desktop` file. A follow-up ports native's name lookup to the plugin
//!   (see `crate::top_apps`' module docs).
//! - **Top apps' collapsed summary while both lists are collapsed.** Native
//!   shows the heaviest app beside the chevron whenever the drawer is open.
//!   This plugin cannot see its page open or close, so its walker runs only
//!   while one of the two lists is expanded, and a collapsed header reads `—`
//!   while nothing is being measured. The flip side is a known cost: a list
//!   left open keeps walking `/proc` after the drawer closes, until #1427
//!   tells the plugin when its page is on screen — see `crate::top_apps`'
//!   module docs for the measured numbers.
//! - **The per-core LED panel.** Native draws it with `hytte_preem::LedMatrix`,
//!   which is not on the wire (#1156); the only lamp the wire has is a preem
//!   `DotMatrix`, which is exactly what this page is no longer allowed to
//!   carry, and the flat alternative — one `Progress` per core — is the 64-bar
//!   strip #702 removed for its minimum width. So the page omits it (and the
//!   "Per-core · N cores" header row that only introduced it). The sidebar card
//!   still draws the lamp row.
//! - **The per-core history** (native's expandable CPU and Clock rows switch
//!   to a `MultiSparkline`): one line per node, and N lines is N nodes; the
//!   overall line is the one that fits.
//!
//! # The page width
//!
//! It is **this tree's content**, not the host's frame. The drawer sizes a
//! plugin page to the page's own natural width, and a host clamp (#1412's
//! `finish_page_clamped(…, DRAWER_MAX_WIDTH_WIDE)`) only *caps* it: libadwaita's
//! clamp reports its child's natural width whenever that is under the cap, so
//! a 1080 px cap never binds here (#1414 review, LOW 7: 736 px as mounted on
//! main and 758 px inside #1412's frame with unequal columns; 882 px and
//! 904 px once they are equal). What makes two columns read as two columns is
//! therefore theirs to decide: the root box is homogeneous, so both columns
//! take the wider one's width — which is also what sets the page's minimum.

use std::collections::VecDeque;

use hytte_plugin::nodes;
use hytte_plugin::proto::{Dir, Node};
use hytte_sensors::app_usage::{ProcSample, TOP_N};

use crate::card::{label, percent_text};
use crate::format;
use crate::sample::{Disk, Snapshot};
use crate::top_apps::TopApps;

/// The page's root node id.
pub const ROOT_ID: &str = "stats-panel";

/// The id of the Disk card's expander — one of the page's three click targets.
///
/// The wire's [`Node::Expander`] is **plugin-driven**: the host fires a click
/// at this id and the plugin flips its own `expanded` flag and re-renders
/// (`Stats::update`), so the model is the only place the state lives.
pub const DISKS_EXPANDER_ID: &str = "stats-panel-disks-mounts";

/// The id of the CPU card's "Top apps · CPU" expander (#1419 item 3), and the
/// prefix of its rows' ids. Plugin-driven like [`DISKS_EXPANDER_ID`].
pub const TOP_APPS_CPU_ID: &str = "stats-panel-top-apps-cpu";

/// The id of the Memory card's "Top apps · RAM" expander (#1419 item 3), and
/// the prefix of its rows' ids.
pub const TOP_APPS_RAM_ID: &str = "stats-panel-top-apps-ram";

/// The natural width, in characters, of a Top apps row's name and of the
/// collapsed summary beside the chevron — native `TOP_APPS_TITLE_CHARS`, so a
/// long app id costs an ellipsis rather than the page's width.
const TOP_APPS_CHARS: i32 = 20;

/// Which of the page's expanders are open — held by the plugin, because the
/// wire's [`Node::Expander`] is plugin-driven, and collapsed by default like
/// every native one.
///
/// Three independent flags, one per expander on the page: the Disk card's
/// mounts and the two Top apps lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Expanded {
    /// The Disk card's mount list ([`DISKS_EXPANDER_ID`]).
    pub disks: bool,
    /// The CPU card's Top apps list ([`TOP_APPS_CPU_ID`]).
    pub top_cpu: bool,
    /// The Memory card's Top apps list ([`TOP_APPS_RAM_ID`]).
    pub top_ram: bool,
}

impl Expanded {
    /// Whether either Top apps list is open — which is what gates the `/proc`
    /// walker (see `crate::top_apps`), since the plugin cannot see its page
    /// open or close.
    #[must_use]
    pub const fn top_apps(self) -> bool {
        self.top_cpu || self.top_ram
    }
}

/// How many samples each history line keeps: the native page's 60 (one a
/// second for a minute at the default cadence — `build_history_row`'s
/// `Sparkline::new(60)`).
pub const HISTORY_LEN: usize = 60;

/// libadwaita's own row-header class: `row > box.header` gives a box 12 px
/// side margins, 6 px spacing and a 50 px minimum height, and a [`Node::Row`]
/// inside a `boxed-list` is exactly `row > box` (the host wraps each list child
/// in a `GtkListBoxRow`). This is what `slim_row` builds by hand natively.
const HEADER: &str = "header";

/// The fixed 80 px name/value columns of a history row (`ts-stat-col` in the
/// shell's stylesheet — the `set_size_request(scale(80))` a node cannot make).
const STAT_COL: &str = "ts-stat-col";

/// The 88 px indent of the Disk I/O detail lines (`ts-stat-detail`).
const STAT_DETAIL: &str = "ts-stat-detail";

/// The class that opts a card back into libadwaita's card surface on a plugin
/// page (`.ts-plugin-panel list.boxed-list.ts-page-card` in the shell's
/// stylesheet), out-ranking the shell's page-wide `boxed-list` flattening.
const PAGE_CARD: &str = "ts-page-card";

/// The Disk card's expander hook (`ts-page-expander`): the shell stylesheet
/// gives its header button a libadwaita row's 50 px height and 12 px inset.
const PAGE_EXPANDER: &str = "ts-page-expander";

/// The page's history lines, as plain samples — oldest first, each capped at
/// [`HISTORY_LEN`].
///
/// Held in the model and fed by [`History::push`] once per sample, because the
/// host keeps no history for a [`Node::Sparkline`]: every render restates the
/// whole window. A reading the snapshot withholds (a cold `/proc/stat` tick, a
/// vendor with no VRAM counter) is **not** a sample and does not move its line,
/// the same rule the sidebar card's scope ring follows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct History {
    /// Overall CPU load, `0.0..=1.0`.
    cpu: VecDeque<f32>,
    /// Aggregate clock over the `cpuinfo_max_freq` ceiling, `0.0..=1.0`.
    clock: VecDeque<f32>,
    /// Memory used over total, `0.0..=1.0`.
    memory: VecDeque<f32>,
    /// GPU load, `0.0..=1.0`.
    gpu_load: VecDeque<f32>,
    /// VRAM used over total, `0.0..=1.0`.
    gpu_vram: VecDeque<f32>,
    /// GPU temperature, °C (auto-scaled).
    gpu_temp: VecDeque<f32>,
    /// Combined disk read + write rate, bytes/s (auto-scaled).
    disk_io: VecDeque<f32>,
}

/// Append one sample to a ring, dropping the oldest past [`HISTORY_LEN`].
fn push_capped(ring: &mut VecDeque<f32>, sample: f32) {
    while ring.len() >= HISTORY_LEN {
        ring.pop_front();
    }
    ring.push_back(sample);
}

impl History {
    /// Fold one snapshot's readings into the lines — one point per line that
    /// has a reading this tick, none for a line that does not.
    pub fn push(&mut self, snapshot: &Snapshot) {
        if let Some(cpu) = snapshot.cpu {
            push_capped(&mut self.cpu, cpu.clamp(0.0, 1.0));
        }
        if let (Some(hz), Some(ceiling)) = (snapshot.cpu_clock_hz, snapshot.cpu_clock_ceiling_hz)
            && ceiling > 0.0
        {
            // A ratio in `0.0..=1.0`, so the narrowing cast loses nothing a
            // line can draw.
            #[allow(clippy::cast_possible_truncation)]
            push_capped(&mut self.clock, (hz / ceiling).clamp(0.0, 1.0) as f32);
        }
        if let Some(m) = snapshot.memory.filter(|m| m.total > 0) {
            push_capped(&mut self.memory, format::fraction(m.used, m.total));
        }
        if let Some(gpu) = snapshot.gpu.as_ref() {
            if let Some(load) = gpu.load {
                push_capped(&mut self.gpu_load, load.clamp(0.0, 1.0));
            }
            if let Some((used, total)) = gpu
                .memory_used_bytes
                .zip(gpu.memory_total_bytes)
                .filter(|(_, total)| *total > 0)
            {
                push_capped(&mut self.gpu_vram, format::fraction(used, total));
            }
            if let Some(c) = gpu.temperature_c.filter(|c| c.is_finite()) {
                push_capped(&mut self.gpu_temp, c);
            }
        }
        if let Some(io) = snapshot.disk_io.as_ref() {
            // Bytes per second in an `f32`: exact to 2^24 B/s and within a
            // part in ten million above it, far finer than the line's pixels.
            #[allow(clippy::cast_possible_truncation)]
            push_capped(&mut self.disk_io, (io.read_bps + io.write_bps) as f32);
        }
    }

    /// The `(min, max)` combined disk rate over the window, in bytes/s — the
    /// native Disk I/O row's `min … · max …` line, which is taken over the
    /// same sixty samples the line draws. `(0, 0)` before the first sample.
    fn disk_io_range(&self) -> (f64, f64) {
        let mut samples = self.disk_io.iter().copied().map(f64::from);
        let Some(first) = samples.next() else {
            return (0.0, 0.0);
        };
        samples.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v)))
    }
}

/// Project a snapshot into the drawer page.
///
/// Pure, like [`card::card`](crate::card::card) and
/// [`card::chips`](crate::card::chips): every branch is decided by `cfg`, by
/// what the snapshot, the history and the latest Top apps walk hold, and by
/// which expanders are open, so the whole page is testable from literals.
///
/// Of `cfg`'s keys, the page reads `cpu` / `memory` / `gpu` / `disk` (a card
/// each — the two Top apps lists ride the CPU and Memory cards, as native)
/// and `temperature` (every `°C` reading). `per_core` and `history` are the
/// **chips'** switches — the per-core lamp row and the chip's scope sweep — and
/// the page, which draws neither, ignores them: its history lines are the
/// native page's, and the native page does not make them optional.
#[must_use]
pub fn panel(
    cfg: crate::config::Card,
    snapshot: &Snapshot,
    history: &History,
    top_apps: &TopApps,
    expanded: Expanded,
) -> Node {
    let mut left = Vec::new();
    let mut right = Vec::new();

    if cfg.cpu {
        let mut rows = cpu_rows(cfg, snapshot, history);
        // Last in the CPU card, as native `build_stats_cpu_card` adds it.
        rows.push(top_apps_expander(
            TOP_APPS_CPU_ID,
            "Top apps \u{00b7} CPU",
            &top_apps.by_cpu,
            cpu_share,
            expanded.top_cpu,
        ));
        left.push(boxed("stats-panel-cpu", rows));
    }
    // The GPU card hides itself entirely when there is nothing to read — the
    // native page's `bind(sensors::gpu().map(|g| g.is_some()), &group, …)`,
    // which hides the whole `PreferencesGroup` rather than drawing an empty
    // card.
    if cfg.gpu && snapshot.gpu.is_some() {
        left.push(boxed("stats-panel-gpu", gpu_rows(cfg, snapshot, history)));
    }
    if cfg.memory {
        let mut rows = memory_rows(snapshot, history);
        // Last in the Memory card, as native `build_stats_memory_card` adds it.
        rows.push(top_apps_expander(
            TOP_APPS_RAM_ID,
            "Top apps \u{00b7} RAM",
            &top_apps.by_mem,
            ram_size,
            expanded.top_ram,
        ));
        right.push(boxed("stats-panel-memory", rows));
    }
    if cfg.disk {
        right.push(boxed(
            "stats-panel-disks",
            disk_rows(snapshot, history, expanded.disks),
        ));
    }

    // A column with nothing in it is left out rather than drawn empty: an
    // empty vertical box still takes a gap's worth of the row, and the page
    // would sit off-centre. The two ids are stable either way, so the
    // reconciler keeps the surviving column's widgets across the change.
    let columns = [("stats-panel-col-0", left), ("stats-panel-col-1", right)]
        .into_iter()
        .filter(|(_, cards)| !cards.is_empty())
        .map(|(id, cards)| {
            nodes::container(Dir::Vertical, cards)
                .id(id)
                .spacing(12)
                .build()
        })
        .collect();

    // No `.card` and no `.ts-plugin-card` on the root: the drawer supplies the
    // page chrome, and a panel root that adds its own reads as a card in a
    // card — the SDK's *Styling* docs, same rule the sidebar card follows.
    //
    // **Homogeneous**, as the native page's grid is: without it each column
    // takes its own natural width and the spare is split equally on top, so
    // the left column (short CPU/GPU rows) came out 289 px beside a 435 px
    // right one and its history lines were 85 px thumbnails (#1414 review,
    // MEDIUM 2). An older shell ignores the class and draws the unequal
    // columns it always did.
    nodes::container(Dir::Horizontal, columns)
        .id(ROOT_ID)
        .spacing(12)
        .homogeneous(true)
        .build()
}

/// One card: a `boxed-list` around its rows, the vocabulary's
/// `adw::PreferencesGroup`. Not `dense` — the native cards keep libadwaita's
/// row height, and so does this one.
///
/// [`PAGE_CARD`] as well as `boxed-list`: the shell flattens every
/// `boxed-list` on a plugin page onto the drawer surface
/// (`.ts-plugin-panel list.boxed-list`), which on this page erased the cards
/// themselves (#1414 review, HIGH 1); the extra class is the page's opt back in
/// to libadwaita's card look.
fn boxed(id: &str, rows: Vec<Node>) -> Node {
    nodes::list(rows)
        .id(id)
        .class("boxed-list")
        .class(PAGE_CARD)
        .build()
}

/// An `AdwActionRow`, on one line: the title, the subtitle beside it (smaller
/// and dimmed by libadwaita's own `row label.subtitle` rule), then whatever the
/// native row puts in its suffix box, pinned right.
fn title_row(id: &str, title: &str, subtitle: Option<Node>, suffix: Vec<Node>) -> Node {
    let mut children = vec![label(title, &[])];
    children.extend(subtitle);
    children.push(Node::Spacer);
    children.extend(suffix);
    nodes::row(children).id(id).class(HEADER).spacing(6).build()
}

/// A row's subtitle — the reading an `AdwActionRow` prints under its title.
fn subtitle(text: impl Into<String>, class: &str) -> Node {
    label(text, &["subtitle", "numeric", class])
}

/// `build_history_row`: `[name | line | value]`, the value column dropped for
/// the one native row that hides it (Disk I/O).
fn history_row(id: &str, name: &str, line: Node, value: Option<Node>) -> Node {
    let mut children = vec![label(name, &["ts-stat-name", STAT_COL]), line];
    children.extend(value);
    nodes::row(children)
        .id(id)
        .class("ts-history-row")
        .spacing(8)
        .build()
}

/// A history row's right-hand reading.
fn stat_value(text: impl Into<String>, class: &str) -> Node {
    label(text, &["ts-stat-value", STAT_COL, class])
}

/// One history line — a [`Node::Sparkline`] against a shell that draws one, a
/// `Progress` at the newest sample's level against one that does not.
fn line(id: &str, ring: &VecDeque<f32>, max: Option<f32>, class: &str) -> Node {
    let builder = nodes::sparkline(ring.iter().copied().collect::<Vec<f32>>())
        .id(id)
        .class(class);
    match max {
        Some(m) => builder.max(m),
        None => builder,
    }
    .build()
}

/// `{t:.0} °C` — the native CPU and GPU rows' temperature suffix, which unlike
/// the bar chip's `{c:.0}°` carries the unit letter and a space.
fn celsius(c: f32) -> String {
    format!("{c:.0} \u{00b0}C")
}

/// The CPU card: the headline row (load as the subtitle, the package
/// temperature as the suffix), Processes, the CPU history line and — with a
/// `cpufreq` governor — the Clock line.
fn cpu_rows(cfg: crate::config::Card, snapshot: &Snapshot, history: &History) -> Vec<Node> {
    let temperature = snapshot
        .cpu_temp_c
        .filter(|c| cfg.temperature && c.is_finite());
    let mut rows = vec![
        title_row(
            "stats-panel-cpu-row",
            "CPU",
            Some(subtitle(percent_text(snapshot.cpu), "ts-cpu")),
            temperature
                .map(|c| label(celsius(c), &["numeric", "ts-cpu-temp"]))
                .into_iter()
                .collect(),
        ),
        // Processes — native `build_live_processes_row`. No hide rule of its
        // own; the `—` is the seed render before the first tick.
        title_row(
            "stats-panel-processes-row",
            "Processes",
            None,
            vec![label(
                snapshot
                    .processes
                    .map_or_else(|| "—".to_owned(), |n| n.to_string()),
                &["numeric", "ts-cpu"],
            )],
        ),
        history_row(
            "stats-panel-cpu-history-row",
            "CPU",
            line("stats-panel-cpu-history", &history.cpu, Some(1.0), "ts-cpu"),
            Some(stat_value(percent_text(snapshot.cpu), "ts-cpu")),
        ),
    ];

    // Clock — native `build_expandable_cpu_clock_row`'s collapsed line: the
    // aggregate clock over the highest `cpuinfo_max_freq`, hidden with no
    // `cpufreq` governor (`snapshot.cpu_clock_hz` is `None` then — see
    // `Sampler::tick`).
    if let Some(hz) = snapshot.cpu_clock_hz {
        rows.push(history_row(
            "stats-panel-clock-history-row",
            "Clock",
            line(
                "stats-panel-clock-history",
                &history.clock,
                Some(1.0),
                "ts-cpu",
            ),
            Some(stat_value(format::hz(hz), "ts-cpu")),
        ));
    }
    rows
}

/// The Memory card: memory and — on a machine that has any — swap, each as
/// the native row's `used / total (pct%)` subtitle over a `ts-stat-progress`
/// bar, then the memory history line.
fn memory_rows(snapshot: &Snapshot, history: &History) -> Vec<Node> {
    let mem = snapshot.memory.as_ref();
    let meter = |used: u64, total: u64| Node::Progress {
        id: None,
        fraction: f64::from(format::fraction(used, total)),
        classes: vec!["ts-stat-progress".to_owned()],
    };
    let mut rows = vec![title_row(
        "stats-panel-memory-row",
        "Memory",
        Some(subtitle(
            mem.map_or_else(
                || "—".to_owned(),
                |m| format::used_of_total(m.used, m.total),
            ),
            "ts-memory",
        )),
        vec![mem.map_or_else(|| meter(0, 0), |m| meter(m.used, m.total))],
    )];
    // Swap hides itself when the machine has none — the native page's own rule
    // (`m.swap_total > 0`), not a new one.
    if let Some(m) = mem.filter(|m| m.swap_total > 0) {
        rows.push(title_row(
            "stats-panel-swap-row",
            "Swap",
            Some(subtitle(
                format::used_of_total(m.swap_used, m.swap_total),
                "ts-memory",
            )),
            vec![meter(m.swap_used, m.swap_total)],
        ));
    }
    rows.push(history_row(
        "stats-panel-memory-history-row",
        "Memory",
        line(
            "stats-panel-memory-history",
            &history.memory,
            Some(1.0),
            "ts-memory",
        ),
        Some(stat_value(
            mem.filter(|m| m.total > 0).map_or_else(
                || "—".to_owned(),
                |m| format!("{:.0}%", format::fraction(m.used, m.total) * 100.0),
            ),
            "ts-memory",
        )),
    ));
    rows
}

/// The GPU card: the adapter row (its name as the subtitle, the temperature —
/// or the load where there is no thermal probe — as the suffix), then the
/// usage, VRAM and temperature lines, each hidden when its reading is not
/// reported, exactly as the native rows hide.
///
/// Only reached with `snapshot.gpu.is_some()`.
fn gpu_rows(cfg: crate::config::Card, snapshot: &Snapshot, history: &History) -> Vec<Node> {
    let Some(gpu) = snapshot.gpu.as_ref() else {
        return Vec::new();
    };
    let temperature = gpu
        .temperature_c
        .filter(|c| cfg.temperature && c.is_finite());

    // The native suffix: `{t:.0} °C`, else the load, else nothing.
    let suffix = match (temperature, gpu.load) {
        (Some(c), _) => Some(label(celsius(c), &["numeric", "ts-gpu-temp"])),
        (None, Some(load)) => Some(label(percent_text(Some(load)), &["numeric", "ts-gpu"])),
        (None, None) => None,
    };
    // The device name is driver-reported and can be long; the native row caps
    // its subtitle at 20 characters (`GPU_SUBTITLE_CHARS`) and ellipsizes, and
    // an ellipsizing `Text` tooltips its own full string.
    let name = (!gpu.name.trim().is_empty()).then(|| Node::Text {
        id: None,
        text: gpu.name.clone(),
        max_width_chars: Some(20),
        ellipsize: true,
        classes: vec!["subtitle".to_owned(), "ts-gpu".to_owned()],
        tooltip: None,
    });
    let mut rows = vec![title_row(
        "stats-panel-gpu-row",
        "GPU",
        name,
        suffix.into_iter().collect(),
    )];

    if let Some(load) = gpu.load {
        rows.push(history_row(
            "stats-panel-gpu-usage-history-row",
            "GPU usage",
            line(
                "stats-panel-gpu-usage-history",
                &history.gpu_load,
                Some(1.0),
                "ts-gpu",
            ),
            Some(stat_value(percent_text(Some(load)), "ts-gpu")),
        ));
    }
    // Hidden unless both used and total VRAM are reported — some vendors expose
    // load and temperature but not memory.
    if let Some((used, total)) = gpu
        .memory_used_bytes
        .zip(gpu.memory_total_bytes)
        .filter(|(_, total)| *total > 0)
    {
        rows.push(history_row(
            "stats-panel-gpu-vram-history-row",
            "GPU VRAM",
            line(
                "stats-panel-gpu-vram-history",
                &history.gpu_vram,
                Some(1.0),
                "ts-gpu",
            ),
            Some(stat_value(
                format!("{:.0}%", format::fraction(used, total) * 100.0),
                "ts-gpu",
            )),
        ));
    }
    if let Some(c) = temperature {
        rows.push(history_row(
            "stats-panel-gpu-temp-history-row",
            "GPU temp",
            // A temperature has no natural ceiling: auto-scaled, as native.
            line(
                "stats-panel-gpu-temp-history",
                &history.gpu_temp,
                None,
                "ts-gpu-temp",
            ),
            Some(stat_value(celsius(c), "ts-gpu-temp")),
        ));
    }
    rows
}

/// The Disk card: the native `AdwExpanderRow` — "Disk", `N mount(s)`, one row
/// per mount inside, collapsed until clicked — then the Disk I/O line and its
/// three detail lines.
fn disk_rows(snapshot: &Snapshot, history: &History, expanded: bool) -> Vec<Node> {
    let mounts: Vec<Node> = if snapshot.disks.is_empty() {
        // The native expander opens onto nothing at all, which reads as a
        // broken card; one line saying so is the honest version.
        vec![label("No mounts", &["dim-label", "ts-history-row"])]
    } else {
        snapshot
            .disks
            .iter()
            .enumerate()
            .map(|(i, disk)| mount_row(i, disk))
            .collect()
    };
    let mut rows = vec![Node::Expander {
        id: DISKS_EXPANDER_ID.to_owned(),
        header: Box::new(
            nodes::row(vec![
                label("Disk", &[]),
                subtitle(format!("{} mount(s)", snapshot.disks.len()), "ts-disk"),
                Node::Spacer,
            ])
            .spacing(6)
            .build(),
        ),
        children: mounts,
        expanded,
        // The header is the host's flat button, not a list row, so
        // libadwaita's `row > box.header` rule (the 50 px, 12 px-inset row every
        // other title row on the page gets) cannot reach it; `ts-page-expander`
        // is the shell stylesheet's hook that gives it the same height and
        // inset (#1414 review, nit — it measured 24 px).
        classes: vec![PAGE_EXPANDER.to_owned()],
        tooltip: None,
    }];

    // Disk I/O — native `build_history_disk_io_row`: the auto-scaled line of
    // the combined rate (its right-hand value hidden, "units live in the
    // detail rows below"), then the current `↓ read ↑ write`, the window's
    // `min · max` and the since-boot totals.
    if let Some(io) = snapshot.disk_io.as_ref() {
        let (min, max) = history.disk_io_range();
        let detail = |text: String| label(text, &["ts-stat-value", STAT_DETAIL, "ts-disk"]);
        rows.push(Node::Box {
            id: Some("stats-panel-disk-io".to_owned()),
            dir: Dir::Vertical,
            spacing: 2,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                history_row(
                    "stats-panel-disk-io-history-row",
                    "Disk I/O",
                    line(
                        "stats-panel-disk-io-history",
                        &history.disk_io,
                        None,
                        "ts-disk",
                    ),
                    None,
                ),
                detail(format!(
                    "\u{2193} {} \u{2191} {}",
                    format::rate(io.read_bps),
                    format::rate(io.write_bps),
                )),
                detail(format!(
                    "min {} \u{00b7} max {}",
                    format::rate(min),
                    format::rate(max),
                )),
                detail(format!(
                    "total \u{2193} {} \u{2191} {}",
                    format::bytes(io.total_read_bytes),
                    format::bytes(io.total_write_bytes),
                )),
            ],
            tooltip: None,
        });
    }
    rows
}

/// One mounted filesystem — native `build_disk_mount_row`: its path, the
/// `used / total (pct%)` text and a `ts-stat-progress` bar, with the path as
/// the row's hover.
///
/// Padded with `ts-history-row` rather than classed `header`: the row sits in
/// the expander's body box, not directly in a list row, so libadwaita's
/// `row > box.header` rule cannot reach it, and the history row's padding is
/// the nearest native spacing that can.
fn mount_row(index: usize, disk: &Disk) -> Node {
    nodes::row(vec![
        // A bind-mount path can be long; the native row caps its title at 8
        // characters of natural width and ellipsizes (`DISK_MOUNT_TITLE_CHARS`).
        Node::Text {
            id: None,
            text: disk.path.clone(),
            max_width_chars: Some(8),
            ellipsize: true,
            classes: vec!["ts-disk".to_owned()],
            tooltip: None,
        },
        Node::Spacer,
        label(
            format::used_of_total(disk.used_bytes, disk.total_bytes),
            &["numeric", "dim-label", "ts-disk"],
        ),
        Node::Progress {
            id: None,
            // The native row recomputes the fraction from the byte counts
            // rather than taking `DiskMount.usage`; both are the same quantity
            // and this takes the sampler's sanitised one.
            fraction: f64::from(disk.usage.clamp(0.0, 1.0)),
            classes: vec!["ts-stat-progress".to_owned()],
        },
    ])
    .id(format!("stats-panel-mount-{index}"))
    .class("ts-history-row")
    .spacing(6)
    .tooltip(disk.path.clone())
    .build()
}

/// The "Top apps · CPU" value — native `build_stats_cpu_card`'s formatter,
/// `format!("{:.0}%", s.cpu_frac * 100.0)`, in the same `f64` arithmetic so the
/// two pages round a share identically.
fn cpu_share(app: &ProcSample) -> String {
    format!("{:.0}%", app.cpu_frac * 100.0)
}

/// The "Top apps · RAM" value — native `build_stats_memory_card`'s
/// `fmt_bytes(s.mem_bytes)`, which [`format::bytes`] mirrors.
fn ram_size(app: &ProcSample) -> String {
    format::bytes(app.mem_bytes)
}

/// One Top apps list — native `build_top_apps_expander` plus the rows
/// `rebuild_top_apps` puts in it: `title`, the heaviest app's
/// `name · value` (or `—` with nothing measured) dimmed beside the chevron,
/// and one row per app, heaviest first, at most [`TOP_N`].
///
/// The cap is the walker's own constant rather than a copy of it: the walker
/// already truncates to it, and taking it again here is what bounds the frame
/// whatever reaches the model, without letting the two pages drift apart.
fn top_apps_expander(
    id: &str,
    title: &str,
    list: &[ProcSample],
    value: fn(&ProcSample) -> String,
    expanded: bool,
) -> Node {
    // Native `rebuild_top_apps`' collapsed summary, character for character.
    let summary = list.first().map_or_else(
        || "\u{2014}".to_owned(),
        |app| format!("{} \u{00b7} {}", app.name, value(app)),
    );
    Node::Expander {
        id: id.to_owned(),
        header: Box::new(
            nodes::row(vec![
                label(title, &[]),
                Node::Spacer,
                // The native summary is a fixed-width `dim-label` suffix that
                // hugs the chevron; an ellipsizing `Text` tooltips its own full
                // string, as native sets the summary's tooltip to it.
                Node::Text {
                    id: None,
                    text: summary,
                    max_width_chars: Some(TOP_APPS_CHARS),
                    ellipsize: true,
                    classes: vec!["dim-label".to_owned()],
                    tooltip: None,
                },
            ])
            .spacing(6)
            .build(),
        ),
        children: list
            .iter()
            .take(TOP_N)
            .enumerate()
            .map(|(index, app)| top_app_row(id, index, app, value))
            .collect(),
        expanded,
        // The same header hook as the Disk card's (see `disk_rows`).
        classes: vec![PAGE_EXPANDER.to_owned()],
        tooltip: None,
    }
}

/// One app in a Top apps list — native `rebuild_top_apps`' `slim_row`: the
/// group's name, `N processes` only when more than one PID collapsed into it,
/// and the value pinned right, with the name as the row's hover.
///
/// Classed `ts-history-row` rather than `header` for [`mount_row`]'s reason:
/// it sits in the expander's body box, where libadwaita's `row > box.header`
/// rule cannot reach it. **No icon** — the icon column is pending #1419's
/// question, so there is no node for it, placeholder or otherwise.
fn top_app_row(
    expander_id: &str,
    index: usize,
    app: &ProcSample,
    value: fn(&ProcSample) -> String,
) -> Node {
    let mut children = vec![Node::Text {
        id: None,
        text: app.name.clone(),
        max_width_chars: Some(TOP_APPS_CHARS),
        ellipsize: true,
        classes: Vec::new(),
        tooltip: None,
    }];
    if app.procs > 1 {
        children.push(label(
            format!("{} processes", app.procs),
            &["subtitle", "numeric"],
        ));
    }
    children.push(Node::Spacer);
    children.push(label(value(app), &["numeric"]));
    nodes::row(children)
        .id(format!("{expander_id}-{index}"))
        .class("ts-history-row")
        .spacing(6)
        .tooltip(app.name.clone())
        .build()
}

#[cfg(test)]
mod tests {
    use super::{
        DISKS_EXPANDER_ID, Expanded, HISTORY_LEN, History, ROOT_ID, TOP_APPS_CPU_ID,
        TOP_APPS_RAM_ID, panel,
    };
    use crate::config::Card;
    use crate::sample::{Disk, DiskIo, Gpu, Memory, Snapshot};
    use crate::top_apps::{TopApps, app};
    use hytte_plugin::display::testing::with_negotiated_vocab;
    use hytte_plugin::proto::{Dir, HOMOGENEOUS_CLASS, Node, SPARKLINE_VOCAB};
    use hytte_sensors::app_usage::{ProcSample, TOP_N};

    /// A machine with something to say about every card.
    fn busy() -> Snapshot {
        Snapshot {
            cpu: Some(0.42),
            per_core: vec![0.1, 0.5, 0.9, 0.3],
            cpu_temp_c: Some(61.0),
            gpu: Some(Gpu {
                name: "Test Adapter".to_owned(),
                load: Some(0.37),
                temperature_c: Some(52.0),
                // 4 GiB / 16 GiB — a clean 25%.
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
            cpu_clock_ceiling_hz: Some(5_000_000_000.0),
            disk_io: Some(DiskIo {
                read_bps: 2_097_152.0,
                write_bps: 1_048_576.0,
                total_read_bytes: 107_374_182_400,
                total_write_bytes: 53_687_091_200,
            }),
        }
    }

    /// A walk with something in both lists: a group of three processes on
    /// top of the CPU list, a lone `System` bucket (no app id) below it, and
    /// a RAM list headed by the same browser.
    fn busy_apps() -> TopApps {
        TopApps {
            by_cpu: vec![
                ProcSample {
                    procs: 3,
                    ..app("org.mozilla.firefox", 0.42, 3_435_973_837)
                },
                ProcSample {
                    app_id: None,
                    ..app("System", 0.07, 734_003_200)
                },
            ],
            by_mem: vec![
                ProcSample {
                    procs: 3,
                    ..app("org.mozilla.firefox", 0.42, 3_435_973_837)
                },
                app("pipewire", 0.01, 52_428_800),
            ],
        }
    }

    /// A history with a few ticks of [`busy`] in it.
    fn warm() -> History {
        let mut history = History::default();
        for _ in 0..3 {
            history.push(&busy());
        }
        history
    }

    /// The page against a shell that draws sparklines (today's), or against
    /// the newest one that does not.
    fn page_at(vocab: u16, cfg: Card, snapshot: &Snapshot, expanded: Expanded) -> Node {
        with_negotiated_vocab(vocab, || {
            panel(cfg, snapshot, &warm(), &busy_apps(), expanded)
        })
    }

    /// Every expander on the page open, so every row it can draw is in the tree.
    const ALL_OPEN: Expanded = Expanded {
        disks: true,
        top_cpu: true,
        top_ram: true,
    };

    fn page(cfg: Card, snapshot: &Snapshot) -> Node {
        page_at(SPARKLINE_VOCAB, cfg, snapshot, ALL_OPEN)
    }

    /// Every node in tree order, the expander's header and body included.
    fn walk<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
        out.push(node);
        match node {
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => {
                for child in children {
                    walk(child, out);
                }
            }
            Node::Expander {
                header, children, ..
            } => {
                walk(header, out);
                for child in children {
                    walk(child, out);
                }
            }
            Node::Button { child, .. }
            | Node::Revealer { child, .. }
            | Node::Scrolled { child, .. } => walk(child, out),
            _ => {}
        }
    }

    fn nodes_of(node: &Node) -> Vec<&Node> {
        let mut out = Vec::new();
        walk(node, &mut out);
        out
    }

    fn id_of(node: &Node) -> Option<&str> {
        match node {
            Node::Box { id, .. }
            | Node::Row { id, .. }
            | Node::ListBox { id, .. }
            | Node::Label { id, .. }
            | Node::Text { id, .. }
            | Node::Progress { id, .. }
            | Node::Sparkline { id, .. }
            | Node::Preem { id, .. }
            | Node::Pixels { id, .. } => id.as_deref(),
            Node::Expander { id, .. } | Node::Button { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    fn texts(node: &Node) -> Vec<String> {
        nodes_of(node)
            .into_iter()
            .filter_map(|n| match n {
                Node::Label { text, .. } | Node::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn classes(node: &Node) -> Vec<String> {
        nodes_of(node)
            .into_iter()
            .flat_map(|n| match n {
                Node::Box { classes, .. }
                | Node::Row { classes, .. }
                | Node::ListBox { classes, .. }
                | Node::Label { classes, .. }
                | Node::Text { classes, .. }
                | Node::Progress { classes, .. }
                | Node::Sparkline { classes, .. }
                | Node::Expander { classes, .. } => classes.clone(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// The column ids and, under each, the card ids — the page's layout as a
    /// literal.
    fn layout(node: &Node) -> Vec<(String, Vec<String>)> {
        let Node::Box {
            dir: Dir::Horizontal,
            children,
            ..
        } = node
        else {
            panic!("the root is a horizontal box: {node:?}");
        };
        children
            .iter()
            .map(|column| match column {
                Node::Box {
                    id,
                    dir: Dir::Vertical,
                    children,
                    ..
                } => (
                    id.clone().unwrap_or_default(),
                    children
                        .iter()
                        .map(|card| id_of(card).unwrap_or_default().to_owned())
                        .collect(),
                ),
                other => panic!("a column is a vertical box: {other:?}"),
            })
            .collect()
    }

    /// **The #1252 layout**: two columns, as the native multicolumn page —
    /// CPU over GPU on the left, Memory over Disk on the right — each card a
    /// `boxed-list`, the root a horizontal box of two vertical ones.
    ///
    /// **Falsified** by laying the cards out in one vertical column again (the
    /// root-is-horizontal `let … else` panics), by putting GPU beside CPU, by
    /// dropping `.homogeneous(true)` from the root (the columns take their own
    /// widths again — #1414 review, MEDIUM 2), or by dropping `PAGE_CARD` from
    /// `boxed` (the shell flattens the cards away — #1414 review, HIGH 1; the
    /// shell side of that is `wire_map`'s render test).
    #[test]
    fn the_page_is_two_columns_of_boxed_list_cards() {
        let node = page(Card::bar_default(), &busy());
        let Node::Box { id, classes, .. } = &node else {
            panic!("root is a box");
        };
        assert_eq!(id.as_deref(), Some(ROOT_ID));
        assert!(
            classes.iter().any(|c| c == HOMOGENEOUS_CLASS),
            "the two columns are one width, like the native grid: {classes:?}",
        );
        assert_eq!(
            layout(&node),
            vec![
                (
                    "stats-panel-col-0".to_owned(),
                    vec!["stats-panel-cpu".to_owned(), "stats-panel-gpu".to_owned()],
                ),
                (
                    "stats-panel-col-1".to_owned(),
                    vec![
                        "stats-panel-memory".to_owned(),
                        "stats-panel-disks".to_owned(),
                    ],
                ),
            ],
        );
        for n in nodes_of(&node) {
            if let Node::ListBox { classes, dense, .. } = n {
                assert!(classes.iter().any(|c| c == "boxed-list"), "{classes:?}");
                assert!(
                    classes.iter().any(|c| c == "ts-page-card"),
                    "each card opts back into its card surface: {classes:?}",
                );
                assert!(!dense, "native rows keep libadwaita's row height");
            }
        }
    }

    /// **No preem on the page** — the whole of Annika's ask on #1252 — against
    /// both a shell that draws sparklines and the newest one that does not
    /// (whose fallback is a flat `Progress`, not a scope), while the sidebar
    /// card, which she does like, still is preem.
    ///
    /// **Falsified** by putting any preem widget back on the page (e.g. the
    /// sidebar card's `SevenSeg` temperature in the CPU row), or by making the
    /// SDK's sparkline fallback a preem `Scope`.
    #[test]
    fn the_page_carries_no_preem_and_the_sidebar_card_still_does() {
        let mut cfg = Card::bar_default();
        cfg.per_core = true;
        cfg.history = true;
        for vocab in [SPARKLINE_VOCAB, SPARKLINE_VOCAB - 1, 0] {
            let node = page_at(vocab, cfg, &busy(), ALL_OPEN);
            let preem: Vec<_> = nodes_of(&node)
                .into_iter()
                .filter(|n| matches!(n, Node::Preem { .. } | Node::Pixels { .. }))
                .collect();
            assert!(preem.is_empty(), "vocab {vocab}: {preem:?}");
        }

        let card = with_negotiated_vocab(SPARKLINE_VOCAB, || {
            crate::card::card(
                Card::sidebar_default(),
                &busy(),
                &crate::card::Widgets::default(),
            )
        });
        assert!(
            nodes_of(&card)
                .iter()
                .any(|n| matches!(n, Node::Preem { .. })),
            "the sidebar card is still the preem surface",
        );
    }

    /// Every history line is a `Sparkline` on today's shell — the same widget
    /// the native rows draw — on the native rows' axes, and a `Progress` bar
    /// on an older one.
    ///
    /// **Falsified** by building the lines with `build_unnegotiated` (the
    /// old-shell arm then carries `Sparkline`s it cannot decode), or by
    /// dropping a line's fixed top.
    #[test]
    fn the_history_lines_are_sparklines_on_the_native_axes() {
        let lines = |vocab| {
            nodes_of(&page_at(vocab, Card::bar_default(), &busy(), ALL_OPEN))
                .into_iter()
                .filter_map(|n| match n {
                    Node::Sparkline { id, max, .. } => Some((id.clone().unwrap(), *max)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines(SPARKLINE_VOCAB),
            vec![
                ("stats-panel-cpu-history".to_owned(), Some(1.0)),
                ("stats-panel-clock-history".to_owned(), Some(1.0)),
                ("stats-panel-gpu-usage-history".to_owned(), Some(1.0)),
                ("stats-panel-gpu-vram-history".to_owned(), Some(1.0)),
                ("stats-panel-gpu-temp-history".to_owned(), None),
                ("stats-panel-memory-history".to_owned(), Some(1.0)),
                ("stats-panel-disk-io-history".to_owned(), None),
            ],
        );
        assert!(
            lines(SPARKLINE_VOCAB - 1).is_empty(),
            "an older shell is never sent a variant it cannot decode",
        );
        let old = page_at(SPARKLINE_VOCAB - 1, Card::bar_default(), &busy(), ALL_OPEN);
        assert!(
            nodes_of(&old)
                .iter()
                .any(|n| id_of(n) == Some("stats-panel-cpu-history")
                    && matches!(n, Node::Progress { .. })),
            "…it gets the fallback bar under the same id",
        );
    }

    /// Every id on the page is unique — a node's id is its reconciler key, and
    /// two sharing one is a widget flickering between two states.
    #[test]
    fn every_id_on_the_page_is_unique() {
        let node = page(Card::bar_default(), &busy());
        let mut ids: Vec<&str> = nodes_of(&node).into_iter().filter_map(id_of).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "duplicate ids on the page");
    }

    /// Every card can be switched off on its own, and switching one off removes
    /// exactly its own `boxed-list`; a column left with no card is dropped.
    #[test]
    fn each_switch_removes_exactly_its_own_card() {
        let cards = |cfg: Card| {
            layout(&page(cfg, &busy()))
                .into_iter()
                .flat_map(|(_, cards)| cards)
                .collect::<Vec<_>>()
        };
        let all = cards(Card::bar_default());
        assert_eq!(all.len(), 4);
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
            let got = cards(off);
            assert!(!got.iter().any(|c| c == name), "{name}");
            assert_eq!(got.len(), all.len() - 1, "{name} removed more than itself");
        }

        // CPU and GPU both off: the left column goes, the right keeps its id.
        let right_only = layout(&page(
            Card {
                cpu: false,
                gpu: false,
                ..Card::bar_default()
            },
            &busy(),
        ));
        assert_eq!(right_only.len(), 1);
        assert_eq!(right_only[0].0, "stats-panel-col-1");
    }

    /// A machine with no GPU draws no GPU card at all, the native rule.
    #[test]
    fn a_machine_with_no_gpu_draws_no_gpu_card() {
        let snapshot = Snapshot {
            gpu: None,
            ..busy()
        };
        let node = page(Card::bar_default(), &snapshot);
        assert!(
            !nodes_of(&node)
                .into_iter()
                .filter_map(id_of)
                .any(|id| id.contains("gpu")),
        );
    }

    /// Swap, the Clock line and the GPU's usage / VRAM / temperature lines each
    /// hide exactly the way the native row they mirror hides.
    #[test]
    fn the_optional_rows_hide_like_their_native_ones() {
        let original = busy();
        let bare = Snapshot {
            memory: Some(Memory {
                swap_used: 0,
                swap_total: 0,
                ..original.memory.unwrap()
            }),
            cpu_clock_hz: None,
            cpu_clock_ceiling_hz: None,
            gpu: original.gpu.clone().map(|g| Gpu {
                load: None,
                temperature_c: None,
                memory_used_bytes: None,
                memory_total_bytes: None,
                ..g
            }),
            ..original
        };
        let ids: Vec<String> = nodes_of(&page(Card::bar_default(), &bare))
            .into_iter()
            .filter_map(id_of)
            .map(ToOwned::to_owned)
            .collect();
        for gone in [
            "stats-panel-swap-row",
            "stats-panel-clock-history-row",
            "stats-panel-gpu-usage-history-row",
            "stats-panel-gpu-vram-history-row",
            "stats-panel-gpu-temp-history-row",
        ] {
            assert!(!ids.iter().any(|id| id == gone), "{gone} in {ids:?}");
        }
        // …while the rows with no hide rule stay.
        for kept in [
            "stats-panel-gpu-row",
            "stats-panel-memory-row",
            "stats-panel-processes-row",
        ] {
            assert!(ids.iter().any(|id| id == kept), "{kept} in {ids:?}");
        }
    }

    /// `temperature = false` takes every `°C` reading off the page — the CPU
    /// row's suffix, the GPU row's (which falls back to the load, as native
    /// does with no probe) and the GPU temperature line.
    #[test]
    fn the_temperature_switch_governs_every_reading() {
        let node = page(
            Card {
                temperature: false,
                ..Card::bar_default()
            },
            &busy(),
        );
        let texts = texts(&node);
        assert!(!texts.iter().any(|t| t.contains('\u{00b0}')), "{texts:?}");
        assert!(
            texts.iter().filter(|t| *t == "37%").count() >= 2,
            "the GPU row's suffix falls back to the load: {texts:?}",
        );
        assert!(
            !nodes_of(&node)
                .into_iter()
                .filter_map(id_of)
                .any(|id| id == "stats-panel-gpu-temp-history"),
        );
    }

    /// The exact strings the page prints for [`busy`] — the mirror of the
    /// native page's own format strings, asserted as literals so a change to
    /// either side of the mirror is visible here (#1026).
    #[test]
    fn the_page_prints_the_native_pages_strings() {
        let texts = texts(&page(Card::bar_default(), &busy()));
        for want in [
            "CPU",
            "42%",
            "61 °C",
            "Processes",
            "287",
            "Clock",
            "3.8 GHz",
            "Memory",
            "11.2 GiB / 31.2 GiB (36%)",
            "36%",
            "Swap",
            "1.0 GiB / 8.0 GiB (12%)",
            "GPU",
            "Test Adapter",
            "52 °C",
            "GPU usage",
            "37%",
            "GPU VRAM",
            "25%",
            "GPU temp",
            "Disk",
            "2 mount(s)",
            "/",
            "37.3 GiB / 93.1 GiB (40%)",
            "/home",
            "679.9 GiB / 931.3 GiB (73%)",
            "Disk I/O",
            "\u{2193} 2.0 MiB/s \u{2191} 1.0 MiB/s",
            "min 3.0 MiB/s \u{00b7} max 3.0 MiB/s",
            "total \u{2193} 100.0 GiB \u{2191} 50.0 GiB",
        ] {
            assert!(texts.iter().any(|t| t == want), "{want:?} in {texts:?}");
        }
        // The per-core header row went with the lamp panel it introduced.
        assert!(!texts.iter().any(|t| t == "Per-core"), "{texts:?}");
    }

    /// The seed render — nothing sampled yet — dashes rather than inventing
    /// numbers, and still draws every card's skeleton.
    #[test]
    fn the_seed_render_is_dashes_not_invented_numbers() {
        let node = with_negotiated_vocab(SPARKLINE_VOCAB, || {
            panel(
                Card::bar_default(),
                &Snapshot::default(),
                &History::default(),
                &TopApps::default(),
                Expanded::default(),
            )
        });
        let texts = texts(&node);
        assert!(texts.iter().filter(|t| *t == "—").count() >= 3, "{texts:?}");
        assert!(!texts.iter().any(|t| t == "0%"), "{texts:?}");
    }

    /// The Disk card is a collapsed expander until the plugin says otherwise:
    /// the flag the model holds is the one the node carries, under the id the
    /// plugin's `update` toggles on.
    #[test]
    fn the_mounts_live_in_the_disk_expander() {
        for expanded in [false, true] {
            // The two Top apps lists the other way round, so the Disk card is
            // shown to read its own flag and not a sibling's.
            let flags = Expanded {
                disks: expanded,
                top_cpu: !expanded,
                top_ram: !expanded,
            };
            let node = page_at(SPARKLINE_VOCAB, Card::bar_default(), &busy(), flags);
            let expander = nodes_of(&node)
                .into_iter()
                .find_map(|n| match n {
                    Node::Expander {
                        id,
                        expanded,
                        children,
                        ..
                    } if id == DISKS_EXPANDER_ID => Some((id.clone(), *expanded, children.len())),
                    _ => None,
                })
                .expect("the Disk card has an expander");
            assert_eq!(expander, (DISKS_EXPANDER_ID.to_owned(), expanded, 2));
        }

        // An empty mount list says so inside the expander rather than opening
        // onto nothing.
        let empty = page(
            Card::bar_default(),
            &Snapshot {
                disks: Vec::new(),
                ..busy()
            },
        );
        let texts = texts(&empty);
        assert!(texts.iter().any(|t| t == "No mounts"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "0 mount(s)"), "{texts:?}");
    }

    /// The shell's own classes ride the page — the `ts-*` tint contract Annika
    /// asked for on #1235 ("the skins carry over"), the native history row's
    /// `ts-history-row` / `ts-stat-name` / `ts-stat-value`, libadwaita's row
    /// `header`, the progress bars' `ts-stat-progress` — plus the two additive
    /// rules this page brought (`ts-stat-col`, `ts-stat-detail`).
    ///
    /// **Falsified** by dropping any of them from its helper.
    #[test]
    fn the_page_keeps_the_shells_own_classes() {
        let got = classes(&page(Card::bar_default(), &busy()));
        for want in [
            "ts-cpu",
            "ts-cpu-temp",
            "ts-gpu",
            "ts-gpu-temp",
            "ts-memory",
            "ts-disk",
            "ts-stat-progress",
            "ts-history-row",
            "ts-stat-name",
            "ts-stat-value",
            "ts-stat-col",
            "ts-stat-detail",
            "header",
            "subtitle",
            "boxed-list",
            "ts-page-card",
            "ts-page-expander",
            HOMOGENEOUS_CLASS,
        ] {
            assert!(got.iter().any(|c| c == want), "{want} missing: {got:?}");
        }
    }

    /// **Each line carries its own reading** — not merely a reading of the
    /// right length (#1414 review, MEDIUM 5). The bounded-and-withheld test
    /// below counts samples; this one reads them, which is what kills a memory
    /// line plotting swap, an inverted CPU line, a NaN temperature pushed as a
    /// 0 °C dip, and a disk "min" that is really the running max.
    ///
    /// **Falsified** by each of: `fraction(m.swap_used, m.swap_total)` in the
    /// memory arm, `1.0 - cpu` in the CPU arm, dropping the `is_finite` filter
    /// on the GPU temperature, and `lo.max(v)` in `disk_io_range`.
    #[test]
    fn each_line_carries_its_own_reading() {
        let mut h = History::default();
        h.push(&busy());
        assert_eq!(h.cpu.back().copied(), Some(0.42));
        let memory = h.memory.back().copied().expect("a memory point");
        assert!(
            (memory - 11_999_999_000.0_f32 / 33_500_000_000.0).abs() < 1e-6,
            "memory used over total, not swap: {memory}",
        );
        assert_eq!(h.gpu_load.back().copied(), Some(0.37));
        assert_eq!(h.gpu_vram.back().copied(), Some(0.25));
        assert_eq!(h.gpu_temp.back().copied(), Some(52.0));

        let mut nan = busy();
        nan.gpu.as_mut().expect("busy has a GPU").temperature_c = Some(f32::NAN);
        h.push(&nan);
        assert_eq!(h.gpu_temp.len(), 1, "a NaN temperature is not a sample");

        let mut quiet = busy();
        quiet
            .disk_io
            .as_mut()
            .expect("busy reads disk I/O")
            .read_bps = 0.0;
        h.push(&quiet);
        assert_eq!(h.disk_io_range(), (1_048_576.0, 3_145_728.0));
    }

    /// The history rings keep the native rows' sixty samples, and a withheld
    /// reading is not a sample — the rule the sidebar card's scope ring
    /// follows for a cold `/proc/stat` tick.
    ///
    /// **Falsified** by dropping `push_capped`'s `pop_front` (the rings grow
    /// past sixty), or by pushing a point for a `None` reading.
    #[test]
    fn the_history_is_bounded_and_skips_withheld_readings() {
        let mut history = History::default();
        for _ in 0..(HISTORY_LEN * 3) {
            history.push(&busy());
        }
        for (name, ring) in [
            ("cpu", &history.cpu),
            ("clock", &history.clock),
            ("memory", &history.memory),
            ("gpu_load", &history.gpu_load),
            ("gpu_vram", &history.gpu_vram),
            ("gpu_temp", &history.gpu_temp),
            ("disk_io", &history.disk_io),
        ] {
            assert_eq!(ring.len(), HISTORY_LEN, "{name}");
        }

        let mut cold = History::default();
        cold.push(&Snapshot::default());
        assert_eq!(cold, History::default(), "nothing measured, nothing drawn");

        // The clock is on the native row's fixed axis: max over the ceiling.
        let mut one = History::default();
        one.push(&busy());
        let clock = one.clock.back().copied().expect("one clock point");
        assert!(
            (clock - 0.76).abs() < 1e-6,
            "3.8 GHz of a 5 GHz ceiling: {clock}"
        );
        assert_eq!(
            one.disk_io.back().map(|v| v.to_bits()),
            Some(3_145_728.0_f32.to_bits()),
            "read + write, in bytes/s",
        );
    }

    // ── Top apps · CPU / RAM (#1419 item 3) ──────────────────────────────────

    /// The expander `id` on `node`: its header's texts, its body and its flag.
    fn top_apps_list<'a>(node: &'a Node, want: &str) -> (Vec<String>, &'a [Node], bool) {
        nodes_of(node)
            .into_iter()
            .find_map(|n| match n {
                Node::Expander {
                    id,
                    header,
                    children,
                    expanded,
                    ..
                } if id == want => Some((texts(header), children.as_slice(), *expanded)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no expander {want} on the page"))
    }

    /// The id of the last row of the card `card`.
    fn last_row_of(node: &Node, card: &str) -> String {
        nodes_of(node)
            .into_iter()
            .find_map(|n| match n {
                Node::ListBox { id, children, .. } if id.as_deref() == Some(card) => {
                    children.last().and_then(id_of).map(ToOwned::to_owned)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no card {card} on the page"))
    }

    /// **Where the lists sit and how they start**: "Top apps · CPU" is the
    /// CPU card's last row and "Top apps · RAM" the Memory card's — where
    /// native `build_stats_cpu_card` / `build_stats_memory_card` add them —
    /// both collapsed by default, and each open exactly when its own flag is.
    ///
    /// **Falsified** by hard-wiring either `expanded` to `true`, by swapping
    /// the two flags, or by moving a list out of its card.
    #[test]
    fn the_top_apps_lists_close_the_cpu_and_memory_cards_collapsed() {
        let seed = page_at(
            SPARKLINE_VOCAB,
            Card::bar_default(),
            &busy(),
            Expanded::default(),
        );
        assert_eq!(last_row_of(&seed, "stats-panel-cpu"), TOP_APPS_CPU_ID);
        assert_eq!(last_row_of(&seed, "stats-panel-memory"), TOP_APPS_RAM_ID);
        assert!(
            !top_apps_list(&seed, TOP_APPS_CPU_ID).2,
            "collapsed, as native"
        );
        assert!(
            !top_apps_list(&seed, TOP_APPS_RAM_ID).2,
            "collapsed, as native"
        );

        for (top_cpu, top_ram) in [(true, false), (false, true)] {
            let node = page_at(
                SPARKLINE_VOCAB,
                Card::bar_default(),
                &busy(),
                Expanded {
                    disks: false,
                    top_cpu,
                    top_ram,
                },
            );
            assert_eq!(top_apps_list(&node, TOP_APPS_CPU_ID).2, top_cpu);
            assert_eq!(top_apps_list(&node, TOP_APPS_RAM_ID).2, top_ram);
        }

        // Each list rides its card's switch, as native: no CPU card, no CPU list.
        let no_cpu = page(
            Card {
                cpu: false,
                ..Card::bar_default()
            },
            &busy(),
        );
        assert!(
            !nodes_of(&no_cpu)
                .into_iter()
                .any(|n| id_of(n) == Some(TOP_APPS_CPU_ID))
        );
        assert_eq!(top_apps_list(&no_cpu, TOP_APPS_RAM_ID).1.len(), 2);
    }

    /// **The native strings**, as literals (#1026): the two titles, the
    /// collapsed summary `name · value`, `N processes` only for a group of
    /// more than one, the CPU value as native's `{:.0}%` of the share and the
    /// RAM value as native's `fmt_bytes`, heaviest first.
    ///
    /// **Falsified** by `{:.1}%`, by a `1 processes` subtitle, by dropping the
    /// `·` summary for the bare name, or by the RAM list printing the share.
    #[test]
    fn the_top_apps_lists_print_the_native_strings() {
        let node = page(Card::bar_default(), &busy());

        let (header, rows, _) = top_apps_list(&node, TOP_APPS_CPU_ID);
        assert_eq!(header, vec!["Top apps · CPU", "org.mozilla.firefox · 42%"]);
        assert_eq!(
            rows.iter().map(texts).collect::<Vec<_>>(),
            vec![
                vec!["org.mozilla.firefox", "3 processes", "42%"],
                vec!["System", "7%"],
            ],
        );

        let (header, rows, _) = top_apps_list(&node, TOP_APPS_RAM_ID);
        assert_eq!(
            header,
            vec!["Top apps · RAM", "org.mozilla.firefox · 3.2 GiB"]
        );
        assert_eq!(
            rows.iter().map(texts).collect::<Vec<_>>(),
            vec![
                vec!["org.mozilla.firefox", "3 processes", "3.2 GiB"],
                vec!["pipewire", "50.0 MiB"],
            ],
        );

        // Each row's hover is the group's full name, as native.
        let tooltips: Vec<Option<&str>> = rows
            .iter()
            .map(|row| match row {
                Node::Row { tooltip, .. } => tooltip.as_deref(),
                other => panic!("a row is a Row: {other:?}"),
            })
            .collect();
        assert_eq!(
            tooltips,
            vec![Some("org.mozilla.firefox"), Some("pipewire")]
        );
    }

    /// Nothing measured — both lists parked, or the page's first render — is
    /// a dash beside each title and no rows, not an invented app; and a cold
    /// walk, which carries the RAM list but withholds the CPU one, draws
    /// exactly that.
    #[test]
    fn nothing_measured_reads_as_a_dash_beside_each_title() {
        let empty = with_negotiated_vocab(SPARKLINE_VOCAB, || {
            panel(
                Card::bar_default(),
                &busy(),
                &warm(),
                &TopApps::default(),
                ALL_OPEN,
            )
        });
        for id in [TOP_APPS_CPU_ID, TOP_APPS_RAM_ID] {
            let (header, rows, _) = top_apps_list(&empty, id);
            assert_eq!(header[1], "\u{2014}", "{id}");
            assert!(rows.is_empty(), "{id}");
        }

        let cold = with_negotiated_vocab(SPARKLINE_VOCAB, || {
            panel(
                Card::bar_default(),
                &busy(),
                &warm(),
                &TopApps {
                    by_cpu: Vec::new(),
                    ..busy_apps()
                },
                ALL_OPEN,
            )
        });
        assert_eq!(top_apps_list(&cold, TOP_APPS_CPU_ID).0[1], "\u{2014}");
        assert_eq!(
            top_apps_list(&cold, TOP_APPS_RAM_ID).0[1],
            "org.mozilla.firefox · 3.2 GiB",
        );
    }

    /// **The row cap**: a list never draws more rows than the walker keeps —
    /// six, native's `TOP_N` — whatever reaches the model.
    ///
    /// **Falsified** by dropping the `.take(TOP_N)` in `top_apps_expander`.
    #[test]
    fn a_top_apps_list_draws_at_most_six_rows() {
        let many: Vec<ProcSample> = (0..10_u32)
            .map(|i| app(&format!("app{i}"), 0.01, u64::from(10 - i) << 20))
            .collect();
        let node = with_negotiated_vocab(SPARKLINE_VOCAB, || {
            panel(
                Card::bar_default(),
                &busy(),
                &warm(),
                &TopApps {
                    by_cpu: many.clone(),
                    by_mem: many,
                },
                ALL_OPEN,
            )
        });
        assert_eq!(TOP_N, 6, "the walker's own cap, as native");
        for id in [TOP_APPS_CPU_ID, TOP_APPS_RAM_ID] {
            let (_, rows, _) = top_apps_list(&node, id);
            assert_eq!(rows.len(), 6, "{id}");
            assert_eq!(texts(&rows[0])[0], "app0", "heaviest first, in walk order");
        }
    }

    /// **No icon, and no placeholder for one** — the icon column is pending
    /// #1419's question. A list's rows are text all the way down.
    #[test]
    fn the_top_apps_rows_carry_no_icon() {
        let node = page(Card::bar_default(), &busy());
        for id in [TOP_APPS_CPU_ID, TOP_APPS_RAM_ID] {
            let (_, rows, _) = top_apps_list(&node, id);
            for row in rows {
                for n in nodes_of(row) {
                    assert!(
                        matches!(
                            n,
                            Node::Row { .. }
                                | Node::Label { .. }
                                | Node::Text { .. }
                                | Node::Spacer
                        ),
                        "{id}: {n:?}",
                    );
                }
            }
        }
    }
}
