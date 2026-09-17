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
//! shell runs. Everything that needs the *shell* — the two fallback latches —
//! is here, and it is deliberately kind-agnostic. #1143
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
//! # One arm, and what a chip does when it cannot draw (#1157)
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
//! From there the arms landed kind by kind — #1143 the gauge, #1144 the dot
//! matrix, #1152 the two text kinds on Annika's word for the rest of #865,
//! #1153 the meter, #1154 the readout, #1155 the board, #1156 the shell's own
//! LED panel — until every kind had one, and **#1157 then retired the CPU
//! renderer and the `TROLLSHELL_PREEM_RENDERER=cpu` kill switch with it**
//! (Annika on #865: *"CPU renderer gone soon? ❤️"*).
//!
//! So [`Arm`] no longer answers *which renderer a widget takes*, because there
//! is only one: it answers **whether this pipeline can draw at all**, and the
//! other answer is the broken-widget placeholder — the degradation
//! `Node::Shader` has taken by design since #893, now the whole preem seam's.
//! `RUST_LOG=hytte_gl=debug` names which loader route resolved the entry
//! points.
//!
//! Two things take a kit widget off the GPU, and only these:
//!
//! 1. **a GL context could not be created**, which `hytte-ui` latches
//!    (`gl_abandoned`) and reports through the hook installed in [`install`];
//! 2. **this driver refused this pipeline** (#1232) — a compile or link
//!    failure for one program, which says nothing about the seven others.
//!
//! Both are sticky for the session, both are folded in by [`arm_for`], and
//! both end in the same picture: an empty surface where the chip was, with one
//! journal line naming the cause. That is what "gone" costs, and it is the
//! trade #1157 states out loud — `hytte-preem` is still in the tree (it is the
//! parity oracle the harness measures against, and the rasteriser every
//! plugin's own `Frame::into_node` still runs in *its* process), but the shell
//! no longer carries a second renderer to keep a chip alive on a box whose GL
//! is broken.
//!
//! # What this module does not do
//!
//! It does not touch `pump.rs`. That was true when `Renderer::Scope` had to
//! carry the CPU arm's `pending`/`idle`/`fades`/`settle_steps` fields verbatim
//! and answer `animates()` with the same expression so #926's frame-clock park
//! behaved identically across a kill-switch flip; with the switch gone the
//! property is simply inherited, because those fields and that expression are
//! the ones that stayed. The animation half of the host has never had a line
//! changed by this seam.

use hytte::ui::gl_surface::GlProgram;

mod dot_matrix;
mod flip_board;
mod gauge;
mod kind;
mod led_matrix;
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
// The one kind that is not on the wire (#1156): the Stats drawer's per-core
// panel is a widget the *shell* rasterises, so its mapping is reached from
// `panels::stats` rather than from `preem_render`, and this re-export is
// `pub(crate)` where the others are `pub(super)`. See `led_matrix`'s module
// docs, and `kind::Kind::on_the_wire`.
pub(crate) use led_matrix::{LED_MATRIX, led_matrix_surface};
pub(super) use led_strip::{LED_STRIP, led_strip_surface};
pub(super) use marquee::{MARQUEE, Window, marquee_surface, window as encode_window};
// `KitSurface` is `pub(crate)` for #1156: `panels::stats` builds one directly
// for the LED panel, which no plugin node reaches.
pub(crate) use program::KitSurface;
pub(super) use program::{SCOPE, scope_surface};
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

/// Whether a kit widget's pipeline can draw at all.
///
/// **Per pipeline, not per widget** — one answer for every chip naming that
/// program. `preem_render::build` asks [`arm_for`] once, up front, about the
/// pipeline its widget's kind names (`preem_render::program_for`): since #1155
/// that is every wire kind there is — the `Scope`, the `Gauge`, the
/// `DotMatrix`, the `Marquee`, the `TextBox`, the `LedStrip`, the `SevenSeg`
/// and the `FlipBoard` — plus the shell's own `LedMatrix` (#1156), which is not
/// on the wire and asks from `panels::stats` instead.
///
/// It used to answer "which of the two renderers" and its second variant was
/// named `Cpu`. #1157 retired that renderer, so the question narrowed and the
/// variant is named for what now happens instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arm {
    /// A `GtkGLArea` running one of the pipelines [`install`] registers — the
    /// only way a kit widget reaches the screen since #1157.
    Gl,
    /// No GL for this pipeline: the context failed, or this driver refused
    /// this program. There is nothing to fall back *to* since #1157, so the
    /// widget draws the broken-widget placeholder — an empty surface keeping
    /// its id and classes, the same degradation `Node::Shader` takes.
    Placeholder,
}

/// Record `program` as refused by this driver, through the **production**
/// hook — the seam a consumer outside `plugins` needs to exercise the latch
/// [`arm_for`] folds in (#1156 review, MEDIUM-3).
///
/// `panels::stats` is the first such consumer: the Stats drawer's LED panel is
/// a kit widget the shell draws itself, so its fallback test lives beside it
/// rather than in `plugins::tests`, where [`on_build_refused`]'s `pub(super)`
/// would already be in scope. This is a one-line forward to that function
/// rather than a stand-in for it, so a test still drives the real wire.
///
/// **Sticky and thread-local, like everything [`arm_for`] reads** — see
/// [`REFUSED`]. A plain `#[test]` gets its own thread and so its own blast
/// radius; a `#[gtk::test]` does **not** — `gtk4-macros` marshals every one of
/// them onto a single shared GTK main thread ("creates a main thread for GTK
/// and runs all tests on that thread"), so refusing a pipeline there would
/// poison every other `#[gtk::test]` in the binary that expects the GL arm, in
/// an order libtest does not fix. Callers must be plain `#[test]`s.
#[cfg(test)]
pub(crate) fn refuse_for_test(program: GlProgram, reason: &str) {
    on_build_refused(program, (0, 0), reason);
}

thread_local! {
    /// A scoped override of [`arm`]'s answer, for the tests that must see
    /// **both** of them — see [`with_no_gl`].
    #[cfg(test)]
    static TEST_ARM: std::cell::Cell<Option<Arm>> = const { std::cell::Cell::new(None) };
}

/// Run `body` as if this session had no GL at all — the seam a test uses to
/// watch a widget take the placeholder and then take the GPU back.
///
/// #1157 retired the kill switch, and with it the only **non-sticky** way to
/// ask what the shell does without GL: `hytte-ui`'s `abandon_gl` latches for
/// the *process* and [`refuse_for_test`] latches for the *thread*, so neither
/// can run inside a `#[gtk::test]` without poisoning every sibling that shares
/// `gtk4-macros`' one main thread — which is exactly the constraint #1156 split
/// the Stats panel's two fallback tests along.
///
/// It is a **test seam, not the knob that was deleted**: scoped, restoring, and
/// with no `cfg(not(test))` counterpart, so nothing a shipped build runs can
/// reach it. The two sticky latches stay the right instrument for asserting
/// that a *failure* is sticky —
/// `an_abandoned_context_takes_every_kit_widget_to_the_placeholder` below
/// deliberately does not use this.
#[cfg(test)]
pub(crate) fn with_no_gl<T>(body: impl FnOnce() -> T) -> T {
    let previous = TEST_ARM.replace(Some(Arm::Placeholder));
    let out = body();
    TEST_ARM.set(previous);
    out
}

/// The **session-wide** half of the answer: has GL itself gone?
///
/// [`hytte::ui::gl_surface::gl_abandoned`] is the whole of it since #1157 took
/// the kill switch away — a context that failed once has failed for the
/// process, and a renderer built onto a pipeline that can never draw would
/// leave a blank chip with nothing saying why. Building the placeholder
/// instead is what puts a journal line and an empty surface on the record.
///
/// Only for a caller with no pipeline in hand: [`arm_for`] is what
/// `preem_render::build` asks, because a driver that refuses one pipeline
/// (#1232) may build every other one.
pub(super) fn arm() -> Arm {
    #[cfg(test)]
    if let Some(forced) = TEST_ARM.get() {
        return forced;
    }
    if hytte::ui::gl_surface::gl_abandoned() {
        return Arm::Placeholder;
    }
    Arm::Gl
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

/// Whether the kit widget drawn by `program` can draw: [`arm`], plus the one
/// thing that is per pipeline rather than per session (#1232).
///
/// A `GtkGLArea` can come up with a perfectly good context and still be
/// refused *one* pipeline — a compile or link failure, which `hytte-ui`
/// latches per surface (#1180 item 2) so the driver is asked exactly once.
/// Before this, nothing in the shell heard about that: the chip drew nothing
/// at all for the life of the context while `arm()` kept answering `Gl`,
/// because neither of the failures it folds in had happened.
///
/// Narrower than a second `gl_abandoned`, deliberately: a refused
/// `preem.scope` says nothing about `preem.gauge`, and a session that loses
/// one pipeline keeps the GPU for every other kind on the bar. That narrowness
/// is worth *more* since #1157, not less — the kind that loses its pipeline
/// now loses its picture, so condemning its neighbours with it would empty a
/// bar over one driver's opinion of one shader.
pub(crate) fn arm_for(program: GlProgram) -> Arm {
    if REFUSED.with_borrow(|refused| refused.contains(&program)) {
        return Arm::Placeholder;
    }
    arm()
}

/// The host's half of `hytte-ui`'s build-refusal hook (#1232): record the
/// pipeline as refused, take the chips that were drawing with it to the
/// placeholder, say so once, and ask for one re-map.
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
/// `preem_render::rebuild_gl_renderers_as_placeholders` documents for the
/// context hook.
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
    // would answer with the same `Gauge` it already had and restart its
    // needle's spring for nothing.
    let chips = super::preem_render::rebuild_refused_gl_renderers_as_placeholders(program);
    tracing::warn!(
        program = program.0,
        grid = format!("{}x{}", grid.0, grid.1),
        reason,
        chips,
        "this driver will not build that preem GL pipeline; since #1157 there is no CPU kit to \
         fall back to, so its chips draw nothing for the rest of this session (restart the shell \
         to offer it GL again)",
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
    // `a_refused_pipeline_puts_that_chip_on_the_placeholder_and_leaves_the_others_on_gl`
    // (PR #1243 review, NEW LOW B) — which is also where the *rebuild* half
    // is pinned, so what is unasserted narrows to the mailbox write alone.
    super::pump::request_preem_repaint_all_when_live();
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
        // 1. Rebuild every GL instance right now, the way
        //    `invalidate_cached_frames` rebuilds a `TextBox`. Since #1157 a
        //    rebuild with the latch set yields no renderer at all, which is
        //    how the chip becomes the placeholder. Also drops the caches, so
        //    the `Cached::Gl` uniforms an instance was last mapped with cannot
        //    keep a blank `GlSurface` on screen.
        super::preem_render::rebuild_gl_renderers_as_placeholders();
        super::preem_render::invalidate_cached_frames();
        // 2. Ask for one full re-map, so the reconciler swaps the on-screen
        //    `GlSurface` node for the empty `Pixels` one the rebuilt instance
        //    now produces. Guarded (a context can fail with no live host) and
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
    use super::{Arm, GAUGE, REFUSED, SCOPE, arm, arm_for, on_build_refused, with_no_gl};

    /// **A failed GL context takes every kit widget off the GPU, and keeps
    /// doing it** — since #1157 the one branch that stands between a session
    /// with broken GL and a bar of blank chips nothing explains.
    ///
    /// It is not new code, but what rests on it has grown twice. While the
    /// shipped default was CPU (#1072), a regression here was **masked**: the
    /// configured arm answered `Cpu` anyway, so a session whose context failed
    /// still got the kit by the other route. #1072's restoration of the GL
    /// default made this the only thing left, and #1157's retirement of the
    /// kill switch made it the only thing in [`arm`] at all.
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
    fn an_abandoned_context_takes_every_kit_widget_to_the_placeholder() {
        assert!(
            !hytte::ui::gl_surface::gl_abandoned(),
            "the premise: a fresh test thread has not abandoned GL",
        );
        assert_eq!(arm(), Arm::Gl, "the premise: GL is the only arm there is");

        hytte::ui::gl_surface::abandon_gl("no GL in this test");

        assert_eq!(
            arm(),
            Arm::Placeholder,
            "a context that failed leaves the kit widgets nothing to draw with",
        );
        // Sticky, and that is the property a *session* depends on: the
        // verdict has to hold for every later mount, not just the one that
        // observed the failure.
        assert_eq!(
            arm(),
            Arm::Placeholder,
            "…and keeps holding on the next mount",
        );
    }

    /// [`with_no_gl`] overrides the session-wide answer and **puts it back**.
    ///
    /// The seam's own pin, and the successor of the switch-seam test #1157
    /// deleted (`the_shader_arm_defaults_to_gl_and_the_seam_turns_it_off`).
    /// Both halves matter: a seam that did not restore would leave every later
    /// test on this thread believing the session has no GL, and `panels::stats`'
    /// `the_panel_follows_the_renderer_arm` — the one consumer — depends on
    /// going back, since it asserts the surface swaps *both* ways.
    ///
    /// **Falsified** by dropping the `TEST_ARM.set(previous)` line (the third
    /// assertion), or by having [`arm`] ignore the override (the second).
    #[test]
    fn the_no_gl_seam_overrides_the_session_answer_and_restores_it() {
        assert_eq!(arm(), Arm::Gl, "the premise: this thread has GL");
        with_no_gl(|| {
            assert_eq!(arm(), Arm::Placeholder, "the seam takes GL away");
            assert_eq!(
                arm_for(SCOPE),
                Arm::Placeholder,
                "…for every pipeline, since it stands in for the session-wide latch",
            );
        });
        assert_eq!(arm(), Arm::Gl, "…and puts it back");
    }

    /// **#1232.** A refused pipeline takes **its own** kind to the placeholder
    /// and nobody else's.
    ///
    /// The third failure, and the only one that is not session-wide: what
    /// [`arm`] folds in is an answer about the whole process, so before this
    /// the shell had no way to say "this driver builds seven of my eight
    /// pipelines". It said nothing at all instead, and the eighth kind's chips
    /// stayed blank.
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
        assert_eq!(arm_for(SCOPE), Arm::Gl, "the premise: GL is available");
        assert_eq!(arm_for(GAUGE), Arm::Gl);

        on_build_refused(SCOPE, (48, 24), "fragment shader failed to compile");

        assert_eq!(
            arm_for(SCOPE),
            Arm::Placeholder,
            "a pipeline this driver refused is not offered to it again",
        );
        assert_eq!(
            arm_for(GAUGE),
            Arm::Gl,
            "…and every other pipeline keeps the GPU: the refusal is per program, not a second \
             `gl_abandoned`",
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
            Arm::Placeholder,
            "…and a second refusal of the same pipeline does not un-refuse it",
        );
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
