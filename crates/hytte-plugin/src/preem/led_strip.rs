//! The **LED peak/level strip**: a row of discrete LEDs lighting up with a
//! `0.0..=1.0` level, topped by a peak-hold dot that floats above the level and
//! decays back down — the kit's VU-meter widget (issue #506).
//!
//! Like every other kit widget it renders in a [`DisplayStyle`] skin: the lit
//! LEDs glow in the palette ink (accent-tinted, #376), the unlit ones show the
//! ghost matrix through on ghosting skins, and glowing skins bloom the lit
//! segments. The single **peak-hold** LED is composited toward a brightened cap
//! color so it reads as a distinct bright dot riding the top of the level.
//!
//! # State lives in the plugin (the kit owns no clock)
//!
//! [`led_strip`] renders are **pure**: it takes the current `level` and the
//! peak-hold `peak` position and draws one frame — it holds nothing between
//! frames, exactly like [`dot_matrix`](super::dot_matrix) and the
//! [`Marquee`](super::Marquee). The peak-hold *value* is modelled by the small
//! pure [`PeakHold`] helper the plugin drives: [`push`](PeakHold::push) each
//! fresh level in (the dot only ever rises to it), [`decay`](PeakHold::decay)
//! once per animation tick (the dot falls back). Splitting the decay out of the
//! renderer keeps the cadence the plugin's, matching the kit's stance that the
//! plugin owns the frame timer (see the `preem` module docs on timing).
//!
//! # Sizing — the #313 lesson
//!
//! Sized via its **buffer dimensions** (a `Pixels` node's natural size), never
//! shell CSS: a shell CSS minimum below the buffer size is a silent no-op (the
//! `.caw-lcd`/`.pet-lcd` lesson). The default [`LedStrip::leds`] count of
//! [`DEFAULT_LEDS`] renders [`DEFAULT_WIDTH`] px wide — inside the ~296 px
//! sidebar card — and the count is the one knob that changes the width.

use super::frame::{Frame, Rgba};
use super::style::{DisplayStyle, Emission, mix};

/// Default LED count: 24 segments render [`DEFAULT_WIDTH`] px wide, inside the
/// ~296 px sidebar card (see the module docs on sizing).
pub const DEFAULT_LEDS: usize = 24;

/// One LED cell's width in buffer pixels.
const CELL_W: usize = 8;
/// One LED cell's height in buffer pixels (a single chunky row).
const CELL_H: usize = 16;
/// Blank field gap between adjacent LED cells.
const GAP: usize = 3;
/// Field padding around the LED row, on every side.
const PAD: usize = 4;

/// The rendered width of a strip of [`DEFAULT_LEDS`] LEDs at the kit metrics:
/// `2*PAD + n*CELL_W + (n-1)*GAP` = `8 + 192 + 69` = 269 px.
pub const DEFAULT_WIDTH: usize = 2 * PAD + DEFAULT_LEDS * CELL_W + (DEFAULT_LEDS - 1) * GAP;

/// White — the endpoint the peak-hold dot's cap color mixes the ink toward, so
/// the dot reads brighter than the lit level even on a glow-free skin.
const WHITE: Rgba = [0xff, 0xff, 0xff, 0xff];
/// How far to mix the ink toward [`WHITE`] for the peak-hold cap (`t`/255).
const CAP_MIX: u16 = 130;

/// How many LEDs a `0.0..=1.0` level lights, rounding to the nearest segment and
/// clamped to `[0, leds]`. `1.0` fills the strip; `0.0` lights nothing; monotone
/// in `level`. A `NaN` level lights nothing (the saturating `as usize` cast).
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn lit_count(level: f32, leds: usize) -> usize {
    // `leds` is a small segment count and the product is `0.0..=leds`, so the
    // round-then-cast neither loses precision nor wraps; `.min(leds)` caps an
    // over-unit level (and the saturating cast maps NaN → 0).
    ((level.clamp(0.0, 1.0) * leds as f32).round() as usize).min(leds)
}

/// The LED index the peak-hold dot sits on for a `0.0..=1.0` peak, or `None`
/// when the peak has decayed to (or below) zero — nothing to mark. `1.0` marks
/// the top LED, a peak just above zero marks the first; monotone in `peak`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn peak_led(peak: f32, leds: usize) -> Option<usize> {
    // A rested/negative peak or a NaN reads as "no dot" (the `is_nan` arm keeps
    // the `<= 0.0` check off NaN, and stays clear of a negated `>` comparison).
    if leds == 0 || peak.is_nan() || peak <= 0.0 {
        return None;
    }
    // ceil so any positive peak lights at least the first LED; clamp into
    // `1..=leds` before the `- 1` so `1.0` lands on the last index, never past
    // it. Same cast-safety as `lit_count`.
    let idx = (peak.clamp(0.0, 1.0) * leds as f32).ceil() as usize;
    Some(idx.clamp(1, leds) - 1)
}

/// A decaying **peak-hold** value in `0.0..=1.0`: the recent maximum of a level,
/// falling by a fixed rate each tick — the state behind the LED strip's peak
/// dot. Pure (no clock): the plugin [`push`](Self::push)es each fresh level and
/// [`decay`](Self::decay)s once per animation tick, choosing the tick rate that
/// gives the wall-clock fall time it wants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakHold {
    value: f32,
    rate: f32,
}

impl PeakHold {
    /// A peak-hold starting at rest (`0.0`), falling `rate` per [`decay`](Self::decay)
    /// tick. A negative `rate` is clamped to `0.0` (a dot that never falls).
    #[must_use]
    pub fn new(rate: f32) -> Self {
        Self {
            value: 0.0,
            rate: rate.max(0.0),
        }
    }

    /// Raise the held value to `level` if it is higher — the dot only ever rises
    /// to a fresh peak, never drops on a quieter frame (that's [`decay`](Self::decay)'s
    /// job). `level` is clamped to `0.0..=1.0`; a `NaN` is ignored.
    pub fn push(&mut self, level: f32) {
        let level = level.clamp(0.0, 1.0);
        if level > self.value {
            self.value = level;
        }
    }

    /// Fall one tick: drop the held value by the decay rate, never below `0.0`.
    pub fn decay(&mut self) {
        self.value = (self.value - self.rate).max(0.0);
    }

    /// The current held value, in `0.0..=1.0` — feed it as `led_strip`'s `peak`.
    #[must_use]
    pub fn value(&self) -> f32 {
        self.value
    }
}

/// A builder for an [`LedStrip::render`]: the skin and the LED count. A value,
/// so one `LedStrip` renders many frames (matching [`Marquee`](super::Marquee) /
/// [`TextBox`](super::TextBox)).
#[derive(Debug, Clone)]
pub struct LedStrip {
    style: DisplayStyle,
    leds: usize,
}

impl LedStrip {
    /// A strip in `style` with [`DEFAULT_LEDS`] segments.
    #[must_use]
    pub fn new(style: DisplayStyle) -> Self {
        Self {
            style,
            leds: DEFAULT_LEDS,
        }
    }

    /// Set the segment count (clamped to at least 1). The rendered width is
    /// `2*PAD + n*CELL_W + (n-1)*GAP` px — keep it within the ~296 px sidebar
    /// card (the default [`DEFAULT_LEDS`] is [`DEFAULT_WIDTH`] px).
    #[must_use]
    pub fn leds(mut self, n: usize) -> Self {
        self.leds = n.max(1);
        self
    }

    /// Render the strip: `level` (`0.0..=1.0`) lights the LEDs from the left, and
    /// `peak` (`0.0..=1.0`) places the bright peak-hold dot. The buffer is fully
    /// opaque and always satisfies the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, level: f32, peak: f32) -> Frame {
        let palette = self.style.palette();
        let leds = self.leds.max(1);
        let width = 2 * PAD + leds * CELL_W + (leds - 1) * GAP;
        let height = 2 * PAD + CELL_H;
        let mut frame = Frame::filled(width, height, palette.bg);

        // Ghost pass: the unlit LED matrix shows through on ghosting skins —
        // every cell, lit or not, exactly like the hardware.
        if let Some(ghost) = palette.ghost {
            for i in 0..leds {
                fill_cell(&mut frame, i, ghost);
            }
        }

        // Level pass: stamp the lit LEDs, bloom on glowing skins, composite
        // toward the ink.
        let count = lit_count(level, leds);
        let mut lit = Emission::new(width, height);
        for i in 0..count {
            stamp_cell(&mut lit, i);
        }
        if let Some(bloom) = palette.bloom {
            lit.bloom(bloom);
        }
        lit.composite(&mut frame, palette.ink);

        // Peak-hold dot: a single LED composited toward a brightened cap so it
        // reads distinct from the level even on a glow-free skin.
        if let Some(idx) = peak_led(peak, leds) {
            let cap = mix(palette.ink, WHITE, CAP_MIX);
            let mut dot = Emission::new(width, height);
            stamp_cell(&mut dot, idx);
            if let Some(bloom) = palette.bloom {
                dot.bloom(bloom);
            }
            dot.composite(&mut frame, cap);
        }

        frame
    }
}

/// Convenience free-function form: render an [`LedStrip`] with the default LED
/// count in one call, mirroring [`dot_matrix`](super::dot_matrix) /
/// [`seven_seg`](super::seven_seg).
#[must_use]
pub fn led_strip(level: f32, peak: f32, style: DisplayStyle) -> Frame {
    LedStrip::new(style).render(level, peak)
}

/// The x of LED cell `i`'s left edge.
fn cell_x0(i: usize) -> usize {
    PAD + i * (CELL_W + GAP)
}

/// Paint LED cell `i` flat into the frame (the ghost pass).
fn fill_cell(frame: &mut Frame, i: usize, color: Rgba) {
    let x0 = cell_x0(i);
    for y in PAD..PAD + CELL_H {
        for x in x0..x0 + CELL_W {
            frame.set(x, y, color);
        }
    }
}

/// Stamp LED cell `i` into the emission grid at full intensity (the lit pass).
fn stamp_cell(lit: &mut Emission, i: usize) {
    let x0 = cell_x0(i);
    for y in PAD..PAD + CELL_H {
        for x in x0..x0 + CELL_W {
            lit.add(x, y, 255);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DisplayStyle, Frame};
    use super::{
        CELL_W, DEFAULT_LEDS, DEFAULT_WIDTH, GAP, LedStrip, PAD, PeakHold, led_strip, lit_count,
        peak_led,
    };

    // ── LED mapping math ─────────────────────────────────────────────────────

    /// `lit_count` fills the strip at 1.0, empties at 0.0, is about half at 0.5,
    /// clamps an over-unit level, and never lights a negative/NaN level.
    #[test]
    fn lit_count_maps_level_to_segments() {
        let n = 24;
        assert_eq!(lit_count(0.0, n), 0, "silence lights nothing");
        assert_eq!(lit_count(1.0, n), n, "full level fills the strip");
        assert_eq!(lit_count(0.5, n), 12, "half level, half the LEDs");
        assert_eq!(lit_count(2.0, n), n, "an over-unit level clamps to full");
        assert_eq!(lit_count(-1.0, n), 0, "a negative level lights nothing");
        assert_eq!(lit_count(f32::NAN, n), 0, "NaN lights nothing (no panic)");
    }

    /// `lit_count` is monotone non-decreasing across the whole range.
    #[test]
    fn lit_count_is_monotone() {
        let n = 32;
        let mut prev = 0;
        for step in 0..=100 {
            #[allow(clippy::cast_precision_loss)]
            let level = step as f32 / 100.0;
            let c = lit_count(level, n);
            assert!(c >= prev, "level {level} lit fewer LEDs than a quieter one");
            assert!(c <= n);
            prev = c;
        }
    }

    /// `peak_led` marks the top LED at 1.0, the first just above 0, nothing at
    /// (or below) 0, and is monotone.
    #[test]
    fn peak_led_places_the_dot() {
        let n = 24;
        assert_eq!(peak_led(0.0, n), None, "a rested peak marks no LED");
        assert_eq!(peak_led(-0.5, n), None, "a negative peak marks no LED");
        assert_eq!(peak_led(f32::NAN, n), None, "NaN marks no LED (no panic)");
        assert_eq!(
            peak_led(1.0, n),
            Some(n - 1),
            "a full peak marks the top LED"
        );
        assert_eq!(peak_led(0.01, n), Some(0), "a whisper marks the first LED");
        // Monotone: a higher peak never sits on a lower LED.
        let mut prev = 0;
        for step in 1..=100 {
            #[allow(clippy::cast_precision_loss)]
            let peak = step as f32 / 100.0;
            let idx = peak_led(peak, n).expect("positive peak marks an LED");
            assert!(idx >= prev, "peak {peak} sat lower than a quieter one");
            assert!(idx < n);
            prev = idx;
        }
    }

    // ── Peak-hold decay ──────────────────────────────────────────────────────

    /// `push` only ever raises the held value; a quieter push never lowers it.
    #[test]
    fn peak_hold_push_only_rises() {
        let mut h = PeakHold::new(0.1);
        // The held value is clamped `>= 0.0`, so `<= 0.0` proves exact rest
        // without a float `==` (which clippy's `float_cmp` forbids).
        assert!(h.value() <= 0.0, "starts at rest");
        h.push(0.7);
        assert!((h.value() - 0.7).abs() < 1e-6, "rises to a fresh peak");
        h.push(0.3);
        assert!((h.value() - 0.7).abs() < 1e-6, "a quieter push holds");
        h.push(0.9);
        assert!((h.value() - 0.9).abs() < 1e-6, "a louder push rises");
    }

    /// `push` clamps its input and ignores NaN.
    #[test]
    fn peak_hold_push_clamps() {
        let mut h = PeakHold::new(0.1);
        h.push(2.0);
        assert!(
            (h.value() - 1.0).abs() < 1e-6,
            "an over-unit push clamps to 1"
        );
        let held = h.value();
        h.push(f32::NAN);
        assert!((h.value() - held).abs() < 1e-6, "NaN is ignored");
        h.push(-5.0);
        assert!(
            (h.value() - held).abs() < 1e-6,
            "a negative push can't lower it"
        );
    }

    /// `decay` falls by the rate each tick and never goes below zero; a negative
    /// rate is clamped so the dot never falls.
    #[test]
    fn peak_hold_decays_to_zero() {
        let mut h = PeakHold::new(0.25);
        h.push(1.0);
        h.decay();
        assert!(
            (h.value() - 0.75).abs() < 1e-6,
            "one tick falls by the rate"
        );
        h.decay();
        h.decay();
        h.decay();
        // Clamped `>= 0.0`, so `<= 0.0` is exact rest (no undershoot) sans a
        // float `==`.
        assert!(h.value() <= 0.0, "bottoms out at exactly zero, no undershoot");
        h.decay();
        assert!(h.value() <= 0.0, "and stays there");

        let mut frozen = PeakHold::new(-1.0);
        frozen.push(0.5);
        frozen.decay();
        assert!(
            (frozen.value() - 0.5).abs() < 1e-6,
            "a clamped rate never falls"
        );
    }

    /// The full loop: a loud frame raises the dot, silence lets it decay back —
    /// the plugin's push-on-audio / decay-on-tick cadence.
    #[test]
    fn peak_hold_rises_then_decays() {
        let mut h = PeakHold::new(0.1);
        h.push(0.8);
        let peak = h.value();
        // Quiet frames: the level is low but the dot floats, decaying slowly.
        for _ in 0..3 {
            h.push(0.1);
            h.decay();
        }
        assert!(h.value() < peak, "the held dot falls during quiet frames");
        assert!(h.value() > 0.0, "but hasn't hit the floor yet");
    }

    // ── Renderer ─────────────────────────────────────────────────────────────

    /// The host invariant across skins, levels, and peaks — extremes included.
    #[test]
    fn every_buffer_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for &(level, peak) in &[(0.0, 0.0), (0.5, 0.7), (1.0, 1.0), (2.0, -1.0)] {
                let f = LedStrip::new(style).render(level, peak);
                assert_eq!(f.data().len(), f.width() * f.height() * 4, "{style:?}");
                assert!(f.width() > 0 && f.height() > 0);
            }
        }
    }

    /// The strip is a screen: every pixel is opaque, wall to wall.
    #[test]
    fn every_pixel_is_opaque() {
        for style in DisplayStyle::ALL {
            let f = LedStrip::new(style).render(0.6, 0.8);
            assert!(
                f.data().chunks_exact(4).all(|px| px[3] == 0xff),
                "{style:?} strip is opaque"
            );
        }
    }

    /// The default strip is [`DEFAULT_WIDTH`] px and fits the sidebar card; the
    /// count knob drives the width and clamps a zero count to one LED.
    #[test]
    fn dimensions_follow_the_metrics() {
        let f = led_strip(0.5, 0.5, DisplayStyle::Vfd);
        assert_eq!(f.width(), DEFAULT_WIDTH);
        assert_eq!(f.width(), 269, "24 LEDs render 269 px wide");
        assert!(f.width() <= 296, "the default fits the sidebar card");
        assert_eq!(DEFAULT_LEDS, 24);
        // The count knob widens the buffer predictably.
        let ten = LedStrip::new(DisplayStyle::Vfd).leds(10).render(1.0, 1.0);
        assert_eq!(ten.width(), 2 * PAD + 10 * CELL_W + 9 * GAP);
        // A zero count clamps to one LED rather than a degenerate buffer.
        let zero = LedStrip::new(DisplayStyle::Vfd).leds(0).render(1.0, 1.0);
        assert_eq!(zero.width(), 2 * PAD + CELL_W);
    }

    /// A louder level lights more of the strip: more non-background pixels.
    #[test]
    fn a_louder_level_lights_more() {
        fn lit_pixels(level: f32) -> usize {
            let bg = DisplayStyle::Oled.palette().bg;
            let f = LedStrip::new(DisplayStyle::Oled).render(level, 0.0);
            f.data().chunks_exact(4).filter(|px| *px != bg).count()
        }
        // OLED has no ghost, so background pixels are only the unlit field —
        // lit pixels grow strictly with the level.
        assert_eq!(lit_pixels(0.0), 0, "silence lights nothing on OLED");
        assert!(lit_pixels(0.25) < lit_pixels(0.5));
        assert!(lit_pixels(0.5) < lit_pixels(1.0));
    }

    /// The peak-hold dot actually paints: a peak above the level lights pixels a
    /// bare level render leaves dark.
    #[test]
    fn the_peak_dot_paints_above_the_level() {
        let no_dot = LedStrip::new(DisplayStyle::Oled).render(0.2, 0.0);
        let with_dot = LedStrip::new(DisplayStyle::Oled).render(0.2, 0.9);
        assert_ne!(no_dot, with_dot, "a floating peak dot changes the render");
        let bg = DisplayStyle::Oled.palette().bg;
        let lit = |f: &Frame| f.data().chunks_exact(4).filter(|px| *px != bg).count();
        assert!(lit(&with_dot) > lit(&no_dot), "the dot lights extra pixels");
    }

    /// Renders are deterministic, and the three skins render differently.
    #[test]
    fn render_is_deterministic_and_skins_differ() {
        assert_eq!(
            led_strip(0.6, 0.8, DisplayStyle::Vfd),
            led_strip(0.6, 0.8, DisplayStyle::Vfd)
        );
        let vfd = led_strip(0.6, 0.8, DisplayStyle::Vfd);
        let lcd = led_strip(0.6, 0.8, DisplayStyle::Lcd);
        let oled = led_strip(0.6, 0.8, DisplayStyle::Oled);
        assert_ne!(vfd, lcd);
        assert_ne!(vfd, oled);
        assert_ne!(lcd, oled);
    }
}
