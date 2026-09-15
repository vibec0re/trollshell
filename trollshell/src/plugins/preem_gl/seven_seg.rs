//! The `SevenSeg` GL program (#1154): the pipeline declaration, the layer
//! splice, the cell strip and the pure `(readout, palette) → GlUniforms`
//! mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge),
//! [`dot_matrix`](super::dot_matrix), [`marquee`](super::marquee),
//! [`textbox`](super::textbox) and [`led_strip`](super::led_strip) in every
//! structural way — it references nothing above itself either, so the parity
//! harness (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this**
//! file and measures the shell's own pipeline and mapping against the CPU kit.
//!
//! # What is better than the kit
//!
//! The kit's segments are **tapered hexagons**, drawn as a six-row staircase:
//! `hytte-preem`'s `stamp_bar` insets row `k` of a `THICK`-wide bar from both
//! ends by [`kit::seven_seg_taper`]`(k)`, which for `THICK = 6` is
//! `2, 1, 0, 0, 1, 2`. The shell then blows that buffer up with
//! `PixelSurface`'s nearest-neighbour scaling, so a chip a layout gave more
//! room than its natural size — or any chip at all on a `scale_factor >= 2`
//! screen — gets its mitres as 2×2 blocks of a 1-pixel stair.
//!
//! That staircase is a sampled 45° chamfer, and `seven_seg.frag` draws the
//! chamfer: the segment is the intersection of three half-plane pairs (two long
//! faces, two ends, four mitres), each antialiased against the fragment's own
//! footprint. The diagonals come out as smooth as the screen can draw them
//! instead of a magnified stair, and the **halo** rides the same improvement —
//! read bilinearly at the fragment's resolution off the snap (#1186) rather
//! than replicated out of the kit's grid.
//!
//! At 1:1 the shader snaps to the pixel centre and takes the *point* test,
//! where each of the three residuals is a non-zero half-integer and the answer
//! is exactly `stamp_bar`'s integer range test — see `seven_seg.frag`'s header,
//! and [`tests::the_taper_law_is_the_kits_own_staircase`] for the one law this
//! side copies rather than reads.
//!
//! # Why this pipeline has a blur pass where `led_strip`'s does not
//!
//! #1153's meter has a **closed-form** halo because its emission is a product
//! of two one-dimensional sets — a row of segments across, one band down — so
//! each half of the kit's separable blur is a measure rather than a sum. A
//! digit is a figure-8: which columns are lit depends on the row, so the
//! vertical pass is a genuine sum over rows of a per-row horizontal measure and
//! no closed form exists. This arm therefore takes
//! [`dot_matrix`](super::dot_matrix)'s shape — four passes over three aux
//! textures — which is the general answer and the one every kind with a
//! non-separable emission has to take.
//!
//! # Nothing geometric is a GLSL literal
//!
//! `seven_seg.frag` declares no segment metric of its own. The nine elements a
//! cell can stamp arrive as nine `vec4` uniforms read straight off
//! [`kit::SEVEN_SEG_BARS`] and [`kit::SEVEN_SEG_COLON_DOTS`] (`pub` since
//! #1154, on the [`kit::Dial`]/[`kit::dot_cell`]/[`kit::LED_CELL_W`]
//! precedent), the bar thickness off [`kit::SEVEN_SEG_THICK`], the cell top
//! edge off [`kit::SEVEN_SEG_PAD`], and each cell's left edge off
//! [`kit::seven_seg_layout`]'s own answer, encoded into the strip. The only
//! numbers the shader restates are the CRT pass's four fixed-point constants —
//! which every shader on this seam restates, and which
//! [`program::assert_crt_constants`](super::program::assert_crt_constants)
//! holds to the kit's own items — plus `TAPER_EDGE`, the continuous reading of
//! the taper staircase, which [`tests::the_taper_law_is_the_kits_own_staircase`]
//! holds to [`kit::seven_seg_taper`].
//!
//! # What the issue assumed and the kit does not have
//!
//! #1154's title asks for "each digit's seven segments **plus the decimal
//! point**". The kit has no decimal point: `hytte-preem`'s `seven_seg`
//! understands digits, `:`, `-` and space, and its two cell shapes are a
//! 30 px digit and a 12 px colon. A decimal point would be a new kit cell and a
//! new wire character — a change to `hytte-preem` and a decision for the issue
//! thread, not something a renderer gets to add while claiming parity with the
//! renderer it differs from. So the mixed-readout parity case carries the
//! **colon** instead, which is the kit's own second cell shape and the one
//! thing a single-width case cannot exercise.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_preem as kit;

use super::program::{BLUR_H_FRAG, BLUR_V_FRAG, FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `SevenSeg` pipeline.
pub(crate) const SEVEN_SEG: GlProgram = GlProgram("preem.seven_seg");

/// The two layers of the readout, which are the same body with the layer
/// prepended.
///
/// The `blur.frag` trick, for the same reason and with the same payoff as
/// `dot_matrix.frag`'s: `GlUniforms` is one bag applied to every pass, so there
/// is nowhere to say "this pass is the lit one" — and both layers need
/// `strip255` and everything under it. A hand-copied second segment geometry
/// would drift, and a drift between *these* two draws a halo that does not sit
/// on the segments it came from.
const LIT_FRAG: &str = concat!("const int LAYER = 0;\n", include_str!("seven_seg.frag"));
const BLIT_FRAG: &str = concat!("const int LAYER = 1;\n", include_str!("seven_seg.frag"));

/// The `SevenSeg` pipeline. See the module docs for why it is the dot matrix's
/// four passes and not the LED strip's one.
pub(crate) const SEVEN_SEG_PIPELINE: GlPipeline = GlPipeline {
    // The lit layer into 0, then the blur's two halves into 1 and 2.
    aux: 3,
    // No animation of any kind: a readout's whole state is its text, which
    // arrives as a strip. `step_seq` stays at `0` and this list stays empty.
    step: &[],
    frame: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: LIT_FRAG,
            target: GlTarget::Aux(0),
            // `u_tex0` is the cell strip in **both** spliced layers, which is
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

/// Bit `COLON_BIT` of a strip code: this cell is the colon, whose two dots are
/// not segments and take no mask.
///
/// The **shell's** encoding, not the kit's — a digit mask occupies bits
/// `0..SEVEN_SEG_BARS.len()`, so the next one up is free, the way
/// `textbox.rs`'s `NOTDEF_BIT` sits above the glyph rows. `seven_seg.frag`
/// declares the same number as `COLON_BIT`, and
/// [`tests::the_shader_and_the_mapping_agree_about_the_strip_encoding`] reads
/// it back out of the GLSL.
const COLON_BIT: u8 = 7;

/// One readout as the shader consumes it: its buffer, how many cells it has,
/// and where each one starts and what it lights.
///
/// Built once per text change and shared by every monitor's mapping pass, the
/// way `Renderer::DotMatrixGl`'s `glyphs` is (#911's rule, for uniforms): the
/// `Arc` makes a repeat mapping's dedup a pointer compare rather than a
/// re-encode of the whole readout.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Readout {
    /// Display cells, digits and colons together — `text.chars().count()`.
    pub(crate) cells: usize,
    /// The buffer [`kit::seven_seg_size`] measures for this text. Carried
    /// rather than re-derived per mapping pass for the `Arc`'s reason.
    pub(crate) size: (u32, u32),
    /// Two texels per cell — `[origin_x, code]` — with `code` the lit-segment
    /// mask for a digit and bit [`COLON_BIT`] alone for a colon. `None` for an
    /// empty readout, which is what `GlUniforms::data` wants there (it binds a
    /// 1×1 zero texture and sets `u_data_len` to `0`).
    ///
    /// Two texels rather than one packed word because both values are small
    /// integers that an `f32` carries exactly, and a packed `origin * 256 +
    /// code` would be a second encoding for the shader to undo. The origins
    /// come from [`kit::seven_seg_layout`] rather than from a re-derivation of
    /// `PAD + Σ(width + GAP)`, which is the whole reason that function is
    /// `pub`.
    pub(crate) strip: Option<Arc<[f32]>>,
}

/// Encode `text` for the shader — the pure half of the mapping.
///
/// Goes through [`kit::seven_seg_layout`], so an uncovered char becomes the
/// kit's own blank (all-ghost) cell here rather than a second fallback policy
/// in the shader, and a `:` becomes the kit's colon.
pub(crate) fn readout(text: &str) -> Readout {
    let cells = kit::seven_seg_layout(text);
    let (width, height) = kit::seven_seg_size(text);
    let mut strip = Vec::with_capacity(cells.len() * 2);
    for cell in &cells {
        strip.push(f32_of(cell.x));
        strip.push(f32::from(cell.mask.unwrap_or(1 << COLON_BIT)));
    }
    Readout {
        cells: cells.len(),
        size: (u32_of(width), u32_of(height)),
        strip: (!strip.is_empty()).then(|| Arc::from(strip.into_boxed_slice())),
    }
}

/// Map one already-encoded readout onto the GL node payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface) and
/// its five siblings are: it reads no globals, resolves no palette and touches
/// no GL. The caller passes the palette it has already resolved *inside the
/// widget's `with_pins` scope*, so accent / role / pin precedence stays the
/// kit's one implementation.
///
/// There is no animation state to carry and no clock: `seven_seg` is a pure
/// function of `(text, style)` in the kit too.
pub(crate) fn seven_seg_surface(readout: &Readout, palette: &kit::PaletteSnapshot) -> KitSurface {
    // A skin with no bloom reaches the shaders as radius 0 / strength 0, which
    // makes the blur the identity and the blit's max-combine a no-op — see
    // `program`'s module docs on why the passes run unconditionally, and the
    // corner it records about a hypothetical `radius: 0, strength > 256` skin.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;

    KitSurface {
        width: readout.size.0,
        height: readout.size.1,
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                ("u_cells", GlValue::Int(int_of(readout.cells))),
                ("u_pad", GlValue::Int(int_of(kit::SEVEN_SEG_PAD))),
                ("u_thick", GlValue::Int(int_of(kit::SEVEN_SEG_THICK))),
                // The nine elements, as the kit's own table rather than as the
                // numbers it currently holds.
                ("u_seg_a", bar(kit::SEVEN_SEG_BARS[0])),
                ("u_seg_b", bar(kit::SEVEN_SEG_BARS[1])),
                ("u_seg_c", bar(kit::SEVEN_SEG_BARS[2])),
                ("u_seg_d", bar(kit::SEVEN_SEG_BARS[3])),
                ("u_seg_e", bar(kit::SEVEN_SEG_BARS[4])),
                ("u_seg_f", bar(kit::SEVEN_SEG_BARS[5])),
                ("u_seg_g", bar(kit::SEVEN_SEG_BARS[6])),
                ("u_dot_0", bar(kit::SEVEN_SEG_COLON_DOTS[0])),
                ("u_dot_1", bar(kit::SEVEN_SEG_COLON_DOTS[1])),
                (
                    "u_ghost_on",
                    GlValue::Int(i32::from(palette.ghost.is_some())),
                ),
                (
                    "u_ghost",
                    channels(palette.ghost.unwrap_or([0, 0, 0, 0xff])),
                ),
                ("u_bloom_radius", GlValue::Int(int_of(bloom.radius))),
                ("u_bloom_strength", GlValue::Int(i32::from(bloom.strength))),
                ("u_bg", channels(palette.bg)),
                ("u_ink", channels(palette.ink)),
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                // **Not re-phased**, like the LED strip and unlike the two dot
                // surfaces (#1091): this widget has no dot grid for the comb to
                // sit in the seams of, so it keeps `Mask::CRT` exactly as the
                // skin states it — which is what the kit's own `composite` call
                // hands `Emission::composite` here.
                (
                    "u_mask_pitch",
                    GlValue::Int(mask.map_or(0, |m| int_of(m.pitch))),
                ),
                (
                    "u_mask_phase",
                    GlValue::Int(mask.map_or(0, |m| int_of(m.phase))),
                ),
                (
                    "u_scanline_keep",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.scanline_keep).unwrap_or(0))),
                ),
                (
                    "u_corner_keep",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.corner_keep).unwrap_or(0))),
                ),
            ],
            data: readout.strip.clone(),
            // The buffer the kit renders, which for this widget is already the
            // native one: the cell metrics are the size knob, so there is no
            // `scale` to multiply (the dot matrix's situation, #1091).
            grid: (readout.size.0, readout.size.1),
            // No step passes, so nothing counts steps. See
            // [`SEVEN_SEG_PIPELINE`].
            step_seq: 0,
        },
    }
}

/// One [`kit::SevenSegBar`] as the `vec4` its uniform carries:
/// `(x, y, len, flags)` with `flags` = `vertical | tapered << 1`.
///
/// The two booleans ride in one channel rather than two uniforms because they
/// are properties of the same table row: a bar that lost its `tapered` flag on
/// the way across would be a different shape, and keeping them together means
/// there is one row to get wrong instead of three.
fn bar(bar: kit::SevenSegBar) -> GlValue {
    let flags = u8::from(bar.vertical) | (u8::from(bar.tapered) << 1);
    GlValue::Vec4([
        f32_of(bar.x),
        f32_of(bar.y),
        f32_of(bar.len),
        f32::from(flags),
    ])
}

/// A buffer coordinate as the `float` a texel or a uniform channel carries.
///
/// Every value reaching this is a cell origin or a bar metric, bounded by the
/// wire's `MAX_STRIP_DIM` — far inside `f32`'s exact integer range of `2^24`,
/// which is what makes the shader's `+ 0.5` round-trips exact.
#[allow(clippy::cast_precision_loss)]
fn f32_of(value: usize) -> f32 {
    value as f32
}

/// A buffer dimension as a `u32`, saturating.
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
        Arc, COLON_BIT, GlBlend, GlDraw, GlInput, GlTarget, GlUniforms, GlValue,
        SEVEN_SEG_PIPELINE, kit, readout, seven_seg_surface,
    };

    /// The shader body — the source the scans below read back.
    const BODY: &str = include_str!("seven_seg.frag");
    /// …and the blur's, which the mirror transcribes too.
    const BLUR_BODY: &str = include_str!("blur.frag");

    fn uniform(uniforms: &GlUniforms, name: &str) -> GlValue {
        let (_, value) = uniforms
            .values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("no uniform named {name}"));
        *value
    }

    /// **The uniform table is the kit's own metrics**, name for name and value
    /// for value — and every geometric row is written as the kit item it comes
    /// from, never as the literal it currently equals (the #1164 shape).
    ///
    /// That is the whole reason [`kit::SEVEN_SEG_BARS`] and friends became
    /// `pub`: a `vec4(1.0, 0.0, 28.0, 2.0)` here would agree with the kit today
    /// and keep agreeing after someone widened a digit, which is a mirror
    /// agreeing with itself. The names are the contract with the GLSL — a
    /// rename on one side alone draws nothing and says nothing — and the order
    /// is pinned so a reordering shows as a diff.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row, by
    /// spelling a metric as a literal and then moving the kit's, or by packing
    /// a bar's flags in the wrong channel.
    #[test]
    fn the_uniform_table_is_the_kits_own_metrics() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
        let surface = seven_seg_surface(&readout("12:34"), &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_cells",
                "u_pad",
                "u_thick",
                "u_seg_a",
                "u_seg_b",
                "u_seg_c",
                "u_seg_d",
                "u_seg_e",
                "u_seg_f",
                "u_seg_g",
                "u_dot_0",
                "u_dot_1",
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

        let int = |name| match uniform(&surface.uniforms, name) {
            GlValue::Int(value) => value,
            other => panic!("{name} is {other:?}, wanted an Int"),
        };
        assert_eq!(int("u_cells"), 5);
        // The two scalar metrics, as the kit's items rather than as 8 and 6.
        assert_eq!(int("u_pad"), i32::try_from(kit::SEVEN_SEG_PAD).unwrap());
        assert_eq!(int("u_thick"), i32::try_from(kit::SEVEN_SEG_THICK).unwrap());
        // …and the nine element rows, each against its own table entry.
        for (name, bar) in [
            ("u_seg_a", kit::SEVEN_SEG_BARS[0]),
            ("u_seg_b", kit::SEVEN_SEG_BARS[1]),
            ("u_seg_c", kit::SEVEN_SEG_BARS[2]),
            ("u_seg_d", kit::SEVEN_SEG_BARS[3]),
            ("u_seg_e", kit::SEVEN_SEG_BARS[4]),
            ("u_seg_f", kit::SEVEN_SEG_BARS[5]),
            ("u_seg_g", kit::SEVEN_SEG_BARS[6]),
            ("u_dot_0", kit::SEVEN_SEG_COLON_DOTS[0]),
            ("u_dot_1", kit::SEVEN_SEG_COLON_DOTS[1]),
        ] {
            assert_eq!(uniform(&surface.uniforms, name), super::bar(bar), "{name}");
        }

        assert_eq!(int("u_ghost_on"), i32::from(palette.ghost.is_some()));
        let bloom = palette.bloom.expect("the CRT skin glows");
        assert_eq!(int("u_bloom_radius"), i32::try_from(bloom.radius).unwrap());
        assert_eq!(int("u_bloom_strength"), i32::from(bloom.strength));
        assert_eq!(
            uniform(&surface.uniforms, "u_bg"),
            super::channels(palette.bg),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(palette.ink),
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
    }

    /// **The grid and the natural size are `seven_seg_size`'s buffer** — the
    /// one the kit actually renders, with no upscale to multiply.
    ///
    /// **Falsified** by transcribing the width formula here instead of calling
    /// the kit, and then moving `SEVEN_SEG_GAP`.
    #[test]
    fn the_grid_is_the_kits_own_buffer() {
        for text in ["", " ", "8", "12:34", "9876543210"] {
            let surface = seven_seg_surface(
                &readout(text),
                &kit::palette_snapshot(kit::DisplayStyle::Vfd),
            );
            let frame = kit::seven_seg(text, kit::DisplayStyle::Vfd);
            assert_eq!(
                (surface.width as usize, surface.height as usize),
                (frame.width(), frame.height()),
                "{text:?}",
            );
            assert_eq!(surface.uniforms.grid, (surface.width, surface.height));
        }
    }

    /// **The strip is [`kit::seven_seg_layout`]'s own answer**, two texels per
    /// cell, with the colon spelled by a bit no digit mask can occupy.
    ///
    /// The origins being **strictly increasing** is the precondition
    /// `seven_seg.frag`'s binary search runs on — a search over an unsorted
    /// sequence silently returns the wrong cell rather than failing — so it is
    /// asserted here rather than assumed there.
    ///
    /// **Falsified** by encoding the mask before the origin, by re-deriving the
    /// origins from `PAD`/`GAP` instead of reading the layout, or by giving a
    /// colon the mask `0` (which a space already has).
    #[test]
    fn the_strip_is_the_kits_own_layout() {
        let text = "12:34";
        let encoded = readout(text);
        let strip = encoded.strip.as_ref().expect("five cells");
        let cells = kit::seven_seg_layout(text);
        assert_eq!(encoded.cells, cells.len());
        assert_eq!(strip.len(), cells.len() * 2);

        let mut previous = None;
        for (i, cell) in cells.iter().enumerate() {
            let origin = strip[i * 2];
            let code = strip[i * 2 + 1];
            assert!(
                previous.is_none_or(|last| origin > last),
                "origins must strictly increase for the shader's binary search",
            );
            previous = Some(origin);
            assert!(
                (origin - super::f32_of(cell.x)).abs() < f32::EPSILON,
                "cell {i}",
            );
            match cell.mask {
                Some(mask) => {
                    assert!(
                        (code - f32::from(mask)).abs() < f32::EPSILON,
                        "cell {i} is a digit",
                    );
                    assert!(
                        mask < 1 << COLON_BIT,
                        "a digit mask never reaches the colon bit",
                    );
                }
                None => assert!(
                    (code - f32::from(1_u8 << COLON_BIT)).abs() < f32::EPSILON,
                    "cell {i} is the colon",
                ),
            }
        }

        // An empty readout binds no strip at all, which is what makes
        // `u_data_len == 0` the shader's "nothing to draw".
        let empty = readout("");
        assert_eq!(empty.cells, 0);
        assert!(empty.strip.is_none());
        let surface = seven_seg_surface(&empty, &kit::palette_snapshot(kit::DisplayStyle::Lcd));
        assert!(surface.uniforms.data.is_none());
        assert_eq!(uniform(&surface.uniforms, "u_cells"), GlValue::Int(0));
    }

    /// **The encoded readout travels as one allocation** — #911's rule, the
    /// same one `Renderer::DotMatrixGl`'s strip follows: a repeat mapping pass
    /// clones an `Arc` instead of walking the text again.
    #[test]
    fn the_strip_travels_as_the_callers_allocation() {
        let encoded = readout("07:16");
        let surface = seven_seg_surface(&encoded, &kit::palette_snapshot(kit::DisplayStyle::Oled));
        let held = surface.uniforms.data.as_ref().expect("five cells");
        assert!(Arc::ptr_eq(held, encoded.strip.as_ref().unwrap()));
    }

    /// **Four passes over three aux textures, the last one on the screen** —
    /// the structural claim the module docs make, asserted rather than
    /// described.
    ///
    /// **Falsified** by any of: an `aux` below `3`, a step pass (this widget
    /// has no animation at all), a blit that reads the *lit* texture back
    /// instead of recomputing it, or a target other than the screen last.
    #[test]
    fn the_pipeline_blurs_the_lit_layer_and_ends_on_the_screen() {
        assert_eq!(SEVEN_SEG_PIPELINE.aux, 3);
        assert!(
            SEVEN_SEG_PIPELINE.step.is_empty(),
            "a readout carries no cross-frame GPU state",
        );
        assert_eq!(
            SEVEN_SEG_PIPELINE.frame.len(),
            4,
            "lit, blur H, blur V, blit"
        );
        assert_eq!(SEVEN_SEG_PIPELINE.frame[0].target, GlTarget::Aux(0));
        assert_eq!(SEVEN_SEG_PIPELINE.frame[1].target, GlTarget::Aux(1));
        assert_eq!(SEVEN_SEG_PIPELINE.frame[2].target, GlTarget::Aux(2));
        let blit = SEVEN_SEG_PIPELINE.frame[3];
        assert_eq!(blit.target, GlTarget::Screen);
        assert_eq!(
            blit.inputs,
            &[GlInput::Data, GlInput::Aux(2)],
            "the blit recomputes the lit layer and samples only the blurred copy",
        );
        for pass in SEVEN_SEG_PIPELINE.frame {
            assert_eq!(pass.blend, GlBlend::Replace);
            assert_eq!(pass.draw, GlDraw::FullScreen);
            assert_ne!(
                pass.target,
                GlTarget::Accumulator,
                "this pipeline declares no accumulator",
            );
        }
    }

    /// **The shader declares the CRT pass's four constants with the kit's own
    /// values** — the shared #1186 helper, called here for the reason it exists
    /// (a `.frag` cannot read a Rust `const`, so the copy must be checked).
    #[test]
    fn the_shader_declares_the_kits_crt_constants() {
        super::super::program::assert_crt_constants("seven_seg.frag", BODY);
    }

    /// **The shader takes its segment geometry from uniforms, never from a
    /// literal of its own.**
    ///
    /// The #1164 shape adapted to a shader whose mirrored constants are
    /// *uniforms*: the nine elements and the two scalar metrics are read by
    /// name, and the strongest statement available is that no compile-time
    /// constant beside them is a metric.
    ///
    /// **Falsified** by inlining any of them (a `const int THICK = 6;` beside
    /// the CRT block, or a bare `28.0` where `bar.z` stands), which would let
    /// the kit's own value move without moving what CI compiles.
    #[test]
    fn the_shader_reads_its_geometry_from_uniforms() {
        for name in ["u_pad", "u_thick", "u_cells"] {
            assert!(
                BODY.contains(&format!("uniform int {name};")),
                "seven_seg.frag must declare `uniform int {name};`",
            );
        }
        for name in [
            "u_seg_a", "u_seg_b", "u_seg_c", "u_seg_d", "u_seg_e", "u_seg_f", "u_seg_g", "u_dot_0",
            "u_dot_1",
        ] {
            assert!(
                BODY.contains(&format!("uniform vec4 {name};")),
                "seven_seg.frag must declare `uniform vec4 {name};`",
            );
        }
        let declared: Vec<&str> = BODY
            .lines()
            .filter_map(|line| line.trim().strip_prefix("const int "))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        assert_eq!(
            declared,
            vec![
                "LAYER_LIT",
                "LAYER_BLIT",
                "SEG_COUNT",
                "COLON_BIT",
                "CELL_SEARCH_STEPS",
                "MASK_ONE",
                "COORD_ONE",
                "BAND_DIV",
                "CORNER_DIV",
            ],
            "seven_seg.frag grew a compile-time constant — a segment metric \
             belongs in the uniform bag, where it is the kit's own item",
        );
    }

    /// **The shader's `SEG_COUNT` and `COLON_BIT` are the mapping's**, so the
    /// strip one side writes is the strip the other reads.
    ///
    /// `SEG_COUNT` is `hytte_preem::SEVEN_SEG_BARS.len()` and also how many
    /// `u_seg_*` uniforms exist; `COLON_BIT` is this module's own
    /// [`COLON_BIT`]. A disagreement about either draws a blank cell where a
    /// colon belongs, or lights segment `A` on every colon — both silent.
    #[test]
    fn the_shader_and_the_mapping_agree_about_the_strip_encoding() {
        assert!(
            BODY.contains(&format!(
                "const int SEG_COUNT = {};",
                kit::SEVEN_SEG_BARS.len()
            )),
            "seven_seg.frag's SEG_COUNT must be `SEVEN_SEG_BARS.len()`",
        );
        assert!(
            BODY.contains(&format!("const int COLON_BIT = {COLON_BIT};")),
            "seven_seg.frag's COLON_BIT must be the mapping's",
        );
    }

    /// **`TAPER_EDGE` is the kit's staircase, read continuously** — the one law
    /// this side copies rather than reads off a uniform, and the whole reason
    /// the 1:1 comparison can be pinned bit-exact.
    ///
    /// `hytte_preem::seven_seg_taper(k)` insets row `k` of a `THICK`-wide bar;
    /// the shader instead measures the row's own centre against the bar's
    /// centre line and cuts at 45°. This asserts the two agree at **every**
    /// row, which is where the shader samples them at 1:1.
    ///
    /// **Falsified** by moving `TAPER_EDGE` off `0.5` in either place, or by
    /// changing `taper` in the kit.
    #[test]
    fn the_taper_law_is_the_kits_own_staircase() {
        let edge = taper_edge_from_source();
        for k in 0..kit::SEVEN_SEG_THICK {
            #[allow(clippy::cast_precision_loss)]
            let centre_offset = (k as f32 + 0.5 - kit::SEVEN_SEG_THICK as f32 * 0.5).abs();
            let continuous = (centre_offset - edge).max(0.0);
            #[allow(clippy::cast_precision_loss)]
            let staircase = kit::seven_seg_taper(k) as f32;
            assert!(
                (continuous - staircase).abs() < f32::EPSILON,
                "row {k}: the 45° chamfer reads {continuous}, the kit insets {staircase}",
            );
        }
    }

    /// `TAPER_EDGE` as the shipped GLSL declares it, so the assertion above is
    /// about the number that ships rather than a copy beside it.
    fn taper_edge_from_source() -> f32 {
        let line = BODY
            .lines()
            .find_map(|line| line.trim().strip_prefix("const float TAPER_EDGE = "))
            .expect("seven_seg.frag must declare `const float TAPER_EDGE = …;`");
        line.trim_end_matches(';')
            .trim()
            .parse()
            .expect("TAPER_EDGE must be a float literal")
    }

    /// **The shader's arithmetic, transcribed here, reproduces the kit byte for
    /// byte at 1:1 — on every skin, over every cell shape this widget can
    /// draw.**
    ///
    /// The #1153 pattern, and the test that actually earns the bit-exact pin:
    /// [`shader_frame`] is a line-by-line Rust transcription of
    /// `seven_seg.frag`'s snapped branch **and** of `blur.frag` — the same
    /// half-plane residuals, the same two truncating integer divisions, the
    /// same `mix_kit`, the same CRT pass, in the same order — compared against
    /// `kit::seven_seg`'s own bytes. It needs no GL at all, and llvmpipe is not
    /// available to `cargo test`.
    ///
    /// The taper's continuous reading collapsing onto the kit's integer
    /// staircase is the single most plausible way this arm could be wrong, and
    /// it is a collapse that would stop being exact **silently**.
    ///
    /// The transcription is held to the **shipped** GLSL by
    /// [`the_mirror_is_the_shipped_shaders_arithmetic`], so the two cannot
    /// drift: this test says the arithmetic is right, that one says it is the
    /// arithmetic that ships. Neither replaces `preem_gl_diff` — a driver can
    /// still disagree with both.
    ///
    /// **Falsified** by any of: dropping the `+ 127` from `mix_kit`, dividing
    /// the blur in floats, renormalising the blur window where it clips at a
    /// buffer edge, folding the two blur passes into one 2-D kernel, compositing
    /// the ghost over the lit layer instead of under it, or moving
    /// `TAPER_EDGE`.
    ///
    /// The whole sweep runs inside one [`kit::with_pins`] scope pinning
    /// [`kit::Ink::Base`], for `led_strip.rs`'s measured reason: the kit's
    /// accent is a process-global `AtomicU32` that both `palette_snapshot`
    /// (here) and `seven_seg` (the oracle) read at render time, and the suite
    /// runs concurrently, so a flip landing between the two calls would make
    /// them differ for a reason that is not this code.
    #[test]
    fn the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                for style in kit::DisplayStyle::ALL {
                    for text in [
                        "",
                        " ",
                        "8",
                        "88",
                        "12:34",
                        "-",
                        "9876543210",
                        "x?",
                        "07:16",
                    ] {
                        let reference = kit::seven_seg(text, style);
                        let mirror = shader_frame(style, text);
                        assert_eq!(mirror, reference.data(), "{style:?} {text:?}");
                    }
                }
            },
        );
    }

    /// The one decision point of [`face_cov`]'s ramp that costs a byte to
    /// cross — see [`the_coverage_operands_are_never_near_a_decision_point`]
    /// on why `clamp`'s two bounds do not.
    const MIDPOINT: f32 = 0.5;

    /// What counts as "a rounding distance" from [`MIDPOINT`]. Three orders of
    /// magnitude above the `1e-6` the interpolant was measured at, and three
    /// below the `0.25` the sparsest lattice here leaves.
    const MARGIN: f32 = 1e-3;

    /// One [`sweep_operands`] census of [`face_cov`]'s operands, against
    /// [`MIDPOINT`].
    struct Sweep {
        /// How many were looked at at all — the guard against a sweep that
        /// walks an empty readout and reports a clean zero.
        seen: u64,
        /// Within [`MARGIN`] of the midpoint without being on it. **This is the
        /// number that must be zero.**
        near: u64,
        /// Exactly on the midpoint, where the numerator is exactly zero and the
        /// byte is `128` on any IEEE-754 arithmetic.
        mid: u64,
        /// The closest any operand came without being on it.
        closest: f32,
    }

    /// The elements a cell's `code` lights, which is what `cell255` walks: a
    /// colon's two dots, or the digit's own subset of the seven bars.
    fn lit_elements(code: i32) -> Vec<kit::SevenSegBar> {
        if code & (1 << i32::from(COLON_BIT)) != 0 {
            return kit::SEVEN_SEG_COLON_DOTS.to_vec();
        }
        kit::SEVEN_SEG_BARS
            .into_iter()
            .enumerate()
            .filter(|(bit, _)| code & (1 << bit) != 0)
            .map(|(_, bar)| bar)
            .collect()
    }

    /// `bar255`'s three `(numerator, width)` pairs at `point` — the three
    /// [`face_cov`] calls [`bar_cov`] `min`-combines, before the divide.
    ///
    /// An untapered bar's [`NO_CHAMFER`] is dropped rather than censused: it is
    /// a sentinel nine orders of magnitude off the ramp, not a face, and
    /// [`bar_residuals`]' doc says why it is that far down.
    #[allow(clippy::cast_precision_loss)]
    fn face_operands(
        point: (f32, f32),
        footprint: (f32, f32),
        bar: kit::SevenSegBar,
    ) -> Vec<(f32, f32)> {
        let half_short = kit::SEVEN_SEG_THICK as f32 * 0.5;
        let half_long = bar.len as f32 * 0.5;
        let centre = if bar.vertical {
            (bar.x as f32 + half_short, bar.y as f32 + half_long)
        } else {
            (bar.x as f32 + half_long, bar.y as f32 + half_short)
        };
        let offset = ((point.0 - centre.0).abs(), (point.1 - centre.1).abs());
        let (along, across) = if bar.vertical {
            (offset.1, offset.0)
        } else {
            (offset.0, offset.1)
        };
        let residuals = bar_residuals(along, across, half_long, half_short, bar.tapered);
        let (long, short) = if bar.vertical {
            (footprint.1, footprint.0)
        } else {
            (footprint.0, footprint.1)
        };
        [
            (residuals[0], short),
            (residuals[1], long),
            (residuals[2], long + short),
        ]
        .into_iter()
        .filter(|(residual, _)| *residual > NO_CHAMFER * 0.5)
        .collect()
    }

    /// Walk every device fragment of `text` at `stretch` and census
    /// [`face_operands`] against [`MIDPOINT`].
    ///
    /// `nudge` is added to the sample point, which is
    /// [`the_coverage_operands_are_never_near_a_decision_point`]'s negative
    /// control: the driver's interpolation error, put back.
    #[allow(clippy::cast_precision_loss)]
    fn sweep_operands(text: &str, stretch: usize, nudge: f32) -> Sweep {
        let encoded = readout(text);
        let grid = (encoded.size.0 as usize, encoded.size.1 as usize);
        let view = (grid.0 * stretch, grid.1 * stretch);
        let strip = encoded.strip.as_deref().unwrap_or(&[]);
        let cells = i32::try_from(encoded.cells).unwrap_or(i32::MAX);
        // `u_px_step` — the device→buffer step, divided **once, here**, exactly
        // as `gl_surface.rs` divides it for the shader. It is also
        // `seven_seg.frag`'s `fp`: the ratio *is* how much buffer one fragment
        // covers.
        let footprint = (grid.0 as f32 / view.0 as f32, grid.1 as f32 / view.1 as f32);
        // …and `pc`, which is `px * u_px_step` and nothing else — one
        // correctly-rounded multiply, no divide in the shader at all.
        let lattice = |index: usize, step: f32| (index as f32 + 0.5) * step;
        let mut census = Sweep {
            seen: 0,
            near: 0,
            mid: 0,
            closest: f32::INFINITY,
        };
        for down in 0..view.1 {
            for across in 0..view.0 {
                let point = (
                    lattice(across, footprint.0) + nudge,
                    lattice(down, footprint.1) + nudge,
                );
                let first = cell_at(strip, cells, point.0);
                // `strip255`'s two-cell walk.
                for step in 0..2 {
                    let cell = first + step;
                    if cell >= cells {
                        break;
                    }
                    let local = (
                        point.0 - cell_x(strip, cell),
                        point.1 - kit::SEVEN_SEG_PAD as f32,
                    );
                    for bar in lit_elements(cell_code(strip, cell, false)) {
                        for (numerator, width) in face_operands(local, footprint, bar) {
                            census.seen += 1;
                            let operand = 0.5 - numerator / width.max(1e-6);
                            let distance = (operand - MIDPOINT).abs();
                            if distance == 0.0 {
                                census.mid += 1;
                                // `±0.0` exactly, bit for bit: an operand
                                // reaching the midpoint any other way would mean
                                // the *divide* decided the byte there.
                                assert_eq!(
                                    numerator.to_bits() & 0x7fff_ffff,
                                    0,
                                    "the ramp's midpoint was reached with a non-zero numerator \
                                     ({numerator:e}), so it is the divide that decides the byte \
                                     there and not the geometry",
                                );
                            } else {
                                census.closest = census.closest.min(distance);
                                if distance < MARGIN {
                                    census.near += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        census
    }

    /// How many **native** pixels of `text` at `stretch` contain at least one
    /// device fragment where `halo_at`'s bilinear result lands exactly on
    /// `k + 0.5` — the truncation boundary its `+ 0.5` rounds at, and the
    /// continuous branch's second one.
    ///
    /// The whole of `halo_at` plus the two passes that fill the texture it
    /// reads, so the census is of the aux the shader actually samples and not
    /// of a stand-in: `LAYER_LIT` at the grid (the point test — the emission
    /// the kit blurs is `255` on a lit segment and nothing elsewhere), then
    /// `blur.frag` twice, then the tap.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn halo_boundary_pixels(style: kit::DisplayStyle, text: &str, stretch: usize) -> usize {
        let palette = kit::palette_snapshot(style);
        let encoded = readout(text);
        let (w, h) = (encoded.size.0 as usize, encoded.size.1 as usize);
        let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
            radius: 0,
            strength: 0,
        });
        let mut emission = vec![0_i32; w * h];
        for row in 0..h {
            for col in 0..w {
                emission[row * w + col] = strip255(centre(col, row), &encoded, false);
            }
        }
        let tmp = blur_pass(&emission, (w, h), bloom.radius, (1, 0));
        let blurred = blur_pass(&tmp, (w, h), bloom.radius, (0, 1));

        let (vw, vh) = (w * stretch, h * stretch);
        let step = (w as f32 / vw as f32, h as f32 / vh as f32);
        let mut hits: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
        for down in 0..vh {
            for across in 0..vw {
                let point = ((across as f32 + 0.5) * step.0, (down as f32 + 0.5) * step.1);
                // `halo_at`, verbatim, up to the `+ 0.5` this is about.
                let offset = (point.0 - 0.5, point.1 - 0.5);
                let base = (offset.0.floor(), offset.1.floor());
                let frac = (offset.0 - base.0, offset.1 - base.1);
                let lo_x = (base.0 as i32).clamp(0, w as i32 - 1);
                let lo_y = (base.1 as i32).clamp(0, h as i32 - 1);
                let hi_x = (base.0 as i32 + 1).clamp(0, w as i32 - 1);
                let hi_y = (base.1 as i32 + 1).clamp(0, h as i32 - 1);
                let tap = |x: i32, y: i32| blurred[(y as usize) * w + (x as usize)] as f32;
                let mix = |a: f32, b: f32, t: f32| a * (1.0 - t) + b * t;
                let top = mix(tap(lo_x, lo_y), tap(hi_x, lo_y), frac.0);
                let bottom = mix(tap(lo_x, hi_y), tap(hi_x, hi_y), frac.0);
                let value = mix(top, bottom, frac.1);
                // `== 0.5` bit for bit — the boundary is exact or it is not.
                if value.fract().to_bits() == 0.5_f32.to_bits() {
                    hits.insert((across / stretch, down / stretch));
                }
            }
        }
        hits.len()
    }

    /// **No coverage operand is decided by rounding** (#1298).
    ///
    /// Scoped to [`face_cov`]'s operands, and the name says so. The continuous
    /// branch has a **second** rounding boundary — `halo_at`'s
    /// `int(mix(…) + 0.5)` — which this sweep does not walk and
    /// [`the_halo_tap_rounds_on_a_boundary_the_geometry_reaches`] does.
    ///
    /// The four supersampled readout cases are the only place in the suite
    /// where a *coverage function* is held to bit-exactness off a rasterisation
    /// edge, and at an integer stretch that function is maximally degenerate:
    /// every one of the kit's boundaries is an integer buffer coordinate,
    /// [`TAPER_EDGE`] is a half-integer, and the device lattice is
    /// `(i + 0.5) / stretch` — so `across` and `along` come out odd multiples of
    /// `1 / (2 * stretch)` and [`face_cov`]'s operand `0.5 - r / w` lands on a
    /// lattice of its own, straight on top of the ramp's decision points.
    ///
    /// **Only one of those points costs anything to cross**, which is why this
    /// test is about that one. Crossing `clamp`'s bounds is free: an operand
    /// that should be `0.0` and comes out `+ε` still gives
    /// `int(255 * ε + 0.5) == 0`, and one that should be `1.0` and comes out
    /// `1 - ε` is still the largest thing the `min` sees. The **midpoint** is
    /// not: `int(255 * 0.5 + 0.5)` is `int(128.0)`, sitting exactly on a
    /// truncation boundary, so an operand a millionth under it drops the byte
    /// to `127`. Landing *exactly* on the midpoint is fine — `q == 0.5` means
    /// the numerator is exactly `0.0`, `0.0 / w` is `0.0` on any IEEE-754
    /// arithmetic with no tolerance at all, and `255.0 * 0.5 + 0.5` is exactly
    /// `128.0` under GLSL ES's correctly-rounded `*` and `+`. Landing a
    /// millionth away from it is the failure, because then the last bit of
    /// whatever produced the sample point decides the byte rather than the
    /// geometry.
    ///
    /// Which is the whole of what `seven_seg.frag`'s `fi`/`px`/`pc` and
    /// `u_px_step` exist for. **GLSL ES 3.20 §4.7.1 pins `a + b`, `a - b` and
    /// `a * b` to a correctly rounded result, allows `a / b` and `1.0 / b`
    /// 2.5 ULP, and says nothing at all about attribute interpolation.** Both
    /// of the unpinned ones used to reach this sweep:
    ///
    /// * the sample point was the interpolated `v_uv` scaled by the grid, and
    ///   measured on llvmpipe at the harness's x2 clock face that put 8455 of
    ///   the three faces' operands within `1e-3` of a decision point *without
    ///   being on one*, the closest `1e-6` away;
    /// * and the grid/viewport ratio was a shader-side `/`, which §4.7.1 lets
    ///   an implementation return 2.5 ULP of the quotient's own magnitude off
    ///   — `3.8e-5` at a sample point reaching 188 buffer pixels, 38x the
    ///   interpolation error and on its own enough to move the chamfer's exact
    ///   `0.5` to `0.49992` and its byte to `127`.
    ///
    /// The first is why the lattice comes off `floor(v_uv * vp)`; the second is
    /// why the ratio arrives as `u_px_step`, divided in `gl_surface.rs` where
    /// IEEE-754 says correctly rounded, so the shader multiplies and nothing
    /// under it divides by anything but an exact `0.0`. Rebuilt that way, no
    /// operand is within a rounding distance of the midpoint that is not on
    /// it.
    ///
    /// **What the sweep below prints**, so the figures in this file are the
    /// ones its own code produces (the #1309 review caught a `837` that was
    /// not): at stretch 2, `seen 1042160 / near 0 / mid 1580 / closest 5e-1`;
    /// at 3, `2344860 / 0 / 0 / 2.5e-1`; at 4, `4168640 / 0 / 3160 / 5e-1`.
    /// Nudged by `1e-6` the stretch-2 midpoint population splits
    /// `763 + 817 = 1580`, which is what makes the control readable. The `837`
    /// was measured on the *pre-fix* interpolant-shaped lattice, where
    /// rounding had already knocked the other 743 off the midpoint — a true
    /// number about a lattice this code no longer builds, which is exactly the
    /// kind a reader cannot re-derive.
    ///
    /// The `nudge` arm is the negative control, and the reason this is a test
    /// rather than a comment: a sample point a millionth of a pixel off the
    /// lattice — which is what the driver's interpolant was — puts the
    /// population back. Without it the assertion would read as a property of
    /// the geometry when it is a property of *how the geometry is sampled*.
    ///
    /// Stretch 3 is in the sweep because it is the case that cannot be fixed by
    /// making the sample point exact — `1 / 3` is not a binary fraction, so
    /// `(i + 0.5) / 3` is a rounded number however it is computed. It passes
    /// anyway, and for a reason worth writing down: there the chamfer's operand
    /// lands on `0.5 ± odd/4`, a **quarter of a ramp** from the midpoint, so
    /// the degeneracy the x2 lattice has is not a property of integer stretches
    /// in general.
    ///
    /// This says the invariant holds; [`the_mirror_is_the_shipped_shaders_arithmetic`]
    /// says the shipped GLSL is what builds the lattice it holds for. Neither
    /// replaces the other: a `pc` quietly taken off `v_uv` again would leave
    /// this green, because the sweep below computes its own lattice.
    #[test]
    fn the_coverage_operands_are_never_near_a_decision_point() {
        // 2 is what the harness renders and the stretch the x2 readout cases
        // are; 3 and 4 are the two a `scale_factor` produces. 4's ratio is a
        // binary fraction like 2's, 3's is not — see the doc.
        for stretch in [2_usize, 3, 4] {
            let exact = sweep_operands("12:34", stretch, 0.0);
            assert!(
                exact.seen > 100_000,
                "the stretch-{stretch} sweep looked at only {} operand(s) — it is walking the \
                 wrong readout and every assertion below it is vacuous",
                exact.seen,
            );
            assert_eq!(
                exact.near, 0,
                "at stretch {stretch} the lattice leaves {} operand(s) within {MARGIN} of the \
                 ramp's midpoint without being on it (closest {:e}) — one byte apiece, decided \
                 by whatever produced the sample point rather than by the geometry",
                exact.near, exact.closest,
            );

            // The control, and the reason this is a test — but only where the
            // lattice reaches the midpoint at all, since a nudge can only push
            // something *off* a point it is standing on. At stretch 3 it does
            // not, which the assertion below is the statement of.
            if exact.mid > 0 {
                let nudged = sweep_operands("12:34", stretch, 1e-6);
                assert!(
                    nudged.near > 0,
                    "at stretch {stretch} a sample point 1e-6 off the lattice leaves none of \
                     the {} operand(s) on the midpoint inside the margin — then the assertion \
                     above is not measuring the sampling",
                    exact.mid,
                );
            }
        }

        // **Which stretches reach the midpoint at all**, since "no operand is
        // near it" means two different things on either side of that and only
        // one of them is a statement. At 2 and 4 the chamfer's operand lands on
        // it squarely and the margin above is load-bearing; at 3 it lands on
        // `0.5 ± odd/4`, a quarter of a ramp away, so the x2 degeneracy is a
        // property of *that* lattice and not of integer stretches at large.
        for (stretch, reaches) in [(2_usize, true), (3, false), (4, true)] {
            let seen = sweep_operands("12:34", stretch, 0.0);
            assert_eq!(
                seen.mid > 0,
                reaches,
                "stretch {stretch} puts {} operand(s) exactly on the ramp's midpoint \
                 (closest miss {:e}), which is not what this file says it does",
                seen.mid,
                seen.closest,
            );
        }
    }

    /// **The halo tap rounds on a boundary the geometry reaches — and that one
    /// is deliberately left alone** (#1298).
    ///
    /// The sibling above is scoped to [`face_cov`]'s operands, and this is the
    /// other half of the honest statement: `halo_at`'s `int(mix(…) + 0.5)` is
    /// a second truncation boundary on the same branch, and the same
    /// commensurate lattice walks straight onto it. At an integer stretch the
    /// bilinear weights are exactly `{0.25, 0.75}` and the four taps are whole
    /// bytes, so the result is a multiple of `1/16` and lands on `k + 0.5`
    /// exactly.
    ///
    /// **Why it is not fixed the way the chamfer was.** The chamfer's midpoint
    /// is reached with an exactly-zero *numerator*, so making the sample point
    /// exact makes the whole operand exact and the byte stops being a
    /// rounding. The halo's boundary is reached with a non-zero value —
    /// there is nothing exact to lean on — so the only way to move it is to
    /// change how the bloom rounds, which would move the halo's bytes
    /// *everywhere*, not only where an implementation could disagree. It is
    /// worth one 255th of `glow`, which `glow * u_bloom_strength / 256` and
    /// the `max` below it can only shrink: the same ±1 the coverage carries.
    ///
    /// So it is censused rather than removed, and the census lives here rather
    /// than in a review comment because a number nobody can re-derive is not a
    /// measurement. The `lcd` is the control that costs nothing, exactly as it
    /// is for the edge budget: `palette_snapshot(Lcd).bloom` is `None`, so the
    /// aux is identically zero, every tap is `0.0` and no mix of them can land
    /// on a half.
    ///
    /// Measured on the x2 clock face, and re-derivable by running this:
    /// **583 / 0 / 374 / 796** native pixels of 13160 on vfd / lcd / oled /
    /// crt. The same four numbers came out of a marker painted into the real
    /// GL frames on the #1309 review, which is a second arm agreeing with this
    /// mirror rather than a restatement of it.
    #[test]
    fn the_halo_tap_rounds_on_a_boundary_the_geometry_reaches() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                let counts: Vec<(kit::DisplayStyle, usize)> = kit::DisplayStyle::ALL
                    .into_iter()
                    .map(|style| (style, halo_boundary_pixels(style, "12:34", 2)))
                    .collect();
                for (style, hits) in counts {
                    let blooms = kit::palette_snapshot(style)
                        .bloom
                        .is_some_and(|bloom| bloom.strength > 0 && bloom.radius > 0);
                    assert_eq!(
                        hits > 0,
                        blooms,
                        "{style:?} puts {hits} native pixel(s) of the x2 clock face on \
                         `halo_at`'s rounding boundary while its bloom is {}— the census in \
                         this file's doc describes a different shader",
                        if blooms { "live " } else { "off " },
                    );
                }
            },
        );
    }

    /// **The transcription above is the arithmetic the shipped shaders carry**
    /// — a source scan over `seven_seg.frag` and `blur.frag`, so a fix applied
    /// to one side only reds here instead of leaving a green mirror describing
    /// a shader nobody ships.
    ///
    /// **This scan pins every clause the #1294 review found the mirror
    /// transcribes** (#1293 item 6): the clauses #1153's precedent already
    /// listed below, plus the eight the review found missing — the `bars[]`
    /// order, the CRT `depth`, the `ghost &&` gate, the edge `ex`, the
    /// two-cell loop, the colon's second dot, the comb condition and
    /// `shortSide`. The #1153-era version of this doc claimed "every clause
    /// is listed" and was wrong for exactly these eight (see the review); this
    /// one names the correction instead of repeating the claim that broke.
    /// What is **not** listed here — the continuous (non-`snapped`) branch
    /// this mirror never takes, since [`shader_frame`] always evaluates at a
    /// pixel centre — is `preem_gl_diff`'s job, not this test's: a driver can
    /// still disagree with both.
    ///
    /// **Falsified** by editing any of them in either `.frag` without editing
    /// [`shader_frame`].
    #[test]
    fn the_mirror_is_the_shipped_shaders_arithmetic() {
        for clause in [
            // face_cov / the three residuals / the two combinators
            "return clamp(0.5 - r / max(w, 1e-6), 0.0, 1.0);",
            "across - hs,",
            "along - hl,",
            "tapered ? (along + across - TAPER_EDGE - hl) : NO_CHAMFER",
            "return max(max(r.x, r.y), r.z) <= 0.0;",
            "min(face_cov(r.x, fs), face_cov(r.y, fl)),",
            "face_cov(r.z, fl + fs)",
            // bar255's frame
            "bool vertical = (int(bar.w + 0.5) & 1) == 1;",
            "bool tapered = (int(bar.w + 0.5) & 2) == 2;",
            "float hs = float(u_thick) * 0.5;",
            "float hl = bar.z * 0.5;",
            "? vec2(bar.x + hs, bar.y + hl)",
            ": vec2(bar.x + hl, bar.y + hs);",
            "vec2 d = abs(p - centre);",
            "float along = vertical ? d.y : d.x;",
            "float across = vertical ? d.x : d.y;",
            "return bar_inside(r) ? 255 : 0;",
            "return int(255.0 * bar_cov(r, fl, fs) + 0.5);",
            // the cell and the strip
            "if ((code & (1 << COLON_BIT)) != 0) {",
            "if ((code & (1 << i)) != 0) {",
            "int index = 2 * i;",
            "int index = 2 * i + 1;",
            "int code = int(texelFetch(u_tex0, ivec2(index, 0), 0).r + 0.5);",
            // the `ghost &&` gate (#1293 item 6): the mirror's `ghost &&`
            // guard on the `code & (1 << COLON_BIT) == 0` check.
            "if (ghost && (code & (1 << COLON_BIT)) == 0) {",
            "return (1 << SEG_COUNT) - 1;",
            "int mid = (lo + hi + 1) / 2;",
            "if (cell_x(mid) <= x) {",
            "vec2 local = p - vec2(cell_x(i), float(u_pad));",
            // the two-cell loop (#1293 item 6): `strip255` walks exactly two
            // cells, `i` and `i + 1`, which the Rust mirror's `for k in 0..2`
            // transcribes.
            "for (int k = 0; k < 2; ++k) {",
            // the bars[] order (#1293 item 6): the kit's own bit order, A
            // through G, which `kit::SEVEN_SEG_BARS.into_iter()` transcribes.
            "u_seg_a, u_seg_b, u_seg_c, u_seg_d, u_seg_e, u_seg_f, u_seg_g",
            // the colon's second dot (#1293 item 6): the lower dot, which
            // `.max(bar255(…COLON_DOTS[1]))` transcribes.
            "bar255(p, fp, snapped, u_dot_1)",
            // the composite
            "return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);",
            "return (a * (255 - k) + b * k + 127) / 255;",
            "int halo = min(glow * u_bloom_strength / 256, 255);",
            "lit = min(max(lit, halo), 255);",
            "lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;",
            "under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);",
            "under = mix_kit(under, ink, lit);",
            // the CRT pass
            "int shortSide = min(w, h);",
            "int r2 = (u * u + v * v) / 2;",
            // the CRT depth (#1293 item 6): `mask_one - corner_keep`, unpinned
            // before even though `radial`'s formula below (which reads it)
            // was.
            "int depth = MASK_ONE - u_corner_keep;",
            "int radial = clamp(MASK_ONE - (depth * r2) / (COORD_ONE * COORD_ONE), 0, MASK_ONE);",
            // the edge `ex` (#1293 item 6): `x.min(w - 1 - x)`, the short
            // axis's distance from either edge.
            "int ex = min(x, w - 1 - x);",
            "edge = MASK_ONE * d / band;",
            // the comb condition (#1293 item 6): the CRT's scanline gate,
            // `y % mask.pitch == mask.phase`.
            "if (u_mask_pitch != 0 && (y % u_mask_pitch) == u_mask_phase) {",
            "return radial * edge / MASK_ONE * comb / MASK_ONE;",
            "return ((2 * i + 1 - n) * COORD_ONE) / n;",
            // the lit pass's sample point
            "vec2 p = floor(gl_FragCoord.xy) + 0.5;",
            // …and the **blit** pass's, which #1298 took off the interpolant's
            // value and onto the fragment's own integer index. **All seven
            // clauses** are load-bearing and each fails differently: swap
            // `vp`'s components and every sample lands in the wrong place on a
            // non-square readout; drop the `floor` and the lattice is the
            // interpolant's again; drop the `+ 0.5` and every sample sits on a
            // buffer-pixel corner; take `col`/`row` off `v_uv` again and the
            // two halves can disagree about which buffer pixel a fragment is
            // in; form `u_px_step` in the shader instead of reading the
            // uniform and `pc` inherits GLSL ES's 2.5-ULP divide.
            //
            // `vp` was the one the #1293 rule missed on the first pass and the
            // review caught: swapping its components left the **whole**
            // hermetic suite green while `preem_gl_diff` went `FAIL 22 of 160`,
            // i.e. the only check that saw it needed a driver. See
            // [`the_coverage_operands_are_never_near_a_decision_point`], which
            // asserts the invariant this lattice buys and cannot see whether
            // the shipped file still builds it.
            "vec2 vp = vec2(max(u_viewport.x, 1), max(u_viewport.y, 1));",
            "vec2 fi = clamp(floor(v_uv * vp), vec2(0.0), vp - 1.0);",
            "vec2 px = vec2(fi.x, vp.y - 1.0 - fi.y) + 0.5;",
            "vec2 pc = px * u_px_step;",
            "int col = clamp(int(pc.x), 0, cols - 1);",
            "int row = clamp(int(pc.y), 0, rows - 1);",
            "vec2 fp = snapped ? vec2(1.0) : u_px_step;",
            // …and the halo tap's own rounding boundary, which
            // [`the_halo_tap_rounds_on_a_boundary_the_geometry_reaches`]
            // censuses and this arm deliberately does not move.
            "return int(mix(mix(v00, v10, f.x), mix(v01, v11, f.x), f.y) + 0.5);",
            // …and the gate that keeps it off the snapped branch, which is why
            // the 1:1 cases cannot reach that boundary at all.
            "int glow = snapped ? texel(u_tex1, ivec2(col, row)) : halo_at(pc);",
            // …and the two constants the mirror re-declares rather than reads.
            // `TAPER_EDGE`'s *value* is separately held to the kit itself by
            // `the_taper_law_is_the_kits_own_staircase`, which parses it out of
            // this same file; this pins that the mirror's copy is that value.
            "const float TAPER_EDGE = 0.5;",
            "const float NO_CHAMFER = -1e9;",
        ] {
            assert!(
                BODY.contains(clause),
                "seven_seg.frag no longer carries `{clause}` — the Rust mirror \
                 in this module describes a shader that is not the one shipping",
            );
        }
        // …and the blur, which the mirror transcribes just as literally.
        for clause in [
            "int window = 2 * radius + 1;",
            "sum += int(texelFetch(u_tex0, q, 0).r * 255.0 + 0.5);",
            "o_intensity = vec4(float(sum / window) / 255.0, 0.0, 0.0, 1.0);",
            "if (q.x < 0 || q.y < 0 || q.x >= u_grid.x || q.y >= u_grid.y) {",
        ] {
            assert!(
                BLUR_BODY.contains(clause),
                "blur.frag no longer carries `{clause}` — see above",
            );
        }
        // …and the ghost is composited **under** the lit layer, which is the
        // one ordering the kit states and a mirror could silently invert.
        let ghost_at = BODY
            .find("under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);")
            .expect("the ghost composite");
        let lit_at = BODY
            .find("under = mix_kit(under, ink, lit);")
            .expect("the lit composite");
        assert!(ghost_at < lit_at, "the lit layer composites over the ghost");
    }

    // ── the transcription ───────────────────────────────────────────────────

    /// `seven_seg.frag`'s snapped branch plus `blur.frag`, in Rust: one whole
    /// frame, top-down RGBA8, exactly as `Frame::data` lays it out.
    ///
    /// Everything below mirrors the GLSL statement for statement, `f32` for
    /// `float` and `i32` for `int`, which is what makes a disagreement with the
    /// kit attributable to the shader rather than to this file.
    fn shader_frame(style: kit::DisplayStyle, text: &str) -> Vec<u8> {
        let palette = kit::palette_snapshot(style);
        let encoded = readout(text);
        let (w, h) = (encoded.size.0 as usize, encoded.size.1 as usize);
        let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
            radius: 0,
            strength: 0,
        });

        // `LAYER == LAYER_LIT`, at every grid pixel centre.
        let mut emission = vec![0_i32; w * h];
        for row in 0..h {
            for col in 0..w {
                emission[row * w + col] = strip255(centre(col, row), &encoded, false);
            }
        }
        // `blur.frag`, twice.
        let tmp = blur_pass(&emission, (w, h), bloom.radius, (1, 0));
        let blurred = blur_pass(&tmp, (w, h), bloom.radius, (0, 1));

        // `LAYER == LAYER_BLIT`.
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                let p = centre(col, row);
                let mut under = palette.bg;
                if let Some(ghost) = palette.ghost {
                    let t = strip255(p, &encoded, true);
                    if t > 0 {
                        under = mix_kit(under, ghost, t);
                    }
                }
                let mut lit = strip255(p, &encoded, false);
                let glow = blurred[row * w + col];
                let halo = (glow * i32::from(bloom.strength) / 256).min(255);
                lit = lit.max(halo).min(255);
                if lit > 0 {
                    if let Some(mask) = palette.mask {
                        lit = lit * mask_keep(col, row, w, h, mask) / 256;
                    }
                    if lit > 0 {
                        under = mix_kit(under, palette.ink, lit);
                    }
                }
                out.extend_from_slice(&[under[0], under[1], under[2], 0xff]);
            }
        }
        out
    }

    /// `floor(gl_FragCoord.xy) + 0.5`, which on the snapped branch is also
    /// `vec2(float(col), float(row)) + 0.5`.
    #[allow(clippy::cast_precision_loss)]
    fn centre(col: usize, row: usize) -> (f32, f32) {
        (col as f32 + 0.5, row as f32 + 0.5)
    }

    /// `face_cov` — unreachable on the snapped branch, and transcribed anyway
    /// so the mirror is the whole function rather than the half this test
    /// drives. See the module docs on which branch a real screen takes.
    fn face_cov(r: f32, w: f32) -> f32 {
        (0.5 - r / w.max(1e-6)).clamp(0.0, 1.0)
    }

    /// `bar_residuals`.
    fn bar_residuals(along: f32, across: f32, hl: f32, hs: f32, tapered: bool) -> [f32; 3] {
        [
            across - hs,
            along - hl,
            if tapered {
                along + across - TAPER_EDGE - hl
            } else {
                NO_CHAMFER
            },
        ]
    }

    /// `TAPER_EDGE`, as the shipped GLSL declares it.
    const TAPER_EDGE: f32 = 0.5;
    /// `NO_CHAMFER`.
    const NO_CHAMFER: f32 = -1e9;

    /// `bar_inside`.
    fn bar_inside(r: [f32; 3]) -> bool {
        r[0].max(r[1]).max(r[2]) <= 0.0
    }

    /// `bar_cov` — see [`face_cov`] on why it is transcribed.
    fn bar_cov(r: [f32; 3], fl: f32, fs: f32) -> f32 {
        face_cov(r[0], fs)
            .min(face_cov(r[1], fl))
            .min(face_cov(r[2], fl + fs))
    }

    /// `bar255`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    fn bar255(p: (f32, f32), fp: (f32, f32), snapped: bool, bar: kit::SevenSegBar) -> i32 {
        let vertical = bar.vertical;
        let tapered = bar.tapered;
        let hs = kit::SEVEN_SEG_THICK as f32 * 0.5;
        let hl = bar.len as f32 * 0.5;
        let centre = if vertical {
            (bar.x as f32 + hs, bar.y as f32 + hl)
        } else {
            (bar.x as f32 + hl, bar.y as f32 + hs)
        };
        let d = ((p.0 - centre.0).abs(), (p.1 - centre.1).abs());
        let along = if vertical { d.1 } else { d.0 };
        let across = if vertical { d.0 } else { d.1 };
        let r = bar_residuals(along, across, hl, hs, tapered);
        if snapped {
            return i32::from(bar_inside(r)) * 255;
        }
        let (fl, fs) = if vertical { (fp.1, fp.0) } else { (fp.0, fp.1) };
        (255.0 * bar_cov(r, fl, fs) + 0.5) as i32
    }

    /// `cell255`.
    fn cell255(p: (f32, f32), fp: (f32, f32), snapped: bool, code: i32) -> i32 {
        if code & (1 << i32::from(COLON_BIT)) != 0 {
            return bar255(p, fp, snapped, kit::SEVEN_SEG_COLON_DOTS[0]).max(bar255(
                p,
                fp,
                snapped,
                kit::SEVEN_SEG_COLON_DOTS[1],
            ));
        }
        let mut best = 0;
        for (i, bar) in kit::SEVEN_SEG_BARS.into_iter().enumerate() {
            if code & (1 << i) != 0 {
                best = best.max(bar255(p, fp, snapped, bar));
            }
        }
        best
    }

    /// `cell_x`.
    fn cell_x(strip: &[f32], i: i32) -> f32 {
        let index = 2 * i;
        usize::try_from(index)
            .ok()
            .and_then(|index| strip.get(index).copied())
            .unwrap_or(0.0)
    }

    /// `cell_code`.
    #[allow(clippy::cast_possible_truncation)]
    fn cell_code(strip: &[f32], i: i32, ghost: bool) -> i32 {
        let index = 2 * i + 1;
        let Some(raw) = usize::try_from(index)
            .ok()
            .and_then(|index| strip.get(index).copied())
        else {
            return 0;
        };
        let code = (raw + 0.5) as i32;
        if ghost && code & (1 << i32::from(COLON_BIT)) == 0 {
            return (1 << kit::SEVEN_SEG_BARS.len()) - 1;
        }
        code
    }

    /// `CELL_SEARCH_STEPS`.
    const CELL_SEARCH_STEPS: i32 = 16;

    /// `cell_at`.
    fn cell_at(strip: &[f32], cells: i32, x: f32) -> i32 {
        let mut lo = 0;
        let mut hi = cells - 1;
        let mut s = 0;
        while s < CELL_SEARCH_STEPS && lo < hi {
            let mid = (lo + hi + 1) / 2;
            if cell_x(strip, mid) <= x {
                lo = mid;
            } else {
                hi = mid - 1;
            }
            s += 1;
        }
        lo
    }

    /// `strip255` — at 1:1 the footprint is exactly one buffer pixel, and
    /// `snapped` is always true on the branch this mirror covers.
    #[allow(clippy::cast_precision_loss)]
    fn strip255(p: (f32, f32), encoded: &super::Readout, ghost: bool) -> i32 {
        let cells = i32::try_from(encoded.cells).unwrap_or(i32::MAX);
        if cells <= 0 {
            return 0;
        }
        let strip = encoded.strip.as_deref().unwrap_or(&[]);
        let first = cell_at(strip, cells, p.0);
        let mut best = 0;
        for k in 0..2 {
            let i = first + k;
            if i >= cells {
                break;
            }
            let local = (p.0 - cell_x(strip, i), p.1 - kit::SEVEN_SEG_PAD as f32);
            best = best.max(cell255(local, (1.0, 1.0), true, cell_code(strip, i, ghost)));
        }
        best
    }

    /// `blur.frag`, one direction: the kit's own clipped-window, unrenormalised,
    /// truncating integer box blur.
    fn blur_pass(src: &[i32], size: (usize, usize), radius: usize, dir: (i32, i32)) -> Vec<i32> {
        let (w, h) = size;
        let index = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
        let radius = index(radius);
        let window = 2 * radius + 1;
        let mut out = vec![0_i32; src.len()];
        for row in 0..h {
            for col in 0..w {
                let mut sum = 0;
                for d in -radius..=radius {
                    // `ivec2 q = p + BLUR_DIR * d;`, then the shader's own
                    // four-way bounds test — the window clips at the buffer and
                    // the divisor below stays the full window, which is what
                    // dims the kit's edges.
                    let q = (index(col) + dir.0 * d, index(row) + dir.1 * d);
                    if q.0 < 0 || q.1 < 0 || q.0 >= index(w) || q.1 >= index(h) {
                        continue;
                    }
                    let (qx, qy) = (
                        usize::try_from(q.0).unwrap_or(0),
                        usize::try_from(q.1).unwrap_or(0),
                    );
                    sum += src[qy * w + qx];
                }
                out[row * w + col] = sum / window;
            }
        }
        out
    }

    /// `mix_kit`.
    ///
    /// `many_single_char_names` is allowed here and in [`mask_keep`] on
    /// purpose: the names *are* the shader's, which is what lets a reviewer
    /// read the two side by side and see a transcription rather than a
    /// paraphrase.
    #[allow(clippy::many_single_char_names)]
    fn mix_kit(a: kit::Rgba, b: kit::Rgba, t: i32) -> kit::Rgba {
        let k = t.clamp(0, 255);
        let mut out = [0_u8; 4];
        for (o, (&av, &bv)) in out.iter_mut().zip(a.iter().zip(&b)) {
            let v = (i32::from(av) * (255 - k) + i32::from(bv) * k + 127) / 255;
            *o = u8::try_from(v).unwrap_or(u8::MAX);
        }
        out
    }

    /// `centred`.
    fn centred(i: usize, n: usize) -> i32 {
        if n == 0 {
            return 0;
        }
        let (i, n) = (
            i32::try_from(i).unwrap_or(i32::MAX),
            i32::try_from(n).unwrap_or(i32::MAX),
        );
        ((2 * i + 1 - n) * 1024) / n
    }

    /// `isqrt`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn isqrt(n: i32) -> i32 {
        if n <= 0 {
            return 0;
        }
        let mut s = (n as f32).sqrt() as i32;
        if (s + 1) * (s + 1) <= n {
            s += 1;
        }
        if s * s > n {
            s -= 1;
        }
        s
    }

    /// `mask_keep` — see [`mix_kit`] on the single-character names.
    #[allow(clippy::many_single_char_names)]
    fn mask_keep(x: usize, y: usize, w: usize, h: usize, mask: kit::MaskSnapshot) -> i32 {
        let index = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
        let mask_one = i32::try_from(kit::MASK_ONE).unwrap();
        let coord_one = i32::try_from(kit::COORD_ONE).unwrap();
        let short = w.min(h);
        let band = index(short / kit::BAND_DIV);
        let radius = index(short / kit::CORNER_DIV);

        let u = centred(x, w);
        let v = centred(y, h);
        // The shader spells this `(u * u + v * v) / 2`; both terms are squares
        // and so non-negative, where `midpoint` is that same truncating half.
        let r2 = i32::midpoint(u * u, v * v);
        let depth = mask_one - i32::try_from(mask.corner_keep).unwrap();
        let radial = (mask_one - (depth * r2) / (coord_one * coord_one)).clamp(0, mask_one);

        let mut edge = mask_one;
        if band > 0 {
            let ex = index(x.min(w - 1 - x));
            let ey = index(y.min(h - 1 - y));
            let d = if ex < radius && ey < radius {
                (radius - isqrt((radius - ex) * (radius - ex) + (radius - ey) * (radius - ey)))
                    .max(0)
            } else {
                ex.min(ey)
            };
            edge = mask_one * d.min(band) / band;
        }

        let mut comb = mask_one;
        if mask.pitch != 0 && y % mask.pitch == mask.phase {
            comb = i32::try_from(mask.scanline_keep).unwrap();
        }
        radial * edge / mask_one * comb / mask_one
    }
}
