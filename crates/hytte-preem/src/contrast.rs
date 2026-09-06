//! WCAG relative-luminance contrast (#928) — "can this ink be read on that
//! field?", answered the standard way.
//!
//! Every kit widget is a *screen*: it floods a field and stamps light into it,
//! so legibility is a property of a **pair** of palette slots, never of either
//! one alone. Until #928 nothing in the kit could say that, which is how a
//! light desktop accent came to be painted onto the reflective LCD's light
//! field and vanish (Annika's live verify on #881: *"lcd background skin
//! (greenish) has light tinted foreground so it's not readable"*).
//!
//! # The pair is the scope, and the post-passes are outside it
//!
//! A ratio here is about the **palette**, measured before a single pixel is
//! stamped. The passes that run after — the bloom, the ghost, and above all
//! [`Mask::CRT`](super::style::Mask) — move the light that actually lands, and
//! this module never sees them. The mask is the one that takes light *away*:
//! `150/256` on one scanline row in four and `115/256` at the far corner, so a
//! `Crt` ink admitted to exactly [`AA_TEXT`] reads around **2.2:1 on a comb
//! row**. Every skin's own ink is chosen with enough headroom to survive its
//! own passes (the `Crt`'s is 15.516:1 flat, 5.561:1 on a comb row); an ink
//! admitted to the bar has none. #940's
//! [`admit_role_ink`](super::style::DisplayStyle::admit_role_ink) carries the
//! numbers and the reasoning — it is the first thing in the kit that tints a
//! phosphor skin's ink *to* the bar rather than leaving it as given.
//!
//! [`ratio`] is the whole module's surface.
//! [`DisplayStyle`](super::style::DisplayStyle)'s per-skin accent policy is its
//! consumer inside the kit; it is re-exported from the crate root
//! (`contrast_ratio`, `AA_TEXT`) so a **host** resolving colors of its own can
//! ask the same question before pinning one. Pure `std`, no state, symmetric in
//! its arguments, and alpha is ignored because a kit palette is opaque by
//! construction (a screen, not a sprite).

use super::frame::Rgba;

/// WCAG 2.2 SC 1.4.3 (AA) for body text: **4.5:1**.
///
/// The kit's lit elements are small, high-frequency shapes — a five-by-seven
/// dot-matrix glyph, a seven-segment stroke, a one-pixel scope trace — so the
/// large-text relaxation (3:1) does not apply to them, and the enhanced AAA
/// bar (7:1) would leave the LCD almost no room to carry an accent's hue at
/// all. 4.5 is both the right rule for the content and the one that keeps a
/// tint visible.
pub const AA_TEXT: f32 = 4.5;

/// One sRGB channel byte → linear light, per IEC 61966-2-1 — the transfer
/// function WCAG's relative luminance is defined over.
fn linear(channel: u8) -> f32 {
    let c = f32::from(channel) / 255.0;
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG relative luminance: `0.0` for black, `1.0` for white.
///
/// Alpha is ignored — see the module docs.
fn luminance(color: Rgba) -> f32 {
    0.2126 * linear(color[0]) + 0.7152 * linear(color[1]) + 0.0722 * linear(color[2])
}

/// The WCAG contrast ratio between two colors: `1.0` when they are equally
/// luminous, `21.0` for black against white. Symmetric — the ordering of the
/// arguments never changes the answer, so a caller need not know which side is
/// the ink.
#[must_use]
pub fn ratio(a: Rgba, b: Rgba) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::{AA_TEXT, luminance, ratio};

    const BLACK: [u8; 4] = [0x00, 0x00, 0x00, 0xff];
    const WHITE: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

    /// The two fixed points of the definition. Black-on-white is 21:1 exactly
    /// (`1.05 / 0.05`), and any color against itself is 1:1 — if either drifts,
    /// the transfer function is wrong.
    ///
    /// Both are blind to the *weights*, though, which is why
    /// [`the_luminance_weights_are_the_published_ones`] exists beside this:
    /// black and white have every channel at the same value, so any three
    /// weights summing to 1 reproduce 21:1 exactly, swapped ones included.
    #[test]
    fn the_endpoints_of_the_scale_are_exact() {
        assert!(
            (ratio(BLACK, WHITE) - 21.0).abs() < 1.0e-3,
            "black on white is 21:1, got {}",
            ratio(BLACK, WHITE)
        );
        for color in [
            BLACK,
            WHITE,
            [0x9b, 0x59, 0xb6, 0xff],
            [0xa9, 0xb4, 0x7e, 0xff],
        ] {
            assert!(
                (ratio(color, color) - 1.0).abs() < 1.0e-6,
                "{color:?} against itself is 1:1"
            );
        }
    }

    /// Symmetric, as the rustdoc promises: the caller never has to know which
    /// argument is the ink.
    #[test]
    fn the_ratio_is_symmetric() {
        for a in [BLACK, WHITE, [0x5c, 0xff, 0x82, 0xff]] {
            for b in [[0xa9, 0xb4, 0x7e, 0xff], [0x23, 0x28, 0x1a, 0xff], WHITE] {
                assert!(
                    (ratio(a, b) - ratio(b, a)).abs() < 1.0e-6,
                    "ratio({a:?}, {b:?}) must equal its mirror"
                );
            }
        }
    }

    /// Alpha is not part of the answer — a kit palette is opaque, and a slot
    /// that arrived translucent is drawn opaque anyway, so letting the byte
    /// reach the math would only make the answer disagree with the pixels.
    #[test]
    fn alpha_does_not_move_the_ratio() {
        let opaque = [0x9b, 0x59, 0xb6, 0xff];
        let clear = [0x9b, 0x59, 0xb6, 0x00];
        let field = [0xa9, 0xb4, 0x7e, 0xff];
        assert!((ratio(opaque, field) - ratio(clear, field)).abs() < 1.0e-6);
    }

    /// [`AA_TEXT`] is **4.5**, the number WCAG 2.2 SC 1.4.3 states for body
    /// text, and this is the assertion that says so.
    ///
    /// It looks tautological and is not. Every other test here compares a
    /// measured ratio *against* `AA_TEXT`, so all of them follow the constant
    /// wherever it is moved: review demonstrated `4.5 → 4.0` running the whole
    /// suite green while changing every admitted ink on glass. A constant that
    /// decides what users see needs one assertion that is about its **value**
    /// rather than about its use — the same reason
    /// [`the_luminance_weights_are_the_published_ones`] exists beside a stack
    /// of tests that all use the weights.
    ///
    /// The reference pair is `WebAIM`'s published boundary, reproduced here so
    /// the threshold is anchored to the standard and not to this file: `#777777`
    /// on white fails AA (4.478:1) and `#767676`, one byte lighter, passes
    /// (4.542:1). Move `AA_TEXT` in either direction by more than ~0.05 and one
    /// of the two lands on the wrong side.
    ///
    /// **Falsified** by `AA_TEXT = 4.0` or `= 5.0`: the equality goes red, and
    /// so does whichever boundary assertion the new value crosses.
    #[test]
    fn the_aa_threshold_is_the_published_one() {
        assert!(
            (AA_TEXT - 4.5).abs() < f32::EPSILON,
            "WCAG 2.2 SC 1.4.3 body text is 4.5:1, not {AA_TEXT}"
        );

        let on_white = |c: [u8; 4]| ratio(c, WHITE);
        let fails = [0x77, 0x77, 0x77, 0xff];
        let passes = [0x76, 0x76, 0x76, 0xff];
        assert!(
            on_white(fails) < AA_TEXT,
            "#777777 on white is WebAIM's failing side: {}",
            on_white(fails)
        );
        assert!(
            on_white(passes) >= AA_TEXT,
            "#767676 on white is WebAIM's passing side: {}",
            on_white(passes)
        );
    }

    /// Each of the three primaries at full strength is its own weight — pure
    /// red is `0.2126`, pure green `0.7152`, pure blue `0.0722` — because
    /// [`linear`] takes `255` to exactly `1.0`. That makes this the one
    /// assertion here that can see the weights at all, and it is not
    /// circular: the numbers are WCAG 2.x's published sRGB coefficients, not a
    /// restatement of the code's arithmetic.
    ///
    /// **Falsified** by swapping the red and green coefficients in
    /// [`luminance`]: pure red comes back at `0.7152`.
    #[test]
    fn the_luminance_weights_are_the_published_ones() {
        for (primary, want) in [
            ([0xff, 0x00, 0x00, 0xff], 0.2126_f32),
            ([0x00, 0xff, 0x00, 0xff], 0.7152),
            ([0x00, 0x00, 0xff, 0xff], 0.0722),
        ] {
            let got = luminance(primary);
            assert!(
                (got - want).abs() < 1.0e-6,
                "{primary:?}: want {want}, got {got}"
            );
        }
    }

    /// The sRGB transfer function is a linear toe below `0.04045` spliced to a
    /// gamma-2.4 curve above it, and the splice is **continuous** — which is
    /// what makes a misplaced knee detectable: move it and the two branches
    /// stop meeting, leaving a step at the new boundary.
    ///
    /// So this walks all 256 channel bytes rather than probing near the real
    /// knee. An earlier revision checked only bytes 10 and 11, on the reasoning
    /// that they straddle it — and that test **passed** under the mutation it
    /// claimed to catch (`c <= 0.4045`, the plausible decimal-point typo),
    /// because both of those bytes sit below the wrong knee too and simply take
    /// the linear branch together. A local probe cannot see a discontinuity it
    /// is not standing on; a sweep can, wherever the knee has been moved to.
    ///
    /// The bound is generous on purpose: the correct function's largest
    /// adjacent step is ≈0.0089, right up at white where the curve is
    /// steepest, so `0.02` clears it comfortably while the mutation's step is
    /// five times over.
    ///
    /// **Falsified** by `c <= 0.4045`: red between bytes 103 and 104, step
    /// 0.107.
    #[test]
    fn the_transfer_function_is_monotone_and_has_no_step_anywhere() {
        let grey = |b: u8| luminance([b, b, b, 0xff]);
        let mut worst = 0.0_f32;
        for b in 0..u8::MAX {
            let (below, above) = (grey(b), grey(b + 1));
            assert!(
                above >= below,
                "luminance must rise with the channel byte: {b} → {}",
                b + 1
            );
            let step = above - below;
            assert!(
                step < 0.02,
                "a step of {step} between bytes {b} and {}: the toe and the \
                 curve are not meeting where they should",
                b + 1
            );
            worst = worst.max(step);
        }
        assert!(
            worst > 0.005,
            "…and the bound is not vacuous: the real function does step by \
             {worst} at its steepest"
        );
    }

    /// The reason this module exists, stated as a number: the LCD skin's own
    /// dark ink on its own greenish field clears the AA bar comfortably
    /// (≈6.8:1), while the accent that provoked #928 — anything light — does
    /// not. So the skin's *own* palette was never the problem, and `style`'s
    /// `admit` ramp, whose last stop is that ink, always has a legible stop to
    /// land on.
    #[test]
    fn the_lcd_skins_own_pair_clears_the_bar_and_a_light_ink_does_not() {
        let field = [0xa9, 0xb4, 0x7e, 0xff];
        let ink = [0x23, 0x28, 0x1a, 0xff];
        assert!(
            ratio(ink, field) >= AA_TEXT,
            "the LCD's own ink on its own field: {}",
            ratio(ink, field)
        );
        assert!(
            ratio(WHITE, field) < AA_TEXT,
            "a white ink on that field is the bug: {}",
            ratio(WHITE, field)
        );
    }
}
