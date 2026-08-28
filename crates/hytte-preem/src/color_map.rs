//! The kit's **colour axis** (issue #857) — a map from a cell's position and
//! level to that cell's ink.
//!
//! This is deliberately *not* part of [`DisplayStyle`](super::DisplayStyle),
//! and the separation is the whole design. A `DisplayStyle` is the **skin**:
//! the physical panel a surface is drawn on — its field colour, whether unlit
//! elements ghost through, how (and how far) lit elements bloom, and whether a
//! scanline/vignette pass is multiplied in. It contributes exactly *one* ink,
//! and that ink follows the session accent (#376).
//!
//! A [`ColorMap`] answers a different question: given that a surface has many
//! independently-lit cells, **what colour is each one?** A heat ramp says "the
//! hot ones are red"; a rainbow says "every cell is its own hue". Neither
//! statement has anything to say about scanlines or phosphor bloom.
//!
//! Keeping them orthogonal is what lets them **compose**: a heat-mapped LED
//! panel still gets the CRT pass's comb and vignette, VFD's halo, or LCD's
//! ghost cells, because the map only ever replaces the *ink argument* that
//! `Emission::composite` was already taking — never the palette, never the
//! post-passes. Folding the colours into `DisplayStyle` instead would have made
//! them mutually exclusive (you would pick `Heat` **or** `Crt`), which is
//! precisely the trade the issue asked us not to make.
//!
//! # The default is the old behaviour, exactly
//!
//! [`ColorMap::Style`] is [`Default`], and it returns the palette ink it is
//! handed, unchanged, for every position and every level. A surface rendering
//! with it is byte-for-byte the surface that existed before this module. Three
//! tests pin that from three directions:
//!
//! - `style_is_the_identity_on_the_palette_ink` (below) — the map itself is
//!   the identity on `ink`, for every position and level including the
//!   non-finite ones.
//! - `led_matrix`'s `style_map_is_the_single_ink_path` — end to end through a
//!   widget, `Style` and an explicit `Rgb(palette.ink)` render byte-identically
//!   across all four skins, which exercises the old and the generalised
//!   composite against each other.
//! - `tests/single_ink_golden.rs` — the exact bytes of every *other* kit
//!   widget, captured from the tree before the colour axis existed.
//!
//! # Position, not geometry
//!
//! [`ColorMap::ink`] takes a **normalized position** `pos` in `0.0..=1.0`
//! rather than a `(col, row)` pair, so the map knows nothing about the grid
//! that called it and works unchanged on a 1-D strip, a 2-D panel, or anything
//! else the kit grows. The caller decides what the sweep runs along;
//! [`LedMatrix`](super::LedMatrix) sweeps it along the **cell index**
//! (row-major), so on a multi-row panel a positional map's bands wrap from one
//! row into the next and read as slanted rather than as flat horizontal
//! stripes. That is a deliberate, documented consequence of having one sweep
//! rule instead of a shape-dependent branch.

use super::frame::Rgba;
use super::style::mix;

/// A map from a cell's position and level to the ink it lights in — the kit's
/// colour axis, orthogonal to the [`DisplayStyle`](super::DisplayStyle) skin
/// (the `color_map` module docs carry the full rationale).
///
/// `Style` is the default and reproduces the pre-#857 single-ink behaviour
/// exactly. The rest split into two families:
///
/// - **positional** ([`Rainbow`](Self::Rainbow),
///   [`TransPride`](Self::TransPride)) — the colour is a function of *where*
///   the cell is, so each cell keeps a stable identity as its level moves.
/// - **level-driven** ([`Heat`](Self::Heat)) — the colour is a function of
///   *how lit* the cell is, so the panel reads as a thermal image.
///
/// [`Rgb`](Self::Rgb) is neither: one fixed colour everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorMap {
    /// Follow the skin: every cell lights in the palette's single ink, which
    /// is itself accent-tinted (#376). The default, and the exact behaviour
    /// every kit surface had before the colour axis existed.
    #[default]
    Style,
    /// One fixed opaque colour for every cell, regardless of position and
    /// level — Annika's `(r, g, b)` option.
    Rgb(u8, u8, u8),
    /// A full hue sweep across the panel by position: cell 0 is red, the last
    /// cell is back around at red, and every cell in between takes its own
    /// hue at full saturation and value.
    Rainbow,
    /// The trans pride flag's five stripes — light blue, pink, white, pink,
    /// light blue — banded across the panel by position.
    TransPride,
    /// A thermal ramp driven by the cell's **level**: cool blue at rest,
    /// through cyan, green and amber, to red at full. The load-panel map.
    Heat,
}

impl ColorMap {
    /// Every map that takes no parameters, for demo rotation and test sweeps.
    /// [`Rgb`](Self::Rgb) is excluded because it has no canonical value.
    pub const ALL: [Self; 4] = [Self::Style, Self::Rainbow, Self::TransPride, Self::Heat];

    /// The map as a lowercase word — handy for labels, env-var parsing and CSS
    /// class suffixes, mirroring [`DisplayStyle::name`](super::DisplayStyle::name).
    /// [`Rgb`](Self::Rgb) reports `"rgb"`, without its components.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Style => "style",
            Self::Rgb(..) => "rgb",
            Self::Rainbow => "rainbow",
            Self::TransPride => "transpride",
            Self::Heat => "heat",
        }
    }

    /// The ink a cell lights in.
    ///
    /// `pos` is the cell's normalized position along the caller's sweep
    /// (`0.0..=1.0`), `level` is its `0.0..=1.0` brightness, and `ink` is the
    /// palette ink the skin would have used on its own — which
    /// [`Style`](Self::Style) returns unchanged, making it the identity on the
    /// old path.
    ///
    /// Both floats are sanitized on the way in: non-finite values (`NaN`, the
    /// infinities) read as `0.0`, and finite ones are clamped, so no input can
    /// produce a non-deterministic or out-of-gamut colour. The returned colour
    /// is always opaque — kit surfaces are *screens*, and the composite mixes
    /// toward this as a fully-opaque endpoint.
    pub(crate) fn ink(self, pos: f32, level: f32, ink: Rgba) -> Rgba {
        let pos = sanitize(pos);
        let level = sanitize(level);
        match self {
            Self::Style => ink,
            Self::Rgb(r, g, b) => [r, g, b, 0xff],
            Self::Rainbow => hue(pos),
            Self::TransPride => TRANS_STRIPES[band(pos, TRANS_STRIPES.len())],
            Self::Heat => ramp(&HEAT_STOPS, level),
        }
    }
}

/// Clamp a caller-supplied `0.0..=1.0` parameter, mapping every non-finite
/// value (`NaN`, `±inf`) to `0.0`.
///
/// `f32::clamp` propagates `NaN` rather than pinning it, and every consumer
/// below then casts the result to an integer — where the saturating cast would
/// quietly turn `NaN` into `0` anyway, but only after the arithmetic had
/// already gone through a `NaN`. Doing it once here keeps the maps' `NaN`
/// behaviour *stated* rather than inherited from a cast, matching how
/// `led_strip::lit_count` documents the same case.
fn sanitize(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Which of `n` equal bands a `0.0..=1.0` position falls in, clamped to
/// `0..n-1` so `pos == 1.0` lands in the last band rather than one past it.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn band(pos: f32, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    // `pos` is already sanitized into `0.0..=1.0` and `n` is a tiny stripe
    // count, so the product is in `0.0..=n` and the truncating cast is exact;
    // `.min(n - 1)` catches only the `pos == 1.0` endpoint.
    ((pos * n as f32) as usize).min(n - 1)
}

/// The trans pride flag, top to bottom: light blue, pink, white, pink, light
/// blue.
const TRANS_STRIPES: [Rgba; 5] = [
    [0x5b, 0xce, 0xfa, 0xff],
    [0xf5, 0xa9, 0xb8, 0xff],
    [0xff, 0xff, 0xff, 0xff],
    [0xf5, 0xa9, 0xb8, 0xff],
    [0x5b, 0xce, 0xfa, 0xff],
];

/// The [`ColorMap::Heat`] ramp: cool blue at rest → cyan → green → amber →
/// red at full. Five stops rather than a two-endpoint blue→red lerp, because a
/// straight lerp between those two passes through a muddy purple exactly where
/// a load panel spends most of its time (a half-busy core), while the classic
/// thermal ramp keeps every intermediate reading a distinct, saturated hue.
const HEAT_STOPS: [Rgba; 5] = [
    [0x1e, 0x6f, 0xff, 0xff],
    [0x1e, 0xd8, 0xd8, 0xff],
    [0x4c, 0xe0, 0x4c, 0xff],
    [0xf2, 0xc8, 0x2c, 0xff],
    [0xff, 0x33, 0x22, 0xff],
];

/// Sample a piecewise-linear colour ramp at `t` (`0.0..=1.0`), interpolating
/// between the two bracketing stops with the kit's integer [`mix`] so the
/// result stays bit-deterministic.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn ramp(stops: &[Rgba], t: f32) -> Rgba {
    match stops {
        [] => [0, 0, 0, 0xff],
        [only] => *only,
        _ => {
            let spans = stops.len() - 1;
            // `t` is sanitized into `0.0..=1.0` and `spans` is a tiny stop
            // count, so `scaled` is in `0.0..=spans`: the truncating cast is
            // exact and `.min(spans - 1)` only catches the `t == 1.0`
            // endpoint, keeping `lo + 1` in bounds.
            let scaled = t * spans as f32;
            let lo = (scaled as usize).min(spans - 1);
            let frac = scaled - lo as f32;
            // `frac` is in `0.0..=1.0`, so the product is in `0.0..=255.0`.
            mix(stops[lo], stops[lo + 1], (frac * 255.0).round() as u16)
        }
    }
}

/// A fully saturated, fully bright hue at position `pos` (`0.0..=1.0` around
/// the whole colour wheel) — the [`ColorMap::Rainbow`] sweep.
///
/// Hand-rolled rather than via a general HSV conversion: with `s == v == 1`
/// the conversion collapses to "one channel at 255, one at 0, one ramping",
/// which is six branches of integer-friendly arithmetic and no divisions.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn hue(pos: f32) -> Rgba {
    // Six 60° sectors. `pos` is sanitized into `0.0..=1.0`, so `h` is in
    // `0.0..=6.0` and the truncating cast is exact; `.min(5)` catches only the
    // `pos == 1.0` endpoint, which wraps back onto the red sector.
    let h = pos * 6.0;
    let sector = (h as usize).min(5);
    let frac = h - sector as f32;
    // `frac` is in `0.0..=1.0`, so `up` covers the full `0..=255` ramp.
    let up = (frac * 255.0).round() as u8;
    let down = 0xff - up;
    match sector {
        0 => [0xff, up, 0x00, 0xff],
        1 => [down, 0xff, 0x00, 0xff],
        2 => [0x00, 0xff, up, 0xff],
        3 => [0x00, down, 0xff, 0xff],
        4 => [up, 0x00, 0xff, 0xff],
        _ => [0xff, 0x00, down, 0xff],
    }
}

#[cfg(test)]
mod tests {
    use super::{ColorMap, HEAT_STOPS, TRANS_STRIPES, band, hue, ramp, sanitize};

    /// A stand-in palette ink, distinct from every colour any map produces, so
    /// "the map returned the ink" can never be confused with "the map happened
    /// to pick that colour".
    const INK: [u8; 4] = [0x8d, 0xf5, 0xff, 0xff];

    /// A sweep of `(pos, level)` pairs covering both endpoints, the interior,
    /// and every hostile float.
    const PROBES: [(f32, f32); 9] = [
        (0.0, 0.0),
        (0.0, 1.0),
        (0.5, 0.5),
        (1.0, 0.0),
        (1.0, 1.0),
        (0.25, 0.75),
        (f32::NAN, f32::NAN),
        (f32::INFINITY, f32::NEG_INFINITY),
        (-3.0, 4.0),
    ];

    /// **The #857 regression guard, half one.** `Style` is the identity on the
    /// palette ink for every position and every level — including the hostile
    /// ones — so a surface rendering with the default map composites toward
    /// exactly the ink the skin would have used on its own.
    ///
    /// Falsified by any `Self::Style => …` arm that does not return `ink`
    /// verbatim.
    #[test]
    fn style_is_the_identity_on_the_palette_ink() {
        for (pos, level) in PROBES {
            assert_eq!(
                ColorMap::Style.ink(pos, level, INK),
                INK,
                "Style must hand back the palette ink at pos={pos} level={level}"
            );
        }
        // …and for a *different* ink, so the arm can't be passing by having
        // hard-coded this one.
        let other = [0x12, 0x34, 0x56, 0xff];
        assert_eq!(ColorMap::Style.ink(0.5, 0.5, other), other);
    }

    /// Every map returns an opaque colour whatever it is fed — the composite
    /// mixes toward this as a fully-opaque endpoint, and a kit surface promises
    /// an opaque buffer.
    #[test]
    fn every_map_is_opaque_on_every_input() {
        for map in ColorMap::ALL.into_iter().chain([ColorMap::Rgb(1, 2, 3)]) {
            for (pos, level) in PROBES {
                assert_eq!(
                    map.ink(pos, level, INK)[3],
                    0xff,
                    "{} at pos={pos} level={level}",
                    map.name()
                );
            }
        }
    }

    /// Non-finite inputs are pinned to `0.0` rather than propagated, so a
    /// `NaN` level yields the ramp's cold end and a `NaN` position yields the
    /// sweep's first band — deterministic, never a garbage colour.
    ///
    /// Falsified by deleting the `is_finite` guard in `sanitize`: `NaN` then
    /// flows into the casts and the assertions on the ramp's identity break.
    #[test]
    fn non_finite_inputs_read_as_zero() {
        assert!(sanitize(f32::NAN) <= 0.0);
        assert!(sanitize(f32::INFINITY) <= 0.0);
        assert!(sanitize(f32::NEG_INFINITY) <= 0.0);
        assert!(
            (sanitize(2.0) - 1.0).abs() < 1e-6,
            "an over-unit value clamps"
        );
        assert!(sanitize(-2.0) <= 0.0, "a negative value clamps");

        assert_eq!(
            ColorMap::Heat.ink(0.0, f32::NAN, INK),
            HEAT_STOPS[0],
            "a NaN level reads as the cold end of the heat ramp"
        );
        assert_eq!(
            ColorMap::TransPride.ink(f32::NAN, 0.0, INK),
            TRANS_STRIPES[0],
            "a NaN position reads as the first stripe"
        );
    }

    /// `Rgb` is the fixed colour, opaque, at every position and level.
    #[test]
    fn rgb_is_a_fixed_opaque_colour() {
        let map = ColorMap::Rgb(0x9b, 0x59, 0xb6);
        for (pos, level) in PROBES {
            assert_eq!(map.ink(pos, level, INK), [0x9b, 0x59, 0xb6, 0xff]);
        }
    }

    /// The heat ramp is level-driven and *only* level-driven: it hits both
    /// stops exactly at the endpoints, ignores position entirely, and never
    /// returns the same colour for two levels a quarter of the range apart.
    ///
    /// Falsified by passing `pos` instead of `level` to `ramp` (the endpoint
    /// assertions flip), or by collapsing `HEAT_STOPS` to a single stop (the
    /// distinctness assertion goes red).
    #[test]
    fn heat_follows_the_level_and_ignores_position() {
        assert_eq!(ColorMap::Heat.ink(0.0, 0.0, INK), HEAT_STOPS[0], "rest");
        assert_eq!(ColorMap::Heat.ink(0.0, 1.0, INK), HEAT_STOPS[4], "pinned");
        // The same level at opposite ends of the panel is the same colour.
        assert_eq!(
            ColorMap::Heat.ink(0.0, 0.42, INK),
            ColorMap::Heat.ink(1.0, 0.42, INK),
            "heat is a function of level alone"
        );
        // Quarter-range steps are visibly distinct readings.
        let steps: Vec<_> = (0..=4)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let level = i as f32 / 4.0;
                ColorMap::Heat.ink(0.5, level, INK)
            })
            .collect();
        for (i, a) in steps.iter().enumerate() {
            for b in &steps[i + 1..] {
                assert_ne!(a, b, "two quarter-range levels share a colour");
            }
        }
        // The ramp is continuous: adjacent 1 % steps never jump a whole stop.
        let mut prev = ColorMap::Heat.ink(0.5, 0.0, INK);
        for step in 1..=100 {
            #[allow(clippy::cast_precision_loss)]
            let level = step as f32 / 100.0;
            let next = ColorMap::Heat.ink(0.5, level, INK);
            let jump = (0..3)
                .map(|k| i32::from(next[k]) - i32::from(prev[k]))
                .map(i32::abs)
                .max()
                .unwrap_or(0);
            assert!(jump <= 24, "the ramp jumped {jump} at level {level}");
            prev = next;
        }
    }

    /// The rainbow is position-driven and *only* position-driven: it sweeps
    /// hues across the panel, ignores level, and wraps back to red.
    ///
    /// Falsified by passing `level` instead of `pos` to `hue` (the sweep
    /// assertions flip).
    #[test]
    fn rainbow_follows_position_and_ignores_level() {
        assert_eq!(
            ColorMap::Rainbow.ink(0.0, 0.0, INK),
            [0xff, 0x00, 0x00, 0xff]
        );
        assert_eq!(
            ColorMap::Rainbow.ink(1.0, 0.0, INK),
            [0xff, 0x00, 0x00, 0xff],
            "the sweep wraps back to red at the far end"
        );
        assert_eq!(
            ColorMap::Rainbow.ink(0.3, 0.0, INK),
            ColorMap::Rainbow.ink(0.3, 1.0, INK),
            "the rainbow is a function of position alone"
        );
        // Sixths land on the primaries/secondaries of the wheel.
        let sixth = |i: u8| ColorMap::Rainbow.ink(f32::from(i) / 6.0, 0.5, INK);
        assert_eq!(sixth(1), [0xff, 0xff, 0x00, 0xff], "60° is yellow");
        assert_eq!(sixth(2), [0x00, 0xff, 0x00, 0xff], "120° is green");
        assert_eq!(sixth(3), [0x00, 0xff, 0xff, 0xff], "180° is cyan");
        assert_eq!(sixth(4), [0x00, 0x00, 0xff, 0xff], "240° is blue");
        assert_eq!(sixth(5), [0xff, 0x00, 0xff, 0xff], "300° is magenta");
        // Every hue is fully saturated and fully bright: one channel is
        // saturated and one is dark, at every point of the sweep.
        for step in 0..=120 {
            #[allow(clippy::cast_precision_loss)]
            let pos = step as f32 / 120.0;
            let c = hue(pos);
            assert!(c[..3].contains(&0xff), "no channel is saturated at {pos}");
            assert!(c[..3].contains(&0x00), "no channel is dark at {pos}");
        }
    }

    /// The flag bands the panel into its five stripes, in order, at equal
    /// widths — and both endpoints land inside the flag rather than off it.
    ///
    /// Falsified by dropping `band`'s `.min(n - 1)`: `pos == 1.0` then indexes
    /// one past the last stripe and the test panics on the bounds check.
    #[test]
    fn transpride_bands_the_panel_into_five_stripes() {
        let at = |pos: f32| ColorMap::TransPride.ink(pos, 0.5, INK);
        assert_eq!(at(0.0), TRANS_STRIPES[0]);
        assert_eq!(at(0.1), TRANS_STRIPES[0]);
        assert_eq!(at(0.3), TRANS_STRIPES[1]);
        assert_eq!(at(0.5), TRANS_STRIPES[2], "white in the middle");
        assert_eq!(at(0.7), TRANS_STRIPES[3]);
        assert_eq!(at(0.9), TRANS_STRIPES[4]);
        assert_eq!(at(1.0), TRANS_STRIPES[4], "the far end is still the flag");
        // The flag is symmetric about its middle stripe.
        for step in 0..=50 {
            #[allow(clippy::cast_precision_loss)]
            let pos = step as f32 / 100.0;
            assert_eq!(at(pos), at(1.0 - pos - 1e-4), "asymmetric at {pos}");
        }
    }

    /// `band` divides `0.0..=1.0` into equal parts and never runs off the end,
    /// including the degenerate zero-band case.
    #[test]
    fn band_divides_the_sweep_evenly() {
        assert_eq!(band(0.0, 4), 0);
        assert_eq!(band(0.24, 4), 0);
        assert_eq!(band(0.26, 4), 1);
        assert_eq!(band(0.99, 4), 3);
        assert_eq!(band(1.0, 4), 3, "the endpoint stays in the last band");
        assert_eq!(band(0.5, 0), 0, "a zero band count can't index anything");
        assert_eq!(band(0.5, 1), 0);
    }

    /// `ramp` hits its endpoints exactly, interpolates in between, and copes
    /// with degenerate stop lists rather than panicking on an empty slice or a
    /// `spans - 1` underflow.
    #[test]
    fn ramp_interpolates_between_its_stops() {
        let stops = [[0, 0, 0, 0xff], [0xff, 0xff, 0xff, 0xff]];
        assert_eq!(ramp(&stops, 0.0), stops[0]);
        assert_eq!(ramp(&stops, 1.0), stops[1]);
        let mid = ramp(&stops, 0.5);
        assert!(mid[0] > 0 && mid[0] < 0xff, "the midpoint is between");
        // Degenerate lists: no panic, no underflow.
        assert_eq!(ramp(&[], 0.5), [0, 0, 0, 0xff]);
        assert_eq!(ramp(&[[1, 2, 3, 0xff]], 0.5), [1, 2, 3, 0xff]);
    }

    /// The map names are stable (they are the env-var vocabulary the shell
    /// parses) and unique.
    #[test]
    fn map_names_are_stable_and_unique() {
        assert_eq!(ColorMap::Style.name(), "style");
        assert_eq!(ColorMap::Rainbow.name(), "rainbow");
        assert_eq!(ColorMap::TransPride.name(), "transpride");
        assert_eq!(ColorMap::Heat.name(), "heat");
        assert_eq!(ColorMap::Rgb(1, 2, 3).name(), "rgb");
        let mut names: Vec<_> = ColorMap::ALL.iter().map(|m| m.name()).collect();
        names.sort_unstable();
        let len = names.len();
        names.dedup();
        assert_eq!(names.len(), len, "two maps share a name");
    }

    /// The default is `Style` — i.e. "no colour map" is the old single-ink
    /// behaviour, not a surprise palette.
    #[test]
    fn the_default_map_is_the_style_ink() {
        assert_eq!(ColorMap::default(), ColorMap::Style);
    }
}
