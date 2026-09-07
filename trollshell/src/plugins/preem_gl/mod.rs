//! The GL arm's shell-side glue (#893 stage B): which renderer a `Scope`
//! takes, and the one-time registration that teaches `hytte-ui` how to draw it.
//!
//! The pipeline itself and the `(config, samples, step_seq, palette) →
//! GlUniforms` mapping live in [`program`], which references nothing above it
//! so the parity harness can include the same code the shell runs. Everything
//! that needs the *shell* — the kill switch, the fallback latch — is here.
//!
//! # GL is the default; `TROLLSHELL_PREEM_RENDERER=cpu` is a kill switch
//!
//! Annika's call on #893, taken after the stage-A numbers came back GO
//! (`--areas 3` layer-shell, jank 0, p95 16.77 ms against a 16.67 ms idle
//! baseline). So the variable is **not** an opt-in: unset — or set to anything
//! but `cpu` — a `Scope` renders on a `GtkGLArea`, and `cpu` forces the CPU kit
//! for every preem widget in the shell.
//!
//! Read **once**, at the first `Scope` build, and memoized: a shell whose
//! renderer changed under it mid-session would be far more confusing than one
//! that needs a restart, and the value is a debugging switch rather than a
//! setting.
//!
//! The CPU arm is used in three cases, and only these:
//!
//! 1. the kill switch is set;
//! 2. the widget kind has no GL arm — everything but `Scope` in this PR;
//! 3. **a GL context could not be created**, which `hytte-ui` latches and
//!    reports through the hook installed in [`install`]. Falling back is free
//!    here in a way it is not for #893's shader widget: a kit widget *has* a
//!    CPU implementation, and it is the reference the GL arm is measured
//!    against, so a blank chip would be strictly worse than drawing it.
//!
//! # What this module does not do
//!
//! It does not touch `pump.rs`. `Renderer::ScopeGl` carries the same
//! `pending`/`idle`/`fades`/`settle_steps` fields as the CPU arm and answers
//! `animates()` with the same expression, so #926's frame-clock park and unpark
//! behave identically and the animation half of the host needed no change at
//! all. That is a deliberate property of the seam, not a coincidence.

mod program;

pub(super) use program::{SCOPE, SCOPE_PIPELINE, ScopeSurface, scope_surface};

/// The kill switch. `cpu` forces the CPU kit; unset or anything else is GL.
pub(super) const RENDERER_ENV: &str = "TROLLSHELL_PREEM_RENDERER";

/// Which renderer a `Scope` takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Arm {
    /// A `GtkGLArea` running the [`SCOPE`] pipeline — the default.
    Gl,
    /// The `hytte-preem` kit, rasterised in-process into a `PixelSurface`.
    Cpu,
}

/// Parse [`RENDERER_ENV`].
///
/// A **kill switch**, so the parse is deliberately lopsided: only the exact
/// word `cpu` (case- and whitespace-insensitive) turns GL off, and every other
/// value — including a typo, an empty string, or a hopeful `gl` — leaves the
/// default in place. A switch that failed *closed* on a typo would silently
/// take the shell off the path it is supposed to be on and look like a GL bug.
///
/// Split from [`arm`] so the decision is testable without touching the
/// process environment, which is what makes the "GL by default" contract a
/// hermetic assertion rather than a live-verify note.
fn arm_from_env(value: Option<&str>) -> Arm {
    match value {
        Some(value) if value.trim().eq_ignore_ascii_case("cpu") => Arm::Cpu,
        _ => Arm::Gl,
    }
}

thread_local! {
    /// The arm under `cargo test`, defaulting to the **CPU**.
    ///
    /// Not the production default, and that is the point: CI has no GL, so
    /// every byte-parity assertion in `plugins::tests` — the ones that hold the
    /// CPU arm to the kit — has to run against the CPU arm to mean anything.
    /// The GL arm's own tests opt in with [`with_gl_arm`], and the *default*
    /// itself is covered by [`arm_from_env`]'s tests, which is where the
    /// decision actually lives.
    #[cfg(test)]
    static TEST_ARM: std::cell::Cell<Arm> = const { std::cell::Cell::new(Arm::Cpu) };
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
        // host is to make the fallback take effect promptly: dropping the
        // cached frames means the next mapping pass re-runs `apply`, which
        // rebuilds every `ScopeGl` instance onto the CPU kit (see
        // `preem_render::apply`).
        //
        // No repaint is *forced* from here. `pump`'s fan-out is private to that
        // module and this change deliberately leaves `pump.rs` untouched — and
        // it costs nothing: a scope that just failed to get a context is,
        // almost by definition, still animating (it was built because something
        // was moving), so the next frame-clock tick re-maps it anyway. A
        // settled one has nothing on screen to correct.
        super::preem_render::invalidate_cached_frames();
    });
}

#[cfg(test)]
mod tests {
    use super::{Arm, RENDERER_ENV, arm_from_env};

    /// The switch keeps the name the module docs, `docs/live-verify.md` and the
    /// journal all quote. A rename that misses one of those leaves an operator
    /// setting a variable nothing reads, with no diagnostic — the switch just
    /// silently stops working.
    #[test]
    fn the_kill_switch_keeps_its_documented_name() {
        assert_eq!(RENDERER_ENV, "TROLLSHELL_PREEM_RENDERER");
    }

    /// **GL is the default** (#893, Annika's answer 1: "1 default"), so the
    /// variable is a kill switch rather than an opt-in. Unset means GL.
    ///
    /// This is the hermetic half of that contract — the pixels are live-verify,
    /// but which arm the shell *chooses* is a pure function and is gated here.
    ///
    /// **Falsified** by flipping the match's default to `Arm::Cpu`, which is
    /// what the spec proposed before Annika overrode it.
    #[test]
    fn gl_is_the_default_and_only_cpu_turns_it_off() {
        assert_eq!(arm_from_env(None), Arm::Gl, "unset is GL");
        assert_eq!(arm_from_env(Some("cpu")), Arm::Cpu);
        assert_eq!(arm_from_env(Some("CPU")), Arm::Cpu, "case-insensitive");
        assert_eq!(arm_from_env(Some("  cpu \n")), Arm::Cpu, "trimmed");
        assert_eq!(arm_from_env(Some("gl")), Arm::Gl, "the redundant spelling");
    }

    /// A kill switch fails **open**: anything that is not the word `cpu` leaves
    /// GL on, including a typo and an empty string. Failing closed would take
    /// the shell off the default path on a misspelling and look exactly like a
    /// GL bug.
    #[test]
    fn an_unrecognised_value_leaves_gl_on() {
        for value in ["", " ", "cpü", "CPU!", "opengl", "0", "false", "no-gl"] {
            assert_eq!(
                arm_from_env(Some(value)),
                Arm::Gl,
                "{value:?} is not the kill switch"
            );
        }
    }
}
