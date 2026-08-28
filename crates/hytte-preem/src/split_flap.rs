//! The **split-flap board** and the **nixie readout**: a fixed row of
//! character cells that *change* by a visible mechanism (#397, after
//! [`Gauge`](super::Gauge) and the [`Marquee`](super::Marquee)'s fixed-grid
//! rework).
//!
//! Two mechanisms over one renderer, selected by [`Mechanism`] — the same
//! stance [`DisplayStyle`] takes on skins, one layer up: the *skin* is the
//! panel's palette, the *mechanism* is how a character is replaced.
//!
//! - [`Mechanism::SplitFlap`] — the airport board. The cell's upper card
//!   hinges down over the lower one: the outgoing glyph's top half folds away
//!   (revealing the incoming glyph's top behind it), passes horizontal, and
//!   the incoming glyph's bottom half folds in over the outgoing one. Cells
//!   ripple left to right on a small per-cell stagger.
//! - [`Mechanism::Nixie`] — the glow tube. The outgoing cathode's discharge
//!   collapses while the incoming one strikes, both alight at once for a
//!   moment, with a wide soft halo over the skin's own bloom. Simultaneous
//!   across cells: a tube bank has no ripple.
//!
//! # The fixture is fixed; only the cards move (#839's lesson)
//!
//! The [`Marquee`](super::Marquee) rework established that what is *physical*
//! must not move with what is *content*: its dot grid is nailed to the buffer
//! and the message steps across it in whole dots. The same split runs here.
//!
//! The **fixture** — the bezel, the card row, the gaps between cards, the slot
//! at each hinge, the nixie's unlit cathode stack — is painted at fixed buffer
//! positions and is byte-identical in every frame of a flip (the module's
//! `the_fixture_never_moves_while_the_cards_do` test asserts exactly that).
//! The **content** is font-space: a 5×7 [`font`] glyph sampled into the card,
//! [`glyph_px`](FlipBoard::glyph_px) logical pixels per font pixel, at the
//! kit's usual integer [`scale`](FlipBoard::scale).
//!
//! What *does* move is the falling card, and it genuinely moves between grid
//! rows — a rotation is not a translation, so quantizing it to whole dots (as
//! the marquee's scroll must be) would turn a fold into a shutter. The fold is
//! therefore an **area resample**: a destination row takes the exact
//! coverage-weighted average of the source rows it now spans, and a partly
//! covered row blends the card over what is behind it. That is the same answer
//! [`Gauge`](super::Gauge) gives for a needle at an arbitrary angle — sub-pixel
//! geometry becomes sub-pixel *intensity*, on a logical grid, upscaled to chunk
//! afterwards.
//!
//! # Animation is a closed-form function of the clock
//!
//! There is no per-frame integration anywhere. Each cell stamps the board's
//! clock when its flip begins ([`set_text`](FlipBoard::set_text), offset by its
//! stagger) and every frame is
//!
//! ```text
//! p = clamp((now - started) / duration, 0, 1)
//! ```
//!
//! with the mechanism's shape a pure function of `p`. Frame-rate independence
//! is therefore **structural**, not guarded: the frame at clock `t` is the same
//! frame whatever schedule of [`advance`](FlipBoard::advance) steps got there,
//! and [`advance_to`](FlipBoard::advance_to) lets a caller that owns a
//! monotonic timeline skip the accumulator entirely. The kit still owns no
//! clock (see the `preem` module docs on timing): the plugin measures elapsed
//! time and hands it over, exactly as it does for the gauge's physics and the
//! marquee's offset.
//!
//! ## The flap accelerates
//!
//! The rotation is `θ(p) = π·p²` — **constant angular acceleration**. A flap is
//! released and falls: it is slowest at the top, whips through horizontal, and
//! slams into its stop. A linear `θ(p)` reads as a motor-driven shutter, and is
//! the strawman the module's `the_card_falls_rather_than_being_driven` test
//! rules out. The visible squash is the honest foreshortening `|cos θ|` of that
//! rotation, not a linear scale, so the card is nearly full height for most of
//! the first half and vanishes fast.
//!
//! A consequence worth knowing: horizontal (`θ = π/2`) falls at `p = 1/√2`, so
//! the outgoing card's fall occupies ~71% of the flip and the incoming card's
//! ~29%. That is the airport-board cadence.
//!
//! # Retargeting: the flap in flight always lands
//!
//! Setting a character on a cell that is **mid-flip** keeps the cell's clock and
//! its outgoing glyph and only swaps the *destination*. The mechanism is
//! therefore never interrupted — angle, squash and shading stay a continuous
//! function of the one start stamp, and the flip lands on the newest target at
//! the moment it was always going to land. Nothing teleports, because nothing
//! is restarted.
//!
//! This is also what the hardware does, in both mechanisms. A Solari drum
//! cannot stop mid-card; it flips *through* to whatever it has been told to
//! reach, and you watch the destination change behind the falling card. A nixie
//! tube's anode drives whichever cathode is selected: re-selecting mid-switch
//! simply strikes a different cathode while the previous one's discharge is
//! already collapsing.
//!
//! Setting a character a cell is already resting on, or already heading to, is a
//! total no-op — a board re-told the same time every second does not re-flip.
//!
//! # The drum
//!
//! A physical board can only show the cards on its drum. [`CHARSET`] is this
//! one's: digits, `A`–`Z`, space, and `- . : /`. Lowercase folds to uppercase
//! (a real drum has no lowercase), and anything off the drum renders as the
//! font's hollow [`NOTDEF`](font::NOTDEF) box — never a panic, and
//! deterministically the *same* card for every uncovered input.
//!
//! A real numeric nixie stacks ten cathodes and nothing else; this kit shares
//! one drum across both mechanisms so the same content renders either way.
//! The nixie's ghost layer is still the authentic thing: the **ten digit
//! cathodes stacked**, which is what you see in an unlit tube.
//!
//! ```
//! use hytte_preem::{DisplayStyle, FlipBoard, Mechanism};
//!
//! let mut board = FlipBoard::new(Mechanism::SplitFlap).cells(5);
//! board.set_text("12:34");
//! // The plugin owns the clock: pass the real elapsed seconds each frame.
//! board.advance(1.0 / 60.0);
//! let frame = board.render(DisplayStyle::Vfd);
//! assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
//! assert!(!board.is_settled(), "still flipping");
//! board.advance(5.0);
//! assert!(board.is_settled(), "the whole row has landed");
//! ```

use std::f32::consts::PI;

use super::font;
use super::frame::Frame;
use super::style::{Bloom, DisplayStyle, Emission, Palette, mix};

// ── Metrics ──────────────────────────────────────────────────────────────────

/// Default logical pixels per font pixel. At the kit's default
/// [`scale`](FlipBoard::scale) of 2 this puts a font pixel on a 4 px pitch —
/// the same pitch as the dot-matrix [`DOT`](super::dot_matrix), so a board
/// stacks flush with a ticker.
pub const DEFAULT_GLYPH_PX: usize = 2;
/// Smallest accepted [`glyph_px`](FlipBoard::glyph_px).
const MIN_GLYPH_PX: usize = 2;
/// Largest accepted [`glyph_px`](FlipBoard::glyph_px) — past this a board is
/// far wider than any surface the kit targets.
const MAX_GLYPH_PX: usize = 16;
/// Default integer upscale baked into the output ([`Frame::upscale`]).
const DEFAULT_SCALE: usize = 2;
/// Cells in a default board: `HH:MM:SS`.
const DEFAULT_CELLS: usize = 8;

/// Card padding around the glyph, in **font pixels** — so it scales with
/// [`glyph_px`](FlipBoard::glyph_px) and a bigger board stays in proportion.
const CARD_PAD_FX: usize = 1;
/// Gap between adjacent cards, in font pixels.
const BOARD_GAP_FX: usize = 1;
/// Bezel around the card row, in font pixels.
const BOARD_PAD_FX: usize = 1;

/// Default per-cell flip duration, in seconds. Fast enough that a whole
/// `HH:MM:SS` rollover (six cards, staggered) lands inside the one second a
/// clock has, slow enough to read as a mechanism.
pub const DEFAULT_FLIP_SECS: f32 = 0.38;
/// Default per-cell nixie cross-fade duration, in seconds — shorter than a
/// flap, because nothing has to physically travel.
pub const DEFAULT_FADE_SECS: f32 = 0.30;
/// Default per-cell stagger for [`Mechanism::SplitFlap`], in seconds: the
/// left-to-right ripple. Small — a board reads as one gesture, not a queue.
pub const DEFAULT_STAGGER_SECS: f32 = 0.055;

/// Shortest accepted duration/stagger, in seconds. A floor rather than `0`: a
/// zero duration would divide the progress by nothing.
const MIN_DURATION_SECS: f32 = 0.01;
/// Longest accepted duration, in seconds.
const MAX_DURATION_SECS: f32 = 10.0;
/// Longest accepted stagger, in seconds.
const MAX_STAGGER_SECS: f32 = 2.0;

// Intensities, of 255.
/// A lit font pixel.
const GLYPH_T: f32 = 255.0;
/// The upper card's face, mixed from the field toward the palette ghost.
const FACE_TOP_T: u16 = 255;
/// The lower card's face — a touch darker, because the light is above.
const FACE_BOTTOM_T: u16 = 205;
/// The nixie's unlit cathode stack, mixed from the field toward the ghost.
const CATHODE_T: u16 = 255;
/// Peak brightness of the falling card's lit free edge, at horizontal.
const EDGE_T: f32 = 255.0;
/// Thickness of that edge rule, in logical pixels.
const EDGE_PX: f32 = 1.0;
/// How dark a card gets when it is fully edge-on: the floor of the
/// `SHADE_FLOOR + (1 - SHADE_FLOOR)·|cos θ|` lambert-ish shading, which is
/// exactly `1.0` at both rest angles and so cannot disturb the endpoints.
const SHADE_FLOOR: f32 = 0.45;

/// How much wider than the skin's own bloom the nixie's halo reaches. The
/// tube's glow is a broad haze around a tight core, so the widget blooms twice:
/// this wide, weak pass first, then the palette's own on top. `Emission::bloom`
/// max-combines, so the core never dims.
///
/// It is also the widget's whole cost. Measured on the demo's 8-cell,
/// 260×44 px board: 20 µs/frame for a nixie on the bloom-free LCD skin, 92 µs
/// with both passes on VFD (the split flap, which blooms once, sits at 36 and
/// 63 µs). In line with the marquee's disclosed 105 µs at a comparable
/// geometry, and the lever if it ever matters is #844's bloom bounding rather
/// than anything here.
const NIXIE_HALO_RADIUS_BONUS: usize = 3;
/// Strength of that wide halo pass (of 256).
const NIXIE_HALO_STRENGTH: u16 = 64;

/// The board's drum: every card it physically carries. Lowercase folds onto it;
/// anything else renders as the font's notdef box.
pub const CHARSET: &str = " -.:/0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// The card a char off the [`CHARSET`] drum resolves to. `U+FFFD` is not on the
/// drum itself, so every uncovered input normalizes onto exactly this one card
/// and two different uncovered chars are the *same* card (no phantom flip).
const NOTDEF_CARD: char = '\u{fffd}';

// ── Mechanism ────────────────────────────────────────────────────────────────

/// How a [`FlipBoard`] cell replaces one character with the next.
///
/// A mechanism is orthogonal to the [`DisplayStyle`] skin: the skin is the
/// panel's palette and post-pass, the mechanism is the moving part. Every skin
/// renders every mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mechanism {
    /// The airport board: the upper card hinges down over the lower one, and
    /// cells ripple left to right.
    SplitFlap,
    /// The glow tube: the outgoing cathode's discharge collapses while the
    /// incoming one strikes, simultaneously across the whole row.
    Nixie,
}

impl Mechanism {
    /// Every mechanism, in the canonical demo-rotation order.
    pub const ALL: [Self; 2] = [Self::SplitFlap, Self::Nixie];

    /// The mechanism as a lowercase word (`"split-flap"` / `"nixie"`) — handy
    /// for labels and CSS class suffixes, matching [`DisplayStyle::name`].
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::SplitFlap => "split-flap",
            Self::Nixie => "nixie",
        }
    }

    /// The mechanism's default per-cell transition length, in seconds.
    #[must_use]
    pub fn default_duration_secs(self) -> f32 {
        match self {
            Self::SplitFlap => DEFAULT_FLIP_SECS,
            Self::Nixie => DEFAULT_FADE_SECS,
        }
    }

    /// The mechanism's default per-cell stagger, in seconds: the split-flap
    /// board ripples, a bank of tubes does **not** — they are wired in
    /// parallel and switch together.
    #[must_use]
    pub fn default_stagger_secs(self) -> f32 {
        match self {
            Self::SplitFlap => DEFAULT_STAGGER_SECS,
            Self::Nixie => 0.0,
        }
    }
}

// ── The board ────────────────────────────────────────────────────────────────

/// One character cell of the board: the card it is leaving, the card it is
/// heading to, and when this cell's transition begins on the board's clock.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cell {
    /// The outgoing card (drum-normalized).
    from: char,
    /// The incoming card, and what the cell rests on once settled.
    to: char,
    /// Board-clock time at which this cell's transition **starts** — the
    /// stagger is already folded in, so a cell whose stamp is in the future is
    /// simply waiting its turn and still shows [`from`](Self::from). `None`
    /// means at rest (never moved, or [`settle`](FlipBoard::settle)d).
    started: Option<f64>,
}

/// A **split-flap board / nixie readout**: a fixed row of character cells that
/// animate from one character to the next.
///
/// Pure view-side, like every other kit widget — content in, frames out. It
/// owns no timer: [`set_text`](Self::set_text) says *what*,
/// [`advance`](Self::advance) says *when*, and [`render`](Self::render) draws
/// the moment. The skin is taken at render time, not construction, so a live
/// host re-tint (#376) or a plugin's own rotation can re-skin a board
/// mid-flip without disturbing the mechanism — the same reason
/// [`Gauge`](super::Gauge) and [`Scope`](super::Scope) do it that way.
#[derive(Debug, Clone, PartialEq)]
pub struct FlipBoard {
    /// How a cell changes character.
    mechanism: Mechanism,
    /// Logical pixels per font pixel (always even — see [`Self::glyph_px`]).
    glyph_px: usize,
    /// Integer upscale baked into the rendered [`Frame`].
    scale: usize,
    /// Per-cell transition length, in seconds.
    duration: f32,
    /// Per-cell left-to-right stagger, in seconds.
    stagger: f32,
    /// The board's clock, in seconds since it was built. Advanced by the host;
    /// `f64` so a board left running for days keeps sub-millisecond resolution.
    now: f64,
    /// The fixed row of cells.
    cells: Vec<Cell>,
}

impl FlipBoard {
    /// A board of [`DEFAULT_CELLS`] blank cells driven by `mechanism`, at the
    /// kit's default metrics and the mechanism's own default timings.
    #[must_use]
    pub fn new(mechanism: Mechanism) -> Self {
        Self {
            mechanism,
            glyph_px: DEFAULT_GLYPH_PX,
            scale: DEFAULT_SCALE,
            duration: mechanism.default_duration_secs(),
            stagger: mechanism.default_stagger_secs(),
            now: 0.0,
            cells: vec![blank_cell(); DEFAULT_CELLS],
        }
    }

    /// Set the number of character cells — the board's *physical* width, which
    /// never changes afterwards: [`set_text`](Self::set_text) pads a short
    /// string with blanks and ignores anything past the last cell. A consuming
    /// builder; call it at construction (it rebuilds the row blank).
    #[must_use]
    pub fn cells(mut self, count: usize) -> Self {
        self.cells = vec![blank_cell(); count];
        self
    }

    /// Set the logical pixels per font pixel, clamped to
    /// [`MIN_GLYPH_PX`]`..=`[`MAX_GLYPH_PX`] and rounded **down to an even
    /// number**.
    ///
    /// Even is load-bearing, not fussiness: the hinge cuts a 7-row glyph
    /// through the middle of its centre row, at `3.5 × glyph_px` logical pixels
    /// from the glyph's top. That lands on a pixel boundary — so the two halves
    /// are whole pixel bands and the resting frame is an exact copy of the
    /// glyph — only when `glyph_px` is even. A consuming builder.
    #[must_use]
    pub fn glyph_px(mut self, px: usize) -> Self {
        self.glyph_px = px.clamp(MIN_GLYPH_PX, MAX_GLYPH_PX) / 2 * 2;
        self
    }

    /// Set the integer upscale baked into the output (clamped to at least 1) —
    /// the kit bakes chunkiness into the buffer rather than leaning on shell
    /// CSS (the `.caw-lcd` lesson, #313). A consuming builder.
    #[must_use]
    pub fn scale(mut self, factor: usize) -> Self {
        self.scale = factor.max(1);
        self
    }

    /// Set the per-cell transition length in seconds, clamped to
    /// [`MIN_DURATION_SECS`]`..=`[`MAX_DURATION_SECS`]; a non-finite value keeps
    /// the current one. Defaults to the
    /// [mechanism's](Mechanism::default_duration_secs). A consuming builder.
    #[must_use]
    pub fn duration_secs(mut self, secs: f32) -> Self {
        if secs.is_finite() {
            self.duration = secs.clamp(MIN_DURATION_SECS, MAX_DURATION_SECS);
        }
        self
    }

    /// Set the per-cell left-to-right stagger in seconds, clamped to
    /// `0.0..=`[`MAX_STAGGER_SECS`]; a non-finite value keeps the current one.
    /// `0.0` makes the whole row move together. Defaults to the
    /// [mechanism's](Mechanism::default_stagger_secs). A consuming builder.
    #[must_use]
    pub fn stagger_secs(mut self, secs: f32) -> Self {
        if secs.is_finite() {
            self.stagger = secs.clamp(0.0, MAX_STAGGER_SECS);
        }
        self
    }

    /// The mechanism this board runs.
    #[must_use]
    pub fn mechanism(&self) -> Mechanism {
        self.mechanism
    }

    /// The content the board is resting on, or heading to: one char per cell,
    /// drum-normalized (uppercased, uncovered chars as `U+FFFD`). Always
    /// exactly [`cells`](Self::cells) chars long.
    #[must_use]
    pub fn target(&self) -> String {
        self.cells.iter().map(|c| c.to).collect()
    }

    /// Point the board at `text`.
    ///
    /// Each cell takes the corresponding char (drum-normalized: uppercased,
    /// anything off [`CHARSET`] becoming the notdef card); a short string pads
    /// with blanks and anything past the last cell is ignored. A cell already
    /// resting on — or already heading to — its char is left completely alone,
    /// clock included, so re-stating unchanged content never re-flips it.
    ///
    /// Cells that **do** start moving take the left-to-right stagger in the
    /// order they appear, counting only the cells that actually changed: a
    /// single changed digit moves at once, a whole-row change ripples.
    ///
    /// A cell that is **mid-flip** keeps its clock and its outgoing card and
    /// only swaps its destination — see the module docs on retargeting.
    pub fn set_text(&mut self, text: &str) {
        let (now, duration, stagger) = (self.now, self.duration, self.stagger);
        let mut incoming = text.chars();
        let mut moved = 0usize;
        for cell in &mut self.cells {
            let want = drum(incoming.next().unwrap_or(' '));
            if want == cell.to {
                continue;
            }
            let in_flight = cell
                .started
                .is_some_and(|started| now - started < f64::from(duration));
            if in_flight {
                // The flap in flight lands: keep the clock and the outgoing
                // card, swap only where it is going.
                cell.to = want;
            } else {
                cell.from = cell.to;
                cell.to = want;
                cell.started = Some(now + f64::from(stagger * fx(moved)));
                moved += 1;
            }
        }
    }

    /// Advance the board's clock by `dt` **seconds** of wall clock.
    ///
    /// The clock is all this moves: every cell's progress is a closed-form
    /// function of it (see the module docs), so there is no integration to
    /// drift and no timestep to destabilize. A non-positive or non-finite `dt`
    /// is a no-op — a clock that runs backwards would un-flip cards.
    pub fn advance(&mut self, dt: f32) {
        // A `NaN` fails `is_finite` first, so the comparison only ever sees a
        // real number.
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        self.now += f64::from(dt);
    }

    /// Move the board's clock **to** `secs`, for a caller that already owns a
    /// monotonic timeline (seconds since its own start) and would rather not
    /// keep a delta.
    ///
    /// This is the seam that makes frame-rate independence checkable at its
    /// strongest: the frame at a given absolute `secs` cannot depend on the
    /// schedule that got there, because no schedule is involved. Non-finite
    /// values and backward jumps are no-ops, exactly like a negative
    /// [`advance`](Self::advance).
    pub fn advance_to(&mut self, secs: f64) {
        if secs.is_finite() && secs > self.now {
            self.now = secs;
        }
    }

    /// Snap every cell onto the card it is heading to and stop the mechanism
    /// dead, keeping the geometry and timings a rebuilt [`new`](Self::new)
    /// would have thrown away.
    ///
    /// This is the **park** primitive (#422), the [`Scope::clear`] of a board.
    /// An off-screen widget stops being advanced, so without it a re-shown card
    /// resumes a stale flip from a stale angle; parking means the reopened
    /// board reads its content immediately and animates only the next *real*
    /// change.
    ///
    /// [`Scope::clear`]: super::Scope::clear
    pub fn settle(&mut self) {
        for cell in &mut self.cells {
            cell.from = cell.to;
            cell.started = None;
        }
    }

    /// Whether every cell has landed — nothing is mid-flip and nothing is
    /// waiting out its stagger.
    ///
    /// Worth polling: a plugin can drop its frame timer while this is true and
    /// re-arm it on the next [`set_text`](Self::set_text), since a settled
    /// board renders the same frame forever.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.cells.iter().all(|cell| self.progress(cell) >= 1.0)
    }

    /// The rendered frame width in px (logical columns × [`scale`](Self::scale)).
    #[must_use]
    pub fn width(&self) -> usize {
        self.logical_width() * self.scale
    }

    /// The rendered frame height in px (logical rows × [`scale`](Self::scale)).
    #[must_use]
    pub fn height(&self) -> usize {
        self.logical_height() * self.scale
    }

    /// [`advance`](Self::advance) then [`render`](Self::render) in one call —
    /// the convenience for a plugin that moves and re-renders on the same
    /// frame, matching [`Gauge::tick`](super::Gauge::tick).
    #[must_use]
    pub fn tick(&mut self, dt: f32, style: DisplayStyle) -> Frame {
        self.advance(dt);
        self.render(style)
    }

    /// Compose the current frame in `style`: the fixture (card faces or unlit
    /// cathode stacks, painted flat from the field toward the palette ghost)
    /// under the lit layer — the resting glyph halves and whatever card is
    /// falling across them — bloomed and composited toward the skin's
    /// accent-tinted ink (#376), then upscaled by [`scale`](Self::scale).
    ///
    /// The hinge slots are cut **last**, over everything: they are gaps in the
    /// fixture, so no glow crosses them on any skin. The buffer is fully opaque
    /// and always satisfies the host's `len == w * h * 4` invariant, for every
    /// input including an empty board.
    #[must_use]
    pub fn render(&self, style: DisplayStyle) -> Frame {
        let palette = style.palette();
        let (width, height) = (self.logical_width(), self.logical_height());
        let mut frame = Frame::filled(width, height, palette.bg);
        let mut lit = Emission::new(width, height);

        let bezel = self.bezel();
        for (index, cell) in self.cells.iter().enumerate() {
            let ox = bezel + index * (self.cell_w() + self.gap());
            self.paint_fixture(&mut frame, ox, bezel, &palette);
            match self.mechanism {
                Mechanism::SplitFlap => self.compose_flap(&mut lit, ox, bezel, cell),
                Mechanism::Nixie => self.compose_nixie(&mut lit, ox, bezel, cell),
            }
        }

        // The tube's broad haze goes on first, the skin's own tight bloom over
        // it; `Emission::bloom` max-combines, so the core never dims.
        if self.mechanism == Mechanism::Nixie
            && let Some(bloom) = palette.bloom
        {
            lit.bloom(Bloom {
                radius: bloom.radius + NIXIE_HALO_RADIUS_BONUS,
                strength: NIXIE_HALO_STRENGTH,
            });
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }
        lit.composite(&mut frame, palette.ink, palette.mask);

        // The slots, cut over the finished composite: a gap in the fixture is
        // a gap, so it stays visible even where a skin's bloom would have
        // filled it in.
        if self.mechanism == Mechanism::SplitFlap {
            for index in 0..self.cells.len() {
                let ox = bezel + index * (self.cell_w() + self.gap());
                let y = bezel + self.hinge();
                for x in ox..ox + self.cell_w() {
                    frame.set(x, y, palette.bg);
                }
            }
        }

        frame.upscale(self.scale)
    }

    // ── Geometry ─────────────────────────────────────────────────────────────

    /// One card's width in logical pixels: the glyph plus its padding.
    fn cell_w(&self) -> usize {
        (font::GLYPH_W + 2 * CARD_PAD_FX) * self.glyph_px
    }

    /// One card's height in logical pixels. Always even, so the hinge lands on
    /// a pixel boundary (see [`glyph_px`](Self::glyph_px)).
    fn cell_h(&self) -> usize {
        (font::GLYPH_H + 2 * CARD_PAD_FX) * self.glyph_px
    }

    /// The hinge's cell-local row: the card's midline, and equally the middle
    /// of the glyph's centre font row.
    fn hinge(&self) -> usize {
        self.cell_h() / 2
    }

    /// Bezel around the card row, in logical pixels.
    fn bezel(&self) -> usize {
        BOARD_PAD_FX * self.glyph_px
    }

    /// Gap between adjacent cards, in logical pixels.
    fn gap(&self) -> usize {
        BOARD_GAP_FX * self.glyph_px
    }

    /// Logical buffer width (pre-upscale).
    fn logical_width(&self) -> usize {
        let n = self.cells.len();
        if n == 0 {
            return 2 * self.bezel();
        }
        2 * self.bezel() + n * self.cell_w() + (n - 1) * self.gap()
    }

    /// Logical buffer height (pre-upscale).
    fn logical_height(&self) -> usize {
        2 * self.bezel() + self.cell_h()
    }

    // ── Timing ───────────────────────────────────────────────────────────────

    /// One cell's transition progress on the board's clock: `0.0` before it
    /// starts (including while it waits out its stagger), `1.0` once it has
    /// landed. The closed form the module docs describe, and the only place
    /// time enters a frame.
    #[allow(clippy::cast_possible_truncation)]
    fn progress(&self, cell: &Cell) -> f32 {
        let Some(started) = cell.started else {
            return 1.0;
        };
        let elapsed = self.now - started;
        if elapsed <= 0.0 {
            return 0.0;
        }
        let p = elapsed / f64::from(self.duration);
        if p >= 1.0 {
            1.0
        } else {
            // `p` is in `0.0..1.0` here, well inside `f32`'s exact range.
            p as f32
        }
    }

    // ── Painting ─────────────────────────────────────────────────────────────

    /// Paint one cell's fixture flat into the frame: the two card faces for a
    /// split-flap board, the unlit cathode stack for a nixie. Skipped whole on
    /// a skin without a ghost — an off OLED pixel emits nothing (#354).
    fn paint_fixture(&self, frame: &mut Frame, ox: usize, oy: usize, palette: &Palette) {
        let Some(ghost) = palette.ghost else {
            return;
        };
        match self.mechanism {
            Mechanism::SplitFlap => {
                let hinge = self.hinge();
                for y in 0..self.cell_h() {
                    let tone = if y < hinge { FACE_TOP_T } else { FACE_BOTTOM_T };
                    let color = mix(palette.bg, ghost, tone);
                    for x in 0..self.cell_w() {
                        frame.set(ox + x, oy + y, color);
                    }
                }
            }
            Mechanism::Nixie => {
                let color = mix(palette.bg, ghost, CATHODE_T);
                let stack = cathode_stack();
                self.stamp_glyph(stack, |x, y| frame.set(ox + x, oy + y, color));
            }
        }
    }

    /// Call `sink` for every logical pixel of a glyph's lit font pixels, in
    /// cell-local coordinates.
    fn stamp_glyph(&self, rows: [u8; font::GLYPH_H], mut sink: impl FnMut(usize, usize)) {
        let g = self.glyph_px;
        let pad = CARD_PAD_FX * g;
        for (row, &bits) in rows.iter().enumerate() {
            for col in 0..font::GLYPH_W {
                if (bits >> (font::GLYPH_W - 1 - col)) & 1 == 0 {
                    continue;
                }
                for dy in 0..g {
                    for dx in 0..g {
                        sink(pad + col * g + dx, pad + row * g + dy);
                    }
                }
            }
        }
    }

    /// Compose one **nixie** cell: the outgoing cathode's afterglow and the
    /// incoming one's strike, max-combined — two discharges in one envelope,
    /// so the brighter wins wherever their strokes coincide rather than
    /// summing into a blob.
    fn compose_nixie(&self, lit: &mut Emission, ox: usize, oy: usize, cell: &Cell) {
        let p = self.progress(cell);
        let (out, incoming) = (level(afterglow(p) * GLYPH_T), level(ignite(p) * GLYPH_T));
        let (going, coming) = (rows_of(cell.from), rows_of(cell.to));
        let g = self.glyph_px;
        let pad = CARD_PAD_FX * g;
        for row in 0..font::GLYPH_H {
            for col in 0..font::GLYPH_W {
                let bit = font::GLYPH_W - 1 - col;
                let lit_by_out = (going[row] >> bit) & 1 == 1;
                let lit_by_in = (coming[row] >> bit) & 1 == 1;
                // Two discharges in one envelope: the brighter wins wherever
                // their strokes coincide. `Emission::add` *sums*, so the max is
                // resolved here and each pixel is stamped exactly once.
                let value = match (lit_by_out, lit_by_in) {
                    (true, true) => out.max(incoming),
                    (true, false) => out,
                    (false, true) => incoming,
                    (false, false) => continue,
                };
                if value == 0 {
                    continue;
                }
                for dy in 0..g {
                    for dx in 0..g {
                        lit.add(ox + pad + col * g + dx, oy + pad + row * g + dy, value);
                    }
                }
            }
        }
    }

    /// Compose one **split-flap** cell.
    ///
    /// The whole mechanism, in three lines of geometry:
    ///
    /// - the upper half always shows the **incoming** card's top, statically;
    /// - the lower half always shows the **outgoing** card's bottom, statically;
    /// - one falling card is drawn over them — the outgoing card's top folding
    ///   away above the hinge while `θ ≤ π/2`, then the incoming card's bottom
    ///   folding in below it.
    ///
    /// At `θ = 0` the falling card covers the upper half exactly, so the cell
    /// *is* the outgoing card's resting frame; at `θ = π` it covers the lower
    /// half exactly, so the cell *is* the incoming card's. Both endpoints fall
    /// out of the same code path rather than being special-cased, which is what
    /// makes them pixel-exact.
    fn compose_flap(&self, lit: &mut Emission, ox: usize, oy: usize, cell: &Cell) {
        let theta = flap_theta(self.progress(cell));
        let (sin, cos) = theta.sin_cos();
        // The foreshortening of a rotating card, not a linear scale.
        let squash = cos.abs();
        let falling_up = cos >= 0.0;

        let hinge = self.hinge();
        let mid = fx(hinge);
        // Both halves are `hinge` logical pixels tall.
        let (band_lo, band_hi) = if falling_up {
            (mid - squash * mid, mid)
        } else {
            (mid, mid + squash * mid)
        };
        let leaf = rows_of(if falling_up { cell.from } else { cell.to });
        let (top_rows, bottom_rows) = (rows_of(cell.to), rows_of(cell.from));
        let shade = SHADE_FLOOR + (1.0 - SHADE_FLOOR) * squash;
        let edge_peak = EDGE_T * sin;
        // The card's free edge: the top of a card folding up, the bottom of one
        // folding down. The rule always lies *inside* the band.
        let (edge_lo, edge_hi) = if falling_up {
            (band_lo, band_lo + EDGE_PX)
        } else {
            (band_hi - EDGE_PX, band_hi)
        };

        for y in 0..self.cell_h() {
            let (row_lo, row_hi) = (fx(y), fx(y) + 1.0);
            let cover = (row_hi.min(band_hi) - row_lo.max(band_lo)).max(0.0);
            let source = if cover > 0.0 && squash > 0.0 {
                let (lo, hi) = (row_lo.max(band_lo), row_hi.min(band_hi));
                if falling_up {
                    Some(((lo - band_lo) / squash, (hi - band_lo) / squash))
                } else {
                    Some((mid + (lo - mid) / squash, mid + (hi - mid) / squash))
                }
            } else {
                None
            };
            let edge = edge_peak * (row_hi.min(edge_hi) - row_lo.max(edge_lo)).max(0.0);
            let behind_rows = if y < hinge { top_rows } else { bottom_rows };

            for x in 0..self.cell_w() {
                let behind = self.glyph_at(behind_rows, x, y) * GLYPH_T;
                let value = match source {
                    Some((src_lo, src_hi)) => {
                        let card = self.glyph_coverage(leaf, x, src_lo, src_hi) * GLYPH_T * shade;
                        // A partly covered row blends the card over what is
                        // behind it — the card *occludes*, it does not add.
                        cover.mul_add(card.max(edge) - behind, behind)
                    }
                    None => behind,
                };
                if value > 0.0 {
                    lit.add(ox + x, oy + y, level(value));
                }
            }
        }
    }

    /// Whether the glyph is lit at cell-local logical pixel (`x`, `y`) —
    /// `1.0` or `0.0`, the resting card's binary raster.
    fn glyph_at(&self, rows: [u8; font::GLYPH_H], x: usize, y: usize) -> f32 {
        let g = self.glyph_px;
        let pad = CARD_PAD_FX * g;
        let (Some(col), Some(row)) = (
            x.checked_sub(pad).map(|dx| dx / g),
            y.checked_sub(pad).map(|dy| dy / g),
        ) else {
            return 0.0;
        };
        if col >= font::GLYPH_W || row >= font::GLYPH_H {
            return 0.0;
        }
        f32::from((rows[row] >> (font::GLYPH_W - 1 - col)) & 1)
    }

    /// The lit fraction of `rows` over the cell-local **source** span
    /// `[lo, hi)` at logical column `x`: the exact area average of the font
    /// rows that span covers, normalized by its own length.
    ///
    /// This is the fold's resample. It degenerates *exactly* at rest: an
    /// unsquashed card maps one destination row onto one source row, so the
    /// average is that row's single bit and the frame is a bit-for-bit copy of
    /// the resting glyph. Source outside the glyph (the card's padding) counts
    /// as unlit, which is why a compressed card dims rather than smearing its
    /// bezel into the ink.
    fn glyph_coverage(&self, rows: [u8; font::GLYPH_H], x: usize, lo: f32, hi: f32) -> f32 {
        let span = hi - lo;
        if span <= 0.0 {
            return 0.0;
        }
        let g = self.glyph_px;
        let pad = CARD_PAD_FX * g;
        let Some(col) = x
            .checked_sub(pad)
            .map(|dx| dx / g)
            .filter(|&c| c < font::GLYPH_W)
        else {
            return 0.0;
        };
        let bit = font::GLYPH_W - 1 - col;
        // Into glyph-local coordinates, clipped to the glyph's own band.
        let (lo, hi) = (
            (lo - fx(pad)).max(0.0),
            (hi - fx(pad)).min(fx(font::GLYPH_H * g)),
        );
        if hi <= lo {
            return 0.0;
        }
        let gf = fx(g);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        // `lo` is clamped to `0.0..glyph height` above, so the floor is a small
        // in-range index and the cast is exact.
        let mut row = (lo / gf).floor() as usize;
        let mut acc = 0.0;
        while row < font::GLYPH_H {
            let (row_lo, row_hi) = (fx(row * g), fx((row + 1) * g));
            if row_lo >= hi {
                break;
            }
            if (rows[row] >> bit) & 1 == 1 {
                acc += (hi.min(row_hi) - lo.max(row_lo)).max(0.0);
            }
            row += 1;
        }
        acc / span
    }
}

// ── Free functions ───────────────────────────────────────────────────────────

/// A blank cell at rest.
fn blank_cell() -> Cell {
    Cell {
        from: ' ',
        to: ' ',
        started: None,
    }
}

/// Normalize `c` onto the board's [`CHARSET`] drum: uppercase (a real drum has
/// no lowercase), or the single [`NOTDEF_CARD`] for anything the drum does not
/// carry.
fn drum(c: char) -> char {
    let upper = c.to_ascii_uppercase();
    if CHARSET.contains(upper) {
        upper
    } else {
        NOTDEF_CARD
    }
}

/// The 5×7 bitmap a card shows.
fn rows_of(card: char) -> [u8; font::GLYPH_H] {
    if card == NOTDEF_CARD {
        return font::NOTDEF;
    }
    *font::glyph(card).unwrap_or(&font::NOTDEF)
}

/// The unlit cathode stack a nixie shows: all ten digits at once, which is
/// exactly what the wire meshes behind the lit one look like.
///
/// Derived from the font rather than hand-drawn, so it can never drift from
/// what the tube actually lights. At this font's digit coverage the ten
/// overlap into a solid mesh — which is what a cathode stack looks like
/// head-on, and dim enough (it is the palette's ghost) to read as depth.
fn cathode_stack() -> [u8; font::GLYPH_H] {
    let mut out = [0u8; font::GLYPH_H];
    for digit in '0'..='9' {
        for (slot, bits) in out.iter_mut().zip(rows_of(digit)) {
            *slot |= bits;
        }
    }
    out
}

/// The falling card's rotation at progress `p`: `π·p²`, i.e. **constant
/// angular acceleration** from rest.
///
/// A flap is released and falls — slowest at the top, fastest as it slams into
/// its stop. A linear `θ(p)` would be a motor turning at a constant rate, which
/// is a shutter, not a card. See the module docs.
fn flap_theta(p: f32) -> f32 {
    PI * p * p
}

/// The striking cathode's ignition curve: fast off the mark (`ignite'(0) = 2`),
/// easing into full brightness. Exactly `0.0` at `p = 0` and `1.0` at `p = 1`,
/// which is what keeps the cross-fade's endpoints pixel-exact.
fn ignite(p: f32) -> f32 {
    let left = 1.0 - p;
    1.0 - left * left
}

/// The extinguishing cathode's afterglow: it *lingers* (`afterglow'(0) = 0`)
/// before collapsing. Exactly `1.0` at `p = 0` and `0.0` at `p = 1`.
///
/// The asymmetry against [`ignite`] is the whole nixie look: at half-fade the
/// incoming cathode is already at 75% while the outgoing has only dropped to
/// 75%, so both are alight and the tube briefly glows *brighter* than either
/// digit alone — which is what two conducting cathodes actually do.
fn afterglow(p: f32) -> f32 {
    1.0 - p * p
}

/// A small buffer coordinate or count as an exact `f32`. Buffer sizes are far
/// below `u16::MAX`, and `u16 → f32` is lossless, so this needs no lossy cast.
fn fx(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// Round a `0.0..=255.0` intensity onto the emission's scale.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn level(value: f32) -> u16 {
    // The clamp bounds the value before the cast, so the truncation is exact
    // and never wraps; a `NaN` clamps to the low end and reads as unlit.
    value.clamp(0.0, 255.0).round() as u16
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_PAD_FX, CHARSET, DEFAULT_FLIP_SECS, DisplayStyle, FlipBoard, Frame, Mechanism,
        NOTDEF_CARD, PI, afterglow, cathode_stack, drum, flap_theta, font, fx, ignite, mix,
        rows_of,
    };

    /// A settled board of `n` cells showing `text` — the reference every
    /// endpoint assertion compares against.
    fn resting(mechanism: Mechanism, cells: usize, text: &str) -> FlipBoard {
        let mut board = FlipBoard::new(mechanism).cells(cells);
        board.set_text(text);
        board.settle();
        board
    }

    /// A board resting on `from`, told to show `to`, with its clock still at the
    /// instant of the change.
    fn changing(mechanism: Mechanism, cells: usize, from: &str, to: &str) -> FlipBoard {
        let mut board = resting(mechanism, cells, from);
        board.set_text(to);
        board
    }

    /// Seconds for every cell of a `cells`-wide board to have landed.
    fn whole_row_secs(board: &FlipBoard, cells: usize) -> f32 {
        board.duration + board.stagger * fx(cells.saturating_sub(1))
    }

    /// Linear cross-fade between two frames — the **fair strawman**: an
    /// implementation that animates by blending the two resting frames and
    /// nothing else. It hits both endpoints exactly, is a pure function of `p`,
    /// and shows a real intermediate. See
    /// `a_crossfade_cannot_show_two_glyphs_at_once_but_the_flap_does`.
    fn crossfade(a: &Frame, b: &Frame, p: f32) -> Frame {
        let mut out = a.clone();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let t = (p.clamp(0.0, 1.0) * 255.0).round() as u16;
        for y in 0..a.height() {
            for x in 0..a.width() {
                out.set(x, y, mix(a.at(x, y), b.at(x, y), t));
            }
        }
        out
    }

    /// One buffer row as bytes.
    fn row(frame: &Frame, y: usize) -> Vec<u8> {
        (0..frame.width()).flat_map(|x| frame.at(x, y)).collect()
    }

    // ── The host contract ────────────────────────────────────────────────────

    /// The wire invariant across mechanisms, skins, contents and instants —
    /// including an empty board and cards that are not on the drum.
    #[test]
    fn every_render_satisfies_the_host_invariant() {
        for mechanism in Mechanism::ALL {
            for cells in [0, 1, 5, 8] {
                for text in ["", " ", "12:34:56", "PREEM", "åäö 💕", "----"] {
                    let mut board = changing(mechanism, cells, "88:88:88", text);
                    for step in 0..8 {
                        let frame = board.render(DisplayStyle::ALL[step % 3]);
                        assert_eq!(
                            frame.data().len(),
                            frame.width() * frame.height() * 4,
                            "{mechanism:?} {cells} {text:?} @ {step}"
                        );
                        assert_eq!(frame.width(), board.width());
                        assert_eq!(frame.height(), board.height());
                        board.advance(0.07);
                    }
                }
            }
        }
    }

    /// Display widgets promise fully opaque frames — they are screens.
    #[test]
    fn every_pixel_is_opaque() {
        for mechanism in Mechanism::ALL {
            for style in DisplayStyle::ALL {
                let mut board = changing(mechanism, 4, "0000", "1234");
                board.advance(0.11);
                let frame = board.render(style);
                assert!(
                    frame.data().chunks_exact(4).all(|px| px[3] == 0xff),
                    "{mechanism:?}/{style:?} is opaque wall to wall"
                );
            }
        }
    }

    /// Renders are pure, and the three skins render every mechanism differently.
    #[test]
    fn render_is_deterministic_and_skins_differ() {
        for mechanism in Mechanism::ALL {
            let mut board = changing(mechanism, 3, "000", "888");
            board.advance(0.09);
            assert_eq!(
                board.render(DisplayStyle::Vfd),
                board.render(DisplayStyle::Vfd)
            );
            let vfd = board.render(DisplayStyle::Vfd);
            let lcd = board.render(DisplayStyle::Lcd);
            let oled = board.render(DisplayStyle::Oled);
            assert_ne!(vfd, lcd, "{mechanism:?}");
            assert_ne!(vfd, oled, "{mechanism:?}");
            assert_ne!(lcd, oled, "{mechanism:?}");
        }
        // The two mechanisms are visibly different machines on the same content.
        let mut flap = changing(Mechanism::SplitFlap, 3, "000", "888");
        let mut nixie = changing(Mechanism::Nixie, 3, "000", "888");
        flap.advance(0.1);
        nixie.advance(0.1);
        assert_ne!(
            flap.render(DisplayStyle::Vfd),
            nixie.render(DisplayStyle::Vfd)
        );
    }

    // ── (a) The endpoints are the resting frames, exactly ────────────────────

    /// **Test (a).** At `t = 0` the board is byte-identical to a settled board
    /// showing the *old* content, and once the whole row has landed it is
    /// byte-identical to one showing the *new* content.
    ///
    /// Exact, not approximate: at `θ = 0` the falling card covers the upper
    /// half with an unsquashed — so bit-for-bit resampled — copy of the
    /// outgoing glyph, and the edge highlight is scaled by `sin θ = 0`. The
    /// nixie's curves are exactly `1/0` and `0/1` at the ends. Neither is
    /// special-cased; both fall out of the one code path.
    #[test]
    fn the_endpoints_are_exactly_the_resting_frames() {
        for mechanism in Mechanism::ALL {
            for style in DisplayStyle::ALL {
                let mut board = changing(mechanism, 6, "123456", "ABCDEF");
                assert_eq!(
                    board.render(style),
                    resting(mechanism, 6, "123456").render(style),
                    "{mechanism:?}/{style:?}: t=0 is the old content, exactly"
                );
                assert!(!board.is_settled(), "and it has somewhere to go");
                board.advance(whole_row_secs(&board, 6) + 0.001);
                assert!(board.is_settled());
                assert_eq!(
                    board.render(style),
                    resting(mechanism, 6, "ABCDEF").render(style),
                    "{mechanism:?}/{style:?}: the far end is the new content, exactly"
                );
                // And it stays there however long the clock runs on.
                let landed = board.render(style);
                board.advance(60.0);
                assert_eq!(board.render(style), landed, "a landed board is still");
            }
        }
    }

    // ── (b) The animation is real ────────────────────────────────────────────

    /// **Test (b).** A mid-transition frame differs from *both* endpoints — the
    /// widget animates rather than snapping — for both mechanisms on every skin.
    #[test]
    fn a_mid_transition_frame_differs_from_both_endpoints() {
        for mechanism in Mechanism::ALL {
            for style in DisplayStyle::ALL {
                let old = resting(mechanism, 2, "07").render(style);
                let new = resting(mechanism, 2, "18").render(style);
                let mut board = changing(mechanism, 2, "07", "18");
                let mut seen = 0;
                for _ in 0..7 {
                    board.advance(0.04);
                    let frame = board.render(style);
                    if frame != old && frame != new {
                        seen += 1;
                    }
                }
                assert!(
                    seen >= 4,
                    "{mechanism:?}/{style:?}: only {seen} frames were neither endpoint"
                );
            }
        }
    }

    // ── (c) Frame-rate independence ──────────────────────────────────────────

    /// **Test (c).** The frame at a given clock reading is the same frame
    /// whatever schedule of steps produced it.
    ///
    /// The schedules use dyadic steps, which sum *exactly* in binary floating
    /// point, so this is a byte-equality rather than a tolerance — and that is
    /// the honest scope of the claim. The frame is a pure function of one
    /// number, the clock, so the only path-dependence a closed-form animation
    /// can have is in the caller's own summation of it. The
    /// [`advance_to`](FlipBoard::advance_to) leg removes even that — there is
    /// no accumulator in it at all — and lands on the same bytes.
    #[test]
    fn the_frame_follows_the_clock_not_the_step_schedule() {
        const T: f64 = 0.25;
        for mechanism in Mechanism::ALL {
            let build = || changing(mechanism, 6, "000000", "123456");

            let mut fine = build();
            for _ in 0..64 {
                fine.advance(1.0 / 256.0);
            }
            let mut coarse = build();
            for _ in 0..2 {
                coarse.advance(1.0 / 8.0);
            }
            let mut ragged = build();
            for dt in [1.0 / 8.0, 1.0 / 16.0, 1.0 / 32.0, 1.0 / 32.0] {
                ragged.advance(dt);
            }
            let mut absolute = build();
            absolute.advance_to(T);
            // Rendering along the way must not perturb anything either.
            let mut watched = build();
            for step in 1..=8 {
                watched.advance_to(T * f64::from(step) / 8.0);
                let _ = watched.render(DisplayStyle::Lcd);
            }

            let style = DisplayStyle::Vfd;
            let want = fine.render(style);
            assert_eq!(coarse.render(style), want, "{mechanism:?}: 2 steps == 64");
            assert_eq!(ragged.render(style), want, "{mechanism:?}: uneven steps");
            assert_eq!(
                absolute.render(style),
                want,
                "{mechanism:?}: no accumulator"
            );
            assert_eq!(watched.render(style), want, "{mechanism:?}: watched path");
            assert!(!want.data().is_empty());
        }
    }

    // ── (d) The ripple ───────────────────────────────────────────────────────

    /// **Test (d).** On a split-flap board a card to the left starts no later
    /// and lands no later than one to its right — the airport-board ripple —
    /// and the lead is real, not a tie.
    #[test]
    fn the_row_ripples_left_to_right() {
        let mut board = changing(Mechanism::SplitFlap, 6, "000000", "111111");
        let starts: Vec<f64> = board
            .cells
            .iter()
            .map(|c| c.started.expect("every cell changed"))
            .collect();
        for pair in starts.windows(2) {
            assert!(
                pair[0] < pair[1],
                "the ripple runs left to right: {starts:?}"
            );
        }
        board.advance(0.12);
        let progress: Vec<f32> = board.cells.iter().map(|c| board.progress(c)).collect();
        for pair in progress.windows(2) {
            assert!(
                pair[0] >= pair[1],
                "an earlier card leads a later one: {progress:?}"
            );
        }
        assert!(
            progress[0] > progress[5],
            "and the lead is real: {progress:?}"
        );

        // Only cards that actually change take a slot in the ripple: a lone
        // changed digit moves at once rather than waiting out its column index.
        let mut clock = resting(Mechanism::SplitFlap, 8, "12:34:56");
        clock.advance(4.0);
        let before = clock.now;
        clock.set_text("12:34:57");
        assert_eq!(
            clock.cells[7].started,
            Some(before),
            "the seconds card does not queue behind seven cards ahead of it"
        );
        assert!(
            clock.cells[..7].iter().all(|c| c.started.is_none()),
            "and the unchanged cards were not touched at all"
        );
    }

    /// A bank of tubes is wired in parallel: every cell switches together, with
    /// no ripple at all.
    #[test]
    fn a_tube_bank_switches_together() {
        assert!(Mechanism::Nixie.default_stagger_secs().abs() < f32::EPSILON);
        let board = changing(Mechanism::Nixie, 6, "000000", "111111");
        let starts: Vec<Option<f64>> = board.cells.iter().map(|c| c.started).collect();
        assert!(
            starts.windows(2).all(|w| w[0] == w[1]),
            "every tube strikes at once: {starts:?}"
        );
    }

    // ── (e) Retargeting ──────────────────────────────────────────────────────

    /// A one-cell board at ×1, so buffer coordinates *are* logical ones.
    fn probe(mechanism: Mechanism, from: &str) -> FlipBoard {
        let mut board = FlipBoard::new(mechanism)
            .cells(from.chars().count().max(1))
            .scale(1);
        board.set_text(from);
        board.settle();
        board
    }

    /// **Test (e).** Retargeting mid-flip neither panics nor teleports: the
    /// flap in flight keeps its clock and its outgoing card, so the mechanism's
    /// angle, squash and shading stay continuous and the flip lands on the new
    /// target at the moment it was always going to land.
    ///
    /// The anti-teleport assertion is exact rather than eyeballed: while the
    /// outgoing card is still falling (`θ < π/2`) the lower half of the cell is
    /// pure static outgoing glyph, so retargeting must leave every one of its
    /// rows byte-identical. On LCD, where nothing blooms across the split.
    #[test]
    fn retargeting_mid_flip_lands_the_card_in_flight() {
        let style = DisplayStyle::Lcd;
        let mut board = probe(Mechanism::SplitFlap, "1");
        board.set_text("8");
        // Late in the fall, but before horizontal: enough of the destination is
        // revealed above the card to *see* a retarget, while the lower half is
        // still pure static outgoing glyph.
        let probe_secs = 0.58 * board.duration;
        board.advance(probe_secs);
        let (started, before) = (board.cells[0].started, board.render(style));
        assert!(
            flap_theta(board.progress(&board.cells[0])) < PI / 2.0,
            "the probe instant is mid-fall, where the lower half is static"
        );

        board.set_text("5");
        assert_eq!(
            board.cells[0].started, started,
            "the clock is not restarted"
        );
        assert_eq!(
            board.cells[0].from, '1',
            "the falling card is still falling"
        );
        assert_eq!(board.cells[0].to, '5', "only the destination moved");

        let after = board.render(style);
        let hinge_row = board.bezel() + board.hinge();
        for y in hinge_row..after.height() {
            assert_eq!(row(&after, y), row(&before, y), "row {y} teleported");
        }
        assert_ne!(after, before, "but the destination behind it did change");

        // It still lands when it always would have, on the newest target.
        board.advance(board.duration - probe_secs + 1.0e-4);
        assert!(board.is_settled(), "the original deadline still holds");
        assert_eq!(
            board.render(style),
            probe(Mechanism::SplitFlap, "5").render(style)
        );
    }

    /// Re-stating content a cell is already resting on — or already heading to
    /// — is a total no-op: a board told the same time every second does not
    /// re-flip, and neither does one told, mid-flip, what it is already doing.
    #[test]
    fn restating_the_same_text_never_re_flips() {
        for mechanism in Mechanism::ALL {
            let mut board = resting(mechanism, 5, "12:34");
            board.advance(3.0);
            let before = board.clone();
            board.set_text("12:34");
            assert_eq!(
                board, before,
                "{mechanism:?}: a settled restatement is inert"
            );

            board.set_text("12:35");
            board.advance(0.05);
            let mid = board.clone();
            board.set_text("12:35");
            assert_eq!(board, mid, "{mechanism:?}: so is a mid-flip restatement");
        }
    }

    /// A storm of retargets — including ones that reverse the destination
    /// mid-flight — never panics, never leaves a cell stranded, and always
    /// converges on the last thing it was told.
    #[test]
    fn a_retarget_storm_converges() {
        for mechanism in Mechanism::ALL {
            let mut board = resting(mechanism, 4, "0000");
            for step in 0..40u32 {
                board.set_text(&format!("{:04}", step * 7 % 10_000));
                board.advance(0.017);
                let _ = board.render(DisplayStyle::Vfd);
            }
            board.set_text("9142");
            board.advance(10.0);
            assert!(board.is_settled(), "{mechanism:?}");
            assert_eq!(board.target(), "9142");
            assert_eq!(
                board.render(DisplayStyle::Vfd),
                resting(mechanism, 4, "9142").render(DisplayStyle::Vfd),
                "{mechanism:?}: it lands on the last target, exactly"
            );
        }
    }

    // ── (f) The drum ─────────────────────────────────────────────────────────

    /// **Test (f).** Every card on the drum has a glyph; lowercase folds onto
    /// it; everything else resolves to one defined fallback card.
    #[test]
    fn the_drum_covers_its_charset_and_folds_everything_else() {
        for card in CHARSET.chars() {
            assert!(
                font::glyph(card).is_some(),
                "drum card {card:?} has a font glyph"
            );
            assert_eq!(drum(card), card, "a drum card normalizes to itself");
        }
        assert_eq!(drum('a'), 'A', "a real drum carries no lowercase");
        assert_eq!(drum('z'), 'Z');
        // Font-covered but *not on this drum* still falls back — the drum is
        // the board's physical card set, not the font's coverage.
        for off in ['~', '?', '(', 'ß', '💕', '\u{fffd}'] {
            assert_eq!(drum(off), NOTDEF_CARD, "{off:?} is not on the drum");
        }
        assert_eq!(rows_of(NOTDEF_CARD), font::NOTDEF, "the fallback is notdef");

        // …and the rendered consequence, on both mechanisms.
        for mechanism in Mechanism::ALL {
            let style = DisplayStyle::Vfd;
            assert_eq!(
                resting(mechanism, 3, "abc").render(style),
                resting(mechanism, 3, "ABC").render(style),
                "{mechanism:?}: case folds"
            );
            assert_eq!(
                resting(mechanism, 2, "💕~").render(style),
                resting(mechanism, 2, "~💕").render(style),
                "{mechanism:?}: every off-drum char is the same one card"
            );
            let board = resting(mechanism, 4, "ab");
            assert_eq!(board.target(), "AB  ", "short text pads with blanks");
            assert_eq!(
                resting(mechanism, 2, "12345").target(),
                "12",
                "and anything past the last cell is ignored"
            );
        }
    }

    // ── The strawman ─────────────────────────────────────────────────────────

    /// **The assertion a cross-fade cannot pass.**
    ///
    /// A fair strawman — linearly blending the two resting frames — passes
    /// tests (a), (b) and (c) outright: it hits both endpoints exactly, shows a
    /// real intermediate, and is a pure function of progress. This test builds
    /// that strawman and shows what separates it from a mechanism.
    ///
    /// A split flap has a moment, as the outgoing card passes horizontal, where
    /// the cell shows **two different glyphs at once**: the incoming card's top
    /// half above the hinge (already revealed) and the outgoing card's bottom
    /// half below it (not yet covered) — both at full, un-blended brightness. A
    /// cross-fade renders one blended glyph everywhere and can never do that.
    #[test]
    fn a_crossfade_cannot_show_two_glyphs_at_once_but_the_flap_does() {
        let style = DisplayStyle::Lcd; // no bloom: nothing crosses the hinge.
        let old = probe(Mechanism::SplitFlap, "1").render(style);
        let new = probe(Mechanism::SplitFlap, "8").render(style);

        // Horizontal is θ = π/2, i.e. p = 1/√2 for the flap's `π·p²`.
        let p = (0.5_f32).sqrt();
        let mut board = probe(Mechanism::SplitFlap, "1");
        board.set_text("8");
        board.advance(p * board.duration);
        let flap = board.render(style);
        let hinge_row = board.bezel() + board.hinge();

        for y in 0..hinge_row {
            assert_eq!(
                row(&flap, y),
                row(&new, y),
                "row {y}: above the hinge the incoming card is already there"
            );
        }
        for y in hinge_row..flap.height() {
            assert_eq!(
                row(&flap, y),
                row(&old, y),
                "row {y}: below it the outgoing card has not been covered yet"
            );
        }

        // The strawman: same endpoints, same purity, no mechanism.
        assert_eq!(crossfade(&old, &new, 0.0), old, "it does pass test (a) …");
        assert_eq!(crossfade(&old, &new, 1.0), new);
        let blended = crossfade(&old, &new, p);
        assert_ne!(blended, old, "… and test (b) …");
        assert_ne!(blended, new);
        assert_eq!(
            crossfade(&old, &new, p),
            blended,
            "… and (c), being a pure function of progress"
        );
        // …but it cannot be in two states at once.
        let mut top_matches_new = true;
        let mut bottom_matches_old = true;
        for y in 0..hinge_row {
            top_matches_new &= row(&blended, y) == row(&new, y);
        }
        for y in hinge_row..blended.height() {
            bottom_matches_old &= row(&blended, y) == row(&old, y);
        }
        assert!(
            !top_matches_new && !bottom_matches_old,
            "a cross-fade shows one blended glyph, never two real ones"
        );
    }

    // ── The mechanisms are the mechanisms ────────────────────────────────────

    /// The card **falls**: `θ(p) = π·p²` is constant angular acceleration, so
    /// it is slowest at the top and slams into its stop. The strawman here is a
    /// constant-rate driver (`θ = π·p`), which would be a motor turning a
    /// shutter — and which the same assertions rule out.
    #[test]
    fn the_card_falls_rather_than_being_driven() {
        let driven = |p: f32| PI * p;
        assert!(flap_theta(0.0).abs() < f32::EPSILON, "it starts upright");
        assert!(
            (flap_theta(1.0) - PI).abs() < 1.0e-6,
            "and finishes hanging"
        );
        // Half the time is nowhere near half the rotation.
        assert!(
            (flap_theta(0.5) - PI / 4.0).abs() < 1.0e-6,
            "θ(½) = π/4, not the driver's π/2"
        );
        assert!(
            (driven(0.5) - PI / 2.0).abs() < 1.0e-6,
            "the strawman is exactly halfway round at halfway through"
        );
        // The visible consequence: the outgoing card is still 71% of its height
        // at the halfway point, where a driven one would have vanished.
        assert!(
            (flap_theta(0.5).cos() - 0.5_f32.sqrt()).abs() < 1.0e-6,
            "still most of its height"
        );
        assert!(driven(0.5).cos().abs() < 1.0e-6, "the strawman is edge-on");
        // Monotone, and horizontal comes late.
        let mut previous = f32::MIN;
        let mut horizontal_at = f32::MAX;
        for step in 0..=1000 {
            let p = fx(step) / 1000.0;
            let theta = flap_theta(p);
            assert!(theta >= previous, "θ never runs backwards");
            previous = theta;
            if theta >= PI / 2.0 && horizontal_at > 1.0 {
                horizontal_at = p;
            }
        }
        assert!(
            horizontal_at > 0.70 && horizontal_at < 0.71,
            "horizontal falls at 1/√2, not halfway: {horizontal_at}"
        );
    }

    /// The tube **strikes fast and fades slow**: at every point of the
    /// transition the incoming cathode is further up than the outgoing one is
    /// down, so both are alight together and the tube briefly glows brighter
    /// than either digit alone.
    #[test]
    fn the_tube_strikes_faster_than_it_fades() {
        assert!(ignite(0.0).abs() < f32::EPSILON);
        assert!((ignite(1.0) - 1.0).abs() < f32::EPSILON);
        assert!((afterglow(0.0) - 1.0).abs() < f32::EPSILON);
        assert!(afterglow(1.0).abs() < f32::EPSILON);
        let (mut rising, mut falling) = (f32::MIN, f32::MAX);
        for step in 1..1000 {
            let p = fx(step) / 1000.0;
            assert!(
                ignite(p) > 1.0 - afterglow(p),
                "the strike leads the collapse at p={p}"
            );
            assert!(ignite(p) >= rising && afterglow(p) <= falling, "monotone");
            rising = ignite(p);
            falling = afterglow(p);
            assert!(ignite(p) + afterglow(p) > 1.0, "both cathodes conduct");
        }

        // The rendered consequence, on the bloom-free skin so only stamped
        // pixels can differ: mid-fade lights the union of the two digits.
        let style = DisplayStyle::Lcd;
        let blank = probe(Mechanism::Nixie, " ").render(style);
        let count = |frame: &Frame| {
            (0..frame.height())
                .flat_map(|y| (0..frame.width()).map(move |x| (x, y)))
                .filter(|&(x, y)| frame.at(x, y) != blank.at(x, y))
                .count()
        };
        let one = count(&probe(Mechanism::Nixie, "1").render(style));
        let zero = count(&probe(Mechanism::Nixie, "0").render(style));
        let mut board = probe(Mechanism::Nixie, "1");
        board.set_text("0");
        board.advance(board.duration / 2.0);
        let both = count(&board.render(style));
        assert!(one > 0 && zero > 0);
        assert!(
            both > one && both > zero,
            "mid-fade lights both cathodes ({both} vs {one}/{zero})"
        );
    }

    // ── The fixture ──────────────────────────────────────────────────────────

    /// The #839 discipline, one widget over: **the fixture never moves.** Light
    /// only ever lands inside a card — never in the bezel, never in the gap
    /// between two cards — at every instant of a flip, on the bloom-free skin
    /// where a lit pixel is the only thing that can change one.
    #[test]
    fn the_fixture_never_moves_while_the_cards_do() {
        let style = DisplayStyle::Lcd;
        for mechanism in Mechanism::ALL {
            let mut board = FlipBoard::new(mechanism).cells(4).scale(1);
            board.set_text("0000");
            board.settle();
            board.set_text("MW18");
            let field = DisplayStyle::Lcd.palette().bg;
            let (bezel, cell_w, cell_h, gap) =
                (board.bezel(), board.cell_w(), board.cell_h(), board.gap());
            let mut moved = false;
            for _ in 0..14 {
                let frame = board.render(style);
                for y in 0..frame.height() {
                    for x in 0..frame.width() {
                        let inside_rows = (bezel..bezel + cell_h).contains(&y);
                        let column = x
                            .checked_sub(bezel)
                            .map_or(usize::MAX, |dx| dx % (cell_w + gap));
                        let inside_cols =
                            x >= bezel && x < frame.width() - bezel && column < cell_w;
                        if !(inside_rows && inside_cols) {
                            assert_eq!(
                                frame.at(x, y),
                                field,
                                "{mechanism:?}: light at ({x},{y}) is off the card row"
                            );
                        }
                    }
                }
                moved |= !board.is_settled();
                board.advance(0.05);
            }
            assert!(moved, "{mechanism:?}: the cards really were moving");
        }
    }

    /// The hinge slots are gaps in the fixture, cut over the finished
    /// composite — so they stay dark on **every** skin, including the glowing
    /// ones whose bloom would otherwise have filled them in.
    #[test]
    fn the_hinge_slot_is_cut_on_every_skin() {
        for style in DisplayStyle::ALL {
            let field = style.palette().bg;
            let mut board = FlipBoard::new(Mechanism::SplitFlap).cells(3).scale(1);
            board.set_text("888");
            board.settle();
            board.set_text("000");
            for _ in 0..8 {
                let frame = board.render(style);
                let y = board.bezel() + board.hinge();
                for index in 0..3 {
                    let ox = board.bezel() + index * (board.cell_w() + board.gap());
                    for x in ox..ox + board.cell_w() {
                        assert_eq!(frame.at(x, y), field, "{style:?}: slot filled at {x}");
                    }
                }
                board.advance(0.05);
            }
        }
    }

    /// The kit's ghost rule: LCD and VFD show their unlit fixture — card faces,
    /// or the ten stacked cathodes of an unlit tube — while an OLED emits
    /// nothing at all (#354).
    #[test]
    fn lcd_ghosts_and_oled_does_not() {
        for mechanism in Mechanism::ALL {
            for style in [DisplayStyle::Lcd, DisplayStyle::Vfd] {
                let frame = resting(mechanism, 3, "   ").render(style);
                let field = style.palette().bg;
                assert!(
                    frame.data().chunks_exact(4).any(|px| px != field),
                    "{mechanism:?}/{style:?} shows its unlit fixture"
                );
            }
            let oled = resting(mechanism, 3, "   ").render(DisplayStyle::Oled);
            assert!(
                oled.data().chunks_exact(4).all(|px| px == [0, 0, 0, 0xff]),
                "{mechanism:?}: an unlit OLED emits nothing"
            );
        }
        // A nixie's ghost really is the ten digits stacked: every digit's
        // strokes lie inside it, and it is not merely the notdef box.
        let stack = cathode_stack();
        for digit in '0'..='9' {
            for (slot, bits) in stack.iter().zip(rows_of(digit)) {
                assert_eq!(slot & bits, bits, "digit {digit} lies inside the stack");
            }
        }
    }

    // ── Guards, geometry, resampling ─────────────────────────────────────────

    /// The one real guard: a clock that ran backwards would un-flip cards.
    /// Every nonsense step — negative, zero, non-finite — is a no-op, and so is
    /// a backward [`advance_to`](FlipBoard::advance_to).
    #[test]
    fn nonsense_timesteps_are_no_ops() {
        let mut board = changing(Mechanism::SplitFlap, 3, "000", "123");
        board.advance(0.1);
        let (clock, frame) = (board.now, board.render(DisplayStyle::Vfd));
        for dt in [0.0, -1.0, -0.0, f32::NAN, f32::NEG_INFINITY] {
            board.advance(dt);
            assert!(
                (board.now - clock).abs() < f64::EPSILON,
                "dt {dt} moved the clock"
            );
        }
        board.advance(f32::INFINITY);
        assert!(
            board.now.is_finite(),
            "an infinite step cannot poison the clock"
        );
        for to in [f64::NAN, f64::NEG_INFINITY, -5.0, 0.0, clock] {
            board.advance_to(to);
            assert!((board.now - clock).abs() < f64::EPSILON, "advance_to {to}");
        }
        assert_eq!(
            board.render(DisplayStyle::Vfd),
            frame,
            "and nothing rendered"
        );
    }

    /// `settle` (#422) parks the board on its content in one call — the hide
    /// edge — while keeping the geometry and timings a rebuilt `new` would have
    /// thrown away.
    #[test]
    fn settle_parks_the_board() {
        let mut board = FlipBoard::new(Mechanism::SplitFlap)
            .cells(4)
            .glyph_px(4)
            .scale(3)
            .duration_secs(1.5)
            .stagger_secs(0.2);
        board.set_text("0000");
        board.settle();
        board.set_text("1234");
        board.advance(0.3);
        assert!(!board.is_settled());

        board.settle();
        assert!(board.is_settled(), "parked, not mid-fall");
        assert_eq!(board.target(), "1234", "on the content it was heading to");
        assert!(
            board.cells.iter().all(|c| c.from == c.to),
            "with nothing left in flight"
        );
        assert_eq!((board.glyph_px, board.scale), (4, 3), "geometry survives");
        assert!(
            (board.duration - 1.5).abs() < f32::EPSILON
                && (board.stagger - 0.2).abs() < f32::EPSILON,
            "and so do the timings"
        );
        // Parking is idempotent, and a real change still animates afterwards.
        let parked = board.clone();
        board.settle();
        assert_eq!(board, parked);
        board.set_text("5678");
        assert!(!board.is_settled(), "the next real change still moves");
    }

    /// The buffer follows the metrics, fits the sidebar card (the #313 lesson),
    /// and every degenerate knob clamps rather than producing a broken buffer.
    #[test]
    fn dimensions_follow_the_metrics_and_fit_the_card() {
        let clock = FlipBoard::new(Mechanism::SplitFlap);
        assert_eq!(clock.cells.len(), 8, "HH:MM:SS by default");
        assert_eq!(clock.width(), 260);
        assert_eq!(clock.height(), 44);
        assert!(clock.width() <= 296, "{} fits the card", clock.width());
        assert!(
            (clock.duration - DEFAULT_FLIP_SECS).abs() < f32::EPSILON,
            "the split flap takes the mechanism's own default"
        );

        // The hinge lands on a pixel boundary only for an even glyph scale, so
        // odd scales round down rather than putting the seam mid-pixel.
        for (asked, want) in [(0, 2), (1, 2), (2, 2), (3, 2), (7, 6), (16, 16), (999, 16)] {
            let board = FlipBoard::new(Mechanism::Nixie).glyph_px(asked);
            assert_eq!(board.glyph_px, want, "glyph_px({asked})");
            assert_eq!(board.cell_h() % 2, 0, "the card splits evenly");
            assert_eq!(board.hinge(), board.cell_h() / 2);
        }
        assert_eq!(FlipBoard::new(Mechanism::Nixie).scale(0).scale, 1);

        // Degenerate boards still render a valid buffer.
        for cells in [0, 1] {
            let board = FlipBoard::new(Mechanism::SplitFlap).cells(cells);
            let frame = board.render(DisplayStyle::Lcd);
            assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
            assert!(frame.height() > 0);
            assert!(board.is_settled(), "an untouched board is at rest");
        }

        // Timings clamp, and non-finite values are ignored outright.
        let board = FlipBoard::new(Mechanism::SplitFlap)
            .duration_secs(0.0)
            .stagger_secs(-1.0);
        assert!(board.duration > 0.0 && board.stagger.abs() < f32::EPSILON);
        let kept = FlipBoard::new(Mechanism::SplitFlap)
            .duration_secs(f32::NAN)
            .stagger_secs(f32::INFINITY);
        assert!((kept.duration - DEFAULT_FLIP_SECS).abs() < f32::EPSILON);
        assert!(kept.stagger <= 2.0);
        assert_eq!(Mechanism::SplitFlap.name(), "split-flap");
        assert_eq!(Mechanism::Nixie.name(), "nixie");
        assert_eq!(board.mechanism(), Mechanism::SplitFlap);
    }

    /// The fold's resample: an *unsquashed* card maps one destination row onto
    /// one source row, so it copies the glyph bit for bit — which is what makes
    /// the resting frames exact — while a compressed one takes the true area
    /// average of the rows it now spans.
    #[test]
    fn the_resample_copies_at_rest_and_averages_when_squashed() {
        let board = FlipBoard::new(Mechanism::SplitFlap).cells(1).scale(1);
        let (g, pad) = (board.glyph_px, CARD_PAD_FX * board.glyph_px);
        // A column with a known bit pattern: '-' lights only font row 3.
        let dash = rows_of('-');
        let x = pad + 2 * g; // the glyph's centre column, which '-' lights
        for row in 0..font::GLYPH_H {
            let want = f32::from((dash[row] >> (font::GLYPH_W - 1 - 2)) & 1);
            let lo = fx(pad + row * g);
            let got = board.glyph_coverage(dash, x, lo, lo + 1.0);
            assert!((got - want).abs() < 1.0e-6, "row {row}: {got} vs {want}");
        }
        // A span covering one lit font row and one unlit one averages to a half.
        let lo = fx(pad + 2 * g);
        let half = board.glyph_coverage(dash, x, lo, lo + fx(2 * g));
        assert!(
            (half - 0.5).abs() < 1.0e-6,
            "area average, not a sample: {half}"
        );
        // Outside the glyph — the card's own padding — is unlit, not a smear.
        assert!(board.glyph_coverage(dash, 0, lo, lo + 1.0).abs() < f32::EPSILON);
        assert!(board.glyph_coverage(dash, x, 0.0, 0.0).abs() < f32::EPSILON);
        assert!(board.glyph_coverage(dash, x, 5.0, 1.0).abs() < f32::EPSILON);
    }
}
