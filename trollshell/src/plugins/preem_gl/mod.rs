//! The GL arm's shell-side glue (#893 stage B): which renderer a `Scope`
//! takes, and the one-time registration that teaches `hytte-ui` how to draw it.
//!
//! The pipeline itself and the `(config, samples, step_seq, palette) →
//! GlUniforms` mapping live in [`program`], which references nothing above it
//! so the parity harness can include the same code the shell runs. Everything
//! that needs the *shell* — the kill switch, the fallback latch — is here.
//!
//! # #1072: the shipped default is **CPU**; `TROLLSHELL_PREEM_RENDERER=gl` opts in
//!
//! GL was Annika's call on #893 (`--areas 3` layer-shell, jank 0, p95 16.77 ms
//! against a 16.67 ms idle baseline) — but that call assumed the GL loader
//! worked, and it never had: #1067 found `hytte-gl`'s loader asking every
//! source for entry-point spellings nothing on this platform exports, so the
//! GL arm silently stayed on the CPU fallback for the whole life of #893
//! stage B and was never actually compared against the CPU kit on real glass.
//! The day #1067's fix landed, `preem_gl_diff` ran for the first time with a
//! working GL arm and reported **12 / 12 cases over the parity ceiling**
//! (#1072) — an unclassified disagreement that could be the GL arm's shader
//! math, the parity harness's own assumptions (llvmpipe vs. the expected
//! driver, premultiply/gamma handling), or both.
//!
//! So until #1072 closes with that 12/12 read down to 0, **GL is opt-in, not
//! the default**: unset — or set to anything but the exact word `gl` — a
//! `Scope` renders on the CPU kit, the picture every user has actually been
//! seeing; `gl` renders on a `GtkGLArea` running the pipeline this module
//! registers. The PR that closes #1072 flips this back to GL-default, once
//! the parity numbers back that call up. `RUST_LOG=hytte_gl=debug` names
//! which loader route resolved the entry points, for anyone opting in.
//!
//! Read **once**, at the first `Scope` build, and memoized: a shell whose
//! renderer changed under it mid-session would be far more confusing than one
//! that needs a restart, and the value is a debugging switch rather than a
//! setting.
//!
//! The CPU arm is used in three cases, and only these:
//!
//! 1. the switch is not set to `gl` (the default, per #1072 above);
//! 2. the widget kind has no GL arm — everything but `Scope` in this PR;
//! 3. **a GL context could not be created**, which `hytte-ui` latches and
//!    reports through the hook installed in [`install`]. Falling back is free
//!    here in a way it is not for #893's shader widget: a kit widget *has* a
//!    CPU implementation, and it is the reference the GL arm is measured
//!    against, so a blank chip would be strictly worse than drawing it.
//!
//! The switch is **unconditional**, and since #978 that includes the widget
//! that has no CPU arm: `shader_map::refusal` reads [`shader_arm`] and refuses
//! every plugin shader unless the switch names `gl`, drawing the broken-widget
//! placeholder otherwise. That placeholder is not a #1072 regression — before
//! #1067 the GL loader never worked, so a plugin shader never actually
//! compiled in production either; #1072's default keeps that same visible
//! outcome (an empty tile) rather than switching every shipped shader plugin
//! on for the first time with no parity evidence behind it.
//!
//! # What this module does not do
//!
//! It does not touch `pump.rs`. `Renderer::ScopeGl` carries the same
//! `pending`/`idle`/`fades`/`settle_steps` fields as the CPU arm and answers
//! `animates()` with the same expression, so #926's frame-clock park and unpark
//! behave identically and the animation half of the host needed no change at
//! all. That is a deliberate property of the seam, not a coincidence.

mod program;

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

pub(super) use program::{SCOPE, SCOPE_PIPELINE, ScopeSurface, scope_surface};

/// The renderer switch. `gl` opts into the GL arm; unset, `cpu`, or anything
/// else is the CPU kit — see the module docs (#1072) for why the default
/// flipped and how to read the name.
pub(super) const RENDERER_ENV: &str = "TROLLSHELL_PREEM_RENDERER";

/// Which renderer a `Scope` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arm {
    /// A `GtkGLArea` running the [`SCOPE`] pipeline — opt-in via `gl` while
    /// #1072 is open; the PR that closes it makes this the default again.
    Gl,
    /// The `hytte-preem` kit, rasterised in-process into a `PixelSurface` —
    /// the default while #1072 is open.
    Cpu,
}

/// Parse [`RENDERER_ENV`].
///
/// #1072: the shipped default is **CPU** until `preem_gl_diff` reads 12/12
/// under the parity ceiling — #1067 finally got the GL loader resolving real
/// entry points, and the harness immediately found the GL arm disagreeing
/// with the CPU reference on every measured case, with no classification yet
/// of whether that is the shader math, the parity harness's own assumptions,
/// or both. So the parse is deliberately lopsided the other way from before:
/// only the exact word `gl` (case- and whitespace-insensitive) opts into GL,
/// and every other value — including a typo, an empty string, or the old
/// explicit `cpu` spelling — keeps the CPU default. A switch that failed
/// *open* on a typo would silently switch every preem chip's renderer with no
/// parity evidence behind it, which is exactly the failure #1072 exists to
/// prevent; the PR that closes #1072 flips this match's default arm back to
/// [`Arm::Gl`].
///
/// Split from [`arm`] so the decision is testable without touching the
/// process environment, which is what makes the "CPU by default" contract a
/// hermetic assertion rather than a live-verify note.
fn arm_from_env(value: Option<&str>) -> Arm {
    match value {
        Some(value) if value.trim().eq_ignore_ascii_case("gl") => Arm::Gl,
        _ => Arm::Cpu,
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
    /// Before #1072, production's own default was GL, so this constant merely
    /// matched it; since #1072 flipped the shipped default to CPU (shared with
    /// [`arm`] through the same [`configured_arm`]), this default no longer
    /// tracks production at all, on purpose: [`TEST_ARM`] pins `Cpu` so the
    /// byte-parity suite always measures the kit regardless of the shipped
    /// default, and this pins `Gl` so the shader test suite always exercises
    /// compilation regardless of it too. Defaulting *this* one to `Cpu` would
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
/// **Since #1072,** [`configured_arm`]'s own default is CPU rather than GL, so
/// in production this function now answers `Cpu` unless the switch names
/// `gl`, with no code in this function changing at all. That is not a new
/// regression for a plugin shader: before #1067 the GL loader never resolved
/// real entry points, so a shader never actually compiled in production
/// either — the visible result was already the broken-widget placeholder. The
/// PR that closes #1072 restores GL as the shared default here too.
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

/// Register the `Scope` pipeline and the context-failure hook.
///
/// Called once from `plugins::install`, on the GTK main thread, before any
/// plugin tree is reconciled. Cheap: registering a pipeline stores a `Copy`
/// struct in a map — no GL is touched until a surface realizes.
pub(super) fn install() {
    hytte::ui::gl_surface::register(SCOPE, SCOPE_PIPELINE);
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
    use super::{Arm, RENDERER_ENV, arm_from_env, shader_arm, with_cpu_kill_switch};

    /// The shader path's test default is **GL**, and [`with_cpu_kill_switch`]
    /// is the seam that turns it off and puts it back.
    ///
    /// The default matters as much as the seam, but since #1072 it is a *test*
    /// default rather than the production one: `configured_arm`'s shared
    /// default flipped to CPU, so `shader_arm`'s production answer flipped
    /// with it (see `shader_arm`'s doc). `TEST_SHADER_ARM` stays pinned to
    /// `Gl` anyway so every other test in the suite that maps a shader node
    /// keeps exercising the compile path rather than the refusal — flipping
    /// this `const` initialiser to `Arm::Cpu` turns roughly a dozen
    /// `shader_map` and `pump` assertions red, which is the intended
    /// tripwire, exactly as it was before #1072.
    ///
    /// **Falsified** by making [`with_cpu_kill_switch`] not restore the
    /// previous value (the third assertion), or by defaulting
    /// `TEST_SHADER_ARM` to `Arm::Cpu` (the first).
    #[test]
    fn the_shader_arm_defaults_to_gl_and_the_seam_turns_it_off() {
        assert_eq!(
            shader_arm(),
            Arm::Gl,
            "the test default, not (since #1072) production's"
        );
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

    /// **#1072: CPU is the default**, so the variable is an opt-in rather than
    /// a kill switch while the parity gap stays unclassified. Unset means CPU.
    ///
    /// This is the hermetic half of that contract — the pixels are
    /// live-verify, but which arm the shell *chooses* is a pure function and
    /// is gated here.
    ///
    /// **Falsified** by flipping the match's default back to `Arm::Gl` — #893's
    /// original call, restored by whichever PR closes #1072.
    #[test]
    fn cpu_is_the_default_and_only_gl_turns_it_on() {
        assert_eq!(arm_from_env(None), Arm::Cpu, "unset is CPU (#1072)");
        assert_eq!(arm_from_env(Some("gl")), Arm::Gl);
        assert_eq!(arm_from_env(Some("GL")), Arm::Gl, "case-insensitive");
        assert_eq!(arm_from_env(Some("  gl \n")), Arm::Gl, "trimmed");
        assert_eq!(
            arm_from_env(Some("cpu")),
            Arm::Cpu,
            "the redundant spelling"
        );
    }

    /// The opt-in fails **closed**: anything that is not the word `gl` leaves
    /// CPU on, including a typo and an empty string. Failing *open* on a
    /// misspelling would switch a preem chip's renderer with no parity
    /// evidence behind it — the exact failure #1072 exists to prevent.
    #[test]
    fn an_unrecognised_value_leaves_cpu_on() {
        for value in ["", " ", "gl!", "glx", "opengl", "0", "true", "yes-gl"] {
            assert_eq!(
                arm_from_env(Some(value)),
                Arm::Cpu,
                "{value:?} does not opt into GL"
            );
        }
    }
}
