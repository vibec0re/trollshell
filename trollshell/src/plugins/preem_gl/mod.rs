//! The GL arm's shell-side glue (#893 stage B): which renderer a kit widget
//! takes, and the one-time registration that teaches `hytte-ui` how to draw
//! each pipeline.
//!
//! The pipelines themselves and their pure `state → GlUniforms` mappings live
//! one module per kind — [`program`] for the `Scope`, [`gauge`] for the
//! `Gauge` (#1143), [`dot_matrix`] for the `DotMatrix` (#1144), [`marquee`]
//! and [`textbox`] for the two text kinds (#1152) — each referencing nothing
//! above it, so the parity harness can `#[path]`-include the same code the
//! shell runs. Everything that needs the *shell* — the kill switch, the
//! fallback latch — is here, and it is deliberately kind-agnostic. #1143
//! predicted "a third kind is a module, a `register` line and a `preem_render`
//! arm, with nothing in this file to change"; #1144 was exactly that and #1152
//! was exactly that twice, so the prediction now reads as a measurement.
//!
//! The [`marquee`] is the one module here with no shader of its own: a ticker
//! is the **same dot hardware** as a static display on a different grid, so it
//! registers `dot_matrix`'s pipeline under its own name and drives `site_at`'s
//! three grid uniforms with a continuous matrix's numbers. See its module docs.
//!
//! # The switch: GL is the default, `TROLLSHELL_PREEM_RENDERER=cpu` is the kill switch
//!
//! GL was Annika's call on #893 (`--areas 3` layer-shell, jank 0, p95 16.77 ms
//! against a 16.67 ms idle baseline) — but that call assumed the GL loader
//! worked, and it never had: #1067 found `hytte-gl`'s loader asking every
//! source for entry-point spellings nothing on this platform exports, so the
//! GL arm silently stayed on the CPU fallback for the whole life of #893
//! stage B and was never actually compared against the CPU kit on real glass.
//! The day #1067's fix landed, `preem_gl_diff` ran for the first time with a
//! working GL arm and reported **12 / 12 cases over the parity ceiling**, so
//! #1072 parked the default on CPU rather than switch every chip's renderer on
//! an unclassified disagreement.
//!
//! **Classified, all twelve were the harness, not the renderer.** A
//! `GtkGLArea` does not own one framebuffer: `gtk_gl_area_snapshot` hands the
//! texture it just drew to GSK and the next `attach_buffers` takes a
//! *different* one out of the area's pool, so `preem_gl_diff`'s post-hoc
//! readback was scoring each case against its predecessor's picture (and the
//! first case against an untouched texture, which it duly reported as
//! `FAIL(blank)`). Reading after the pool has settled, and requiring two
//! renders of the same state to read back identically, the arms come out
//! **byte-identical** — max |Δ| 0 of 255 on every channel of all twelve cases,
//! under llvmpipe. The default is GL again, and the numbers are in
//! `docs/live-verify.md`.
//!
//! So: unset — or anything but the exact word `cpu` — a `Scope` renders on a
//! `GtkGLArea` running the pipeline this module registers; `cpu` forces the
//! kit. `RUST_LOG=hytte_gl=debug` names which loader route resolved the entry
//! points.
//!
//! Read **once**, at the first `Scope` build, and memoized: a shell whose
//! renderer changed under it mid-session would be far more confusing than one
//! that needs a restart, and the value is a debugging switch rather than a
//! setting.
//!
//! The CPU arm is used in three cases, and only these:
//!
//! 1. the switch names `cpu`;
//! 2. the widget kind has no GL arm — everything but `Scope`, `Gauge`,
//!    `DotMatrix`, `Marquee` and `TextBox` today (#1143 added the second,
//!    #1144 the third, and #1152 the two text kinds on Annika's word for the
//!    rest of #865);
//! 3. **a GL context could not be created**, which `hytte-ui` latches and
//!    reports through the hook installed in [`install`]. Falling back is free
//!    here in a way it is not for #893's shader widget: a kit widget *has* a
//!    CPU implementation, and it is the reference the GL arm is measured
//!    against, so a blank chip would be strictly worse than drawing it.
//!
//! The switch is **unconditional**, and since #978 that includes the widget
//! that has no CPU arm: `shader_map::refusal` reads [`shader_arm`] and refuses
//! every plugin shader when the switch names `cpu`, drawing the broken-widget
//! placeholder rather than compiling a plugin's GLSL behind an operator's back
//! — an operator who set the switch because GL was wedging the session meant
//! *that* too.
//!
//! # What this module does not do
//!
//! It does not touch `pump.rs`. `Renderer::ScopeGl` carries the same
//! `pending`/`idle`/`fades`/`settle_steps` fields as the CPU arm and answers
//! `animates()` with the same expression, so #926's frame-clock park and unpark
//! behave identically and the animation half of the host needed no change at
//! all. That is a deliberate property of the seam, not a coincidence — and
//! #1143's `Renderer::GaugeGl` takes it further by holding the very same
//! `kit::Gauge` the CPU arm does: the needle's spring is closed-form and
//! frame-rate independent, so the two gauge arms share their `update`,
//! `advance` and `animates` arms outright and differ only in what they hand
//! the reconciler.

mod dot_matrix;
mod gauge;
mod marquee;
mod program;
mod textbox;

/// The parity harness's arithmetic — see the module docs there.
///
/// Mounted **only under `cfg(test)`**, and that is the whole point of it being
/// a module rather than lines in `examples/preem_gl_diff.rs`: `cargo test` does
/// not run `#[test]`s inside an example (examples default to `test = false`),
/// so the statistic that decides #893's ceiling would have been guarded by
/// tests that compile and never execute. The harness `#[path]`-includes this
/// same file; the shell links none of it.
#[cfg(test)]
mod parity;

pub(super) use dot_matrix::{
    DOT_MATRIX, DOT_MATRIX_PIPELINE, Glyphs, dot_matrix_surface, glyphs as encode_glyphs,
};
pub(super) use gauge::{GAUGE, GAUGE_PIPELINE, gauge_surface};
pub(super) use marquee::{
    MARQUEE, MARQUEE_PIPELINE, Window, marquee_surface, window as encode_window,
};
pub(super) use program::{KitSurface, SCOPE, SCOPE_PIPELINE, scope_surface};
pub(super) use textbox::{
    Block, TEXTBOX, TEXTBOX_PIPELINE, block as encode_block, textbox_surface,
};

/// The renderer switch. `cpu` forces the kit; unset, `gl`, or anything else
/// takes the GL arm — see the module docs for the parity numbers behind that
/// default (#1072).
pub(super) const RENDERER_ENV: &str = "TROLLSHELL_PREEM_RENDERER";

/// Which renderer a kit widget takes.
///
/// **Per kind, not per widget** — one answer for the whole preem renderer, and
/// `preem_render::build` consults it in each arm that *has* a GL pipeline.
/// Since #1152 that is the `Scope`, the `Gauge`, the `DotMatrix`, the
/// `Marquee` and the `TextBox`; every other kind takes [`Arm::Cpu`] because
/// there is nothing else to take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arm {
    /// A `GtkGLArea` running one of the pipelines [`install`] registers — the
    /// default.
    Gl,
    /// The `hytte-preem` kit, rasterised in-process into a `PixelSurface` —
    /// the kill switch's arm, the fallback for a failed context, and what
    /// every kind without a GL arm takes.
    Cpu,
}

/// Parse [`RENDERER_ENV`].
///
/// **GL is the default again** (#1072 closed): the variable is back to being a
/// kill switch, so only the exact word `cpu` (case- and whitespace-insensitive)
/// forces the kit and every other value — a typo, an empty string, unset —
/// takes GL.
///
/// #1072 briefly inverted this. That inversion was not a judgement about the
/// shader math: `preem_gl_diff` had just run for the first time with a working
/// loader (#1067) and reported 12/12 cases over the ceiling, and switching
/// every preem chip's renderer on an unclassified disagreement was not a trade
/// worth taking. Classified, all twelve turned out to be the **harness**
/// reading the wrong framebuffer — `gtk_gl_area_snapshot` hands its texture to
/// GSK and the next `attach_buffers` takes a different one out of the area's
/// pool, so a post-hoc readback scored each case against its *predecessor's*
/// picture. With the readback taken after the pool has settled and proved
/// stable across two renders, the two arms are **byte-identical**: max |Δ| 0
/// of 255 on every channel of all twelve cases, under llvmpipe. So the parse
/// goes back to the shape #893 shipped, with the numbers behind it this time.
///
/// Split from [`arm`] so the decision is testable without touching the
/// process environment, which is what makes the default a hermetic assertion
/// rather than a live-verify note.
fn arm_from_env(value: Option<&str>) -> Arm {
    match value {
        Some(value) if value.trim().eq_ignore_ascii_case("cpu") => Arm::Cpu,
        _ => Arm::Gl,
    }
}

thread_local! {
    /// The arm under `cargo test`, defaulting to the **CPU**.
    ///
    /// CI has no GL, so every byte-parity assertion in `plugins::tests` — the
    /// ones that hold the CPU arm to the kit — has to run against the CPU arm
    /// to mean anything. That reasoning predates #1072 and does not depend on
    /// it: this test default was already decoupled from whatever
    /// [`arm_from_env`] answers in production, and stays `Cpu` regardless of
    /// which way that default currently points. The GL arm's own tests opt in
    /// with [`with_gl_arm`], and the *production* default itself is covered by
    /// [`arm_from_env`]'s own tests, which is where that decision actually
    /// lives.
    #[cfg(test)]
    static TEST_ARM: std::cell::Cell<Arm> = const { std::cell::Cell::new(Arm::Cpu) };

    /// The arm the **shader** path sees under `cargo test`, defaulting to
    /// **GL** — deliberately decoupled from production the same way
    /// [`TEST_ARM`] is, and for the mirror-image reason.
    ///
    /// Production's default is GL again now that #1072 has closed, so this
    /// constant happens to match it — but it is pinned rather than inherited,
    /// and the pinning is the point: [`TEST_ARM`] pins `Cpu` so the
    /// byte-parity suite always measures the kit whichever way the shipped
    /// default points, and this pins `Gl` so the shader test suite always
    /// exercises compilation. Defaulting *this* one to `Cpu` would
    /// make [`shader_arm`] answer `Cpu` for the whole test binary and every
    /// mapped shader in the suite would take the kill switch's refusal,
    /// proving nothing about the paths those tests exist to cover. A test that
    /// wants the switch on says so with [`with_cpu_kill_switch`].
    #[cfg(test)]
    static TEST_SHADER_ARM: std::cell::Cell<Arm> = const { std::cell::Cell::new(Arm::Gl) };
}

/// Run `body` with the GL arm selected — the seam the `ScopeGl` state-machine
/// tests use, since building a `Renderer::ScopeGl` needs no GL at all (only
/// *drawing* one does).
#[cfg(test)]
pub(super) fn with_gl_arm<T>(body: impl FnOnce() -> T) -> T {
    let previous = TEST_ARM.replace(Arm::Gl);
    let out = body();
    TEST_ARM.set(previous);
    out
}

/// The arm a `Scope` built now should take.
///
/// [`hytte::ui::gl_surface::gl_abandoned`] wins over the env: once a context
/// has failed there is nothing to fall back *to*, and a `ScopeGl` that can
/// never draw would leave a blank chip where the kit would have drawn a trace.
pub(super) fn arm() -> Arm {
    if hytte::ui::gl_surface::gl_abandoned() {
        return Arm::Cpu;
    }
    configured_arm()
}

/// The arm the **shader widget** takes — the kill switch, and only the kill
/// switch (#978).
///
/// Deliberately *not* [`arm`]: that one folds the context-failure latch in
/// because a kit widget wants one answer ("draw on the CPU"), while
/// `shader_map` wants the two apart. A shader has no CPU arm at all, so both
/// answers are "the placeholder", but they are different diagnoses pointing at
/// different fixes — *unset the variable* against *restart the shell* — and
/// `shader_map::refusal` keeps its own [`GlAvailability`] input for the
/// latch. Folding them here would hand it one `Cpu` for two causes and the
/// journal would name the wrong one roughly half the time.
///
/// The spec is unambiguous that the switch reaches here at all:
/// `docs/superpowers/specs/2026-09-06-preem-gl-renderer-design.md` calls
/// `TROLLSHELL_PREEM_RENDERER=cpu` "the kill switch, forcing CPU regardless of
/// GL availability", and before #978 the one widget in the shell that runs a
/// *plugin's* GPU code was the one widget that ignored it — an operator who
/// set it because GL was wedging the session still had plugin shaders
/// compiling and drawing, with the whole shell as the blast radius.
///
/// #1072 briefly inverted [`configured_arm`]'s default, which inverted this
/// function's production answer with it and no code here changing at all. With
/// #1072 closed on 12/12 byte-identical cases the default is GL again, so a
/// plugin shader compiles unless the operator says `cpu`.
///
/// [`GlAvailability`]: super::shader_map::GlAvailability
pub(super) fn shader_arm() -> Arm {
    #[cfg(test)]
    {
        TEST_SHADER_ARM.get()
    }
    #[cfg(not(test))]
    {
        configured_arm()
    }
}

/// Run `body` with the kill switch on, as far as [`shader_arm`] is concerned —
/// the seam `shader_map`'s refusal tests use, since the real switch is an env
/// var read once per process.
#[cfg(test)]
pub(super) fn with_cpu_kill_switch<T>(body: impl FnOnce() -> T) -> T {
    let previous = TEST_SHADER_ARM.replace(Arm::Cpu);
    let out = body();
    TEST_SHADER_ARM.set(previous);
    out
}

/// The configured arm, before the context-failure latch is consulted.
#[cfg(not(test))]
fn configured_arm() -> Arm {
    static ARM: std::sync::OnceLock<Arm> = std::sync::OnceLock::new();
    *ARM.get_or_init(|| arm_from_env(std::env::var(RENDERER_ENV).ok().as_deref()))
}

#[cfg(test)]
fn configured_arm() -> Arm {
    TEST_ARM.get()
}

/// Register every kit pipeline and the context-failure hook.
///
/// Called once from `plugins::install`, on the GTK main thread, before any
/// plugin tree is reconciled. Cheap: registering a pipeline stores a `Copy`
/// struct in a map — no GL is touched until a surface realizes — so a new
/// kind's cost here is one line and no startup work.
pub(super) fn install() {
    hytte::ui::gl_surface::register(SCOPE, SCOPE_PIPELINE);
    hytte::ui::gl_surface::register(GAUGE, GAUGE_PIPELINE);
    hytte::ui::gl_surface::register(DOT_MATRIX, DOT_MATRIX_PIPELINE);
    // The ticker's pipeline **is** the dot matrix's, under its own name so a
    // journal line says which widget is on screen — see `marquee`'s module
    // docs. `hytte-ui` compiles programs per surface, so the second name costs
    // one map entry and no extra compilation.
    hytte::ui::gl_surface::register(MARQUEE, MARQUEE_PIPELINE);
    hytte::ui::gl_surface::register(TEXTBOX, TEXTBOX_PIPELINE);
    hytte::ui::gl_surface::set_context_failure_handler(|_reason| {
        // `hytte-ui` has already logged the reason once. What is left for the
        // host is to make the fallback actually reach the screen, and that is
        // two things, because **waiting for the animation clock does not
        // work**: a plugin may declare `persistence: 256`, the kit's own
        // infinite-persistence value, and such a scope answers `animates()`
        // with `false` from the moment it is built. #926's clock parks it, no
        // mapping pass is coming, and `apply`'s `gl_lost` rebuild — which is
        // what handles the animating case — is never re-entered. The chip would
        // stay blank until the plugin sent another frame, which a settled
        // widget may never do.
        //
        // 1. Rebuild every `ScopeGl` instance onto the kit right now, the way
        //    `invalidate_cached_frames` rebuilds a `TextBox`. Also drops the
        //    caches, so a *non*-GL widget re-renders too.
        super::preem_render::rebuild_gl_renderers_on_cpu();
        super::preem_render::invalidate_cached_frames();
        // 2. Ask for one full re-map, so the reconciler swaps the on-screen
        //    `GlSurface` node for the `Pixels` one the rebuilt instance now
        //    produces. Guarded (a context can fail with no live host) and
        //    deferred to idle (we are inside a `GtkGLArea` realize/render
        //    handler, and reconciling a widget tree from there would create and
        //    destroy widgets mid-render).
        super::pump::request_preem_repaint_all_when_live();
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Arm, RENDERER_ENV, arm, arm_from_env, shader_arm, with_cpu_kill_switch, with_gl_arm,
    };

    /// **A failed GL context beats the switch, and keeps beating it** — the one
    /// branch that stands between a session with broken GL and a blank chip.
    ///
    /// It is not new code; what #1072 changed is what rests on it. While the
    /// shipped default was CPU, a regression in this branch was **masked**:
    /// [`configured_arm`](super::configured_arm) answered `Cpu` anyway, so a
    /// session whose context failed still got the kit by the other route. With
    /// GL the default again, `if gl_abandoned() { return Arm::Cpu; }` is the
    /// only thing left — and #1072's review found it was the one decision in
    /// this module with no test at all (the four here drove `arm_from_env` and
    /// `shader_arm`; none drove [`arm`]).
    ///
    /// Driven through the **real** latch, not a fake: `hytte-ui`'s `abandon_gl`
    /// is documented as exactly this seam — "a display server with no GL is not
    /// something a hermetic test can arrange, but 'behave as if the context had
    /// failed' is exactly one call". `plugins::tests` already reaches for it
    /// the same way. The latch is a thread-local that never clears, so this
    /// test's own thread is the blast radius, and the first assertion is the
    /// premise that says so out loud rather than passing vacuously if a future
    /// harness ever reused threads.
    ///
    /// **Falsified** by deleting the `gl_abandoned` branch from [`arm`]: the
    /// second assertion reports `Gl`.
    #[test]
    fn an_abandoned_context_forces_the_cpu_arm_over_the_configured_gl_default() {
        assert!(
            !hytte::ui::gl_surface::gl_abandoned(),
            "the premise: a fresh test thread has not abandoned GL",
        );
        with_gl_arm(|| {
            assert_eq!(arm(), Arm::Gl, "the premise: the switch says GL");

            hytte::ui::gl_surface::abandon_gl("no GL in this test");

            assert_eq!(
                arm(),
                Arm::Cpu,
                "a context that failed wins over the switch",
            );
            // Sticky, and that is the property a *session* depends on: the
            // fallback has to hold for every later mount, not just the one
            // that observed the failure.
            assert_eq!(arm(), Arm::Cpu, "…and keeps winning on the next mount");
        });
    }

    /// The shader path's test default is **GL**, and [`with_cpu_kill_switch`]
    /// is the seam that turns it off and puts it back.
    ///
    /// The default matters as much as the seam, and it is a *test* default,
    /// pinned rather than inherited from production — `TEST_SHADER_ARM` stays
    /// `Gl` whichever way `configured_arm` currently points, so every other
    /// test in the suite that maps a shader node keeps exercising the compile
    /// path rather than the refusal. Flipping this `const` initialiser to
    /// `Arm::Cpu` turns roughly a dozen `shader_map` and `pump` assertions
    /// red, which is the intended tripwire.
    ///
    /// **Falsified** by making [`with_cpu_kill_switch`] not restore the
    /// previous value (the third assertion), or by defaulting
    /// `TEST_SHADER_ARM` to `Arm::Cpu` (the first).
    #[test]
    fn the_shader_arm_defaults_to_gl_and_the_seam_turns_it_off() {
        assert_eq!(shader_arm(), Arm::Gl, "the test default, pinned here");
        with_cpu_kill_switch(|| {
            assert_eq!(shader_arm(), Arm::Cpu, "the seam turns the switch on");
        });
        assert_eq!(shader_arm(), Arm::Gl, "…and puts it back");
    }

    /// The switch keeps the name the module docs, `docs/live-verify.md` and the
    /// journal all quote. A rename that misses one of those leaves an operator
    /// setting a variable nothing reads, with no diagnostic — the switch just
    /// silently stops working.
    #[test]
    fn the_kill_switch_keeps_its_documented_name() {
        assert_eq!(RENDERER_ENV, "TROLLSHELL_PREEM_RENDERER");
    }

    /// **GL is the default and `cpu` is the kill switch** — #893's original
    /// call, restored by #1072 on 12/12 byte-identical parity cases (max |Δ| 0
    /// of 255 per channel, llvmpipe; see the module docs).
    ///
    /// This is the hermetic half of that contract — the pixels are
    /// live-verify, but which arm the shell *chooses* is a pure function and
    /// is gated here.
    ///
    /// **Falsified** by flipping the match's default arm to `Arm::Cpu`, which
    /// is exactly what #1072 did while the disagreement was unclassified.
    #[test]
    fn gl_is_the_default_and_cpu_is_the_kill_switch() {
        assert_eq!(arm_from_env(None), Arm::Gl, "unset is GL");
        assert_eq!(arm_from_env(Some("gl")), Arm::Gl, "the redundant spelling");
        assert_eq!(arm_from_env(Some("cpu")), Arm::Cpu);
        assert_eq!(arm_from_env(Some("CPU")), Arm::Cpu, "case-insensitive");
        assert_eq!(arm_from_env(Some("  cpu \n")), Arm::Cpu, "trimmed");
    }

    /// The kill switch is the **only** thing that turns GL off: anything that
    /// is not the word `cpu` leaves GL on, including a typo and an empty
    /// string.
    ///
    /// It is deliberately the weaker of the two directions to get wrong. A
    /// mistyped kill switch leaves the operator on a renderer that is
    /// byte-identical to the kit (#1072), and the shell says which arm it took
    /// under `RUST_LOG=hytte_gl=debug`; the context-failure latch in [`arm`]
    /// is what actually protects a session whose GL is broken, and no spelling
    /// of this variable can disable that.
    #[test]
    fn an_unrecognised_value_leaves_gl_on() {
        for value in ["", " ", "cpu!", "cpus", "software", "0", "true", "no-gl"] {
            assert_eq!(
                arm_from_env(Some(value)),
                Arm::Gl,
                "{value:?} does not name the kill switch"
            );
        }
    }
}
