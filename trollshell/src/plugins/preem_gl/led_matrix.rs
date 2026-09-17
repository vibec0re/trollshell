//! The `LedMatrix` GL program (#1156): the pipeline declaration, the per-lamp
//! data strip, and the pure `(panel, levels, scale, palette) → GlUniforms`
//! mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge),
//! [`dot_matrix`](super::dot_matrix), [`marquee`](super::marquee),
//! [`textbox`](super::textbox), [`led_strip`](super::led_strip),
//! [`seven_seg`](super::seven_seg) and [`flip_board`](super::flip_board) in
//! every structural way — it references nothing above itself either, so the
//! parity harness (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes
//! **this** file and measures the shell's own pipeline and mapping against the
//! CPU kit.
//!
//! # The one kind that is not on the wire
//!
//! Every other arm on this seam draws a `Node::Preem` a *plugin* sent. This one
//! draws a widget the **shell itself** rasterises: the Stats drawer's per-core
//! LED panel (`trollshell/src/panels/stats.rs`, #857), whose dressing is
//! `core-leds.toml` (#1040). `hytte_preem::LedMatrix` has no wire vocabulary at
//! all — that is #1156's first line, and the reason
//! [`Kind::on_the_wire`](super::kind::Kind::on_the_wire) exists.
//!
//! Nothing about the *arm* is special because of it: the pipeline registers
//! through [`Kind::gl_seam`](super::kind::Kind::gl_seam) with the other eight,
//! the mapping is pure, and the panel widget calls
//! [`led_matrix_surface`] the way `preem_render` calls its siblings. That is
//! deliberate — P3 of epic #1248 (#1252) moves this panel into
//! `hytte-plugin-stats`, and when it does, the widget wiring in `stats.rs` is
//! all that is deleted.
//!
//! # The lattice is the meter's, not the dot matrix's — measured
//!
//! #1156 asks whether this can reuse #1144's `dot_matrix` shader with a per-LED
//! brightness texture "if the geometry matches". Measured, it does not: see
//! `led_matrix.frag`'s header for the five ways the two lattices differ. What
//! it reuses instead is [`led_strip`](super::led_strip)'s shape — one pass,
//! `aux: 0`, the kit's separable box blur **solved in closed form** because the
//! emission is a union of axis-aligned rectangles — extended to two dimensions,
//! with the two things a panel has that a meter does not arriving through
//! `GlUniforms::data`: a per-lamp brightness and a per-lamp ink.
//!
//! # Everything geometric is a uniform or a texel, not a GLSL literal
//!
//! `led_matrix.frag` declares no lamp metric of its own: `u_cell`, `u_gap` and
//! `u_pad` come straight off `hytte-preem`'s [`kit::LED_MATRIX_CELL`] and
//! friends (`pub` since #1156, on the [`kit::LED_CELL_W`]/`Gauge::dial`
//! precedent), the grid shape off `LedMatrix::cols`/`rows`, the ghost count off
//! [`kit::LedMatrix::ghost_slots`], every lamp's brightness off
//! [`kit::LedMatrix::lamp_intensities`] and every lamp's ink off
//! [`kit::LedMatrix::lamp_inks`] — which is the colour axis *and* the
//! spare-slot clamp resolved by the kit rather than restated in GLSL. The only
//! numbers this shader restates are the CRT pass's four fixed-point constants,
//! which every shader on this seam restates and which
//! [`program::assert_crt_constants`](super::program::assert_crt_constants)
//! holds to the kit's own items.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_preem as kit;

use super::program::{FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `LedMatrix` pipeline.
pub(crate) const LED_MATRIX: GlProgram = GlProgram("preem.led_matrix");

/// The one fragment program. Not spliced: there is a single layer, so there is
/// nothing for a `const int LAYER` to select between.
const LED_MATRIX_FRAG: &str = include_str!("led_matrix.frag");

/// The `LedMatrix` pipeline: one fullscreen blit to the screen, reading only
/// the lamp strip.
pub(crate) const LED_MATRIX_PIPELINE: GlPipeline = GlPipeline {
    // No emission texture and no halo texture — see the module docs on why the
    // blur has a closed form for this widget's geometry. The one input is the
    // per-lamp strip, which is state rather than scratch.
    aux: 0,
    step: &[],
    frame: &[GlPass {
        vertex: FULLSCREEN_VERT,
        fragment: LED_MATRIX_FRAG,
        target: GlTarget::Screen,
        inputs: &[GlInput::Data],
        blend: GlBlend::Replace,
        draw: GlDraw::FullScreen,
    }],
};

/// Floats per slot in the data strip — the lamp's `0..=255` brightness followed
/// by its ink's three channels.
///
/// Mirrored in `led_matrix.frag` as `LAMP_STRIDE` and pinned against it by a
/// source scan, since a stride the two halves disagree about reads the ink of
/// the wrong lamp rather than failing.
pub(crate) const LAMP_STRIDE: usize = 4;

/// The per-lamp strip `GlUniforms::data` carries: [`LAMP_STRIDE`] texels per
/// slot, row-major from the top-left, `cols * rows` slots.
///
/// Both halves are the kit's own answers rather than anything resolved here —
/// [`kit::LedMatrix::lamp_intensities`] for the brightness (so `intensity`'s
/// clamp, its round-to-nearest and its `NaN` rule have one implementation) and
/// [`kit::LedMatrix::lamp_inks`] for the ink (so the colour axis and the
/// spare-slot clamp do too).
///
/// Never `None`: `LedMatrix` clamps both dimensions to at least 1, so there is
/// always a slot, and the degenerate "no strip" case the other kinds have
/// (`u_data_len == 0`) is unreachable for this widget. The shader's
/// out-of-range guard is kept anyway, because a texel read is a texel read.
pub(crate) fn lamps(panel: &kit::LedMatrix, levels: &[f32], ink: kit::Rgba) -> Arc<[f32]> {
    let amounts = panel.lamp_intensities(levels);
    let inks = panel.lamp_inks(levels, ink);
    let mut strip = Vec::with_capacity(amounts.len() * LAMP_STRIDE);
    for (amount, ink) in amounts.iter().zip(&inks) {
        strip.push(f32::from(*amount));
        strip.push(f32::from(ink[0]));
        strip.push(f32::from(ink[1]));
        strip.push(f32::from(ink[2]));
    }
    strip.into()
}

/// Map one `LedMatrix` and the levels it is showing onto the GL node payload.
///
/// **Pure**, exactly as [`led_strip_surface`](super::led_strip::led_strip_surface)
/// and its siblings are: it reads no globals, resolves no palette and touches
/// no GL. The caller passes the palette it has already resolved inside whatever
/// pin scope applies, so accent / role / pin precedence stays the kit's one
/// implementation — and passes it on to [`kit::LedMatrix::lamp_inks`], which is
/// why that function takes an `ink` rather than reading `self.style.palette()`.
///
/// `scale` is the shell's **integer upscale** — `core_panel_scale`'s answer,
/// which is what `PixelSurface::set_scale` replicates on the CPU arm. It
/// multiplies the natural size and leaves the grid alone (the scope's and the
/// flip board's arrangement, not the dot matrix's), so a shipping panel always
/// draws through the continuous branch, which is where the improvement is.
pub(crate) fn led_matrix_surface(
    panel: &kit::LedMatrix,
    levels: &[f32],
    scale: u32,
    palette: &kit::PaletteSnapshot,
) -> KitSurface {
    let width = u32_of(panel.width());
    let height = u32_of(panel.height());
    let scale = scale.max(1);

    // A skin with no bloom reaches the shader as radius 0 / strength 0, which
    // `halo255` answers `0` for — the same early return `Emission::bloom`
    // takes, rather than the "run the blur and let the max absorb it"
    // equivalence the multi-pass pipelines rely on. `LedMatrix::render` hands
    // `Emission::bloom` the skin's own `Bloom`, untouched, once.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;

    KitSurface {
        width: width.saturating_mul(scale),
        height: height.saturating_mul(scale),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                (
                    "u_cells",
                    GlValue::Ivec2([int_of(panel.cols()), int_of(panel.rows())]),
                ),
                // The ghost count arrives as the kit's own answer, not as a
                // `Fill` for the shader to re-interpret: `Fill::Spare` vs
                // `Fill::Blank` is decided once, in `ghost_slots`.
                (
                    "u_ghost_slots",
                    GlValue::Int(int_of(panel.ghost_slots(levels.len()))),
                ),
                ("u_cell", GlValue::Int(int_of(kit::LED_MATRIX_CELL))),
                ("u_gap", GlValue::Int(int_of(kit::LED_MATRIX_GAP))),
                ("u_pad", GlValue::Int(int_of(kit::LED_MATRIX_PAD))),
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
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                // **Not re-phased**, unlike the two dot surfaces (#1091):
                // `LedMatrix::render` hands `Emission::composite` the skin's
                // own `palette.mask`, which is what `Mask::with_pitch`'s doc
                // lists this widget under.
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
            data: Some(lamps(panel, levels, palette.ink)),
            // The kit's own pre-upscale buffer — the panel's size knob is its
            // lamp count, and `scale` above is the shell's replication factor.
            grid: (width, height),
            // No step passes, so nothing counts steps. See [`LED_MATRIX_PIPELINE`].
            step_seq: 0,
        },
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
        GlBlend, GlDraw, GlInput, GlTarget, GlUniforms, GlValue, LAMP_STRIDE, LED_MATRIX_PIPELINE,
        kit, lamps, led_matrix_surface,
    };

    /// The shader body — the source the scans below read back.
    const BODY: &str = include_str!("led_matrix.frag");

    /// The shipping panel shape: the wide rectangle `stats.rs` builds for
    /// `cells` cores, in `style`, with the `core-leds.toml` default colour axis.
    fn panel(style: kit::DisplayStyle, cells: usize) -> kit::LedMatrix {
        kit::LedMatrix::wide(style, cells).color(kit::ColorMap::Heat)
    }

    /// A deterministic per-core load ramp — `n` levels spanning the range.
    #[allow(clippy::cast_precision_loss)]
    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32 / n.max(1) as f32).collect()
    }

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
    /// That is the whole reason those three constants became `pub`: an `8` here
    /// would agree with the kit today and keep agreeing after someone widened a
    /// lamp, which is a mirror agreeing with itself. The names are the contract
    /// with the GLSL — a rename on one side alone draws nothing and says
    /// nothing — and the order is pinned so a reordering shows as a diff.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row, by
    /// spelling a metric as a literal and then moving the kit's, or by handing
    /// `u_ghost_slots` anything but [`kit::LedMatrix::ghost_slots`]' answer.
    #[test]
    fn the_uniform_table_is_the_kits_own_metrics() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
        let panel = panel(kit::DisplayStyle::Crt, 16);
        let surface = led_matrix_surface(&panel, &ramp(16), 2, &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_cells",
                "u_ghost_slots",
                "u_cell",
                "u_gap",
                "u_pad",
                "u_ghost_on",
                "u_ghost",
                "u_bloom_radius",
                "u_bloom_strength",
                "u_bg",
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
        assert_eq!(
            uniform(&surface.uniforms, "u_cells"),
            GlValue::Ivec2([
                i32::try_from(panel.cols()).unwrap(),
                i32::try_from(panel.rows()).unwrap(),
            ]),
        );
        assert_eq!(
            int("u_ghost_slots"),
            i32::try_from(panel.ghost_slots(16)).unwrap(),
        );
        // The three lattice metrics, as the kit's items rather than as 8/3/4.
        assert_eq!(int("u_cell"), i32::try_from(kit::LED_MATRIX_CELL).unwrap());
        assert_eq!(int("u_gap"), i32::try_from(kit::LED_MATRIX_GAP).unwrap());
        assert_eq!(int("u_pad"), i32::try_from(kit::LED_MATRIX_PAD).unwrap());

        assert_eq!(int("u_ghost_on"), i32::from(palette.ghost.is_some()));
        let bloom = palette.bloom.expect("the CRT skin glows");
        assert_eq!(int("u_bloom_radius"), i32::try_from(bloom.radius).unwrap());
        assert_eq!(int("u_bloom_strength"), i32::from(bloom.strength));
        assert_eq!(
            uniform(&surface.uniforms, "u_bg"),
            super::channels(palette.bg),
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
    }

    /// **The grid is the kit's own buffer and the natural size is
    /// `grid × scale`** — the scope's and the flip board's arrangement, because
    /// the replication happens outside the kit (`PixelSurface::set_scale`) and
    /// so is the GL arm's to resolve per fragment.
    ///
    /// **Falsified** by transcribing the width formula here instead of calling
    /// the kit, and then moving `LED_MATRIX_GAP`; or by baking `scale` into the
    /// grid, which would put a shipping panel back on the snapped branch and
    /// silently delete the whole improvement.
    #[test]
    fn the_grid_is_the_kits_buffer_and_the_scale_only_multiplies_it() {
        for cells in [1_usize, 4, 16, 64, 128] {
            for scale in [1_u32, 3] {
                let panel = panel(kit::DisplayStyle::Vfd, cells);
                let surface = led_matrix_surface(
                    &panel,
                    &ramp(cells),
                    scale,
                    &kit::palette_snapshot(kit::DisplayStyle::Vfd),
                );
                let frame = panel.render(&ramp(cells));
                assert_eq!(
                    surface.uniforms.grid,
                    (
                        u32::try_from(frame.width()).unwrap(),
                        u32::try_from(frame.height()).unwrap(),
                    ),
                    "{cells} lamps at {scale}x",
                );
                assert_eq!(
                    (surface.width, surface.height),
                    (
                        surface.uniforms.grid.0 * scale,
                        surface.uniforms.grid.1 * scale,
                    ),
                );
            }
        }
        // A zero scale is a caller bug, not a degenerate buffer: it clamps up.
        let panel = panel(kit::DisplayStyle::Vfd, 4);
        let zero = led_matrix_surface(
            &panel,
            &ramp(4),
            0,
            &kit::palette_snapshot(kit::DisplayStyle::Vfd),
        );
        assert_eq!((zero.width, zero.height), zero.uniforms.grid);
    }

    /// **The strip is the kit's own per-lamp answers**, both halves, at
    /// [`LAMP_STRIDE`] floats a slot.
    ///
    /// This is the only place the two arms could disagree about *which* lamp is
    /// how bright or what colour, and it is checked against
    /// `lamp_intensities`/`lamp_inks` rather than against a recomputation — an
    /// independent ramp here would be a second implementation of the colour
    /// axis, which is exactly what #1156 exists not to have.
    ///
    /// **Falsified** by interleaving the two halves differently, by dropping
    /// the spare slots, or by writing the ink before the amount.
    #[test]
    #[allow(clippy::float_cmp)]
    fn the_strip_carries_the_kits_own_lamp_amounts_and_inks() {
        for style in kit::DisplayStyle::ALL {
            let palette = kit::palette_snapshot(style);
            // 22 x 3 = 66 slots for 64 lamps: a ragged last row, so the spare
            // slots' clamp is exercised rather than reasoned about.
            let panel = kit::LedMatrix::new(style, 22, 3)
                .color(kit::ColorMap::Heat)
                .fill(kit::Fill::Blank);
            let levels = ramp(64);
            let strip = lamps(&panel, &levels, palette.ink);
            let amounts = panel.lamp_intensities(&levels);
            let inks = panel.lamp_inks(&levels, palette.ink);
            assert_eq!(strip.len(), 66 * LAMP_STRIDE);
            assert_eq!(amounts.len(), 66);
            for slot in 0..66 {
                let at = slot * LAMP_STRIDE;
                assert_eq!(
                    strip[at],
                    f32::from(amounts[slot]),
                    "{style:?} slot {slot} brightness",
                );
                for channel in 0..3 {
                    assert_eq!(
                        strip[at + 1 + channel],
                        f32::from(inks[slot][channel]),
                        "{style:?} slot {slot} channel {channel}",
                    );
                }
            }
            // …and the two spare slots are dark but keep the last lamp's ink,
            // which is what makes a neighbour's halo the right colour there.
            assert_eq!(strip[64 * LAMP_STRIDE], 0.0);
            assert_eq!(strip[65 * LAMP_STRIDE], 0.0);
            assert_eq!(
                &strip[64 * LAMP_STRIDE + 1..64 * LAMP_STRIDE + 4],
                &strip[63 * LAMP_STRIDE + 1..63 * LAMP_STRIDE + 4],
            );
        }
    }

    /// **An unfed panel still ships a full strip** — the state the Stats drawer
    /// is in before its first `sensors::cpu()` tick.
    ///
    /// `LedMatrix` clamps both dimensions to at least 1, so `data` is never
    /// `None` for this widget and `u_data_len` is never `0`; the shader's
    /// out-of-range guard is a guard, not a code path the shell reaches.
    #[test]
    fn an_unfed_panel_still_ships_a_strip() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        let panel = panel(kit::DisplayStyle::Vfd, 8);
        let surface = led_matrix_surface(&panel, &[], 1, &palette);
        let strip = surface
            .uniforms
            .data
            .clone()
            .expect("a panel always has slots");
        assert_eq!(strip.len(), panel.cols() * panel.rows() * LAMP_STRIDE);
        assert!(
            strip
                .iter()
                .step_by(LAMP_STRIDE)
                .all(|amount| *amount == 0.0),
            "no level means no lamp is lit",
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_ghost_slots"),
            GlValue::Int(i32::try_from(panel.cols() * panel.rows()).unwrap()),
            "`Fill::Spare` still ghosts the whole grid with nothing fed",
        );
    }

    /// **`Fill` reaches the shader as a count, not as a flag** — and the two
    /// arms differ by exactly the slots the kit says.
    #[test]
    fn the_fill_policy_reaches_the_shader_as_the_kits_slot_count() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        let levels = ramp(64);
        for (fill, want) in [(kit::Fill::Spare, 66), (kit::Fill::Blank, 64)] {
            let panel = kit::LedMatrix::new(kit::DisplayStyle::Vfd, 22, 3).fill(fill);
            let surface = led_matrix_surface(&panel, &levels, 1, &palette);
            assert_eq!(
                uniform(&surface.uniforms, "u_ghost_slots"),
                GlValue::Int(want),
                "{fill:?}",
            );
        }
    }

    /// **One pass, no aux textures, one input, no steps** — the structural
    /// claim the closed-form blur buys, asserted rather than described.
    ///
    /// **Falsified** by any of: an `aux` above `0`, a second frame pass, a
    /// second `GlInput`, or a target other than the screen.
    #[test]
    fn the_pipeline_is_one_pass_over_one_strip() {
        assert_eq!(LED_MATRIX_PIPELINE.aux, 0);
        assert!(LED_MATRIX_PIPELINE.step.is_empty());
        assert_eq!(LED_MATRIX_PIPELINE.frame.len(), 1);
        let pass = LED_MATRIX_PIPELINE.frame[0];
        assert_eq!(pass.target, GlTarget::Screen);
        assert_eq!(pass.inputs, &[GlInput::Data]);
        assert_eq!(pass.blend, GlBlend::Replace);
        assert_eq!(pass.draw, GlDraw::FullScreen);
    }

    /// **The shader declares the CRT pass's four constants with the kit's own
    /// values** — the shared #1186 helper.
    #[test]
    fn the_shader_declares_the_kits_crt_constants() {
        super::super::program::assert_crt_constants("led_matrix.frag", BODY);
    }

    /// **The shader takes its lattice geometry from uniforms, never from a
    /// literal of its own** — `led_strip.frag`'s rule, and the reason the three
    /// metrics became `pub`.
    ///
    /// **Falsified** by inlining any metric (a `const int CELL = 8;` beside the
    /// CRT block, or a bare `8.0` where `float(u_cell)` stands).
    #[test]
    fn the_shader_reads_its_geometry_from_uniforms() {
        for name in ["u_cell", "u_gap", "u_pad", "u_ghost_slots", "u_data_len"] {
            assert!(
                BODY.contains(&format!("uniform int {name};")),
                "led_matrix.frag must declare `uniform int {name};`",
            );
        }
        assert!(BODY.contains("uniform ivec2 u_cells;"));
        // The only `const int`s in the file are the CRT four plus the loop
        // bound and the strip stride; anything else is a metric that stopped
        // being a uniform.
        let declared: Vec<&str> = BODY
            .lines()
            .filter_map(|line| line.trim().strip_prefix("const int "))
            .filter_map(|rest| rest.split_whitespace().next())
            .collect();
        assert_eq!(
            declared,
            vec![
                "MASK_ONE",
                "COORD_ONE",
                "BAND_DIV",
                "CORNER_DIV",
                "MAX_SPAN_CELLS",
                "LAMP_STRIDE",
            ],
            "led_matrix.frag grew a compile-time constant — a lattice metric \
             belongs in the uniform bag, where it is the kit's own item",
        );
        assert!(
            BODY.contains(&format!("const int LAMP_STRIDE = {LAMP_STRIDE};")),
            "the strip stride the shader indexes with must be this module's — \
             a disagreement reads the wrong lamp's ink rather than failing",
        );
    }

    /// **The shader's arithmetic, transcribed here, reproduces the kit byte for
    /// byte at 1:1 — on every skin, every colour axis, every fill and every
    /// grid shape this widget can take.**
    ///
    /// This is the test that actually earns the bit-exact pin, and it needs no
    /// GL at all — `led_strip.rs`'s reasoning, in this widget's shape.
    /// [`shader_frame`] below is a line-by-line Rust transcription of
    /// `led_matrix.frag`'s snapped branch and is compared against
    /// `LedMatrix::render`'s own bytes.
    ///
    /// **This arm's closed form is one step harder than the meter's, and that
    /// is what the sweep is for.** The meter's emission is a single intensity
    /// on a union of rectangles, so its horizontal pass is one measure. A
    /// panel's is a *different* intensity per rectangle, so the horizontal pass
    /// is a weighted measure and — the part a plausible wrong implementation
    /// gets wrong — the kit's `u16` `tmp` grid **truncates once per row** before
    /// the vertical pass ever sees it. Hoisting that `floor` out of the row loop
    /// is the natural simplification and it is not the kit.
    ///
    /// The transcription is held to the **shipped** GLSL by
    /// [`the_mirror_is_the_shipped_shaders_arithmetic`]. Neither replaces
    /// `preem_gl_diff` — a driver can still disagree with both.
    ///
    /// **Falsified** by any of: hoisting the per-row `floor` out of the row
    /// loop, folding the two blur passes into one 2-D kernel, dividing in
    /// floats instead of truncating, renormalising the window where it clips at
    /// a buffer edge, resolving the ink at `p` instead of at the buffer pixel,
    /// or dropping the `+ 127` from `mix_kit`.
    ///
    /// The whole sweep runs inside one [`kit::with_pins`] scope pinning
    /// [`kit::Ink::Base`], for `led_strip.rs`'s reason: the kit's accent is a
    /// process-global `AtomicU32` that both `palette_snapshot` (here) and
    /// `LedMatrix::render` (the oracle) read at render time, and the suite runs
    /// concurrently.
    #[test]
    fn the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                for style in kit::DisplayStyle::ALL {
                    for color in [
                        kit::ColorMap::Style,
                        kit::ColorMap::Heat,
                        kit::ColorMap::Rainbow,
                        kit::ColorMap::TransPride,
                        kit::ColorMap::Rgb(0x2a, 0xd0, 0x7f),
                    ] {
                        for fill in [kit::Fill::Spare, kit::Fill::Blank] {
                            for (cols, rows, fed) in
                                [(1, 1, 1), (4, 1, 4), (8, 2, 8), (22, 3, 64), (5, 4, 12)]
                            {
                                let panel = kit::LedMatrix::new(style, cols, rows)
                                    .color(color)
                                    .fill(fill);
                                for levels in [ramp(fed), vec![0.0; fed], vec![1.0; fed], vec![]] {
                                    let reference = panel.render(&levels);
                                    let mirror = shader_frame(style, &panel, &levels);
                                    assert_eq!(
                                        mirror,
                                        reference.data(),
                                        "{style:?} {color:?} {fill:?} {cols}x{rows} fed={} \
                                         levels={}",
                                        fed,
                                        levels.len(),
                                    );
                                }
                            }
                        }
                    }
                }
            },
        );
    }

    /// **The transcription above is the arithmetic the shipped shader carries**
    /// — a source scan over `led_matrix.frag`, so a fix applied to one side only
    /// reds here instead of leaving a green mirror describing a shader nobody
    /// ships.
    ///
    /// Pins every clause the mirror transcribes: the lattice walk, the
    /// footprint integral and its `+ 0.5`, the closed-form blur's window
    /// clipping on both axes, **the per-row `floor` inside the row loop**, the
    /// outer `floor`, the strength scaling, the bloom's early return, the
    /// `min(max(…), 255)` combine, the ghost composite, the ink lookup at the
    /// buffer pixel, `mix_kit`'s rounding, and `MAX_SPAN_CELLS`' own value.
    #[test]
    fn the_mirror_is_the_shipped_shaders_arithmetic() {
        for clause in [
            // the footprint's own half-widths, which the mirror transcribes as
            // `p.0 ± 0.5` at 1:1 (#1156 review, LOW 4). Without these four the
            // whitelist has a hole exactly where a `0.5 -> 0.25` typo lives:
            // the mirror is untouched by it, so the hermetic suite stays green
            // and only the llvmpipe 1:1 pin in CI catches it.
            "float lo_x = p.x - fp.x * 0.5;",
            "float hi_x = p.x + fp.x * 0.5;",
            "float lo_y = p.y - fp.y * 0.5;",
            "float hi_y = p.y + fp.y * 0.5;",
            // the lattice
            "float advance() {",
            "return float(u_cell + u_gap);",
            "return int(floor((lo - float(u_pad)) / advance()));",
            "float x0 = float(u_pad) + float(i) * advance();",
            "return overlap(lo, hi, x0, x0 + float(u_cell));",
            "return min(max(p - u_pad, 0) / (u_cell + u_gap), cells - 1);",
            // the footprint integral
            "sum += float(amount_at(c, r)) * cell_overlap(lo_x, hi_x, c) * oy;",
            "return int(sum / (fp.x * fp.y) + 0.5);",
            "return int(255.0 * sum / (fp.x * fp.y) + 0.5);",
            "if (r * u_cells.x + c >= u_ghost_slots) {",
            // the closed-form blur
            "if (u_bloom_radius <= 0 || u_bloom_strength <= 0) {",
            "float win = float(2 * u_bloom_radius + 1);",
            "float lo_x = max(p.x - half_win, 0.0);",
            "float hi_x = min(p.x + half_win, float(u_grid.x));",
            "float lo_y = max(p.y - half_win, 0.0);",
            "float hi_y = min(p.y + half_win, float(u_grid.y));",
            "row_sum += float(amount_at(c, r)) * cell_overlap(lo_x, hi_x, c);",
            "blurred += floor(row_sum / win) * rows;",
            "int out255 = int(floor(blurred / win));",
            "return min(out255 * u_bloom_strength / 256, 255);",
            // the composite
            "int lit = min(max(stamp255(p, fp), halo255(p)), 255);",
            "under = mix_kit(bg, ivec4(u_ghost + 0.5), ghost);",
            "lit = lit * mask_keep(col, row, cols, rows) / MASK_ONE;",
            "under = mix_kit(under, ink_at(index_at(p.x, u_cells.x), \
             index_at(p.y, u_cells.y)), lit);",
            "return (a * (255 - k) + b * k + 127) / 255;",
            // the loop bound, by value
            "const int MAX_SPAN_CELLS = 8;",
        ] {
            assert!(
                BODY.contains(clause),
                "led_matrix.frag no longer carries `{clause}` — the mirror in \
                 this file transcribes it",
            );
        }
        // …and the per-row truncation is *inside* the row loop, which is the
        // one ordering the kit states and a mirror could silently hoist.
        let row_loop = BODY
            .find("float row_sum = 0.0;")
            .expect("the per-row accumulator");
        let row_floor = BODY
            .find("blurred += floor(row_sum / win) * rows;")
            .expect("the per-row truncation");
        let outer_floor = BODY
            .find("int out255 = int(floor(blurred / win));")
            .expect("the vertical truncation");
        assert!(
            row_loop < row_floor && row_floor < outer_floor,
            "the horizontal pass truncates once per row, before the vertical \
             pass sees it",
        );
    }

    // ── the transcription ───────────────────────────────────────────────────

    /// `led_matrix.frag`'s snapped branch, in Rust: one whole frame, top-down
    /// RGBA8, exactly as `Frame::data` lays it out.
    ///
    /// Everything below mirrors the GLSL statement for statement, `f32` for
    /// `float` and `i32` for `int`, which is what makes a disagreement with the
    /// kit attributable to the shader rather than to this file.
    #[allow(clippy::many_single_char_names)]
    fn shader_frame(style: kit::DisplayStyle, panel: &kit::LedMatrix, levels: &[f32]) -> Vec<u8> {
        let palette = kit::palette_snapshot(style);
        let (w, h) = (panel.width(), panel.height());
        let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
            radius: 0,
            strength: 0,
        });
        let cells = (panel.cols(), panel.rows());
        let amounts = panel.lamp_intensities(levels);
        let inks = panel.lamp_inks(levels, palette.ink);
        let ghost_slots = panel.ghost_slots(levels.len());

        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                #[allow(clippy::cast_precision_loss)]
                let p = (col as f32 + 0.5, row as f32 + 0.5);
                let mut under = palette.bg;
                if let Some(ghost) = palette.ghost {
                    let t = ghost255(p, cells, ghost_slots);
                    if t > 0 {
                        under = mix_kit(under, ghost, t);
                    }
                }
                let mut lit = stamp255(p, cells, &amounts)
                    .max(halo255(p, (w, h), cells, &amounts, bloom))
                    .min(255);
                if lit > 0 {
                    if let Some(mask) = palette.mask {
                        // `kit::MASK_ONE`, not `256`: the shipped line spells
                        // `/ MASK_ONE` and `mask_keep` below already scales to
                        // the kit's item, so agreeing with it by reference is
                        // the point (#1156 review, LOW 2).
                        lit = lit * mask_keep(col, row, w, h, mask)
                            / i32::try_from(kit::MASK_ONE).expect("MASK_ONE fits i32");
                    }
                    if lit > 0 {
                        let c = index_at(p.0, cells.0);
                        let r = index_at(p.1, cells.1);
                        under = mix_kit(under, inks[r * cells.0 + c], lit);
                    }
                }
                out.extend_from_slice(&[under[0], under[1], under[2], 0xff]);
            }
        }
        out
    }

    /// `overlap`.
    fn overlap(lo: f32, hi: f32, a: f32, b: f32) -> f32 {
        (hi.min(b) - lo.max(a)).max(0.0)
    }

    /// `advance`.
    #[allow(clippy::cast_precision_loss)]
    fn advance() -> f32 {
        (kit::LED_MATRIX_CELL + kit::LED_MATRIX_GAP) as f32
    }

    /// `first_index`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn first_index(lo: f32) -> i32 {
        ((lo - kit::LED_MATRIX_PAD as f32) / advance()).floor() as i32
    }

    /// `cell_overlap`.
    #[allow(clippy::cast_precision_loss)]
    fn cell_overlap(lo: f32, hi: f32, i: i32) -> f32 {
        let x0 = kit::LED_MATRIX_PAD as f32 + i as f32 * advance();
        overlap(lo, hi, x0, x0 + kit::LED_MATRIX_CELL as f32)
    }

    /// `index_at`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn index_at(pos: f32, cells: usize) -> usize {
        let p = pos.floor().max(0.0) as usize;
        (p.saturating_sub(kit::LED_MATRIX_PAD) / (kit::LED_MATRIX_CELL + kit::LED_MATRIX_GAP))
            .min(cells - 1)
    }

    /// The span of lattice indices `[lo, hi]` can touch on one axis, clamped to
    /// the grid — the four `first_c`/`last_c`/`first_r`/`last_r` lines.
    fn span(lo: f32, hi: f32, cells: usize) -> (i32, i32) {
        let last = i32::try_from(cells).unwrap_or(i32::MAX) - 1;
        (first_index(lo).max(0), first_index(hi).min(last))
    }

    /// `amount_at`.
    fn amount_at(c: i32, r: i32, cells: (usize, usize), amounts: &[u16]) -> i32 {
        let (cols, rows) = (
            i32::try_from(cells.0).unwrap_or(i32::MAX),
            i32::try_from(cells.1).unwrap_or(i32::MAX),
        );
        if c < 0 || r < 0 || c >= cols || r >= rows {
            return 0;
        }
        let index = usize::try_from(r * cols + c).unwrap_or(usize::MAX);
        amounts.get(index).map_or(0, |a| i32::from(*a))
    }

    /// `stamp255` — at 1:1 the footprint is exactly one buffer pixel.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn stamp255(p: (f32, f32), cells: (usize, usize), amounts: &[u16]) -> i32 {
        let (lo_x, hi_x) = (p.0 - 0.5, p.0 + 0.5);
        let (lo_y, hi_y) = (p.1 - 0.5, p.1 + 0.5);
        let (first_c, last_c) = span(lo_x, hi_x, cells.0);
        let (first_r, last_r) = span(lo_y, hi_y, cells.1);
        let mut sum = 0.0f32;
        let mut r = first_r;
        while r <= last_r && r - first_r < 8 {
            let oy = cell_overlap(lo_y, hi_y, r);
            if oy > 0.0 {
                let mut c = first_c;
                while c <= last_c && c - first_c < 8 {
                    sum +=
                        amount_at(c, r, cells, amounts) as f32 * cell_overlap(lo_x, hi_x, c) * oy;
                    c += 1;
                }
            }
            r += 1;
        }
        (sum + 0.5) as i32
    }

    /// `ghost255`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn ghost255(p: (f32, f32), cells: (usize, usize), ghost_slots: usize) -> i32 {
        let (lo_x, hi_x) = (p.0 - 0.5, p.0 + 0.5);
        let (lo_y, hi_y) = (p.1 - 0.5, p.1 + 0.5);
        let (first_c, last_c) = span(lo_x, hi_x, cells.0);
        let (first_r, last_r) = span(lo_y, hi_y, cells.1);
        let slots = i32::try_from(ghost_slots).unwrap_or(i32::MAX);
        let cols = i32::try_from(cells.0).unwrap_or(i32::MAX);
        let mut sum = 0.0f32;
        let mut r = first_r;
        while r <= last_r && r - first_r < 8 {
            let oy = cell_overlap(lo_y, hi_y, r);
            if oy > 0.0 {
                let mut c = first_c;
                while c <= last_c && c - first_c < 8 {
                    if r * cols + c < slots {
                        sum += cell_overlap(lo_x, hi_x, c) * oy;
                    }
                    c += 1;
                }
            }
            r += 1;
        }
        (255.0 * sum + 0.5) as i32
    }

    /// `halo255`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    fn halo255(
        p: (f32, f32),
        grid: (usize, usize),
        cells: (usize, usize),
        amounts: &[u16],
        bloom: kit::BloomSnapshot,
    ) -> i32 {
        if bloom.radius == 0 || bloom.strength == 0 {
            return 0;
        }
        let win = (2 * bloom.radius + 1) as f32;
        let half_win = win * 0.5;
        let lo_x = (p.0 - half_win).max(0.0);
        let hi_x = (p.0 + half_win).min(grid.0 as f32);
        let lo_y = (p.1 - half_win).max(0.0);
        let hi_y = (p.1 + half_win).min(grid.1 as f32);
        let (first_c, last_c) = span(lo_x, hi_x, cells.0);
        let (first_r, last_r) = span(lo_y, hi_y, cells.1);
        let mut blurred = 0.0f32;
        let mut r = first_r;
        while r <= last_r && r - first_r < 8 {
            let rows = cell_overlap(lo_y, hi_y, r);
            if rows > 0.0 {
                let mut row_sum = 0.0f32;
                let mut c = first_c;
                while c <= last_c && c - first_c < 8 {
                    row_sum += amount_at(c, r, cells, amounts) as f32 * cell_overlap(lo_x, hi_x, c);
                    c += 1;
                }
                blurred += (row_sum / win).floor() * rows;
            }
            r += 1;
        }
        let out255 = (blurred / win).floor() as i32;
        (out255 * i32::from(bloom.strength) / 256).min(255)
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
        let mut out = [0u8; 4];
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
