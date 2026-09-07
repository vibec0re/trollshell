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
//! own `preem_gl::program` with `#[path]`, so what it measures is the code that
//! ships. Examples are not built by `nix build` (crane runs `cargo build
//! --workspace`, which skips them) and `postInstall` copies a hardcoded binary
//! list, so this reaches no closure.
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

/// Logical grid the cases run at. Small enough to keep the whole comparison on
/// screen at 1× and wide enough that the graticule's 12-column pitch repeats.
const COLS: u32 = 48;
const ROWS: u32 = 24;
/// Integer upscale, so the natural size is a clean multiple of the grid.
const SCALE: u32 = 2;
/// Phosphor persistence — the kit's default, a ~17-step settle.
const PERSISTENCE: u16 = 184;

/// The spec's proposed ceiling, per channel, in 255ths.
const CEILING_MEAN: f64 = 2.0;
const CEILING_P99: f64 = 8.0;
const CEILING_MAX: f64 = 32.0;

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
         ceiling mean {CEILING_MEAN} / p99 {CEILING_P99} / max {CEILING_MAX} per channel"
    );

    let app = gtk::Application::builder()
        .application_id("mov.vibec0re.trollshell.preem-gl-diff")
        .build();
    app.connect_activate(move |app| activate(app, &skins));
    app.run_with_args::<&str>(&[])
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
                    println!("PASS every case is inside the proposed ceiling");
                } else {
                    println!("FAIL {} case(s) outside the ceiling", failures.get());
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

/// Read the GL frame back, build the CPU reference, and print the deltas.
/// Returns whether the case is inside the ceiling.
fn measure(area: &GlSurface, case: &Case) -> bool {
    let label = format!("{}.idle{}", case.style.name(), case.idle_steps);
    if let Some(error) = area.error() {
        println!("FAIL {label}: no GL context — {error}");
        return false;
    }
    let Some(context) = area.context() else {
        println!("FAIL {label}: the area never realized");
        return false;
    };
    context.make_current();
    let Ok(gl) = hytte_gl::Gl::current() else {
        println!("FAIL {label}: the GL entry points could not be resolved");
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
    let raw = hytte_gl::read_rgba8(&gl, alloc.0, alloc.1);
    if raw.len() != (alloc.0 as usize) * (alloc.1 as usize) * 4 {
        println!("FAIL {label}: readback returned {} bytes", raw.len());
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

    let stats = compare(&raw, alloc, &reference, scale);
    let verdict =
        if stats.mean <= CEILING_MEAN && stats.p99 <= CEILING_P99 && stats.max <= CEILING_MAX {
            "PASS"
        } else {
            "FAIL"
        };
    println!(
        "{verdict} {label}: mean {:.3} p99 {:.0} max {:.0} of 255 over {} px; \
         peak-row mismatches {}/{COLS}",
        stats.mean, stats.p99, stats.max, stats.pixels, stats.peak_row_mismatches
    );
    verdict == "PASS"
}

/// Per-channel absolute-difference statistics plus the structural check.
struct Stats {
    mean: f64,
    p99: f64,
    max: f64,
    pixels: usize,
    peak_row_mismatches: u32,
}

/// Compare the GL readback against the kit's frame.
///
/// `raw` is GL-order (bottom-up) RGBA8 at `alloc` pixels; `reference` is the
/// kit's top-down RGBA8 at the natural size. `device_scale` is the integer
/// framebuffer multiplier, so a capture on a scaled output is point-sampled
/// back down rather than compared against a different-sized reference.
fn compare(raw: &[u8], alloc: (u32, u32), reference: &kit::Frame, device_scale: u32) -> Stats {
    let ref_w = reference.width();
    let ref_h = reference.height();
    let mut deltas: Vec<u8> = Vec::with_capacity(ref_w * ref_h * 3);
    let mut peak_row_mismatches = 0_u32;

    let gl_at = |x: usize, y: usize| -> [u8; 3] {
        // Bottom-up, and point-sampled through the device scale.
        let sx = x * device_scale as usize;
        let sy = y * device_scale as usize;
        let flipped = (alloc.1 as usize).saturating_sub(1).saturating_sub(sy);
        let i = (flipped * alloc.0 as usize + sx) * 4;
        raw.get(i..i + 3)
            .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
    };
    let ref_at = |x: usize, y: usize| -> [u8; 3] {
        let i = (y * ref_w + x) * 4;
        reference
            .data()
            .get(i..i + 3)
            .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
    };
    let luma = |px: [u8; 3]| u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2]);

    for x in 0..ref_w {
        let (mut gl_peak, mut gl_peak_row) = (0, 0);
        let (mut cpu_peak, mut cpu_peak_row) = (0, 0);
        for y in 0..ref_h {
            let (g, c) = (gl_at(x, y), ref_at(x, y));
            for channel in 0..3 {
                deltas.push(g[channel].abs_diff(c[channel]));
            }
            if luma(g) > gl_peak {
                gl_peak = luma(g);
                gl_peak_row = y;
            }
            if luma(c) > cpu_peak {
                cpu_peak = luma(c);
                cpu_peak_row = y;
            }
        }
        // One logical row of tolerance: the beam's own glow is 5 rows tall, so
        // a peak that moved further than a row is a real structural difference
        // and not a rounding one.
        if gl_peak_row.abs_diff(cpu_peak_row) > device_scale as usize * SCALE as usize {
            peak_row_mismatches += 1;
        }
    }

    let pixels = deltas.len();
    let sum: u64 = deltas.iter().map(|d| u64::from(*d)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = if pixels == 0 {
        0.0
    } else {
        sum as f64 / pixels as f64
    };
    deltas.sort_unstable();
    #[allow(clippy::cast_precision_loss)]
    let p99 = deltas
        .get(pixels.saturating_mul(99) / 100)
        .or_else(|| deltas.last())
        .map_or(0.0, |d| f64::from(*d));
    let max = deltas.last().map_or(0.0, |d| f64::from(*d));
    Stats {
        mean,
        p99,
        max,
        pixels,
        peak_row_mismatches,
    }
}
