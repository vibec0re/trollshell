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
        Arc, COLON_BIT, GlBlend, GlDraw, GlInput, GlTarget, GlUniforms, GlValue, SEVEN_SEG_PIPELINE,
        kit, readout, seven_seg_surface,
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
            assert!((origin - cell.x as f32).abs() < f32::EPSILON, "cell {i}");
            match cell.mask {
                Some(mask) => {
                    assert!(
                        (code - f32::from(mask)).abs() < f32::EPSILON,
                        "cell {i} is a digit",
                    );
                    assert!(
                        (code as u32) & (1 << u32::from(COLON_BIT)) == 0,
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
        assert_eq!(SEVEN_SEG_PIPELINE.frame.len(), 4, "lit, blur H, blur V, blit");
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
            BODY.contains(&format!("const int SEG_COUNT = {};", kit::SEVEN_SEG_BARS.len())),
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
                    for text in ["", " ", "8", "88", "12:34", "-", "9876543210", "x?", "07:16"] {
                        let reference = kit::seven_seg(text, style);
                        let mirror = shader_frame(style, text);
                        assert_eq!(mirror, reference.data(), "{style:?} {text:?}");
                    }
                }
            },
        );
    }

    /// **The transcription above is the arithmetic the shipped shaders carry**
    /// — a source scan over `seven_seg.frag` and `blur.frag`, so a fix applied
    /// to one side only reds here instead of leaving a green mirror describing
    /// a shader nobody ships.
    ///
    /// **Every clause [`shader_frame`] transcribes is listed**, which is the
    /// #1293 item-1 correction to #1153's version of this test: that one pinned
    /// six clauses of a mirror that transcribed rather more, so an edit to an
    /// unpinned line left the mirror green and wrong.
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
            "return (1 << SEG_COUNT) - 1;",
            "int mid = (lo + hi + 1) / 2;",
            "if (cell_x(mid) <= x) {",
            "vec2 local = p - vec2(cell_x(i), float(u_pad));",
            // the composite
            "return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);",
            "return (a * (255 - k) + b * k + 127) / 255;",
            "int halo = min(glow * u_bloom_strength / 256, 255);",
            "lit = min(max(lit, halo), 255);",
            "lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;",
            "under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);",
            "under = mix_kit(under, ink, lit);",
            // the CRT pass
            "int r2 = (u * u + v * v) / 2;",
            "int radial = clamp(MASK_ONE - (depth * r2) / (COORD_ONE * COORD_ONE), 0, MASK_ONE);",
            "edge = MASK_ONE * d / band;",
            "return radial * edge / MASK_ONE * comb / MASK_ONE;",
            "return ((2 * i + 1 - n) * COORD_ONE) / n;",
            // the lit pass's sample point
            "vec2 p = floor(gl_FragCoord.xy) + 0.5;",
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
            return bar255(p, fp, snapped, kit::SEVEN_SEG_COLON_DOTS[0])
                .max(bar255(p, fp, snapped, kit::SEVEN_SEG_COLON_DOTS[1]));
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
            let local = (
                p.0 - cell_x(strip, i),
                p.1 - kit::SEVEN_SEG_PAD as f32,
            );
            best = best.max(cell255(local, (1.0, 1.0), true, cell_code(strip, i, ghost)));
        }
        best
    }

    /// `blur.frag`, one direction: the kit's own clipped-window, unrenormalised,
    /// truncating integer box blur.
    fn blur_pass(src: &[i32], size: (usize, usize), radius: usize, dir: (isize, isize)) -> Vec<i32> {
        let (w, h) = size;
        let radius = i32::try_from(radius).unwrap_or(0);
        let window = 2 * radius + 1;
        let mut out = vec![0_i32; src.len()];
        for row in 0..h {
            for col in 0..w {
                let mut sum = 0;
                for d in -radius..=radius {
                    let qx = isize::try_from(col).unwrap_or(0) + dir.0 * d as isize;
                    let qy = isize::try_from(row).unwrap_or(0) + dir.1 * d as isize;
                    if qx < 0
                        || qy < 0
                        || qx >= isize::try_from(w).unwrap_or(0)
                        || qy >= isize::try_from(h).unwrap_or(0)
                    {
                        continue;
                    }
                    sum += src[qy as usize * w + qx as usize];
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
