//! The `Gauge` GL program (#1143): the pipeline declaration, the layer splice,
//! and the pure `(config, deflection, velocity, palette) → GlUniforms` mapping.
//!
//! Sibling of [`program`](super::program) in every structural way — it
//! references nothing above itself either, so the parity harness
//! (`trollshell/examples/preem_gl_diff.rs`) `#[path]`-includes **this** file and
//! measures the shell's own pipeline and the shell's own uniform mapping
//! against the CPU kit, rather than a copy that would agree with itself.
//!
//! # What is better than the kit, and why that is the whole point
//!
//! The CPU kit rasterises a gauge into a **logical** `cols × rows` grid and
//! replicates it `scale` times on the way out (`hytte-preem/src/gauge.rs` ends
//! in `Frame::upscale`). A dial is nothing but soft edges — a needle sits at an
//! arbitrary angle, so every shape on the face gets its coverage from a
//! distance ramp — and at `scale = 2` each of those one-logical-pixel ramps
//! becomes a two-screen-pixel smear with a doubled bloom halo over it. That is
//! #1090, reported as "the gauge is blurry", and it is a property of the
//! resolution the kit draws at rather than of any constant it could re-tune.
//!
//! So this arm's grid **is** the native buffer: `(cols * scale, rows * scale)`.
//! Every length below is resolved by the kit's own rules at the logical size —
//! so the face keeps exactly the proportions, tick budget, centring and
//! counterweight decisions #931 tuned — and is then multiplied by `scale` into
//! native pixels. The one constant that is deliberately **not** multiplied is
//! the anti-aliasing ramp `FEATHER`, which stays one *native* pixel wide. That
//! single line is the fix: the arc, the ticks and the needle are rasterised at
//! the size they are shown at.
//!
//! Two things deliberately do **not** change with it, because they are the
//! skin's rather than the geometry's: the CRT mask keeps its logical pitch (the
//! shader divides the coordinate back down — see `gauge.frag`), and the bloom
//! keeps the extent the kit gives it (its radius is scaled with everything
//! else, so the halo covers the same area, just smoothly).
//!
//! The bloom is the one of those two that is **not** an identity at `scale > 1`,
//! and it is worth stating rather than leaving to be discovered (#1148 review,
//! LOW-2). The kit box-blurs the logical grid at radius `r` and then replicates,
//! so a logical pixel's halo is the average of a `2r + 1` logical window
//! stretched over `scale` native pixels; this blurs the native grid at radius
//! `r * scale`, a `2·r·scale + 1` native window. Same extent to within one
//! native pixel — the windows differ by `scale - 1` taps out of `2·r·scale + 1`,
//! one of 13 at the default face's `r = 3`, `scale = 2` — and a different
//! falloff inside it, which is the same "smoothly rather than in blocks"
//! improvement the rest of the arm is for. The `scale = 2` harness cases measure
//! it as part of the whole frame; it is inside the #893 ceiling and nowhere near
//! it.
//!
//! At `scale == 1` all of that collapses to the identity and the two arms draw
//! the same picture at the same resolution — which is where the parity harness
//! takes its bit-exact measurement. It also renders the **shipping** `scale = 2`
//! per skin and box-averages the native frame back down to the logical grid
//! before comparing (#1148 review, HIGH-2), which is the one gate that sees
//! every scale-dependent line below.
//!
//! # The dial geometry is the kit's, resolved by the kit
//!
//! This file used to carry a hand mirror of `Gauge::dial` and of the thirty-odd
//! private constants it reads, held together by nothing but the harness. #1148's
//! review drifted one of them (`TIP_FRAC`, by 0.02) and the entire unit suite
//! stayed green. So the mirror is gone: `hytte-preem` made [`kit::Dial`],
//! [`kit::Gauge::dial`], [`kit::on_dial`], [`kit::trail_fraction`],
//! [`kit::bloom_radius`] and the shape constants **public** — a visibility
//! change and nothing else, with every value and every caller in the kit
//! untouched — and [`face`] now resolves a face by *calling* the oracle.
//!
//! What is left on this side is only what a second resolution needs and the kit
//! has no concept of: [`scaled`], which carries a resolved face into native
//! pixels, and [`tick_span`], which is a property of how the *shader* searches
//! for ticks rather than of the dial.
//!
//! The shader is the one copy that remains, because a `.frag` cannot read a Rust
//! `const`. `the_shader_declares_the_kits_own_constants` parses `gauge.frag` and
//! compares every one of them against `hytte-preem`'s, so that copy is checked
//! by a unit test rather than by a driver.
//!
//! # The pipeline
//!
//! One render is four passes, and **no step passes at all** — unlike the scope,
//! a gauge carries no cross-frame GPU state. Its one piece of state is the
//! needle's spring, which stays on the CPU in a `kit::Gauge` (see
//! `preem_render`'s `Renderer::GaugeGl`): it is closed-form, frame-rate
//! independent, and costs a handful of multiplies a tick, so there is nothing
//! to win by moving it and a `Needle` accessor to lose.
//!
//! 1. **lit** — the value arc, the motion-blur fan, the blade, the
//!    counterweight and the hub, max-combined into an R8 aux texture;
//! 2. **blur H** and 3. **blur V** — the kit's separable truncating box blur,
//!    the *same* `blur.frag` the scope uses (which is why that file lost its
//!    `scope_` prefix in this change);
//! 4. **blit** — the flat face, the bloom max-combine, the CRT pass and the
//!    composite, point-sampled into the letterboxed fit rect.
//!
//! The blur runs on a glow-free skin too, for the reason
//! [`program`](super::program) gives: at radius `0` it is the identity and at
//! strength `0` the blit's max-combine is a no-op, so the reflective LCD gets
//! `Emission::bloom`'s early-return bytes out of one pipeline.

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

use super::program::{BLUR_H_FRAG, BLUR_V_FRAG, FULLSCREEN_VERT, KitSurface, channels};

/// The registered name of the `Gauge` pipeline.
pub(crate) const GAUGE: GlProgram = GlProgram("preem.gauge");

/// The two layers of the dial, which are the same body with the layer
/// prepended.
///
/// The `blur.frag` trick, for the same reason and with the same payoff:
/// `GlUniforms` is one bag applied to every pass, so there is nowhere to say
/// "this pass is the lit one" — and the alternative here is worse than a
/// duplicated axis, because both layers need `segment_shade` and `arc_shade`.
/// Two hand-written copies of a distance-to-a-tapered-segment would drift, and
/// a drift between *these* two shows up as tick marks that do not line up with
/// the needle sitting on them.
const LIT_FRAG: &str = concat!("const int LAYER = 0;\n", include_str!("gauge.frag"));
const BLIT_FRAG: &str = concat!("const int LAYER = 1;\n", include_str!("gauge.frag"));

/// The `Gauge` pipeline. See the module docs for what each pass is.
pub(crate) const GAUGE_PIPELINE: GlPipeline = GlPipeline {
    // The lit layer into 0, then the blur's two halves into 1 and 2.
    aux: 3,
    // A gauge has no cross-frame GPU state: no phosphor, no accumulator, no
    // decay. `step_seq` therefore stays at `0` and this list stays empty.
    step: &[],
    frame: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: LIT_FRAG,
            target: GlTarget::Aux(0),
            inputs: &[],
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
            inputs: &[GlInput::Aux(0), GlInput::Aux(2)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
    ],
};

// ── what the kit hands over ──────────────────────────────────────────────────
//
// Nothing is copied here any more. `hytte_preem::gauge`'s shape constants, its
// resolved `Dial`, `Gauge::dial`, `on_dial`, `trail_fraction` and `bloom_radius`
// are public (#1148 review, MEDIUM-2/MEDIUM-3) and this file calls them. See the
// module docs for what that replaced and why.

/// A **major** tick is at least as wide as a minor one, which [`tick_span`]
/// depends on and neither file would otherwise state.
///
/// `tick_span` bounds the search window with `MAJOR_HW` as the widest ink any
/// tick lays down. That is an upper bound only while a major tick is the widest
/// mark; inverted, the window would under-estimate and the shader would drop
/// minor ticks out of the dial — silently, on every skin (#1148 review, LOW-1).
/// A `const` assertion rather than a test because it is a statement about two
/// literals and can be decided at compile time; the kit carries the matching
/// runtime one (`the_major_tick_is_the_widest_mark_on_the_face`), on the side
/// that owns the numbers.
const _: () = assert!(
    kit::MAJOR_HW >= kit::MINOR_HW,
    "tick_span bounds the tick window with MAJOR_HW; a wider minor tick would be clipped",
);

/// The motion-blur fan reaches the shader as a `vec4`, so the kit's blade count
/// has to be four.
///
/// [`gauge_surface`] fills one component per blade with the instant that blade
/// samples, and subdivides [`kit::TRAIL_SPAN_SECS`] by `TRAIL_T.len()`. A fifth
/// intensity added to the kit's array would leave the oldest blade reading a
/// component a `vec4` does not have *and* move every other blade's instant, so
/// it must not compile until the uniform grows with it.
const _: () = assert!(
    kit::TRAIL_T.len() == 4,
    "u_theta_trail is a vec4: one component per kit trail blade",
);

/// Most ticks either side of the nearest one the blit shader will test.
///
/// A ceiling on [`tick_span`]'s answer, not a tuning knob: past it adjacent
/// ticks are closer together than the ink one lays down, so they have merged
/// into a band and a wider window would change no pixel a reader could name.
/// It exists so a pathological `divisions` cannot turn an O(1) loop into a
/// per-fragment walk of the wire's 2048-tick cap.
const MAX_TICK_SPAN: i32 = 16;

/// Resolve a face's geometry in **logical** pixels, by asking the kit.
///
/// The kit is the parity oracle and this is now literally it: the builders do
/// the wire clamps they already did for the CPU arm (`with_size`'s 1×1 floor,
/// `sweep_deg`'s clamp and its non-finite-keeps-the-default rule, `ticks`'
/// floors) and [`kit::Gauge::dial`] resolves #931's radius, centring,
/// counterweight and tick budget. Only the fields `dial` reads are set: the
/// needle's physics and the upscale do not move a face.
///
/// **Falsified** by pointing it at a second `kit::Gauge` built with a different
/// sweep: every gauge parity case moves at once.
pub(crate) fn face(config: vocab::GaugeConfig) -> kit::Dial {
    kit::Gauge::with_size(usize_of(config.cols), usize_of(config.rows))
        .sweep_deg(config.sweep_deg)
        .ticks(usize_of(config.divisions), usize_of(config.subdivisions))
        .dial()
}

/// A resolved face in **native** pixels: every length times `scale`, and the
/// pivot moved to the centre of the logical pixel it sat on.
///
/// `pivot * scale + (scale - 1) / 2` rather than `pivot * scale`, because the
/// kit's logical pixel `x` covers native columns `x*scale .. x*scale+scale-1`
/// after `Frame::upscale`, whose centre is half a block further along. At the
/// default face that is `71.5 * 2 + 0.5 = 143.5`, which is `(288 - 1) / 2` — the
/// native buffer's own centre, as it must be — where a bare `* 2` would put the
/// needle half a screen pixel left of its dial.
///
/// The kit has no counterpart: it replicates a logical raster rather than
/// resampling, so "the same face at a second resolution" is not a concept it
/// has. That is why this one function stayed on this side of the seam when the
/// rest of the mirror went (#1148 review, MEDIUM-3).
///
/// A struct literal on purpose, and **not** a `mut` copy with the lengths
/// multiplied in place: a field the kit grows is then a compile error here,
/// where someone has to decide whether it is a length, an angle or a count,
/// rather than a value that silently keeps its logical size.
fn scaled(dial: kit::Dial, scale: f32) -> kit::Dial {
    let offset = (scale - 1.0) / 2.0;
    kit::Dial {
        pivot: (dial.pivot.0 * scale + offset, dial.pivot.1 * scale + offset),
        radius: dial.radius * scale,
        // An angle, not a length.
        half: dial.half,
        tip: dial.tip * scale,
        tail: dial.tail * scale,
        hub: dial.hub * scale,
        blade: dial.blade * scale,
        major_len: dial.major_len * scale,
        minor_len: dial.minor_len * scale,
        // A count: the face keeps the tick budget #931 tuned.
        subdivisions: dial.subdivisions,
    }
}

/// How many ticks either side of the nearest one can reach a fragment.
///
/// Ticks sit at a constant **angular** pitch, so the blit shader finds the one
/// nearest a fragment's own angle in closed form and only has to test its
/// neighbours. This is how many of those there are, and it must be an *upper
/// bound* rather than a guess: the tightest packing is at the innermost point a
/// tick reaches, where an angular pitch buys the least arc length, and the
/// widest ink a tick lays down is `MAJOR_HW * scale + FEATHER` either side of
/// its centreline.
///
/// At every face the kit tunes for, the answer is `1` — the kit's
/// `MIN_TICK_SPACING` (5.9 logical px) is nearly three times the 2.0 px a major
/// tick spreads. It grows only where `divisions` alone packs the arc, which the
/// kit's tick budget does not bound (it pulls down *subdivisions*): 64 divisions
/// on the default face sit 2.06 px apart and want a window of 2.
fn tick_span(dial: kit::Dial, divisions: usize, scale: f32) -> i32 {
    let steps = divisions.max(1).saturating_mul(dial.subdivisions.max(1));
    let pitch = 2.0 * dial.half / fx_usize(steps.max(1));
    // The innermost radius any tick reaches — the mid tick is the longest.
    let longest = (dial.major_len * kit::MID_LEN_BONUS)
        .min(dial.radius)
        .max(dial.minor_len);
    let inner = (dial.radius - longest).max(1.0);
    let spacing = inner * pitch;
    // `MAJOR_HW` is the widest ink any tick lays down — see the `const`
    // assertion above, which is what makes this an upper bound.
    let pad = kit::MAJOR_HW * scale + kit::FEATHER;
    if !spacing.is_finite() || spacing <= 0.0 {
        return MAX_TICK_SPAN;
    }
    let span = (pad / spacing).ceil();
    if !span.is_finite() {
        return MAX_TICK_SPAN;
    }
    #[allow(clippy::cast_possible_truncation)]
    let span = span.clamp(1.0, f32::from(u16::MAX)) as i32;
    span.min(MAX_TICK_SPAN)
}

/// Map one `Gauge`'s already-clamped config, the needle's current deflection
/// and its velocity onto the GL node payload.
///
/// **Pure**, exactly as [`scope_surface`](super::program::scope_surface) is: it
/// reads no globals, resolves no palette and touches no GL. The caller passes
/// the palette it has already resolved *inside the widget's `with_pins` scope*,
/// so accent/role/pin precedence stays the kit's one implementation.
///
/// `fraction` and `velocity` are `kit::Gauge::fraction()` and
/// `needle().velocity()` — the two numbers the picture is a function of. The
/// spring itself stays on the CPU; see the module docs.
pub(crate) fn gauge_surface(
    config: vocab::GaugeConfig,
    fraction: f32,
    velocity: f32,
    palette: &kit::PaletteSnapshot,
) -> KitSurface {
    let cols = config.cols.max(1);
    let rows = config.rows.max(1);
    let scale = config.scale.max(1);
    let upscale = fx(scale);

    let logical = face(config);
    let dial = scaled(logical, upscale);
    let divisions = usize_of(config.divisions.max(1));

    // The halo this face spends, resolved against the **logical** arc radius
    // the kit resolves it against, then carried into native pixels with
    // everything else so it covers the same area of glass.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let bloom_radius_native =
        kit::bloom_radius(bloom.radius, logical.radius).saturating_mul(usize_of(scale));
    let mask = palette.mask;

    let reading = kit::on_dial(fraction);
    let filled = reading.clamp(0.0, 1.0);
    // Sampling the smear in *time* rather than in past frames is what keeps it
    // frame-rate independent along with the physics, and it is exact at rest:
    // with zero velocity every blade lands on the needle and the max-combine
    // erases the fan entirely. The extrapolation itself is the kit's
    // (`trail_fraction`), so the two arms sample the same instants.
    let step = kit::TRAIL_SPAN_SECS / fx_usize(kit::TRAIL_T.len());
    let mut trail = [0.0f32; 4];
    for (index, slot) in trail.iter_mut().enumerate() {
        let back = fx_usize(index + 1) * step;
        *slot = dial.angle(kit::on_dial(kit::trail_fraction(fraction, velocity, back)));
    }

    KitSurface {
        width: cols.saturating_mul(scale),
        height: rows.saturating_mul(scale),
        uniforms: GlUniforms {
            // Order is part of the golden table in the tests; keep it stable.
            values: vec![
                ("u_upscale", GlValue::Float(upscale)),
                ("u_pivot_x", GlValue::Float(dial.pivot.0)),
                ("u_pivot_y", GlValue::Float(dial.pivot.1)),
                ("u_radius", GlValue::Float(dial.radius)),
                ("u_half", GlValue::Float(dial.half)),
                ("u_tip", GlValue::Float(dial.tip)),
                ("u_tail", GlValue::Float(dial.tail)),
                ("u_hub", GlValue::Float(dial.hub)),
                ("u_blade", GlValue::Float(dial.blade)),
                ("u_major_len", GlValue::Float(dial.major_len)),
                ("u_minor_len", GlValue::Float(dial.minor_len)),
                ("u_divisions", GlValue::Int(int_of(divisions))),
                ("u_subdivisions", GlValue::Int(int_of(dial.subdivisions))),
                (
                    "u_tick_span",
                    GlValue::Int(tick_span(dial, divisions, upscale)),
                ),
                ("u_theta_needle", GlValue::Float(dial.angle(reading))),
                ("u_theta_trail", GlValue::Vec4(trail)),
                ("u_value_end", GlValue::Float(dial.angle(filled))),
                ("u_value_on", GlValue::Int(i32::from(filled > 0.0))),
                ("u_bloom_radius", GlValue::Int(int_of(bloom_radius_native))),
                ("u_bloom_strength", GlValue::Int(i32::from(bloom.strength))),
                ("u_bg", channels(palette.bg)),
                ("u_ink", channels(palette.ink)),
                ("u_mask_on", GlValue::Int(i32::from(mask.is_some()))),
                (
                    "u_mask_pitch",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.pitch).unwrap_or(0))),
                ),
                (
                    "u_mask_phase",
                    GlValue::Int(mask.map_or(0, |m| i32::try_from(m.phase).unwrap_or(0))),
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
            // A gauge ships no per-column strip: every shape is analytic.
            data: None,
            // **The native buffer, not the logical grid** — the whole of the
            // #1090 fix. See the module docs.
            grid: (cols.saturating_mul(scale), rows.saturating_mul(scale)),
            // No step passes, so nothing counts steps. See `GAUGE_PIPELINE`.
            step_seq: 0,
        },
    }
}

/// A small buffer dimension as an exact `f32` — the kit's `fx`, over the wire's
/// `u32`. Buffer sizes are far below `u16::MAX` and `u16 → f32` is lossless, so
/// this needs no lossy cast.
fn fx(value: u32) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// [`fx`] for a `usize` count.
fn fx_usize(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// A wire count as a `usize`, saturating — the wire caps put this far out of
/// reach, and the seam is here so it cannot be reached at all.
fn usize_of(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// A count as the `i32` a uniform carries, saturating at the top.
fn int_of(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{
        BLIT_FRAG, GAUGE_PIPELINE, GlUniforms, GlValue, LIT_FRAG, MAX_TICK_SPAN, face,
        gauge_surface, kit, scaled, tick_span, vocab,
    };

    /// A fixed palette so the golden table below is a function of the mapping
    /// and not of whatever accent the process carries. Shaped like the CRT's
    /// (the only skin with a mask) so every field is exercised.
    fn crt_like() -> kit::PaletteSnapshot {
        kit::PaletteSnapshot {
            bg: [0x03, 0x07, 0x05, 0xff],
            ink: [0x5c, 0xff, 0x82, 0xff],
            ghost: None,
            bloom: Some(kit::BloomSnapshot {
                radius: 3,
                strength: 190,
            }),
            mask: Some(kit::MaskSnapshot {
                pitch: 4,
                phase: 3,
                scanline_keep: 150,
                corner_keep: 115,
            }),
        }
    }

    fn config() -> vocab::GaugeConfig {
        vocab::GaugeConfig::default()
    }

    fn value(surface: &GlUniforms, name: &str) -> GlValue {
        let Some((_, held)) = surface.values.iter().find(|(key, _)| *key == name) else {
            panic!("the mapping always emits {name}")
        };
        *held
    }

    /// **The native grid is the whole of #1090.** The reconciler's natural size
    /// and the offscreen grid are the *same* number — `cols * scale` — where
    /// the scope's grid is the pre-upscale one, and that is what puts the arc,
    /// the ticks and the needle on the pixel grid they are shown at instead of
    /// on a quarter of it.
    ///
    /// **Falsified** by setting `grid` to `(cols, rows)`: the natural size and
    /// the grid stop agreeing, the blit starts point-sampling 144 columns
    /// across 288, and the harness's `scale = 1` cases keep passing — which is
    /// exactly why this assertion is here and not left to the harness.
    #[test]
    fn the_gauge_grid_is_the_native_buffer_not_the_logical_one() {
        let surface = gauge_surface(config(), 0.5, 0.0, &crt_like());
        assert_eq!(surface.width, 288, "cols * scale");
        assert_eq!(surface.height, 128, "rows * scale");
        assert_eq!(
            surface.uniforms.grid,
            (288, 128),
            "the offscreen grid is the native buffer, not the 144x64 the kit rasterises",
        );
        assert_eq!(
            (surface.width, surface.height),
            surface.uniforms.grid,
            "…and the two are the same number, so the blit is 1:1 at the natural size",
        );
    }

    /// The face is resolved by the kit's rules at the **logical** size and then
    /// carried into native pixels, so the pivot lands on the native buffer's
    /// own centre rather than half a pixel to the left of it.
    ///
    /// **Falsified** by dropping the `(scale - 1) / 2` term from
    /// [`scaled`]: `pivot_x` comes back `143.0` on a 288-wide buffer
    /// whose centre is `143.5`, and every needle angle is then measured from a
    /// pivot that is not the one the ticks were drawn around.
    #[test]
    fn the_face_scales_onto_the_native_pixel_centres() {
        let logical = face(config());
        assert!(
            (logical.pivot.0 - 71.5).abs() < 1e-4,
            "the logical pivot is the kit's (cols - 1) / 2, got {}",
            logical.pivot.0,
        );
        let native = scaled(logical, 2.0);
        assert!(
            (native.pivot.0 - 143.5).abs() < 1e-4,
            "the native pivot is (288 - 1) / 2, got {}",
            native.pivot.0,
        );
        assert!(
            (native.radius - logical.radius * 2.0).abs() < 1e-4,
            "every length doubles",
        );
        assert!(
            (native.half - logical.half).abs() < 1e-6,
            "an angle is not a length and must not scale",
        );
        assert_eq!(
            native.subdivisions, logical.subdivisions,
            "and neither is a count: the face keeps the tick budget #931 tuned",
        );
    }

    /// At `scale == 1` the native geometry **is** the logical geometry, which
    /// is the premise the parity harness rests on: the gauge cases run there,
    /// and if this ever stopped holding they would be comparing two different
    /// faces and calling the difference a shader bug.
    #[test]
    fn at_scale_one_the_two_resolutions_coincide() {
        let logical = face(vocab::GaugeConfig {
            scale: 1,
            ..config()
        });
        assert_eq!(scaled(logical, 1.0), logical);
    }

    /// **The golden uniform table.** Every value the shaders read, in order,
    /// for one fully-specified state — the seam CI can gate, since the pixels
    /// need a driver.
    ///
    /// **Falsified** by swapping any two fields of the mapping — `u_bg` with
    /// `u_ink`, `u_major_len` with `u_minor_len`, `u_tip` with `u_tail` — each
    /// of which moves exactly one row.
    #[test]
    fn the_uniform_table_is_the_kits_own_numbers() {
        let surface = gauge_surface(config(), 0.5, 0.0, &crt_like());
        let names: Vec<&str> = surface
            .uniforms
            .values
            .iter()
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(
            names,
            vec![
                "u_upscale",
                "u_pivot_x",
                "u_pivot_y",
                "u_radius",
                "u_half",
                "u_tip",
                "u_tail",
                "u_hub",
                "u_blade",
                "u_major_len",
                "u_minor_len",
                "u_divisions",
                "u_subdivisions",
                "u_tick_span",
                "u_theta_needle",
                "u_theta_trail",
                "u_value_end",
                "u_value_on",
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
        assert_eq!(value(&surface.uniforms, "u_upscale"), GlValue::Float(2.0));
        assert_eq!(value(&surface.uniforms, "u_divisions"), GlValue::Int(4));
        assert_eq!(value(&surface.uniforms, "u_subdivisions"), GlValue::Int(5));
        assert_eq!(
            value(&surface.uniforms, "u_bg"),
            GlValue::Vec4([3.0, 7.0, 5.0, 255.0]),
        );
        assert_eq!(
            value(&surface.uniforms, "u_ink"),
            GlValue::Vec4([92.0, 255.0, 130.0, 255.0]),
        );
        assert_eq!(value(&surface.uniforms, "u_mask_on"), GlValue::Int(1));
        assert_eq!(value(&surface.uniforms, "u_mask_pitch"), GlValue::Int(4));
        assert_eq!(value(&surface.uniforms, "u_mask_phase"), GlValue::Int(3));
        assert_eq!(
            value(&surface.uniforms, "u_scanline_keep"),
            GlValue::Int(150)
        );
        assert_eq!(value(&surface.uniforms, "u_corner_keep"), GlValue::Int(115));
        // The CRT asks for radius 3; the gauge halves it (#930) to 2, the
        // default face's cap (⌊50.54 / 16⌋ = 3) does not bind, and the native
        // grid carries it to 4 so the halo covers the same glass.
        assert_eq!(value(&surface.uniforms, "u_bloom_radius"), GlValue::Int(4));
        assert_eq!(
            value(&surface.uniforms, "u_bloom_strength"),
            GlValue::Int(190),
        );
        assert!(surface.uniforms.data.is_none(), "a gauge ships no strip");
        assert_eq!(surface.uniforms.step_seq, 0, "and counts no steps");
    }

    /// A glow-free skin (the reflective LCD) reaches the shaders as radius 0
    /// and strength 0, which makes the blur the identity and the blit's
    /// max-combine a no-op — `Emission::bloom`'s early-return bytes out of the
    /// same pipeline. A mask-free skin zeroes the whole CRT group.
    #[test]
    fn a_glow_free_skin_zeroes_the_bloom_and_the_mask() {
        let lcd = kit::palette_snapshot(kit::DisplayStyle::Lcd);
        assert!(lcd.bloom.is_none() && lcd.mask.is_none(), "the LCD premise");
        let surface = gauge_surface(config(), 0.25, 0.0, &lcd);
        assert_eq!(value(&surface.uniforms, "u_bloom_radius"), GlValue::Int(0));
        assert_eq!(
            value(&surface.uniforms, "u_bloom_strength"),
            GlValue::Int(0)
        );
        assert_eq!(value(&surface.uniforms, "u_mask_on"), GlValue::Int(0));
        assert_eq!(value(&surface.uniforms, "u_mask_pitch"), GlValue::Int(0));
        assert_eq!(value(&surface.uniforms, "u_scanline_keep"), GlValue::Int(0));
    }

    /// **A settled needle has no motion blur**, and that is an equality rather
    /// than an approximation: at zero velocity every trail blade samples the
    /// same deflection as the live one, so the max-combine erases the fan
    /// exactly. A moving needle spreads them, oldest furthest back.
    ///
    /// **Falsified** by dropping the `velocity * back` term (a moving needle
    /// then draws no smear at all) or by flipping its sign (the fan leads the
    /// needle instead of trailing it, which is a pointer that arrives before
    /// it moves).
    #[test]
    fn the_motion_blur_collapses_onto_a_settled_needle_and_trails_a_moving_one() {
        let settled = gauge_surface(config(), 0.5, 0.0, &crt_like());
        let needle = value(&settled.uniforms, "u_theta_needle");
        let GlValue::Float(needle) = needle else {
            panic!("u_theta_needle is a float")
        };
        let GlValue::Vec4(trail) = value(&settled.uniforms, "u_theta_trail") else {
            panic!("u_theta_trail is a vec4")
        };
        for blade in trail {
            assert!(
                (blade - needle).abs() < 1e-6,
                "a settled fan lands on the blade: {blade} vs {needle}",
            );
        }

        let moving = gauge_surface(config(), 0.5, 2.0, &crt_like());
        let GlValue::Float(needle) = value(&moving.uniforms, "u_theta_needle") else {
            panic!("u_theta_needle is a float")
        };
        let GlValue::Vec4(trail) = value(&moving.uniforms, "u_theta_trail") else {
            panic!("u_theta_trail is a vec4")
        };
        assert!(
            trail[0] < needle,
            "a needle sweeping up trails behind itself",
        );
        for pair in trail.windows(2) {
            assert!(pair[1] < pair[0], "and the fan gets older going back");
        }
    }

    /// The drawn angle is clamped to the dial's mechanical stops while the
    /// physics is not — a full-scale slam bangs the stop rather than sweeping
    /// off the face, and a reading at or below zero lights no value arc.
    #[test]
    fn the_needle_stops_at_the_overtravel_and_the_value_arc_switches_off() {
        let dial = face(config());
        let pegged = gauge_surface(config(), 5.0, 0.0, &crt_like());
        assert_eq!(
            value(&pegged.uniforms, "u_theta_needle"),
            GlValue::Float(dial.angle(1.0 + kit::OVERTRAVEL)),
            "a slam past full scale stops at the overtravel",
        );
        assert_eq!(
            value(&pegged.uniforms, "u_value_end"),
            GlValue::Float(dial.angle(1.0)),
            "…while the lit arc fills to full scale and no further",
        );
        assert_eq!(value(&pegged.uniforms, "u_value_on"), GlValue::Int(1));

        let rest = gauge_surface(config(), 0.0, 0.0, &crt_like());
        assert_eq!(
            value(&rest.uniforms, "u_value_on"),
            GlValue::Int(0),
            "nothing is lit at the low end",
        );

        let nonfinite = gauge_surface(config(), f32::NAN, f32::NAN, &crt_like());
        assert_eq!(
            value(&nonfinite.uniforms, "u_theta_needle"),
            GlValue::Float(dial.angle(0.0)),
            "a non-finite reading parks at zero rather than reaching the shader",
        );
    }

    /// The tick window is an **upper bound**, not a guess: it is `1` at the
    /// face the kit tunes for, and it grows where `divisions` alone packs the
    /// arc tighter than a tick's own ink — which `tick_budget` does **not**
    /// bound, because it pulls down *subdivisions* and floors that at `1`.
    ///
    /// **Falsified** by pinning [`tick_span`] to `1`: the packed case comes
    /// back `1` where its ticks sit 1.35 native px apart and a major tick
    /// spreads 2.85 px, so the shader would stop testing neighbours that can
    /// reach the fragment and drop marks out of the dial.
    #[test]
    fn the_tick_window_covers_what_a_tick_can_reach() {
        let wide = face(config());
        assert_eq!(
            tick_span(scaled(wide, 2.0), 4, 2.0),
            1,
            "the default face's pitch is nearly four times a tick's own ink",
        );
        // A small square dial with the wire's cap on `divisions`: the
        // subdivision budget is already down at its floor, so the *divisions*
        // are what pack the arc and nothing pulls them down.
        let packed = vocab::GaugeConfig {
            cols: 48,
            rows: 48,
            divisions: 64,
            ..config()
        };
        let dense = face(packed);
        assert_eq!(dense.subdivisions, 1, "the budget drops to the boundaries");
        let span = tick_span(scaled(dense, 2.0), 64, 2.0);
        assert!(
            span >= 2,
            "64 divisions on a 48x48 face pack the arc tighter than a tick's ink, got {span}",
        );
        assert!(span <= MAX_TICK_SPAN, "and the window is still bounded");

        // A face so degenerate that its ticks reach all the way in still gets a
        // bounded window rather than a per-fragment walk.
        let tiny = vocab::GaugeConfig {
            cols: 1,
            rows: 1,
            scale: 1,
            ..config()
        };
        let collapsed = face(tiny);
        let span = tick_span(scaled(collapsed, 1.0), 4, 1.0);
        assert!((2..=MAX_TICK_SPAN).contains(&span), "got {span}");
    }

    /// The two layers are one body with one line prepended, so they can only
    /// ever differ in which layer they are. Checked because a hand-copied
    /// second distance-to-a-tapered-segment is exactly the duplicate that
    /// drifts — and a drift here draws ticks that do not line up with the
    /// needle on them.
    #[test]
    fn the_layers_share_one_body() {
        let strip = |src: &str| src.split_once('\n').map(|(_, rest)| rest.to_owned());
        assert_eq!(strip(LIT_FRAG), strip(BLIT_FRAG));
        assert!(LIT_FRAG.starts_with("const int LAYER = 0;"));
        assert!(BLIT_FRAG.starts_with("const int LAYER = 1;"));
    }

    /// One `const <type> <NAME> = <value>;` out of the shipped GLSL.
    fn shader_const(name: &str) -> String {
        let marker = format!(" {name} = ");
        let at = LIT_FRAG
            .find(&marker)
            .unwrap_or_else(|| panic!("gauge.frag declares {name}"));
        let rest = &LIT_FRAG[at + marker.len()..];
        let (value, _) = rest
            .split_once(';')
            .unwrap_or_else(|| panic!("{name}'s declaration ends in a semicolon"));
        value.trim().to_owned()
    }

    /// **Every logical length the shader draws with is multiplied by the
    /// upscale**, and the factor it is multiplied by is the upscale uniform.
    ///
    /// The fourth of the arm's scale-only decisions, and the one no other gate
    /// can see (#1148 review, HIGH-2). The pivot offset is pinned by
    /// [`the_face_scales_onto_the_native_pixel_centres`], the bloom radius by
    /// the golden uniform table, and the CRT mask's logical pitch by the
    /// harness's box-averaged case — that one fails `FAIL(interior)`, because a
    /// comb at the wrong pitch moves flat pixels. A tick drawn at half its
    /// width does **not** move a flat pixel: a major tick is 1.7 logical px
    /// wide, so it is all ramp and all of it lands in the harness's edge bin,
    /// inside the budget. Measured: with `s` pinned to `1.0`, all 28 harness
    /// cases pass and the whole unit suite is green.
    ///
    /// So it is pinned here, in the source, in the two ways it can break:
    /// `s` stopping being the upscale, and a use of a length losing its `* s`.
    /// A source scan is a weak instrument and this is the case that earns one —
    /// the alternative is no check at all on a change that halves every tick on
    /// every dial on the glass.
    ///
    /// **Falsified** by `float s = u_upscale;` -> `float s = 1.0;` (the first
    /// assertion), or by dropping the `* s` from any of the half-widths (the
    /// second).
    #[test]
    fn the_shader_scales_every_logical_length_it_draws_with() {
        assert!(
            LIT_FRAG.contains("float s = u_upscale;"),
            "gauge.frag's face resolves its half-widths against `s`; `s` must be the \
             upscale uniform, or every tick and arc is drawn at its logical width on a \
             native-resolution grid",
        );
        // The declaration block is where these are *defined* in logical px; it
        // is every line after it that has to carry them into native ones.
        let body = LIT_FRAG
            .split_once("const float F32_EPSILON")
            .map_or(LIT_FRAG, |(_, rest)| rest);
        for name in ["ARC_HW", "VALUE_HW_BONUS", "MAJOR_HW", "MINOR_HW", "BLADE_TIP"] {
            for line in body.lines().filter(|line| line.contains(name)) {
                assert!(
                    line.contains("* s") || line.contains("* u_upscale"),
                    "gauge.frag draws with {name} without scaling it into native pixels: \
                     {line}",
                );
            }
        }
    }

    /// **Every constant `gauge.frag` shares with the kit is the kit's value**,
    /// parsed back out of the shipped GLSL rather than restated beside it.
    ///
    /// This is the copy the #1148 review's HIGH-1 was about, and the only one
    /// that has to exist: a `.frag` cannot read a Rust `const`, so the shader
    /// re-declares the eight lengths and the eight intensities it draws with.
    /// Before this they were tied to nothing — the review widened `MINOR_HW`
    /// from 0.55 to 0.60, a 9 % fattening of every minor tick on every dial,
    /// and the whole unit suite, the `glsl` lint and the parity harness all
    /// stayed green. `MINOR_HW` was the worst one to pick because it appears
    /// *only* in the shader; now every one of them appears in
    /// `hytte-preem`'s `gauge.rs` too and this test is what says so.
    ///
    /// Declared as a list rather than derived, so a constant the shader grows
    /// on its own (a purely GL quantity like `F32_EPSILON` or the CRT's
    /// `MASK_ONE`) does not have to be given a Rust counterpart it has no
    /// meaning for. What the list covers is every value that is *shared*.
    ///
    /// **Falsified** by changing any entry on either side of the seam.
    #[test]
    fn the_shader_declares_the_kits_own_constants() {
        let lengths: [(&str, f32); 8] = [
            ("ARC_HW", kit::ARC_HW),
            ("VALUE_HW_BONUS", kit::VALUE_HW_BONUS),
            ("MAJOR_HW", kit::MAJOR_HW),
            ("MINOR_HW", kit::MINOR_HW),
            ("BLADE_TIP", kit::BLADE_TIP),
            ("TAIL_FLARE", kit::TAIL_FLARE),
            ("MID_LEN_BONUS", kit::MID_LEN_BONUS),
            ("FEATHER", kit::FEATHER),
        ];
        for (name, kit_value) in lengths {
            let declared: f32 = shader_const(name)
                .parse()
                .unwrap_or_else(|_| panic!("{name} is a float literal in gauge.frag"));
            assert!(
                (declared - kit_value).abs() < 1e-6,
                "gauge.frag's {name} is {declared}, the kit's is {kit_value}",
            );
        }

        let intensities: [(&str, u16); 7] = [
            ("ARC_T", kit::ARC_T),
            ("MINOR_T", kit::MINOR_T),
            ("MAJOR_T", kit::MAJOR_T),
            ("MID_T", kit::MID_T),
            ("VALUE_T", kit::VALUE_T),
            ("NEEDLE_T", kit::NEEDLE_T),
            ("HUB_T", kit::HUB_T),
        ];
        for (name, kit_value) in intensities {
            assert_eq!(
                shader_const(name).parse::<u16>().ok(),
                Some(kit_value),
                "gauge.frag's {name} is not the kit's {kit_value}",
            );
        }
    }

    /// The shader's motion-blur intensities are the kit's `TRAIL_T`, **element
    /// for element and length included**.
    ///
    /// Two halves of one decision: the shader owns the intensities the blades
    /// are drawn at, this side owns how far back in time each of them samples
    /// (`TRAIL_SPAN_SECS / TRAIL_T.len()`). A fifth intensity added to the kit
    /// without the uniform growing with it would leave the oldest blade reading
    /// a component a `vec4` does not have — the `const` assertion beside
    /// [`MAX_TICK_SPAN`] refuses to compile then — and a *reordered* array
    /// would draw the fan with its brightness running backwards, which is what
    /// this reads the values for rather than only the count.
    #[test]
    fn the_shader_and_the_kit_agree_about_the_motion_blur_fan() {
        let marker = "const int TRAIL_T[";
        let at = LIT_FRAG
            .find(marker)
            .expect("gauge.frag declares the trail intensities");
        let rest = &LIT_FRAG[at + marker.len()..];
        let (count, tail) = rest.split_once(']').expect("a bracketed array length");
        assert_eq!(
            count.trim().parse::<usize>().ok(),
            Some(kit::TRAIL_T.len()),
            "gauge.frag's TRAIL_T length and the kit's are one number",
        );
        let body = tail
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(inside, _)| inside)
            .expect("an int[N](…) initialiser");
        let declared: Vec<u16> = body
            .split(',')
            .map(|value| value.trim().parse().expect("an integer intensity"))
            .collect();
        assert_eq!(
            declared.as_slice(),
            kit::TRAIL_T.as_slice(),
            "gauge.frag's fan is not the kit's TRAIL_T",
        );
    }

    /// The pipeline's shape, which a reader of the GLSL cannot see: **no step
    /// passes at all** (a gauge carries no cross-frame GPU state), the lit
    /// layer into an aux the blur can read, and the screen last.
    ///
    /// **Falsified** by pointing the blit at `Aux(1)` instead of `Aux(2)`,
    /// which would composite the half-blurred layer as the halo.
    #[test]
    fn the_pipeline_is_stateless_and_ends_on_the_screen() {
        use hytte::ui::gl_surface::{GlInput, GlTarget};
        assert!(
            GAUGE_PIPELINE.step.is_empty(),
            "a gauge has no accumulator to step",
        );
        assert_eq!(GAUGE_PIPELINE.frame.len(), 4, "lit, blur H, blur V, blit");
        assert_eq!(GAUGE_PIPELINE.frame[0].target, GlTarget::Aux(0));
        assert_eq!(GAUGE_PIPELINE.frame[1].target, GlTarget::Aux(1));
        assert_eq!(GAUGE_PIPELINE.frame[2].target, GlTarget::Aux(2));
        assert_eq!(GAUGE_PIPELINE.frame[3].target, GlTarget::Screen);
        assert_eq!(
            GAUGE_PIPELINE.frame[3].inputs,
            &[GlInput::Aux(0), GlInput::Aux(2)][..],
            "the blit reads the lit layer and the fully-blurred one",
        );
        let declared = usize::from(GAUGE_PIPELINE.aux);
        for pass in GAUGE_PIPELINE.step.iter().chain(GAUGE_PIPELINE.frame) {
            assert_ne!(
                pass.target,
                GlTarget::Accumulator,
                "nothing in a stateless pipeline may touch the accumulator",
            );
            if let GlTarget::Aux(slot) = pass.target {
                assert!(usize::from(slot) < declared, "aux {slot} is undeclared");
            }
            for input in pass.inputs {
                if let GlInput::Aux(slot) = *input {
                    assert!(usize::from(slot) < declared, "aux {slot} is undeclared");
                }
            }
        }
    }

    /// Every `uniform` name declared by a shader this pipeline ships, in
    /// declaration order per file.
    fn declared_uniforms(source: &str) -> Vec<&str> {
        source
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("uniform ")?;
                let (declaration, _) = rest.split_once(';')?;
                declaration.split_whitespace().next_back()
            })
            .collect()
    }

    /// **The uniform bag and the shipped GLSL agree, in both directions** —
    /// derived from the shader sources, not from a list kept beside them.
    ///
    /// The scope's own version of this test explains at length why this is the
    /// seam that rots: `hytte-ui` sets every name in the bag on every pass and
    /// GL ignores one a program does not declare, so a `uniform` added to a
    /// shader and forgotten in the mapping reads as **zero** — a radius of
    /// nothing, a dial with no sweep, a needle pinned at the low stop — and a
    /// name left in the bag after its shader stopped declaring it is a silent
    /// per-pass `glGetUniformLocation` miss. Neither half fails loudly.
    ///
    /// **Falsified** two ways: add `uniform int u_unused;` to `gauge.frag` (the
    /// first direction goes red), or delete a `("u_…", …)` row from
    /// [`gauge_surface`]'s `values` (the second does).
    #[test]
    fn the_mapping_fills_every_uniform_the_shaders_read() {
        // What `hytte-ui`'s `GlSurface` supplies itself, before the host's bag
        // is applied. A shader may declare these; the mapping must not.
        const HOST_SUPPLIED: [&str; 8] = [
            "u_grid",
            "u_viewport",
            "u_data_len",
            "u_step_back",
            "u_tex0",
            "u_tex1",
            "u_tex2",
            "u_tex3",
        ];

        let shaders = [
            ("fullscreen.vert", super::FULLSCREEN_VERT),
            ("gauge.frag (lit)", LIT_FRAG),
            ("gauge.frag (blit)", BLIT_FRAG),
            ("blur.frag (H)", super::BLUR_H_FRAG),
            ("blur.frag (V)", super::BLUR_V_FRAG),
        ];

        let surface = gauge_surface(config(), 0.5, 0.0, &crt_like());
        assert_ne!(surface.uniforms, GlUniforms::default());
        let bag: Vec<&str> = surface
            .uniforms
            .values
            .iter()
            .map(|(name, _)| *name)
            .collect();

        let mut declared: Vec<&str> = shaders
            .iter()
            .flat_map(|(_, source)| declared_uniforms(source))
            .collect();
        declared.sort_unstable();
        declared.dedup();

        // The premise: the scan found the host's own names too, so a broken
        // parse cannot pass this vacuously.
        assert!(
            HOST_SUPPLIED
                .iter()
                .filter(|name| declared.contains(name))
                .count()
                >= 3,
            "the scan found {} uniform(s) and almost none of the host's — it is \
             the parse that is broken, not the shaders: {declared:?}",
            declared.len(),
        );

        for name in &declared {
            assert!(
                HOST_SUPPLIED.contains(name) || bag.contains(name),
                "{name} is declared by a shader but neither host-supplied nor in the bag",
            );
        }
        for name in &bag {
            assert!(
                !HOST_SUPPLIED.contains(name),
                "{name} shadows a uniform `hytte-ui` supplies itself",
            );
            assert!(
                declared.contains(name),
                "{name} is in the bag but no shipped shader declares it",
            );
        }
    }

    /// A degenerate face still resolves to something drawable rather than to a
    /// negative radius or a division by zero — the wire clamps keep a 1×1
    /// gauge reachable, and the kit's own floors are what make it a dial.
    #[test]
    fn a_degenerate_face_still_resolves() {
        let tiny = vocab::GaugeConfig {
            cols: 1,
            rows: 1,
            scale: 1,
            ..config()
        };
        let dial: kit::Dial = face(tiny);
        assert!(
            dial.radius >= 1.0,
            "the arc never collapses past MIN_RADIUS"
        );
        assert!(
            dial.subdivisions >= 1,
            "a scale with no marks is not a scale"
        );
        assert!(dial.half > 0.0, "and the sweep is clamped open");
        let surface = gauge_surface(tiny, 0.5, 0.0, &crt_like());
        assert_eq!(surface.uniforms.grid, (1, 1));
    }
}
