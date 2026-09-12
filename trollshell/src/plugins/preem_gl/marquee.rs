//! The `Marquee` GL program (#1152): the pipeline alias, the per-frame grid
//! upload and the pure `(strip, window, palette) → GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge) and
//! [`dot_matrix`](super::dot_matrix) in every structural way — it references
//! nothing above itself either, so the parity harness
//! (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this** file and
//! measures the shell's own mapping against the CPU kit.
//!
//! # There is no marquee shader
//!
//! [`MARQUEE_PIPELINE`] *is* [`dot_matrix::DOT_MATRIX_PIPELINE`], by
//! definition rather than by copy. A marquee is the **same dot hardware** — the
//! kit says so in as many words (`hytte-preem/src/marquee.rs`: "The dot
//! *hardware* is `dot_matrix`'s — same pitch, same bezel, same falloff
//! painters, all carried in one shared `Dots` value — so a scrolled dot is
//! pixel-for-pixel a static one") — and every pass of that pipeline is about
//! the dot lattice, the halo and the composite, none of which knows what the
//! lit columns mean.
//!
//! What differs is the **grid**, and it is three numbers: a marquee's is a
//! continuous ticker matrix rather than a row of character cells, centred in
//! its window rather than inset by the bezel. #1152 made those three
//! (`u_origin_x`, `u_cell_cols`, `u_cell_gap`) uniforms of the shared
//! `site_at`, so the dot matrix passes `(dot, GLYPH_W, SPACING)` — the literals
//! the shader carried before — and this module passes `(origin_x, 1, 0)`:
//! every "cell" is one dot column, and there is no gap to skip.
//!
//! A second [`GlProgram`] name rather than reusing `"preem.dot_matrix"` so a
//! journal line, a `RUST_LOG=hytte_ui=debug` trace and a `Resources` rebuild
//! all say which widget is on screen. It costs one entry in the host's
//! registry and nothing else: `hytte-ui` compiles programs **per surface**, so
//! two names over one pipeline compile exactly what two surfaces would have
//! compiled anyway.
//!
//! # The scroll is a texture upload, not a uniform
//!
//! The kit slides the message **one whole dot at a time** — #839 made a sub-dot
//! position inexpressible, and `MarqueeStrip::window` takes its offset in dots
//! — so there is no fractional phase for the shader to interpolate and no
//! offset uniform at all. Each visible step re-uploads the grid's `cols`
//! columns at the new phase, which is what [`Window`] holds.
//!
//! That upload is the kit's own: [`kit::MarqueeStrip::window_columns`] is an
//! **additive** `pub` accessor #1152 added to `hytte-preem` (the
//! [`kit::dot_cell`] precedent — plain data, nothing in the kit reads it, no
//! render path changed to produce it), and `MarqueeStrip::window` is written in
//! terms of it. So the loop wrap, the seam gap and the
//! [hold rule](kit::MarqueeStrip::scrolls) are resolved **once**, in the kit,
//! and this module never re-implements `(offset + col) % period`.
//!
//! # What is better than the kit
//!
//! Exactly what [`dot_matrix`](super::dot_matrix) improved, for exactly its
//! reason, because it is the same blit: the lit layer is the falloff *law*
//! evaluated at the fragment's own position on the lattice rather than a
//! `dot_px`×`dot_px` block replicated out of a fixed table, so a ticker given
//! more room than its natural size draws round dots at the screen's resolution.
//! At 1:1 the shader snaps to the pixel centre and the float path *is* the
//! integer path.

use std::sync::Arc;

use hytte::ui::gl_surface::{GlPipeline, GlProgram, GlUniforms, GlValue};
use hytte_preem as kit;

use super::dot_matrix::DOT_MATRIX_PIPELINE;
use super::program::{KitSurface, channels};

/// The registered name of the `Marquee` pipeline.
pub(crate) const MARQUEE: GlProgram = GlProgram("preem.marquee");

/// The `Marquee` pipeline — **the dot matrix's**, aliased rather than copied.
///
/// See the module docs. Written as an alias so a change to the dot matrix's
/// pass list reaches the ticker in the same commit, which a duplicated literal
/// could not promise.
pub(crate) const MARQUEE_PIPELINE: GlPipeline = DOT_MATRIX_PIPELINE;

/// The visible grid at one scroll phase, as the shader consumes it.
///
/// Built when the phase (or the message) moves and shared by every monitor's
/// mapping pass, the way `Renderer::ScopeGl`'s `samples` and
/// `Renderer::DotMatrixGl`'s `Glyphs` are (#911's rule, for uniforms): the
/// `Arc` makes a repeat mapping's dedup a pointer compare rather than a second
/// walk of the window.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Window {
    /// Dot cells across the window's fixed grid — `MarqueeStrip::cols`.
    pub(crate) cols: usize,
    /// One texel per **grid column**, `cols` of them, each holding that
    /// column's [`kit::font::GLYPH_H`] row bits with bit `row` set for a lit
    /// dot. `None` for a window with no grid at all (a surface narrower than
    /// its own bezels), which is what `GlUniforms::data` wants there.
    pub(crate) strip: Option<Arc<[f32]>>,
}

/// Encode the grid at `offset` for the shader — the pure half of the mapping.
///
/// Goes through [`kit::MarqueeStrip::window_columns`], so the wrap, the loop
/// gap and the hold rule are the kit's own answers rather than a second
/// implementation of them here.
pub(crate) fn window(strip: &kit::MarqueeStrip, offset: usize) -> Window {
    let columns = strip.window_columns(offset);
    if columns.is_empty() {
        return Window {
            cols: 0,
            strip: None,
        };
    }
    let texels: Vec<f32> = columns.into_iter().map(f32::from).collect();
    Window {
        cols: texels.len(),
        strip: Some(Arc::from(&texels[..])),
    }
}

/// Map one `Marquee`'s rasterised strip and current window onto the GL node
/// payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface),
/// [`gauge_surface`](super::gauge::gauge_surface) and
/// [`dot_matrix_surface`](super::dot_matrix::dot_matrix_surface) are: it reads
/// no globals, resolves no palette and touches no GL. The caller passes the
/// palette it has already resolved *inside the widget's `with_pins` scope*, so
/// accent / role / pin precedence stays the kit's one implementation.
///
/// It takes no `MarqueeConfig` at all, and that is the point of the accessors:
/// the window width, the dot pitch, the grid's column count and its centred
/// origin are all things the **rasterised strip** already decided, and reading
/// them back off it is what keeps the two arms drawing on one grid.
pub(crate) fn marquee_surface(
    strip: &kit::MarqueeStrip,
    window: &Window,
    palette: &kit::PaletteSnapshot,
) -> KitSurface {
    let dot = strip.dot_px();
    let (width, height) = (strip.width(), strip.height());

    // Identical to the dot matrix's, and deliberately so — `Marquee::render`
    // hands `Emission::bloom` the skin's own `Bloom` untouched, exactly as
    // `DotMatrix::render` does.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;
    // **Re-phased onto this strip's dot grid**, which the kit does at its own
    // `composite` call site ("the two surfaces share the hardware, so they
    // share the tube's alignment to it"). `dot` is never zero — the kit clamps
    // the pitch at construction.
    let mask_pitch = mask.map_or(0, |_| dot);
    let mask_phase = mask.map_or(0, |_| dot - 1);

    KitSurface {
        width: u32_of(width),
        height: u32_of(height),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            // It is deliberately the dot matrix's order, name for name: the two
            // mappings feed one shader and a reader comparing them should be
            // able to do it line by line.
            values: vec![
                ("u_dot", GlValue::Int(int_of(dot))),
                // A "cell" is one dot column here — the continuous ticker
                // matrix — so the grid's group count *is* its column count.
                ("u_cells", GlValue::Int(int_of(window.cols))),
                // The **centred** grid origin, not the bezel: a window width
                // that is not a whole number of dots widens the margin on both
                // sides rather than clipping a dot, so this is
                // `(window_px - cols*dot) / 2` and can exceed `dot`.
                ("u_origin_x", GlValue::Int(int_of(strip.origin_x()))),
                ("u_cell_cols", GlValue::Int(1)),
                ("u_cell_gap", GlValue::Int(0)),
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
            // The grid at this scroll phase. Shared rather than rebuilt per
            // mapping pass — see [`Window`].
            data: window.strip.clone(),
            // The window the kit would have produced, which is already native:
            // the dot pitch is the size knob and the window width is stated in
            // final buffer pixels, so there is no `scale` to multiply (#1091).
            grid: (u32_of(width), u32_of(height)),
            // No step passes, so nothing counts steps. The scroll is a texture
            // upload, not GPU state — see the module docs.
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
        DOT_MATRIX_PIPELINE, GlUniforms, GlValue, MARQUEE_PIPELINE, kit, marquee_surface, window,
    };

    /// A strip on the kit's own default hardware, in `style`.
    fn strip(style: kit::DisplayStyle, text: &str) -> kit::MarqueeStrip {
        kit::Marquee::new(style).window_px(96).render(text)
    }

    /// A message that overflows a 96 px window at the default pitch, so it
    /// actually scrolls.
    const LONG: &str = "PREEM RASTER KIT ~ SCROLLING TICKER ~ ";

    /// **The ticker runs the dot matrix's pipeline**, not a copy of it.
    ///
    /// The alias is the whole argument of this module: a marquee is the same
    /// dot hardware on a different grid, so a second pass list would be a
    /// duplicate that drifts — and a drift *there* is a halo that does not sit
    /// on the dots it came from, on one of the two widgets only.
    ///
    /// `GlPipeline` is `Copy` with no `PartialEq`, so this compares what a
    /// reader would: the aux count, the pass count, and each pass's fragment
    /// source **by pointer**, which is what proves they are one `&'static str`
    /// rather than two equal ones.
    ///
    /// **Falsified** by re-declaring `MARQUEE_PIPELINE` as its own literal.
    #[test]
    fn the_marquee_runs_the_dot_matrix_pipeline_itself() {
        assert_eq!(MARQUEE_PIPELINE.aux, DOT_MATRIX_PIPELINE.aux);
        assert_eq!(MARQUEE_PIPELINE.step.len(), DOT_MATRIX_PIPELINE.step.len());
        assert_eq!(
            MARQUEE_PIPELINE.frame.len(),
            DOT_MATRIX_PIPELINE.frame.len(),
        );
        for (ours, theirs) in MARQUEE_PIPELINE
            .frame
            .iter()
            .zip(DOT_MATRIX_PIPELINE.frame.iter())
        {
            assert!(
                std::ptr::eq(ours.fragment, theirs.fragment),
                "one shader source, not two equal ones",
            );
            assert!(std::ptr::eq(ours.vertex, theirs.vertex));
        }
    }

    /// **The grid is continuous**: one dot column per "cell", no gap, and the
    /// centred origin rather than the bezel.
    ///
    /// These three are the whole of what #1152 added to `site_at`, and they are
    /// the whole of what makes one shader draw two widgets. The dot matrix's
    /// half is
    /// `dot_matrix::tests::the_uniform_table_is_the_kits_own_numbers`.
    ///
    /// **Falsified** by passing `GLYPH_W`/`SPACING` here, or by passing
    /// `dot` (the bezel) as the origin: a 96 px window at pitch 4 has a 22-cell
    /// grid and a **4 px** origin, but at 97 px it is still 22 cells and the
    /// origin moves to 4 while the bezel does not — the third assertion is the
    /// one that separates them.
    #[test]
    fn the_marquee_drives_the_lattice_as_a_continuous_grid() {
        let strip = strip(kit::DisplayStyle::Vfd, LONG);
        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        let surface = marquee_surface(&strip, &window(&strip, 0), &palette);
        assert_eq!(uniform(&surface.uniforms, "u_cell_cols"), GlValue::Int(1));
        assert_eq!(uniform(&surface.uniforms, "u_cell_gap"), GlValue::Int(0));
        assert_eq!(
            uniform(&surface.uniforms, "u_origin_x"),
            GlValue::Int(i32::try_from(strip.origin_x()).unwrap()),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_cells"),
            GlValue::Int(i32::try_from(strip.cols()).unwrap()),
            "one group per grid column",
        );

        // A window that is not a whole number of dots: the grid stays the same
        // size and the margin widens, so the origin is no longer the bezel.
        let odd = kit::Marquee::new(kit::DisplayStyle::Vfd)
            .window_px(99)
            .render(LONG);
        assert_eq!(odd.cols(), strip.cols(), "the premise: the same grid");
        assert_ne!(
            odd.origin_x(),
            odd.dot_px(),
            "the premise: the origin is not the bezel here",
        );
        let surface = marquee_surface(&odd, &window(&odd, 0), &palette);
        assert_eq!(
            uniform(&surface.uniforms, "u_origin_x"),
            GlValue::Int(i32::try_from(odd.origin_x()).unwrap()),
        );
    }

    /// **The golden uniform table**: every name, in order, with the values a
    /// known ticker resolves to.
    ///
    /// The names are the contract with the GLSL — and here they are also the
    /// contract with the *dot matrix's* mapping, since both feed one shader, so
    /// the assertion is written as "the dot matrix's names, exactly".
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row on
    /// either side.
    #[test]
    fn the_uniform_table_is_the_dot_matrixs_own_names() {
        let style = kit::DisplayStyle::Crt;
        let palette = kit::palette_snapshot(style);
        let strip = strip(style, LONG);
        let surface = marquee_surface(&strip, &window(&strip, 3), &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_dot",
                "u_cells",
                "u_origin_x",
                "u_cell_cols",
                "u_cell_gap",
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
        assert_eq!(
            uniform(&surface.uniforms, "u_dot"),
            GlValue::Int(i32::try_from(strip.dot_px()).unwrap()),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_bg"),
            super::channels(palette.bg),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_ink"),
            super::channels(palette.ink),
        );
        // The comb is re-phased onto the dot grid, not the skin's own pitch.
        assert_eq!(
            uniform(&surface.uniforms, "u_mask_pitch"),
            GlValue::Int(i32::try_from(strip.dot_px()).unwrap()),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_mask_phase"),
            GlValue::Int(i32::try_from(strip.dot_px() - 1).unwrap()),
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
        assert_eq!(
            (surface.width, surface.height),
            (96, 36),
            "the kit's own window and dot height",
        );
        assert_eq!(surface.uniforms.grid, (96, 36));
    }

    /// **The uploaded grid is the kit's own window columns** — and it moves
    /// with the phase, which is the entire animation of this widget.
    ///
    /// The shader has no offset uniform: #839 made a sub-dot position
    /// inexpressible in the kit, so a scroll step is a *different set of lit
    /// columns* rather than a shifted sample coordinate. This asserts both
    /// halves — that the texels are `window_columns`' bytes, and that two
    /// different phases really do upload different data.
    ///
    /// **Falsified** by encoding the bitmap instead of the window, by dropping
    /// the offset argument, or by reversing the column order.
    #[test]
    fn the_uploaded_grid_is_the_kits_window_at_this_phase() {
        let strip = strip(kit::DisplayStyle::Lcd, LONG);
        assert!(strip.scrolls(), "the premise: this message scrolls");
        for offset in [0, 1, 5, strip.period() - 1, strip.period(), 9_999] {
            let window = window(&strip, offset);
            assert_eq!(window.cols, strip.cols());
            let texels = window.strip.expect("a grid of 22 columns");
            let want: Vec<f32> = strip
                .window_columns(offset)
                .into_iter()
                .map(f32::from)
                .collect();
            assert_eq!(&texels[..], &want[..], "@ {offset}");
        }
        assert_ne!(
            window(&strip, 0).strip,
            window(&strip, 1).strip,
            "one dot of scroll is a different grid",
        );
    }

    /// A held message — one short enough to fit the grid — ignores the offset
    /// entirely, so every phase uploads the **same** grid and the GL arm has
    /// nothing to redraw.
    ///
    /// That is the kit's [hold rule](kit::MarqueeStrip::scrolls) reaching the
    /// GPU for free, because the rule lives in `window_columns` rather than
    /// here.
    ///
    /// **Falsified** by wrapping the offset against `cols` instead of asking
    /// the kit.
    #[test]
    fn a_held_message_uploads_one_grid_at_every_phase() {
        let strip = strip(kit::DisplayStyle::Lcd, "HI");
        assert!(!strip.scrolls(), "the premise: 'HI' fits a 22-cell grid");
        let first = window(&strip, 0);
        for offset in [1, 7, 9_999] {
            assert_eq!(window(&strip, offset), first, "@ {offset}");
        }
    }

    /// An empty window — a surface too narrow to hold a single dot cell between
    /// its bezels — uploads nothing and declares no cells, rather than binding
    /// a zero-length texture.
    ///
    /// `GlUniforms::data` wants `None` there (it binds a 1×1 zero texture and
    /// sets `u_data_len` to `0`), and `site_at` refuses every fragment on
    /// `cellf >= u_cells`, so the frame is the field and the bezel — which is
    /// exactly what the kit renders.
    ///
    /// **Falsified** by returning `Some` for an empty column list.
    #[test]
    fn a_gridless_window_uploads_nothing() {
        let narrow = kit::Marquee::new(kit::DisplayStyle::Vfd)
            .window_px(8)
            .render(LONG);
        assert_eq!(narrow.cols(), 0, "the premise: no grid fits");
        let window = window(&narrow, 0);
        assert_eq!(window.cols, 0);
        assert!(window.strip.is_none());
        let surface = marquee_surface(
            &narrow,
            &window,
            &kit::palette_snapshot(kit::DisplayStyle::Vfd),
        );
        assert_eq!(uniform(&surface.uniforms, "u_cells"), GlValue::Int(0));
        assert!(surface.uniforms.data.is_none());
    }

    /// **A pinned ink reaches the GL arm** — the mapping composites toward the
    /// palette the caller resolved, never toward the skin's own.
    ///
    /// It matters more here than on the static display: the marquee is the one
    /// kit widget whose *state* change re-bakes a palette (`preem_render`'s
    /// `update` opens a second `with_pins` scope for a new message), so an arm
    /// that re-derived its own palette would disagree with the CPU arm only
    /// after a text change.
    ///
    /// **Falsified** by calling `kit::palette_snapshot` inside
    /// `marquee_surface` instead of taking the argument.
    #[test]
    fn a_pinned_ink_reaches_the_uniforms() {
        let pinned = kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Fixed([0xff, 0x00, 0x00, 0xff]),
                field: None,
            },
            || kit::palette_snapshot(kit::DisplayStyle::Vfd),
        );
        let strip = strip(kit::DisplayStyle::Vfd, LONG);
        let surface = marquee_surface(&strip, &window(&strip, 0), &pinned);
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

    fn uniform(uniforms: &GlUniforms, name: &str) -> GlValue {
        let (_, value) = uniforms
            .values
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .unwrap_or_else(|| panic!("no uniform named {name}"));
        *value
    }
}
