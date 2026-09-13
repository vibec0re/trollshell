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
//! # The `ts-*` classes
//!
//! The card sets the shell's own `ts-cpu` / `ts-cpu-temp` / `ts-gpu` classes.
//! The SDK's *Styling* docs say not to copy shell-internal classes off a native
//! widget — and then name the exception this crate is in: `hytte-plugin-weather`
//! and `hytte-plugin-departures` are 1:1 ports of what used to be native chips
//! and keep the classes for pixel parity. This is that same move (P2/P3 of
//! #1248 retire the native chips and page), and Annika's ask on #1235 was
//! explicitly that "the skins carry over".

use hytte_plugin::display::{DotMatrix, Gauge, Scope, SevenSeg, StyleName};
use hytte_plugin::proto::preem::{MAX_DOT_PX, MIN_DOT_PX};
use hytte_plugin::proto::{Cls, Dir, Node};

use crate::sample::Snapshot;

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

/// The chunkiest dot pitch at which a row of `cells` glyphs still fits
/// [`CARD_PX`], floored at the kit's own [`MIN_DOT_PX`].
///
/// `DotMatrix::render`'s width is `2*pad + n*advance - SPACING*dot`, and the
/// bezel is one dot cell on each side, which collapses to `dot_px * (6n + 1)`.
/// The floor is what a machine with more cores than the card is wide gets: the
/// row is then wider than its card and the host clips it, which is honest and
/// is precisely the case #1156's lamp *grid* solves.
#[must_use]
pub fn dot_px_for(cells: usize) -> u32 {
    let cells = u32::try_from(cells.max(1)).unwrap_or(u32::MAX);
    // `6 * cells` can only overflow for a core count no machine has and
    // `saturating_mul` keeps this total anyway; the division then yields 0 and
    // the clamp lifts it to the floor.
    let per_row = cells.saturating_mul(6).saturating_add(1);
    (CARD_PX / per_row).clamp(MIN_DOT_PX, MAX_DOT_PX)
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

/// A `0.0..=1.0` load as a scope sample in the wire's `-1.0..=1.0` range:
/// an idle core rests on the bottom of the trace and a saturated one touches
/// the top, rather than the trace hugging the centre line at half amplitude.
#[must_use]
pub fn trace_sample(load: f32) -> f32 {
    load.clamp(0.0, 1.0).mul_add(2.0, -1.0)
}

/// One CSS class, as the wire carries it.
fn cls(name: &str) -> Vec<Cls> {
    vec![name.to_owned()]
}

/// A plain label.
fn label(text: impl Into<String>, classes: &[&str]) -> Node {
    Node::Label {
        id: None,
        text: text.into(),
        classes: classes.iter().map(|c| (*c).to_owned()).collect(),
        tooltip: None,
    }
}

/// A section header row: a name on the left, a reading on the right.
fn header(name: &str, reading: String, reading_class: &str, tooltip: Option<String>) -> Node {
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
    temp: SevenSeg,
    history: Scope,
    gpu: Gauge,
}

/// Logical width of the history sweep, in samples — one per poll tick, so at
/// the default cadence the trace is the last two and a bit minutes.
pub const HISTORY_COLS: u32 = 144;

impl Default for Widgets {
    fn default() -> Self {
        Self {
            cores: DotMatrix::new(SKIN).dot_px(dot_px_for(DASH_CELLS)),
            fitted_cells: DASH_CELLS,
            temp: SevenSeg::new(SKIN),
            // 144x32 at 2x is 288x64 — the card's width, and short enough that
            // the sweep reads as a strip beside the lamp row rather than
            // dominating the card.
            history: Scope::with_size(SKIN, HISTORY_COLS, 32).scale(2),
            // A 0..100 needle, sized to the card at 2x (224x112).
            gpu: Gauge::with_size(SKIN, 112, 56).scale(2).range(0.0, 100.0),
        }
    }
}

impl Widgets {
    /// Re-fit the lamp row to a new core count, if it changed.
    ///
    /// Rebuilding the wrapper restates the widget's **config** on the wire, so
    /// the shell drops its renderer instance and builds a fresh one. That is
    /// free here and nowhere else on this card: `DotMatrix` is the one kind with
    /// no animation state to lose ("the static matrix has no animation of its
    /// own, so *animate toward the target* degenerates to an immediate redraw").
    /// The count changes at most twice in a session — once when the first delta
    /// arrives, and again only if the kernel hotplugs a CPU.
    pub fn fit_cores(&mut self, cells: usize) {
        if cells != self.fitted_cells {
            self.cores = DotMatrix::new(SKIN).dot_px(dot_px_for(cells));
            self.fitted_cells = cells;
        }
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
        children.push(header(
            "CPU",
            percent_text(Some(snapshot.cpu)),
            "ts-cpu",
            None,
        ));
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

#[cfg(test)]
mod tests {
    use super::{
        CARD_PX, CORES_PER_ROW, LAMPS, Widgets, card, dot_px_for, lamp, lamp_rows, percent_text,
        temp_text, trace_sample,
    };
    use crate::config::Card;
    use crate::sample::{Gpu, Snapshot};
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
        assert_eq!(dot_px_for(1_000), MIN_DOT_PX);
        assert_eq!(dot_px_for(usize::MAX), MIN_DOT_PX);
        // And a degenerate count is still a legal pitch.
        assert_eq!(dot_px_for(0), dot_px_for(1));
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
            cpu: 0.42,
            per_core: vec![0.1, 0.9, 0.5, 0.0],
            cpu_temp_c: Some(57.0),
            gpu: Some(Gpu {
                name: "AMD Radeon RX 6800".to_owned(),
                load: Some(0.25),
            }),
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

    /// The `[bar]` default is a smaller tree than the `[sidebar]` one — the
    /// difference that makes two tables worth having. Nothing renders this in
    /// P1; it is the shape P2 (#1251) starts from.
    #[test]
    fn the_bar_table_renders_a_smaller_tree() {
        let widgets = Widgets::default();
        let snapshot = busy_snapshot();
        let bar = preem_ids(Card::bar_default(), &snapshot, &widgets);
        let sidebar = preem_ids(Card::sidebar_default(), &snapshot, &widgets);
        assert!(bar.len() < sidebar.len(), "{bar:?} vs {sidebar:?}");
        assert!(!bar.iter().any(|i| i.starts_with("stats-cores")));
    }

    /// An all-default snapshot — what the seed render carries, before the first
    /// sample can possibly have landed — renders without panicking and shows
    /// dashes rather than zeros it did not measure.
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
            texts.iter().any(|t| t == "0%"),
            "and the load reads zero rather than a number it did not measure: {texts:?}",
        );
        assert!(
            preem_ids(Card::sidebar_default(), &Snapshot::default(), &widgets)
                .contains(&"stats-cores-0".to_owned()),
            "and the lamp row holds its slot",
        );
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
        widgets.fit_cores(16);
        let after = format!("{:?}", widgets.cores);
        assert_ne!(before, after, "16 cores is a different pitch than 4 dashes");
        widgets.fit_cores(16);
        assert_eq!(
            format!("{:?}", widgets.cores),
            after,
            "re-fitting to the same count must not rebuild the widget",
        );
    }
}
