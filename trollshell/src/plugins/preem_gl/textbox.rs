//! The `TextBox` GL program (#1152): the pipeline declaration, the glyph block
//! and the pure `(layout, block) → GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge),
//! [`dot_matrix`](super::dot_matrix) and [`marquee`](super::marquee) in every
//! structural way — it references nothing above itself either, so the parity
//! harness (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this**
//! file and measures the shell's own pipeline and mapping against the CPU kit.
//!
//! # The simplest pipeline on the seam
//!
//! **One pass, no aux textures, no step passes.** A textbox has no emission:
//! the kit paints a field, cuts its corners to transparent and stamps 5×7 glyph
//! pixels over it, and that is the whole widget. There is no falloff to
//! evaluate, no halo to blur and no CRT comb to re-phase — so `textbox.frag` is
//! the one shader here that is not spliced with a `const int LAYER`, because
//! there is only one layer for it to be.
//!
//! # What is better than the kit
//!
//! **The corner, and only the corner.** The kit decides the rounded cut on the
//! *pre-scale* buffer and then replicates each logical pixel `scale` times
//! ([`kit::TextBox::scale`]), so a bubble at `scale = 2` has a corner made of
//! 2×2 blocks, and one that layout stretched has it made of whatever GSK's
//! nearest-neighbour scaling produces. The shader evaluates the kit's own
//! predicate at the fragment's position on the logical lattice instead, so the
//! arc is as round as the screen can draw it.
//!
//! The **glyphs are deliberately not** improved: they are bitmap pixels and
//! square is the look, so the fragment's position is floored to a logical pixel
//! before the strip is consulted. Bigger hard-edged pixels, never a smoothed
//! font.
//!
//! At 1:1 the shader snaps to the *logical* pixel centre, where the corner's
//! `poke` returns exactly the integers the kit's `corner_delta` does and the
//! floor picks exactly the logical pixel the kit blitted — so the float path is
//! the integer path at any upscale, and the parity cases are pinned bit-exact.
//!
//! # Nothing at all is mirrored
//!
//! Unlike [`gauge`](super::gauge), and even more so than
//! [`dot_matrix`](super::dot_matrix), this module copies **no** kit constant
//! and **no** kit formula. Every number it needs comes off
//! [`kit::TextBox::layout`] — an additive `pub` accessor #1152 added to
//! `hytte-preem` on the [`kit::dot_cell`] precedent, and one that
//! `TextBox::render` is itself written in terms of, so the wrap, the width
//! rule, the buffer formula and the corner predicate cannot drift from what
//! this reads back. The only thing that *is* duplicated is the corner
//! predicate's shape, in GLSL, because a fragment shader cannot call a Rust
//! function — and `tests::the_corner_cut_is_the_kits_own_field` holds the
//! shader's continuous form to [`kit::TextBoxLayout::field_at`] at every pixel
//! of a box, which is where the shader samples it at 1:1.
//!
//! # The palette arrives baked, and that is this widget's peculiarity
//!
//! [`textbox_surface`] takes **no** [`kit::PaletteSnapshot`], where every other
//! mapping on this seam does. `TextBox` is the one kit widget that resolves its
//! palette at *construction* (`TextBox::styled` bakes bg/ink/notdef into the
//! builder, which is why `preem_render::invalidate_cached_frames` rebuilds its
//! renderer rather than only dropping its bytes) — so the resolved colors are
//! already in the layout, and asking for a snapshot here would re-derive a
//! palette the builder had deliberately frozen, losing a pinned field or a
//! plugin's own `.notdef` on the way.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_preem as kit;

use super::program::{FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `TextBox` pipeline.
pub(crate) const TEXTBOX: GlProgram = GlProgram("preem.textbox");

/// The one fragment program. Not spliced — see the module docs.
const TEXTBOX_FRAG: &str = include_str!("textbox.frag");

/// Bit 7 of a strip texel: this cell is an uncovered char, so its set pixels
/// take the `.notdef` color. Bits 0..=6 are the glyph's own rows, so 7 is the
/// first one free. Mirrored in `textbox.frag` as `NOTDEF_BIT`, and
/// [`tests::the_shader_and_the_mapping_agree_about_the_font_metrics`] reads it
/// back out of the GLSL.
const NOTDEF_BIT: u32 = 7;

/// The `TextBox` pipeline: one fullscreen blit to the screen.
pub(crate) const TEXTBOX_PIPELINE: GlPipeline = GlPipeline {
    // No emission, no halo, no accumulator — nothing to render off-screen.
    aux: 0,
    step: &[],
    frame: &[GlPass {
        vertex: FULLSCREEN_VERT,
        fragment: TEXTBOX_FRAG,
        target: GlTarget::Screen,
        inputs: &[GlInput::Data],
        blend: GlBlend::Replace,
        draw: GlDraw::FullScreen,
    }],
};

/// The laid-out text as the shader consumes it: how many glyph cells the block
/// has and the bits for them.
///
/// Built once per text (or palette) change and shared by every monitor's
/// mapping pass, the way `Renderer::ScopeGl`'s `samples` and
/// `Renderer::DotMatrixGl`'s glyph strip are (#911's rule, for uniforms): the
/// `Arc` makes a repeat mapping's dedup a pointer compare rather than a
/// re-encode of the whole block.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Block {
    /// One texel per glyph **column** of the block — a **rectangle** of
    /// `lines × content_cols` cells, so a line shorter than the widest one
    /// carries blank cells rather than moving the ones after it —
    /// `cells * GLYPH_W` texels, each holding
    /// that column's [`kit::font::GLYPH_H`] row bits (bit `row` for a set
    /// pixel) plus [`NOTDEF_BIT`] where the cell's char was uncovered. `None`
    /// for a block with no cells at all — an empty hugging box — which is what
    /// `GlUniforms::data` wants there (it binds a 1×1 zero texture and sets
    /// `u_data_len` to `0`).
    ///
    /// A column rather than a whole cell because a cell is 35 bits and an `f32`
    /// carries 24 exactly; eight bits is exact, and the block is
    /// `5 × lines × cols` texels — 40 960 at the wire's own
    /// `MAX_TEXT_COLS × MAX_TEXT_LINES` ceiling.
    pub(crate) strip: Option<Arc<[f32]>>,
}

/// Encode a laid-out box for the shader — the pure half of the mapping.
///
/// Goes through [`kit::font::glyph`], so an uncovered char becomes the kit's
/// own hollow [`kit::font::NOTDEF`] box here rather than a second fallback
/// policy in the shader; the [`NOTDEF_BIT`] flag rides along so the shader can
/// give it the box's own color, which is the one thing the dot matrix's strip
/// does not have to carry.
pub(crate) fn block(layout: &kit::TextBoxLayout) -> Block {
    let cols = layout.content_cols();
    let cells = layout.lines().len() * cols;
    if cells == 0 {
        return Block { strip: None };
    }
    let mut strip = Vec::with_capacity(cells * kit::font::GLYPH_W);
    for line in layout.lines() {
        // The block is a rectangle: a short line's missing cells are blank,
        // which is what keeps `(line * cols + cell)` a valid index for every
        // line rather than only the widest one.
        let mut chars = line.chars();
        for _ in 0..cols {
            let (rows, notdef) = match chars.next() {
                Some(ch) => match kit::font::glyph(ch) {
                    Some(rows) => (rows, false),
                    None => (&kit::font::NOTDEF, true),
                },
                None => (&BLANK, false),
            };
            for col in 0..kit::font::GLYPH_W {
                let mut bits = 0u32;
                for (row, &pixels) in rows.iter().enumerate() {
                    // The kit reads a glyph row MSB-first across GLYPH_W; the
                    // strip is transposed to one texel per column, bit `row`.
                    if (pixels >> (kit::font::GLYPH_W - 1 - col)) & 1 == 1 {
                        bits |= 1 << row;
                    }
                }
                if notdef {
                    bits |= 1 << NOTDEF_BIT;
                }
                strip.push(f32::from(u16::try_from(bits).unwrap_or(0)));
            }
        }
    }
    Block {
        strip: Some(Arc::from(&strip[..])),
    }
}

/// A cell past the end of its line: no pixels, and so nothing to draw.
const BLANK: [u8; kit::font::GLYPH_H] = [0; kit::font::GLYPH_H];

/// Map one `TextBox`'s resolved layout and encoded block onto the GL node
/// payload.
///
/// **Pure**, exactly as the other mappings on this seam are: it reads no
/// globals, resolves no palette and touches no GL. Unlike them it takes no
/// palette *argument* either — see the module docs on why this widget's colors
/// arrive already baked into the layout.
pub(crate) fn textbox_surface(layout: &kit::TextBoxLayout, block: &Block) -> KitSurface {
    let (width, height) = layout.buffer();
    let (bg, ink, notdef) = layout.colors();

    KitSurface {
        width: u32_of(width),
        height: u32_of(height),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                (
                    "u_logical",
                    GlValue::Ivec2([int_of(layout.width()), int_of(layout.height())]),
                ),
                ("u_upscale", GlValue::Int(int_of(layout.scale().max(1)))),
                ("u_pad", GlValue::Int(int_of(layout.pad()))),
                ("u_corner", GlValue::Int(int_of(layout.corner()))),
                ("u_cols", GlValue::Int(int_of(layout.content_cols()))),
                ("u_lines", GlValue::Int(int_of(layout.lines().len()))),
                ("u_bg", channels(bg)),
                ("u_ink", channels(ink)),
                ("u_notdef", channels(notdef)),
            ],
            // The glyph block. Shared rather than rebuilt per mapping pass —
            // see [`Block`].
            data: block.strip.clone(),
            // The buffer the kit would have produced, which for this widget is
            // the **final** one: `TextBox::scale` bakes its upscale into the
            // bytes, so the grid the reconciler asks for is already
            // `logical × scale` and the shader divides back down rather than
            // multiplying up (the gauge does the opposite — #1143 — because
            // there the kit upscales a *rasterisation* and here it replicates
            // finished pixels).
            grid: (u32_of(width), u32_of(height)),
            // No step passes, so nothing counts steps. See [`TEXTBOX_PIPELINE`].
            step_seq: 0,
        },
    }
}

/// A buffer dimension as a `u32`, saturating. Every value reaching this is
/// bounded by the wire's `MAX_BUFFER_DIM`, far below `u32::MAX`.
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
        GlBlend, GlDraw, GlInput, GlTarget, GlUniforms, GlValue, NOTDEF_BIT, TEXTBOX_PIPELINE,
        block, kit, textbox_surface,
    };

    /// The shader body — the source the constants below are read back out of.
    const BODY: &str = include_str!("textbox.frag");

    /// A styled box with every knob at a value that is not its default, so a
    /// uniform that silently took a default would show.
    fn boxed() -> kit::TextBox {
        kit::TextBox::styled(kit::DisplayStyle::Vfd)
            .cols(9)
            .max_lines(3)
            .pad(3)
            .corner(2)
            .scale(2)
            .fixed_width(true)
    }

    /// **The uniform table is the kit's own layout**, name for name and value
    /// for value.
    ///
    /// The names are the contract with the GLSL — a rename on one side alone
    /// draws nothing and says nothing — and the order is pinned so a reordering
    /// shows up as a diff here rather than silently.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row, or by
    /// handing `u_logical` the post-scale buffer (which is `u_grid`).
    #[test]
    fn the_uniform_table_is_the_kits_own_layout() {
        let boxed = boxed();
        let layout = boxed.layout("mrrp mrrp");
        let surface = textbox_surface(&layout, &block(&layout));

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_logical",
                "u_upscale",
                "u_pad",
                "u_corner",
                "u_cols",
                "u_lines",
                "u_bg",
                "u_ink",
                "u_notdef",
            ],
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_logical"),
            GlValue::Ivec2([
                i32::try_from(layout.width()).unwrap(),
                i32::try_from(layout.height()).unwrap(),
            ]),
        );
        assert_eq!(uniform(&surface.uniforms, "u_upscale"), GlValue::Int(2));
        assert_eq!(uniform(&surface.uniforms, "u_pad"), GlValue::Int(3));
        assert_eq!(uniform(&surface.uniforms, "u_corner"), GlValue::Int(2));
        assert_eq!(uniform(&surface.uniforms, "u_cols"), GlValue::Int(9));
        assert_eq!(
            uniform(&surface.uniforms, "u_lines"),
            GlValue::Int(i32::try_from(layout.lines().len()).unwrap()),
        );
        let (bg, ink, notdef) = layout.colors();
        assert_eq!(uniform(&surface.uniforms, "u_bg"), super::channels(bg));
        assert_eq!(uniform(&surface.uniforms, "u_ink"), super::channels(ink));
        assert_eq!(
            uniform(&surface.uniforms, "u_notdef"),
            super::channels(notdef),
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
    }

    /// **The grid is the final buffer**, upscale included — the frame the kit
    /// actually hands the reconciler, not the logical one it lays glyphs out
    /// on.
    ///
    /// This is the direction the gauge goes the *other* way (#1143 renders a
    /// native grid from a logical config), and getting it backwards here would
    /// draw a box a `scale`th of its size in the corner of its own allocation.
    ///
    /// **Falsified** by handing `grid` the pre-scale size.
    #[test]
    fn the_grid_is_the_kits_final_buffer_at_every_scale() {
        for scale in [1_usize, 2, 3, 8] {
            for text in ["", "hi", "one two three four five six"] {
                let boxed = kit::TextBox::styled(kit::DisplayStyle::Lcd)
                    .cols(9)
                    .scale(scale);
                let layout = boxed.layout(text);
                let frame = boxed.render(text);
                let surface = textbox_surface(&layout, &block(&layout));
                assert_eq!(
                    (surface.width as usize, surface.height as usize),
                    (frame.width(), frame.height()),
                    "scale {scale}, {text:?}",
                );
                assert_eq!(surface.uniforms.grid, (surface.width, surface.height));
                assert_eq!(
                    uniform(&surface.uniforms, "u_upscale"),
                    GlValue::Int(i32::try_from(scale).unwrap()),
                );
            }
        }
    }

    /// **The corner cut is the kit's own field** — the shader's continuous
    /// `poke` reproduced in Rust and measured against
    /// [`kit::TextBoxLayout::field_at`] at every pixel of several boxes.
    ///
    /// This is the one formula in this module that *is* duplicated (a fragment
    /// shader cannot call a Rust function), and the duplicate has a sharp edge:
    /// the ±0.5 shifts are what turn the continuous distance back into the
    /// kit's integer `corner_delta` at a pixel centre, and dropping either one
    /// moves the corner by half a pixel — invisible at 1:1 in a screenshot and
    /// a full-byte disagreement in the harness.
    ///
    /// **Falsified** by dropping the `bias` from either term, by using `n`
    /// instead of `n - bias - corner` on the far edge, or by comparing against
    /// `corner` rather than `corner²`.
    #[test]
    fn the_corner_cut_is_the_kits_own_field() {
        for corner in [0_usize, 1, 2, 5, 9] {
            for pad in [0_usize, 1, 3] {
                let boxed = kit::TextBox::new().cols(7).pad(pad).corner(corner);
                for text in ["", "hi", "wrap me over lines"] {
                    let layout = boxed.layout(text);
                    #[allow(clippy::cast_precision_loss)]
                    let (w, h) = (layout.width() as f64, layout.height() as f64);
                    #[allow(clippy::cast_precision_loss)]
                    let c = corner as f64;
                    for y in 0..layout.height() {
                        for x in 0..layout.width() {
                            #[allow(clippy::cast_precision_loss)]
                            let (qx, qy) = (x as f64 + 0.5, y as f64 + 0.5);
                            let (fx, fy) = (poke(qx, w, c, 0.5), poke(qy, h, c, 0.5));
                            assert_eq!(
                                fx * fx + fy * fy <= c * c,
                                layout.field_at(x, y),
                                "corner {corner} pad {pad} {text:?}: ({x},{y})",
                            );
                        }
                    }
                }
            }
        }
    }

    /// **The unbiased arc leaves the straight edges alone**, which is the
    /// entire reason `poke` takes a `bias` rather than carrying one constant.
    ///
    /// The kit's discrete disc (`bias = 0.5`) has its extreme points at pixel
    /// *centres*, so sampling it densely finds the half-pixel strip
    /// `v ∈ [0, 0.5)` outside along all four straight edges — and box-averaging
    /// that back down cuts a border off a box the kit filled solid. #1152's
    /// first cut shipped exactly that and the harness caught it: four
    /// `FAIL(interior)` cases with the whole field bin moved. This asserts the
    /// property that fix rests on, at the sub-pixel positions a `stretch = 2`
    /// render actually samples.
    ///
    /// **Falsified** by giving the continuous branch `bias = 0.5` (the first
    /// assertion), or by widening the arc's radius (the second).
    #[test]
    fn the_unbiased_arc_fills_the_buffers_own_edges() {
        let c = 5.0_f64;
        let (w, h) = (69.0, 11.0);
        // Straight edges, at the sub-pixel offsets a `stretch = 2` render of a
        // `scale = 1` box samples the outermost logical pixel at (0.25 and
        // 0.75), and at the `stretch = 4` ones either side of them.
        for v in [0.125, 0.25, 0.375, 0.75] {
            assert!(
                poke(v, w, c, 0.0) <= c,
                "the arc keeps the left edge at v = {v}",
            );
            assert!(
                poke(w - v, w, c, 0.0) <= c,
                "…and the right edge at v = {v}",
            );
            // The kit's own disc reaches only to the pixel *centre*, so every
            // sample in `[0, 0.5)` falls outside it — that is the bug, and it
            // is deliberately not asserted at `0.75`, which is the half of the
            // outermost pixel the disc does cover.
            assert_eq!(
                poke(v, w, c, 0.5) > c,
                v < 0.5,
                "the premise: the kit's own disc cuts the outer half-pixel",
            );
        }
        // …and the two shapes still agree about the corner to within one
        // logical pixel, which is what keeps every disagreement in the
        // harness's `edge` bin.
        for y in 0..11_usize {
            for x in 0..69_usize {
                #[allow(clippy::cast_precision_loss)]
                let (qx, qy) = (x as f64 + 0.5, y as f64 + 0.5);
                let kit_in = poke(qx, w, c, 0.5).powi(2) + poke(qy, h, c, 0.5).powi(2) <= c * c;
                let arc_in = poke(qx, w, c, 0.0).powi(2) + poke(qy, h, c, 0.0).powi(2) <= c * c;
                if kit_in != arc_in {
                    assert!(
                        arc_in,
                        "({x},{y}): the arc only ever adds material, never removes it",
                    );
                    let corner_band = !(5..64).contains(&x) && !(5..6).contains(&y);
                    assert!(corner_band, "({x},{y}) is not in a corner");
                }
            }
        }
    }

    /// `textbox.frag`'s `poke`, verbatim, in `f64`.
    fn poke(v: f64, n: f64, corner: f64, bias: f64) -> f64 {
        (corner + bias - v).max(v - (n - bias - corner)).max(0.0)
    }

    /// **The block is a rectangle**, and an uncovered char is flagged rather
    /// than silently drawn in the ink.
    ///
    /// Both halves matter to the shader's one index expression
    /// (`(line * u_cols + cell) * GLYPH_W + col`): a ragged block would put
    /// every line after the first one's cells at the wrong offset, and a
    /// missing flag would draw a `.notdef` box in the text ink — which, on the
    /// pet's bubble, is the one color a plugin pinned by hand.
    ///
    /// **Falsified** by encoding `line.chars().count()` cells instead of
    /// `content_cols`, or by dropping the `NOTDEF_BIT`.
    #[test]
    fn the_block_is_a_rectangle_and_flags_the_notdef_cells() {
        let boxed = kit::TextBox::new().cols(6).fixed_width(true);
        let layout = boxed.layout("hi \u{1f495}");
        let block = block(&layout);
        assert_eq!(layout.content_cols(), 6, "the premise: a fixed 6-cell slot");
        let strip = block.strip.expect("a non-empty block");
        assert_eq!(
            strip.len(),
            layout.lines().len() * 6 * kit::font::GLYPH_W,
            "a rectangle of cells, five texels each",
        );

        let bits = |cell: usize, col: usize| texel(&strip, cell, col);
        let notdef =
            |cell: usize| (0..kit::font::GLYPH_W).all(|c| bits(cell, c) >> NOTDEF_BIT == 1);
        // "hi 💕" wraps to one line of four cells plus two blank ones.
        assert!(!notdef(0), "'h' is covered");
        assert!(!notdef(1), "'i' is covered");
        assert!(!notdef(2), "a space is covered — an empty glyph, not a box");
        assert!(notdef(3), "the emoji is not");
        for cell in 4..6 {
            assert_eq!(
                (0..kit::font::GLYPH_W).map(|c| bits(cell, c)).sum::<u32>(),
                0,
                "cell {cell} is past the end of the line and draws nothing",
            );
        }
    }

    /// An empty hugging box has **no** cells and uploads nothing, rather than
    /// binding a zero-length texture: `content_cols` is `0` there, so the
    /// shader's `u_cols > 0` guard is the one that fires.
    ///
    /// **Falsified** by returning `Some` for an empty block.
    #[test]
    fn an_empty_box_uploads_nothing() {
        let layout = kit::TextBox::new().layout("");
        assert_eq!(layout.content_cols(), 0, "the premise: nothing to draw");
        let block = block(&layout);
        assert!(block.strip.is_none());
        let surface = textbox_surface(&layout, &block);
        assert_eq!(uniform(&surface.uniforms, "u_cols"), GlValue::Int(0));
        assert!(surface.uniforms.data.is_none());
        assert!(surface.width > 0 && surface.height > 0, "a bare field");
    }

    /// The **baked** palette reaches the uniforms — a pinned field, a pinned
    /// ink and a plugin's own `.notdef` all survive the mapping.
    ///
    /// This is the property the module docs are about: `TextBox` resolves its
    /// palette at construction, so a mapping that asked for a fresh
    /// `palette_snapshot` would quietly drop all three. The pet's and caw's
    /// bubbles are exactly that configuration (#884/#885).
    ///
    /// **Falsified** by resolving a palette inside `textbox_surface`.
    #[test]
    fn the_baked_palette_reaches_the_uniforms() {
        let field = [0x2a, 0x1e, 0x3c, 0xff];
        let ink = [0xf0, 0xd0, 0xff, 0xff];
        let notdef = [0x80, 0x60, 0xa0, 0x80];
        let boxed = kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Fixed(ink),
                field: Some(field),
            },
            || kit::TextBox::styled(kit::DisplayStyle::Vfd).notdef(notdef),
        );
        let layout = boxed.layout("hi");
        let surface = textbox_surface(&layout, &block(&layout));
        assert_eq!(uniform(&surface.uniforms, "u_bg"), super::channels(field));
        assert_eq!(uniform(&surface.uniforms, "u_ink"), super::channels(ink));
        assert_eq!(
            uniform(&surface.uniforms, "u_notdef"),
            super::channels(notdef),
        );
        assert_ne!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(kit::palette_snapshot(kit::DisplayStyle::Vfd).ink),
            "the premise: the pin actually moved the ink",
        );
    }

    /// The **shader** carries the kit's own font metrics and the same notdef
    /// bit this file flags with.
    ///
    /// A source read rather than an assertion about a value, because nothing in
    /// the tree compiles the GLSL until a driver does: `nix/lint-glsl.py`
    /// proves it *parses*, this proves it says the same thing. The four metrics
    /// are read straight off `hytte-preem`, so this is the whole chain for
    /// them.
    ///
    /// **Falsified** by changing any of the five on either side alone.
    #[test]
    fn the_shader_and_the_mapping_agree_about_the_font_metrics() {
        for (name, want) in [
            ("GLYPH_W", kit::font::GLYPH_W),
            ("GLYPH_H", kit::font::GLYPH_H),
            ("SPACING", kit::font::SPACING),
            ("LINE_GAP", kit::font::LINE_GAP),
            ("NOTDEF_BIT", NOTDEF_BIT as usize),
        ] {
            assert_eq!(
                glsl_int(name),
                want,
                "`{name}` disagrees between textbox.frag and the kit",
            );
        }
    }

    /// Every uniform the shader reads is filled by the mapping.
    ///
    /// The host publishes `u_grid`, `u_viewport`, `u_data_len` and
    /// `u_step_back` itself and binds `u_tex0`; everything else has to come
    /// from the table above, and a uniform the GLSL declares but nothing sets
    /// reads as zero — a transparent box, silently.
    ///
    /// **Falsified** by deleting any row from `textbox_surface`.
    #[test]
    fn the_mapping_fills_every_uniform_the_shader_reads() {
        let boxed = boxed();
        let layout = boxed.layout("hi");
        let surface = textbox_surface(&layout, &block(&layout));
        let set: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        let host = [
            "u_grid",
            "u_viewport",
            "u_data_len",
            "u_step_back",
            "u_tex0",
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

    /// The pipeline's shape: no aux textures, no step passes, one blit.
    ///
    /// **Falsified** by adding a pass, by asking for an aux texture nothing
    /// writes, or by pointing the one pass anywhere but the screen.
    #[test]
    fn the_pipeline_is_one_blit_and_nothing_else() {
        assert_eq!(TEXTBOX_PIPELINE.aux, 0, "no emission, so nothing offscreen");
        assert!(TEXTBOX_PIPELINE.step.is_empty(), "no cross-frame GPU state");
        assert_eq!(TEXTBOX_PIPELINE.frame.len(), 1);
        let pass = &TEXTBOX_PIPELINE.frame[0];
        assert!(matches!(pass.target, GlTarget::Screen));
        assert!(matches!(pass.blend, GlBlend::Replace));
        assert!(matches!(pass.draw, GlDraw::FullScreen));
        assert_eq!(pass.inputs.len(), 1);
        assert!(matches!(pass.inputs[0], GlInput::Data));
    }

    /// Read `const int <name> = <value>;` out of the shader source.
    fn glsl_int(name: &str) -> usize {
        let needle = format!("const int {name} = ");
        let line = BODY
            .lines()
            .find(|line| line.trim_start().starts_with(&needle))
            .unwrap_or_else(|| panic!("textbox.frag declares no `{name}`"));
        line.trim()
            .trim_start_matches(&needle)
            .trim_end_matches(';')
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("`{name}` is not an integer literal"))
    }

    fn uniform(uniforms: &GlUniforms, name: &str) -> GlValue {
        let (_, value) = uniforms
            .values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("no uniform named {name}"));
        *value
    }

    /// The `Block` is `PartialEq` so a mapping pass can dedup on it; this keeps
    /// that honest across a text change (and is what lets `preem_render`'s
    /// `update` re-encode only when the message actually moved).
    #[test]
    fn two_texts_encode_to_two_blocks() {
        let boxed = boxed();
        let a = boxed.layout("mrrp");
        let b = boxed.layout("purr");
        assert_ne!(block(&a), block(&b));
        assert_eq!(block(&a), block(&boxed.layout("mrrp")));
    }

    /// One strip texel as the `u32` bit set the shader reads back.
    fn texel(strip: &[f32], cell: usize, col: usize) -> u32 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let raw = strip[cell * kit::font::GLYPH_W + col] as u32;
        raw
    }
}
