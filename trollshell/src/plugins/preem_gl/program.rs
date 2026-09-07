//! The `Scope` GL program: the pipeline declaration, the GLSL, and the pure
//! `(config, samples, step_seq, palette) → GlUniforms` mapping.
//!
//! # Self-contained, deliberately
//!
//! This module references nothing above it — no `super::`, no `crate::` — and
//! that is a load-bearing property, not tidiness. The parity harness
//! (`trollshell/examples/preem_gl_diff.rs`) pulls it in with `#[path]` so it
//! measures **the shell's own** pipeline and **the shell's own** uniform
//! mapping against the CPU kit. A harness carrying its own copy of either
//! would agree with itself by construction and prove nothing.
//!
//! # Where the GLSL lives, and why it is not under `assets/`
//!
//! `*.vert` / `*.frag` beside this file, `include_str!`'d — the extension names
//! the stage, for `glslangValidator` and for a reader. Not under the top-level
//! `assets/`, which crane's source filter strips (#480/#446) — a compile-time
//! `include_str!` of anything under there passes `cargo build` locally and
//! fails every `nix build`. The nix filter does still have to learn those
//! suffixes (it keeps only `.rs`/`.toml`/`.lock` by default), because it
//! filters by *extension* and not by directory; `nix/package.nix` carries that
//! clause, and `nix/lint-glsl.py` compiles what it keeps.
//!
//! # The pipeline
//!
//! One `Scope::advance` step is two passes over the accumulator —
//!
//! 1. **decay**, a full-screen `(v * retained) >> 8` reading the front buffer
//!    and writing the back one;
//! 2. **beam**, one quad instance per column covering that column's polyline
//!    span padded by the glow kernel, `GL_MAX`-blended into the same target;
//!
//! — and one render is three more:
//!
//! 3. **blur H** and 4. **blur V**, the kit's separable truncating box blur
//!    into the two auxiliary textures;
//! 5. **blit**, the graticule, the bloom max-combine, the CRT mask and the
//!    composite, point-sampled into the letterboxed fit rect.
//!
//! The blur runs even on a skin with no bloom. That is not waste worth
//! branching away: at radius `0` the blur is the identity and at strength `0`
//! the blit's `max(v, blurred * strength / 256)` reduces to `v`, so the
//! reflective LCD gets the same bytes `Emission::bloom`'s early return gives
//! it — from one pipeline, with two fragment passes over a 144×48 texture.

use std::sync::Arc;

use hytte::ui::gl_surface::{
    GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms, GlValue,
};
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

/// The registered name of the `Scope` pipeline.
pub(crate) const SCOPE: GlProgram = GlProgram("preem.scope");

const FULLSCREEN_VERT: &str = include_str!("fullscreen.vert");
const DECAY_FRAG: &str = include_str!("scope_decay.frag");
const BEAM_VERT: &str = include_str!("scope_beam.vert");
const BEAM_FRAG: &str = include_str!("scope_beam.frag");
const BLIT_FRAG: &str = include_str!("scope_blit.frag");

/// The two halves of the separable blur, which are the same body with the
/// direction prepended.
///
/// `GlUniforms` is one bag applied to every pass, so there is nowhere to say
/// "this pass is the horizontal one" — and two hand-written copies of a
/// truncating integer blur is exactly the duplication that drifts. `concat!`
/// over an `include_str!` is a compile-time splice of one body, so the two
/// shaders cannot disagree about anything but the axis.
const BLUR_H_FRAG: &str = concat!(
    "const ivec2 BLUR_DIR = ivec2(1, 0);\n",
    include_str!("scope_blur.frag")
);
const BLUR_V_FRAG: &str = concat!(
    "const ivec2 BLUR_DIR = ivec2(0, 1);\n",
    include_str!("scope_blur.frag")
);

/// The `Scope` pipeline. See the module docs for what each pass is.
pub(crate) const SCOPE_PIPELINE: GlPipeline = GlPipeline {
    // The blur's two halves: horizontal into 0, vertical into 1.
    aux: 2,
    step: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: DECAY_FRAG,
            target: GlTarget::Accumulator,
            inputs: &[GlInput::Accumulator],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: BEAM_VERT,
            fragment: BEAM_FRAG,
            target: GlTarget::Accumulator,
            inputs: &[GlInput::Data],
            // The kit stamps with `max`, not `+=` — see `Scope::stamp`.
            blend: GlBlend::Max,
            draw: GlDraw::PerColumn,
        },
    ],
    frame: &[
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_H_FRAG,
            target: GlTarget::Aux(0),
            inputs: &[GlInput::Accumulator],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLUR_V_FRAG,
            target: GlTarget::Aux(1),
            inputs: &[GlInput::Aux(0)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
        GlPass {
            vertex: FULLSCREEN_VERT,
            fragment: BLIT_FRAG,
            target: GlTarget::Screen,
            inputs: &[GlInput::Accumulator, GlInput::Aux(1)],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        },
    ],
};

/// `u_batch_step_back` for a state whose sample batch is not replayed by any
/// step this render will run — so every step flatlines on the axis while the
/// trail decays, which is the kit's documented empty-batch behaviour.
pub(crate) const NO_BATCH: i32 = -1;

/// The kit's persistence ceiling (`256/256 = 1.0`, an infinite-persistence
/// phosphor). `kit::Scope::persistence` clamps to it, so the uniform must be
/// clamped the same way before the decay shader shifts by it.
pub(crate) const KIT_MAX_PERSISTENCE: u16 = 256;

/// A `Scope`'s GL node payload: the natural size the reconciler measures with,
/// and the uniforms one render needs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ScopeSurface {
    /// Natural width in logical pixels — `cols * scale`, exactly what
    /// `Frame::upscale` would have produced.
    pub(crate) width: u32,
    /// Natural height in logical pixels — `rows * scale`.
    pub(crate) height: u32,
    /// The uniform bag, data strip, grid and step count.
    pub(crate) uniforms: GlUniforms,
}

/// Map one `Scope`'s already-clamped config, its pending sample batch and its
/// step counters onto the GL node payload.
///
/// **Pure**, and that is what makes the golden table below a real test: it
/// reads no globals, resolves no palette and touches no GL. The caller passes
/// the palette it has already resolved *inside the widget's `with_pins`
/// scope*, so the accent/role/pin precedence is the kit's one implementation
/// rather than a second copy on this side of the boundary.
///
/// `batch_step` is the absolute step index the current `samples` batch is
/// stamped on, or `None` once it has scrolled out of what a render will
/// replay. It reaches the shader as a **distance back from the newest step**
/// rather than an absolute index, so neither counter can overflow the `int` a
/// uniform carries however long the shell runs.
pub(crate) fn scope_surface(
    config: vocab::ScopeConfig,
    samples: &Arc<[f32]>,
    batch_step: Option<u64>,
    step_seq: u64,
    palette: &kit::PaletteSnapshot,
) -> ScopeSurface {
    let cols = config.cols.max(1);
    let rows = config.rows.max(1);
    let scale = config.scale.max(1);
    // A skin with no bloom reaches the shaders as radius 0 / strength 0, which
    // makes the blur the identity and the blit's max-combine a no-op — see the
    // module docs on why the passes run unconditionally.
    //
    // One corner where that equivalence would stop holding, recorded because it
    // is invisible from the shader: the kit's `Emission::bloom` early-returns on
    // `radius == 0 || strength == 0`, while this runs the pass, and at radius 0
    // the halo is `v * strength / 256` — which is `<= v`, and so absorbed by the
    // `max`, only while `strength <= 256`. A skin declaring `radius: 0` with
    // `strength > 256` would brighten here and no-op in the kit. No skin does
    // (`Vfd` 2/150, `Oled` 1/120, `Crt` 3/190, `Lcd` none), and `Bloom` is
    // skin-defined, so no plugin can reach it over the wire.
    let bloom = palette.bloom.unwrap_or(kit::BloomSnapshot {
        radius: 0,
        strength: 0,
    });
    let mask = palette.mask;
    // The newest step is `step_seq - 1`, so a batch stamped there is `0` back.
    // A batch older than `i32::MAX` steps saturates to a value no replayed
    // step can equal, which is the same answer as `NO_BATCH` and one the
    // shader needs no extra branch for.
    let batch_step_back = batch_step.map_or(NO_BATCH, |at| {
        step_seq
            .saturating_sub(1)
            .checked_sub(at)
            .map_or(NO_BATCH, |back| i32::try_from(back).unwrap_or(i32::MAX))
    });

    ScopeSurface {
        width: cols.saturating_mul(scale),
        height: rows.saturating_mul(scale),
        uniforms: GlUniforms {
            // Order is part of the golden table below; keep it stable.
            values: vec![
                (
                    "u_retained",
                    GlValue::Int(i32::from(config.persistence.min(KIT_MAX_PERSISTENCE))),
                ),
                (
                    "u_bloom_radius",
                    GlValue::Int(i32::try_from(bloom.radius).unwrap_or(0)),
                ),
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
                ("u_batch_step_back", GlValue::Int(batch_step_back)),
            ],
            data: Some(Arc::clone(samples)),
            grid: (cols, rows),
            step_seq,
        },
    }
}

/// One RGBA colour as a `vec4` of **0..255 channel values**, not 0..1.
///
/// The blit shader's arithmetic is integer end to end — the kit's `mix` is
/// `(a * (255 - t) + b * t + 127) / 255` — so the palette arrives in the units
/// that arithmetic is written in. Every value is a small integer, exactly
/// representable in `f32`, so the `ivec4(u_bg + 0.5)` on the far side recovers
/// it with no rounding question.
fn channels(rgba: kit::Rgba) -> GlValue {
    GlValue::Vec4([
        f32::from(rgba[0]),
        f32::from(rgba[1]),
        f32::from(rgba[2]),
        f32::from(rgba[3]),
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        BEAM_FRAG, BEAM_VERT, BLIT_FRAG, BLUR_H_FRAG, BLUR_V_FRAG, DECAY_FRAG, FULLSCREEN_VERT,
        GlUniforms, GlValue, KIT_MAX_PERSISTENCE, NO_BATCH, SCOPE_PIPELINE, kit, scope_surface,
        vocab,
    };
    use std::sync::Arc;

    /// A fixed palette, so the golden table below is a function of the mapping
    /// and not of whatever accent the process happens to carry. Shaped like the
    /// CRT's (the only skin with a mask) so every field is exercised.
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

    fn config() -> vocab::ScopeConfig {
        vocab::ScopeConfig {
            cols: 48,
            rows: 24,
            scale: 2,
            persistence: 184,
            ..vocab::ScopeConfig::default()
        }
    }

    /// **The golden uniform table.** Every value the shaders read, in order,
    /// for one fully-specified state.
    ///
    /// This is the seam CI can actually gate: the pixels are live-verify (there
    /// is no GL under `xvfb`), so what a hermetic test can hold is that the
    /// *inputs* to the shaders are the ones the kit would have used. A wrong
    /// number here is a wrong colour, a wrong fade rate or a missing scanline
    /// on glass, with nothing else to catch it.
    ///
    /// **Falsified** by swapping any two fields of the mapping — `u_bg` with
    /// `u_ink`, `u_mask_phase` with `u_mask_pitch`, `u_bloom_radius` with
    /// `u_bloom_strength` — each of which moves exactly one row.
    #[test]
    fn the_uniform_table_is_the_kits_own_numbers() {
        let samples: Arc<[f32]> = Arc::from(&[0.0f32, 0.5, -0.5][..]);
        let surface = scope_surface(config(), &samples, Some(6), 7, &crt_like());

        assert_eq!(surface.width, 96, "cols * scale, as Frame::upscale would");
        assert_eq!(surface.height, 48, "rows * scale");
        assert_eq!(surface.uniforms.grid, (48, 24), "the pre-upscale grid");
        assert_eq!(surface.uniforms.step_seq, 7);
        assert!(
            surface
                .uniforms
                .data
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, &samples)),
            "the batch travels as the caller's allocation, not a copy"
        );

        assert_eq!(
            surface.uniforms.values,
            vec![
                ("u_retained", GlValue::Int(184)),
                ("u_bloom_radius", GlValue::Int(3)),
                ("u_bloom_strength", GlValue::Int(190)),
                ("u_bg", GlValue::Vec4([3.0, 7.0, 5.0, 255.0])),
                ("u_ink", GlValue::Vec4([92.0, 255.0, 130.0, 255.0])),
                ("u_mask_on", GlValue::Int(1)),
                ("u_mask_pitch", GlValue::Int(4)),
                ("u_mask_phase", GlValue::Int(3)),
                ("u_scanline_keep", GlValue::Int(150)),
                ("u_corner_keep", GlValue::Int(115)),
                // step 6 is the newest of 7, so the batch is 0 steps back.
                ("u_batch_step_back", GlValue::Int(0)),
            ]
        );
    }

    /// A glow-free skin (the reflective LCD) reaches the shaders as radius 0
    /// and strength 0 — which makes the blur the identity and the blit's
    /// max-combine a no-op, so it gets `Emission::bloom`'s early-return bytes
    /// out of the same pipeline. A mask-free skin zeroes the whole CRT group.
    #[test]
    fn a_glow_free_skin_zeroes_the_bloom_and_the_mask() {
        let lcd = kit::palette_snapshot(kit::DisplayStyle::Lcd);
        assert!(lcd.bloom.is_none() && lcd.mask.is_none(), "the LCD premise");
        let samples: Arc<[f32]> = Arc::from(&[0.0f32][..]);
        let surface = scope_surface(config(), &samples, None, 1, &lcd);
        let value = |name: &str| {
            surface
                .uniforms
                .values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, v)| *v)
        };
        assert_eq!(value("u_bloom_radius"), Some(GlValue::Int(0)));
        assert_eq!(value("u_bloom_strength"), Some(GlValue::Int(0)));
        assert_eq!(value("u_mask_on"), Some(GlValue::Int(0)));
        assert_eq!(value("u_mask_pitch"), Some(GlValue::Int(0)));
        assert_eq!(value("u_scanline_keep"), Some(GlValue::Int(0)));
    }

    /// `u_batch_step_back` counts **back from the newest step**, so the beam
    /// shader can tell which of several replayed steps stamps the batch — the
    /// CPU kit stamps a batch on the first step of an advance and flatlines on
    /// the axis for the rest, and this is what reproduces that.
    ///
    /// Counting back rather than forward is what makes it overflow-proof: an
    /// absolute step index would outgrow the `int` a uniform carries after a
    /// few years of continuous animation, and would then compare equal to a
    /// saturated step index and stamp the wrong frame.
    #[test]
    fn the_batch_step_is_carried_as_a_distance_back_from_the_newest() {
        let samples: Arc<[f32]> = Arc::from(&[0.25f32][..]);
        let back = |batch: Option<u64>, step_seq: u64| {
            let surface = scope_surface(config(), &samples, batch, step_seq, &crt_like());
            surface
                .uniforms
                .values
                .iter()
                .find(|(key, _)| *key == "u_batch_step_back")
                .map(|(_, v)| *v)
                .expect("the mapping always emits u_batch_step_back")
        };
        assert_eq!(back(Some(0), 1), GlValue::Int(0), "the debut batch");
        assert_eq!(back(Some(9), 10), GlValue::Int(0), "stamped on the newest");
        assert_eq!(
            back(Some(7), 10),
            GlValue::Int(2),
            "two steps of decay since"
        );
        assert_eq!(back(None, 10), GlValue::Int(NO_BATCH), "no batch to stamp");
        assert_eq!(
            back(Some(11), 10),
            GlValue::Int(NO_BATCH),
            "a batch ahead of the counter cannot be stamped by any replayed step"
        );
    }

    /// Persistence is clamped to the kit's ceiling before the decay shader
    /// shifts by it. Past `256` the recurrence `(v * retained) >> 8` would
    /// *grow* the phosphor instead of fading it — a runaway white screen rather
    /// than a long trail.
    #[test]
    fn persistence_is_clamped_to_the_kits_ceiling() {
        let samples: Arc<[f32]> = Arc::from(&[0.0f32][..]);
        let retained = |persistence: u16| {
            let mut config = config();
            config.persistence = persistence;
            let surface = scope_surface(config, &samples, None, 1, &crt_like());
            surface.uniforms.values[0]
        };
        assert_eq!(retained(184), ("u_retained", GlValue::Int(184)));
        assert_eq!(retained(256), ("u_retained", GlValue::Int(256)));
        assert_eq!(
            retained(u16::MAX),
            ("u_retained", GlValue::Int(i32::from(KIT_MAX_PERSISTENCE))),
            "an out-of-range persistence cannot make the decay a gain"
        );
    }

    /// The two blur shaders are one body with one line prepended, so they can
    /// only ever differ in the axis. Checked because a hand-copied second
    /// truncating integer blur is exactly the kind of duplicate that drifts.
    #[test]
    fn the_blur_halves_share_one_body() {
        let strip = |src: &str| src.split_once('\n').map(|(_, rest)| rest.to_owned());
        assert_eq!(strip(BLUR_H_FRAG), strip(BLUR_V_FRAG));
        assert!(BLUR_H_FRAG.starts_with("const ivec2 BLUR_DIR = ivec2(1, 0);"));
        assert!(BLUR_V_FRAG.starts_with("const ivec2 BLUR_DIR = ivec2(0, 1);"));
    }

    /// The pipeline's shape, which is the part a reader of the GLSL cannot see:
    /// two passes per animation step over a ping-pong accumulator, three per
    /// render, and the beam `GL_MAX`-blended because the kit stamps with `max`.
    ///
    /// **Falsified** by flipping the beam pass to `Replace`, which would erase
    /// the decayed trail every step and leave a 1-frame trace with no phosphor
    /// at all.
    #[test]
    fn the_pipeline_steps_the_accumulator_and_ends_on_the_screen() {
        use hytte::ui::gl_surface::{GlBlend, GlDraw, GlTarget};
        assert_eq!(SCOPE_PIPELINE.step.len(), 2, "decay then beam");
        assert_eq!(SCOPE_PIPELINE.step[0].blend, GlBlend::Replace);
        assert_eq!(SCOPE_PIPELINE.step[1].blend, GlBlend::Max);
        assert_eq!(SCOPE_PIPELINE.step[1].draw, GlDraw::PerColumn);
        for pass in SCOPE_PIPELINE.step {
            assert_eq!(pass.target, GlTarget::Accumulator);
        }
        assert_eq!(SCOPE_PIPELINE.frame.len(), 3, "blur H, blur V, blit");
        assert_eq!(
            SCOPE_PIPELINE.frame[2].target,
            GlTarget::Screen,
            "the last pass is the only one that reaches the GLArea"
        );
        for pass in SCOPE_PIPELINE.frame {
            assert_ne!(
                pass.target,
                GlTarget::Accumulator,
                "a frame pass writing the accumulator would feed back into the next step"
            );
        }
        // Every aux slot the passes address is declared.
        let declared = usize::from(SCOPE_PIPELINE.aux);
        for pass in SCOPE_PIPELINE.step.iter().chain(SCOPE_PIPELINE.frame) {
            if let GlTarget::Aux(slot) = pass.target {
                assert!(usize::from(slot) < declared, "aux {slot} is undeclared");
            }
        }
    }

    /// Every `uniform` name declared by a shader this pipeline ships, in
    /// declaration order per file. Parsed out of the GLSL rather than listed,
    /// which is the whole point of the test below.
    fn declared_uniforms(source: &str) -> Vec<&str> {
        source
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("uniform ")?;
                // `uniform <type> <name>;` — take the word before the `;`.
                let (declaration, _) = rest.split_once(';')?;
                declaration.split_whitespace().next_back()
            })
            .collect()
    }

    /// **The uniform bag and the shipped GLSL agree, in both directions** —
    /// derived from the shader sources, not from a list kept beside them.
    ///
    /// This is the seam that rots. `hytte-ui` sets every name in the bag on
    /// *every* pass and GL ignores a name a program does not declare (location
    /// `-1`), so neither half fails loudly when they drift: a `uniform` added
    /// to a shader and forgotten in the mapping reads as **zero** — a black
    /// ink, a dead scanline, a phosphor that never fades — and a name left in
    /// the bag after its shader stopped declaring it is a silent no-op that
    /// still costs a `glGetUniformLocation` miss per pass per frame. Neither
    /// shows up in the golden table, and CI has no driver to notice.
    ///
    /// A hand-written list cannot hold this: it goes stale in exactly the
    /// commit that introduces the drift, and then passes.
    ///
    /// **Falsified** two ways, each moving one direction of the assertion:
    /// add `uniform int u_unused;` to any shader (the first assertion goes
    /// red), or delete a `("u_…", …)` row from [`scope_surface`]'s `values`
    /// (the second does).
    #[test]
    fn the_mapping_fills_every_uniform_the_shaders_read() {
        // What `hytte-ui`'s `GlSurface` supplies itself, before the host's bag
        // is applied — see `gl_surface::imp::Resources::run`. A shader may
        // declare these; the mapping must not.
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
            ("fullscreen.vert", FULLSCREEN_VERT),
            ("scope_beam.vert", BEAM_VERT),
            ("scope_beam.frag", BEAM_FRAG),
            ("scope_decay.frag", DECAY_FRAG),
            ("scope_blur.frag (H)", BLUR_H_FRAG),
            ("scope_blur.frag (V)", BLUR_V_FRAG),
            ("scope_blit.frag", BLIT_FRAG),
        ];

        let samples: Arc<[f32]> = Arc::from(&[0.0f32][..]);
        let surface = scope_surface(config(), &samples, Some(0), 1, &crt_like());
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

        // The premise. Direction 2 below already fails if the scan finds
        // nothing, but it cannot notice a scan that finds only the *host's*
        // names — so those are asserted separately. (Two shaders legitimately
        // declare none: `fullscreen.vert` reads no state, and
        // `scope_beam.frag` gets everything through `flat in` varyings.)
        assert!(
            HOST_SUPPLIED
                .iter()
                .filter(|name| declared.contains(name))
                .count()
                >= 4,
            "the scan found {} uniform(s) and almost none of the host's — it is \
             the parse that is broken, not the shaders: {declared:?}",
            declared.len(),
        );

        // Direction 1: nothing a shader reads is left unset. An unset uniform
        // reads as zero, which is a wrong picture rather than an error.
        for name in &declared {
            assert!(
                HOST_SUPPLIED.contains(name) || bag.contains(name),
                "{name} is declared by a shader but neither host-supplied nor in the bag",
            );
        }
        // Direction 2: nothing in the bag is dead weight, and nothing shadows a
        // name the surface supplies itself (the module docs forbid that — the
        // host writes its five *before* the bag, so a shadow would win).
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
}
