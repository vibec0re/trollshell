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
//! **Glass only.** It opens a window and needs a real GL context; CI has
//! neither (`nix flake check` runs `xvfb-run` in a sandbox with no `/dev/dri`
//! and no mesa in the closure), which is exactly why the numbers live in
//! `docs/live-verify.md` and not in a test.
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
//! the per-column peak-row check and a guard against reporting numbers against
//! an **undrawn** framebuffer; see `preem_gl::parity` for all three and their
//! tests.

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

    let app = gtk::Application::builder()
        .application_id("mov.vibec0re.trollshell.preem-gl-diff")
        .build();
    app.connect_activate(move |app| activate(app, &skins));
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

fn activate(app: &gtk::Application, skins: &[kit::DisplayStyle]) {
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

    // One case per timeout tick: `set_state` + `queue_render` on the way in,
    // then read the previous case's framebuffer back on the next tick — GTK
    // needs a frame between the two, and a timeout is the least clever way to
    // give it one.
    let pending: Rc<RefCell<Option<usize>>> = Rc::new(RefCell::new(None));
    let index = Rc::new(std::cell::Cell::new(0_usize));
    let failures = Rc::new(std::cell::Cell::new(0_u32));
    let cases = Rc::new(cases);

    glib::timeout_add_local(std::time::Duration::from_millis(120), {
        let area = area.clone();
        let app = app.clone();
        move || {
            if let Some(previous) = pending.borrow_mut().take()
                && !measure(&area, &cases[previous])
            {
                failures.set(failures.get() + 1);
            }
            let next = index.get();
            if next >= cases.len() {
                println!("-- summary --");
                if failures.get() == 0 {
                    println!(
                        "PASS all {} case(s) inside the proposed ceiling on every channel",
                        cases.len()
                    );
                } else {
                    println!(
                        "FAIL {} of {} case(s) — see the per-case verdict above",
                        failures.get(),
                        cases.len()
                    );
                    FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                println!("=== preem_gl_diff done — paste this into issue #893 ===");
                app.quit();
                return glib::ControlFlow::Break;
            }
            index.set(next + 1);
            drive(&area, &cases[next]);
            *pending.borrow_mut() = Some(next);
            glib::ControlFlow::Continue
        }
    });
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

/// Read the GL frame back, build the CPU reference, and print the per-channel
/// deltas. Returns whether the case passed — see [`parity::Verdict`] for the
/// four ways it can fail.
fn measure(area: &GlSurface, case: &Case) -> bool {
    let label = format!("{}.idle{}", case.style.name(), case.idle_steps);
    if let Some(error) = area.error() {
        println!("FAIL(context) {label}: no GL context — {error}");
        return false;
    }
    let Some(context) = area.context() else {
        println!("FAIL(context) {label}: the area never realized");
        return false;
    };
    context.make_current();
    let Ok(gl) = hytte_gl::Gl::current() else {
        println!("FAIL(context) {label}: the GL entry points could not be resolved");
        return false;
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
    // passes ago. The shell never does this — `glGetError` is a
    // synchronisation point on some drivers — but a harness that stalls the
    // pipeline anyway can afford the honesty.
    if let Some(code) = gl.take_error() {
        println!("INFO {label}: a GL error was pending before the readback ({code:#x})");
    }
    let raw = hytte_gl::read_rgba8(&gl, alloc.0, alloc.1);
    if let Some(code) = gl.take_error() {
        println!("FAIL(gl) {label}: the framebuffer readback raised GL error {code:#x}");
        return false;
    }
    let wanted = (alloc.0 as usize) * (alloc.1 as usize) * 4;
    if raw.len() != wanted || wanted == 0 {
        println!(
            "FAIL(readback) {label}: got {} bytes for a {}x{} framebuffer, wanted {wanted} bytes",
            raw.len(),
            alloc.0,
            alloc.1,
        );
        return false;
    }

    // The CPU reference: the same batches through the kit.
    let mut oracle = kit::Scope::with_size(COLS as usize, ROWS as usize)
        .scale(SCALE as usize)
        .persistence(PERSISTENCE);
    oracle.advance(&samples());
    for _ in 0..case.idle_steps {
        oracle.advance(&[]);
    }
    let reference = oracle.render(case.style);

    let expected = (COLS * SCALE * scale, ROWS * SCALE * scale);
    if alloc != expected {
        println!(
            "INFO {label}: allocation {}x{} is not the natural size {}x{} — \
             the window manager overrode the size request, so point sampling \
             differs and the numbers below are not a fair comparison",
            alloc.0, alloc.1, expected.0, expected.1
        );
    }

    let stats = parity::compare(
        &raw,
        reference.data(),
        parity::Layout {
            alloc,
            reference: (reference.width(), reference.height()),
            device_scale: scale,
            // The kit's beam is `2 * GLOW_SPAN + 1` grid rows tall, so a peak
            // that moved less than that is rounding rather than a structural
            // difference. Expressed in *reference* pixels, hence the upscale.
            peak_row_tolerance: (scale as usize) * (SCALE as usize),
        },
    );
    let verdict = stats.verdict();
    println!(
        "{} {label}: worst channel mean {:.3} p99 {:.0} max {:.0} of 255 \
         over {} px; peak-row mismatches {}/{COLS}",
        verdict.label(),
        stats.worst_mean(),
        stats.worst_p99(),
        stats.worst_max(),
        stats.pixels,
        stats.peak_row_mismatches,
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
    verdict.is_pass()
}
