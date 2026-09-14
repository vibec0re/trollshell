//! The card's geometry and its one piece of real translation: **per-core load
//! as dot-matrix lamps**.
//!
//! # Why the lamp row is a `DotMatrix` and not an [`LedMatrix`]
//!
//! [`LedMatrix`]: hytte_plugin::preem::LedMatrix
//!
//! The native Stats page draws its per-core `BlinkenLichten` with
//! `hytte_preem::LedMatrix` (#857) — a grid of independently-lit lamps, each at
//! its own core's intensity — rasterised in-process into a `PixelSurface`. That
//! type is **not on the wire**: `PreemWidget` has eight variants and a lamp grid
//! is not one of them, which is exactly what #1156 exists to change. So P1
//! expresses the row in the vocabulary that *does* exist.
//!
//! The closest honest thing the wire can say is a [`DotMatrix`] row with one
//! **glyph cell** per core, picked from a ramp that is monotone in lit dots:
//! the cell is a 5×7 dot grid, and the busier the core the more of its dots are
//! lit. It is the same dot hardware at the same pitch, drawn by the same
//! shell-side renderer, on the GPU (`DotMatrix` is one of the five kinds with a
//! GL arm) — it just cannot address a lamp individually, so a core's intensity
//! is quantised to [`LAMPS`]`.len()` steps instead of being continuous.
//!
//! The ramp is calibrated against the kit's own font, not by eye:
//! `the_lamp_ramp_is_monotone_in_lit_dots` counts the set bits of each glyph in
//! `hytte_plugin::preem::font` and pins both the order and the measured counts,
//! so a change to a glyph bitmap that broke the ramp's monotonicity would be
//! caught here rather than looked at on glass.
//!
//! # Wrapping, and the machine this does not fit
//!
//! A row is `dot_px * (6n + 1)` buffer pixels wide (`DotMatrix::render`'s own
//! formula), so a 16-core row at pitch 3 is 291 px — inside the ~296 px sidebar
//! card, and the reason [`CORES_PER_ROW`] is 16. Past that the lamps wrap onto
//! further rows, which is also what makes the panel read as a *grid* rather than
//! a 700 px strip. [`dot_px_for`] picks the chunkiest pitch that still fits, so
//! a 4-core laptop gets fat lamps and a 32-thread desktop gets two tight rows.
//!
//! # There are no icons on the chips, and that is a gap
//!
//! The five native bar chips each lead with a bundled SVG
//! (`assets/trollshell/icons/{cpu,memory,disk,gpu,emblem-system}.svg`, loaded
//! with `gtk::Image::from_file`). A plugin cannot ask for those: [`Node::Icon`]
//! carries an **icon-theme name**, the host maps it straight onto
//! `gtk::Image::set_icon_name`, and the shell installs its SVGs under
//! `share/trollshell/icons/` — a data directory, not an icon theme. Adwaita
//! itself ships `drive-harddisk-symbolic` and nothing at all for a CPU, a GPU
//! or RAM, so four of the five would render `image-missing` and the fifth would
//! not match the others.
//!
//! So each chip leads with a short uppercase **word** instead (`CPU`, `MEM`,
//! `DSK`, `GPU`). That is a visible difference from the native chips and it is
//! named in the PR rather than papered over; closing it is a shell-side or
//! wire-side change (install the five SVGs into an icon theme the host can look
//! up, or grow the vocabulary), neither of which is in this plugin's lane.
//!
//! # The `ts-*` classes
//!
//! The card sets the shell's own `ts-cpu` / `ts-cpu-temp` / `ts-gpu` classes.
//! The SDK's *Styling* docs say not to copy shell-internal classes off a native
//! widget — and then name the exception this crate is in: `hytte-plugin-weather`
//! and `hytte-plugin-departures` are 1:1 ports of what used to be native chips
//! and keep the classes for pixel parity. This is that same move (P2/P3 of
//! #1248 retire the native chips and page), and Annika's ask on #1235 was
//! explicitly that "the skins carry over".

use hytte_plugin::display::{DotMatrix, Gauge, LedStrip, Scope, SevenSeg, StyleName};
use hytte_plugin::proto::preem::{MAX_DOT_PX, MIN_DOT_PX};
use hytte_plugin::proto::{Cls, Dir, Node};

use crate::format;
use crate::sample::{Disk, Gpu, Memory, Snapshot};

/// The brightness ramp, dimmest first — one glyph per lamp step.
///
/// Five steps of roughly four lit dots each, measured against the kit's 5×7
/// font: `.` 4, `:` 8, `o` 12, `O` 16, `0` 19. An idle core is a single dot
/// rather than a blank cell on purpose — a blank reads as "this core is gone",
/// and an idle core is very much still there.
pub const LAMPS: [char; 5] = ['.', ':', 'o', 'O', '0'];

/// The index of the brightest lamp, as a float — the multiplier [`lamp`] scales
/// a `0.0..=1.0` load by.
///
/// A literal rather than `(LAMPS.len() - 1) as f32`, which is a
/// precision-losing cast the workspace's pedantic lints refuse; the
/// `const` assertion below is what keeps the two in step, at compile time.
const TOP_LAMP: f32 = 4.0;
const _: () = assert!(LAMPS.len() == 5, "TOP_LAMP is LAMPS.len() - 1");

/// Lamps per row before wrapping. Sixteen both fits the card at a legible pitch
/// (see the module doc) and is the bank size real lamp panels come in.
pub const CORES_PER_ROW: usize = 16;

/// The width budget a row is fitted to, in buffer pixels: the sidebar card the
/// kit's own sizing notes are written against ("the default `LedStrip::leds`
/// count of 24 renders 269 px wide — inside the ~296 px sidebar card").
pub const CARD_PX: u32 = 296;

/// The width budget the **drawer page**'s per-core row is fitted to, in buffer
/// pixels.
///
/// The drawer clamps at `DRAWER_MAX_WIDTH = 680` (the shell's
/// `components/layout.rs`) and the host's own panel chrome takes some of that,
/// so 640 is the honest budget. This is the whole reason the page holds a
/// *second* `DotMatrix`: "the P1 row at full width" (#1251) means the row is
/// fitted to the page it is on, and a 16-wide row admits a 6 px pitch here
/// against the sidebar card's 3 px.
pub const PAGE_PX: u32 = 640;

/// The dot pitch every **bar** lamp is drawn at.
///
/// Fixed rather than fitted: a bar chip's budget is its *height*, not its
/// width, and at the kit's floor a strip is `9 * dot_px` = 18 px tall — between
/// the native chip's 14 px fill bar and its 16 px icon. Fitting to a width
/// budget the way the card does would make a one-lamp chip draw a 8 px dot and
/// blow the bar's height out.
pub const CHIP_DOT_PX: u32 = MIN_DOT_PX;

/// How many lamp cells the placeholder row shows before the first delta is
/// available. `/proc/stat` is cumulative, so the very first tick has no
/// previous reading to subtract and reports no per-core loads at all; the row
/// shows dashes for that one poll period rather than a wrong number or a
/// disappearing widget.
const DASH_CELLS: usize = 4;

/// The skin every widget on this card is drawn in.
///
/// VFD: a near-black field with a phosphor halo off every lit dot — the
/// blinken-lichten look, and the skin `core-leds.toml` already defaults the
/// native panel to (#857/#869). Not a config key here: the epic has
/// `core-leds.toml`'s ownership moving to this plugin in P3, and inventing a
/// second spelling for the same knob in P1 would be the thing that migration
/// then has to undo.
const SKIN: StyleName = StyleName::Vfd;

/// The lamp for one `0.0..=1.0` load.
///
/// Monotone by construction, and total: a load outside the range is clamped and
/// a `NaN` takes the dimmest lamp (the saturating float→int cast maps it to
/// `0`). The sampler already sanitises every load it publishes — this is the
/// second belt, because `view` must never be the thing that panics.
#[must_use]
pub fn lamp(load: f32) -> char {
    // The product is `0.0..=TOP_LAMP` and the cast saturates — so the index
    // cannot be out of bounds for any input at all, `NaN` included.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let idx = (load.clamp(0.0, 1.0) * TOP_LAMP).round() as usize;
    LAMPS[idx.min(LAMPS.len() - 1)]
}

/// The lamp rows for a set of per-core loads, wrapped at [`CORES_PER_ROW`].
///
/// An empty reading yields exactly one row of dashes — see [`DASH_CELLS`].
#[must_use]
pub fn lamp_rows(loads: &[f32]) -> Vec<String> {
    if loads.is_empty() {
        return vec!["-".repeat(DASH_CELLS)];
    }
    loads
        .chunks(CORES_PER_ROW)
        .map(|chunk| chunk.iter().copied().map(lamp).collect())
        .collect()
}

/// How many glyph cells the **widest row** of a set of per-core loads holds —
/// which is the number the dot pitch must be fitted to, and the only number
/// [`dot_px_for`] will give a useful answer for.
///
/// [`lamp_rows`] wraps at [`CORES_PER_ROW`], so no row is ever wider than that:
/// feeding the *total* core count to `dot_px_for` computes the width of a row
/// that does not exist, and past sixteen threads the division underflows and
/// the clamp lifts it to the floor. A 32-thread desktop then draws its two tight
/// rows at two-thirds of the pitch the card admits — the exact machine the
/// module doc's worked example names (#1277 MEDIUM 1).
#[must_use]
pub fn row_cells(loads: &[f32]) -> usize {
    if loads.is_empty() {
        DASH_CELLS
    } else {
        loads.len().min(CORES_PER_ROW)
    }
}

/// The chunkiest dot pitch at which a row of `cells` glyphs still fits
/// [`CARD_PX`], floored at the kit's own [`MIN_DOT_PX`].
///
/// `cells` means **cells in one row** — see [`row_cells`], which is the only
/// thing that should be computing it.
///
/// `DotMatrix::render`'s width is `2*pad + n*advance - SPACING*dot`, and the
/// bezel is one dot cell on each side, which collapses to `dot_px * (6n + 1)`.
/// The floor is what a machine with more cores than the card is wide gets: the
/// row is then wider than its card and the host clips it, which is honest and
/// is precisely the case #1156's lamp *grid* solves.
#[must_use]
pub fn dot_px_for(cells: usize) -> u32 {
    dot_px_for_in(CARD_PX, cells)
}

/// [`dot_px_for`] against an arbitrary width budget — the sidebar card's
/// [`CARD_PX`] or the drawer page's [`PAGE_PX`].
///
/// One function with the budget as a parameter rather than two transcriptions
/// of `dot_px * (6n + 1)`: a second copy of a width formula is exactly the
/// mirror the kit's own sizing notes warn about, and #1277's MEDIUM 1 was a
/// *caller* feeding this the wrong `cells` — a second body would have been a
/// second place for that to happen.
#[must_use]
pub fn dot_px_for_in(budget: u32, cells: usize) -> u32 {
    let cells = u32::try_from(cells.max(1)).unwrap_or(u32::MAX);
    // `6 * cells` can only overflow for a core count no machine has and
    // `saturating_mul` keeps this total anyway; the division then yields 0 and
    // the clamp lifts it to the floor.
    let per_row = cells.saturating_mul(6).saturating_add(1);
    (budget / per_row).clamp(MIN_DOT_PX, MAX_DOT_PX)
}

/// The seven-segment text for a temperature reading. `--` when no sensor
/// answered — the readout stays on the card rather than vanishing, so the row
/// does not reflow the moment a chip is hotplugged away.
#[must_use]
pub fn temp_text(celsius: Option<f32>) -> String {
    match celsius {
        // The kit's seven-segment cells are digits, `:`, `-` and space, so a
        // degree sign is not available here — the unit is the `°C` label beside
        // the readout instead. A reading is rounded to whole degrees: the
        // sensors report millidegrees and a tenth of a degree on a three-digit
        // seven-segment display is noise that costs a whole cell.
        Some(c) if c.is_finite() => format!("{c:.0}"),
        _ => "--".to_owned(),
    }
}

/// A `0.0..=1.0` load as a whole-percent label, or an em dash when the vendor
/// exposes no counter.
#[must_use]
pub fn percent_text(load: Option<f32>) -> String {
    match load {
        Some(v) if v.is_finite() => format!("{:.0}%", v.clamp(0.0, 1.0) * 100.0),
        _ => "—".to_owned(),
    }
}

/// The CPU chip's hover text — the native `widgets/cpu.rs` line
/// (`format!("CPU {:.0}%", c.overall * 100.0)`), with this plugin's `—` for a
/// reading it has not taken yet.
#[must_use]
pub fn cpu_tooltip(load: Option<f32>) -> String {
    format!("CPU {}", percent_text(load))
}

/// The memory chip's hover text.
///
/// Both arms are the native `widgets/memory.rs` literals, including the fact
/// that the unknown arm carries a colon and the percent arm does not — copied
/// rather than tidied, because the point of this chip is that it reads like the
/// one it replaces.
#[must_use]
pub fn memory_tooltip(memory: Option<&Memory>) -> String {
    match memory.filter(|m| m.total > 0) {
        Some(m) => format!("Memory {:.0}%", format::fraction(m.used, m.total) * 100.0),
        None => "Memory: unknown".to_owned(),
    }
}

/// The disk chip's hover text: the native per-bar line
/// (`format!("{}: {:.0}%", m.path, m.usage * 100.0)`) for every mount, joined.
///
/// The native chip builds one `gtk::ProgressBar` per mount and puts that string
/// on **each**. The wire has no per-cell tooltip — a `Node::Preem` carries no
/// tooltip field at all, and the lamps for every mount are one node — so the
/// whole set lands on the chip instead. That is a real divergence and the
/// reason this function exists rather than being inlined.
#[must_use]
pub fn disk_tooltip(disks: &[Disk]) -> String {
    if disks.is_empty() {
        return "No mounts".to_owned();
    }
    disks
        .iter()
        .map(|d| format!("{}: {:.0}%", d.path, d.usage.clamp(0.0, 1.0) * 100.0))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The GPU chip's hover text — the native `widgets/gpu.rs` three-way branch,
/// including the bare adapter name when the vendor exposes no busy counter and
/// the literal `"GPU"` when there is no adapter at all.
#[must_use]
pub fn gpu_tooltip(gpu: Option<&Gpu>) -> String {
    match gpu {
        Some(g) => match g.load {
            Some(l) => format!("{}: {:.0}%", g.name, l.clamp(0.0, 1.0) * 100.0),
            None => g.name.clone(),
        },
        None => "GPU".to_owned(),
    }
}

/// A `0.0..=1.0` load as a scope sample in the wire's `-1.0..=1.0` range:
/// an idle core rests on the bottom of the trace and a saturated one touches
/// the top, rather than the trace hugging the centre line at half amplitude.
#[must_use]
pub fn trace_sample(load: f32) -> f32 {
    load.clamp(0.0, 1.0).mul_add(2.0, -1.0)
}

/// One CSS class, as the wire carries it.
pub(crate) fn cls(name: &str) -> Vec<Cls> {
    vec![name.to_owned()]
}

/// A plain label.
pub(crate) fn label(text: impl Into<String>, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.into(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        tooltip: None,
    }
}

/// A section header row: a name on the left, a reading on the right.
pub(crate) fn header(
    name: &str,
    reading: String,
    reading_class: &str,
    tooltip: Option<String>,
) -> Node {
    Node::Row {
        id: None,
        classes: Vec::new(),
        spacing: 6,
        children: vec![
            label(name, &["caption-heading", "dim-label"]),
            Node::Spacer,
            Node::Label {
                id: None,
                text: reading,
                classes: vec!["numeric".to_owned(), reading_class.to_owned()],
                tooltip: None,
            },
        ],
        tooltip,
    }
}

/// The stateful preem widgets the card holds across renders.
///
/// They live in the model rather than being rebuilt per `view` because the
/// wrappers carry animation state in raster mode (the gauge's needle spring,
/// the scope's phosphor) **and** because a rebuilt widget is a config change on
/// the wire, which makes the shell drop its renderer instance and start that
/// animation over. The one exception is [`Widgets::cores`], which is rebuilt
/// deliberately when the core count changes — see [`Widgets::fit_cores`].
#[derive(Debug)]
pub struct Widgets {
    cores: DotMatrix,
    /// The cell count `cores` was last fitted to, so the rebuild happens once
    /// rather than on every render.
    fitted_cells: usize,
    /// The drawer page's per-core row — the same lamps fitted to [`PAGE_PX`]
    /// instead of [`CARD_PX`], which is what "the P1 row at full width" means.
    page_cores: DotMatrix,
    page_fitted_cells: usize,
    /// Every **bar** lamp, and every disk lamp on the **sidebar card**: one
    /// pitch, many nodes. A `DotMatrix` wrapper carries no per-node state —
    /// the id, the classes and the text are all arguments to `node_classed` —
    /// so one value renders the CPU chip's lamp, the memory chip's, the GPU
    /// chip's and the sidebar card's disk row without them sharing anything
    /// but their pitch.
    lamps: DotMatrix,
    /// The drawer page's disk-lamp row, fitted to [`PAGE_PX`] like
    /// [`page_cores`] — #1295 review LOW 4: at the bar's fixed [`CHIP_DOT_PX`]
    /// floor a two-mount row is `2*(6*2+1) = 26 px` wide sitting under a
    /// per-core row six times that pitch, the same "fit the row to the
    /// surface it is on" argument [`PAGE_PX`]'s own doc makes.
    page_disk_lamps: DotMatrix,
    page_disk_fitted_cells: usize,
    temp: SevenSeg,
    history: Scope,
    /// The CPU chip's sweep when `[bar] history = true`: the same trace as
    /// `history`, sized for a bar rather than for a card.
    chip_history: Scope,
    gpu: Gauge,
    memory: LedStrip,
    swap: LedStrip,
    /// The drawer page's disk-I/O-history sweep (#1295 review MED 1) — the
    /// native Disks card's combined read+write rate row
    /// (`trollshell/src/panels/stats.rs:610` → `:1293`), auto-scaled against
    /// the peak rate this session has seen (`Stats::apply`) rather than the
    /// native row's windowed max: a simplification, named here because #1251's
    /// whole point is an honest list of where this page and the native one
    /// diverge.
    disk_io: Scope,
    /// The drawer page's GPU-VRAM-history sweep (#1295 review MED 1) — the
    /// native GPU card's "GPU VRAM" row (`stats.rs:632`).
    gpu_vram: Scope,
}

/// Logical width of the history sweep, in samples — one per poll tick, so at
/// the default cadence the trace is the last two and a bit minutes.
pub const HISTORY_COLS: u32 = 144;

impl Default for Widgets {
    fn default() -> Self {
        Self {
            cores: DotMatrix::new(SKIN).dot_px(dot_px_for(DASH_CELLS)),
            fitted_cells: DASH_CELLS,
            page_cores: DotMatrix::new(SKIN).dot_px(dot_px_for_in(PAGE_PX, DASH_CELLS)),
            page_fitted_cells: DASH_CELLS,
            lamps: DotMatrix::new(SKIN).dot_px(CHIP_DOT_PX),
            page_disk_lamps: DotMatrix::new(SKIN).dot_px(dot_px_for_in(PAGE_PX, DASH_CELLS)),
            page_disk_fitted_cells: DASH_CELLS,
            temp: SevenSeg::new(SKIN),
            // 144x32 at 2x is 288x64 — the card's width, and short enough that
            // the sweep reads as a strip beside the lamp row rather than
            // dominating the card.
            history: Scope::with_size(SKIN, HISTORY_COLS, 32).scale(2),
            // A bar chip has no room for the card's 288x64 sweep: 64x14 at 1x
            // is a ticker-tape strip the same height as the lamps beside it.
            chip_history: Scope::with_size(SKIN, 64, 14),
            // A 0..100 needle, sized to the card at 2x (224x112).
            gpu: Gauge::with_size(SKIN, 112, 56).scale(2).range(0.0, 100.0),
            // Twelve segments render 2*4 + 12*8 + 11*3 = 137 px — half the
            // sidebar card, and a comfortable meter in the drawer page beside
            // the `used / total` text that carries the exact numbers.
            memory: LedStrip::new(SKIN).leds(12),
            swap: LedStrip::new(SKIN).leds(12),
            // Both new page-only sweeps are the same footprint as `history` —
            // neither has a bar-sized twin, since neither row exists on a chip.
            disk_io: Scope::with_size(SKIN, HISTORY_COLS, 32).scale(2),
            gpu_vram: Scope::with_size(SKIN, HISTORY_COLS, 32).scale(2),
        }
    }
}

impl Widgets {
    /// Re-fit the lamp row to a fresh set of per-core loads, if the row it
    /// produces is a different width than the last one.
    ///
    /// Takes the **loads**, not a count, so the one number that matters here
    /// can only be computed one way: [`row_cells`] of the slice the row is
    /// actually drawn from. Handing this a core *total* is what #1277's MEDIUM 1
    /// was, and there is now no argument to hand it one with.
    ///
    /// Rebuilding the wrapper restates the widget's **config** on the wire, so
    /// the shell drops its renderer instance and builds a fresh one. That is
    /// free here and nowhere else on this card: `DotMatrix` is the one kind with
    /// no animation state to lose ("the static matrix has no animation of its
    /// own, so *animate toward the target* degenerates to an immediate redraw").
    /// The width changes at most twice in a session — once when the first delta
    /// arrives, and again only if the kernel hotplugs a CPU across the wrap
    /// boundary.
    pub fn fit_cores(&mut self, loads: &[f32]) {
        let cells = row_cells(loads);
        if cells != self.fitted_cells {
            self.cores = DotMatrix::new(SKIN).dot_px(dot_px_for(cells));
            self.fitted_cells = cells;
        }
        if cells != self.page_fitted_cells {
            self.page_cores = DotMatrix::new(SKIN).dot_px(dot_px_for_in(PAGE_PX, cells));
            self.page_fitted_cells = cells;
        }
    }

    /// Re-fit the drawer page's disk-lamp row to a fresh set of mount usages —
    /// [`fit_cores`](Self::fit_cores)'s twin for the Disks card (#1295 review
    /// LOW 4). The sidebar card's and the bar chip's disk lamps stay at the
    /// fixed [`CHIP_DOT_PX`] pitch (`self.lamps`); only the page's row is
    /// fitted to a width budget, for the same reason the per-core row's page
    /// copy is.
    pub fn fit_disk_lamps(&mut self, usages: &[f32]) {
        let cells = row_cells(usages);
        if cells != self.page_disk_fitted_cells {
            self.page_disk_lamps = DotMatrix::new(SKIN).dot_px(dot_px_for_in(PAGE_PX, cells));
            self.page_disk_fitted_cells = cells;
        }
    }

    /// Point the memory and swap meters, and tick their peak-hold dots.
    ///
    /// `advance` is a no-op while the host speaks preem — the shell owns the
    /// peak decay there — and is exactly the per-heartbeat tick a rasterising
    /// plugin writes, which is what keeps one code path serving both.
    pub fn set_memory(&mut self, memory: Option<&Memory>) {
        let (used, total, swap_used, swap_total) = memory.map_or((0, 0, 0, 0), |m| {
            (m.used, m.total, m.swap_used, m.swap_total)
        });
        self.memory.set_level(format::fraction(used, total));
        self.memory.advance();
        self.swap.set_level(format::fraction(swap_used, swap_total));
        self.swap.advance();
    }

    /// Point the GPU needle at a load, and tick the local animation.
    ///
    /// `advance` is a **no-op while the host speaks preem** — the shell owns the
    /// needle spring and runs it on its own frame clock — so this call is what
    /// keeps one code path serving both a preem host and an older one, where it
    /// is exactly the per-heartbeat tick a rasterising plugin writes today.
    pub fn set_gpu(&mut self, load: Option<f32>, dt: f32) {
        self.gpu
            .set_target(load.unwrap_or(0.0).clamp(0.0, 1.0) * 100.0);
        self.gpu.advance(dt);
    }

    /// Stamp a fresh history batch onto the scope.
    ///
    /// The whole ring, not the newest sample: the wire's scope state is
    /// *consumed* rather than held — each batch is drawn over the decaying
    /// phosphor — so a one-sample batch would draw one point, not a sweep.
    pub fn push_history(&mut self, ring: &[f32]) {
        self.history.push(ring);
        self.chip_history.push(ring);
    }

    /// Stamp a fresh batch onto the drawer page's disk-I/O sweep (#1295 review
    /// MED 1) — see [`push_history`](Self::push_history) for why it is the
    /// whole ring rather than the newest sample.
    pub fn push_disk_io_history(&mut self, ring: &[f32]) {
        self.disk_io.push(ring);
    }

    /// Stamp a fresh batch onto the drawer page's GPU-VRAM sweep (#1295 review
    /// MED 1).
    pub fn push_gpu_vram_history(&mut self, ring: &[f32]) {
        self.gpu_vram.push(ring);
    }

    /// The card's history sweep, as a node.
    pub(crate) fn history_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.history.node_classed(id, classes)
    }

    /// The drawer page's disk-I/O sweep.
    pub(crate) fn disk_io_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.disk_io.node_classed(id, classes)
    }

    /// The drawer page's GPU-VRAM sweep.
    pub(crate) fn gpu_vram_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.gpu_vram.node_classed(id, classes)
    }

    /// The bar chip's history sweep.
    pub(crate) fn chip_history_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.chip_history.node_classed(id, classes)
    }

    /// The GPU needle.
    pub(crate) fn gpu_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.gpu.node_classed(id, classes)
    }

    /// The seven-segment temperature readout for `celsius`.
    pub(crate) fn temp_node(&self, id: &str, classes: Vec<Cls>, celsius: Option<f32>) -> Node {
        self.temp.node_classed(id, classes, &temp_text(celsius))
    }

    /// The memory meter.
    pub(crate) fn memory_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.memory.node_classed(id, classes)
    }

    /// The swap meter.
    pub(crate) fn swap_node(&self, id: &str, classes: Vec<Cls>) -> Node {
        self.swap.node_classed(id, classes)
    }

    /// One small lamp row at the bar/sidebar pitch — `text` is a
    /// [`lamp_rows`] row.
    pub(crate) fn lamp_node(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.lamps.node_classed(id, classes, text)
    }

    /// The drawer page's disk-lamp row, at the page's own pitch (#1295 review
    /// LOW 4) — [`lamp_node`](Self::lamp_node)'s twin for
    /// [`fit_disk_lamps`](Self::fit_disk_lamps).
    pub(crate) fn page_disk_lamp_node(&self, id: &str, classes: Vec<Cls>, text: &str) -> Node {
        self.page_disk_lamps.node_classed(id, classes, text)
    }

    /// The drawer page's per-core rows, at the page's own pitch.
    pub(crate) fn page_core_nodes(&self, loads: &[f32]) -> Vec<Node> {
        lamp_rows(loads)
            .iter()
            .enumerate()
            .map(|(i, row)| {
                self.page_cores
                    .node_classed(&format!("stats-panel-cores-{i}"), cls("ts-cpu"), row)
            })
            .collect()
    }

    /// The lamp rows, as nodes.
    ///
    /// Each row carries its own stable id (`stats-cores-<n>`), which #900 makes
    /// a contract for a preem node: an anonymous one is keyed by its ordinal
    /// among the tree's un-id'd preem nodes, and a *variable-length row of
    /// per-core meters* is the exact example that documentation gives for what
    /// then goes wrong.
    fn core_nodes(&self, loads: &[f32]) -> Vec<Node> {
        lamp_rows(loads)
            .iter()
            .enumerate()
            .map(|(i, row)| {
                self.cores
                    .node_classed(&format!("stats-cores-{i}"), cls("ts-cpu"), row)
            })
            .collect()
    }
}

/// Project a snapshot into the card's widget tree.
///
/// Pure: every branch is decided by `cfg` and by what the snapshot holds, so the
/// whole card is testable by handing it a `Snapshot` literal. `widgets` is
/// `&Widgets` rather than `&mut` for the same reason — the animation setters are
/// `update`'s job, and `view` only reads.
#[must_use]
pub fn card(cfg: crate::config::Card, snapshot: &Snapshot, widgets: &Widgets) -> Node {
    let mut children = Vec::new();

    if cfg.cpu {
        // `snapshot.cpu` is an `Option`, so the headline dashes on the seed
        // frame exactly like the lamp row does — two answers to "do we know
        // yet?" in one frame was #1277's LOW 5.
        children.push(header("CPU", percent_text(snapshot.cpu), "ts-cpu", None));
        if cfg.per_core {
            children.extend(widgets.core_nodes(&snapshot.per_core));
        }
        if cfg.history {
            children.push(
                widgets
                    .history
                    .node_classed("stats-cpu-history", cls("ts-cpu")),
            );
        }
        if cfg.temperature {
            children.push(Node::Row {
                id: None,
                classes: Vec::new(),
                spacing: 4,
                children: vec![
                    widgets.temp.node_classed(
                        "stats-cpu-temp",
                        cls("ts-cpu-temp"),
                        &temp_text(snapshot.cpu_temp_c),
                    ),
                    label("°C", &["dim-label", "ts-cpu-temp"]),
                    Node::Spacer,
                ],
                tooltip: None,
            });
        }
    }

    if cfg.memory {
        children.extend(memory_rows(snapshot, widgets, "stats"));
    }

    if cfg.disk {
        children.extend(disk_rows(snapshot, widgets, "stats"));
    }

    // The GPU half hides itself entirely when there is nothing to read — the
    // native `widgets/gpu.rs` precedent, and the reason `gpu = true` costs
    // nothing on a GPU-less machine.
    if let Some(gpu) = snapshot.gpu.as_ref().filter(|_| cfg.gpu) {
        children.push(header(
            "GPU",
            percent_text(gpu.load),
            "ts-gpu",
            (!gpu.name.trim().is_empty()).then(|| gpu.name.clone()),
        ));
        children.push(widgets.gpu.node_classed("stats-gpu-load", cls("ts-gpu")));
    }

    Node::Box {
        id: Some("stats-card".to_owned()),
        dir: Dir::Vertical,
        spacing: 6,
        scroll: false,
        // No `.ts-plugin-card` and no `.card`: the host's region wrapper already
        // supplies the card treatment, and stacking a second one reads as a
        // card-in-a-card (the SDK's *Styling* docs).
        classes: cls("flat"),
        children,
        tooltip: None,
    }
}

/// The memory rows shared by the sidebar card and the drawer page: a header
/// carrying the exact `used / total (pct%)` the native row prints, the LED
/// meter, and — only on a machine that has swap — the same pair for swap.
///
/// `prefix` namespaces the node ids, because the chip tree and the panel tree
/// are two trees in one frame and a preem node's id is its reconciler key
/// (#900).
pub(crate) fn memory_rows(snapshot: &Snapshot, widgets: &Widgets, prefix: &str) -> Vec<Node> {
    let mem = snapshot.memory.as_ref();
    let mut out = vec![
        header(
            "Memory",
            mem.map_or_else(
                || "—".to_owned(),
                |m| format::used_of_total(m.used, m.total),
            ),
            "ts-memory",
            None,
        ),
        widgets.memory_node(&format!("{prefix}-memory-level"), cls("ts-memory")),
    ];
    // Swap hides itself when the machine has none — the native page's own rule
    // (`m.swap_total > 0`), not a new one.
    if let Some(m) = mem.filter(|m| m.swap_total > 0) {
        out.push(header(
            "Swap",
            format::used_of_total(m.swap_used, m.swap_total),
            "ts-memory",
            None,
        ));
        out.push(widgets.swap_node(&format!("{prefix}-swap-level"), cls("ts-memory")));
    }
    out
}

/// The disk rows shared by the sidebar card and the bar chip's sibling on a
/// card: a header counting mounts the way the native expander does
/// (`"{} mount(s)"`), and one lamp per mount.
pub(crate) fn disk_rows(snapshot: &Snapshot, widgets: &Widgets, prefix: &str) -> Vec<Node> {
    let usages: Vec<f32> = snapshot.disks.iter().map(|d| d.usage).collect();
    vec![
        header(
            "Disks",
            format!("{} mount(s)", snapshot.disks.len()),
            "ts-disk",
            Some(disk_tooltip(&snapshot.disks)),
        ),
        Node::Row {
            id: None,
            classes: Vec::new(),
            spacing: 4,
            children: vec![
                widgets.lamp_node(
                    &format!("{prefix}-disk-lamps"),
                    cls("ts-disk"),
                    &lamp_rows(&usages).join(" "),
                ),
                Node::Spacer,
            ],
            tooltip: Some(disk_tooltip(&snapshot.disks)),
        },
    ]
}

/// [`disk_rows`]'s twin for the **drawer page** (#1295 review LOW 4): the same
/// header and lamp row, but through [`Widgets::page_disk_lamp_node`] so the
/// row draws at the page's own pitch rather than the bar/sidebar's fixed
/// [`CHIP_DOT_PX`] floor. Not folded into `disk_rows` with a pitch parameter:
/// the two are ten lines of otherwise-identical layout, and the module already
/// keeps `history_node`/`chip_history_node` and `lamp_node`/
/// `page_disk_lamp_node` as separate named wrappers rather than parametrised
/// ones (see the module doc's precedent).
pub(crate) fn page_disk_rows(snapshot: &Snapshot, widgets: &Widgets, prefix: &str) -> Vec<Node> {
    let usages: Vec<f32> = snapshot.disks.iter().map(|d| d.usage).collect();
    vec![
        header(
            "Disks",
            format!("{} mount(s)", snapshot.disks.len()),
            "ts-disk",
            Some(disk_tooltip(&snapshot.disks)),
        ),
        Node::Row {
            id: None,
            classes: Vec::new(),
            spacing: 4,
            children: vec![
                widgets.page_disk_lamp_node(
                    &format!("{prefix}-disk-lamps"),
                    cls("ts-disk"),
                    &lamp_rows(&usages).join(" "),
                ),
                Node::Spacer,
            ],
            tooltip: Some(disk_tooltip(&snapshot.disks)),
        },
    ]
}

/// One bar chip: a click target carrying its `ts-*` class, a short word where
/// the native chip has an icon, whatever extra nodes it draws, and the native
/// chip's hover text on the row so it covers the whole pill.
///
/// The button id is the click id the host sends back as
/// [`EventKind::Click`](hytte_plugin::proto::EventKind::Click), and **every**
/// chip's click does the same thing — opens this plugin's drawer page — so the
/// four ids exist to be distinct reconciler keys rather than to be told apart
/// in `update`.
fn chip(name: &str, class: &str, tooltip: String, body: Vec<Node>) -> Node {
    let mut children = vec![label(name, &["caption-heading", "dim-label"])];
    children.extend(body);
    Node::Button {
        id: chip_button_id(class),
        // `flat` strips the button chrome: the host already wraps a bar
        // plugin's root in `.ts-plugin-chip`, which is the pill, and a second
        // one inside it reads as a chip in a chip (the SDK's *Styling* docs,
        // and `hytte-plugin-weather`'s precedent for a whole-card button).
        classes: vec!["flat".to_owned(), format!("ts-{class}")],
        child: Box::new(Node::Row {
            id: Some(format!("stats-chip-{class}-row")),
            classes: cls(&format!("ts-{class}")),
            spacing: 3,
            children,
            tooltip: Some(tooltip),
        }),
    }
}

/// The four chips a bar instance can draw, by the `ts-*` class each keeps.
///
/// Five in the bar today: `services` is missing, deliberately — see [`chips`].
pub const CHIP_CLASSES: [&str; 4] = ["cpu", "memory", "disk", "gpu"];

/// The click id of one chip's button.
#[must_use]
pub fn chip_button_id(class: &str) -> String {
    format!("stats-chip-{class}")
}

/// Whether a node id the host sent an event for is one of **this plugin's**
/// chip buttons.
///
/// Checked rather than assumed: `update` turns a click into an
/// `OpenPage` effect, and "any click at all opens a page" is a broader rule
/// than this plugin means. Driven off [`CHIP_CLASSES`] so a fifth chip is
/// clickable the day it is drawn, and a stale id is not.
#[must_use]
pub fn is_chip_button(node: &str) -> bool {
    CHIP_CLASSES.iter().any(|c| node == chip_button_id(c))
}

/// Project a snapshot into the **bar** instance's chips (#1251).
///
/// Four chips, not five: the native `ts-services` chip counts failed systemd
/// units (a system-bus client) and flapping shell tasks (the shell's own
/// supervisor), and a plugin process can reach neither. Epic #1248's P3 already
/// says that chip stays native; see `crate::config`'s `DEFAULT_TOML`.
///
/// Pure, like [`card`]: every branch is decided by `cfg` and by what the
/// snapshot holds.
#[must_use]
pub fn chips(cfg: crate::config::Card, snapshot: &Snapshot, widgets: &Widgets) -> Node {
    let mut children = Vec::new();

    if cfg.cpu {
        let mut body = Vec::new();
        // The native chip shows the temperature as a plain `{c:.0}°` label with
        // no unit letter, and shows NOTHING at all when no sensor answers —
        // both copied, which is why this is an `if let` and not a `--` readout
        // like the card's seven-seg.
        if cfg.temperature
            && let Some(c) = snapshot.cpu_temp_c.filter(|c| c.is_finite())
        {
            body.push(label(format!("{c:.0}°"), &["ts-cpu-temp"]));
        }
        // One lamp for the whole package, or one per core when the deployment
        // asks for the BlinkenLichten in the bar.
        let loads: Vec<f32> = if cfg.per_core {
            snapshot.per_core.clone()
        } else {
            snapshot.cpu.map_or_else(Vec::new, |c| vec![c])
        };
        body.push(widgets.lamp_node(
            "stats-chip-cpu-lamps",
            cls("ts-cpu"),
            &lamp_rows(&loads).join(" "),
        ));
        if cfg.history {
            body.push(widgets.chip_history_node("stats-chip-cpu-history", cls("ts-cpu")));
        }
        children.push(chip("CPU", "cpu", cpu_tooltip(snapshot.cpu), body));
    }

    if cfg.memory {
        let level = snapshot
            .memory
            .as_ref()
            .map(|m| format::fraction(m.used, m.total));
        children.push(chip(
            "MEM",
            "memory",
            memory_tooltip(snapshot.memory.as_ref()),
            vec![widgets.lamp_node(
                "stats-chip-memory-lamp",
                cls("ts-memory"),
                &lamp_rows(&level.map_or_else(Vec::new, |l| vec![l])).join(" "),
            )],
        ));
    }

    if cfg.disk {
        let usages: Vec<f32> = snapshot.disks.iter().map(|d| d.usage).collect();
        children.push(chip(
            "DSK",
            "disk",
            disk_tooltip(&snapshot.disks),
            vec![widgets.lamp_node(
                "stats-chip-disk-lamps",
                cls("ts-disk"),
                &lamp_rows(&usages).join(" "),
            )],
        ));
    }

    // The GPU chip hides itself entirely on a machine with no GPU — the native
    // `widgets/gpu.rs` `bind_visible`, which is the one chip of the five that
    // genuinely disappears.
    if let Some(gpu) = snapshot.gpu.as_ref().filter(|_| cfg.gpu) {
        let mut body = Vec::new();
        // Same `{c:.0}°` label, same `ts-gpu-temp` class, same
        // show-nothing-when-absent rule as the native `widgets/gpu.rs`.
        if cfg.temperature
            && let Some(c) = gpu.temperature_c.filter(|c| c.is_finite())
        {
            body.push(label(format!("{c:.0}°"), &["ts-gpu-temp"]));
        }
        body.push(widgets.lamp_node(
            "stats-chip-gpu-lamp",
            cls("ts-gpu"),
            &lamp_rows(&gpu.load.map_or_else(Vec::new, |l| vec![l])).join(" "),
        ));
        children.push(chip("GPU", "gpu", gpu_tooltip(Some(gpu)), body));
    }

    // An all-off `[bar]` table has nothing to draw (#1295 review LOW 6). The
    // host wraps every bar plugin's root in `.ts-plugin-chip`
    // (`trollshell/src/plugins/region.rs`), which paints a background and
    // padding — so an empty `stats-chips` `Node::Box` would still be a small,
    // empty, translucent pill rather than nothing at all. `Node::Spacer` is
    // the vocabulary's own "nothing" shape (its doc: "an empty, style-less
    // box"), and the host's `root_renders_nothing` treats a card whose tree
    // bottoms out in one as having nothing to show, hiding the whole pill —
    // the same "hide entirely rather than draw a parked meter" rule the GPU
    // chip already rides one level down (a machine with no adapter draws no
    // GPU chip, not a needle at zero).
    if children.is_empty() {
        return Node::Spacer;
    }

    Node::Box {
        id: Some("stats-chips".to_owned()),
        dir: Dir::Horizontal,
        spacing: 6,
        scroll: false,
        classes: Vec::new(),
        children,
        tooltip: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_PX, CHIP_CLASSES, CHIP_DOT_PX, CORES_PER_ROW, DASH_CELLS, LAMPS, PAGE_PX, Widgets,
        card, chip_button_id, chips, cls, cpu_tooltip, disk_tooltip, dot_px_for, dot_px_for_in,
        gpu_tooltip, is_chip_button, lamp, lamp_rows, memory_tooltip, percent_text, row_cells,
        temp_text, trace_sample,
    };
    use crate::config::Card;
    use crate::sample::{Disk, Gpu, Memory, Snapshot};
    use hytte_plugin::display::RenderMode;
    use hytte_plugin::display::testing::with_render_mode;
    use hytte_plugin::preem::font;
    use hytte_plugin::proto::Node;
    use hytte_plugin::proto::preem::{MAX_DOT_PX, MIN_DOT_PX};

    /// How many dots the kit's font lights for one glyph.
    fn lit_dots(c: char) -> u32 {
        font::glyph(c)
            .unwrap_or_else(|| panic!("the kit's 5x7 font must cover the lamp glyph {c:?}"))
            .iter()
            .map(|row| row.count_ones())
            .sum()
    }

    /// **The ramp is a ramp**: every lamp lights strictly more dots than the one
    /// below it, measured against the kit's own font rather than asserted by
    /// eye — and the measured counts are pinned as literals, so a changed glyph
    /// bitmap reds here instead of quietly flattening the middle of the ramp.
    ///
    /// This is the whole claim the per-core row rests on: "brighter means
    /// busier" is only true if the glyph sequence is monotone in ink.
    ///
    /// **Falsified** by swapping any two entries of `LAMPS`.
    #[test]
    fn the_lamp_ramp_is_monotone_in_lit_dots() {
        let measured: Vec<u32> = LAMPS.iter().copied().map(lit_dots).collect();
        assert_eq!(
            measured,
            vec![4, 8, 12, 16, 19],
            "the ramp's measured ink, glyph by glyph",
        );
        for pair in measured.windows(2) {
            assert!(
                pair[1] > pair[0],
                "every lamp must light strictly more dots than the one below it: {measured:?}",
            );
        }
    }

    /// The two ends of the ramp are the two ends of the load range, and the
    /// mapping is monotone all the way across.
    #[test]
    fn a_busier_core_is_never_a_dimmer_lamp() {
        assert_eq!(lamp(0.0), LAMPS[0]);
        assert_eq!(lamp(1.0), LAMPS[LAMPS.len() - 1]);
        let mut last = 0;
        for step in 0..=100 {
            #[allow(clippy::cast_precision_loss)]
            let load = step as f32 / 100.0;
            let ink = lit_dots(lamp(load));
            assert!(ink >= last, "load {load} dimmed the lamp");
            last = ink;
        }
    }

    /// Out-of-range and non-finite loads cannot panic or index past the ramp —
    /// `view` is the one function on this card that must never fail.
    #[test]
    fn a_nonsense_load_still_picks_a_lamp() {
        assert_eq!(lamp(-5.0), LAMPS[0]);
        assert_eq!(lamp(f32::NEG_INFINITY), LAMPS[0]);
        assert_eq!(lamp(5.0), LAMPS[LAMPS.len() - 1]);
        assert_eq!(lamp(f32::INFINITY), LAMPS[LAMPS.len() - 1]);
        assert_eq!(lamp(f32::NAN), LAMPS[0], "a NaN takes the dimmest lamp");
    }

    /// One cell per core, in the kernel's order, wrapped at `CORES_PER_ROW`.
    #[test]
    fn there_is_one_lamp_per_core_wrapped_into_banks() {
        let loads: Vec<f32> = (0..20_u8).map(|i| f32::from(i) / 19.0).collect();
        let rows = lamp_rows(&loads);
        assert_eq!(rows.len(), 2, "20 cores wrap onto two banks");
        assert_eq!(rows[0].chars().count(), CORES_PER_ROW);
        assert_eq!(rows[1].chars().count(), 20 - CORES_PER_ROW);
        assert_eq!(
            rows.concat().chars().count(),
            loads.len(),
            "every core gets exactly one lamp and none is invented",
        );
        assert_eq!(rows[0].chars().next(), Some(LAMPS[0]), "core 0 is idle");
        assert_eq!(
            rows[1].chars().last(),
            Some(LAMPS[LAMPS.len() - 1]),
            "the last core is saturated",
        );
    }

    /// Before the first `/proc/stat` delta there are no per-core loads at all;
    /// the row shows dashes rather than vanishing or rendering an empty buffer.
    #[test]
    fn no_reading_yet_is_a_row_of_dashes() {
        assert_eq!(lamp_rows(&[]), vec!["----".to_owned()]);
    }

    /// The pitch fits the card: a row of `n` lamps at the chosen pitch is
    /// `dot_px * (6n + 1)` px wide, which must stay inside `CARD_PX` for every
    /// core count that can fit at all — and must never leave the kit's own
    /// pitch bounds.
    ///
    /// **Falsified** by dropping the `.clamp` (a 64-thread box then asks for a
    /// pitch of 0, which the kit refuses).
    #[test]
    fn the_dot_pitch_fits_the_card_and_stays_inside_the_kits_bounds() {
        for cells in 1..=CORES_PER_ROW {
            let px = dot_px_for(cells);
            assert!(
                (MIN_DOT_PX..=MAX_DOT_PX).contains(&px),
                "{cells} cells asked for a pitch of {px}",
            );
            let width = px * (6 * u32::try_from(cells).unwrap() + 1);
            assert!(
                width <= CARD_PX,
                "{cells} lamps at pitch {px} is {width}px, past the {CARD_PX}px card",
            );
        }
        // Past the card's width the pitch bottoms out rather than going to zero.
        // `dot_px_for` is a *row* width, so nothing on this card ever asks it
        // these — but it is total over them, which is what keeps `row_cells`
        // the only clamp and not a second one.
        assert_eq!(dot_px_for(1_000), MIN_DOT_PX);
        assert_eq!(dot_px_for(usize::MAX), MIN_DOT_PX);
        // And a degenerate count is still a legal pitch.
        assert_eq!(dot_px_for(0), dot_px_for(1));
    }

    /// **The pitch is fitted to the widest ROW, not to the core count** —
    /// extended past the wrap boundary, which is where the loop above stops and
    /// where #1277's MEDIUM 1 lived.
    ///
    /// Below seventeen threads the two numbers coincide, so every count the old
    /// loop covered passed either way; from seventeen up, `6 * cells + 1` is
    /// computed for a row `lamp_rows` never emits, the division underflows, and
    /// the clamp hands back `MIN_DOT_PX`. A 32-thread desktop then drew its
    /// lamps at 2 px instead of the 3 px the card admits, filling 194 px of 296.
    ///
    /// **Falsified** by making `row_cells` answer `loads.len().max(1)` — every
    /// row from 17 up then reds, at the pitch and again at the width.
    #[test]
    fn the_lamp_pitch_is_fitted_to_the_widest_row_past_the_wrap_boundary() {
        for cores in [1_usize, 4, 8, 12, 16, 17, 20, 24, 32, 64, 128] {
            let loads = vec![0.5_f32; cores];
            let rows = lamp_rows(&loads);
            let widest = rows
                .iter()
                .map(|r| r.chars().count())
                .max()
                .expect("lamp_rows never returns nothing");
            assert_eq!(
                row_cells(&loads),
                widest,
                "{cores} cores wrap into rows at most {widest} wide",
            );

            let px = dot_px_for(row_cells(&loads));
            assert_eq!(
                px,
                dot_px_for(widest),
                "{cores} cores must be drawn at the pitch its widest row admits",
            );
            let width = px * (6 * u32::try_from(widest).unwrap() + 1);
            assert!(
                width <= CARD_PX,
                "{cores} cores: a {widest}-lamp row at pitch {px} is {width}px, \
                 past the {CARD_PX}px card",
            );
        }

        // Every count from the wrap boundary up is the *same* pitch — the 16-wide
        // row's — rather than sliding down to the floor.
        let sixteen = dot_px_for(CORES_PER_ROW);
        for cores in [16_usize, 17, 32, 64, 128, 1_024] {
            assert_eq!(
                dot_px_for(row_cells(&vec![0.5_f32; cores])),
                sixteen,
                "{cores} cores still draws a 16-wide row",
            );
        }
        assert!(sixteen > MIN_DOT_PX, "…and that is not the floor");

        // An empty reading is the dash row's width, not one cell.
        assert_eq!(row_cells(&[]), DASH_CELLS);
    }

    /// The temperature readout uses only glyphs the kit's seven-segment cells
    /// actually have (digits, `:`, `-`, space) — a `°` would render blank.
    #[test]
    fn the_temperature_readout_is_seven_segment_safe() {
        assert_eq!(temp_text(Some(58.4)), "58");
        assert_eq!(temp_text(Some(100.0)), "100");
        assert_eq!(temp_text(None), "--", "an absent sensor is a dash");
        assert_eq!(temp_text(Some(f32::NAN)), "--");
        assert_eq!(temp_text(Some(f32::INFINITY)), "--");
        for text in [temp_text(Some(58.4)), temp_text(None)] {
            assert!(
                text.chars()
                    .all(|c| c.is_ascii_digit() || c == '-' || c == ':' || c == ' '),
                "{text:?} must be drawable on a seven-segment cell",
            );
        }
    }

    /// The percentage label, including the absent-counter case.
    #[test]
    fn a_missing_load_reads_as_a_dash() {
        assert_eq!(percent_text(Some(0.375)), "38%");
        assert_eq!(percent_text(Some(0.0)), "0%");
        assert_eq!(percent_text(Some(1.0)), "100%");
        assert_eq!(percent_text(None), "—");
        assert_eq!(percent_text(Some(f32::NAN)), "—");
    }

    /// The history trace maps rest to the bottom of the scope and saturation to
    /// the top, rather than hugging the centre line at half amplitude.
    #[test]
    fn the_trace_uses_the_scopes_whole_range() {
        assert!((trace_sample(0.0) + 1.0).abs() < 1e-6);
        assert!(trace_sample(0.5).abs() < 1e-6);
        assert!((trace_sample(1.0) - 1.0).abs() < 1e-6);
        assert!((trace_sample(-9.0) + 1.0).abs() < 1e-6);
        assert!((trace_sample(9.0) - 1.0).abs() < 1e-6);
    }

    /// The ids of every preem widget the card rendered, in tree order, built
    /// with the host **speaking preem** — which is what a `Node::Preem` id list
    /// requires.
    ///
    /// Without the forced mode this would come back empty and every assertion
    /// below would be vacuous: the SDK's wrappers default to
    /// [`RenderMode::Raster`] until a real host `Hello` raises the negotiated
    /// generation, and a unit test has no session to receive one. That is the
    /// whole reason `display::testing` exists.
    fn preem_ids(cfg: Card, snapshot: &Snapshot, widgets: &Widgets) -> Vec<String> {
        with_render_mode(RenderMode::State, || {
            let tree = card(cfg, snapshot, widgets);
            let mut out = Vec::new();
            walk(&tree, &mut out);
            out
        })
    }

    /// Every widget node's id, whichever shape it went out as — so the raster
    /// arm can be asserted with the same helper.
    fn walk(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::Preem { id, .. } | Node::Pixels { id, .. } => {
                out.push(id.clone().unwrap_or_default());
            }
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    walk(child, out);
                }
            }
            _ => {}
        }
    }

    fn busy_snapshot() -> Snapshot {
        Snapshot {
            cpu: Some(0.42),
            per_core: vec![0.1, 0.9, 0.5, 0.0],
            cpu_temp_c: Some(57.0),
            gpu: Some(Gpu {
                name: "AMD Radeon RX 6800".to_owned(),
                load: Some(0.25),
                temperature_c: Some(49.0),
                memory_used_bytes: None,
                memory_total_bytes: None,
            }),
            memory: Some(Memory {
                used: 11_999_999_000,
                total: 33_500_000_000,
                swap_used: 0,
                swap_total: 0,
            }),
            // Not exercised by the card or the chips: `processes` /
            // `cpu_clock_hz` / `disk_io` are drawer-page-only rows
            // (`crate::panel`), whose own `busy()` fixture carries real values.
            processes: None,
            cpu_clock_hz: None,
            disk_io: None,
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
        }
    }

    /// Every chip button in a bar tree, with the classes it carries.
    fn chip_buttons(node: &Node) -> Vec<(String, Vec<String>)> {
        fn walk(node: &Node, out: &mut Vec<(String, Vec<String>)>) {
            match node {
                Node::Button { id, classes, child } => {
                    out.push((id.clone(), classes.clone()));
                    walk(child, out);
                }
                Node::Box { children, .. } | Node::Row { children, .. } => {
                    for child in children {
                        walk(child, out);
                    }
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        walk(node, &mut out);
        out
    }

    /// Every widget node id in a bar tree, in tree order.
    fn chip_preem_ids(cfg: Card, snapshot: &Snapshot, widgets: &Widgets) -> Vec<String> {
        with_render_mode(RenderMode::State, || {
            let tree = chips(cfg, snapshot, widgets);
            let mut out = Vec::new();
            chip_walk(&tree, &mut out);
            out
        })
    }

    fn chip_walk(node: &Node, out: &mut Vec<String>) {
        match node {
            Node::Preem { id, .. } | Node::Pixels { id, .. } => {
                out.push(id.clone().unwrap_or_default());
            }
            Node::Button { child, .. } => chip_walk(child, out),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    chip_walk(child, out);
                }
            }
            _ => {}
        }
    }

    /// The full sidebar card: every preem node the config asks for, each with a
    /// stable id, and no duplicates (#918 — two preem nodes sharing an id
    /// collapse onto one renderer instance).
    #[test]
    fn the_sidebar_card_renders_every_widget_with_a_unique_stable_id() {
        let widgets = Widgets::default();
        let ids = preem_ids(Card::sidebar_default(), &busy_snapshot(), &widgets);
        assert_eq!(
            ids,
            vec![
                "stats-cores-0".to_owned(),
                "stats-cpu-history".to_owned(),
                "stats-cpu-temp".to_owned(),
                "stats-gpu-load".to_owned(),
            ],
        );
        assert!(
            ids.iter().all(|id| !id.is_empty()),
            "every preem node carries an id — an anonymous one inherits a sibling's animation",
        );
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "ids are unique within the tree");
    }

    /// **The GPU half hides itself** when there is no GPU — the native
    /// `widgets/gpu.rs` behaviour, and what makes `gpu = true` free on a
    /// machine that has none.
    ///
    /// **Falsified** by rendering the gauge unconditionally: the id list then
    /// carries `stats-gpu-load` on a GPU-less machine.
    #[test]
    fn a_machine_with_no_gpu_draws_no_gpu_half() {
        let widgets = Widgets::default();
        let snapshot = Snapshot {
            gpu: None,
            ..busy_snapshot()
        };
        let ids = preem_ids(Card::sidebar_default(), &snapshot, &widgets);
        assert!(
            !ids.iter().any(|id| id.starts_with("stats-gpu")),
            "no GPU, no gauge: {ids:?}",
        );
        assert!(
            ids.iter().any(|id| id.starts_with("stats-cores")),
            "…and the CPU half is untouched: {ids:?}",
        );
    }

    /// Each config switch removes exactly its own widget — the `[sidebar]`
    /// table's whole purpose.
    #[test]
    fn each_config_switch_removes_exactly_its_own_widget() {
        let widgets = Widgets::default();
        let snapshot = busy_snapshot();
        let all = Card::sidebar_default();

        let off = |f: fn(&mut Card)| {
            let mut cfg = all;
            f(&mut cfg);
            preem_ids(cfg, &snapshot, &widgets)
        };

        assert!(
            !off(|c| c.per_core = false)
                .iter()
                .any(|i| i.starts_with("stats-cores"))
        );
        assert!(!off(|c| c.history = false).contains(&"stats-cpu-history".to_owned()));
        assert!(!off(|c| c.temperature = false).contains(&"stats-cpu-temp".to_owned()));
        assert!(!off(|c| c.gpu = false).contains(&"stats-gpu-load".to_owned()));
        // `cpu = false` is the whole half, not one widget.
        let cpu_off = off(|c| c.cpu = false);
        assert_eq!(cpu_off, vec!["stats-gpu-load".to_owned()]);
    }

    /// The two tables ask for **different parts of the machine**, which is the
    /// difference that makes two tables worth having.
    ///
    /// Until P2 this asserted the bar table was merely *smaller*; it no longer
    /// is (a bar carries memory and disk, a compact sidebar card does not), and
    /// asserting a size would now pass for the wrong reason. Spelled as which
    /// widgets each table produces instead.
    ///
    /// **Falsified** by making `for_family` answer one table for both.
    #[test]
    fn the_two_tables_ask_for_different_parts_of_the_machine() {
        let widgets = Widgets::default();
        let snapshot = busy_snapshot();
        let bar = preem_ids(Card::bar_default(), &snapshot, &widgets);
        let sidebar = preem_ids(Card::sidebar_default(), &snapshot, &widgets);

        assert!(
            !bar.iter().any(|i| i.starts_with("stats-cores")),
            "a bar chip has no room for the lamp row: {bar:?}",
        );
        assert!(
            !bar.contains(&"stats-cpu-history".to_owned()),
            "…nor for the sweep: {bar:?}",
        );
        assert!(
            bar.contains(&"stats-memory-level".to_owned())
                && bar.contains(&"stats-disk-lamps".to_owned()),
            "…and it carries memory and disk, which the bar always has: {bar:?}",
        );

        assert!(
            sidebar.iter().any(|i| i.starts_with("stats-cores")),
            "{sidebar:?}",
        );
        assert!(
            !sidebar.contains(&"stats-memory-level".to_owned())
                && !sidebar.contains(&"stats-disk-lamps".to_owned()),
            "the sidebar card is the COMPACT CPU + GPU card (#1235): {sidebar:?}",
        );
    }

    /// **The bar instance**: four chips, each a click target keeping the class
    /// its native counterpart wears (#1251), in the order the shell's own bar
    /// group builds them (`trollshell/src/main.rs`: cpu, memory, gpu, disk,
    /// services).
    ///
    /// The order here is cpu, memory, disk, gpu rather than the shell's cpu,
    /// memory, gpu, disk, because the GPU chip is the one that vanishes on a
    /// machine without one — putting it last keeps the other three from
    /// shifting sideways when it does.
    ///
    /// **Falsified** by dropping any `if cfg.…` arm in `chips`, or by writing
    /// a class by hand instead of from the chip's own name.
    #[test]
    fn the_bar_renders_four_chips_each_keeping_its_class() {
        let buttons = chip_buttons(&chips(
            Card::bar_default(),
            &busy_snapshot(),
            &Widgets::default(),
        ));
        assert_eq!(
            buttons,
            vec![
                (
                    "stats-chip-cpu".to_owned(),
                    vec!["flat".to_owned(), "ts-cpu".to_owned()]
                ),
                (
                    "stats-chip-memory".to_owned(),
                    vec!["flat".to_owned(), "ts-memory".to_owned()]
                ),
                (
                    "stats-chip-disk".to_owned(),
                    vec!["flat".to_owned(), "ts-disk".to_owned()]
                ),
                (
                    "stats-chip-gpu".to_owned(),
                    vec!["flat".to_owned(), "ts-gpu".to_owned()]
                ),
            ],
        );
        // …and the four classes are exactly the four the vocabulary knows, so
        // `is_chip_button` and `chips` cannot drift apart.
        assert_eq!(CHIP_CLASSES, ["cpu", "memory", "disk", "gpu"]);
        for (id, _) in &buttons {
            assert!(is_chip_button(id), "{id} must be clickable");
        }
        assert!(
            !is_chip_button("stats-chip-services"),
            "the services chip is not this plugin's (see the module doc)",
        );
        assert!(!is_chip_button("stats-card"), "nor is the card root");
        assert_eq!(chip_button_id("cpu"), "stats-chip-cpu");
    }

    /// Every chip's meter, with a stable id, and no duplicates — the same #900
    /// contract the sidebar card is held to.
    #[test]
    fn every_chip_draws_one_meter_with_a_unique_stable_id() {
        let ids = chip_preem_ids(Card::bar_default(), &busy_snapshot(), &Widgets::default());
        assert_eq!(
            ids,
            vec![
                "stats-chip-cpu-lamps".to_owned(),
                "stats-chip-memory-lamp".to_owned(),
                "stats-chip-disk-lamps".to_owned(),
                "stats-chip-gpu-lamp".to_owned(),
            ],
        );
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "duplicate ids in {ids:?}");
    }

    /// Each `[bar]` switch removes exactly its own chip — and `history` /
    /// `per_core`, which are modifiers on the CPU chip rather than chips of
    /// their own, add exactly one node each.
    ///
    /// **Falsified** by rendering any chip unconditionally.
    #[test]
    fn each_bar_switch_removes_exactly_its_own_chip() {
        let snapshot = busy_snapshot();
        let widgets = Widgets::default();
        let ids = |f: fn(&mut Card)| {
            let mut cfg = Card::bar_default();
            f(&mut cfg);
            chip_preem_ids(cfg, &snapshot, &widgets)
        };

        assert!(!ids(|c| c.cpu = false).iter().any(|i| i.contains("-cpu-")));
        assert!(
            !ids(|c| c.memory = false)
                .iter()
                .any(|i| i.contains("-memory-"))
        );
        assert!(!ids(|c| c.disk = false).iter().any(|i| i.contains("-disk-")));
        assert!(!ids(|c| c.gpu = false).iter().any(|i| i.contains("-gpu-")));

        // The two modifiers: on, they add one node to the CPU chip; nothing
        // else moves.
        let plain = ids(|_| {});
        let with_history = ids(|c| c.history = true);
        assert_eq!(with_history.len(), plain.len() + 1);
        assert!(with_history.contains(&"stats-chip-cpu-history".to_owned()));
        // `per_core` changes the lamp row's CONTENT, not the node count.
        assert_eq!(ids(|c| c.per_core = true).len(), plain.len());
    }

    /// `per_core` on a bar turns the CPU chip's single package lamp into one
    /// cell per core — the `BlinkenLichten` in the bar, and the reason that key
    /// is not dead on the `[bar]` table.
    ///
    /// **Falsified** by having `chips` always draw the overall load.
    #[test]
    fn the_per_core_switch_turns_the_cpu_chips_lamp_into_a_row() {
        let snapshot = busy_snapshot();
        let widgets = Widgets::default();
        let text_of = |cfg: Card| {
            with_render_mode(RenderMode::State, || {
                let tree = chips(cfg, &snapshot, &widgets);
                let mut out = Vec::new();
                preem_text(&tree, "stats-chip-cpu-lamps", &mut out);
                out
            })
        };
        let mut cfg = Card::bar_default();
        assert_eq!(
            text_of(cfg),
            vec![lamp(0.42).to_string()],
            "one package lamp"
        );
        cfg.per_core = true;
        assert_eq!(
            text_of(cfg),
            vec![
                snapshot
                    .per_core
                    .iter()
                    .copied()
                    .map(lamp)
                    .collect::<String>()
            ],
            "one cell per core",
        );
    }

    /// The text a dot-matrix node with `want` as its id carries.
    fn preem_text(node: &Node, want: &str, out: &mut Vec<String>) {
        use hytte_plugin::proto::preem::PreemWidget;
        match node {
            Node::Preem { id, widget, .. } if id.as_deref() == Some(want) => {
                if let PreemWidget::DotMatrix { state, .. } = widget.as_ref() {
                    out.push(state.text.clone());
                }
            }
            Node::Button { child, .. } => preem_text(child, want, out),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    preem_text(child, want, out);
                }
            }
            _ => {}
        }
    }

    /// A machine with no GPU draws no GPU **chip** — the native
    /// `widgets/gpu.rs` `bind_visible`, which is the one chip of the five that
    /// genuinely disappears.
    #[test]
    fn a_machine_with_no_gpu_draws_no_gpu_chip() {
        let snapshot = Snapshot {
            gpu: None,
            ..busy_snapshot()
        };
        let buttons = chip_buttons(&chips(Card::bar_default(), &snapshot, &Widgets::default()));
        assert!(
            !buttons.iter().any(|(id, _)| id == "stats-chip-gpu"),
            "{buttons:?}",
        );
        assert_eq!(buttons.len(), 3, "and the other three are untouched");
    }

    /// **The hover text is the native chips' hover text**, literal for literal.
    ///
    /// Copied from `trollshell/src/widgets/{cpu,memory,disk,gpu}.rs` rather
    /// than derived here, because a mirror asserted from its own arithmetic
    /// cannot see a divergence from the thing it mirrors (#1026). Includes the
    /// native memory chip's inconsistency — a colon in the unknown arm, none in
    /// the percent arm — on purpose.
    #[test]
    fn the_chip_tooltips_are_the_native_chips_strings() {
        assert_eq!(cpu_tooltip(Some(0.42)), "CPU 42%");
        assert_eq!(cpu_tooltip(None), "CPU —");

        assert_eq!(
            memory_tooltip(Some(&Memory {
                used: 36,
                total: 100,
                swap_used: 0,
                swap_total: 0,
            })),
            "Memory 36%",
        );
        assert_eq!(memory_tooltip(None), "Memory: unknown");
        assert_eq!(
            memory_tooltip(Some(&Memory::default())),
            "Memory: unknown",
            "a zero total is the native chip's unknown arm, not 0%",
        );

        assert_eq!(
            disk_tooltip(&busy_snapshot().disks),
            "/: 40%, /home: 73%",
            "one fragment per mount, the native per-bar string",
        );

        assert_eq!(
            gpu_tooltip(Some(&Gpu {
                name: "AMD Radeon RX 6800".to_owned(),
                load: Some(0.37),
                temperature_c: None,
                ..Gpu::default()
            })),
            "AMD Radeon RX 6800: 37%",
        );
        assert_eq!(
            gpu_tooltip(Some(&Gpu {
                name: "AMD Radeon RX 6800".to_owned(),
                load: None,
                temperature_c: None,
                ..Gpu::default()
            })),
            "AMD Radeon RX 6800",
            "no busy counter is the bare adapter name, not a dash",
        );
        assert_eq!(gpu_tooltip(None), "GPU");
    }

    /// **`"No mounts"` is an invented string, not a native one** — #1295
    /// review NIT 7. The native disk chip (`trollshell/src/widgets/disk.rs`)
    /// sets no tooltip at all when there are no mounts; it only ever sets one
    /// per bar. This plugin's whole-chip tooltip has nowhere to fall back to,
    /// so it says so instead — plausibly better, but not a mirror, and it does
    /// not belong under a test titled "the native chips' strings".
    #[test]
    fn an_empty_mount_list_gets_an_invented_disk_tooltip() {
        assert_eq!(disk_tooltip(&[]), "No mounts");
    }

    /// The CPU and GPU chips carry the native `{c:.0}°` label — no unit letter,
    /// and **nothing at all** when no sensor answers (the native chips set an
    /// empty label rather than a dash; the card's seven-seg is the one that
    /// shows `--`).
    #[test]
    fn the_chip_temperature_label_is_the_native_one() {
        let widgets = Widgets::default();
        let texts = |snapshot: &Snapshot, cfg: Card| {
            let tree = chips(cfg, snapshot, &widgets);
            let mut out = Vec::new();
            collect_text(&tree, &mut out);
            out
        };

        let hot = busy_snapshot();
        let got = texts(&hot, Card::bar_default());
        assert!(got.iter().any(|t| t == "57°"), "the CPU package: {got:?}");
        assert!(got.iter().any(|t| t == "49°"), "the adapter: {got:?}");

        let cold = Snapshot {
            cpu_temp_c: None,
            gpu: Some(Gpu {
                name: "g".to_owned(),
                load: Some(0.5),
                temperature_c: None,
                ..Gpu::default()
            }),
            ..busy_snapshot()
        };
        let got = texts(&cold, Card::bar_default());
        assert!(
            !got.iter().any(|t| t.ends_with('°')),
            "no sensor, no label: {got:?}",
        );

        // …and `temperature = false` removes both even when the sensors answer.
        let mut off = Card::bar_default();
        off.temperature = false;
        let got = texts(&hot, off);
        assert!(!got.iter().any(|t| t.ends_with('°')), "{got:?}");
    }

    /// The drawer page's lamp row is fitted to the page's own width, not the
    /// sidebar card's — "the P1 row at full width" (#1251), which is a
    /// measurable claim and not a layout wish.
    ///
    /// **Falsified** by having `Widgets::fit_cores` point `page_cores` at
    /// `dot_px_for` (the card's budget): the two pitches then agree and the
    /// second assertion reds.
    #[test]
    fn the_pages_lamp_row_is_fitted_to_the_page() {
        // A `const` assertion rather than a runtime one: both sides are
        // compile-time constants, so this is a claim about the source and
        // belongs where the compiler checks it.
        const _: () = assert!(PAGE_PX > CARD_PX, "the page is wider than the card");
        for cells in [1_usize, 4, 8, 16, 17, 32, 64] {
            let row = cells.min(CORES_PER_ROW);
            let page = dot_px_for_in(PAGE_PX, row);
            let card_pitch = dot_px_for(row);
            assert!(page >= card_pitch, "cells={cells}");
            assert!(
                page * (6 * u32::try_from(row).expect("small") + 1) <= PAGE_PX
                    || page == MIN_DOT_PX,
                "cells={cells}: the row must fit the page",
            );
        }
        assert!(
            dot_px_for_in(PAGE_PX, CORES_PER_ROW) > dot_px_for(CORES_PER_ROW),
            "a 16-wide row is chunkier on the page than on the card",
        );
    }

    /// The dot pitch of the `DotMatrix` node with id `want`, read off the wire
    /// state the wrapper lowered to.
    fn node_dot_px(node: &Node, want: &str) -> Option<u32> {
        use hytte_plugin::proto::preem::PreemWidget;
        match node {
            Node::Preem { id, widget, .. } if id.as_deref() == Some(want) => {
                match widget.as_ref() {
                    PreemWidget::DotMatrix { config, .. } => Some(config.dot_px),
                    _ => None,
                }
            }
            Node::Button { child, .. } => node_dot_px(child, want),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                children.iter().find_map(|c| node_dot_px(c, want))
            }
            _ => None,
        }
    }

    /// **`fit_cores` fits two rows, not one**: the sidebar card's lamps to the
    /// card's width and the drawer page's to the page's, in the same call —
    /// asserted off the pitch each **widget** actually emits, not off the two
    /// formulas.
    ///
    /// The formula-level test above cannot see this: it compares
    /// `dot_px_for_in(PAGE_PX, …)` with `dot_px_for(…)` and would stay green
    /// with `fit_cores` handing the page row the card's budget (measured — that
    /// mutation left it passing). This is the test that reds.
    ///
    /// **Falsified** by pointing `page_cores` at `dot_px_for(cells)`:
    /// `left: 3 / right: 6`.
    #[test]
    fn fit_cores_fits_the_card_row_and_the_page_row_separately() {
        let mut widgets = Widgets::default();
        let loads = vec![0.5_f32; CORES_PER_ROW];
        widgets.fit_cores(&loads);

        with_render_mode(RenderMode::State, || {
            let snapshot = Snapshot {
                per_core: loads.clone(),
                ..busy_snapshot()
            };
            let card_pitch = node_dot_px(
                &card(Card::sidebar_default(), &snapshot, &widgets),
                "stats-cores-0",
            );
            let page_pitch =
                node_dot_px(&widgets.page_core_nodes(&loads)[0], "stats-panel-cores-0");

            assert_eq!(card_pitch, Some(dot_px_for(CORES_PER_ROW)));
            assert_eq!(page_pitch, Some(dot_px_for_in(PAGE_PX, CORES_PER_ROW)));
            assert!(
                page_pitch > card_pitch,
                "the page's row must be the chunkier one: {page_pitch:?} vs {card_pitch:?}",
            );
        });

        // …and the bar's lamps are neither: a fixed pitch, because a chip's
        // budget is its height.
        with_render_mode(RenderMode::State, || {
            let snapshot = Snapshot {
                per_core: loads.clone(),
                ..busy_snapshot()
            };
            let mut cfg = Card::bar_default();
            cfg.per_core = true;
            assert_eq!(
                node_dot_px(&chips(cfg, &snapshot, &widgets), "stats-chip-cpu-lamps"),
                Some(super::CHIP_DOT_PX),
            );
        });
    }

    /// An all-default snapshot — what the seed render carries, before the first
    /// sample can possibly have landed — renders without panicking and shows
    /// dashes rather than numbers it did not measure.
    ///
    /// **Every** reading dashes, which is the whole point: before #1277 the
    /// headline read `CPU 0%` beside a `----` lamp row, two different answers
    /// to "do we know yet?" in one frame, and this test asserted the wrong one
    /// of them (`texts.contains("0%")`, with a message about a number it did
    /// not measure).
    ///
    /// **Falsified** by making `Snapshot.cpu` an `f32` again: the headline
    /// reads `0%` and the first assertion below reds.
    #[test]
    fn the_seed_render_is_dashes_not_invented_numbers() {
        let widgets = Widgets::default();
        let tree = card(Card::sidebar_default(), &Snapshot::default(), &widgets);
        let mut texts = Vec::new();
        collect_text(&tree, &mut texts);
        assert!(
            texts.iter().any(|t| t == "CPU"),
            "the header is there: {texts:?}",
        );
        assert!(
            texts.iter().any(|t| t == "—"),
            "and the load reads a dash rather than a number it did not measure: {texts:?}",
        );
        assert!(
            !texts.iter().any(|t| t == "0%"),
            "…and in particular not zero, which is a measurement: {texts:?}",
        );
        assert!(
            preem_ids(Card::sidebar_default(), &Snapshot::default(), &widgets)
                .contains(&"stats-cores-0".to_owned()),
            "and the lamp row holds its slot",
        );
    }

    /// The headline and the lamp row agree, frame by frame, about whether the
    /// card knows anything yet — the invariant LOW 5 was a violation of.
    #[test]
    fn the_headline_and_the_lamp_row_never_disagree_about_what_is_known() {
        let widgets = Widgets::default();
        for (snapshot, known) in [
            (Snapshot::default(), false),
            (
                Snapshot {
                    cpu: None,
                    per_core: Vec::new(),
                    cpu_temp_c: Some(50.0),
                    ..Snapshot::default()
                },
                false,
            ),
            (busy_snapshot(), true),
        ] {
            let tree = card(Card::sidebar_default(), &snapshot, &widgets);
            let mut texts = Vec::new();
            collect_text(&tree, &mut texts);
            // `header` emits [name, reading], and the CPU header is first — so
            // this is the CPU reading and not a GPU gauge that happens to be
            // dashed for a reason of its own.
            assert_eq!(texts.first().map(String::as_str), Some("CPU"));
            let headline_dashes = texts.get(1).map(String::as_str) == Some("—");
            let row_dashes = lamp_rows(&snapshot.per_core) == vec!["----".to_owned()];
            assert_eq!(
                headline_dashes, row_dashes,
                "known={known}: headline and row must make the same claim ({texts:?})",
            );
        }
    }

    /// **The compat arm**: against a host that never advertised the preem
    /// vocabulary the very same card comes out as CPU-rasterised
    /// `Node::Pixels`, with the same ids in the same order — the promise the
    /// SDK's `display` wrappers make, exercised here because this plugin has
    /// no `view` of its own for that case.
    ///
    /// **Falsified** by reaching past the wrappers and building `Node::Preem`
    /// by hand, which would emit a frame an older shell cannot decode and put
    /// the plugin in the #437 redial crash-loop.
    #[test]
    fn an_unadvertised_host_gets_the_same_card_as_pixels() {
        let widgets = Widgets::default();
        let snapshot = busy_snapshot();

        let state_ids = preem_ids(Card::sidebar_default(), &snapshot, &widgets);
        let (raster_ids, kinds) = with_render_mode(RenderMode::Raster, || {
            let tree = card(Card::sidebar_default(), &snapshot, &widgets);
            let mut ids = Vec::new();
            walk(&tree, &mut ids);
            let mut kinds = Vec::new();
            kind_names(&tree, &mut kinds);
            (ids, kinds)
        });

        assert_eq!(
            state_ids, raster_ids,
            "the same widgets, keyed the same way"
        );
        assert!(
            kinds.iter().all(|k| *k == "Pixels"),
            "an unadvertised host must receive pixels, never state: {kinds:?}",
        );
        assert!(!kinds.is_empty(), "…and the card is not simply empty");
    }

    /// The wire kind of every widget node in a tree, in order.
    fn kind_names(node: &Node, out: &mut Vec<&'static str>) {
        match node {
            Node::Preem { .. } => out.push("Preem"),
            Node::Pixels { .. } => out.push("Pixels"),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    kind_names(child, out);
                }
            }
            _ => {}
        }
    }

    fn collect_text(node: &Node, out: &mut Vec<String>) {
        match node {
            // A bar chip's content hangs off a `Button`, so a walker that only
            // descends containers sees an empty tree for `chips`.
            Node::Button { child, .. } => collect_text(child, out),
            Node::Label { text, .. } => out.push(text.clone()),
            Node::Box { children, .. } | Node::Row { children, .. } => {
                for child in children {
                    collect_text(child, out);
                }
            }
            _ => {}
        }
    }

    /// The lamp row is re-fitted once when the core count changes, and not
    /// again — a per-render rebuild would restate the widget's config on every
    /// frame.
    #[test]
    fn the_lamp_row_is_refitted_once_per_core_count() {
        let mut widgets = Widgets::default();
        let before = format!("{:?}", widgets.cores);
        widgets.fit_cores(&[0.5; 16]);
        let after = format!("{:?}", widgets.cores);
        assert_ne!(before, after, "16 cores is a different pitch than 4 dashes");
        widgets.fit_cores(&[0.5; 16]);
        assert_eq!(
            format!("{:?}", widgets.cores),
            after,
            "re-fitting to the same count must not rebuild the widget",
        );
        // …and a 32-thread box is the same 16-wide row, so it is the same
        // widget: no rebuild, and above all no narrower pitch (#1277 MEDIUM 1).
        widgets.fit_cores(&[0.5; 32]);
        assert_eq!(
            format!("{:?}", widgets.cores),
            after,
            "past the wrap boundary the row is still 16 wide — same pitch, same widget",
        );
    }

    /// **The drawer page's disk lamps draw at the page's own pitch, not the
    /// bar's floor** (#1295 review LOW 4) —
    /// `fit_cores_fits_the_card_row_and_the_page_row_separately`'s twin for
    /// the Disks card.
    ///
    /// **Falsified** by pointing `fit_disk_lamps` at [`dot_px_for`] (the
    /// bar/sidebar budget) instead of [`dot_px_for_in`]`(`[`PAGE_PX`]`, …)`:
    /// `left: Some(3) / right: Some(6)`.
    #[test]
    fn fit_disk_lamps_fits_the_page_row_not_the_bars_floor() {
        let mut widgets = Widgets::default();
        // Sixteen mounts, not two: at a small cell count both `CARD_PX` and
        // `PAGE_PX` clamp to the kit's shared `MAX_DOT_PX` ceiling and the two
        // budgets are indistinguishable by coincidence (measured: a two-mount
        // row saturates both to 8 px and this test would stay green under the
        // LOW 4 mutation) — the same trap `row_cells`'s own doc names for the
        // per-core row, and the reason `CORES_PER_ROW`-sized fixtures are used
        // there too.
        let usages = vec![0.5_f32; CORES_PER_ROW];
        widgets.fit_disk_lamps(&usages);

        with_render_mode(RenderMode::State, || {
            let page_pitch = node_dot_px(
                &widgets.page_disk_lamp_node(
                    "stats-panel-disk-lamps",
                    cls("ts-disk"),
                    &lamp_rows(&usages).join(" "),
                ),
                "stats-panel-disk-lamps",
            );
            let bar_pitch = node_dot_px(
                &widgets.lamp_node(
                    "stats-disk-lamps",
                    cls("ts-disk"),
                    &lamp_rows(&usages).join(" "),
                ),
                "stats-disk-lamps",
            );

            assert_eq!(bar_pitch, Some(CHIP_DOT_PX), "left: {bar_pitch:?}");
            assert_eq!(
                page_pitch,
                Some(dot_px_for_in(PAGE_PX, row_cells(&usages))),
                "right: {page_pitch:?}",
            );
            assert!(
                page_pitch > bar_pitch,
                "the page's disk row must be the chunkier one: \
                 left: {bar_pitch:?} / right: {page_pitch:?}",
            );
        });
    }

    /// **An all-off `[bar]` table renders nothing** (#1295 review LOW 6) — not
    /// an empty `stats-chips` pill, which the host would still wrap in
    /// `.ts-plugin-chip`'s background and padding.
    ///
    /// **Falsified** by dropping the `children.is_empty()` guard in `chips`:
    /// the tree is then an empty `Node::Box` and this test's first assertion
    /// reds.
    #[test]
    fn an_all_off_bar_configuration_renders_nothing() {
        let cfg = Card {
            cpu: false,
            memory: false,
            disk: false,
            gpu: false,
            ..Card::bar_default()
        };
        let node = chips(cfg, &busy_snapshot(), &Widgets::default());
        assert_eq!(node, Node::Spacer, "an all-off bar draws no pill at all");

        // A single switch left on is not "all off" — the ordinary one-chip
        // case must still draw its `Node::Box`, not fall into the same guard.
        let one_on = Card { cpu: true, ..cfg };
        assert_ne!(
            chips(one_on, &busy_snapshot(), &Widgets::default()),
            Node::Spacer,
        );
    }
}
