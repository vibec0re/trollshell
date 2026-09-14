//! The `LedStrip` GL program (#1153): the pipeline declaration and the pure
//! `(config, level, peak, palette) → GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program), [`gauge`](super::gauge),
//! [`dot_matrix`](super::dot_matrix), [`marquee`](super::marquee) and
//! [`textbox`](super::textbox) in every structural way — it references nothing
//! above itself either, so the parity harness
//! (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this** file
//! and measures the shell's own pipeline and mapping against the CPU kit.
//!
//! # The smallest pipeline on the seam, and the first with no texture at all
//!
//! **One pass, `aux: 0`, no inputs, no step passes.** The text box (#1152) was
//! the smallest before this one and still binds the glyph strip; this widget
//! binds nothing. That is not a simplification of the kit — the strip has a
//! ghost row, a bloom and the CRT pass, all three of which
//! [`dot_matrix`](super::dot_matrix) needs three aux textures and four passes
//! for — it is a consequence of what the kit's emission *is* here.
//!
//! The kit's bloom is a separable, truncating box blur of an intensity grid.
//! Where that grid is a union of axis-aligned rectangles at a single intensity
//! — which is exactly what `stamp_cell` produces — each of the two passes is a
//! one-dimensional **measure** rather than a sum over samples, so the whole
//! blur has a closed form: `floor(255 · L / win)` horizontally and
//! `floor(tmp · M / win)` vertically, with `L` the lit length inside the
//! clipped window and `M` the number of window rows inside the segment band.
//! Two integrals and two floors, evaluated at the fragment. So there is nothing
//! to render off-screen and nothing to read back — and, unlike #1186's bilinear
//! tap, nothing is interpolated: a stretched chip's halo is *computed* at the
//! screen's resolution rather than resampled from the kit's grid.
//!
//! # What is better than the kit
//!
//! Two things, and they are the two the nearest-neighbour upscale replicates:
//! the segment **edge** (a box-filter coverage of the fragment's own footprint,
//! so a chip given a width that is not a whole multiple of the buffer's keeps
//! every LED the same proportion instead of making some a pixel wider) and the
//! **halo** above. At 1:1 both collapse onto the kit's integers exactly — see
//! `led_strip.frag`'s header for why that is arithmetic rather than luck — so
//! the 1:1 cases are pinned bit-exact.
//!
//! # What is *not* here: round LEDs
//!
//! #1153's title asks for "round LEDs … as SDF discs". The kit's segments are
//! `LED_CELL_W`×`LED_CELL_H` = 8×16 **bars** (`stamp_cell` fills a rectangle),
//! which is the VU-meter shape this widget has always had, and the same issue
//! asks for the 1:1 comparison to be pinned bit-exact against that kit. Those
//! two asks are not jointly satisfiable: a disc and a bar disagree at the first
//! corner pixel, so a disc here would fail its own parity pin. This arm draws
//! the kit's bar, evaluated as a continuous shape; making the *kit's* segments
//! round is a change to `hytte-preem` and a decision for the issue thread.
//!
//! # Everything geometric is a uniform, not a GLSL literal
//!
//! `led_strip.frag` declares no segment metric of its own: `u_cell_w`,
//! `u_cell_h`, `u_gap` and `u_pad` come straight off `hytte-preem`'s
//! [`kit::LED_CELL_W`] and friends (`pub` since #1153, on the
//! [`kit::dot_cell`]/`Gauge::dial` precedent), the level reaches the shader as
//! [`kit::led_lit_count`]'s answer rather than as a float to re-round, the peak
//! dot as [`kit::led_peak_led`]'s index, the grid as [`kit::led_strip_size`]'s
//! buffer and the dot's cap as [`kit::led_cap_ink`]'s quad. The only numbers
//! this shader restates are the CRT pass's four fixed-point constants, which
//! every shader on this seam restates and which
//! [`program::assert_crt_constants`](super::program::assert_crt_constants)
//! holds to the kit's own items.

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

use super::program::{FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `LedStrip` pipeline.
pub(crate) const LED_STRIP: GlProgram = GlProgram("preem.led_strip");

/// The one fragment program. Not spliced: there is a single layer, so there is
/// nothing for a `const int LAYER` to select between.
const LED_STRIP_FRAG: &str = include_str!("led_strip.frag");

/// The `LedStrip` pipeline: one fullscreen blit to the screen, reading nothing.
pub(crate) const LED_STRIP_PIPELINE: GlPipeline = GlPipeline {
    // No emission texture and no halo texture — see the module docs on why the
    // blur has a closed form for this widget's geometry.
    aux: 0,
    step: &[],
    frame: &[GlPass {
        vertex: FULLSCREEN_VERT,
        fragment: LED_STRIP_FRAG,
        target: GlTarget::Screen,
        // The only pipeline on this seam that samples nothing at all: the whole
        // state is nineteen uniforms.
        inputs: &[],
        blend: GlBlend::Replace,
        draw: GlDraw::FullScreen,
    }],
};

/// The `u_peak` value for "no dot" — what [`kit::led_peak_led`] answers `None`
/// for (a rested, negative or `NaN` peak). Any non-negative value is a segment
/// index, so a sentinel below zero needs no second uniform.
const NO_PEAK: i32 = -1;

/// The segment count the kit would render, under `LedStrip::leds`' own clamp.
fn leds(config: vocab::LedStripConfig) -> usize {
    usize::try_from(config.leds)
        .unwrap_or(kit::MAX_LEDS)
        .clamp(1, kit::MAX_LEDS)
}

/// Map one `LedStrip`'s already-clamped config and its current level/peak onto
/// the GL node payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface),
/// [`gauge_surface`](super::gauge::gauge_surface) and
/// [`dot_matrix_surface`](super::dot_matrix::dot_matrix_surface) are: it reads
/// no globals, resolves no palette and touches no GL. The caller passes the
/// palette it has already resolved *inside the widget's `with_pins` scope*, so
/// accent / role / pin precedence stays the kit's one implementation.
///
/// `peak` is the value the shell has already resolved — the plugin's explicit
/// peak when it sent one, else the shell-held decaying value, else `0.0` — so
/// **no decay logic crosses into the shader**. The pump folds `PeakHoldConfig`
/// on its own tick (`hytte-plugin-proto`'s `preem` module says so in as many
/// words), and both arms read the same folded number.
pub(crate) fn led_strip_surface(
    config: vocab::LedStripConfig,
    level: f32,
    peak: f32,
    palette: &kit::PaletteSnapshot,
) -> KitSurface {
    let leds = leds(config);
    let (width, height) = kit::led_strip_size(leds);

    // A skin with no bloom reaches the shader as radius 0 / strength 0, which
    // `halo255` answers `0` for — the same early return `Emission::bloom`
    // takes, rather than the "run the blur and let the max absorb it"
    // equivalence the multi-pass pipelines rely on. Unlike the gauge there is
    // no halving and no cap: `LedStrip::render` hands `Emission::bloom` the
    // skin's own `Bloom`, untouched, once per layer.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;

    KitSurface {
        width: u32_of(width),
        height: u32_of(height),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                ("u_leds", GlValue::Int(int_of(leds))),
                // The level and the peak arrive as the kit's own answers, not
                // as floats for the shader to re-round: `lit_count`'s
                // round-to-nearest and `peak_led`'s ceil-then-clamp (and both
                // of their `NaN` rules) have one implementation this way.
                (
                    "u_lit",
                    GlValue::Int(int_of(kit::led_lit_count(level, leds))),
                ),
                (
                    "u_peak",
                    GlValue::Int(kit::led_peak_led(peak, leds).map_or(NO_PEAK, int_of)),
                ),
                ("u_cell_w", GlValue::Int(int_of(kit::LED_CELL_W))),
                ("u_cell_h", GlValue::Int(int_of(kit::LED_CELL_H))),
                ("u_gap", GlValue::Int(int_of(kit::LED_GAP))),
                ("u_pad", GlValue::Int(int_of(kit::LED_PAD))),
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
                // The peak dot's brightened ink, resolved by the kit's own
                // `cap_ink` so the `CAP_MIX` toward white lives in one place.
                ("u_cap", channels(kit::led_cap_ink(palette.ink))),
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                // **Not re-phased**, unlike the two dot surfaces (#1091): this
                // widget has no dot grid for the comb to sit in the seams of,
                // so it keeps `Mask::CRT` exactly as the skin states it —
                // which is what `Mask::with_pitch`'s own doc lists it under.
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
            // Nothing to sample: the pass declares no inputs.
            data: None,
            // The buffer the kit would have produced, which for this widget is
            // already the native one: the segment metrics are the size knob, so
            // there is no `scale` to multiply (the dot matrix's situation,
            // #1091).
            grid: (u32_of(width), u32_of(height)),
            // No step passes, so nothing counts steps. See [`LED_STRIP_PIPELINE`].
            step_seq: 0,
        },
    }
}

/// A buffer dimension as a `u32`, saturating. Every value reaching this is
/// bounded by `MAX_LEDS`, far below `u32::MAX`.
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
        GlBlend, GlDraw, GlTarget, GlUniforms, GlValue, LED_STRIP_PIPELINE, NO_PEAK, kit,
        led_strip_surface, vocab,
    };

    /// The shader body — the source the scans below read back.
    const BODY: &str = include_str!("led_strip.frag");

    fn config(leds: u32) -> vocab::LedStripConfig {
        vocab::LedStripConfig {
            leds,
            ..vocab::LedStripConfig::default()
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

    /// **The uniform table is the kit's own metrics**, name for name and value
    /// for value — and every geometric row is written as the kit item it comes
    /// from, never as the literal it currently equals (the #1164 shape).
    ///
    /// That is the whole reason those four constants became `pub`: an `8` here
    /// would agree with the kit today and keep agreeing after someone widened a
    /// segment, which is a mirror agreeing with itself. The names are the
    /// contract with the GLSL — a rename on one side alone draws nothing and
    /// says nothing — and the order is pinned so a reordering shows as a diff.
    ///
    /// **Falsified** by adding, removing, renaming or reordering any row, by
    /// spelling a metric as a literal and then moving the kit's, or by handing
    /// `u_lit`/`u_peak` anything but the kit's own answers.
    #[test]
    fn the_uniform_table_is_the_kits_own_metrics() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
        let surface = led_strip_surface(config(9), 0.5, 0.75, &palette);

        let names: Vec<&str> = surface.uniforms.values.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            vec![
                "u_leds",
                "u_lit",
                "u_peak",
                "u_cell_w",
                "u_cell_h",
                "u_gap",
                "u_pad",
                "u_ghost_on",
                "u_ghost",
                "u_bloom_radius",
                "u_bloom_strength",
                "u_bg",
                "u_ink",
                "u_cap",
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
        assert_eq!(int("u_leds"), 9);
        assert_eq!(
            int("u_lit"),
            i32::try_from(kit::led_lit_count(0.5, 9)).unwrap(),
        );
        assert_eq!(
            int("u_peak"),
            i32::try_from(kit::led_peak_led(0.75, 9).unwrap()).unwrap(),
        );
        // The four segment metrics, as the kit's items rather than as 8/16/3/4.
        assert_eq!(int("u_cell_w"), i32::try_from(kit::LED_CELL_W).unwrap());
        assert_eq!(int("u_cell_h"), i32::try_from(kit::LED_CELL_H).unwrap());
        assert_eq!(int("u_gap"), i32::try_from(kit::LED_GAP).unwrap());
        assert_eq!(int("u_pad"), i32::try_from(kit::LED_PAD).unwrap());

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
        assert_eq!(
            uniform(&surface.uniforms, "u_cap"),
            super::channels(kit::led_cap_ink(palette.ink)),
            "the peak dot's cap is the kit's own `cap_ink`, not a second mix",
        );
        assert_eq!(surface.uniforms.step_seq, 0, "no cross-frame GPU state");
        assert!(
            surface.uniforms.data.is_none(),
            "this widget samples nothing"
        );
    }

    /// **The grid and the natural size are `led_strip_size`'s buffer** — the
    /// one the kit actually renders, with no upscale to multiply.
    ///
    /// **Falsified** by transcribing the width formula here instead of calling
    /// the kit, and then moving `LED_GAP`.
    #[test]
    fn the_grid_is_the_kits_own_buffer() {
        for leds in [1_u32, 7, 24, 128] {
            let surface = led_strip_surface(
                config(leds),
                1.0,
                1.0,
                &kit::palette_snapshot(kit::DisplayStyle::Vfd),
            );
            let frame = kit::LedStrip::new(kit::DisplayStyle::Vfd)
                .leds(leds as usize)
                .render(1.0, 1.0);
            assert_eq!(
                (surface.width as usize, surface.height as usize),
                (frame.width(), frame.height()),
                "{leds} LEDs",
            );
            assert_eq!(surface.uniforms.grid, (surface.width, surface.height));
        }
    }

    /// **The segment count takes `LedStrip::leds`' own clamp**, at both ends.
    ///
    /// The wire already clamps (`clamp_in_place`), so this is the second line
    /// of the same defence — and the one that matters for the *grid*, since a
    /// zero count would otherwise ask for a `2*PAD - GAP` buffer the kit never
    /// produces.
    #[test]
    fn the_segment_count_takes_the_kits_clamp() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Vfd);
        let zero = led_strip_surface(config(0), 1.0, 0.0, &palette);
        let one = led_strip_surface(config(1), 1.0, 0.0, &palette);
        assert_eq!(zero.width, one.width, "0 clamps up to exactly 1 LED");

        let at = led_strip_surface(
            config(u32::try_from(kit::MAX_LEDS).unwrap()),
            1.0,
            0.0,
            &palette,
        );
        let over = led_strip_surface(config(u32::MAX), 1.0, 0.0, &palette);
        assert_eq!(at.width, over.width, "u32::MAX clamps down to MAX_LEDS");
    }

    /// **A peak the kit marks no LED for reaches the shader as [`NO_PEAK`]**,
    /// and every peak it does mark reaches it as that index.
    ///
    /// `peak_led`'s three "no dot" inputs — rested, negative and `NaN` — are
    /// the kit's decision, and a sentinel is how a `None` crosses a uniform
    /// bag that carries no options.
    ///
    /// **Falsified** by mapping `None` to `0`, which would light the first
    /// segment's cap on every silent frame.
    #[test]
    fn a_rested_peak_reaches_the_shader_as_no_dot() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Oled);
        let peak_of = |peak: f32| match uniform(
            &led_strip_surface(config(24), 0.0, peak, &palette).uniforms,
            "u_peak",
        ) {
            GlValue::Int(value) => value,
            other => panic!("u_peak is {other:?}"),
        };
        assert_eq!(peak_of(0.0), NO_PEAK, "a rested peak marks no LED");
        assert_eq!(peak_of(-1.0), NO_PEAK, "a negative peak marks no LED");
        assert_eq!(peak_of(f32::NAN), NO_PEAK, "NaN marks no LED");
        assert_eq!(peak_of(0.01), 0, "a whisper marks the first LED");
        assert_eq!(peak_of(1.0), 23, "a full peak marks the top LED");
        // …and the mapping is `peak_led`'s, across the range, rather than a
        // second rounding rule that happens to agree at the ends.
        for step in 1..=100 {
            let peak = f32::from(u8::try_from(step).unwrap()) / 100.0;
            assert_eq!(
                peak_of(peak),
                i32::try_from(kit::led_peak_led(peak, 24).unwrap()).unwrap(),
            );
        }
    }

    /// **The CRT comb is the skin's, not the segment pitch** — this widget is
    /// on `Mask::with_pitch`'s "untouched" list (#1091), unlike the two dot
    /// surfaces, because it has no dot grid for a comb to sit in the seams of.
    ///
    /// **Falsified** by re-phasing onto `LED_CELL_H` or the segment pitch,
    /// which is the copy-paste a reader of `dot_matrix_surface` would make.
    #[test]
    fn the_comb_keeps_the_skins_own_pitch() {
        let palette = kit::palette_snapshot(kit::DisplayStyle::Crt);
        let mask = palette.mask.expect("the CRT skin masks");
        let surface = led_strip_surface(config(24), 0.5, 0.0, &palette);
        assert_eq!(
            uniform(&surface.uniforms, "u_mask_pitch"),
            GlValue::Int(i32::try_from(mask.pitch).unwrap()),
        );
        assert_eq!(
            uniform(&surface.uniforms, "u_mask_phase"),
            GlValue::Int(i32::try_from(mask.phase).unwrap()),
        );
        assert_eq!(uniform(&surface.uniforms, "u_mask_on"), GlValue::Int(1));

        // …and a skin with no mask reaches the shader switched off, with the
        // three numbers zeroed rather than left at a previous skin's.
        let flat = led_strip_surface(
            config(24),
            0.5,
            0.0,
            &kit::palette_snapshot(kit::DisplayStyle::Lcd),
        );
        assert_eq!(uniform(&flat.uniforms, "u_mask_on"), GlValue::Int(0));
        assert_eq!(uniform(&flat.uniforms, "u_mask_pitch"), GlValue::Int(0));
        assert_eq!(uniform(&flat.uniforms, "u_bloom_radius"), GlValue::Int(0));
        assert_eq!(uniform(&flat.uniforms, "u_bloom_strength"), GlValue::Int(0));
    }

    /// **One pass, no aux textures, no inputs, no steps** — the structural
    /// claim the closed-form blur buys, asserted rather than described.
    ///
    /// **Falsified** by any of: an `aux` above `0`, a second frame pass, a
    /// `GlInput` in the list, or a target other than the screen.
    #[test]
    fn the_pipeline_is_one_pass_that_samples_nothing() {
        assert_eq!(LED_STRIP_PIPELINE.aux, 0);
        assert!(LED_STRIP_PIPELINE.step.is_empty());
        assert_eq!(LED_STRIP_PIPELINE.frame.len(), 1);
        let pass = LED_STRIP_PIPELINE.frame[0];
        assert_eq!(pass.target, GlTarget::Screen);
        assert!(pass.inputs.is_empty());
        assert_eq!(pass.blend, GlBlend::Replace);
        assert_eq!(pass.draw, GlDraw::FullScreen);
    }

    /// **The shader declares the CRT pass's four constants with the kit's own
    /// values** — the shared #1186 helper, called here for the reason it exists
    /// (a `.frag` cannot read a Rust `const`, so the copy must be checked).
    #[test]
    fn the_shader_declares_the_kits_crt_constants() {
        super::super::program::assert_crt_constants("led_strip.frag", BODY);
    }

    /// **The shader takes its segment geometry from uniforms, never from a
    /// literal of its own.**
    ///
    /// This is the #1164 shape adapted to a shader whose mirrored constants are
    /// *uniforms*: `assert_scaled_lengths` pins a kit constant through the
    /// multiply that scales it, because the gauge's lengths have to be GLSL
    /// literals; here they do not, so the stronger statement available is that
    /// no such literal exists — the four metrics are read by name and the
    /// shader declares only the CRT four.
    ///
    /// **Falsified** by inlining any metric (a `const int CELL_W = 8;` beside
    /// the CRT block, or a bare `8.0` where `float(u_cell_w)` stands), which
    /// would let the kit's own value move without moving what CI compiles.
    #[test]
    fn the_shader_reads_its_geometry_from_uniforms() {
        for name in ["u_cell_w", "u_cell_h", "u_gap", "u_pad"] {
            assert!(
                BODY.contains(&format!("uniform int {name};")),
                "led_strip.frag must declare `uniform int {name};`",
            );
        }
        // The only `const int`s in the file are the CRT four plus the loop
        // bound; anything else is a metric that stopped being a uniform.
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
            ],
            "led_strip.frag grew a compile-time constant — a segment metric \
             belongs in the uniform bag, where it is the kit's own item",
        );
    }

    /// **The shader's arithmetic, transcribed here, reproduces the kit byte for
    /// byte at 1:1 — on every skin, at every level and peak this widget can
    /// take.**
    ///
    /// This is the test that actually earns the bit-exact pin, and it needs no
    /// GL at all. [`shader_frame`] below is a line-by-line Rust transcription of
    /// `led_strip.frag`'s snapped branch — the same closed-form blur, the same
    /// two `floor`s, the same `mix_kit`, the same CRT pass, in the same order —
    /// and it is compared against `LedStrip::render`'s own bytes. A closed-form
    /// box blur that is subtly *not* the kit's separable truncating one is the
    /// single most plausible way this arm could be wrong, and llvmpipe is not
    /// available to `cargo test`.
    ///
    /// The transcription is held to the **shipped** GLSL by
    /// [`the_mirror_is_the_shipped_shaders_arithmetic`], so the two cannot
    /// drift silently: this test says the arithmetic is right, that one says it
    /// is the arithmetic that ships. Neither replaces `preem_gl_diff` — a
    /// driver can still disagree with both — which is why the harness runs the
    /// same states through a real context.
    ///
    /// **Falsified** by any of: folding the two blur passes into one 2-D
    /// kernel, dividing in floats instead of truncating, renormalising the
    /// window where it clips at a buffer edge, compositing the peak dot before
    /// the level, or dropping the `+ 127` from `mix_kit`.
    ///
    /// The whole sweep runs inside one [`kit::with_pins`] scope pinning
    /// [`kit::Ink::Base`], which is what keeps it hermetic *without* reaching
    /// for `plugins::tests`' process-wide ink mutex — this module is
    /// `#[path]`-included by the harness and deliberately references nothing
    /// above itself. The kit's accent is a process-global `AtomicU32` that
    /// **both** `palette_snapshot` (here) and `LedStrip::render` (the oracle)
    /// read at render time, and the suite runs concurrently, so a flip landing
    /// between the two calls would make them differ for a reason that is not
    /// this code. `with_pins` is thread-local and `Ink::Base` ignores the
    /// accent outright, so both sides resolve the same palette by
    /// construction. Measured: without it this test fails roughly one run in
    /// three, against the two tests that write the global.
    #[test]
    fn the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one() {
        kit::with_pins(
            kit::Pins {
                ink: kit::Ink::Base,
                field: None,
            },
            || {
                for style in kit::DisplayStyle::ALL {
                    for leds in [1_usize, 7, 24] {
                        for &(level, peak) in &[
                            (0.0_f32, 0.0_f32),
                            (0.5, 0.0),
                            (1.0, 0.0),
                            (0.35, 0.8),
                            (0.6, 0.6),
                            (1.0, 1.0),
                            (0.04, 0.04),
                        ] {
                            let reference =
                                kit::LedStrip::new(style).leds(leds).render(level, peak);
                            let mirror = shader_frame(style, leds, level, peak);
                            assert_eq!(
                                mirror,
                                reference.data(),
                                "{style:?} leds={leds} level={level} peak={peak}",
                            );
                        }
                    }
                }
            },
        );
    }

    /// **The transcription above is the arithmetic the shipped shader carries**
    /// — a source scan over `led_strip.frag`, so a fix applied to one side only
    /// reds here instead of leaving a green mirror describing a shader nobody
    /// ships.
    ///
    /// The five clauses are the five decisions the mirror could get wrong and
    /// still look plausible: the window's size, each of the two truncating
    /// divisions, the strength scaling, and the composite order.
    ///
    /// **Falsified** by editing any of them in the `.frag` without editing
    /// [`shader_frame`].
    #[test]
    fn the_mirror_is_the_shipped_shaders_arithmetic() {
        for clause in [
            "float win = float(2 * u_bloom_radius + 1);",
            "int tmp = int(floor(255.0 * span_range(lo, hi, from, to) / win));",
            "int blurred = int(floor(float(tmp) * band_span(top, bottom) / win));",
            "return min(blurred * u_bloom_strength / 256, 255);",
            "return (a * (255 - k) + b * k + 127) / 255;",
            "return int(255.0 * sx * sy + 0.5);",
        ] {
            assert!(
                BODY.contains(clause),
                "led_strip.frag no longer carries `{clause}` — the Rust mirror \
                 in this module describes a shader that is not the one shipping",
            );
        }
        // …and the level is composited before the peak dot, which is the one
        // ordering the kit states and a mirror could silently invert.
        let level_at = BODY
            .find("mix_kit(under, ink, level)")
            .expect("the level composite");
        let peak_at = BODY
            .find("mix_kit(under, cap, dot)")
            .expect("the peak composite");
        assert!(
            level_at < peak_at,
            "the peak dot composites on top of the level"
        );
    }

    // ── the transcription ───────────────────────────────────────────────────

    /// `led_strip.frag`'s snapped branch, in Rust: one whole frame, top-down
    /// RGBA8, exactly as `Frame::data` lays it out.
    ///
    /// Everything below mirrors the GLSL statement for statement, `f32` for
    /// `float` and `i32` for `int`, which is what makes a disagreement with the
    /// kit attributable to the shader rather than to this file.
    fn shader_frame(style: kit::DisplayStyle, leds: usize, level: f32, peak: f32) -> Vec<u8> {
        let palette = kit::palette_snapshot(style);
        let (w, h) = kit::led_strip_size(leds);
        let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
            radius: 0,
            strength: 0,
        });
        let index = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
        let lit = index(kit::led_lit_count(level, leds));
        let dot = kit::led_peak_led(peak, leds).map(index);
        let last = index(leds) - 1;
        let cap = kit::led_cap_ink(palette.ink);

        let mut out = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            for col in 0..w {
                #[allow(clippy::cast_precision_loss)]
                let p = (col as f32 + 0.5, row as f32 + 0.5);
                let mut under = palette.bg;
                if let Some(ghost) = palette.ghost {
                    let t = stamp255(p, (w, h), 0, last, bloom);
                    if t > 0 {
                        under = mix_kit(under, ghost, t);
                    }
                }
                let keep = palette.mask.map_or(256, |m| mask_keep(col, row, w, h, m));
                let level255 = layer255(p, (w, h), 0, lit - 1, bloom);
                if level255 > 0 {
                    let i = level255 * keep / 256;
                    if i > 0 {
                        under = mix_kit(under, palette.ink, i);
                    }
                }
                if let Some(dot) = dot {
                    let dot255 = layer255(p, (w, h), dot, dot, bloom);
                    if dot255 > 0 {
                        let i = dot255 * keep / 256;
                        if i > 0 {
                            under = mix_kit(under, cap, i);
                        }
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

    /// `span_range`.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn span_range(lo: f32, hi: f32, from: i32, to: i32) -> f32 {
        if to < from || hi <= lo {
            return 0.0;
        }
        let advance = (kit::LED_CELL_W + kit::LED_GAP) as f32;
        let pad = kit::LED_PAD as f32;
        let first = (((lo - pad) / advance).floor() as i32).max(from);
        let last = (((hi - pad) / advance).floor() as i32).min(to);
        let mut sum = 0.0;
        let mut i = first;
        while i <= last && i - first < 8 {
            let x0 = pad + i as f32 * advance;
            sum += overlap(lo, hi, x0, x0 + kit::LED_CELL_W as f32);
            i += 1;
        }
        sum
    }

    /// `band_span`.
    #[allow(clippy::cast_precision_loss)]
    fn band_span(lo: f32, hi: f32) -> f32 {
        overlap(
            lo,
            hi,
            kit::LED_PAD as f32,
            (kit::LED_PAD + kit::LED_CELL_H) as f32,
        )
    }

    /// `stamp255` — at 1:1 the footprint is exactly one buffer pixel.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn stamp255(
        p: (f32, f32),
        _grid: (usize, usize),
        from: i32,
        to: i32,
        _b: kit::BloomSnapshot,
    ) -> i32 {
        let sx = span_range(p.0 - 0.5, p.0 + 0.5, from, to);
        let sy = band_span(p.1 - 0.5, p.1 + 0.5);
        (255.0 * sx * sy + 0.5) as i32
    }

    /// `halo255`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    fn halo255(
        p: (f32, f32),
        grid: (usize, usize),
        from: i32,
        to: i32,
        bloom: kit::BloomSnapshot,
    ) -> i32 {
        if bloom.radius == 0 || bloom.strength == 0 {
            return 0;
        }
        let win = (2 * bloom.radius + 1) as f32;
        let half_win = win * 0.5;
        let lo = (p.0 - half_win).max(0.0);
        let hi = (p.0 + half_win).min(grid.0 as f32);
        let tmp = (255.0 * span_range(lo, hi, from, to) / win).floor() as i32;
        let top = (p.1 - half_win).max(0.0);
        let bottom = (p.1 + half_win).min(grid.1 as f32);
        let blurred = (tmp as f32 * band_span(top, bottom) / win).floor() as i32;
        (blurred * i32::from(bloom.strength) / 256).min(255)
    }

    /// `layer255`.
    fn layer255(
        p: (f32, f32),
        grid: (usize, usize),
        from: i32,
        to: i32,
        bloom: kit::BloomSnapshot,
    ) -> i32 {
        if to < from {
            return 0;
        }
        stamp255(p, grid, from, to, bloom)
            .max(halo255(p, grid, from, to, bloom))
            .min(255)
    }

    /// `mix_kit`.
    ///
    /// `many_single_char_names` is allowed here and in [`mask_keep`] on
    /// purpose: the names *are* the shader's, which is what lets a reviewer
    /// read the two side by side and see a transcription rather than a
    /// paraphrase. Renaming them would make this file easier to lint and
    /// harder to check.
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
