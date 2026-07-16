//! The pet's speech bubble: the SDK's shared **5×7 pixel font**, dressed in
//! the pet's lilac.
//!
//! This module used to *be* the font (#304) — the glyph bitmaps, word-wrap,
//! and renderer were all hand-rolled here. Issue #356 promoted that whole
//! toolkit into [`hytte_plugin::preem`] (`preem::font` + [`TextBox`]); what
//! remains is only the pet-specific dressing:
//!
//! - the **lilac palette** matching `face.rs` (field a touch lighter than
//!   the LCD's `SCREEN_BG`, bright-lilac ink, dim-lilac `.notdef` box);
//! - the **fixed bubble slot** beside the 128 px face (#313/#323): the
//!   compact row packs the bubble at its natural buffer width, so the width
//!   is pinned and long text wraps *down* (capped at [`MAX_LINES`], marked
//!   with `…`) instead of growing sideways;
//! - the ×[`SCALE`] chunk baked into the buffer — bigger chunky pixels have
//!   to come from the buffer itself, since the host does no upscaling of a
//!   natural-size `Pixels`.
//!
//! Coverage, wrapping, and the dim hollow box for uncovered chars (emoji
//! never panic) are all the kit's — see `preem::font`'s docs.

use hytte_plugin::preem::{Rgba, TextBox};
use hytte_plugin::proto::Node;

/// Integer pixel-scale baked into the bubble buffer (#323 — bigger pixel
/// font, multiline).
const SCALE: usize = 2;

/// Target **on-screen** pixel width of the bubble's slot beside the face in
/// the compact row (#313). The sidebar card is 320 px; inside its ~12 px
/// padding (~296 px) the 128 px LCD face plus its bezel/border eats
/// ~143 px, so this is roughly the width left for the bubble.
const BUBBLE_SLOT_PX: usize = 126;

/// Hard cap on wrapped lines; an overflow is truncated with a trailing `…`.
const MAX_LINES: usize = 3;

/// Bubble background: a lilac a touch lighter than the face's screen field
/// (`SCREEN_BG` in `face.rs`), opaque. Corners are cut to transparent.
const BUBBLE_BG: Rgba = [0x3a, 0x22, 0x50, 0xff];
/// Text ink: bright lilac, matching the face's fur/highlight family.
const INK: Rgba = [0xf0, 0xe0, 0xf8, 0xff];
/// The `.notdef` box for an uncovered char — a dim lilac outline.
const NOTDEF: Rgba = [0x6c, 0x4e, 0x86, 0xff];

/// The pet's bubble as a kit [`TextBox`]: a fixed-width slot sized to
/// [`BUBBLE_SLOT_PX`], wrapped and `…`-capped, lilac on lilac.
fn bubble() -> TextBox {
    TextBox::new()
        .fit_px(BUBBLE_SLOT_PX)
        .max_lines(MAX_LINES)
        .scale(SCALE)
        .fixed_width(true)
        .colors(BUBBLE_BG, INK, NOTDEF)
}

/// Render `line` into a chunky-pixel speech bubble [`Node::Pixels`]. The
/// buffer is a **fixed-width** slot (background baked in, transparent
/// rounded corners), upscaled ×[`SCALE`] for the 8-bit look — so it never
/// resizes as the message length changes (#323); longer text wraps down
/// into more rows instead. Satisfies the host's `len == w * h * 4`
/// invariant for every input (including the empty string).
pub(crate) fn bubble_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    bubble().render(line).into_node(Some(id), classes)
}

#[cfg(test)]
mod tests {
    use super::{INK, bubble_node};
    use crate::brain::{self, ThinkKind, ThinkReq};
    use hytte_plugin::proto::Node;

    /// Unwrap the bubble's `Pixels` parts.
    fn pixels(line: &str) -> (u32, u32, Vec<u8>) {
        match bubble_node(line, "pet-bubble", vec!["pet-bubble".to_owned()]) {
            Node::Pixels {
                width,
                height,
                data,
                ..
            } => (width, height, data),
            other => panic!("bubble is a Pixels node, got {other:?}"),
        }
    }

    /// The buffer invariant the host enforces: `len == width * height * 4`,
    /// for every canned line and every mood word, plus the accent/box
    /// torture set — the same sweep the pre-#356 in-crate font carried.
    #[test]
    fn every_bubble_buffer_satisfies_the_host_invariant() {
        let mut lines: Vec<String> = Vec::new();
        // Sweep the brain's canned pools through every context.
        for kind in [ThinkKind::Poke, ThinkKind::Idle] {
            for pokes in [0, 1, crate::GRUMPY_AT] {
                for mood in ["happy", "sleepy", "excited", "grumpy", "thinking"] {
                    let req = ThinkReq {
                        kind,
                        hour: 2,
                        mood,
                        pokes,
                    };
                    for step in 0..8 {
                        lines.push(brain::canned(req, step));
                    }
                }
            }
        }
        // Accents that survive sanitize, an emoji that must box-fallback,
        // and the pathological inputs.
        lines.push("smörgåsbord ölçäÜ é ßÅÄÖ".to_owned());
        lines.push("💕 unmapped: ☺ \u{1F63A}".to_owned());
        lines.push(String::new());
        lines.push("a".repeat(80));
        lines.push("supercalifragilisticexpialidocious".to_owned());

        for line in &lines {
            let (w, h, data) = pixels(line);
            assert_eq!(
                data.len(),
                w as usize * h as usize * 4,
                "buffer for {line:?} must be w*h*4"
            );
            assert!(w > 0 && h > 0, "buffer for {line:?} is non-degenerate");
        }
    }

    /// The slot is fixed-width (#323): the bubble never resizes sideways
    /// with the message, and it fits its ~126 px slot beside the face.
    #[test]
    fn bubble_width_is_fixed_and_fits_the_slot() {
        let (short_w, ..) = pixels("hi");
        let (long_w, long_h, _) = pixels(&"word ".repeat(12));
        assert_eq!(short_w, long_w, "width never varies with the text");
        assert!(long_w <= 126, "slot width {long_w} fits beside the face");
        let (_, short_h, _) = pixels("hi");
        assert!(long_h > short_h, "long text wraps down instead");
    }

    /// A known short string renders deterministically and draws lilac ink.
    #[test]
    fn a_known_string_renders_deterministically_with_ink() {
        let a = pixels("mrrp!");
        let b = pixels("mrrp!");
        assert_eq!(a, b, "render is pure");
        let (_, _, data) = a;
        assert!(
            data.chunks_exact(4).any(|px| px == INK),
            "the text draws ink pixels"
        );
    }

    /// An uncovered char (emoji) box-falls-back and never panics.
    #[test]
    fn uncovered_chars_fall_back_to_the_box() {
        let (_, _, data) = pixels("💕");
        assert!(!data.is_empty());
        assert!(
            data.chunks_exact(4).any(|px| px == super::NOTDEF),
            "the notdef box draws in the dim lilac"
        );
    }
}
