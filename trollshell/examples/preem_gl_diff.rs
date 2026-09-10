//! `preem_gl_diff` — the GL/CPU parity harness for #893 stage B.
//!
//! Renders the same `Scope` state through **both** arms and prints the
//! per-channel delta, so the ceiling the spec proposes — mean ≤ 2/255,
//! p99 ≤ 8/255, max ≤ 32/255 — is a measurement rather than a hope.
//!
//! ```sh
//! nix develop --command cargo run -p trollshell --example preem_gl_diff
//! nix develop --command cargo run -p trollshell --example preem_gl_diff -- --skins vfd,crt
//! ```
//!
//! It opens a window and needs a real GL context, which today's `nix flake
//! check` does not have (`xvfb-run` in a sandbox with no `/dev/dri` and **no
//! mesa in the closure**) — which is why the numbers live in
//! `docs/live-verify.md` and not in a test. It does **not** need glass: the
//! #1036 spike proved llvmpipe under `xvfb-run` gives GTK4 a real GLES 3.2
//! context, in a nix sandbox included, and that is how #1072's numbers were
//! taken. `docs/live-verify.md`'s preem entry carries the four environment
//! variables; #1036 is the issue for putting this in CI.
//!
//! # Why it lives in `trollshell` and not in `hytte-ui`
//!
//! The design spec put it in `hytte-ui`. It cannot go there and stay honest:
//! the pipeline, the GLSL and the state → uniform mapping are the *shell's*,
//! and `hytte-ui` does not (and must not) depend on `hytte-preem`, so a harness
//! there would need its own copy of both halves — and a harness that agrees
//! with itself by construction measures nothing. Here it pulls in the shell's
//! own `preem_gl::program` and `preem_gl::parity` with `#[path]`, so what it
//! measures — and the arithmetic it measures with — is the code that ships.
//!
//! It reaches no closure, but **not** because `nix build` skips examples: the
//! workspace derivation sets `doCheck = true` and `cargo test --workspace`
//! builds every example, which is exactly how `nix/package.nix` harvests the
//! `probe`/`wifi_probe` binaries out of `target/release/examples/`. What keeps
//! this one out is that it is not in `postInstall`'s hardcoded copy list and
//! has no `[[example]]` entry asking to be installed.
//!
//! # What it measures, and what it cannot
//!
//! The GL side is read back out of the `GtkGLArea`'s own framebuffer with
//! `glReadPixels` after a frame has been drawn — a full pipeline stall, which
//! is precisely why the *shell* never does it. The CPU side is `kit::Scope`
//! driven through the same batches. Both are then compared at the logical
//! grid's natural size.
//!
//! **Which frame it reads is the hard part**, and getting it wrong is the
//! whole of #1072's original 12/12 — see [`SETTLE_RENDERS`], which is the
//! single most load-bearing constant in this file.
//!
//! # What it writes
//!
//! Three netpbm files per case under `gates/` (`PREEM_GL_DIFF_OUT` moves
//! them): `<case>.gl.ppm`, `<case>.cpu.ppm` and `<case>.delta.pgm`, the last
//! being the per-pixel worst-channel `|Δ|`. Plus, in the transcript, the worst
//! pixel's coordinates and channel and an **edge / field / lit** region split
//! of the deltas — which is what turns a bare `max 141` into a
//! classification. See [`print_regions`].
//!
//! Two differences are expected and are what the ceiling is for:
//!
//! * **`round()` at an exact `.5`.** GLSL ES leaves the direction of a halfway
//!   case implementation-defined. The beam's `row_for` sidesteps it with
//!   `floor(x + 0.5)`, but `sample_at`'s interpolation and the driver's own
//!   `highp` rounding mode (which ES does not specify) can still land a column
//!   one row off.
//! * **The blit's point sampling** when the allocation is not an exact integer
//!   multiple of the grid. The harness sizes the area to the natural size to
//!   avoid it; a mismatch here means the window manager overrode the size
//!   request, and the harness says so.
//!
//! The statistic is **per channel** and the verdict takes the worst of the
//! three — #893's answer 4 states the ceiling per channel, and one lumped
//! R+G+B population divides a single-channel drift by three. It also folds in
//! the per-column peak-row check, a guard against reporting numbers against an
//! **undrawn** framebuffer, and — since #1072, for #1070's review finding M2 —
//! a named failure for a GL arm that drew **nothing at all**, which a dark
//! skin otherwise hides comfortably inside the ceiling. See `preem_gl::parity`
//! for all of them and their tests.
//!
//! Measured under llvmpipe on 2026-09-10 (Mesa 26.2.2, GLES 3.2), every one of
//! the twelve cases came out **bit-exact**: max |Δ| 0 of 255 on R, G and B.
//! Treat any non-zero number from this harness as real.
//!
//! # `TROLLSHELL_PARITY_EXACT` — the 0-pinned assertion (#1080)
//!
//! The ceiling (mean ≤ 2 / p99 ≤ 8 / max ≤ 32) exists for driver portability —
//! it is deliberately loose enough to survive a different GPU's rounding. It
//! does **not** protect the bit-exactness this file measured under llvmpipe
//! (#1078's review, INFO-1): a 1–6/255 regression on every channel still
//! reports `PASS`. With `TROLLSHELL_PARITY_EXACT=1` set, a case that is inside
//! the ceiling but not bit-exact (`max |Δ| > 0` on any channel) fails anyway,
//! named `FAIL(exact)`. This is meant for the sandboxed `system-tests` check
//! (`flake.nix`), where the driver is pinned to Mesa llvmpipe and 0 is the
//! only value that has ever been measured — it is **not** set when running
//! this by hand against real glass, where the ceiling is the real contract.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, glib, prelude::*};
use hytte::ui::gl_surface::GlSurface;
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

// The shell's own pipeline, GLSL and uniform mapping — see the module docs on
// why this is a `#[path]` include and not a second copy.
#[path = "../src/plugins/preem_gl/program.rs"]
mod program;
// The delta statistics and the verdict. Also a `#[path]` include, and for a
// second reason on top of the first: `cargo test` does not run `#[test]`s
// inside an example (examples default to `test = false`), so the arithmetic
// that decides #893's ceiling lives in the shell's tree — `#[cfg(test)]`-
// mounted there — where `cargo test -p trollshell --lib` actually runs it.
#[path = "../src/plugins/preem_gl/parity.rs"]
mod parity;

/// Logical grid the cases run at. Small enough to keep the whole comparison on
/// screen at 1× and wide enough that the graticule's 12-column pitch repeats.
const COLS: u32 = 48;
const ROWS: u32 = 24;
/// Integer upscale, so the natural size is a clean multiple of the grid.
const SCALE: u32 = 2;
/// Phosphor persistence — the kit's default, a ~17-step settle.
const PERSISTENCE: u16 = 184;

/// Whether any case failed, for [`main`]'s exit status.
///
/// A process-global rather than a value threaded out of `activate`, because
/// there is nowhere to thread it *to*: `GApplication::run` returns the
/// application's own status, which nothing here sets, and the `activate`
/// callback returns `()`. `Relaxed` is enough — it is written on the GTK main
/// thread and read after `run` returns on the same thread.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> glib::ExitCode {
    let skins = match parse_skins(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(skins) => skins,
        Err(usage) => {
            println!("{usage}");
            return glib::ExitCode::FAILURE;
        }
    };

    println!("=== preem_gl_diff — #893 stage B parity harness ===");
    println!(
        "grid {COLS}x{ROWS} scale {SCALE} persistence {PERSISTENCE}; \
         ceiling mean {} / p99 {} / max {} per channel",
        parity::CEILING_MEAN,
        parity::CEILING_P99,
        parity::CEILING_MAX,
    );
    let exact = parity_exact();
    if exact {
        println!(
            "TROLLSHELL_PARITY_EXACT=1: any case with a non-zero delta on any \
             channel fails as FAIL(exact), even inside the ceiling above"
        );
    }

    let app = gtk::Application::builder()
        .application_id("mov.vibec0re.trollshell.preem-gl-diff")
        .build();
    app.connect_activate(move |app| activate(app, &skins, exact));
    let status = app.run_with_args::<&str>(&[]);
    // **The verdict is ours, not `GApplication`'s.** `run_with_args` returns the
    // application's exit status, which nothing in this program sets, so a
    // ceiling breach, a missing context, a GL error and a short readback all
    // came back `0` before this. The transcript goes on #893 to settle the
    // ceiling; a harness that always exits clean cannot be scripted, cannot be
    // trusted at a glance, and would let a red run be pasted as a green one.
    if FAILED.load(std::sync::atomic::Ordering::Relaxed) {
        return glib::ExitCode::FAILURE;
    }
    status
}

/// Whether `TROLLSHELL_PARITY_EXACT=1` is set — see the module docs.
///
/// Same convention as `TROLLSHELL_REQUIRE_GL`/`TROLLSHELL_REQUIRE_ICON_THEME`
/// (`crates/hytte-ui/src/gl_surface.rs`): only the exact string `"1"` turns it
/// on, so `=0`/`=true`/unset all leave the ceiling as the sole verdict.
fn parity_exact() -> bool {
    std::env::var_os("TROLLSHELL_PARITY_EXACT").is_some_and(|want| want == "1")
}

/// `--skins vfd,lcd,oled,crt` (default: all four).
fn parse_skins(args: &[String]) -> Result<Vec<kit::DisplayStyle>, String> {
    const USAGE: &str = "\
preem_gl_diff — #893 stage B GL/CPU parity harness

USAGE:
  cargo run -p trollshell --example preem_gl_diff -- [--skins vfd,lcd,oled,crt]";
    let mut skins: Vec<kit::DisplayStyle> = kit::DisplayStyle::ALL.to_vec();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--skins" => {
                let list = rest.next().ok_or_else(|| USAGE.to_owned())?;
                skins = list
                    .split(',')
                    .map(|name| {
                        kit::DisplayStyle::ALL
                            .into_iter()
                            .find(|style| style.name() == name.trim())
                            .ok_or_else(|| format!("unknown skin {name:?}\n\n{USAGE}"))
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--help" | "-h" => return Err(USAGE.to_owned()),
            other => return Err(format!("unexpected argument {other:?}\n\n{USAGE}")),
        }
    }
    Ok(skins)
}

/// One comparison: a skin plus how many animation steps to run before reading.
struct Case {
    style: kit::DisplayStyle,
    /// Extra idle steps after the debut batch, so the phosphor trail — the one
    /// thing the GL arm reimplements as a recurrence — is measured mid-fade
    /// rather than only at full intensity.
    idle_steps: u32,
}

/// How many renders of the **same** state to issue before reading the
/// framebuffer back.
///
/// **This is the fix for the harness's own worst bug** (#1072). `GtkGLArea`
/// does not own one framebuffer: `gtk_gl_area_snapshot` hands the texture it
/// just rendered to GSK and drops its own reference, and the next
/// `attach_buffers` takes a *different* texture out of the area's pool — the
/// one GSK has already released, or a freshly created one. So a readback taken
/// after the frame has been composited does not read the frame that was
/// composited. Measured, on llvmpipe, that is exactly what it did: case 1 read
/// an untouched texture (all zeroes, reported `FAIL(blank)`) and every case
/// after it read the *previous* case's picture — so `lcd.idle0` was scored
/// against a VFD frame and came back with a mean of 128/255. Not one of those
/// twelve numbers was a parity measurement.
///
/// Re-rendering the same state is safe to do and is what makes this work: the
/// pipeline's step passes run `step_seq - last_drawn` times, so a repeat render
/// runs **zero** of them and only re-runs the frame passes — `hytte-ui`'s
/// documented idempotence rule, asserted by
/// `program`'s `the_pipeline_steps_the_accumulator_and_ends_on_the_screen`
/// (no frame pass targets the accumulator). After enough repeats every texture
/// in the pool holds the same picture, and the two-capture equality check below
/// is what proves it did rather than assuming it.
const SETTLE_RENDERS: u32 = 6;

/// One framebuffer readback.
struct Capture {
    /// Bottom-up RGBA8 at [`Self::alloc`].
    raw: Vec<u8>,
    /// The framebuffer's size in device pixels.
    alloc: (u32, u32),
    /// The area's integer device scale.
    scale: u32,
}

/// Where the per-case evidence images go — `gates/` by default, the repo's
/// scratch directory. Overridable with `PREEM_GL_DIFF_OUT`.
fn out_dir() -> std::path::PathBuf {
    std::env::var_os("PREEM_GL_DIFF_OUT").map_or_else(
        || std::path::PathBuf::from("gates"),
        std::path::PathBuf::from,
    )
}

fn activate(app: &gtk::Application, skins: &[kit::DisplayStyle], exact: bool) {
    // The same registration `plugins::install` does in the shell, with the same
    // pipeline constant — the harness drives the shipping pipeline, not a copy.
    hytte::ui::gl_surface::register(program::SCOPE, program::SCOPE_PIPELINE);

    let cases: Vec<Case> = skins
        .iter()
        .flat_map(|style| {
            [0_u32, 1, 5].into_iter().map(move |idle_steps| Case {
                style: *style,
                idle_steps,
            })
        })
        .collect();

    let area = GlSurface::new();
    let natural = (COLS * SCALE, ROWS * SCALE);
    let width = i32::try_from(natural.0).unwrap_or(i32::MAX);
    let height = i32::try_from(natural.1).unwrap_or(i32::MAX);
    area.set_size_request(width, height);
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("preem_gl_diff")
        .default_width(width + 32)
        .default_height(height + 32)
        .child(&area)
        .build();
    window.present();

    let evidence = out_dir();
    if let Err(why) = std::fs::create_dir_all(&evidence) {
        println!(
            "INFO: no evidence images — {} is not writable ({why})",
            evidence.display()
        );
    }
    println!(
        "evidence images -> {}/<case>.{{gl,cpu,delta}}.p[pg]m",
        evidence.display()
    );

    // One case per *several* ticks — see [`Runner::step`].
    let runner = Rc::new(Runner {
        cases,
        evidence,
        exact,
        index: std::cell::Cell::new(0),
        phase: std::cell::Cell::new(0),
        failures: std::cell::Cell::new(0),
        first: RefCell::new(None),
    });

    glib::timeout_add_local(std::time::Duration::from_millis(60), {
        let area = area.clone();
        let app = app.clone();
        move || runner.step(&area, &app)
    });
}

/// The tick machine that walks the cases.
struct Runner {
    cases: Vec<Case>,
    /// Where [`write_evidence`] puts its images.
    evidence: std::path::PathBuf,
    /// `TROLLSHELL_PARITY_EXACT=1` (see [`parity_exact`]) — read once here so
    /// every case's [`measure`] call sees the same value.
    exact: bool,
    /// The case being driven.
    index: std::cell::Cell<usize>,
    /// Which tick within that case — see [`Runner::step`]'s phases.
    phase: std::cell::Cell<u32>,
    failures: std::cell::Cell<u32>,
    /// The first of the two readbacks, held for the stability check.
    first: RefCell<Option<Capture>>,
}

impl Runner {
    /// One timeout tick.
    ///
    /// A case takes `SETTLE_RENDERS + 3` of these: push the state, re-render it
    /// [`SETTLE_RENDERS`] times so every texture in the area's pool holds it,
    /// then read the framebuffer back **twice** with a render between and
    /// require the two to be identical. See [`SETTLE_RENDERS`] for what that is
    /// guarding — it is the whole of #1072.
    fn step(&self, area: &GlSurface, app: &gtk::Application) -> glib::ControlFlow {
        let at = self.index.get();
        let Some(case) = self.cases.get(at) else {
            self.summary();
            app.quit();
            return glib::ControlFlow::Break;
        };
        let label = label(case);

        match self.phase.get() {
            0 => {
                drive(area, case);
                self.phase.set(1);
            }
            p if p <= SETTLE_RENDERS => {
                area.queue_render();
                self.phase.set(p + 1);
            }
            p if p == SETTLE_RENDERS + 1 => match capture(area, &label) {
                Ok(shot) => {
                    *self.first.borrow_mut() = Some(shot);
                    area.queue_render();
                    self.phase.set(p + 1);
                }
                Err(why) => {
                    println!("FAIL(context) {label}: {why}");
                    self.done(at, false);
                }
            },
            _ => {
                let held = self.first.borrow_mut().take();
                let passed = match (held, capture(area, &label)) {
                    (Some(a), Ok(b)) if a.raw == b.raw && a.alloc == b.alloc => {
                        measure(case, &b, &self.evidence, self.exact)
                    }
                    (Some(_), Ok(_)) => {
                        // Two renders of one state disagreed, so whatever the
                        // numbers would say, they are not this state's.
                        println!(
                            "FAIL(unstable) {label}: two readbacks of the same state differ — \
                             the area's texture pool has not settled after {SETTLE_RENDERS} \
                             renders; raise SETTLE_RENDERS"
                        );
                        false
                    }
                    (_, Err(why)) => {
                        println!("FAIL(context) {label}: {why}");
                        false
                    }
                    (None, Ok(_)) => {
                        println!("FAIL(context) {label}: the first readback went missing");
                        false
                    }
                };
                self.done(at, passed);
            }
        }
        glib::ControlFlow::Continue
    }

    /// Record a case's verdict and move to the next one.
    fn done(&self, at: usize, passed: bool) {
        if !passed {
            self.failures.set(self.failures.get() + 1);
        }
        self.index.set(at + 1);
        self.phase.set(0);
        self.first.borrow_mut().take();
    }

    /// The closing verdict, and the process exit status behind it.
    fn summary(&self) {
        println!("-- summary --");
        if self.failures.get() == 0 {
            println!(
                "PASS all {} case(s) inside the proposed ceiling on every channel",
                self.cases.len()
            );
        } else {
            println!(
                "FAIL {} of {} case(s) — see the per-case verdict above",
                self.failures.get(),
                self.cases.len()
            );
            FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        println!("=== preem_gl_diff done — paste this into issue #893 ===");
    }
}

/// The sample batch every case stamps — a wave with steep segments, so the
/// polyline join (the part `GL_LINES` would have got wrong) is exercised.
fn samples() -> Vec<f32> {
    (0..32_u8)
        .map(|i| {
            let t = f32::from(i) / 8.0;
            (t.sin() * 0.9 + (t * 3.0).cos() * 0.3).clamp(-1.0, 1.0)
        })
        .collect()
}

fn config(style: kit::DisplayStyle) -> vocab::ScopeConfig {
    let name = vocab::StyleName::ALL
        .into_iter()
        .find(|candidate| candidate.name() == style.name())
        .unwrap_or_default();
    vocab::ScopeConfig {
        style: vocab::StyleRef::new(name),
        cols: COLS,
        rows: ROWS,
        scale: SCALE,
        persistence: PERSISTENCE,
    }
}

/// One case's name in the transcript and on its evidence files.
fn label(case: &Case) -> String {
    format!("{}.idle{}", case.style.name(), case.idle_steps)
}

/// Push a case's state at the surface and ask for a frame.
fn drive(area: &GlSurface, case: &Case) {
    let batch: std::sync::Arc<[f32]> = std::sync::Arc::from(&samples()[..]);
    // The debut batch is step 0; `idle_steps` more steps carry it into the
    // fade, exactly as `Renderer::ScopeGl::advance` counts them.
    let step_seq = 1 + u64::from(case.idle_steps);
    let surface = program::scope_surface(
        config(case.style),
        &batch,
        Some(0),
        step_seq,
        &kit::palette_snapshot(case.style),
    );
    area.set_state(
        program::SCOPE,
        surface.width,
        surface.height,
        &std::sync::Arc::new(surface.uniforms),
    );
}

/// Read the area's framebuffer back. See [`SETTLE_RENDERS`] for *when* this is
/// allowed to be called and why that matters more than anything it does.
fn capture(area: &GlSurface, label: &str) -> Result<Capture, String> {
    if let Some(error) = area.error() {
        return Err(format!("no GL context — {error}"));
    }
    let Some(context) = area.context() else {
        return Err("the area never realized".to_owned());
    };
    context.make_current();
    let Ok(gl) = hytte_gl::Gl::current() else {
        return Err("the GL entry points could not be resolved".to_owned());
    };
    // Back to GTK's own framebuffer — a `GtkGLArea` does not render into 0 —
    // then read what the last frame left in it.
    area.attach_buffers();
    let scale = u32::try_from(area.scale_factor().max(1)).unwrap_or(1);
    let alloc = (
        u32::try_from(area.width().max(0)).unwrap_or(0) * scale,
        u32::try_from(area.height().max(0)).unwrap_or(0) * scale,
    );
    // Drain anything the pipeline left queued *before* the readback, so a GL
    // error reported below is attributable to the read and not to a draw three
    // passes ago. `take_error` empties the whole queue, which is what makes
    // that attribution true — `glGetError` pops one entry, so a single call
    // would leave a second pending error to surface below as a readback
    // failure. The shell never does any of this (`glGetError` is a
    // synchronisation point on some drivers), but a harness that stalls the
    // pipeline anyway can afford the honesty.
    if let Some(code) = gl.take_error() {
        println!("INFO {label}: a GL error was pending before the readback ({code:#x})");
    }
    let raw = hytte_gl::read_rgba8(&gl, alloc.0, alloc.1);
    if let Some(code) = gl.take_error() {
        return Err(format!(
            "the framebuffer readback raised GL error {code:#x}"
        ));
    }
    let wanted = (alloc.0 as usize) * (alloc.1 as usize) * 4;
    if raw.len() != wanted || wanted == 0 {
        return Err(format!(
            "got {} bytes for a {}x{} framebuffer, wanted {wanted} bytes",
            raw.len(),
            alloc.0,
            alloc.1,
        ));
    }
    Ok(Capture { raw, alloc, scale })
}

/// Build the CPU reference, compare, print the per-channel deltas and the
/// worst pixel, and write the evidence images. Returns whether the case passed
/// — see [`parity::Verdict`] for the five ways it can fail.
fn measure(case: &Case, shot: &Capture, evidence: &std::path::Path, exact: bool) -> bool {
    let label = label(case);

    // The CPU reference: the same batches through the kit.
    let mut oracle = kit::Scope::with_size(COLS as usize, ROWS as usize)
        .scale(SCALE as usize)
        .persistence(PERSISTENCE);
    oracle.advance(&samples());
    for _ in 0..case.idle_steps {
        oracle.advance(&[]);
    }
    let reference = oracle.render(case.style);

    let expected = (COLS * SCALE * shot.scale, ROWS * SCALE * shot.scale);
    if shot.alloc != expected {
        println!(
            "INFO {label}: allocation {}x{} is not the natural size {}x{} — \
             the window manager overrode the size request, so point sampling \
             differs and the numbers below are not a fair comparison",
            shot.alloc.0, shot.alloc.1, expected.0, expected.1
        );
    }

    // `for_capture` rather than a struct literal: the beam tolerance is
    // `parity::peak_row_tolerance`'s to compute, and it is the number that
    // decides `FAIL(beam)` in a transcript pasted on #893. Written out here it
    // drifted — it carried a stray device-scale factor, which made the verdict
    // depend on the monitor the harness ran on.
    let layout = parity::Layout::for_capture(
        shot.alloc,
        (reference.width(), reference.height()),
        shot.scale,
        SCALE as usize,
    );
    let stats = parity::compare(&shot.raw, reference.data(), layout);
    let verdict = stats.verdict();
    println!(
        "{} {label}: worst channel mean {:.3} p99 {:.0} max {:.0} of 255 \
         over {} px; peak-row mismatches {}/{}",
        verdict.label(),
        stats.worst_mean(),
        stats.worst_p99(),
        stats.worst_max(),
        stats.pixels,
        stats.peak_row_mismatches,
        reference.width(),
    );
    for (channel, name) in stats.channels.iter().zip(parity::CHANNELS) {
        println!(
            "      {name}: mean {:.3} p99 {:.0} max {:.0} of 255{}",
            channel.mean,
            channel.p99,
            channel.max,
            if channel.inside_ceiling() {
                ""
            } else {
                "   <-- outside the ceiling"
            },
        );
    }
    if let Some(worst) = stats.worst {
        println!(
            "      worst pixel ({}, {}) on {}: |Δ| {} — gl {:?} vs cpu {:?}",
            worst.x,
            worst.y,
            parity::CHANNELS[worst.channel],
            worst.delta,
            worst.gl,
            worst.cpu,
        );
    }

    let deltas = parity::delta_map(&shot.raw, reference.data(), layout);
    print_regions(&deltas, &reference);
    write_evidence(evidence, &label, shot, layout, &reference, &deltas);

    // #1080's 0-pinned assertion (#1078 review, INFO-1): the ceiling is loose
    // on purpose, for a driver this harness has never measured. Under
    // llvmpipe every case has come out bit-exact, so with
    // `TROLLSHELL_PARITY_EXACT=1` a case that clears the ceiling but is not
    // bit-exact is *still* a failure — named separately from `verdict`'s own
    // labels so a transcript can tell "outside the ceiling" from "inside the
    // ceiling, but not the zero this sandbox is pinned to" at a glance. `max
    // == 0.0` on every channel is equivalent to "every compared pixel had
    // `|Δ| == 0`", since mean/p99 are drawn from that same non-negative
    // distribution and cannot exceed its max.
    let bit_exact = stats.channels.iter().all(|c| c.max == 0.0);
    if exact && verdict.is_pass() && !bit_exact {
        println!(
            "FAIL(exact) {label}: TROLLSHELL_PARITY_EXACT=1 — inside the ceiling \
             but not bit-exact (worst channel mean {:.3} p99 {:.0} max {:.0} of \
             255), and llvmpipe has never measured anything but 0",
            stats.worst_mean(),
            stats.worst_p99(),
            stats.worst_max(),
        );
    }

    verdict.is_pass() && (!exact || bit_exact)
}

/// The classification aid, and the reason a transcript from this harness can be
/// triaged without a driver in front of you (#1072's four buckets).
///
/// Every compared pixel goes in exactly one of three bins, decided by the **CPU
/// reference's own structure** rather than by the palette — so this needs no
/// knowledge of which skin is running and cannot drift from one:
///
/// * **edge** — the reference disagrees with one of its four neighbours. A
///   difference that lives only here is rasterisation edge coverage (bucket c):
///   a boundary landing one pixel over.
/// * **field** — not an edge, and the frame's *modal* colour, i.e. the flat
///   unlit background. A difference that lives here is a tone-curve problem
///   (bucket b): gamma/sRGB moves a flat fill uniformly, and nothing else does.
/// * **lit** — not an edge, not the field: the interior of the trace, the
///   graticule and the bloom. A difference concentrated here, with the field
///   clean, is real shader math (bucket d) — or, where it tracks how *dim* the
///   pixel is, blend/premultiply (bucket a).
fn print_regions(deltas: &[u8], reference: &kit::Frame) {
    let (w, h) = (reference.width(), reference.height());
    let rgb = |x: usize, y: usize| -> [u8; 3] {
        let i = (y * w + x) * 4;
        reference.data()[i..i + 3].try_into().unwrap_or([0, 0, 0])
    };
    let mut counts: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
    for y in 0..h {
        for x in 0..w {
            *counts.entry(rgb(x, y)).or_default() += 1;
        }
    }
    let field = counts
        .into_iter()
        .max_by_key(|(colour, n)| (*n, *colour))
        .map_or([0, 0, 0], |(colour, _)| colour);

    // (count, sum, max) per bin.
    let mut bins = [(0_usize, 0_u64, 0_u8); 3];
    for y in 0..h {
        for x in 0..w {
            let here = rgb(x, y);
            let edge = [(-1_isize, 0_isize), (1, 0), (0, -1), (0, 1)]
                .into_iter()
                .any(|(dx, dy)| {
                    let (nx, ny) = (x.wrapping_add_signed(dx), y.wrapping_add_signed(dy));
                    nx < w && ny < h && rgb(nx, ny) != here
                });
            let bin = if edge {
                0
            } else if here == field {
                1
            } else {
                2
            };
            let delta = deltas[y * w + x];
            bins[bin].0 += 1;
            bins[bin].1 += u64::from(delta);
            bins[bin].2 = bins[bin].2.max(delta);
        }
    }
    let show = |(n, sum, max): (usize, u64, u8)| {
        #[allow(clippy::cast_precision_loss)]
        let mean = if n == 0 { 0.0 } else { sum as f64 / n as f64 };
        format!("n={n} mean {mean:.3} max {max}")
    };
    println!(
        "      regions: edge[{}]  field{field:?}[{}]  lit[{}]",
        show(bins[0]),
        show(bins[1]),
        show(bins[2]),
    );
}

/// Write the three evidence files for one case: what GL drew, what the kit
/// drew, and where they disagree.
///
/// Netpbm rather than PNG, and deliberately: PNG needs a deflate stream, which
/// means a dependency, and this is an example in a workspace that has no image
/// crate. `P6`/`P5` are eight lines of `Vec<u8>` and every viewer and
/// `netpbm`/`ImageMagick` reads them — `pnmtopng gates/crt.idle0.delta.pgm` if a
/// browser is what you have.
fn write_evidence(
    dir: &std::path::Path,
    label: &str,
    shot: &Capture,
    layout: parity::Layout,
    reference: &kit::Frame,
    deltas: &[u8],
) {
    let (w, h) = (reference.width(), reference.height());
    let mut cpu = Vec::with_capacity(w * h * 3);
    for pixel in reference.data().chunks_exact(4) {
        cpu.extend_from_slice(&pixel[..3]);
    }
    let files: [(&str, Vec<u8>); 3] = [
        ("gl.ppm", ppm(w, h, &parity::gl_image(&shot.raw, layout))),
        ("cpu.ppm", ppm(w, h, &cpu)),
        ("delta.pgm", pgm(w, h, deltas)),
    ];
    for (suffix, bytes) in files {
        let path = dir.join(format!("{label}.{suffix}"));
        if let Err(why) = std::fs::write(&path, &bytes) {
            println!("INFO {label}: could not write {} — {why}", path.display());
        }
    }
}

/// A binary `P6` (RGB) netpbm.
fn ppm(w: usize, h: usize, rgb: &[u8]) -> Vec<u8> {
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(rgb);
    out
}

/// A binary `P5` (grayscale) netpbm.
fn pgm(w: usize, h: usize, grey: &[u8]) -> Vec<u8> {
    let mut out = format!("P5\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(grey);
    out
}
