//! The `DotMatrix` GL program (#1144): the pipeline declaration, the layer
//! splice, the glyph strip and the pure `(config, glyphs, palette) →
//! GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program) and [`gauge`](super::gauge) in every
//! structural way — it references nothing above itself either, so the parity
//! harness (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this**
//! file and measures the shell's own pipeline and the shell's own uniform
//! mapping against the CPU kit, rather than a copy that would agree with
//! itself.
//!
//! # What is better than the kit
//!
//! The CPU kit stamps each set font pixel as a `dot_px`×`dot_px` block read out
//! of a fixed integer table (`Dots::falloff`), so a dot **is a replicated font
//! pixel**. `GlSurface::measure` deliberately requests a minimum of `0` on both
//! axes "so CSS/layout can scale the surface above its grid size, which is the
//! whole LCD look" — and a chip given more room than its natural size gets that
//! table magnified by GSK's nearest-neighbour scaling, i.e. visible squares.
//!
//! So this arm does not sample its lit layer back out of a grid-sized texture
//! the way the gauge's blit does. It evaluates the kit's own falloff **law** at
//! the fragment's continuous position on the lattice, and the offscreen texture
//! exists only to be blurred into the halo. A stretched chip therefore draws
//! round dots at the screen's resolution instead of a magnified 4×4 block. See
//! `dot_matrix.frag`'s header.
//!
//! At the natural size — what the reconciler requests and what the parity
//! harness measures — the shader snaps that position to the pixel centre
//! (`u_viewport == u_grid`), where the law's arguments are exactly the integers
//! `Dots::new` feeds `intensity`, so the float path *is* the integer path and
//! the two arms draw the same picture.
//!
//! There is no `scale` on this widget and nothing here multiplies one: the dot
//! pitch already *is* the size knob (#1091), so the kit's buffer is native to
//! begin with and the #1090 story the gauge tells has no counterpart. What is
//! resolution-dependent about a dot matrix is the **dot**, and that is what
//! moved.
//!
//! # Almost nothing is mirrored
//!
//! Unlike [`gauge`](super::gauge), whose `Dial` is a hand copy of private kit
//! constants, this module reads the geometry out of `hytte-preem` itself: the
//! pitch clamp is [`kit::MIN_DOT_PX`]`..=`[`kit::MAX_DOT_PX`], the cell is
//! [`kit::font::GLYPH_W`]×[`kit::font::GLYPH_H`] with
//! [`kit::font::SPACING`] between cells, and the glyph bitmaps are
//! [`kit::font::glyph`]'s own (with its own [`kit::font::NOTDEF`] fallback).
//! The bezel, the advance and the buffer size are then one line each.
//!
//! The **one** copy is the falloff law, and it has to be one: `intensity` is
//! private in the kit and a fragment shader cannot call a Rust function
//! whatever its visibility, so `dot_matrix.frag` carries the law's four knot
//! constants. Three tests hold that copy in place, as a chain:
//!
//! 1. [`kit::dot_cell`] — an **additive** `pub` accessor #1144 added to
//!    `hytte-preem` (plain data, nothing in the kit reads it, no render path
//!    changed) publishing the very table `ghost_dot`/`lit_dot` stamp;
//! 2. [`tests::the_falloff_law_is_the_kits_own_published_cell`] holds a Rust
//!    mirror of the law to that table, at every pitch and every pixel — which
//!    is exactly where the shader samples it at 1:1;
//! 3. [`tests::the_shader_and_the_mapping_agree_about_the_falloff_knots`] reads
//!    the constants back out of the GLSL, so the mirror and the shader cannot
//!    drift.
//!
//! [`tests::the_falloff_law_is_the_kits_own_ghost_dot`] then measures the
//! *geometry* — where those dots land — against the kit's rendered bytes.
//!
//! # The pipeline
//!
//! One render is four passes, and **no step passes at all** — a dot matrix
//! carries no cross-frame GPU state whatsoever (its only state is the text,
//! which arrives as a strip).
//!
//! 1. **lit** — one falloff dot per set font pixel, into an R8 aux texture;
//! 2. **blur H** and 3. **blur V** — the kit's separable truncating box blur,
//!    the *same* `blur.frag` the scope and the gauge use;
//! 4. **blit** — the field, the unlit ghost matrix, the lit layer recomputed at
//!    the fragment's own resolution with the halo max-combined under it, the
//!    CRT pass and the composite.
//!
//! The blur runs on a glow-free skin too, for the reason
//! [`program`](super::program) gives: at radius `0` it is the identity and at
//! strength `0` the blit's max-combine is a no-op, so the reflective LCD gets
//! `Emission::bloom`'s early-return bytes out of one pipeline.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

use super::program::{BLUR_H_FRAG, BLUR_V_FRAG, FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `DotMatrix` pipeline.
pub(crate) const DOT_MATRIX: GlProgram = GlProgram("preem.dot_matrix");

/// The two layers of the display, which are the same body with the layer
/// prepended.
///
/// The `blur.frag` trick, for the same reason and with the same payoff as
/// `gauge.frag`'s: `GlUniforms` is one bag applied to every pass, so there is
/// nowhere to say "this pass is the lit one" — and both layers need `falloff`
/// and `site_at`. A hand-copied second dot falloff would drift, and a drift
/// between *these* two draws a halo that does not sit on the dots it came from.
const LIT_FRAG: &str = concat!("const int LAYER = 0;\n", include_str!("dot_matrix.frag"));
const BLIT_FRAG: &str = concat!("const int LAYER = 1;\n", include_str!("dot_matrix.frag"));

/// The `DotMatrix` pipeline. See the module docs for what each pass is.
pub(crate) const DOT_MATRIX_PIPELINE: GlPipeline = GlPipeline {
    // The lit layer into 0, then the blur's two halves into 1 and 2.
    aux: 3,
    // No phosphor, no accumulator, no decay — and unlike the gauge, not even a
    // CPU-side spring. `step_seq` stays at `0` and this list stays empty.
    step: &[],
    frame: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: LIT_FRAG,
            target: GlTarget::Aux(0),
            // `u_tex0` is the glyph strip in **both** spliced layers, which is
            // why it is declared first in the blit's list too.
            inputs: &[GlInput::Data],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_H_FRAG,
            target: GlTarget::Aux(1),
            inputs: &[GlInput::Aux(0)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_V_FRAG,
            target: GlTarget::Aux(2),
            inputs: &[GlInput::Aux(1)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLIT_FRAG,
            target: GlTarget::Screen,
            // The lit layer is **recomputed**, not read back — see the module
            // docs. Only the blurred copy is sampled.
            inputs: &[GlInput::Data, GlInput::Aux(2)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
    ],
};

/// One line of text as the shader consumes it: how many character cells the
/// display has, and the glyph bits for them.
///
/// Built once per text change and shared by every monitor's mapping pass, the
/// way `Renderer::ScopeGl`'s `samples` is (#911's rule, for uniforms): the
/// `Arc` makes a repeat mapping's dedup a pointer compare rather than a
/// re-encode of the whole line.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Glyphs {
    /// Character cells on the display — `text.chars().count()`, which is what
    /// the kit's own width formula counts.
    pub(crate) cells: usize,
    /// One texel per glyph **column**, `cells * GLYPH_W` of them, each holding
    /// that column's [`kit::font::GLYPH_H`] row bits with bit `row` set for a
    /// lit font pixel. `None` for an empty display, which is what
    /// `GlUniforms::data` wants there (it binds a 1×1 zero texture and sets
    /// `u_data_len` to `0`).
    ///
    /// A column rather than a whole cell because a cell is 35 bits and an
    /// `f32` carries 24 exactly; seven bits is exact, and the strip is
    /// `5 × chars` texels — under a thousand for any display the wire's own
    /// `clamp_strip_text` budget allows at the default pitch.
    pub(crate) strip: Option<Arc<[f32]>>,
}

/// Encode `text` for the shader — the pure half of the mapping.
///
/// Goes through [`kit::font::glyph`], so an uncovered char becomes the kit's
/// own hollow [`kit::font::NOTDEF`] box here rather than a second fallback
/// policy in the shader.
pub(crate) fn glyphs(text: &str) -> Glyphs {
    let cells = text.chars().count();
    if cells == 0 {
        return Glyphs { cells, strip: None };
    }
    let mut strip = Vec::with_capacity(cells * kit::font::GLYPH_W);
    for ch in text.chars() {
        let rows = kit::font::glyph(ch).unwrap_or(&kit::font::NOTDEF);
        for col in 0..kit::font::GLYPH_W {
            let mut bits = 0u32;
            for (row, &pixels) in rows.iter().enumerate() {
                // The kit reads a glyph row MSB-first across GLYPH_W; the strip
                // is transposed to one texel per column, bit `row`.
                if (pixels >> (kit::font::GLYPH_W - 1 - col)) & 1 == 1 {
                    bits |= 1 << row;
                }
            }
            strip.push(f32::from(u16::try_from(bits).unwrap_or(0)));
        }
    }
    Glyphs {
        cells,
        strip: Some(Arc::from(&strip[..])),
    }
}

/// The dot pitch the kit would render at: the wire's value under
/// `Dots::new`'s clamp.
pub(crate) fn pitch(dot_px: u32) -> usize {
    usize_of(dot_px).clamp(kit::MIN_DOT_PX, kit::MAX_DOT_PX)
}

/// The buffer `DotMatrix::render` produces for `cells` characters at `dot`
/// pitch — `Dots::height` and the width formula, read off the kit.
///
/// Kept beside the mapping (rather than inlined into it) because the node's
/// natural size and the offscreen grid are the same number for this widget and
/// saying so once is what makes that a statement rather than a coincidence.
fn buffer(cells: usize, dot: usize) -> (usize, usize) {
    let pad = dot; // `Dots::pad`: the bezel **is** one dot cell on every side.
    let advance = (kit::font::GLYPH_W + kit::font::SPACING) * dot;
    let width = if cells == 0 {
        2 * pad
    } else {
        2 * pad + cells * advance - kit::font::SPACING * dot
    };
    (width, 2 * pad + kit::font::GLYPH_H * dot)
}

/// Map one `DotMatrix`'s already-clamped config and its encoded line onto the
/// GL node payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface) and
/// [`gauge_surface`](super::gauge::gauge_surface) are: it reads no globals,
/// resolves no palette and touches no GL. The caller passes the palette it has
/// already resolved *inside the widget's `with_pins` scope*, so accent / role /
/// pin precedence stays the kit's one implementation.
pub(crate) fn dot_matrix_surface(
    config: vocab::DotMatrixConfig,
    glyphs: &Glyphs,
    palette: &kit::PaletteSnapshot,
) -> KitSurface {
    let dot = pitch(config.dot_px);
    let (width, height) = buffer(glyphs.cells, dot);

    // A skin with no bloom reaches the shaders as radius 0 / strength 0, which
    // makes the blur the identity and the blit's max-combine a no-op — see
    // `program`'s module docs on why the passes run unconditionally. Unlike the
    // gauge there is no halving and no cap: `DotMatrix::render` hands
    // `Emission::bloom` the skin's own `Bloom`, untouched.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;
    // **The comb is re-phased onto this surface's dot grid** — `Mask::with_pitch`
    // (#1091), which `DotMatrix::render` applies at its own `composite` call
    // site. One dark line in the seam below each dot row is what the pass has
    // always meant, and a fixed 4-row comb over a grid of any other pitch stops
    // being a raster and becomes interference. `phase` is `pitch - 1`, the
    // cell's last sub-row, and `dot` is never zero (the clamp above).
    let mask_pitch = mask.map_or(0, |_| dot);
    let mask_phase = mask.map_or(0, |_| dot - 1);

    KitSurface {
        width: u32_of(width),
        height: u32_of(height),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                ("u_dot", GlValue::Int(int_of(dot))),
                ("u_cells", GlValue::Int(int_of(glyphs.cells))),
                ("u_ghost_on", GlValue::Int(i32::from(palette.ghost.is_some()))),
                (
                    "u_ghost",
                    channels(palette.ghost.unwrap_or([0, 0, 0, 0xff])),
                ),
                ("u_bloom_radius", GlValue::Int(int_of(bloom.radius))),
                ("u_bloom_strength", GlValue::Int(i32::from(bloom.strength))),
                ("u_bg", channels(palette.bg)),
                ("u_ink", channels(palette.ink)),
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                ("u_mask_pitch", GlValue::Int(int_of(mask_pitch))),
                ("u_mask_phase", GlValue::Int(int_of(mask_phase))),
                (
                    "u_scanline_keep",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.scanline_keep).unwrap_or(0))),
                ),
                (
                    "u_corner_keep",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.corner_keep).unwrap_or(0))),
                ),
            ],
            // The glyph grid. Shared rather than rebuilt per mapping pass —
            // see [`Glyphs`].
            data: glyphs.strip.clone(),
            // The buffer the kit would have produced, which for this widget is
            // already the native one: the dot pitch is the size knob, so there
            // is no `scale` to multiply (#1091).
            grid: (u32_of(width), u32_of(height)),
            // No step passes, so nothing counts steps. See `DOT_MATRIX_PIPELINE`.
            step_seq: 0,
        },
    }
}

/// A wire `u32` as a `usize`, saturating.
fn usize_of(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A buffer dimension as a `u32`, saturating. Every value reaching this is
/// bounded by the wire's `MAX_STRIP_DIM`, far below `u32::MAX`.
fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A count as the `int` a uniform carries, saturating.
fn int_of(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        BLIT_FRAG, DOT_MATRIX_PIPELINE, GlBlend, GlDraw, GlInput, GlTarget, GlUniforms, GlValue,
        LIT_FRAG, buffer, dot_matrix_surface, glyphs, kit, pitch, vocab,
    };

    /// The shader body, without either splice — what both layers share.
    const BODY: &str = include_str!("dot_matrix.frag");

    /// The four knot constants of the falloff law, as a Rust mirror of what
    /// `dot_matrix.frag` carries and of what `hytte-preem`'s private
    /// `intensity` computes.
    const SEG1_BASE: usize = 795;
    const SEG1_SLOPE: usize = 1080;
    const SEG2_BASE: usize = 955;
    const SEG2_SLOPE: usize = 760;

    /// `round(num / denom)`, half away from zero — the kit's `round_div`.
    fn round_div(num: usize, denom: usize) -> u16 {
        u16::try_from((2 * num + denom) / (2 * denom)).unwrap_or(255)
    }

    /// The falloff law, from the *doubled* offsets from the dot centre. This is
    /// the Rust mirror the tests below hold against the kit's rendered bytes on
    /// one side and against the GLSL's constants on the other.
    fn falloff(qx: i32, qy: i32, dot: usize) -> u16 {
        let denom = dot * dot;
        let num = usize::try_from(qx * qx + qy * qy).unwrap_or(0);
        if 2 * num <= denom {
            return 255;
        }
        if 8 * num <= 5 * denom {
            return round_div(SEG1_BASE * denom - SEG1_SLOPE * num, denom);
        }
        let quarters = SEG2_BASE * denom;
        let taken = SEG2_SLOPE * num;
        if quarters <= taken {
            return 0;
        }
        round_div(quarters - taken, 4 * denom)
    }

    /// `hytte-preem/src/style.rs`'s `mix`, for the oracle below.
    fn mix(a: kit::Rgba, b: kit::Rgba, t: u16) -> kit::Rgba {
        let t = u32::from(t.min(255));
        let mut out = [0u8; 4];
        for (o, (&av, &bv)) in out.iter_mut().zip(a.iter().zip(&b)) {
            let v = (u32::from(av) * (255 - t) + u32::from(bv) * t + 127) / 255;
            *o = u8::try_from(v).unwrap_or(u8::MAX);
        }
        out
    }

    fn config(style: vocab::StyleName, dot_px: u32) -> vocab::DotMatrixConfig {
        vocab::DotMatrixConfig {
            style: vocab::StyleRef::new(style),
            dot_px,
        }
    }

    /// **The law is the kit's own**, evaluated against
    /// [`kit::dot_cell`] — the additive accessor #1144 added to `hytte-preem`
    /// for exactly this, rather than copying the private table over here.
    ///
    /// At a pixel centre the shader's `q` values are the integers the kit's
    /// `radius_sq` squares, so the continuous law has to return the kit's
    /// discrete table there, at every pitch and every pixel of the cell. That
    /// is the whole parity claim for this widget in one assertion.
    ///
    /// **Falsified** by nudging any of the four knot constants, by moving the
    /// plateau off `s ≤ 1/2`, or by dropping the `+ denom` from `round_div`.
    #[test]
    fn the_falloff_law_is_the_kits_own_published_cell() {
        for dot in kit::MIN_DOT_PX..=kit::MAX_DOT_PX {
            let cell = kit::dot_cell(dot);
            assert_eq!(cell.pitch(), dot, "the premise: no clamping in this range");
            for j in 0..dot {
                for i in 0..dot {
                    assert_eq!(
                        falloff(doubled(i, dot), doubled(j, dot), dot),
                        cell.at(i, j),
                        "pitch {dot}, cell ({i},{j})",
                    );
                }
            }
        }
    }

    /// **…and it lands where the kit lands it**, measured against the kit's own
    /// rendered bytes at every pitch the wire can carry.
    ///
    /// The law above says what a dot looks like; this says where the dots *are*
    /// — the bezel, the advance, the cell grid, and that nothing is painted in
    /// the spacing column or the margins. The oracle is not a copied table: a
    /// ghosting skin paints `mix(bg, ghost, falloff)` at **every** dot position
    /// of every cell before anything is lit, so a one-space display's rendered
    /// frame *is* the lattice, in colour. Rendering a space also keeps the
    /// emission empty, so no bloom and no CRT comb can reach the pixels
    /// compared here — the frame is the ghost pass and the field, and nothing
    /// else.
    ///
    /// This is the geometry half of what `site_at` has to reproduce, and it is
    /// the reason the shader's `pad`/`advance`/spacing arithmetic is written
    /// the way it is.
    ///
    /// **Falsified** by moving the bezel, by dropping the spacing column's
    /// exclusion, or by an off-by-one in either axis of `buffer`.
    #[test]
    fn the_falloff_law_is_the_kits_own_ghost_dot() {
        for style in [kit::DisplayStyle::Vfd, kit::DisplayStyle::Lcd] {
            let palette = kit::palette_snapshot(style);
            let ghost = palette.ghost.expect("a ghosting skin, by construction");
            for dot in kit::MIN_DOT_PX..=kit::MAX_DOT_PX {
                let frame = kit::DotMatrix::new(style).dot_px(dot).render(" ");
                let (width, height) = buffer(1, dot);
                assert_eq!(
                    (frame.width(), frame.height()),
                    (width, height),
                    "the buffer formula is the kit's at pitch {dot}",
                );
                for y in 0..height {
                    for x in 0..width {
                        // Where the one cell's dot grid puts this pixel.
                        let inside = x >= dot
                            && y >= dot
                            && x < dot + kit::font::GLYPH_W * dot
                            && y < dot + kit::font::GLYPH_H * dot;
                        // The kit's own published intensity, not this file's
                        // mirror of the law — the two are tied together by
                        // `the_falloff_law_is_the_kits_own_published_cell`, and
                        // keeping them apart here is what makes this test about
                        // *placement* alone.
                        let want = if inside {
                            let (i, j) = ((x - dot) % dot, (y - dot) % dot);
                            mix(palette.bg, ghost, kit::dot_cell(dot).at(i, j))
                        } else {
                            palette.bg
                        };
                        let got = pixel(&frame, x, y);
                        assert_eq!(
                            got,
                            want,
                            "{}, pitch {dot}, pixel ({x},{y})",
                            style.name(),
                        );
                    }
                }
            }
        }
    }

    /// `2*k + 1 - dot`, the doubled offset the shader's `site_at` produces at a
    /// pixel centre.
    fn doubled(k: usize, dot: usize) -> i32 {
        i32::try_from(2 * k + 1).unwrap_or(0) - i32::try_from(dot).unwrap_or(0)
    }

    fn pixel(frame: &kit::Frame, x: usize, y: usize) -> kit::Rgba {
        let at = (y * frame.width() + x) * 4;
        let bytes = &frame.data()[at..at + 4];
        [bytes[0], bytes[1], bytes[2], bytes[3]]
    }

    /// The **shader** carries the same four knots this file's mirror does.
    ///
    /// The mirror is measured against the kit above; this is the other half of
    /// the chain, and it is a source read rather than an assertion about a
    /// value, because nothing in the tree compiles the GLSL until a driver
    /// does. `nix/lint-glsl.py` proves it *parses*; this proves it says the
    /// same thing.
    ///
    /// **Falsified** by changing any knot on either side alone.
    #[test]
    fn the_shader_and_the_mapping_agree_about_the_falloff_knots() {
        for (name, value) in [
            ("SEG1_BASE", SEG1_BASE),
            ("SEG1_SLOPE", SEG1_SLOPE),
            ("SEG2_BASE", SEG2_BASE),
            ("SEG2_SLOPE", SEG2_SLOPE),
        ] {
            let wanted = format!("const float {name} = {value}.0;");
            assert!(
                BODY.contains(&wanted),
                "dot_matrix.frag must declare `{wanted}`",
            );
        }
    }

    /// …and the same for the font metrics, which this side reads out of
    /// `hytte-preem` while the shader must state them as constants.
    ///
    /// **Falsified** by editing either `GLYPH_W`, `GLYPH_H` or `SPACING` in the
    /// shader without the kit moving under it.
    #[test]
    fn the_shader_and_the_mapping_agree_about_the_font_metrics() {
        for (name, value) in [
            ("GLYPH_W", kit::font::GLYPH_W),
            ("GLYPH_H", kit::font::GLYPH_H),
            ("SPACING", kit::font::SPACING),
        ] {
            let wanted = format!("const int {name} = {value};");
            assert!(
                BODY.contains(&wanted),
                "dot_matrix.frag must declare `{wanted}`",
            );
        }
    }

    /// The two layers are the **same body** with one line in front of it.
    ///
    /// **Falsified** by turning either into its own `include_str!`.
    #[test]
    fn the_two_layers_are_one_body_with_the_layer_spliced_in() {
        assert!(LIT_FRAG.starts_with("const int LAYER = 0;"));
        assert!(BLIT_FRAG.starts_with("const int LAYER = 1;"));
        assert_eq!(
            LIT_FRAG.strip_prefix("const int LAYER = 0;\n"),
            BLIT_FRAG.strip_prefix("const int LAYER = 1;\n"),
            "one body, two splices",
        );
    }

    /// The strip is the kit's own font, transposed to one texel per column.
    ///
    /// **Falsified** by reversing the bit order on either axis, or by dropping
    /// the `NOTDEF` fallback (the `?` case below then reads as blank).
    #[test]
    fn the_glyph_strip_is_the_kits_own_font() {
        let text = "A\u{1f4a9}";
        let encoded = glyphs(text);
        assert_eq!(encoded.cells, 2, "chars, not bytes");
        let strip = encoded.strip.expect("a non-empty display carries a strip");
        assert_eq!(strip.len(), 2 * kit::font::GLYPH_W);

        for (cell, ch) in text.chars().enumerate() {
            let rows = kit::font::glyph(ch).unwrap_or(&kit::font::NOTDEF);
            for col in 0..kit::font::GLYPH_W {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let bits = strip[cell * kit::font::GLYPH_W + col] as u32;
                for (row, &pixels) in rows.iter().enumerate() {
                    let kit_lit = (pixels >> (kit::font::GLYPH_W - 1 - col)) & 1 == 1;
                    let strip_lit = (bits >> row) & 1 == 1;
                    assert_eq!(strip_lit, kit_lit, "cell {cell}, col {col}, row {row}");
                }
            }
        }
        // The uncovered char took the kit's hollow box rather than a blank.
        let notdef: u32 = (0..kit::font::GLYPH_W)
            .map(|col| {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let bits = strip[kit::font::GLYPH_W + col] as u32;
                bits
            })
            .sum();
        assert!(notdef > 0, "an uncovered char draws NOTDEF, not nothing");
    }

    /// An empty display carries **no** strip at all — which is what
    /// `GlUniforms::data`'s `None` means (a 1×1 zero texture, `u_data_len = 0`),
    /// rather than a zero-length allocation the driver would have to refuse.
    ///
    /// **Falsified** by returning `Some(Arc::from(&[][..]))` for the empty
    /// string.
    #[test]
    fn an_empty_display_carries_no_strip_and_only_its_bezel() {
        let encoded = glyphs("");
        assert_eq!(encoded.cells, 0);
        assert!(encoded.strip.is_none());

        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        let surface = dot_matrix_surface(config(vocab::StyleName::Vfd, 4), &encoded, &palette);
        assert!(surface.uniforms.data.is_none());
        // `2 * pad` wide, `9 * dot` tall — the kit's own degenerate case.
        assert_eq!((surface.width, surface.height), (8, 36));
        assert_eq!(
            (surface.width, surface.height),
            size_of_kit_frame(kit::DisplayStyle::Vfd, 4, ""),
            "the natural size is the kit's frame, to the pixel",
        );
    }

    fn size_of_kit_frame(style: kit::DisplayStyle, dot_px: usize, text: &str) -> (u32, u32) {
        let frame = kit::DotMatrix::new(style).dot_px(dot_px).render(text);
        (
            u32::try_from(frame.width()).unwrap_or(0),
            u32::try_from(frame.height()).unwrap_or(0),
        )
    }

    /// The node's natural size is the kit's frame at every pitch and every
    /// length — including the clamped pitches, where the wire's value and the
    /// kit's differ.
    ///
    /// This is the layout contract: a kill-switch flip is a node-kind change,
    /// so it rebuilds the widget, and a rebuild that also resized would reflow
    /// the whole card.
    ///
    /// **Falsified** by dropping the `- SPACING * dot` from `buffer`, or by
    /// dropping the clamp from `pitch`.
    #[test]
    fn the_natural_size_is_the_kits_frame_at_every_pitch() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        for dot_px in 0..=12u32 {
            for text in ["", " ", "PREEM", "0123456789~/"] {
                let surface = dot_matrix_surface(
                    config(vocab::StyleName::Vfd, dot_px),
                    &glyphs(text),
                    &palette,
                );
                assert_eq!(
                    (surface.width, surface.height),
                    size_of_kit_frame(kit::DisplayStyle::Vfd, pitch(dot_px), text),
                    "pitch {dot_px}, text {text:?}",
                );
                assert_eq!(
                    surface.uniforms.grid,
                    (surface.width, surface.height),
                    "the offscreen grid **is** the native buffer: no upscale exists",
                );
            }
        }
    }

    /// The pitch clamp is the kit's, both ends.
    ///
    /// **Falsified** by widening either bound.
    #[test]
    fn the_pitch_is_clamped_the_way_the_kit_clamps_it() {
        assert_eq!(pitch(0), kit::MIN_DOT_PX);
        assert_eq!(pitch(1), kit::MIN_DOT_PX);
        assert_eq!(pitch(4), 4);
        assert_eq!(pitch(9), kit::MAX_DOT_PX);
        assert_eq!(pitch(u32::MAX), kit::MAX_DOT_PX);
    }

    /// **The CRT comb is re-phased onto the dot grid** (#1091's
    /// `Mask::with_pitch`), and only on a skin that has a mask at all.
    ///
    /// A fixed 4-row comb over a 3 px or 2 px grid is interference rather than
    /// a raster — it lands on the dot *core* of some glyph rows and misses
    /// others — which is exactly what the kit re-phases to avoid, and what this
    /// mapping has to carry over to the shader because the shader takes the
    /// pitch as a uniform.
    ///
    /// **Falsified** by passing `m.pitch`/`m.phase` through from the snapshot
    /// instead of the dot pitch.
    #[test]
    fn the_crt_comb_is_re_phased_onto_the_dot_grid() {
        for dot_px in kit::MIN_DOT_PX..=kit::MAX_DOT_PX {
            let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
            let surface = dot_matrix_surface(
                config(vocab::StyleName::Crt, u32::try_from(dot_px).unwrap_or(4)),
                &glyphs("8"),
                &palette,
            );
            assert_eq!(uniform(&surface.uniforms, "u_mask_on"), GlValue::Int(1));
            assert_eq!(
                uniform(&surface.uniforms, "u_mask_pitch"),
                GlValue::Int(i32::try_from(dot_px).unwrap_or(0)),
                "one dark line per dot row, at pitch {dot_px}",
            );
            assert_eq!(
                uniform(&surface.uniforms, "u_mask_phase"),
                GlValue::Int(i32::try_from(dot_px - 1).unwrap_or(0)),
                "…in the seam below it",
            );
        }
        // …and a skin with no tube reaches the shader switched off, with no
        // stray pitch that a future `u_mask_on`-less branch could act on.
        let surface = dot_matrix_surface(
            config(vocab::StyleName::Vfd, 4),
            &glyphs("8"),
            &kit::palette_snapshot(kit::DisplayStyle::Vfd),
        );
        assert_eq!(uniform(&surface.uniforms, "u_mask_on"), GlValue::Int(0));
        assert_eq!(uniform(&surface.uniforms, "u_mask_pitch"), GlValue::Int(0));
    }

    /// The skin's own halo reaches the shader **untouched** — no halving and no
    /// cap, unlike the gauge's (#930/#931). `DotMatrix::render` hands
    /// `Emission::bloom` the palette's `Bloom` as it stands.
    ///
    /// **Falsified** by dividing the radius, the way `preem_gl::gauge` must.
    #[test]
    fn the_halo_is_the_skins_own() {
        for (style, name) in [
            (kit::DisplayStyle::Vfd, vocab::StyleName::Vfd),
            (kit::DisplayStyle::Lcd, vocab::StyleName::Lcd),
            (kit::DisplayStyle::Oled, vocab::StyleName::Oled),
            (kit::DisplayStyle::Crt, vocab::StyleName::Crt),
        ] {
            let palette = kit::palette_snapshot(style);
            let surface = dot_matrix_surface(config(name, 4), &glyphs("8"), &palette);
            let (radius, strength) = palette
                .bloom
                .map_or((0, 0), |b| (i32::try_from(b.radius).unwrap_or(0), b.strength));
            assert_eq!(
                uniform(&surface.uniforms, "u_bloom_radius"),
                GlValue::Int(radius),
                "{}",
                style.name(),
            );
            assert_eq!(
                uniform(&surface.uniforms, "u_bloom_strength"),
                GlValue::Int(i32::from(strength)),
                "{}",
                style.name(),
            );
        }
    }

    /// The **golden uniform table**: every name, in order, with the values a
    /// known display resolves to.
    ///
    /// The names are the contract with the GLSL — a rename on one side alone
    /// draws nothing and says nothing — and the order is pinned so a reordering
    /// shows up as a diff here rather than silently.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row.
    #[test]
    fn the_uniform_table_is_the_kits_own_numbers() {
        let style = kit::DisplayStyle::Crt;
        let palette = kit::palette_snapshot(style);
        let surface = dot_matrix_surface(config(vocab::StyleName::Crt, 3), &glyphs("42"), &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_dot",
                "u_cells",
                "u_ghost_on",
                "u_ghost",
                "u_bloom_radius",
                "u_bloom_strength",
                "u_bg",
                "u_ink",
                "u_mask_on",
                "u_mask_pitch",
                "u_mask_phase",
                "u_scanline_keep",
                "u_corner_keep",
            ],
        );
        assert_eq!(uniform(&surface.uniforms, "u_dot"), GlValue::Int(3));
        assert_eq!(uniform(&surface.uniforms, "u_cells"), GlValue::Int(2));
        assert_eq!(
            uniform(&surface.uniforms, "u_ghost_on"),
            GlValue::Int(i32::from(palette.ghost.is_some())),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_bg"),
            super::channels(palette.bg),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(palette.ink),
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
        assert_eq!(
            surface.uniforms.data.as_ref().map(|d| d.len()),
            Some(2 * kit::font::GLYPH_W),
            "one texel per glyph column",
        );
    }

    /// **A pinned ink reaches the GL arm** — the mapping composites toward the
    /// palette the caller resolved, never toward the skin's own.
    ///
    /// This is the one property that cannot be checked by looking at the
    /// mapping alone: the pins are a thread-local scope in the kit, and the
    /// shell opens it around `gl_surface`. What this pins is that the mapping
    /// *takes* the resolved palette rather than re-deriving one.
    ///
    /// **Falsified** by calling `kit::palette_snapshot` inside
    /// `dot_matrix_surface` instead of taking the argument.
    #[test]
    fn a_pinned_ink_reaches_the_uniforms() {
        let pinned = kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Fixed([0xff, 0x00, 0x00, 0xff]),
                field: None,
            },
            || kit::palette_snapshot(kit::DisplayStyle::Vfd),
        );
        let surface = dot_matrix_surface(config(vocab::StyleName::Vfd, 4), &glyphs("8"), &pinned);
        assert_eq!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(pinned.ink),
        );
        assert_ne!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(kit::palette_snapshot(kit::DisplayStyle::Vfd).ink),
            "the premise: the pin actually moved the ink",
        );
    }

    /// The pipeline's shape: three aux textures, no step passes, the blur in
    /// the middle, and the screen last.
    ///
    /// **Falsified** by dropping a pass, by pointing the blit at the wrong aux,
    /// or by letting a frame pass target the accumulator (which `hytte-ui`
    /// skips with a `debug_assert`).
    #[test]
    fn the_pipeline_blurs_the_lit_layer_and_ends_on_the_screen() {
        assert!(
            DOT_MATRIX_PIPELINE.step.is_empty(),
            "a dot matrix carries no cross-frame GPU state at all",
        );
        assert_eq!(DOT_MATRIX_PIPELINE.frame.len(), 4, "lit, blur H, blur V, blit");
        assert_eq!(DOT_MATRIX_PIPELINE.frame[0].target, GlTarget::Aux(0));
        assert_eq!(DOT_MATRIX_PIPELINE.frame[0].inputs, &[GlInput::Data]);
        assert_eq!(DOT_MATRIX_PIPELINE.frame[1].inputs, &[GlInput::Aux(0)]);
        assert_eq!(DOT_MATRIX_PIPELINE.frame[2].inputs, &[GlInput::Aux(1)]);
        assert_eq!(DOT_MATRIX_PIPELINE.frame[3].target, GlTarget::Screen);
        assert_eq!(
            DOT_MATRIX_PIPELINE.frame[3].inputs,
            &[GlInput::Data, GlInput::Aux(2)],
            "the lit layer is recomputed at the fragment's resolution; only the \
             blurred copy is sampled",
        );
        for pass in DOT_MATRIX_PIPELINE.frame {
            assert_eq!(pass.blend, GlBlend::Replace);
            assert_eq!(pass.draw, GlDraw::FullScreen);
            assert_ne!(
                pass.target,
                GlTarget::Accumulator,
                "a frame pass may not target the accumulator",
            );
        }
        let declared = usize::from(DOT_MATRIX_PIPELINE.aux);
        for pass in DOT_MATRIX_PIPELINE.frame {
            if let GlTarget::Aux(index) = pass.target {
                assert!(usize::from(index) < declared, "aux {index} is not declared");
            }
            for input in pass.inputs {
                if let GlInput::Aux(index) = input {
                    assert!(usize::from(*index) < declared, "aux {index} is not declared");
                }
            }
        }
    }

    /// Every uniform the shaders read is filled by the mapping.
    ///
    /// The host publishes `u_grid`, `u_viewport`, `u_data_len` and `u_step_back`
    /// itself and binds `u_tex0`/`u_tex1`; everything else has to come from the
    /// table above, and a uniform the GLSL declares but nothing sets reads as
    /// zero — a black display, silently.
    ///
    /// **Falsified** by deleting any row from `dot_matrix_surface`.
    #[test]
    fn the_mapping_fills_every_uniform_the_shaders_read() {
        let surface = dot_matrix_surface(
            config(vocab::StyleName::Crt, 4),
            &glyphs("8"),
            &kit::palette_snapshot(kit::DisplayStyle::Crt),
        );
        let set: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        let host = [
            "u_grid",
            "u_viewport",
            "u_data_len",
            "u_step_back",
            "u_tex0",
            "u_tex1",
        ];
        for line in BODY.lines() {
            let Some(rest) = line.trim().strip_prefix("uniform ") else {
                continue;
            };
            let name = rest
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .trim_end_matches(';');
            assert!(
                host.contains(&name) || set.contains(&name),
                "`{name}` is declared by the shader and set by nobody",
            );
        }
    }

    fn uniform(uniforms: &GlUniforms, name: &str) -> GlValue {
        let (_, value) = uniforms
            .values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("no uniform named {name}"));
        *value
    }
}
