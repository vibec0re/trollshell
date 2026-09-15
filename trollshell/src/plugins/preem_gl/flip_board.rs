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
//! * the **halo** is read with a plain `texelFetch` at the fragment's logical
//!   pixel on *both* branches. `dot_matrix.frag` and `seven_seg.frag` take
//!   #1186's bilinear tap instead, and this one deliberately does not: their
//!   grid is already the buffer a chip is drawn at, while this one is the
//!   pre-upscale buffer and `Frame::upscale` replicates a *finished* halo, so
//!   reading it bilinearly would invent a glow the kit never draws. It is also
//!   what keeps the supersampled standard meetable — see `flip_board.frag`'s
//!   note above `mix_kit` for the `interior_max == 4` that measured it.
//!
//! **A nixie therefore has no improvement at all, and that is the honest
//! statement rather than an omission.** A tube is a cross-fade between two
//! bitmap cathodes: no geometry moves, so every one of the four things above is
//! point-sampled for it and its frame is byte-identical to the kit's at every
//! scale. This arm draws the *flap mechanism* at native resolution, and a tube
//! has no mechanism to draw. What it does get is the same pipeline — including
//! the kit's own double bloom — so the two mechanisms keep one implementation
//! instead of the board falling back to the CPU whenever a plugin picks the
//! tube.
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

#[cfg(test)]
mod tests {
    use super::{
        Arc, CELL_TEXELS, FLIP_BOARD_PIPELINE, GlBlend, GlDraw, GlInput, GlTarget, GlUniforms,
        GlValue, HALO_BLUR_H_FRAG, HALO_BLUR_V_FRAG, MECH_NIXIE, MECH_SPLIT_FLAP, cards,
        flip_board_surface, kit, pack_glyph,
    };

    /// The shader body — the source the scans below read back.
    const BODY: &str = include_str!("flip_board.frag");
    /// …and the blur's, which the mirror transcribes four times over.
    const BLUR_BODY: &str = include_str!("blur.frag");

    fn uniform(uniforms: &GlUniforms, name: &str) -> GlValue {
        let (_, value) = uniforms
            .values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("no uniform named {name}"));
        *value
    }

    fn int_at(uniforms: &GlUniforms, name: &str) -> i32 {
        match uniform(uniforms, name) {
            GlValue::Int(value) => value,
            other => panic!("{name} is {other:?}, wanted an Int"),
        }
    }

    /// A board driven into one state: settled on `from`, told to show `to`,
    /// then advanced by `secs`. The kit's builder chain in its own order —
    /// `cells` rebuilds the row blank, so it comes first.
    fn board(
        mechanism: kit::Mechanism,
        cells: usize,
        from: &str,
        to: &str,
        secs: f32,
        scale: usize,
    ) -> kit::FlipBoard {
        let mut board = kit::FlipBoard::new(mechanism).cells(cells).scale(scale);
        board.set_text(from);
        board.settle();
        board.set_text(to);
        board.advance(secs);
        board
    }

    // ── the mapping ─────────────────────────────────────────────────────────

    /// **The uniform table is the kit's own metrics**, name for name and value
    /// for value — and every geometric row is written as the kit item it comes
    /// from, never as the literal it currently equals (the #1164 shape).
    ///
    /// That is the whole reason [`kit::FlipBoard::metrics`] and the card
    /// constants became `pub`: a `GlValue::Int(14)` here would agree with the
    /// kit today and keep agreeing after someone widened a card, which is a
    /// mirror agreeing with itself. The names are the contract with the GLSL —
    /// a rename on one side alone draws nothing and says nothing — and the
    /// order is pinned so a reordering shows as a diff.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row, or by
    /// spelling a metric as a literal and then moving the kit's.
    #[test]
    fn the_uniform_table_is_the_kits_own_metrics() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
        let kit_board = board(kit::Mechanism::SplitFlap, 5, "00:00", "12:34", 0.11, 2);
        let metrics = kit_board.metrics();
        let surface = flip_board_surface(&cards(&kit_board), &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_mechanism",
                "u_cells",
                "u_cell_w",
                "u_cell_h",
                "u_hinge",
                "u_bezel",
                "u_gap",
                "u_glyph_px",
                "u_glyph_pad",
                "u_face_top",
                "u_face_bottom",
                "u_cathode",
                "u_stack_lo",
                "u_stack_hi",
                "u_ghost_on",
                "u_ghost",
                "u_bloom_radius",
                "u_bloom_strength",
                "u_halo_radius",
                "u_halo_strength",
                "u_bg",
                "u_ink",
                "u_mask_on",
                "u_mask_pitch",
                "u_mask_phase",
                "u_scanline_keep",
                "u_corner_keep",
            ],
        );

        assert_eq!(int_at(&surface.uniforms, "u_mechanism"), MECH_SPLIT_FLAP);
        // The eight geometric rows, each against the kit's own answer.
        for (name, value) in [
            ("u_cells", metrics.cells),
            ("u_cell_w", metrics.cell_w),
            ("u_cell_h", metrics.cell_h),
            ("u_hinge", metrics.hinge),
            ("u_bezel", metrics.bezel),
            ("u_gap", metrics.gap),
            ("u_glyph_px", metrics.glyph_px),
            ("u_glyph_pad", metrics.glyph_pad),
        ] {
            assert_eq!(
                int_at(&surface.uniforms, name),
                i32::try_from(value).unwrap(),
                "{name}",
            );
        }
        // …and the three fixture tones, as the kit's constants rather than as
        // 255, 205 and 255.
        assert_eq!(
            int_at(&surface.uniforms, "u_face_top"),
            i32::from(kit::FLIP_FACE_TOP_T),
        );
        assert_eq!(
            int_at(&surface.uniforms, "u_face_bottom"),
            i32::from(kit::FLIP_FACE_BOTTOM_T),
        );
        assert_eq!(
            int_at(&surface.uniforms, "u_cathode"),
            i32::from(kit::NIXIE_CATHODE_T),
        );
        // …and the cathode stack, derived by the kit from the font.
        let (stack_lo, stack_hi) = pack_glyph(kit::nixie_cathode_stack());
        assert_eq!(
            int_at(&surface.uniforms, "u_stack_lo"),
            i32::try_from(stack_lo).unwrap(),
        );
        assert_eq!(
            int_at(&surface.uniforms, "u_stack_hi"),
            i32::try_from(stack_hi).unwrap(),
        );

        assert_eq!(
            int_at(&surface.uniforms, "u_ghost_on"),
            i32::from(palette.ghost.is_some()),
        );
        let bloom = palette.bloom.expect("the CRT skin glows");
        assert_eq!(
            int_at(&surface.uniforms, "u_bloom_radius"),
            i32::try_from(bloom.radius).unwrap(),
        );
        assert_eq!(
            int_at(&surface.uniforms, "u_bloom_strength"),
            i32::from(bloom.strength),
        );
        // A split-flap board blooms **once**, so the wide pass is off.
        assert_eq!(int_at(&surface.uniforms, "u_halo_radius"), 0);
        assert_eq!(int_at(&surface.uniforms, "u_halo_strength"), 0);
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

    /// **A nixie's wide halo is the kit's own extra bloom pass**, and a split
    /// flap has none at all.
    ///
    /// The kit's `render` blooms a nixie at
    /// `bloom.radius + NIXIE_HALO_RADIUS_BONUS` / strength
    /// [`kit::NIXIE_HALO_STRENGTH`] *before* the palette's own; this is the
    /// mapping's half of that, and it is written as the kit's two constants
    /// rather than as `+ 3` and `64`.
    ///
    /// **Falsified** by giving the split flap a wide pass, by dropping the
    /// bonus, or by leaving the wide pass on for a skin whose `bloom` is
    /// `None` (where the kit would not run the first `bloom` either).
    #[test]
    fn only_a_nixie_takes_the_kits_extra_wide_halo() {
        for style in kit::DisplayStyle::ALL {
            let palette = kit::palette_snapshot(style);
            for mechanism in kit::Mechanism::ALL {
                let kit_board = board(mechanism, 4, "0000", "1234", 0.09, 1);
                let surface = flip_board_surface(&cards(&kit_board), &palette);
                let wide = match (mechanism, palette.bloom) {
                    (kit::Mechanism::Nixie, Some(bloom)) => (
                        i32::try_from(bloom.radius + kit::NIXIE_HALO_RADIUS_BONUS).unwrap(),
                        i32::from(kit::NIXIE_HALO_STRENGTH),
                    ),
                    _ => (0, 0),
                };
                assert_eq!(
                    (
                        int_at(&surface.uniforms, "u_halo_radius"),
                        int_at(&surface.uniforms, "u_halo_strength"),
                    ),
                    wide,
                    "{style:?}/{mechanism:?}",
                );
            }
        }
    }

    /// **The grid is the kit's pre-upscale buffer and the natural size is that
    /// times `scale`** — the scope's arrangement, not the gauge's, because here
    /// `Frame::upscale` replicates *finished* pixels rather than magnifying a
    /// rasterisation.
    ///
    /// **Falsified** by handing `grid` the post-upscale size (the offscreen
    /// passes then blur at the wrong radius and the CRT comb comes out at the
    /// wrong pitch), or by handing the reconciler the pre-upscale one (the chip
    /// is drawn at a `scale`th of its natural size).
    #[test]
    fn the_grid_is_the_kits_pre_upscale_buffer() {
        for scale in [1_usize, 2, 3, 8] {
            for mechanism in kit::Mechanism::ALL {
                let kit_board = board(mechanism, 5, "00:00", "12:34", 0.11, scale);
                let metrics = kit_board.metrics();
                let surface = flip_board_surface(
                    &cards(&kit_board),
                    &kit::palette_snapshot(kit::DisplayStyle::Vfd),
                );
                let frame = kit_board.render(kit::DisplayStyle::Vfd);
                assert_eq!(
                    (surface.width as usize, surface.height as usize),
                    (frame.width(), frame.height()),
                    "scale {scale} {mechanism:?}: the natural size is the kit's frame",
                );
                assert_eq!(
                    surface.uniforms.grid,
                    (
                        u32::try_from(metrics.width).unwrap(),
                        u32::try_from(metrics.height).unwrap(),
                    ),
                    "scale {scale} {mechanism:?}: the grid is the pre-upscale buffer",
                );
                assert_eq!(
                    (
                        surface.uniforms.grid.0 as usize * scale,
                        surface.uniforms.grid.1 as usize * scale,
                    ),
                    (frame.width(), frame.height()),
                );
            }
        }
    }

    /// **The strip is [`kit::FlipBoard::cell_states`]' own answer**, run
    /// through the kit's own phase curves — [`CELL_TEXELS`] texels per cell, in
    /// cell order.
    ///
    /// **Falsified** by re-deriving a cell's progress here, by transcribing
    /// `PI * p * p` instead of calling [`kit::flap_theta`], or by encoding the
    /// incoming card where the outgoing one belongs.
    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn the_strip_is_the_kits_own_cell_states() {
        let kit_board = board(kit::Mechanism::Nixie, 5, "00:00", "12:34", 0.11, 2);
        let metrics = kit_board.metrics();
        let encoded = cards(&kit_board);
        let strip = encoded.strip.as_ref().expect("five cells");
        let states = kit_board.cell_states();
        assert_eq!(states.len(), 5);
        assert_eq!(strip.len(), states.len() * CELL_TEXELS);

        let mid = metrics.hinge as f32;
        for (index, cell) in states.iter().enumerate() {
            let base = index * CELL_TEXELS;
            let (from_lo, from_hi) = pack_glyph(kit::flip_rows_of(cell.from));
            let (to_lo, to_hi) = pack_glyph(kit::flip_rows_of(cell.to));
            let (sin, cos) = kit::flap_theta(cell.progress).sin_cos();
            let squash = cos.abs();
            let falling_up = cos >= 0.0;
            let (band_lo, band_hi) = if falling_up {
                (mid - squash * mid, mid)
            } else {
                (mid, mid + squash * mid)
            };
            let want = [
                super::f32_of_bits(from_lo),
                super::f32_of_bits(from_hi),
                super::f32_of_bits(to_lo),
                super::f32_of_bits(to_hi),
                band_lo,
                band_hi,
                if falling_up {
                    band_lo
                } else {
                    band_hi - kit::FLIP_EDGE_PX
                },
                if falling_up {
                    band_lo + kit::FLIP_EDGE_PX
                } else {
                    band_hi
                },
                kit::FLIP_EDGE_T * sin,
                kit::FLIP_SHADE_FLOOR + (1.0 - kit::FLIP_SHADE_FLOOR) * squash,
                squash,
                if falling_up { 1.0 } else { 0.0 },
                f32::from(kit::flip_level(
                    kit::nixie_afterglow(cell.progress) * kit::FLIP_GLYPH_T,
                )),
                f32::from(kit::flip_level(
                    kit::nixie_ignite(cell.progress) * kit::FLIP_GLYPH_T,
                )),
            ];
            assert_eq!(&strip[base..base + CELL_TEXELS], &want[..], "cell {index}");
        }

        // A board with no cells binds no strip at all, which is what makes
        // `u_data_len == 0` the shader's "nothing to draw".
        let empty = cards(&board(kit::Mechanism::SplitFlap, 0, "", "", 0.0, 2));
        assert_eq!(empty.metrics.cells, 0);
        assert!(empty.strip.is_none());
        let surface = flip_board_surface(&empty, &kit::palette_snapshot(kit::DisplayStyle::Lcd));
        assert!(surface.uniforms.data.is_none());
        assert_eq!(uniform(&surface.uniforms, "u_cells"), GlValue::Int(0));
    }

    /// **The encoded board travels as one allocation** — the strip is minted
    /// once per mapping pass and handed over by `Arc`, rather than rebuilt
    /// inside [`flip_board_surface`].
    #[test]
    fn the_strip_travels_as_the_callers_allocation() {
        let encoded = cards(&board(
            kit::Mechanism::SplitFlap,
            5,
            "00:00",
            "12:34",
            0.11,
            2,
        ));
        let surface = flip_board_surface(&encoded, &kit::palette_snapshot(kit::DisplayStyle::Oled));
        let held = surface.uniforms.data.as_ref().expect("five cells");
        assert!(Arc::ptr_eq(held, encoded.strip.as_ref().unwrap()));
    }

    /// **Seven passes over six aux textures, the last one on the screen** — the
    /// structural claim the module docs make, asserted rather than described.
    ///
    /// **Falsified** by any of: an `aux` below `6`, a step pass (a board's
    /// whole animation is CPU-side), a second blur stage that reads the raw
    /// emission instead of the recombined layer, a blit that reads the lit
    /// texture back instead of recomputing it, or a target other than the
    /// screen last.
    #[test]
    fn the_pipeline_blooms_twice_and_ends_on_the_screen() {
        assert_eq!(FLIP_BOARD_PIPELINE.aux, 6);
        assert!(
            FLIP_BOARD_PIPELINE.step.is_empty(),
            "a board carries no cross-frame GPU state",
        );
        assert_eq!(
            FLIP_BOARD_PIPELINE.frame.len(),
            7,
            "lit, wide blur H/V, recombine, blur H/V, blit",
        );
        let targets: Vec<GlTarget> = FLIP_BOARD_PIPELINE.frame.iter().map(|p| p.target).collect();
        assert_eq!(
            targets,
            vec![
                GlTarget::Aux(0),
                GlTarget::Aux(1),
                GlTarget::Aux(2),
                GlTarget::Aux(3),
                GlTarget::Aux(4),
                GlTarget::Aux(5),
                GlTarget::Screen,
            ],
        );
        let inputs: Vec<&[GlInput]> = FLIP_BOARD_PIPELINE.frame.iter().map(|p| p.inputs).collect();
        assert_eq!(
            inputs,
            vec![
                &[GlInput::Data][..],
                &[GlInput::Aux(0)][..],
                &[GlInput::Aux(1)][..],
                // The recombine sees the raw emission **and** the wide blur.
                &[GlInput::Aux(0), GlInput::Aux(2)][..],
                // …and the skin's own blur runs over the recombined layer,
                // which is what `Emission::bloom` applied twice means.
                &[GlInput::Aux(3)][..],
                &[GlInput::Aux(4)][..],
                // The blit recomputes the lit layer and samples only the two
                // blurred copies.
                &[GlInput::Data, GlInput::Aux(2), GlInput::Aux(5)][..],
            ],
        );
        for pass in FLIP_BOARD_PIPELINE.frame {
            assert_eq!(pass.blend, GlBlend::Replace);
            assert_eq!(pass.draw, GlDraw::FullScreen);
            assert_ne!(
                pass.target,
                GlTarget::Accumulator,
                "this pipeline declares no accumulator",
            );
        }
    }

    /// **The wide blur is `blur.frag` pointed at its own radius uniform** — the
    /// splice declares `u_halo_radius` and defines `BLUR_RADIUS` to it, so the
    /// two stages of a nixie's double bloom cannot end up reading one number.
    ///
    /// **Falsified** by dropping either splice line (the body then reads
    /// `u_bloom_radius` in all four blur passes and the wide haze silently
    /// becomes the narrow one), or by deleting `blur.frag`'s `#ifndef` default
    /// (every *other* pipeline's splice stops compiling).
    #[test]
    fn the_wide_blur_splice_names_its_own_radius_uniform() {
        for source in [HALO_BLUR_H_FRAG, HALO_BLUR_V_FRAG] {
            assert!(source.contains("uniform int u_halo_radius;\n"));
            assert!(source.contains("#define BLUR_RADIUS u_halo_radius\n"));
        }
        assert!(HALO_BLUR_H_FRAG.contains("const ivec2 BLUR_DIR = ivec2(1, 0);\n"));
        assert!(HALO_BLUR_V_FRAG.contains("const ivec2 BLUR_DIR = ivec2(0, 1);\n"));
        for clause in [
            "#ifndef BLUR_RADIUS\n",
            "#define BLUR_RADIUS u_bloom_radius\n",
            "int radius = max(BLUR_RADIUS, 0);",
        ] {
            assert!(
                BLUR_BODY.contains(clause),
                "blur.frag no longer carries `{clause}` — every other splice \
                 reads its radius through that default",
            );
        }
    }

    /// **The shader declares the CRT pass's four constants with the kit's own
    /// values** — the shared #1186 helper, called here for the reason it exists
    /// (a `.frag` cannot read a Rust `const`, so the copy must be checked).
    #[test]
    fn the_shader_declares_the_kits_crt_constants() {
        super::super::program::assert_crt_constants("flip_board.frag", BODY);
    }

    /// **The shader takes its geometry from uniforms, never from a literal of
    /// its own** — the #1164 shape adapted to a shader whose mirrored constants
    /// are uniforms.
    ///
    /// **Falsified** by inlining any of them (a `const int CELL_W = 14;` beside
    /// the CRT block), which would let the kit's own value move without moving
    /// what CI compiles.
    #[test]
    fn the_shader_reads_its_geometry_from_uniforms() {
        for name in [
            "u_mechanism",
            "u_cells",
            "u_cell_w",
            "u_cell_h",
            "u_hinge",
            "u_bezel",
            "u_gap",
            "u_glyph_px",
            "u_glyph_pad",
            "u_face_top",
            "u_face_bottom",
            "u_cathode",
            "u_stack_lo",
            "u_stack_hi",
            "u_halo_strength",
        ] {
            assert!(
                BODY.contains(&format!("uniform int {name};")),
                "flip_board.frag must declare `uniform int {name};`",
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
                "LAYER_HALO1",
                "LAYER_BLIT",
                "MECH_SPLIT_FLAP",
                "MECH_NIXIE",
                "GLYPH_W",
                "GLYPH_H",
                "CELL_TEXELS",
                "MASK_ONE",
                "COORD_ONE",
                "BAND_DIV",
                "CORNER_DIV",
            ],
            "flip_board.frag grew a compile-time constant — a card metric \
             belongs in the uniform bag, where it is the kit's own item",
        );
    }

    /// **The only geometry this shader restates is the font's glyph box**, and
    /// it restates [`kit::font`]'s own numbers.
    ///
    /// It has to be a `const`: `glyph_coverage`'s loop bound cannot be a
    /// uniform without giving up the unrolled seven iterations, and a 5×7 font
    /// is the one thing in this widget that is not a metric a board can be
    /// built with.
    ///
    /// **Falsified** by moving `font::GLYPH_W` or `GLYPH_H` without moving the
    /// shader's copy.
    #[test]
    fn the_shader_restates_only_the_fonts_glyph_box() {
        assert!(BODY.contains(&format!("const int GLYPH_W = {};", kit::font::GLYPH_W)));
        assert!(BODY.contains(&format!("const int GLYPH_H = {};", kit::font::GLYPH_H)));
    }

    /// **The shader's `CELL_TEXELS` and the two mechanism codes are the
    /// mapping's**, so the strip one side writes is the strip the other reads.
    ///
    /// A disagreement about the stride shifts every cell's state onto its
    /// neighbour, and one about the codes draws a nixie's cross-fade with the
    /// flap's geometry — both silent.
    #[test]
    fn the_shader_and_the_mapping_agree_about_the_strip_encoding() {
        assert!(BODY.contains(&format!("const int CELL_TEXELS = {CELL_TEXELS};")));
        assert!(BODY.contains(&format!("const int MECH_SPLIT_FLAP = {MECH_SPLIT_FLAP};")));
        assert!(BODY.contains(&format!("const int MECH_NIXIE = {MECH_NIXIE};")));
    }

    // ── the transcription ───────────────────────────────────────────────────

    /// Every board state the transcription sweep drives, as
    /// `(cells, from, to, secs)`.
    ///
    /// The empty board, a settled row, five points along one change (three of
    /// them either side of horizontal, which falls at `p = 1/sqrt(2)`), a
    /// staggered whole-row change, cards that are not on the drum, and the
    /// [`kit::FLIP_MAX_CELLS`] row — the widest board the kit will build, whose
    /// 896-texel strip and 1026 px buffer no *parity* case renders, because its
    /// natural size at the shipping upscale would be wider than the harness's
    /// own display. Nothing bounds the cell count in a uniform array — the
    /// per-cell payload is a data texture — so this is where that edge is
    /// covered.
    const SWEEP: [(usize, &str, &str, f32); 10] = [
        (0, "", "", 0.0),
        (5, "12:34", "12:34", 0.0),
        (5, "00:00", "12:34", 0.03),
        (5, "00:00", "12:34", 0.11),
        (5, "00:00", "12:34", 0.19),
        (5, "00:00", "12:34", 0.27),
        (5, "00:00", "12:34", 0.37),
        (8, "88:88:88", "PREEM   ", 0.13),
        (8, "12:34:56", "########", 0.23),
        (kit::FLIP_MAX_CELLS, "", "DEPARTURES 12:34 PLATFORM 9", 0.17),
    ];

    /// Every board the sweeps below drive, at the kit's `scale = 1` — the only
    /// upscale a 1:1 comparison exists at.
    fn sweep_boards() -> Vec<(kit::Mechanism, kit::FlipBoard)> {
        let mut boards = Vec::new();
        for mechanism in kit::Mechanism::ALL {
            for (cells, from, to, secs) in SWEEP {
                boards.push((mechanism, board(mechanism, cells, from, to, secs, 1)));
            }
        }
        boards
    }

    /// **The shader's arithmetic, transcribed here, reproduces the kit byte for
    /// byte at 1:1 — on every skin, on both mechanisms, over every board shape
    /// this widget can draw.**
    ///
    /// The #1153/#1154 pattern, and the test that actually earns the bit-exact
    /// pin: [`shader_frame`] is a line-by-line Rust transcription of
    /// `flip_board.frag`'s snapped branch **and** of `blur.frag`, which this
    /// pipeline runs four times — the same fold, the same two truncating
    /// integer divisions, the same `mix_kit`, the same CRT pass, in the same
    /// order — compared against `kit::FlipBoard::render`'s own bytes. It needs
    /// no GL at all, and llvmpipe is not available to `cargo test`.
    ///
    /// **This arm's 1:1 branch is the first on this seam that is not a point
    /// test**, and that is what makes the transcription worth having. The
    /// scope, the dot matrix, the meter and the readout all collapse onto an
    /// integer at a pixel centre; a flip board does not, because the kit itself
    /// takes a fractional area average of the falling card there. So this
    /// reproduces float arithmetic — including a fused multiply-add and two
    /// divisions — rather than a degeneracy, and the thing most likely to go
    /// silently wrong is an operation order.
    ///
    /// The transcription is held to the **shipped** GLSL by
    /// [`the_mirror_is_the_shipped_shaders_arithmetic`], so the two cannot
    /// drift: this test says the arithmetic is right, that one says it is the
    /// arithmetic that ships. Neither replaces `preem_gl_diff` — a driver can
    /// still disagree with both.
    ///
    /// **Falsified** by any of: dropping the `+ 127` from `mix_kit`, dividing
    /// the blur in floats, renormalising the blur window where it clips at a
    /// buffer edge, folding the two blur *stages* into one (a nixie blooms
    /// twice and the second pass blurs the first's result), compositing the
    /// fixture over the lit layer instead of under it, cutting the hinge slot
    /// before the composite instead of after, or replacing the fused
    /// multiply-add with a multiply and an add.
    ///
    /// The whole sweep runs inside one [`kit::with_pins`] scope pinning
    /// [`kit::Ink::Base`], for `led_strip.rs`'s measured reason: the kit's
    /// accent is a process-global `AtomicU32` that both `palette_snapshot`
    /// (here) and `render` (the oracle) read at render time, and the suite runs
    /// concurrently, so a flip landing between the two calls would make them
    /// differ for a reason that is not this code.
    #[test]
    fn the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                for style in kit::DisplayStyle::ALL {
                    for (mechanism, kit_board) in sweep_boards() {
                        let reference = kit_board.render(style);
                        let mirror = shader_frame(&kit_board, style, 0.0);
                        assert_eq!(
                            mirror,
                            reference.data(),
                            "{style:?} {mechanism:?} {} cell(s)",
                            kit_board.metrics().cells,
                        );
                    }
                }
            },
        );
    }

    /// An upper bound on GLSL ES 3.20 §4.7.1's **2.5 ULP** allowance for
    /// `a / b`, as a relative perturbation.
    ///
    /// A float's ULP is at most `EPSILON` of its own magnitude (exactly that
    /// just above a power of two, half of it just below), so
    /// `2.5 * f32::EPSILON` relative is never smaller than 2.5 ULP and is
    /// usually larger — which is the direction a bound has to err in.
    const DIVIDE_SLACK: f32 = 2.5 * f32::EPSILON;

    /// **Ten times the allowance, which the sweep also survives** (#1155
    /// review, MEDIUM).
    ///
    /// Measured there: the first perturbation that moves any byte of the sweep
    /// is `4e-6`, i.e. **13.4×** [`DIVIDE_SLACK`] (2.98e-7). So the pin holds
    /// by a *margin*, and asserting the margin is what makes a kit change that
    /// walks a `flap_value` onto `level`'s truncation boundary red here — while
    /// there is still headroom — rather than at the moment it flips.
    ///
    /// The review also plumbed a **second** slack through the mirror's
    /// [`divide`] so the two quotients could be perturbed in *opposite*
    /// directions, since a driver's two independent 2.5 ULP errors are free to
    /// be anti-correlated: the first byte moves at `4e-6` there too. The
    /// single-`slack` model below is therefore adequate, which is why there is
    /// one and not two.
    const DIVIDE_SLACK_HEADROOM: f32 = 10.0 * DIVIDE_SLACK;

    /// The negative control's perturbation: four orders above [`DIVIDE_SLACK`],
    /// and small enough that it is still obviously a *rounding* rather than a
    /// different picture.
    const CONTROL_SLACK: f32 = 1e-3;

    /// **No byte of a 1:1 frame is decided by the slack GLSL ES allows the two
    /// divisions** (#1309's lesson, answered for the one kind that cannot
    /// remove them).
    ///
    /// #1309 established the rule: a shader must not divide on a branch that is
    /// held bit-exact, because §4.7.1 pins `+`, `-` and `*` to a correctly
    /// rounded result and allows `a / b` **2.5 ULP**, so a value sitting on a
    /// rounding boundary is decided by the driver rather than by the geometry.
    /// Its fix was to move the divide to the CPU — `u_px_step`.
    ///
    /// **That fix is unavailable here, and not by omission.** Both divisions on
    /// this branch are the *kit's*: `FlipBoard::compose_flap` maps a
    /// destination row onto its source span with `(lo - band_lo) / squash` and
    /// `glyph_coverage` normalises its integral with `acc / span`. Replacing
    /// either with a CPU-computed reciprocal would make this arm disagree with
    /// the renderer it is measured against — `a * (1/b)` is not `a / b` — so
    /// the honest move is to keep them and *measure* whether the slack can
    /// reach a byte.
    ///
    /// It cannot, and this is the measurement: the whole 1:1 sweep re-derived
    /// with **both** quotients perturbed by `±`[`DIVIDE_SLACK`] and again by
    /// `±`[`DIVIDE_SLACK_HEADROOM`], asserting every byte is still the kit's.
    ///
    /// **It holds by a margin, not by construction, and the two are different
    /// claims** (#1155 review, MEDIUM). The structural half is real but bounds
    /// the *error*: the two quotients feed a coverage in `0..=1` which is then
    /// weighted by `cover`, and `cover` is small exactly where the span the
    /// second quotient normalises is small, so the composed value moves by at
    /// most `255 * squash` times the relative slack — under `1e-4` of a byte at
    /// [`DIVIDE_SLACK`]. Whether a *byte* moves depends on the **margin**
    /// between that value and `level`'s truncation boundary, and nothing about
    /// the geometry makes that margin large. It is measured instead, twice
    /// over: [`the_fold_never_lands_on_the_rounding_boundary`] walks every
    /// `flap_value` in the sweep and reports how close the closest comes, and
    /// the `DIVIDE_SLACK_HEADROOM` arm here says the sweep survives ten times
    /// the allowance. The review put the first moving byte at `4e-6`, i.e.
    /// 13.4× — so ten is a floor under a measured number, not a guess.
    ///
    /// The `CONTROL_SLACK` arm is the negative control, and the reason this is
    /// a test rather than a comment: a perturbation four orders larger **does**
    /// move bytes, so the assertions above are measuring the arithmetic's
    /// sensitivity and not the sweep's inability to see anything at all.
    ///
    /// **Falsified** by widening either slack (the control arm stops being a
    /// control), or by making a quotient feed something the `cover` weighting
    /// does not damp.
    #[test]
    fn the_coverage_bytes_are_never_decided_by_the_divides_slack() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                let mut moved = 0_u32;
                for style in kit::DisplayStyle::ALL {
                    for (mechanism, kit_board) in sweep_boards() {
                        let reference = kit_board.render(style);
                        for slack in [
                            DIVIDE_SLACK,
                            -DIVIDE_SLACK,
                            DIVIDE_SLACK_HEADROOM,
                            -DIVIDE_SLACK_HEADROOM,
                        ] {
                            assert_eq!(
                                shader_frame(&kit_board, style, slack),
                                reference.data(),
                                "{style:?} {mechanism:?}: a quotient {slack:e} off its \
                                 correctly-rounded value decided a byte",
                            );
                        }
                        if shader_frame(&kit_board, style, CONTROL_SLACK) != reference.data() {
                            moved += 1;
                        }
                    }
                }
                assert!(
                    moved > 0,
                    "the control never moved a byte — then the assertion above is not \
                     measuring the divides",
                );
            },
        );
    }

    /// How close the closest `flap_value` in the sweep comes to `level`'s
    /// truncation boundary, and how many were looked at at all — the guard
    /// against a census that walks an empty board and reports a clean answer.
    struct Margins {
        seen: u64,
        closest: f32,
    }

    /// The floor [`the_fold_never_lands_on_the_rounding_boundary`] holds the
    /// census to.
    ///
    /// Measured at **3.265e-3** over the whole sweep (see that test's doc), so
    /// this is a third of it — room for a kit change to move a value without
    /// reporting a regression, and not so much room that the statement goes
    /// vacuous.
    ///
    /// **How it composes with the sibling's slack, in numbers rather than in
    /// adjectives.** The largest displacement a relative slack `s` can produce
    /// in a `flap_value` is `255 * squash * s`, i.e. at most `255 s`. At
    /// [`DIVIDE_SLACK`] (2.98e-7) that is **7.6e-5** — thirteen times under
    /// this floor, which is why no byte can move there. At
    /// [`DIVIDE_SLACK_HEADROOM`] (2.98e-6) it is **7.6e-4**, *comparable* to
    /// the floor — which is exactly why the ten-times arm is run as a
    /// measurement rather than inferred from this one.
    const MARGIN_FLOOR: f32 = 1e-3;

    /// **No `flap_value` in the sweep lands on `level`'s truncation boundary**
    /// (#1155 review, MEDIUM and LOW-1 together).
    ///
    /// This is the statistic `flap_value` is split out of `flap255` for, and
    /// the one the sibling above cannot report: that one asks "did any byte
    /// move", which is a yes/no about a perturbation that was applied, and this
    /// one asks "how much room was there", which is what a *later* kit change
    /// spends. A change that walks a value onto `level`'s boundary reds here
    /// while the sibling is still comfortably green — which is the whole
    /// difference between a gate and a coincidence.
    ///
    /// `level(v)` is `int(clamp(v, 0, 255) + 0.5)`, so its answer changes where
    /// `v + 0.5` crosses an integer: the margin is that value's distance to the
    /// nearest one. Measured over [`SWEEP`]'s ten board shapes — the **split
    /// flap only**, since a nixie's emission is two integer cathode levels and
    /// never reaches this rounding at all — **25 498 values looked at, closest
    /// 3.265e-3**.
    ///
    /// **Falsified** by [`MARGIN_FLOOR`] being raised past the measurement, and
    /// — the direction that matters — by any kit change to the phase curve, the
    /// shading floor or the edge rule that walks a value onto the boundary.
    #[test]
    fn the_fold_never_lands_on_the_rounding_boundary() {
        let census = kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                let mut census = Margins {
                    seen: 0,
                    closest: f32::INFINITY,
                };
                for (mechanism, kit_board) in sweep_boards() {
                    if mechanism != kit::Mechanism::SplitFlap {
                        continue;
                    }
                    margin_census(&kit_board, &mut census);
                }
                census
            },
        );
        assert!(
            census.seen > 20_000,
            "the census looked at only {} value(s) — it is walking the wrong boards \
             and the assertion below is vacuous",
            census.seen,
        );
        assert!(
            census.closest >= MARGIN_FLOOR,
            "a fold value came within {:e} of `level`'s truncation boundary — under the \
             {MARGIN_FLOOR:e} floor, so the byte there is decided by the last bit of \
             whatever computed it rather than by the geometry",
            census.closest,
        );
    }

    /// Walk every fragment of `kit_board`'s cards at 1:1 and fold each
    /// [`flap_value`]'s distance from `level`'s truncation boundary into
    /// `census`.
    ///
    /// Values the clamp swallows are skipped: `level` answers `0` for
    /// everything at or below `-0.5` and `255` for everything at or above
    /// `254.5`, so no rounding there can change a byte and counting the
    /// distance would report a margin that is not one.
    #[allow(clippy::cast_precision_loss)]
    fn margin_census(kit_board: &kit::FlipBoard, census: &mut Margins) {
        let encoded = cards(kit_board);
        let metrics = encoded.metrics;
        for row in 0..metrics.height {
            for col in 0..metrics.width {
                let (col, row) = (index(col), index(row));
                let cell = cell_of(&encoded, col);
                if cell < 0 {
                    continue;
                }
                let local_y = row - index(metrics.bezel);
                if local_y < 0 || local_y >= index(metrics.cell_h) {
                    continue;
                }
                let lo = local_y as f32;
                let value = flap_value(
                    &encoded,
                    cell * index(CELL_TEXELS),
                    cell_local_x(&encoded, col, cell),
                    local_y,
                    lo,
                    lo + 1.0,
                    0.0,
                );
                if value <= -0.5 || value >= 254.5 {
                    continue;
                }
                census.seen += 1;
                let rounded = value + 0.5;
                census.closest = census.closest.min((rounded - rounded.round()).abs());
            }
        }
    }

    /// **The transcription above is the arithmetic the shipped shaders carry**
    /// — a source scan over `flip_board.frag` and `blur.frag`, so a fix applied
    /// to one side only reds here instead of leaving a green mirror describing
    /// a shader nobody ships.
    ///
    /// **Every clause [`shader_frame`] transcribes is listed**, which is #1293
    /// item 1's correction applied from the start rather than as a follow-up —
    /// the #1294 review measured eight arithmetic clauses missing from the
    /// readout's version of this scan, each of which could be edited in the
    /// `.frag` alone with the whole hermetic suite green. What is **not**
    /// listed is the continuous (non-`snapped`) branch this mirror never takes,
    /// since [`shader_frame`] always evaluates over a whole logical row: that
    /// is `preem_gl_diff`'s job, not this test's.
    ///
    /// **Falsified** by editing any of them in either `.frag` without editing
    /// [`shader_frame`].
    #[test]
    #[allow(clippy::too_many_lines)]
    fn the_mirror_is_the_shipped_shaders_arithmetic() {
        for clause in [
            // the strip
            "return texelFetch(u_tex0, ivec2(index, 0), 0).r;",
            "return int(strip_at(index) + 0.5);",
            // the glyph lattice
            "int word = row < 4 ? lo : hi;",
            "int shift = (row < 4 ? row : row - 4) * 5;",
            "int bits = (word >> shift) & 31;",
            "return (bits >> (GLYPH_W - 1 - col)) & 1;",
            "if (x < u_glyph_pad || y < u_glyph_pad) {",
            "int col = (x - u_glyph_pad) / u_glyph_px;",
            "int row = (y - u_glyph_pad) / u_glyph_px;",
            "if (col >= GLYPH_W || row >= GLYPH_H) {",
            "return float(glyph_bit(lo, hi, row, col));",
            // the fold's resample
            "float span = src_hi - src_lo;",
            "if (span <= 0.0) {",
            "if (x < u_glyph_pad) {",
            "if (col >= GLYPH_W) {",
            "float padf = float(u_glyph_pad);",
            "float clipped_lo = max(src_lo - padf, 0.0);",
            "float clipped_hi = min(src_hi - padf, float(GLYPH_H * u_glyph_px));",
            "if (clipped_hi <= clipped_lo) {",
            "for (int row = 0; row < GLYPH_H; ++row) {",
            "float row_lo = float(row * u_glyph_px);",
            "float row_hi = float((row + 1) * u_glyph_px);",
            "if (row_lo >= clipped_hi) {",
            "if (glyph_bit(lo, hi, row, col) == 1) {",
            "acc += max(min(clipped_hi, row_hi) - max(clipped_lo, row_lo), 0.0);",
            "return acc / span;",
            // the rounding
            "return int(clamp(value, 0.0, 255.0) + 0.5);",
            // the row
            "if (u_cells <= 0) {",
            "int rel = col - u_bezel;",
            "if (rel < 0) {",
            "int pitch = max(u_cell_w + u_gap, 1);",
            "int index = rel / pitch;",
            "if (index >= u_cells || rel - index * pitch >= u_cell_w) {",
            "return col - u_bezel - index * (u_cell_w + u_gap);",
            // the flap
            "float band_lo = strip_at(base + 4);",
            "float band_hi = strip_at(base + 5);",
            "float edge_lo = strip_at(base + 6);",
            "float edge_hi = strip_at(base + 7);",
            "float edge_peak = strip_at(base + 8);",
            "float shade = strip_at(base + 9);",
            "float squash = strip_at(base + 10);",
            "bool falling_up = strip_int(base + 11) != 0;",
            "int leaf_lo = falling_up ? strip_int(base + 0) : strip_int(base + 2);",
            "int leaf_hi = falling_up ? strip_int(base + 1) : strip_int(base + 3);",
            "float covered = max(min(hi, band_hi) - max(lo, band_lo), 0.0);",
            "float cover = snapped ? covered : covered / max(fstep, 1e-6);",
            "bool has_source = covered > 0.0 && squash > 0.0;",
            "float mid = float(u_hinge);",
            "float from = max(lo, band_lo);",
            "float to = min(hi, band_hi);",
            "src_lo = (from - band_lo) / squash;",
            "src_hi = (to - band_lo) / squash;",
            "src_lo = mid + (from - mid) / squash;",
            "src_hi = mid + (to - mid) / squash;",
            "float ruled = max(min(hi, edge_hi) - max(lo, edge_lo), 0.0);",
            "float edge = edge_peak * (snapped ? ruled : ruled / max(fstep, 1e-6));",
            "int behind_lo = row < u_hinge ? strip_int(base + 2) : strip_int(base + 0);",
            "int behind_hi = row < u_hinge ? strip_int(base + 3) : strip_int(base + 1);",
            "float behind = glyph_at(behind_lo, behind_hi, x, row) * 255.0;",
            // …and the fused multiply-add the kit's `mul_add` is, which GLSL ES
            // leaves free to be split unless the computation is `precise`.
            "precise float value;",
            "float card = glyph_coverage(leaf_lo, leaf_hi, x, src_lo, src_hi) * 255.0 * shade;",
            "value = fma(cover, max(card, edge) - behind, behind);",
            "value = behind;",
            "return value > 0.0 ? level(value) : 0;",
            // the tube
            "bool by_out = glyph_at(strip_int(base + 0), strip_int(base + 1), x, row) > 0.0;",
            "bool by_in = glyph_at(strip_int(base + 2), strip_int(base + 3), x, row) > 0.0;",
            "int out_level = strip_int(base + 12);",
            "int in_level = strip_int(base + 13);",
            "return max(out_level, in_level);",
            // the board
            "int index = cell_of(col);",
            "int local_y = row - u_bezel;",
            "if (local_y < 0 || local_y >= u_cell_h) {",
            "int base = index * CELL_TEXELS;",
            "if (u_mechanism == MECH_NIXIE) {",
            "lo = float(local_y);",
            "hi = lo + 1.0;",
            "float centre = p.y - float(u_bezel);",
            "lo = centre - 0.5 * fstep;",
            "hi = centre + 0.5 * fstep;",
            // the fixture
            "return local_y < u_hinge ? u_face_top : u_face_bottom;",
            "return glyph_at(u_stack_lo, u_stack_hi, x, local_y) > 0.0 ? u_cathode : 0;",
            // the composite
            "return int(texelFetch(tex, p, 0).r * 255.0 + 0.5);",
            "return (a * (255 - k) + b * k + 127) / 255;",
            "int tone = fixture255(col, row);",
            "under = mix_kit(bg, ivec4(u_ghost + 0.5), tone);",
            "int lit = board255(col, row, p, fstep, snapped);",
            "int haze = texel(u_tex1, ivec2(col, row));",
            "lit = min(max(lit, min(haze * u_halo_strength / 256, 255)), 255);",
            "int glow = texel(u_tex2, ivec2(col, row));",
            "lit = min(max(lit, min(glow * u_bloom_strength / 256, 255)), 255);",
            "lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;",
            "under = mix_kit(under, ink, lit);",
            // …and the slot, cut over the finished composite.
            "if (u_mechanism == MECH_SPLIT_FLAP && row == u_bezel + u_hinge && cell_of(col) >= 0) {",
            "o_colour = vec4(vec3(under.rgb) / 255.0, 1.0);",
            // the CRT pass
            "int shortSide = min(w, h);",
            "int r2 = (u * u + v * v) / 2;",
            "int depth = MASK_ONE - u_corner_keep;",
            "int radial = clamp(MASK_ONE - (depth * r2) / (COORD_ONE * COORD_ONE), 0, MASK_ONE);",
            "int ex = min(x, w - 1 - x);",
            "edge = MASK_ONE * d / band;",
            "if (u_mask_pitch != 0 && (y % u_mask_pitch) == u_mask_phase) {",
            "return radial * edge / MASK_ONE * comb / MASK_ONE;",
            "return ((2 * i + 1 - n) * COORD_ONE) / n;",
            // the lit pass's sample point …
            "vec2 p = floor(gl_FragCoord.xy) + 0.5;",
            "o_colour = vec4(float(board255(col, row, p, 1.0, true)) / 255.0, 0.0, 0.0, 1.0);",
            // … the wide-halo recombine …
            "int emitted = texel(u_tex0, q);",
            "int haze = min(texel(u_tex1, q) * u_halo_strength / 256, 255);",
            "o_colour = vec4(float(max(emitted, haze)) / 255.0, 0.0, 0.0, 1.0);",
            // … and the blit's, which #1298 took off the interpolant's value
            // and onto the fragment's own integer index. All seven clauses are
            // load-bearing and each fails differently — see `seven_seg.rs`'s
            // copy of this list for the census that made them so.
            "vec2 vp = vec2(max(u_viewport.x, 1), max(u_viewport.y, 1));",
            "vec2 fi = clamp(floor(v_uv * vp), vec2(0.0), vp - 1.0);",
            "vec2 px = vec2(fi.x, vp.y - 1.0 - fi.y) + 0.5;",
            "vec2 pc = px * u_px_step;",
            "int col = clamp(int(pc.x), 0, cols - 1);",
            "int row = clamp(int(pc.y), 0, rows - 1);",
            "bool snapped = (u_viewport == u_grid);",
            "vec2 p = snapped ? vec2(float(col), float(row)) + 0.5 : pc;",
            "float fstep = snapped ? 1.0 : u_px_step.y;",
        ] {
            assert!(
                BODY.contains(clause),
                "flip_board.frag no longer carries `{clause}` — the Rust mirror \
                 in this module describes a shader that is not the one shipping",
            );
        }
        // …and the blur, which the mirror transcribes just as literally and
        // this pipeline runs four times.
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
        // …and the three composite steps are in the kit's own order: the
        // fixture under the lit layer, and the hinge slot cut over both. A
        // mirror could silently invert either.
        let fixture_at = BODY
            .find("under = mix_kit(bg, ivec4(u_ghost + 0.5), tone);")
            .expect("the fixture composite");
        let lit_at = BODY
            .find("under = mix_kit(under, ink, lit);")
            .expect("the lit composite");
        let slot_at = BODY
            .find("if (u_mechanism == MECH_SPLIT_FLAP && row == u_bezel + u_hinge")
            .expect("the slot cut");
        assert!(
            fixture_at < lit_at,
            "the lit layer composites over the fixture"
        );
        assert!(slot_at > lit_at, "the hinge slot is cut over the composite");
    }

    // ── the mirror ──────────────────────────────────────────────────────────

    /// `flip_board.frag`'s three layers plus `blur.frag`'s two directions run
    /// twice, in Rust: one whole frame, top-down RGBA8, exactly as
    /// `Frame::data` lays it out.
    ///
    /// Everything below mirrors the GLSL statement for statement, `f32` for
    /// `float` and `i32` for `int`, which is what makes a disagreement with the
    /// kit attributable to the shader rather than to this file.
    ///
    /// `slack` is the relative perturbation
    /// [`the_coverage_bytes_are_never_decided_by_the_divides_slack`] applies to
    /// the two quotients GLSL ES does not pin; `0.0` is the shipped arithmetic.
    fn shader_frame(kit_board: &kit::FlipBoard, style: kit::DisplayStyle, slack: f32) -> Vec<u8> {
        let palette = kit::palette_snapshot(style);
        let encoded = cards(kit_board);
        let metrics = encoded.metrics;
        let (w, h) = (metrics.width, metrics.height);
        let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
            radius: 0,
            strength: 0,
        });
        let (halo_radius, halo_strength) = match (encoded.mechanism, palette.bloom) {
            (kit::Mechanism::Nixie, Some(bloom)) => (
                bloom.radius + kit::NIXIE_HALO_RADIUS_BONUS,
                kit::NIXIE_HALO_STRENGTH,
            ),
            _ => (0, 0),
        };

        // `LAYER == LAYER_LIT`, at every grid pixel centre.
        let mut emission = vec![0_i32; w * h];
        for row in 0..h {
            for col in 0..w {
                emission[row * w + col] = board255(&encoded, index(col), index(row), slack);
            }
        }
        // `blur.frag` twice, at the **wide** radius …
        let tmp = blur_pass(&emission, (w, h), halo_radius, (1, 0));
        let wide = blur_pass(&tmp, (w, h), halo_radius, (0, 1));
        // … then `LAYER == LAYER_HALO1`, which is `Emission::bloom`'s
        // max-combine at the grid …
        let recombined: Vec<i32> = emission
            .iter()
            .zip(&wide)
            .map(|(emitted, blurred)| {
                (*emitted).max((blurred * i32::from(halo_strength) / 256).min(255))
            })
            .collect();
        // … and the skin's own bloom over **that**, which is what makes the two
        // passes a composition rather than a sum.
        let tmp = blur_pass(&recombined, (w, h), bloom.radius, (1, 0));
        let narrow = blur_pass(&tmp, (w, h), bloom.radius, (0, 1));

        // `LAYER == LAYER_BLIT`.
        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                let mut under = palette.bg;
                if let Some(ghost) = palette.ghost {
                    let tone = fixture255(&encoded, index(col), index(row));
                    if tone > 0 {
                        under = mix_kit(under, ghost, tone);
                    }
                }
                let mut lit = board255(&encoded, index(col), index(row), slack);
                let haze = wide[row * w + col];
                lit = lit
                    .max((haze * i32::from(halo_strength) / 256).min(255))
                    .min(255);
                let glow = narrow[row * w + col];
                lit = lit
                    .max((glow * i32::from(bloom.strength) / 256).min(255))
                    .min(255);
                if lit > 0 {
                    if let Some(mask) = palette.mask {
                        lit = lit * mask_keep(col, row, w, h, mask)
                            / i32::try_from(kit::MASK_ONE).unwrap();
                    }
                    if lit > 0 {
                        under = mix_kit(under, palette.ink, lit);
                    }
                }
                // The slots, cut over the finished composite.
                if encoded.mechanism == kit::Mechanism::SplitFlap
                    && row == metrics.bezel + metrics.hinge
                    && cell_of(&encoded, index(col)) >= 0
                {
                    under = palette.bg;
                }
                out.extend_from_slice(&[under[0], under[1], under[2], 0xff]);
            }
        }
        out
    }

    /// A buffer coordinate as the `int` the GLSL carries.
    fn index(value: usize) -> i32 {
        i32::try_from(value).unwrap_or(i32::MAX)
    }

    /// `strip_at`.
    fn strip_at(cards: &super::Cards, i: i32) -> f32 {
        let strip = cards.strip.as_deref().unwrap_or(&[]);
        usize::try_from(i)
            .ok()
            .and_then(|i| strip.get(i).copied())
            .unwrap_or(0.0)
    }

    /// `strip_int`.
    #[allow(clippy::cast_possible_truncation)]
    fn strip_int(cards: &super::Cards, i: i32) -> i32 {
        (strip_at(cards, i) + 0.5) as i32
    }

    /// `glyph_bit`.
    fn glyph_bit(lo: i32, hi: i32, row: i32, col: i32) -> i32 {
        let word = if row < 4 { lo } else { hi };
        let shift = (if row < 4 { row } else { row - 4 }) * 5;
        let bits = (word >> shift) & 31;
        (bits >> (index(kit::font::GLYPH_W) - 1 - col)) & 1
    }

    /// `glyph_at`.
    #[allow(clippy::cast_precision_loss)]
    fn glyph_at(cards: &super::Cards, lo: i32, hi: i32, x: i32, y: i32) -> f32 {
        let pad = index(cards.metrics.glyph_pad);
        let g = index(cards.metrics.glyph_px);
        if x < pad || y < pad {
            return 0.0;
        }
        let col = (x - pad) / g;
        let row = (y - pad) / g;
        if col >= index(kit::font::GLYPH_W) || row >= index(kit::font::GLYPH_H) {
            return 0.0;
        }
        glyph_bit(lo, hi, row, col) as f32
    }

    /// `a / b`, with the quotient perturbed by `slack` relative — the one seam
    /// [`the_coverage_bytes_are_never_decided_by_the_divides_slack`] moves.
    fn divide(a: f32, b: f32, slack: f32) -> f32 {
        (a / b) * (1.0 + slack)
    }

    /// `glyph_coverage`.
    #[allow(clippy::cast_precision_loss)]
    fn glyph_coverage(
        cards: &super::Cards,
        lo: i32,
        hi: i32,
        x: i32,
        src_lo: f32,
        src_hi: f32,
        slack: f32,
    ) -> f32 {
        let span = src_hi - src_lo;
        if span <= 0.0 {
            return 0.0;
        }
        let pad = index(cards.metrics.glyph_pad);
        let g = index(cards.metrics.glyph_px);
        if x < pad {
            return 0.0;
        }
        let col = (x - pad) / g;
        if col >= index(kit::font::GLYPH_W) {
            return 0.0;
        }
        let padf = pad as f32;
        let clipped_lo = (src_lo - padf).max(0.0);
        let clipped_hi = (src_hi - padf).min((index(kit::font::GLYPH_H) * g) as f32);
        if clipped_hi <= clipped_lo {
            return 0.0;
        }
        let mut acc = 0.0;
        for row in 0..index(kit::font::GLYPH_H) {
            let row_lo = (row * g) as f32;
            let row_hi = ((row + 1) * g) as f32;
            if row_lo >= clipped_hi {
                break;
            }
            if glyph_bit(lo, hi, row, col) == 1 {
                acc += (clipped_hi.min(row_hi) - clipped_lo.max(row_lo)).max(0.0);
            }
        }
        divide(acc, span, slack)
    }

    /// `level`.
    #[allow(clippy::cast_possible_truncation)]
    fn level(value: f32) -> i32 {
        (value.clamp(0.0, 255.0) + 0.5) as i32
    }

    /// `cell_of`.
    fn cell_of(cards: &super::Cards, col: i32) -> i32 {
        let metrics = cards.metrics;
        if metrics.cells == 0 {
            return -1;
        }
        let rel = col - index(metrics.bezel);
        if rel < 0 {
            return -1;
        }
        let pitch = (index(metrics.cell_w) + index(metrics.gap)).max(1);
        let cell = rel / pitch;
        if cell >= index(metrics.cells) || rel - cell * pitch >= index(metrics.cell_w) {
            return -1;
        }
        cell
    }

    /// `cell_local_x`.
    fn cell_local_x(cards: &super::Cards, col: i32, cell: i32) -> i32 {
        let metrics = cards.metrics;
        col - index(metrics.bezel) - cell * (index(metrics.cell_w) + index(metrics.gap))
    }

    /// `flap_value` — the snapped branch, where the fragment is one whole
    /// logical row and `cover` is the covered length itself.
    ///
    /// Split out of [`flap255`] on both sides for the same reason: it is the
    /// number [`level`]'s rounding decides, and
    /// [`the_fold_never_lands_on_the_rounding_boundary`] walks it to measure
    /// how much room that rounding has.
    #[allow(clippy::cast_precision_loss, clippy::too_many_arguments)]
    fn flap_value(
        cards: &super::Cards,
        base: i32,
        x: i32,
        row: i32,
        lo: f32,
        hi: f32,
        slack: f32,
    ) -> f32 {
        let band_lo = strip_at(cards, base + 4);
        let band_hi = strip_at(cards, base + 5);
        let edge_lo = strip_at(cards, base + 6);
        let edge_hi = strip_at(cards, base + 7);
        let edge_peak = strip_at(cards, base + 8);
        let shade = strip_at(cards, base + 9);
        let squash = strip_at(cards, base + 10);
        let falling_up = strip_int(cards, base + 11) != 0;
        let leaf_lo = if falling_up {
            strip_int(cards, base)
        } else {
            strip_int(cards, base + 2)
        };
        let leaf_hi = if falling_up {
            strip_int(cards, base + 1)
        } else {
            strip_int(cards, base + 3)
        };

        let covered = (hi.min(band_hi) - lo.max(band_lo)).max(0.0);
        let cover = covered;
        let has_source = covered > 0.0 && squash > 0.0;

        let mut src_lo = 0.0;
        let mut src_hi = 0.0;
        if has_source {
            let mid = cards.metrics.hinge as f32;
            let from = lo.max(band_lo);
            let to = hi.min(band_hi);
            if falling_up {
                src_lo = divide(from - band_lo, squash, slack);
                src_hi = divide(to - band_lo, squash, slack);
            } else {
                src_lo = mid + divide(from - mid, squash, slack);
                src_hi = mid + divide(to - mid, squash, slack);
            }
        }

        let ruled = (hi.min(edge_hi) - lo.max(edge_lo)).max(0.0);
        let edge = edge_peak * ruled;

        let behind_lo = if row < index(cards.metrics.hinge) {
            strip_int(cards, base + 2)
        } else {
            strip_int(cards, base)
        };
        let behind_hi = if row < index(cards.metrics.hinge) {
            strip_int(cards, base + 3)
        } else {
            strip_int(cards, base + 1)
        };
        let behind = glyph_at(cards, behind_lo, behind_hi, x, row) * 255.0;

        if has_source {
            let card =
                glyph_coverage(cards, leaf_lo, leaf_hi, x, src_lo, src_hi, slack) * 255.0 * shade;
            // `fma`, which is `f32::mul_add` and which the shader declares
            // `precise` so a driver cannot split it.
            cover.mul_add(card.max(edge) - behind, behind)
        } else {
            behind
        }
    }

    /// `flap255`.
    fn flap255(
        cards: &super::Cards,
        base: i32,
        x: i32,
        row: i32,
        lo: f32,
        hi: f32,
        slack: f32,
    ) -> i32 {
        let value = flap_value(cards, base, x, row, lo, hi, slack);
        if value > 0.0 { level(value) } else { 0 }
    }

    /// `nixie255`.
    fn nixie255(cards: &super::Cards, base: i32, x: i32, row: i32) -> i32 {
        let by_out = glyph_at(
            cards,
            strip_int(cards, base),
            strip_int(cards, base + 1),
            x,
            row,
        ) > 0.0;
        let by_in = glyph_at(
            cards,
            strip_int(cards, base + 2),
            strip_int(cards, base + 3),
            x,
            row,
        ) > 0.0;
        let out_level = strip_int(cards, base + 12);
        let in_level = strip_int(cards, base + 13);
        match (by_out, by_in) {
            (true, true) => out_level.max(in_level),
            (true, false) => out_level,
            (false, true) => in_level,
            (false, false) => 0,
        }
    }

    /// `board255`, on the snapped branch.
    #[allow(clippy::cast_precision_loss)]
    fn board255(cards: &super::Cards, col: i32, row: i32, slack: f32) -> i32 {
        let cell = cell_of(cards, col);
        if cell < 0 {
            return 0;
        }
        let local_y = row - index(cards.metrics.bezel);
        if local_y < 0 || local_y >= index(cards.metrics.cell_h) {
            return 0;
        }
        let x = cell_local_x(cards, col, cell);
        let base = cell * index(CELL_TEXELS);
        if cards.mechanism == kit::Mechanism::Nixie {
            return nixie255(cards, base, x, local_y);
        }
        let lo = local_y as f32;
        flap255(cards, base, x, local_y, lo, lo + 1.0, slack)
    }

    /// `fixture255`.
    fn fixture255(cards: &super::Cards, col: i32, row: i32) -> i32 {
        let cell = cell_of(cards, col);
        if cell < 0 {
            return 0;
        }
        let local_y = row - index(cards.metrics.bezel);
        if local_y < 0 || local_y >= index(cards.metrics.cell_h) {
            return 0;
        }
        if cards.mechanism == kit::Mechanism::SplitFlap {
            return if local_y < index(cards.metrics.hinge) {
                i32::from(kit::FLIP_FACE_TOP_T)
            } else {
                i32::from(kit::FLIP_FACE_BOTTOM_T)
            };
        }
        let x = cell_local_x(cards, col, cell);
        let (stack_lo, stack_hi) = pack_glyph(kit::nixie_cathode_stack());
        if glyph_at(
            cards,
            i32::try_from(stack_lo).unwrap_or(0),
            i32::try_from(stack_hi).unwrap_or(0),
            x,
            local_y,
        ) > 0.0
        {
            i32::from(kit::NIXIE_CATHODE_T)
        } else {
            0
        }
    }

    /// `blur.frag`, one direction: the kit's own clipped-window,
    /// unrenormalised, truncating integer box blur.
    fn blur_pass(src: &[i32], size: (usize, usize), radius: usize, dir: (i32, i32)) -> Vec<i32> {
        let (w, h) = size;
        let radius = index(radius);
        let window = 2 * radius + 1;
        let mut out = vec![0_i32; src.len()];
        for row in 0..h {
            for col in 0..w {
                let mut sum = 0;
                for d in -radius..=radius {
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
        (2 * index(i) + 1 - index(n)) * i32::try_from(kit::COORD_ONE).unwrap() / index(n)
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
