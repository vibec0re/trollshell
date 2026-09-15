//! The GL arm's shell-side glue (#893 stage B): which renderer a kit widget
//! takes, and the one-time registration that teaches `hytte-ui` how to draw
//! each pipeline.
//!
//! The pipelines themselves and their pure `state → GlUniforms` mappings live
//! one module per kind — [`program`] for the `Scope`, [`gauge`] for the
//! `Gauge` (#1143), [`dot_matrix`] for the `DotMatrix` (#1144), [`marquee`]
//! and [`textbox`] for the two text kinds (#1152), [`led_strip`] for the meter
//! (#1153), [`seven_seg`] for the readout (#1154), [`flip_board`] for the
//! split-flap board and the nixie readout (#1155) — each referencing nothing
//! above it, so the parity harness can `#[path]`-include the same code the
//! shell runs. Everything that needs the *shell* — the kill switch, the
//! fallback latch — is here, and it is deliberately kind-agnostic. #1143
//! predicted "a third kind is a module, a `register` line and a `preem_render`
//! arm, with nothing in this file to change"; #1144 was exactly that, #1152
//! was exactly that twice, and #1153, #1154 and #1155 once more each, so the
//! prediction now reads as a measurement.
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
//!    `DotMatrix`, `Marquee`, `TextBox`, `LedStrip`, `SevenSeg` and
//!    `FlipBoard` today
//!    (#1143 added the second, #1144 the third, #1152 the two text kinds on
//!    Annika's word for the rest of #865, #1153 the meter, #1154 the
//!    readout and #1155 the board);
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

use hytte::ui::gl_surface::GlProgram;

mod dot_matrix;
mod flip_board;
mod gauge;
mod kind;
mod led_strip;
mod marquee;
mod program;
mod seven_seg;
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

/// The parity harness's case list — see the module docs there.
///
/// `#[cfg(test)]`-mounted for [`parity`]'s reason, and `#[path]`-included by
/// `examples/preem_gl_diff.rs` the same way. Split out of the example (#1211)
/// because a hermetic test cannot import from one: `cases_for`'s real output
/// is what `plugins::tests`' `kind_enumeration` checks against
/// [`kind::Kind::ALL`], rather than a hand-kept mirror of the harness's case
/// counts that could drift from it unnoticed.
#[cfg(test)]
mod cases;

// The `_PIPELINE` constant each kind is registered with is no longer named
// here (#1211): `install` and the harness's own registration both loop over
// `kind::Kind::ALL` and reach each pipeline through `Kind::gl_seam` instead,
// which resolves it via `super::{program, gauge, dot_matrix, marquee,
// textbox}` directly. The plain program name stays re-exported — used to
// build a `UiNode::GlSurface` at every mapping call site.
pub(super) use dot_matrix::{DOT_MATRIX, Glyphs, dot_matrix_surface, glyphs as encode_glyphs};
pub(super) use flip_board::{FLIP_BOARD, cards as encode_cards, flip_board_surface};
pub(super) use gauge::{GAUGE, gauge_surface};
pub(super) use led_strip::{LED_STRIP, led_strip_surface};
pub(super) use marquee::{MARQUEE, Window, marquee_surface, window as encode_window};
pub(super) use program::{KitSurface, SCOPE, scope_surface};
pub(super) use seven_seg::{Readout, SEVEN_SEG, readout as encode_readout, seven_seg_surface};
pub(super) use textbox::{Block, TEXTBOX, block as encode_block, textbox_surface};

/// Re-exported for `plugins::tests`' `kind_enumeration` module (#1211): both
/// `parity` and `cases` are private to this module (test-only, so a stray
/// non-test reference should not compile), and a sibling under `plugins`
/// cannot reach a private child of a private child without one of these.
#[cfg(test)]
pub(super) use cases::{Case, cases_for};
#[cfg(test)]
pub(super) use parity::Kind;

/// The renderer switch. `cpu` forces the kit; unset, `gl`, or anything else
/// takes the GL arm — see the module docs for the parity numbers behind that
/// default (#1072).
pub(super) const RENDERER_ENV: &str = "TROLLSHELL_PREEM_RENDERER";

/// Which renderer a kit widget takes.
///
/// **Per kind, not per widget** — one answer for the whole preem renderer, and
/// `preem_render::build` consults it in each arm that *has* a GL pipeline.
/// Since #1155 that is the `Scope`, the `Gauge`, the `DotMatrix`, the
/// `Marquee`, the `TextBox`, the `LedStrip`, the `SevenSeg` and the
/// `FlipBoard`; every other kind takes [`Arm::Cpu`] because there is nothing
/// else to take.
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
///
/// **The session-wide half of the answer**, and the whole of it only for a
/// caller with no pipeline in hand: [`arm_for`] is what `preem_render::build`
/// asks, because a driver that refuses one pipeline (#1232) may build every
/// other one.
pub(super) fn arm() -> Arm {
    if hytte::ui::gl_surface::gl_abandoned() {
        return Arm::Cpu;
    }
    configured_arm()
}

thread_local! {
    /// The pipelines this session's driver has **refused to build** (#1232).
    ///
    /// A `Vec` and a linear scan rather than a set: there are six registered
    /// programs in the whole shell, the list is empty on every healthy
    /// session, and [`arm_for`] runs once per renderer *build* — hashing to
    /// save at most five comparisons that never happen would be the wrong
    /// trade in both directions.
    ///
    /// **Sticky for the session**, like [`hytte::ui::gl_surface::gl_abandoned`]
    /// and deliberately unlike the per-surface latch it is fed from. That one
    /// is per `GdkGLContext` and clears on unrealize, because "the same GLSL
    /// compiles the same way" is an argument about one context; this one is a
    /// decision about which *renderer* to build, and re-offering a pipeline
    /// the driver has already refused — on every re-parent, each time
    /// restarting the phosphor from black to find out — is worse on the desk
    /// than a kit chip that stays a kit chip. The journal line names the
    /// program, so the restart that undoes it is an informed one.
    ///
    /// **Keyed by program, although the refusal arrives keyed by
    /// `(grid, program)`** — `hytte-ui`'s per-surface latch remembers the
    /// grid too, and this deliberately drops it (PR #1243 review, LOW 3). A
    /// compile or link refusal is a fact about the *source*, and the grid
    /// reaches a shader as a uniform rather than as a splice, so a driver
    /// that will not build `preem.scope` at 48×24 will not build it at 96×32
    /// either: condemning the program is the honest generalisation, and the
    /// alternative leaves every other-grid chip on GL to go blank one by one
    /// as each surface asks for itself. The one case it over-reaches is a
    /// genuinely grid-dependent refusal (an allocation the driver will not
    /// make at a large grid), where a small chip loses the GPU for a refusal
    /// that was not about it — and a kit chip is a far better outcome there
    /// than a blank one.
    ///
    /// Thread-local, like every other latch this module and `hytte-ui` keep:
    /// the GTK main thread is the only one that builds renderers, and a
    /// `#[test]`'s own thread is then the blast radius of anything it refuses.
    static REFUSED: std::cell::RefCell<Vec<GlProgram>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The arm a kit widget drawn by `program` should take: [`arm`], plus the one
/// thing that is per pipeline rather than per session (#1232).
///
/// A `GtkGLArea` can come up with a perfectly good context and still be
/// refused *one* pipeline — a compile or link failure, which `hytte-ui`
/// latches per surface (#1180 item 2) so the driver is asked exactly once.
/// Before this, nothing in the shell heard about that: the chip drew nothing
/// at all for the life of the context while `arm()` kept answering `Gl`,
/// because neither of the two failures it folds in had happened.
///
/// Narrower than a second `gl_abandoned`, deliberately: a refused
/// `preem.scope` says nothing about `preem.gauge`, and a session that loses
/// one pipeline keeps the GPU arm for every other kind on the bar.
pub(super) fn arm_for(program: GlProgram) -> Arm {
    if REFUSED.with_borrow(|refused| refused.contains(&program)) {
        return Arm::Cpu;
    }
    arm()
}

/// The host's half of `hytte-ui`'s build-refusal hook (#1232): record the
/// pipeline as refused, rebuild the chips that were drawing with it onto the
/// CPU kit, say so once, and ask for one re-map.
///
/// Registered by [`install`]; also the function a test drives, through
/// `hytte::ui::gl_surface::refuse_build` — the same entry point `GlSurface`
/// itself calls, so a test exercises the production wire rather than a
/// stand-in for it.
///
/// **Ordering, load-bearing, and it is `abandon_gl`'s lesson restated:**
/// the program is recorded *before* the rebuild, because the rebuild resolves
/// its arm through [`arm_for`], which reads that record. Reverse the two
/// statements and every rebuilt chip lands back on the GL arm — the function
/// silently accomplishes nothing, which is exactly the shape
/// `preem_render::rebuild_gl_renderers_on_cpu` documents for the context
/// hook.
///
/// **Idempotent per program**, which matters because the hook fires once per
/// *surface*, not once per program: `hytte-ui`'s latch is per instance, so
/// four chips of one kind refused in one frame call this four times. The
/// second call finds the program recorded and returns — one sweep and one
/// journal line instead of four of each.
///
/// **What the suite pins of that is the record being a set**, not the early
/// return (PR #1243 review, LOW 2, measured both ways):
/// [`tests::a_refused_pipeline_takes_only_its_own_kinds_arm`] reds if the
/// `contains` check below is dropped so [`REFUSED`] grows an entry per
/// surface, and stays **green** if `if !first { return; }` is deleted
/// outright. That is honest rather than a gap to close: with the program
/// already recorded, a second sweep finds no instance still on that pipeline
/// and changes nothing at all, so the early return's whole effect is one
/// duplicate journal line and one redundant re-map request. The re-map
/// request is unobservable in this suite (#1253; PR #1243 review, LOW 1 /
/// NEW LOW B — pinned instead at the shared callee,
/// `pump::request_preem_repaint_all_when_live`). The journal line **is**
/// observable, and is pinned right here:
/// [`tests::one_journal_line_per_program`] reds (`left: 2, right: 1`) if the
/// same `contains` check is dropped. It is kept because a bar with four chips
/// of one kind would otherwise write four identical lines about one driver
/// refusal.
///
/// A kind registered under **two** names is two refusals by construction and
/// that is correct, not a bug to read into a doubled journal line: the
/// `Marquee` runs the dot matrix's pipeline under its own name (see
/// [`marquee`]), so a driver that will not build that source refuses it once
/// per name, sweeps once per name, and each kind's chips recover on their own
/// refusal.
pub(super) fn on_build_refused(program: GlProgram, grid: (u32, u32), reason: &str) {
    let first = REFUSED.with_borrow_mut(|refused| {
        if refused.contains(&program) {
            return false;
        }
        refused.push(program);
        true
    });
    if !first {
        return;
    }
    // Only the instances actually drawing with this pipeline, unlike the
    // context hook's wholesale sweep: a gauge rebuilt for a scope's refusal
    // would answer with the same `GaugeGl` it already had and restart its
    // needle's spring for nothing.
    let chips = super::preem_render::rebuild_refused_gl_renderers_on_cpu(program);
    tracing::warn!(
        program = program.0,
        grid = format!("{}x{}", grid.0, grid.1),
        reason,
        chips,
        "this driver will not build that preem GL pipeline; its chips fall back to the CPU kit \
         for the rest of this session (restart the shell to offer it GL again)",
    );
    // The re-map is the other half, for the context hook's reason: a chip
    // whose plugin has gone quiet gets no mapping pass of its own — a
    // `persistence: 256` scope answers `animates()` with `false` from birth,
    // #926's clock parks it, and the rebuilt kit renderer would never reach
    // the screen. Guarded and deferred to idle there; both matter here too,
    // since this runs inside a `GtkGLArea` render callback.
    //
    // Whether the *mailbox write* this eventually performs reaches the
    // screen is still unpinned — that would take a live `PluginHandles` in
    // the registry (the guard this function reads) plus a pumped idle turn,
    // i.e. a host fixture no test in this tree stands up (PR #1243 review,
    // LOW 1). But whether the *call* happens at all is pinned since #1253,
    // in the shared callee both this hook and the context hook above call:
    // `pump::request_preem_repaint_all_when_live`'s `#[cfg(test)]` counter,
    // asserted alongside the builds probe in `preem_render`'s
    // `a_refused_pipeline_puts_that_chip_on_the_kit_and_leaves_the_others_on_gl`
    // (PR #1243 review, NEW LOW B) — which is also where the *rebuild* half
    // is pinned, so what is unasserted narrows to the mailbox write alone.
    super::pump::request_preem_repaint_all_when_live();
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
///
/// Loops over [`kind::Kind::ALL`] rather than naming each pipeline by hand
/// (#1211): a kind added to that array without a [`kind::Kind::gl_seam`] arm
/// fails to compile, so this list cannot silently fall one kind behind it.
/// The ticker's pipeline **is** the dot matrix's, under its own name so a
/// journal line says which widget is on screen — see `marquee`'s module docs
/// and [`kind::Kind::gl_seam`]. `hytte-ui` compiles programs per surface, so
/// the second name costs one map entry and no extra compilation.
pub(super) fn install() {
    for kind in kind::Kind::ALL {
        let (program, pipeline) = kind.gl_seam();
        hytte::ui::gl_surface::register(program, pipeline);
    }
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
    // The third failure (#1232), and the only one of the three that is per
    // *pipeline*: a context that came up fine and a driver that will not build
    // one of the programs registered above. See [`on_build_refused`], which is
    // the same two steps this hook takes, narrowed to the chips drawing with
    // that one pipeline.
    hytte::ui::gl_surface::set_build_refusal_handler(on_build_refused);
}

#[cfg(test)]
mod tests {
    use super::{
        Arm, GAUGE, REFUSED, RENDERER_ENV, SCOPE, arm, arm_for, arm_from_env, on_build_refused,
        shader_arm, with_cpu_kill_switch, with_gl_arm,
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

    /// **#1232.** A refused pipeline takes **its own** kind's arm to the kit
    /// and nobody else's.
    ///
    /// The third failure, and the first one that is not session-wide: the two
    /// [`arm`] folds in — the kill switch and the context-failure latch — are
    /// each an answer about the whole process, so before this the shell had
    /// no way to say "this driver builds four of my five pipelines". It said
    /// nothing at all instead, and the fifth kind's chips stayed blank.
    ///
    /// Driven through [`on_build_refused`] rather than by poking the latch,
    /// because the record and the rebuild sweep are one decision: a version
    /// that recorded the program *after* rebuilding would leave every chip on
    /// the pipeline that will not build, and the assertions below would still
    /// pass if this only checked the record.
    ///
    /// **Falsified** by having [`arm_for`] ignore its argument and return
    /// [`arm`]: the second assertion reports `Gl` — which is the shipped
    /// behaviour, i.e. #1232.
    #[test]
    fn a_refused_pipeline_takes_only_its_own_kinds_arm() {
        let _ink = crate::plugins::tests::preem_ink_lock();
        with_gl_arm(|| {
            assert_eq!(arm_for(SCOPE), Arm::Gl, "the premise: the switch says GL");
            assert_eq!(arm_for(GAUGE), Arm::Gl);

            on_build_refused(SCOPE, (48, 24), "fragment shader failed to compile");

            assert_eq!(
                arm_for(SCOPE),
                Arm::Cpu,
                "a pipeline this driver refused is not offered to it again",
            );
            assert_eq!(
                arm_for(GAUGE),
                Arm::Gl,
                "…and every other pipeline keeps the GPU: the refusal is per program, not a \
                 second `gl_abandoned`",
            );
            assert_eq!(
                arm(),
                Arm::Gl,
                "…which is exactly what the session-wide answer still says",
            );

            // The record is a **set**, which is the observable half of the
            // idempotence guard (PR #1243 review, LOW 2). The hook fires once
            // per *surface* — `hytte-ui`'s latch is per instance — so a bar
            // with four scope chips calls this four times in one frame, and a
            // record that appended per call would grow for the life of the
            // session and lengthen every `arm_for` scan with it.
            //
            // Measured: dropping the `contains` check reds this (`left: 2`);
            // deleting `if !first { return; }` does **not** red anything, and
            // `on_build_refused`'s doc says so rather than claiming coverage
            // it has not got.
            on_build_refused(SCOPE, (96, 32), "the same driver, a second chip");
            assert_eq!(
                REFUSED.with_borrow(Vec::len),
                1,
                "a program already known refused is recorded once, not once per surface",
            );
            assert_eq!(
                arm_for(SCOPE),
                Arm::Cpu,
                "…and a second refusal of the same pipeline does not un-refuse it",
            );
        });
    }

    /// **#1253** (PR #1243 review, NEW LOW A). The idempotence guard's
    /// doc used to say the early return's duplicate-journal-line effect is
    /// "unobservable" — that was wrong, and this is the pin that says so.
    ///
    /// [`on_build_refused`] is driven directly, with no chip in `STORE` to
    /// rebuild — the journal line fires whether or not there is anything to
    /// sweep, and a bare double-refusal is the narrowest fixture that
    /// exercises the `contains` check. `hytte_config::test_support::capture`
    /// is the same WARN-counting harness `plugins::tests::warns_with` already
    /// uses (`plugins/tests.rs`), reached here without that helper since this
    /// module has no `warns_with` of its own and one field filter is not
    /// worth importing across a module boundary for.
    ///
    /// **Falsified** by dropping the `contains` check in [`on_build_refused`]
    /// (the same M7 mutation LOW 2's test above pins from the other side, the
    /// record growing to two entries): `REFUSED` no longer short-circuits the
    /// second call, so the hook's `tracing::warn!` fires twice and this reds
    /// at `left: 2, right: 1`.
    #[test]
    fn one_journal_line_per_program() {
        let (captured, _guard) = hytte_config::test_support::capture();

        on_build_refused(SCOPE, (48, 24), "the first chip");
        on_build_refused(SCOPE, (96, 32), "the second chip, same driver");

        let lines = captured
            .events()
            .into_iter()
            .filter(|e| e.level == tracing::Level::WARN && e.fields.contains_key("program"))
            .count();
        assert_eq!(
            lines, 1,
            "one journal line per refused program, not one per refused surface",
        );
    }
}
