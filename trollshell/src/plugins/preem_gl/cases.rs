//! The parity harness's **case list** — which [`Case`] `cases_for` builds,
//! per skin, for each [`Kind`] (#1211).
//!
//! # Why this is a module in the shell and not lines in the example
//!
//! `trollshell/examples/preem_gl_diff.rs` pulls this in with `#[path]`, the
//! same way it pulls in [`parity`](super::parity). It used to be written
//! there in full — `Case` and its four supporting enums, `cases_for`, and the
//! handful of constants `cases_for` reads — but a hermetic test has no way to
//! import from an example (examples depend on the lib, never the other way
//! around), and the #1209 review (LOW-1) found that the harness's case list
//! was one of the per-kind lists nothing enumerated: a sixth [`Kind`] could
//! land with zero cases here and every other gate would stay green.
//!
//! So the list moved here, `#[cfg(test)]`-mounted for [`parity`]'s reason
//! (`cargo test` does not run `#[test]`s inside an example), where
//! `plugins::tests`' `kind_enumeration` module can call [`cases_for`] and
//! check its **real** output against [`Kind::ALL`] — not a hand-kept count
//! that could drift from it unnoticed. What stayed in the example is
//! everything that turns a [`Case`] into an actual GL/CPU comparison
//! (`bubble_box`, `ticker_strip`, `gauge_state`, `drive`, `measure`, …): none
//! of that needs a display or a GL context to enumerate, only to *run*, so it
//! has no reason to be hermetic-test-reachable and every reason to stay next
//! to the harness code that drives it.
//!
//! Kept intentionally thin: the five supporting enums carry their **shape**
//! here (so [`Case`] can name them as field types) but not their behaviour —
//! `DisplayAt::line`, `TickerAt::line`, `BubbleAt::spec`, `MeterAt::reading`
//! and `NeedleAt`'s own `impl` stay in the example, since inherent impls need
//! only share a crate with their type, not a file.

use super::kind::Kind;
use hytte_preem as kit;

/// One comparison: a kit widget, a skin, and the state to drive it into.
///
/// Moved out of `examples/preem_gl_diff.rs` for #1211 — see the module docs.
///
/// The `LedStrip` variant (#1153) is the sixth, the `SevenSeg` one (#1154)
/// the seventh.
///
/// `#[allow(dead_code)]`: every field but `style` (read by [`Case::kind`]'s
/// sibling match in `plugins::tests`) is read only by the harness's own
/// `label`/`drive`/`geometry`/`measure` functions, which stayed in the
/// example (its `impl Case` block, not moved here) and so live in a
/// *different* crate compilation of this same source file — `cargo test`'s
/// dead-code pass over the lib/bin never sees them read, even though the
/// harness genuinely reads every one.
#[allow(dead_code)]
pub(crate) enum Case {
    /// A `Scope` after its debut batch plus `idle_steps` idle ones.
    Scope {
        style: kit::DisplayStyle,
        /// Extra idle steps after the debut batch, so the phosphor trail — the
        /// one thing the GL arm reimplements as a recurrence — is measured
        /// mid-fade rather than only at full intensity.
        idle_steps: u32,
    },
    /// A `Gauge` with its needle driven into one of three positions (#1143).
    Gauge {
        style: kit::DisplayStyle,
        needle: NeedleAt,
        /// The integer upscale the GL arm renders at. [`GAUGE_SCALE`] compares
        /// pixel against pixel; [`GAUGE_SUPERSAMPLE`] compares a box-averaged
        /// native frame against the kit's logical one.
        scale: u32,
    },
    /// A `DotMatrix` showing one line at one pitch (#1144).
    DotMatrix {
        style: kit::DisplayStyle,
        display: DisplayAt,
        /// How many times the area's size request exceeds the kit's buffer —
        /// see [`STRETCH`]. `1` for every case but the stretched one.
        stretch: u32,
    },
    /// A `Marquee` at one scroll phase (#1152).
    Marquee {
        style: kit::DisplayStyle,
        ticker: TickerAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
        /// The window this case renders at, in final buffer pixels —
        /// [`TICKER_WINDOW_PX`] for every case but one per skin, which runs at
        /// [`TICKER_ORIGIN_WINDOW_PX`] instead (#1209 review, MEDIUM-1).
        window_px: u32,
    },
    /// A `TextBox` in one configuration (#1152).
    TextBox {
        style: kit::DisplayStyle,
        bubble: BubbleAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
    },
    /// An `LedStrip` at one level/peak pair (#1153).
    LedStrip {
        style: kit::DisplayStyle,
        meter: MeterAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
        /// The segment count this case renders at — [`METER_LEDS`] for every
        /// case but one, which runs at `1` instead (#1293 item 2: every case
        /// ran at [`METER_LEDS`], so `strip_size`'s `(leds − 1) * GAP` term
        /// vanishing was mirror-only, never rendered on a driver).
        leds: usize,
    },
    /// A `SevenSeg` showing one readout (#1154).
    SevenSeg {
        style: kit::DisplayStyle,
        readout: ReadoutAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
    },
}

impl Case {
    /// Which [`Kind`] this case measures.
    ///
    /// An exhaustive `match` with no catch-all — the compile-time half of
    /// #1211's enumeration on the harness side: a sixth [`Case`] variant
    /// fails to compile here until it names which kind it belongs to, rather
    /// than silently reporting no coverage for a kind nothing ever pairs it
    /// with.
    pub(crate) fn kind(&self) -> Kind {
        match self {
            Self::Scope { .. } => Kind::Scope,
            Self::Gauge { .. } => Kind::Gauge,
            Self::DotMatrix { .. } => Kind::DotMatrix,
            Self::Marquee { .. } => Kind::Marquee,
            Self::TextBox { .. } => Kind::TextBox,
            Self::LedStrip { .. } => Kind::LedStrip,
            Self::SevenSeg { .. } => Kind::SevenSeg,
        }
    }
}

/// What a seven-segment case is reading out.
///
/// Six: the first five are the issue's own (#1154) adapted to what the kit
/// actually has — every segment off, every segment on, a mixed readout, every
/// distinct digit mask in one frame, and a pinned ink — plus
/// [`Empty`](Self::Empty), closing a gap the #1294 review found (#1293 item
/// 8): no case drove `u_data_len == 0`, the shape `SevenSegState::default()`
/// actually is.
///
/// **#1154 asks for "a mixed readout with a decimal point" and the kit has no
/// decimal point** — `hytte-preem`'s `seven_seg` understands digits, `:`, `-`
/// and space, and its two cell shapes are a 30 px digit and a 12 px colon.
/// [`Clock`](Self::Clock) carries the **colon** instead, which is the kit's own
/// second cell shape and the only thing that puts two different cell *widths*
/// (and so a non-uniform cell pitch, which is what the shader's binary search
/// over origins exists for) in one frame. Adding a decimal point is a change to
/// `hytte-preem`, not something this arm gets to invent.
///
/// Two digit counts at least, as the issue asks: one cell, two, five and ten.
#[derive(Clone, Copy)]
pub(crate) enum ReadoutAt {
    /// The empty string: `u_data_len == 0` and `seven_seg.frag`'s
    /// `strip255`'s `if (u_cells <= 0) { return 0; }` early return is the
    /// whole of the emission — the shape `data: None` binds a 1×1 zero
    /// texture for (#1293 item 8). Different from [`Blank`](Self::Blank): a
    /// blank readout is one ghosted cell with nothing lit; this one has no
    /// cell at all, and it is `vocab::SevenSegState::default()` — the first
    /// frame of every `SevenSeg` chip. Covered hermetically (`""` is in the
    /// transcription sweep) and, before this case, never on a driver.
    Empty,
    /// A single space: every ghost segment, nothing lit. The frame a clock
    /// spends none of its life in and a blanked readout spends all of it, and
    /// the one case where the lit emission is empty everywhere — so the halo is
    /// provably absent rather than merely dim.
    ///
    /// Two different vacuities here, not one (#1293 item 7). **Against an
    /// undrawn framebuffer**, only the OLED's blank case is vacuous — its
    /// field is pure black (`DisplayAt::Blank`'s finding, #1150 review,
    /// MEDIUM-3, in this widget's shape), while the CRT's non-black field
    /// (`[3, 7, 5]`) means a blank readout there *does* catch that failure.
    /// **Against a geometry drift**, the CRT's blank case is vacuous too:
    /// `palette_snapshot(Crt).ghost` is `None`, the same as the OLED's
    /// (`Vfd`/`Lcd` both have one), so a blank CRT readout draws nothing at
    /// all — measured `edge[n=0] lit[n=0]` against `vfd`/`lcd`'s
    /// `edge[n=600] lit[n=580]` — and there is nothing on screen for a drift
    /// to move (confirmed under a `snapped := false` probe, where
    /// `oled.blank` and `crt.blank` are the two blank cases that survive
    /// while `vfd.blank` and `lcd.blank` both red). The empty-strip coverage
    /// is carried by the two ghosted skins only. It is kept rather than
    /// special-cased for the reason [`MeterAt::Empty`]'s is.
    Blank,
    /// `88`: every segment of two cells lit. Every mitre in the font is
    /// adjacent to another mitre here, which is the arrangement where a taper
    /// drawn one pixel wide of the kit's closes a gap the kit leaves open.
    Eights,
    /// `12:34`: the clock face, and the only case with **two cell widths** —
    /// see the type docs on the decimal point the kit does not have.
    Clock,
    /// `9876543210`: ten cells and every distinct digit mask in one frame, so a
    /// bar indexed by the wrong bit shows somewhere.
    Digits,
    /// `-8` under a **pinned ink**, the `Ink::Fixed` arm of the kit's palette
    /// precedence — so the mapping's `palette_snapshot` and the kit's own
    /// `palette()` are compared through a pin rather than only through the
    /// skin's default. `-` is also the one non-digit, non-colon character the
    /// kit maps to a mask.
    Pinned,
}

/// What a marquee case is showing, and where the message has scrolled to.
///
/// Five, and the choice is the issue's: the degenerate display, the
/// [hold rule](kit::MarqueeStrip::scrolls), and a scrolling message at **three**
/// phases. Three rather than one because the whole widget is the phase — the
/// shader has no offset uniform at all (#839 made a sub-dot position
/// inexpressible, so a step is a different set of lit columns) — and because
/// the three chosen below are the three shapes the wrap can take.
///
/// `#[allow(dead_code)]`: `Scrolled`'s offset is read only by the harness's
/// `TickerAt::line` (stayed in the example, [`Case`]'s doc explains why).
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) enum TickerAt {
    /// The empty string: the bezel and the **fixed** ghost grid, with nothing
    /// lit. `u_data_len` is the grid's width and every texel is `0`, which is
    /// the one case that separates "no strip" from "a blank strip" — the kit
    /// paints the ghost lattice either way, so a shader that refused to draw on
    /// an all-zero grid would go dark here and nowhere else.
    Empty,
    /// A message that fits the grid, so the kit **holds** it static and ignores
    /// the offset entirely. Left-aligned on the grid, and the phase below can
    /// never move it.
    Held,
    /// A message wider than the grid, at scroll phase `offset` **in dots**.
    ///
    /// The three the case list uses are `0` (the message's head at the grid's
    /// first column), `7` (mid-message, so every visible column is a glyph
    /// column of a *different* character than at `0`) and one that lands inside
    /// the **loop seam** — the blank gap the kit appends after the message —
    /// where the grid is part message and part nothing, which is the one phase
    /// where `window_columns`' `bitmap.get` returns `None` for some columns and
    /// not others.
    Scrolled(usize),
}

/// What a textbox case is showing.
///
/// Four, the issue's list: the degenerate box, one line, a message wrapped to
/// the last row it is allowed, and the pinned-palette configuration the pet's
/// and caw's bubbles actually ship in.
#[derive(Clone, Copy)]
pub(crate) enum BubbleAt {
    /// The empty string, hugging: the `max(1)`-wide degenerate buffer, no
    /// cells at all (`u_cols == 0`, no strip), and one of the two cases at the
    /// kit's native `scale = 1` — so `u_upscale == 1` is rendered somewhere
    /// rather than only reasoned about.
    ///
    /// **On the OLED this case carries no information, and that is worth
    /// knowing rather than hiding** — `DisplayAt::Blank`'s finding (#1150
    /// review, MEDIUM-3) in this widget's shape. The OLED's field is
    /// `0, 0, 0` and the corner cut is to transparent *black*, so an empty box
    /// there is a frame of pure zeros: `verdict_for` excuses both blank guards
    /// by design (a flat reference is not evidence of an undrawn framebuffer)
    /// and every delta is 0 for any renderer that outputs black. Measured under
    /// an all-black blit it is the one text-box case of twenty that still says
    /// `PASS`. It is kept rather than special-cased because the other three
    /// skins' empty cases *do* detect — `textbox.lcd.empty` reports mean 152.308
    /// there — and a case list that varies by skin is a worse thing to reason
    /// about than one case that is vacuously green on one skin.
    Empty,
    /// One short line at `scale = 2`, hugging — the pet's own bubble.
    OneLine,
    /// A sentence wrapped to exactly `BUBBLE_LINES` rows with the kit's
    /// trailing `…`, in a **fixed-width** slot: three lines of different
    /// lengths, so the block's blank padding cells are what keep every line
    /// after the first at the right strip offset.
    Wrapped,
    /// A pinned ink, an explicit `.notdef` and an uncovered char, over a corner
    /// cut wide enough to reach the glyph block — #884/#885's configuration,
    /// and the one that renders the ink, the notdef box and the largest arc in
    /// one frame.
    Pinned,
}

/// What an LED-strip case's meter is reading.
///
/// Six, and the first five are the issue's own (#1153): the strip empty, half
/// lit and full — the three points `lit_count`'s round-to-nearest has to land
/// on — plus the two peak-dot arrangements the widget can be in. Those two are
/// the ones that matter: the dot is a *second* emission with a *second* halo,
/// composited on top of the level's, so a renderer that drew one pass instead
/// of two, or drew them in the wrong order, agrees with the kit on the first
/// three cases and disagrees on these. The sixth, [`PeakInside`](Self::PeakInside),
/// closes a gap the #1290 review found in the other two: no case put the dot
/// *inside* the lit run (#1293 item 2).
///
/// [`Empty`](Self::Empty) is deliberately a rested peak as well as a silent
/// level: it is the frame a meter spends most of its life in, and it is the one
/// where `peak_led`'s "no dot" answer reaches the shader as the `-1` sentinel.
///
/// Two different vacuities here, not one (#1293 item 3). **Against an undrawn
/// framebuffer**, only the OLED's empty case is vacuous — its field is pure
/// black, so a renderer that drew nothing at all still matches it, while the
/// CRT's non-black field (`[3, 7, 5]`) means a silent strip there *does* catch
/// that failure (measured: deleting the kind from `Kind::ALL` reds
/// `led_strip.crt.empty` at mean 7.000, and leaves only `led_strip.oled.empty`
/// green). **Against a segment-geometry drift**, the CRT's empty case is
/// vacuous too: `palette_snapshot(Crt).ghost` is `None`, the same as the
/// OLED's (`Vfd`/`Lcd` both have one), so a silent CRT strip has no segment on
/// screen to widen either. It is kept rather than special-cased because the
/// two ghosted skins' empty cases *do* detect a geometry drift — the VFD and
/// LCD draw their whole ghost row there — and a case list that varies by skin
/// is a worse thing to reason about than two cases that are each vacuously
/// green against one kind of drift.
#[derive(Clone, Copy)]
pub(crate) enum MeterAt {
    /// Silence: no lit segment, no peak dot. The ghost row and the field, and
    /// the `-1` peak sentinel.
    Empty,
    /// Half scale, no peak dot — the ordinary reading.
    Half,
    /// Full scale, no peak dot: every segment lit, so the halo's window is
    /// saturated almost everywhere and the buffer's own edge clipping (the
    /// unrenormalised divisor) is what is left to get wrong.
    Full,
    /// A quiet level with the dot floating well above it — the two emissions
    /// visibly apart, which is the arrangement a single-pass renderer cannot
    /// produce.
    PeakAbove,
    /// The dot sitting on the **first unlit** segment, adjacent to the level
    /// rather than overlapping it (#1293 item 2 — this doc used to claim the
    /// two emissions overlap; at this case's own numbers, `lit_count`
    /// rounding `0.6 × 24` to 14 and `peak_led` ceiling `0.6 × 24` onto index
    /// 14, they never do). [`MeterAt::PeakInside`] is the arrangement that
    /// actually overlaps.
    PeakAt,
    /// The dot sitting **inside** the lit run, where `mix_kit(under = ink,
    /// cap, dot)` genuinely composites the cap over ink the level pass has
    /// already laid down — the overlap [`MeterAt::PeakAt`] was wrongly
    /// documented as (#1293 item 2). No case exercised this arrangement on a
    /// driver before.
    PeakInside,
}

/// The segment count every LED-strip case runs at — the kit's own
/// [`kit::DEFAULT_LEDS`], which is what every meter on the glass uses and what
/// [`kit::DEFAULT_WIDTH`] is stated for.
///
/// `#[allow(dead_code)]` for [`Case`]'s reason: it is read by the harness's own
/// `led_strip_config`/`meter_strip`, which stayed in the example and so live in
/// a different crate compilation of this same source file.
#[allow(dead_code)]
pub(crate) const METER_LEDS: usize = kit::DEFAULT_LEDS;

/// What a dot-matrix case puts on the display.
///
/// Five, chosen to cover what the shader has to get right: the degenerate
/// buffer, the ordinary readout, the font's fallback path, and each end of the
/// pitch clamp — the one where the CRT comb has to be **re-phased** or it stops
/// being a raster (#1091), and the one where the vignette's band is a different
/// number of pixels than any other case makes it. Each is the same lattice
/// arithmetic at a different corner of it.
#[derive(Clone, Copy)]
pub(crate) enum DisplayAt {
    /// The empty string: bezel only, no strip, no lit pixel — `2*pad` × `9*dot`.
    /// The one case where `u_data_len` is `0` and the shader must draw the
    /// field rather than sample an unbound texture.
    ///
    /// **On the OLED this case carries no information, and that is worth
    /// knowing rather than hiding** (#1150 review, MEDIUM-3). The kit's frame
    /// there is 8×72 of pure black (`style.rs`'s OLED `bg` is `0, 0, 0` with no
    /// ghost), so both sides are flat *and* black: `verdict_for` excuses both
    /// blank guards by design — a flat reference is not evidence of an undrawn
    /// framebuffer — and every delta is 0 for any renderer that outputs black.
    /// Measured: under an all-black blit it is the one dot-matrix case that
    /// still says `PASS`. It is kept rather than special-cased because the
    /// other three skins' blank cases *do* detect, and a case list that varies
    /// by skin is a worse thing to reason about than one case that is
    /// vacuously green on one skin.
    Blank,
    /// An ordinary readout at the default pitch: the ghost lattice, lit glyphs,
    /// the skin's halo, and the comb where the kit's own golden digests have it.
    Readout,
    /// Accented glyphs, a space and an uncovered char — so the hollow `NOTDEF`
    /// box reaches the strip encoder and the shader end to end.
    Notdef,
    /// The same readout at `MIN_DOT_PX`, where every pixel of a dot sits on the
    /// falloff plateau (a solid block, no rim) **and** the CRT comb is re-phased
    /// onto a 2-row grid. A fixed 4-row comb here is interference, not a raster.
    Dense,
    /// The same readout at `MAX_DOT_PX`, the other end of the clamp — and the
    /// only case whose short side is **not** a multiple of both 8 and 9
    /// (#1150 review, MEDIUM-2). It also gets the upper clamp rendered at all,
    /// which nothing did before.
    Coarse,
}

/// Where a gauge case's needle is when the frame is taken.
///
/// Three positions, chosen to cover what the shader has to get right: the
/// motion-blur fan **off** and **on** (it is the needle's own geometry
/// max-combined, so at rest it must vanish exactly rather than fatten the
/// blade), the lit value arc empty and full, and the overtravel stop.
#[derive(Clone, Copy)]
pub(crate) enum NeedleAt {
    /// Settled at rest, low on the scale: no fan, a short value arc.
    Rest,
    /// Mid-sweep toward full scale: the fan is spread, the arc is partly lit.
    Sweeping,
    /// Slammed to full scale and overshooting into the mechanical stop.
    Pegged,
}

/// How much bigger than the kit's buffer the **stretched** cases run their
/// `GlSurface` at — what a layout wider than a chip's grid, or a
/// `scale_factor >= 2` screen, does to it on real glass.
pub(crate) const STRETCH: u32 = 2;

/// The ticker's window, in final buffer pixels, for every marquee case but the
/// one per skin that runs at [`TICKER_ORIGIN_WINDOW_PX`] instead.
pub(crate) const TICKER_WINDOW_PX: u32 = 96;

/// The ticker window one case per skin runs at instead of
/// [`TICKER_WINDOW_PX`], where the centred grid's `origin_x` no longer
/// coincides with the bezel — closing the #1209 review's MEDIUM-1.
pub(crate) const TICKER_ORIGIN_WINDOW_PX: u32 = 98;

/// The scroll phase that lands inside the loop seam — see
/// [`TickerAt::Scrolled`].
pub(crate) const TICKER_SEAM_PHASE: usize = 215;

/// The upscale the **1:1** gauge cases run at — see [`Case::Gauge`].
pub(crate) const GAUGE_SCALE: u32 = 1;

/// The upscale the **supersampled** gauge cases run at — `GaugeConfig`'s own
/// default, which is what every dial on the glass actually uses (#1148 review,
/// HIGH-2).
pub(crate) const GAUGE_SUPERSAMPLE: u32 = 2;

/// Every case the harness runs, one skin at a time.
///
/// The single source both `examples/preem_gl_diff.rs`'s harness and
/// `plugins::tests`' `kind_enumeration` module read (#1211): the harness
/// builds and drives exactly this list, and the hermetic test calls this same
/// function to check every [`Kind::ALL`] member has at least one case per
/// skin here, rather than trusting a count typed twice.
///
/// The `too_many_lines` allow is the case list's, not this function's: it is
/// one flat `let … = …;` per kind group with no nesting between them, and it
/// crossed the ceiling when #1153 added the sixth. Splitting it would put half
/// the case list somewhere else, which is worse to read and worse to review
/// than a long function — the same trade `preem_render::advance` states.
#[allow(clippy::too_many_lines)]
pub(crate) fn cases_for(skins: &[kit::DisplayStyle]) -> Vec<Case> {
    skins
        .iter()
        .flat_map(|style| {
            let scopes = [0_u32, 1, 5]
                .into_iter()
                .map(move |idle_steps| Case::Scope {
                    style: *style,
                    idle_steps,
                });
            let gauges = [NeedleAt::Rest, NeedleAt::Sweeping, NeedleAt::Pegged]
                .into_iter()
                .map(move |needle| Case::Gauge {
                    style: *style,
                    needle,
                    scale: GAUGE_SCALE,
                });
            // One supersampled case per skin, at the needle position that puts
            // the most anti-aliased edge on the face: the blade is at an
            // arbitrary angle, the fan is spread across four blades, and the
            // value arc has both of its ends on screen. See
            // [`GAUGE_SUPERSAMPLE`].
            let shipping = std::iter::once(Case::Gauge {
                style: *style,
                needle: NeedleAt::Sweeping,
                scale: GAUGE_SUPERSAMPLE,
            });
            let displays = [
                DisplayAt::Blank,
                DisplayAt::Readout,
                DisplayAt::Notdef,
                DisplayAt::Dense,
                DisplayAt::Coarse,
            ]
            .into_iter()
            .map(move |display| Case::DotMatrix {
                style: *style,
                display,
                stretch: 1,
            });
            // …and the same readout given more room than its natural size,
            // which is where the improvement actually lives (#1144).
            let stretched = std::iter::once(Case::DotMatrix {
                style: *style,
                display: DisplayAt::Readout,
                stretch: STRETCH,
            });
            // The ticker (#1152): the degenerate display, the hold rule, and a
            // scrolling message at three phases — the head, mid-message, and
            // one straddling the loop seam. See [`TickerAt`].
            let tickers = [
                TickerAt::Empty,
                TickerAt::Held,
                TickerAt::Scrolled(0),
                TickerAt::Scrolled(7),
                TickerAt::Scrolled(TICKER_SEAM_PHASE),
            ]
            .into_iter()
            .map(move |ticker| Case::Marquee {
                style: *style,
                ticker,
                stretch: 1,
                window_px: TICKER_WINDOW_PX,
            });
            // …and the same mid-message phase given more room than its natural
            // size, which is where this arm's improvement lives — the dot
            // lattice at the screen's resolution rather than a magnified table.
            let stretched_ticker = std::iter::once(Case::Marquee {
                style: *style,
                ticker: TickerAt::Scrolled(7),
                stretch: STRETCH,
                window_px: TICKER_WINDOW_PX,
            });
            // …and the same mid-message phase again, but through a window
            // where the centred grid's origin does not coincide with the
            // bezel (#1209 review, MEDIUM-1) — still 1:1 and exact-pinned,
            // since the point is the uniform's *value*, not a new sampling
            // standard. See [`TICKER_ORIGIN_WINDOW_PX`].
            let ticker_origin = std::iter::once(Case::Marquee {
                style: *style,
                ticker: TickerAt::Scrolled(7),
                stretch: 1,
                window_px: TICKER_ORIGIN_WINDOW_PX,
            });
            // The bubble (#1152): the degenerate box, one line, a message
            // wrapped to the last row, and the pinned-palette configuration.
            let bubbles = [
                BubbleAt::Empty,
                BubbleAt::OneLine,
                BubbleAt::Wrapped,
                BubbleAt::Pinned,
            ]
            .into_iter()
            .map(move |bubble| Case::TextBox {
                style: *style,
                bubble,
                stretch: 1,
            });
            // …and the widest corner given more room than its natural size,
            // which is where *this* arm's improvement lives: the cut is an arc
            // at the screen's resolution rather than a replicated logical-pixel
            // mask, and `Pinned`'s radius-5 corner is the one that shows it.
            let stretched_bubble = std::iter::once(Case::TextBox {
                style: *style,
                bubble: BubbleAt::Pinned,
                stretch: STRETCH,
            });
            // The meter (#1153): the strip empty, half, full, and the three
            // peak-dot arrangements — above the lit run, on its first unlit
            // segment, and (#1293 item 2) inside it. See [`MeterAt`].
            let meters = [
                MeterAt::Empty,
                MeterAt::Half,
                MeterAt::Full,
                MeterAt::PeakAbove,
                MeterAt::PeakAt,
                MeterAt::PeakInside,
            ]
            .into_iter()
            .map(move |meter| Case::LedStrip {
                style: *style,
                meter,
                stretch: 1,
                leds: METER_LEDS,
            });
            // …and the floating-dot state given more room than its natural
            // size, which is where *this* arm's improvement lives: the segment
            // edges and both halos resolved at the screen's resolution rather
            // than replicated out of the kit's grid. `PeakAbove` because it is
            // the one state with two separate halos on the glass at once.
            let stretched_meter = std::iter::once(Case::LedStrip {
                style: *style,
                meter: MeterAt::PeakAbove,
                stretch: STRETCH,
                leds: METER_LEDS,
            });
            // …and a full strip at a **second segment count** (#1293 item 2):
            // every other meter case runs at `METER_LEDS`, so `leds = 1` —
            // where `strip_size`'s `(leds − 1) * GAP` term vanishes — was
            // mirror-only, never rendered through a real GL context.
            let single_led = std::iter::once(Case::LedStrip {
                style: *style,
                meter: MeterAt::Full,
                stretch: 1,
                leds: 1,
            });
            // The readout (#1154): empty, blank, all-on, the clock face,
            // every digit, and a pinned ink. See [`ReadoutAt`].
            let readouts = [
                ReadoutAt::Empty,
                ReadoutAt::Blank,
                ReadoutAt::Eights,
                ReadoutAt::Clock,
                ReadoutAt::Digits,
                ReadoutAt::Pinned,
            ]
            .into_iter()
            .map(move |readout| Case::SevenSeg {
                style: *style,
                readout,
                stretch: 1,
            });
            // …and the clock face given more room than its natural size, which
            // is where *this* arm's improvement lives: the segments' 45° mitres
            // and the halo around them resolved at the screen's resolution
            // rather than replicated out of the kit's six-row stair.
            // `Clock` because it is the case with the most mitres per pixel of
            // buffer and both cell shapes on screen at once.
            let stretched_readout = std::iter::once(Case::SevenSeg {
                style: *style,
                readout: ReadoutAt::Clock,
                stretch: STRETCH,
            });
            scopes
                .chain(gauges)
                .chain(shipping)
                .chain(displays)
                .chain(stretched)
                .chain(tickers)
                .chain(stretched_ticker)
                .chain(ticker_origin)
                .chain(bubbles)
                .chain(stretched_bubble)
                .chain(meters)
                .chain(stretched_meter)
                .chain(single_led)
                .chain(readouts)
                .chain(stretched_readout)
        })
        .collect()
}
