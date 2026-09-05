//! The pet's speech bubble: the SDK's shared **5×7 pixel font**, dressed in
//! the pet's lilac.
//!
//! This module used to *be* the font (#304) — the glyph bitmaps, word-wrap,
//! and renderer were all hand-rolled here. Issue #356 promoted that whole
//! toolkit into [`hytte_plugin::preem`] (`preem::font` + `TextBox`); what
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
//!
//! # On the state path since #884/#885
//!
//! The bubble is a [`display::TextBox`](hytte_plugin::display::TextBox), so
//! against a shell that speaks the preem vocabulary it ships as a typed
//! `Node::Preem` the shell draws, and the wire carries the message rather than
//! a ~30 KB RGBA buffer per change. Against an older shell it rasterises
//! locally and ships the same `Node::Pixels` it always did — byte for byte, as
//! `the_raster_bubble_is_byte_identical_to_the_pre_seam_kit_call` pins.
//!
//! It got here late. #914 had to leave it behind because the seam's wire form
//! could not carry the palette: the bubble sets **three** colors and #912's
//! `StyleRef.ink` pinned one. #885's widening (`field` + `ink` + `notdef`) is
//! what unblocked it, and the three pins below are the whole migration.
//!
//! The **face** stays on the raster escape hatch permanently — it is a
//! hand-drawn `Frame` and the vocabulary has no word for it (`face.rs`).

use hytte_plugin::display::TextBox;
use hytte_plugin::preem::Rgba;
use hytte_plugin::proto::Node;
use hytte_plugin::proto::preem::StyleName;

/// Integer pixel-scale baked into the bubble buffer (#323 — bigger pixel
/// font, multiline).
const SCALE: u32 = 2;

/// Target **on-screen** pixel width of the bubble's slot beside the face in
/// the compact row (#313). The sidebar card is 320 px; inside its ~12 px
/// padding (~296 px) the 128 px LCD face plus its bezel/border eats
/// ~143 px, so this is roughly the width left for the bubble.
const BUBBLE_SLOT_PX: u32 = 126;

/// Hard cap on wrapped lines; an overflow is truncated with a trailing `…`.
const MAX_LINES: u32 = 3;

/// The skin the bubble travels as. **Inert here, and deliberately stated
/// anyway:** a `TextBox` draws exactly three colors and all three are pinned
/// below, so no skin's palette reaches a single pixel of this widget — the
/// raster and state arms render identically whichever name this is. `Lcd` is
/// the honest one to say: the face beside it is an LCD, and if a pin is ever
/// dropped this is the palette that should show through.
const STYLE: StyleName = StyleName::Lcd;

/// Bubble background: a lilac a touch lighter than the face's screen field
/// (`SCREEN_BG` in `face.rs`), opaque. Corners are cut to transparent.
const BUBBLE_BG: Rgba = [0x3a, 0x22, 0x50, 0xff];
/// Text ink: bright lilac, matching the face's fur/highlight family.
const INK: Rgba = [0xf0, 0xe0, 0xf8, 0xff];
/// The `.notdef` box for an uncovered char — a dim lilac outline.
const NOTDEF: Rgba = [0x6c, 0x4e, 0x86, 0xff];

/// The pet's bubble as a [`TextBox`]: a fixed-width slot sized to
/// [`BUBBLE_SLOT_PX`], wrapped and `…`-capped, lilac on lilac.
///
/// The three pins are the whole of #885's palette override, and they are the
/// exact three colors the pre-seam `colors(BUBBLE_BG, INK, NOTDEF)` call set —
/// which is why the raster arm is byte-identical to it.
fn bubble() -> TextBox {
    TextBox::new(STYLE)
        .fit_px(BUBBLE_SLOT_PX)
        .max_lines(MAX_LINES)
        .scale(SCALE)
        .fixed_width(true)
        .field(BUBBLE_BG)
        .ink(INK)
        .notdef(NOTDEF)
}

/// Render `line` into a chunky-pixel speech bubble node, in whichever wire
/// shape this session negotiated.
///
/// Against a preem-speaking shell that is a typed `Node::Preem`; against an
/// older one, the [`Node::Pixels`] it has always been — a **fixed-width** slot
/// (background baked in, transparent rounded corners), upscaled ×[`SCALE`] for
/// the 8-bit look, so it never resizes as the message length changes (#323) and
/// longer text wraps down into more rows instead. The raster buffer satisfies
/// the host's `len == w * h * 4` invariant for every input (including the empty
/// string).
pub(crate) fn bubble_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    bubble().node_classed(id, classes, line)
}

#[cfg(test)]
mod tests {
    use super::{BUBBLE_BG, BUBBLE_SLOT_PX, INK, MAX_LINES, NOTDEF, SCALE, bubble_node};
    use crate::brain::{self, ThinkKind, ThinkReq};
    use hytte_plugin::display::{RenderMode, testing::with_render_mode};
    use hytte_plugin::preem;
    use hytte_plugin::proto::{Node, preem::PreemWidget};

    /// The bubble's `Pixels` parts, from a session that has **not** negotiated
    /// the preem vocabulary — the arm the assertions below are about.
    ///
    /// Stated rather than left to the default: a raster-shaped assertion that
    /// silently depends on `NEGOTIATED` still being 0 would start passing for
    /// the wrong reason the day anything in this binary raises it.
    fn pixels(line: &str) -> (u32, u32, Vec<u8>) {
        match with_render_mode(RenderMode::Raster, || {
            bubble_node(line, "pet-bubble", vec!["pet-bubble".to_owned()])
        }) {
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
            data.chunks_exact(4).any(|px| px == NOTDEF),
            "the notdef box draws in the dim lilac"
        );
    }

    // ── #884/#885: the seam, and the compat promise it has to keep ──────────

    /// Against a shell that does not speak preem, the migrated bubble is
    /// **byte-for-byte** the node the hand-written kit call produced before the
    /// seam — id, classes and every RGBA byte. This is the whole of what the
    /// migration promises to an old shell.
    ///
    /// The oracle is the pre-#884 line from this file's own history
    /// (`TextBox::new().…​.colors(BUBBLE_BG, INK, NOTDEF)`), written out rather
    /// than derived from `bubble()`: an oracle built from the code under test
    /// agrees with it by construction and measures nothing.
    ///
    /// The torture set is deliberate — an emoji reaches the `notdef` slot (the
    /// color the palette scope *cannot* carry, and so the one a wrong wiring
    /// would drop), and a long line reaches the wrap and the `…` cap.
    ///
    /// **Falsified** three ways, one per pin: dropping `.field(BUBBLE_BG)`
    /// (the box floods the Lcd skin's olive), dropping `.ink(INK)` (glyphs come
    /// out in the skin's dark ink, or the session accent), and dropping
    /// `.notdef(NOTDEF)` (the emoji's box comes out in the skin's ghost). Each
    /// mutation turns this red on its own.
    #[test]
    fn the_raster_bubble_is_byte_identical_to_the_pre_seam_kit_call() {
        let scale = SCALE as usize;
        let slot = BUBBLE_SLOT_PX as usize;
        let max_lines = MAX_LINES as usize;
        for line in [
            "mrrp!",
            "",
            "💕 unmapped: ☺",
            "supercalifragilisticexpialidocious",
            "the cat has a great many opinions about this",
        ] {
            let migrated = with_render_mode(RenderMode::Raster, || {
                bubble_node(line, "pet-bubble", vec!["pet-bubble".to_owned()])
            });
            let by_hand = preem::TextBox::new()
                .fit_px(slot)
                .max_lines(max_lines)
                .scale(scale)
                .fixed_width(true)
                .colors(BUBBLE_BG, INK, NOTDEF)
                .render(line)
                .into_node(Some("pet-bubble"), vec!["pet-bubble".to_owned()]);
            assert!(
                migrated == by_hand,
                "raster bubble for {line:?} must match the pre-seam kit call byte for byte",
            );
        }
    }

    /// Against a shell that *does* speak preem, the same call ships typed state:
    /// the message as text, the three colors as pins, the skin as a name, and no
    /// pixels at all.
    ///
    /// The pin assertions are the load-bearing ones — they are what an ink-only
    /// override (#912) could not have carried, and what the shell needs in order
    /// to reproduce the lilac.
    #[test]
    fn against_a_preem_shell_the_bubble_is_a_state_node() {
        let node = with_render_mode(RenderMode::State, || {
            bubble_node("mrrp!", "pet-bubble", vec!["pet-bubble".to_owned()])
        });
        let Node::Preem {
            id,
            classes,
            widget,
        } = node
        else {
            panic!("a preem-speaking host must get a state node, got {node:?}")
        };
        assert_eq!(id.as_deref(), Some("pet-bubble"), "the reconciler's key");
        assert_eq!(
            classes,
            vec!["pet-bubble".to_owned()],
            "classes pass through"
        );
        let PreemWidget::TextBox { config, state } = *widget else {
            panic!("the bubble is a TextBox")
        };
        assert_eq!(state.text, "mrrp!", "the message the shell wraps and draws");
        assert_eq!(
            config.style.style,
            super::STYLE,
            "the skin travels as a name, never as colors",
        );
        assert_eq!(config.style.field, Some(BUBBLE_BG), "the lilac field");
        assert_eq!(config.style.ink, Some(INK), "the bright-lilac ink");
        assert_eq!(config.notdef, Some(NOTDEF), "the dim-lilac notdef box");
        assert_eq!(config.max_lines, MAX_LINES, "the `…` cap");
        assert_eq!(config.scale, SCALE, "the chunky-pixel upscale");
        assert!(config.fixed_width, "the slot never resizes (#323)");
    }
}
