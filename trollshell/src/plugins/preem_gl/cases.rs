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
//! Kept intentionally thin: the four supporting enums carry their **shape**
//! here (so [`Case`] can name them as field types) but not their behaviour —
//! `DisplayAt::line`, `TickerAt::line`, `BubbleAt::spec` and `NeedleAt`'s own
//! `impl` stay in the example, since inherent impls need only share a crate
//! with their type, not a file.

use super::kind::Kind;
use hytte_preem as kit;

/// One comparison: a kit widget, a skin, and the state to drive it into.
///
/// Moved out of `examples/preem_gl_diff.rs` for #1211 — see the module docs.
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
        }
    }

}

/// What a marquee case is showing, and where the message has scrolled to.
///
/// Five, and the choice is the issue's: the degenerate display, the
/// [hold rule](kit::MarqueeStrip::scrolls), and a scrolling message at **three**
/// phases. Three rather than one because the whole widget is the phase — the
/// shader has no offset uniform at all (#839 made a sub-dot position
/// inexpressible, so a step is a different set of lit columns) — and because
/// the three chosen below are the three shapes the wrap can take.
#[derive(Clone, Copy)]
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
        })
        .collect()
}
