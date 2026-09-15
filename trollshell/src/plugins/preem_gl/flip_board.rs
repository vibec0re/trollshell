//! The `FlipBoard` GL program (#1155): the pipeline declaration, the three
//! layer splices, the per-cell strip and the pure
//! `(board, palette) → GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge),
//! [`dot_matrix`](super::dot_matrix), [`marquee`](super::marquee),
//! [`textbox`](super::textbox), [`led_strip`](super::led_strip) and
//! [`seven_seg`](super::seven_seg) in every structural way — it references
//! nothing above itself either, so the parity harness
//! (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this** file
//! and measures the shell's own pipeline and mapping against the CPU kit.
//!
//! # What is better than the kit
//!
//! The kit rasterises into a **logical** buffer and replicates it
//! [`scale`](kit::FlipBoard::scale) times on the way out
//! (`hytte-preem/src/split_flap.rs`'s `render` ends in `Frame::upscale`), so
//! this arm's grid is the **pre-upscale** buffer and the natural size is
//! `grid × scale` — the [`Scope`](super::program)'s shape, not the
//! [`Gauge`](super::gauge)'s. `scale` defaults to `2`, so a shipping board
//! always draws through `flip_board.frag`'s continuous branch.
//!
//! What that buys is the **fold**. A rotation is not a translation, so the kit
//! resamples the falling card: a destination row takes the exact
//! coverage-weighted average of the source rows it now spans, and a partly
//! covered row blends the card over what is behind it. The kit takes that
//! integral over a *logical* row and then replicates the answer, so the flap's
//! lit free edge and the fold's own boundary come out as `scale`-tall bands of
//! one stair. Here the same integral is taken over the fragment's own vertical
//! footprint, so both land where the screen can draw them.
//!
//! Everything else is deliberately **not** improved, and each for a stated
//! reason:
//!
//! * the **fixture** (the card faces, the nixie's cathode stack, the bezel, the
//!   hinge slot) is the kit's own physical furniture and is byte-identical in
//!   every frame of a flip — the kit's
//!   `the_fixture_never_moves_while_the_cards_do` asserts exactly that — so it
//!   is sampled at the fragment's logical pixel, which is what `Frame::upscale`
//!   replicates;
//! * the **glyphs** are 5×7 bitmap pixels and square is the look, so the
//!   resting halves are floored to a logical pixel too ([`textbox`]'s rule:
//!   bigger hard-edged pixels, never a smoothed font);
//! * a **nixie** has no sub-pixel geometry at all — it is a cross-fade between
//!   two bitmap cathodes — so its emission is a point test on both branches and
//!   its improvement is the halo, read bilinearly at the fragment's resolution
//!   off the snap (#1186's tap) rather than replicated out of the kit's grid.
//!
//! [`textbox`]: super::textbox
//!
//! # Seven passes, because a tube glows twice
//!
//! `FlipBoard::render` blooms a **nixie** twice: a wide weak pass at
//! `radius + NIXIE_HALO_RADIUS_BONUS` / strength [`kit::NIXIE_HALO_STRENGTH`],
//! then the palette's own on top. `Emission::bloom` max-combines into the
//! emission, and the second pass blurs the *result* of the first, so the two
//! cannot be folded into one. Hence [`HALO1_FRAG`]: an offscreen pass that
//! recombines the wide haze at the grid, whose output the second blur reads.
//!
//! A split-flap board blooms once and a bloom-free skin not at all, and both
//! fall out of the same seven passes rather than a second pipeline — a radius
//! of `0` makes the blur the identity and a strength of `0` makes the combine
//! `max(v, 0)`, exactly as the kit's early return does.
//!
//! That is also why `blur.frag` grew a `BLUR_RADIUS` splice (#1155): the
//! uniform bag is one bag applied to every pass, so a pipeline that blurs at
//! two radii cannot say "this pass is the wide one" any more than it can say
//! "this pass is the horizontal one". [`HALO_BLUR_H_FRAG`] declares
//! `u_halo_radius` and points the macro at it; every pre-existing splice
//! defines nothing and keeps `u_bloom_radius`.
//!
//! # Nothing geometric is a GLSL literal, and nothing is re-derived here
//!
//! Every length comes off [`kit::FlipBoard::metrics`] and every per-cell number
//! off [`kit::FlipBoard::cell_states`] through the kit's own
//! [`kit::flap_theta`] / [`kit::nixie_ignite`] / [`kit::nixie_afterglow`] —
//! all `pub` since #1155 on the [`kit::Dial`] / `SEVEN_SEG_BARS` precedent
//! (#1148/#1154). The card metrics, the fixture tones and the emission's
//! ceiling are the kit's constants, read by name. The only numbers restated
//! anywhere on this side are `font`'s 5×7 glyph box (held to the kit by
//! [`tests::the_shader_restates_only_the_fonts_glyph_box`]) and the CRT pass's
//! four fixed-point constants, which every shader on this seam restates and
//! [`program::assert_crt_constants`](super::program::assert_crt_constants)
//! holds to the kit's items.
//!
//! # The strip is rebuilt every mapping pass, and that is not #911's case
//!
//! [`seven_seg`](super::seven_seg)'s readout and
//! [`dot_matrix`](super::dot_matrix)'s glyph strip are encoded once per *state
//! change* and cloned as an `Arc` on every pass, because their content is a
//! function of the text alone. A flip board's is not: the band, the shading and
//! the free edge move with the clock, so a cached strip would be a stale frame.
//! [`cards`] therefore walks the row on each mapping pass — 14 texels per cell,
//! at most [`kit::FLIP_MAX_CELLS`] of them, which is 896 `f32` on the widest
//! board the kit will build.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_preem as kit;

use super::program::{BLUR_H_FRAG, BLUR_V_FRAG, FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `FlipBoard` pipeline.
pub(crate) const FLIP_BOARD: GlProgram = GlProgram("preem.flip_board");

/// The three layers of the board, which are the same body with the layer
/// prepended.
///
/// The `blur.frag` trick, for the same reason and with the same payoff as
/// `dot_matrix.frag`'s and `seven_seg.frag`'s: `GlUniforms` is one bag applied
/// to every pass, so there is nowhere to say "this pass is the lit one" — and
/// all three layers need the strip, the row geometry and the glyph lattice
/// under it. A hand-copied second fold would drift, and a drift between *these*
/// draws a halo that does not sit on the card it came from.
const LIT_FRAG: &str = concat!("const int LAYER = 0;\n", include_str!("flip_board.frag"));
const HALO1_FRAG: &str = concat!("const int LAYER = 1;\n", include_str!("flip_board.frag"));
const BLIT_FRAG: &str = concat!("const int LAYER = 2;\n", include_str!("flip_board.frag"));

/// The **wide** half of the nixie's double bloom: `blur.frag` again, pointed at
/// a second radius uniform this splice also declares.
///
/// See the module docs — the bag is per surface, not per pass, so two radii in
/// one pipeline need two splices of one body rather than a second copy of a
/// truncating integer blur.
const HALO_BLUR_H_FRAG: &str = concat!(
    "const ivec2 BLUR_DIR = ivec2(1, 0);\n",
    "uniform int u_halo_radius;\n",
    "#define BLUR_RADIUS u_halo_radius\n",
    include_str!("blur.frag")
);
const HALO_BLUR_V_FRAG: &str = concat!(
    "const ivec2 BLUR_DIR = ivec2(0, 1);\n",
    "uniform int u_halo_radius;\n",
    "#define BLUR_RADIUS u_halo_radius\n",
    include_str!("blur.frag")
);

/// The `FlipBoard` pipeline. See the module docs for why it is seven passes and
/// not the readout's four.
pub(crate) const FLIP_BOARD_PIPELINE: GlPipeline = GlPipeline {
    // The lit layer into 0; the wide blur's halves into 1 and 2; the
    // recombined layer into 3; the skin's own blur's halves into 4 and 5.
    aux: 6,
    // No cross-frame GPU state: a board's whole animation is a closed-form
    // function of its clock, and the clock is CPU-side on both arms. `step_seq`
    // stays at `0` and this list stays empty.
    step: &[],
    frame: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: LIT_FRAG,
            target: GlTarget::Aux(0),
            inputs: &[GlInput::Data],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: HALO_BLUR_H_FRAG,
            target: GlTarget::Aux(1),
            inputs: &[GlInput::Aux(0)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: HALO_BLUR_V_FRAG,
            target: GlTarget::Aux(2),
            inputs: &[GlInput::Aux(1)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: HALO1_FRAG,
            target: GlTarget::Aux(3),
            inputs: &[GlInput::Aux(0), GlInput::Aux(2)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_H_FRAG,
            target: GlTarget::Aux(4),
            inputs: &[GlInput::Aux(3)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_V_FRAG,
            target: GlTarget::Aux(5),
            inputs: &[GlInput::Aux(4)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLIT_FRAG,
            target: GlTarget::Screen,
            // The lit layer is **recomputed**, not read back — that is the
            // whole of the continuous branch. Only the two blurred copies are
            // sampled.
            inputs: &[GlInput::Data, GlInput::Aux(2), GlInput::Aux(5)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
    ],
};

/// Texels per cell in the strip — `flip_board.frag` declares the same number as
/// `CELL_TEXELS`, and
/// [`tests::the_shader_and_the_mapping_agree_about_the_strip_encoding`] reads
/// it back out of the GLSL.
const CELL_TEXELS: usize = 14;

/// `Mechanism::SplitFlap` as the shader's `MECH_SPLIT_FLAP`.
const MECH_SPLIT_FLAP: i32 = 0;
/// `Mechanism::Nixie` as the shader's `MECH_NIXIE`.
const MECH_NIXIE: i32 = 1;

/// One board as the shader consumes it: the kit's own metrics, its mechanism,
/// and one strip row per cell.
///
/// Built **per mapping pass** rather than per state change — see the module
/// docs on why #911's rule does not apply to a widget whose content moves with
/// the clock.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Cards {
    /// The kit's own layout, read rather than re-derived.
    pub(crate) metrics: kit::FlipMetrics,
    /// Which mechanism drives the row.
    pub(crate) mechanism: kit::Mechanism,
    /// [`CELL_TEXELS`] texels per cell, in cell order. `None` for a board with
    /// no cells at all, which is what `GlUniforms::data` wants there (it binds
    /// a 1×1 zero texture and sets `u_data_len` to `0`).
    pub(crate) strip: Option<Arc<[f32]>>,
}

/// Encode `board` for the shader — the pure half of the mapping.
///
/// Everything per-cell is resolved **here**, on the CPU, through the kit's own
/// curves: the fold's band from [`kit::flap_theta`], the lambert-ish shading
/// from [`kit::FLIP_SHADE_FLOOR`], the free edge's rule from
/// [`kit::FLIP_EDGE_PX`]/[`kit::FLIP_EDGE_T`], and a nixie's two cathode levels
/// from [`kit::nixie_afterglow`]/[`kit::nixie_ignite`] through the kit's own
/// [`kit::flip_level`]. The shader restates none of it.
pub(crate) fn cards(board: &kit::FlipBoard) -> Cards {
    let metrics = board.metrics();
    let mid = fx(metrics.hinge);
    let mut strip = Vec::with_capacity(metrics.cells * CELL_TEXELS);
    for cell in board.cell_states() {
        // `FlipBoard::compose_flap`'s hoisted half, verbatim.
        let theta = kit::flap_theta(cell.progress);
        let (sin, cos) = theta.sin_cos();
        let squash = cos.abs();
        let falling_up = cos >= 0.0;
        let (band_lo, band_hi) = if falling_up {
            (mid - squash * mid, mid)
        } else {
            (mid, mid + squash * mid)
        };
        let shade = kit::FLIP_SHADE_FLOOR + (1.0 - kit::FLIP_SHADE_FLOOR) * squash;
        let edge_peak = kit::FLIP_EDGE_T * sin;
        let (edge_lo, edge_hi) = if falling_up {
            (band_lo, band_lo + kit::FLIP_EDGE_PX)
        } else {
            (band_hi - kit::FLIP_EDGE_PX, band_hi)
        };
        // …and `compose_nixie`'s.
        let out_level = kit::flip_level(kit::nixie_afterglow(cell.progress) * kit::FLIP_GLYPH_T);
        let in_level = kit::flip_level(kit::nixie_ignite(cell.progress) * kit::FLIP_GLYPH_T);

        let (from_lo, from_hi) = pack_glyph(kit::flip_rows_of(cell.from));
        let (to_lo, to_hi) = pack_glyph(kit::flip_rows_of(cell.to));
        strip.extend_from_slice(&[
            f32_of_bits(from_lo),
            f32_of_bits(from_hi),
            f32_of_bits(to_lo),
            f32_of_bits(to_hi),
            band_lo,
            band_hi,
            edge_lo,
            edge_hi,
            edge_peak,
            shade,
            squash,
            if falling_up { 1.0 } else { 0.0 },
            f32::from(out_level),
            f32::from(in_level),
        ]);
    }
    Cards {
        metrics,
        mechanism: board.mechanism(),
        strip: (!strip.is_empty()).then(|| Arc::from(strip.into_boxed_slice())),
    }
}

/// Map one already-encoded board onto the GL node payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface) and
/// its six siblings are: it reads no globals, resolves no palette and touches
/// no GL. The caller passes the palette it has already resolved *inside the
/// widget's `with_pins` scope*, so accent / role / pin precedence stays the
/// kit's one implementation.
pub(crate) fn flip_board_surface(cards: &Cards, palette: &kit::PaletteSnapshot) -> KitSurface {
    // A skin with no bloom reaches the shaders as radius 0 / strength 0, which
    // makes both blurs the identity and both combines a no-op — see the module
    // docs and `program`'s on why the passes run unconditionally.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    // …and the nixie's extra wide haze, which a split-flap board never has.
    let (halo_radius, halo_strength) = match (cards.mechanism, palette.bloom) {
        (kit::Mechanism::Nixie, Some(bloom)) => (
            bloom.radius + kit::NIXIE_HALO_RADIUS_BONUS,
            kit::NIXIE_HALO_STRENGTH,
        ),
        _ => (0, 0),
    };
    let mask = palette.mask;
    let metrics = cards.metrics;
    let (stack_lo, stack_hi) = pack_glyph(kit::nixie_cathode_stack());

    KitSurface {
        // `Frame::upscale`'s output — the buffer the kit hands the reconciler,
        // which is `scale` times the grid the passes above run at.
        width: u32_of(metrics.width.saturating_mul(metrics.scale)),
        height: u32_of(metrics.height.saturating_mul(metrics.scale)),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                ("u_mechanism", GlValue::Int(mechanism_code(cards.mechanism))),
                ("u_cells", GlValue::Int(int_of(metrics.cells))),
                ("u_cell_w", GlValue::Int(int_of(metrics.cell_w))),
                ("u_cell_h", GlValue::Int(int_of(metrics.cell_h))),
                ("u_hinge", GlValue::Int(int_of(metrics.hinge))),
                ("u_bezel", GlValue::Int(int_of(metrics.bezel))),
                ("u_gap", GlValue::Int(int_of(metrics.gap))),
                ("u_glyph_px", GlValue::Int(int_of(metrics.glyph_px))),
                ("u_glyph_pad", GlValue::Int(int_of(metrics.glyph_pad))),
                // The fixture's three tones, as the kit's own constants rather
                // than as the numbers they currently equal.
                ("u_face_top", GlValue::Int(i32::from(kit::FLIP_FACE_TOP_T))),
                (
                    "u_face_bottom",
                    GlValue::Int(i32::from(kit::FLIP_FACE_BOTTOM_T)),
                ),
                ("u_cathode", GlValue::Int(i32::from(kit::NIXIE_CATHODE_T))),
                // …and the unlit cathode stack, derived from the font by the
                // kit rather than drawn here.
                ("u_stack_lo", GlValue::Int(i32_of_bits(stack_lo))),
                ("u_stack_hi", GlValue::Int(i32_of_bits(stack_hi))),
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
                ("u_halo_radius", GlValue::Int(int_of(halo_radius))),
                ("u_halo_strength", GlValue::Int(i32::from(halo_strength))),
                ("u_bg", channels(palette.bg)),
                ("u_ink", channels(palette.ink)),
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                // **Not re-phased**, like the LED strip and the readout and
                // unlike the two dot surfaces (#1091): this widget has no dot
                // grid for the comb to sit in the seams of, so it keeps
                // `Mask::CRT` exactly as the skin states it — which is what the
                // kit's own `composite` call hands `Emission::composite` here.
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
            data: cards.strip.clone(),
            // **The pre-upscale buffer**, which is what the kit rasterises into
            // before `Frame::upscale` replicates it — the scope's situation and
            // not the gauge's (#1143), because here the upscale replicates
            // *finished* pixels rather than magnifying a rasterisation.
            grid: (u32_of(metrics.width), u32_of(metrics.height)),
            // No step passes, so nothing counts steps. See
            // [`FLIP_BOARD_PIPELINE`].
            step_seq: 0,
        },
    }
}

/// One glyph's seven 5-bit font rows as two packed words: rows `0..4` in the
/// first, `4..7` in the second, five bits each, low row first.
///
/// Two words rather than one because seven rows are 35 bits and both an `f32`
/// texel and an `int` uniform carry 24 exactly; two rather than seven because
/// 20 and 15 bits both are. The kit indexes a row's bits from the *left*
/// (`GLYPH_W - 1 - col`) and so does the shader, so only the row packing is
/// this side's invention.
fn pack_glyph(rows: [u8; kit::font::GLYPH_H]) -> (u32, u32) {
    let mut lo = 0_u32;
    let mut hi = 0_u32;
    for (index, bits) in rows.into_iter().enumerate() {
        // Bits above the glyph's own width are not addressable by the kit's
        // `>> (GLYPH_W - 1 - col)` for any `col`, so they are dropped rather
        // than packed into the next row's field.
        let bits = u32::from(bits) & ((1 << kit::font::GLYPH_W) - 1);
        if index < 4 {
            lo |= bits << (index * kit::font::GLYPH_W);
        } else {
            hi |= bits << ((index - 4) * kit::font::GLYPH_W);
        }
    }
    (lo, hi)
}

/// A packed glyph word as the `f32` a strip texel carries. Bounded by `2^20`,
/// far inside `f32`'s exact integer range.
#[allow(clippy::cast_precision_loss)]
fn f32_of_bits(value: u32) -> f32 {
    value as f32
}

/// A packed glyph word as the `int` a uniform carries — the cathode stack's
/// two, which are uniforms rather than strip texels because the stack is the
/// same figure in every cell.
fn i32_of_bits(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// A small buffer coordinate or count as an exact `f32` — `hytte-preem`'s own
/// `fx`, which is what makes the band arithmetic here identical to the kit's.
fn fx(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// The mechanism as the shader's code.
fn mechanism_code(mechanism: kit::Mechanism) -> i32 {
    match mechanism {
        kit::Mechanism::SplitFlap => MECH_SPLIT_FLAP,
        kit::Mechanism::Nixie => MECH_NIXIE,
    }
}

/// A buffer dimension as a `u32`, saturating.
fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// A count as the `int` a uniform carries, saturating.
fn int_of(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}
