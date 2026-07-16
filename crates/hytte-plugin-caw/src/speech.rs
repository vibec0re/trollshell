//! caw's speech, rendered in the preem raster kit's **pixel font** — dressed
//! in her violet palette (#368).
//!
//! Annika asked for caw's speech to read like the rest of her: the same
//! chunky-pixel look as her LCD face and the preem VFD/dot-matrix screens,
//! not the shell's TTF. So her line is a preem [`TextBox`] rendered to a
//! [`Node::Pixels`], not a real-font label. This trades the TTF's crisp
//! readability and free wrapping for the on-brand 8-bit look (#368); it's a
//! one-module swap to revert if a hybrid is ever wanted.
//!
//! No preset [`DisplayStyle`](hytte_plugin::preem::DisplayStyle) is violet
//! (they are cyan-VFD, olive-LCD, and blue-OLED), so — exactly as the pet
//! dresses its bubble in lilac — we build a plain [`TextBox`] in caw's own
//! palette: the field is the old `.caw-bubble` violet, the ink her `.caw-say`
//! lilac-white, the notdef box a dim violet. The buffer bakes its own field +
//! transparent rounded corners and is upscaled ×[`SCALE`], so (like the face)
//! the shell's CSS fills are ignored — see the `.caw-say` note in `style.css`.

use hytte_plugin::preem::{Rgba, TextBox};
use hytte_plugin::proto::Node;

/// Integer pixel-scale baked into the speech buffer — matches the face's 2×
/// so the two screens read at the same chunkiness.
const SCALE: usize = 2;

/// On-screen width budget for the speech slot: the face renders at 256 px
/// (128 px buffer, `.caw-lcd` min-width 256), so the bubble sits at most that
/// wide directly beneath it. `fit_px` accounts for the ×[`SCALE`] upscale and
/// padding when it picks the wrap width.
const SLOT_PX: usize = 256;

/// Hard cap on wrapped lines; a longer utterance is truncated with a trailing
/// `…` by the kit's wrap — so a chatty crow can never blow up the buffer.
const MAX_LINES: usize = 4;

/// Speech field: the old `.caw-bubble` violet (`rgb(58, 34, 80)`), opaque; the
/// corners are cut to transparent by the box.
const FIELD: Rgba = [0x3a, 0x22, 0x50, 0xff];
/// Text ink: caw's `.caw-say` lilac-white.
const INK: Rgba = [0xf3, 0xe9, 0xff, 0xff];
/// The notdef box for an uncovered char (e.g. emoji) — a dim violet outline.
const NOTDEF: Rgba = [0x6c, 0x4e, 0x86, 0xff];

/// caw's speech box: a preem [`TextBox`] in her violet palette, wrapped to the
/// [`SLOT_PX`] slot and `…`-capped at [`MAX_LINES`], upscaled ×[`SCALE`].
/// Hugging (not fixed-width) so a short line stays a compact chip the plugin's
/// Spacer row centers, and a long one fills the slot and wraps down.
fn speech() -> TextBox {
    TextBox::new()
        .fit_px(SLOT_PX)
        .max_lines(MAX_LINES)
        .scale(SCALE)
        .colors(FIELD, INK, NOTDEF)
}

/// Render `line` into caw's chunky-pixel speech [`Node::Pixels`]. Valid for
/// every input (the empty string, emoji, an overlong utterance): the buffer
/// always satisfies the host's `len == w * h * 4` invariant.
pub(crate) fn speech_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    speech().render(line).into_node(Some(id), classes)
}

#[cfg(test)]
mod tests {
    use super::{INK, MAX_LINES, NOTDEF, speech_node};
    use hytte_plugin::preem::font;
    use hytte_plugin::proto::Node;

    /// Unwrap the speech node's `Pixels` parts.
    fn pixels(line: &str) -> (u32, u32, Vec<u8>) {
        match speech_node(line, "caw-say", vec!["caw-say".to_owned()]) {
            Node::Pixels {
                width,
                height,
                data,
                ..
            } => (width, height, data),
            other => panic!("speech is a Pixels node, got {other:?}"),
        }
    }

    /// The buffer invariant the host enforces: `len == width * height * 4`,
    /// across caw's canned lines, the accent/emoji torture set, and
    /// pathological inputs — none may panic or produce a mis-sized buffer.
    #[test]
    fn every_speech_buffer_satisfies_the_host_invariant() {
        let lines = [
            "",
            "Rogue DHCP mode engaged",
            "caw?! you dare poke a rogue DHCP server",
            "*unbound process, purring*",
            "hoarding shiny MAC addresses…",
            "smörgåsbord ölçäÜ é ßÅÄÖ",
            "💕 unmapped: ☺ \u{1F63A}",
            "supercalifragilisticexpialidocious",
            &"caw ".repeat(40),
            &"a".repeat(200),
        ];
        for line in lines {
            let (w, h, data) = pixels(line);
            assert_eq!(
                data.len(),
                w as usize * h as usize * 4,
                "buffer for {line:?} must be w*h*4"
            );
            assert!(w > 0 && h > 0, "buffer for {line:?} is non-degenerate");
        }
    }

    /// The slot is bounded: a very long utterance can't blow up the buffer —
    /// it caps at [`MAX_LINES`] rows and never exceeds the ~256 px slot width.
    #[test]
    fn long_speech_is_capped_and_fits_the_slot() {
        let (_, tall_h, _) = pixels(&"caw ".repeat(200));
        let max_h = 2 * 3 + MAX_LINES * font::GLYPH_H + (MAX_LINES - 1) * font::LINE_GAP;
        // ×2 scale (see SCALE), with a little slack for padding rounding.
        assert!(
            (tall_h as usize) <= (max_h * 2) + 8,
            "height {tall_h} is capped at {MAX_LINES} rows"
        );
        let (wide_w, ..) = pixels(&"a".repeat(200));
        assert!(wide_w <= 256, "slot width {wide_w} fits beneath the face");
    }

    /// A short line hugs (a compact chip), a wrapping line grows the slot —
    /// the "short lines hug, long lines fill and wrap" behaviour.
    #[test]
    fn short_hugs_long_wraps_taller() {
        let (short_w, short_h, _) = pixels("caw");
        let (long_w, long_h, _) = pixels("caw caw caw caw caw caw caw caw caw caw");
        assert!(long_w > short_w, "a wrapping line is wider than a tiny one");
        assert!(long_h > short_h, "and wraps down into more rows");
    }

    /// A known line renders deterministically and draws lilac ink pixels.
    #[test]
    fn a_known_line_renders_deterministically_with_ink() {
        let a = pixels("Rogue DHCP mode engaged");
        let b = pixels("Rogue DHCP mode engaged");
        assert_eq!(a, b, "render is pure");
        assert!(
            a.2.chunks_exact(4).any(|px| px == INK),
            "the text draws lilac ink"
        );
    }

    /// An uncovered char (emoji) box-falls-back to the dim violet notdef and
    /// never panics.
    #[test]
    fn uncovered_chars_fall_back_to_the_box() {
        let (_, _, data) = pixels("💕");
        assert!(
            data.chunks_exact(4).any(|px| px == NOTDEF),
            "the notdef box draws in the dim violet"
        );
    }
}
