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
//!
//! # On the state path since #884/#885
//!
//! Those three colors are now three **wire pins** rather than a local
//! `colors()` call, so the box is a
//! [`display::TextBox`](hytte_plugin::display::TextBox): against a
//! preem-speaking shell it ships as a typed `Node::Preem` and the shell draws
//! it, against an older one it rasterises to the same `Node::Pixels` byte for
//! byte. caw was #884's original blocker — her violet is exactly the "the color
//! *is* the meaning" case the vocabulary had no word for — and #912's ink-only
//! pin was not enough, because she sets a field and a notdef too. Annika settled
//! the widening on #885.
//!
//! Her **face** stays on the raster escape hatch permanently (`face.rs`): it is
//! a hand-drawn `Frame`, and the vocabulary has no word for that.

use hytte_plugin::display::TextBox;
use hytte_plugin::preem::Rgba;
use hytte_plugin::proto::Node;
use hytte_plugin::proto::preem::StyleName;

/// Integer pixel-scale baked into the speech buffer — matches the face's 2×
/// so the two screens read at the same chunkiness.
const SCALE: u32 = 2;

/// On-screen width budget for the speech slot: the face renders at 256 px
/// (128 px buffer, `.caw-lcd` min-width 256), so the bubble sits at most that
/// wide directly beneath it. `fit_px` accounts for the ×[`SCALE`] upscale and
/// padding when it picks the wrap width.
const SLOT_PX: u32 = 256;

/// Hard cap on wrapped lines; a longer utterance is truncated with a trailing
/// `…` by the kit's wrap — so a chatty crow can never blow up the buffer.
const MAX_LINES: u32 = 4;

/// Line cap for the once-a-day **morning briefing** (#407): two-to-three short
/// sentences need roughly double the ordinary bubble, so the news gets a
/// taller box while normal chatter keeps its compact [`MAX_LINES`].
const MAX_LINES_BRIEFING: u32 = 8;

/// The skin the box travels as. **Inert**, for the reason the pet's is: a
/// `TextBox` draws three colors and all three are pinned below, so no skin's
/// palette reaches a pixel. Stated as `Lcd` to match the face beside it.
const STYLE: StyleName = StyleName::Lcd;

/// Speech field: the old `.caw-bubble` violet (`rgb(58, 34, 80)`), opaque; the
/// corners are cut to transparent by the box.
const FIELD: Rgba = [0x3a, 0x22, 0x50, 0xff];
/// Text ink: caw's `.caw-say` lilac-white.
const INK: Rgba = [0xf3, 0xe9, 0xff, 0xff];
/// The notdef box for an uncovered char (e.g. emoji) — a dim violet outline.
const NOTDEF: Rgba = [0x6c, 0x4e, 0x86, 0xff];

/// caw's speech box: a [`TextBox`] in her violet palette, wrapped to the
/// [`SLOT_PX`] slot and `…`-capped at `max_lines`, upscaled ×[`SCALE`].
/// Hugging (not fixed-width) so a short line stays a compact chip the plugin's
/// Spacer row centers, and a long one fills the slot and wraps down.
///
/// The three pins are #885's palette override, and they are the exact three
/// colors the pre-seam `colors(FIELD, INK, NOTDEF)` call set — which is what
/// makes the raster arm byte-identical to it.
fn speech(max_lines: u32) -> TextBox {
    TextBox::new(STYLE)
        .fit_px(SLOT_PX)
        .max_lines(max_lines)
        .scale(SCALE)
        .field(FIELD)
        .ink(INK)
        .notdef(NOTDEF)
}

/// Render `line` into caw's chunky-pixel speech node, in whichever wire shape
/// this session negotiated. Valid for every input (the empty string, emoji, an
/// overlong utterance): on the raster arm the buffer always satisfies the host's
/// `len == w * h * 4` invariant.
pub(crate) fn speech_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    speech(MAX_LINES).node_classed(id, classes, line)
}

/// The **briefing** variant of [`speech_node`]: same palette, same slot width,
/// but [`MAX_LINES_BRIEFING`] rows so the morning news (#407) fits without
/// truncating mid-sentence.
pub(crate) fn briefing_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    speech(MAX_LINES_BRIEFING).node_classed(id, classes, line)
}

#[cfg(test)]
mod tests {
    use super::{FIELD, INK, MAX_LINES, MAX_LINES_BRIEFING, NOTDEF, SCALE, SLOT_PX, speech_node};
    use hytte_plugin::display::{RenderMode, testing::with_render_mode};
    use hytte_plugin::preem::{self, font};
    use hytte_plugin::proto::{Node, preem::PreemWidget};

    /// The ordinary bubble's `Pixels` parts, from a session that has **not**
    /// negotiated the preem vocabulary — the arm these assertions are about.
    /// Stated rather than left to the default, so none of them can start passing
    /// for the wrong reason if anything in this binary raises `NEGOTIATED`.
    fn pixels(line: &str) -> (u32, u32, Vec<u8>) {
        raster_pixels(with_render_mode(RenderMode::Raster, || {
            speech_node(line, "caw-say", vec!["caw-say".to_owned()])
        }))
    }

    /// The `Pixels` parts of a node that must be one.
    fn raster_pixels(node: Node) -> (u32, u32, Vec<u8>) {
        match node {
            Node::Pixels {
                width,
                height,
                data,
                ..
            } => (width, height, data),
            other => panic!("speech is a Pixels node, got {other:?}"),
        }
    }

    /// The briefing box's `Pixels` parts, same arm.
    fn briefing_pixels(line: &str) -> (u32, u32, Vec<u8>) {
        raster_pixels(with_render_mode(RenderMode::Raster, || {
            super::briefing_node(line, "caw-say", vec![])
        }))
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
        let rows = MAX_LINES as usize;
        let max_h = 2 * 3 + rows * font::GLYPH_H + (rows - 1) * font::LINE_GAP;
        // ×2 scale (see SCALE), with a little slack for padding rounding.
        assert!(
            (tall_h as usize) <= (max_h * 2) + 8,
            "height {tall_h} is capped at {MAX_LINES} rows"
        );
        let (wide_w, ..) = pixels(&"a".repeat(200));
        assert!(wide_w <= 256, "slot width {wide_w} fits beneath the face");
    }

    /// The briefing box (#407) holds more rows than the ordinary bubble but
    /// keeps the same slot width and the host's buffer invariant.
    #[test]
    fn briefing_box_is_taller_but_still_bounded() {
        let news = "morning, meat-computer. 3° rain, high 8°. S9 to Spandau in 12 — move, choom.";
        let (w, h, data) = briefing_pixels(news);
        assert_eq!(data.len(), w as usize * h as usize * 4);
        assert!(w <= 256, "slot width {w} fits beneath the face");
        // The same text through the 4-line bubble is shorter (truncated) than
        // through the briefing box — the extra rows are real.
        let (_, small_h, _) = pixels(news);
        assert!(
            h > small_h,
            "briefing box {h} outgrows the bubble {small_h}"
        );
        // And the briefing cap still bounds a pathological input.
        let rows = MAX_LINES_BRIEFING as usize;
        let max_h = 2 * 3 + rows * font::GLYPH_H + (rows - 1) * font::LINE_GAP;
        let (_, tall_h, _) = briefing_pixels(&"caw ".repeat(400));
        assert!(
            (tall_h as usize) <= (max_h * 2) + 8,
            "height {tall_h} is capped at {MAX_LINES_BRIEFING} rows",
        );
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

    // ── #884/#885: the seam, and the compat promise it has to keep ──────────

    /// Against a shell that does not speak preem, **both** boxes are
    /// byte-for-byte the nodes the hand-written kit calls produced before the
    /// seam — id, classes and every RGBA byte.
    ///
    /// The briefing variant is covered explicitly rather than assumed from the
    /// ordinary one: it is the same builder with a different `max_lines`, which
    /// is exactly the kind of "surely it follows" that
    /// [`speech`](super::speech)'s two call sites could stop sharing.
    ///
    /// The oracle is the pre-#884 line from this file's own history, written out
    /// rather than derived from `speech()` — an oracle built from the code under
    /// test agrees with it by construction.
    ///
    /// **Falsified** three ways, one per pin: dropping `.field(FIELD)`,
    /// `.ink(INK)` or `.notdef(NOTDEF)` each turns this red on its own (the
    /// emoji case is what reaches the third).
    #[test]
    fn the_raster_boxes_are_byte_identical_to_the_pre_seam_kit_calls() {
        let scale = SCALE as usize;
        let slot = SLOT_PX as usize;
        let by_hand = |max_lines: u32, line: &str, classes: Vec<String>| {
            preem::TextBox::new()
                .fit_px(slot)
                .max_lines(max_lines as usize)
                .scale(scale)
                .colors(FIELD, INK, NOTDEF)
                .render(line)
                .into_node(Some("caw-say"), classes)
        };
        for line in [
            "",
            "Rogue DHCP mode engaged",
            "💕 unmapped: ☺ \u{1F63A}",
            "smörgåsbord ölçäÜ é ßÅÄÖ",
            "caw?! you dare poke a rogue DHCP server, meat-computer",
        ] {
            let migrated = with_render_mode(RenderMode::Raster, || {
                speech_node(line, "caw-say", vec!["caw-say".to_owned()])
            });
            assert!(
                migrated == by_hand(MAX_LINES, line, vec!["caw-say".to_owned()]),
                "raster speech for {line:?} must match the pre-seam kit call byte for byte",
            );

            let migrated = with_render_mode(RenderMode::Raster, || {
                super::briefing_node(line, "caw-say", vec![])
            });
            assert!(
                migrated == by_hand(MAX_LINES_BRIEFING, line, vec![]),
                "raster briefing for {line:?} must match the pre-seam kit call byte for byte",
            );
        }
    }

    /// Against a shell that *does* speak preem, both boxes ship typed state: the
    /// utterance as text, caw's three colors as pins, the skin as a name, and
    /// the only difference between them the row cap.
    #[test]
    fn against_a_preem_shell_both_boxes_are_state_nodes() {
        let state = |node: Node, want_id: &str| match node {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(want_id), "the reconciler's key");
                match *widget {
                    PreemWidget::TextBox { config, state } => (config, state),
                    other => panic!("caw's box is a TextBox, got {other:?}"),
                }
            }
            other => panic!("a preem-speaking host must get a state node, got {other:?}"),
        };

        let (config, text) = state(
            with_render_mode(RenderMode::State, || {
                speech_node("Rogue DHCP mode engaged", "caw-say", vec![])
            }),
            "caw-say",
        );
        assert_eq!(text.text, "Rogue DHCP mode engaged", "the utterance");
        assert_eq!(
            config.style.style,
            super::STYLE,
            "the skin travels as a name, never as colors",
        );
        assert_eq!(config.style.field, Some(FIELD), "the violet field");
        assert_eq!(config.style.ink, Some(INK), "the lilac-white ink");
        assert_eq!(config.notdef, Some(NOTDEF), "the dim-violet notdef box");
        assert_eq!(config.max_lines, MAX_LINES, "the ordinary row cap");
        assert_eq!(config.scale, SCALE, "the chunky-pixel upscale");
        assert!(!config.fixed_width, "she hugs, so a short caw stays a chip");

        let (briefing, _) = state(
            with_render_mode(RenderMode::State, || {
                super::briefing_node("morning, meat-computer.", "caw-say", vec![])
            }),
            "caw-say",
        );
        assert_eq!(
            briefing.max_lines, MAX_LINES_BRIEFING,
            "the briefing gets the taller box (#407)",
        );
        assert_eq!(
            (briefing.style, briefing.notdef, briefing.scale),
            (config.style, config.notdef, config.scale),
            "…and is otherwise the same box",
        );
    }
}
