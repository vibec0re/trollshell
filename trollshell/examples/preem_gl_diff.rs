//! `preem_gl_diff` — the GL/CPU parity harness for #893 stage B.
//!
//! Renders the same kit-widget state through **both** arms and prints the
//! per-channel delta, so the ceiling the spec proposes — mean ≤ 2/255,
//! p99 ≤ 8/255, max ≤ 32/255 — is a measurement rather than a hope.
//!
//! Three kinds since #1144: the `Scope` (four skins × three fade depths), the
//! `Gauge` (four skins × three needle positions, plus one at the **shipping**
//! upscale) and the `DotMatrix` (four skins × four displays). The
//! three-per-skin gauge cases run at `scale = 1`, where the GL arm's native
//! grid and the kit's logical one are the same number and the two can be
//! compared pixel against pixel — that is where `TROLLSHELL_PARITY_EXACT=1`
//! pins them at zero.
//!
//! The fourth gauge case per skin runs at `scale = 2`, which is
//! `GaugeConfig::default()` and therefore every dial on the glass (#1148
//! review, HIGH-2). It cannot be compared naively — the GL arm is rasterising
//! at twice the resolution *on purpose*, which is the whole of #1090's fix — so
//! the harness box-averages the native readback back down to the kit's logical
//! grid (`parity::box_downsample`) and then holds it to the split that
//! difference is supposed to have: **every pixel off a rasterisation edge is
//! bit-identical to the kit's**, and the edge region has its own budget.
//! Measured on llvmpipe, all four land exactly there — field and lit interiors
//! at `max |Δ| 0`, an edge mean of 6.1 to 9.4 — which is a sharper statement
//! than #893's ceiling could make about them, and one #893's ceiling itself
//! would fail (a dial is nearly a third edge pixels). A dropped half-pixel
//! offset, an unscaled length, a doubled mask pitch or a mis-scaled bloom is
//! what breaks it. See `preem_gl::gauge` and `preem_gl::parity`'s
//! `Kind`/`Sampling`/`case_verdict`.
//!
//! The dot matrix has no `scale` at all — the dot pitch is its size knob
//! (#1091), so at the natural size both arms fill the same buffer and the
//! question does not arise. What its four cases vary instead is the *line* and
//! the *pitch*, which is where its own arithmetic lives; see `DisplayAt`.
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
//! reports `PASS`. With `TROLLSHELL_PARITY_EXACT=1` set, a **1:1** case that is
//! inside the ceiling but not bit-exact (`max |Δ| > 0` on any channel) fails
//! anyway, named `FAIL(exact)`. This is meant for the sandboxed `system-tests`
//! check (`flake.nix`), where the driver is pinned to Mesa llvmpipe and 0 is
//! the only value that has ever been measured — it is **not** set when running
//! this by hand against real glass, where the ceiling is the real contract.
//!
//! Since #1148's review it pins **both kinds**, not the scope alone. The
//! supersampled cases are unaffected either way: their verdict is the region
//! split above, which is already exact where exactness is meaningful and does
//! not depend on this variable at all.

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
// The gauge's pipeline and mapping (#1143), included the same way and for the
// same reason. It is a sibling of `program` in the shell too, and reaches it
// through `super::program::…`, which resolves here as well because both land
// one module down from a crate root.
#[path = "../src/plugins/preem_gl/gauge.rs"]
mod gauge;
// The dot matrix's pipeline and mapping (#1144), included the same way and for
// the same reason.
#[path = "../src/plugins/preem_gl/dot_matrix.rs"]
mod dot_matrix;

/// Logical grid the **scope** cases run at. Small enough to keep the whole
/// comparison on screen at 1× and wide enough that the graticule's 12-column
/// pitch repeats.
const SCOPE_COLS: u32 = 48;
const SCOPE_ROWS: u32 = 24;
/// Integer upscale, so the natural size is a clean multiple of the grid.
const SCOPE_SCALE: u32 = 2;
/// Phosphor persistence — the kit's default, a ~17-step settle.
const PERSISTENCE: u16 = 184;

/// Logical grid the **gauge** cases run at — the kit's own default face, the
/// one #931 tuned and #1090 was reported against.
const GAUGE_COLS: u32 = 144;
const GAUGE_ROWS: u32 = 64;
/// The upscale the **1:1** gauge cases run at.
///
/// The GL gauge's offscreen grid is the *native* buffer (`cols * scale`), not
/// the logical one, because drawing the dial at the size it is shown at is the
/// point of the arm — see `preem_gl::gauge`. At `scale = 1` the two arms draw
/// the same picture at the same resolution, which is the one place a *pixel
/// against pixel* number means something, and it is where
/// `TROLLSHELL_PARITY_EXACT=1` pins them both at zero.
const GAUGE_SCALE: u32 = 1;

/// The upscale the **supersampled** gauge cases run at — `GaugeConfig`'s own
/// default, which is what every dial on the glass actually uses (#1148 review,
/// HIGH-2).
///
/// Without these, nothing in CI ever rendered the gauge in the configuration it
/// ships in: `Dial::scaled`'s half-pixel offset, the `u_upscale` on every
/// length, the bloom radius's `* scale` and the CRT mask's logical-pitch
/// division are all the identity at `scale = 1`, so four separate scale-only
/// decisions went unrendered by every gate.
///
/// Comparing them naively would indeed measure the improvement rather than a
/// regression — a sharper edge is *supposed* to differ from a smeared one — so
/// the harness does not compare them naively. It renders GL at the native grid,
/// box-averages each `GAUGE_SUPERSAMPLE`² block back down to the kit's logical
/// frame (`parity::box_downsample`) and holds the result to the supersampled
/// standard in `parity::case_verdict`: every pixel off a rasterisation edge
/// bit-identical (`interior_max() == 0`) and the edge bin inside a measured
/// budget — not #893's ceiling, which a dial's roughly one-quarter edge pixels
/// would fail by construction. That is what a dropped `(scale - 1) / 2`, an
/// unscaled length, a doubled mask pitch or a mis-scaled bloom breaks: they
/// move the field, not only the edges.
const GAUGE_SUPERSAMPLE: u32 = 2;

/// The dot pitch the **dot matrix** cases run at — the kit's `DEFAULT_DOT_PX`,
/// which is what every caller written before #1091 renders at and what the
/// kit's own golden digests were recorded against.
///
/// There is no `scale` on this widget: the pitch *is* the size knob, so the
/// kit's buffer is already the one both arms fill and the gauge's "which
/// resolution are we comparing at" question does not arise. What varies instead
/// is the pitch, and [`DisplayAt::Dense`] is the other end of it.
const DOT_PX: u32 = 4;
/// `MIN_DOT_PX` — see [`DisplayAt::Dense`].
const DENSE_DOT_PX: u32 = 2;

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
        "scope {SCOPE_COLS}x{SCOPE_ROWS} scale {SCOPE_SCALE} persistence {PERSISTENCE}; \
         gauge {GAUGE_COLS}x{GAUGE_ROWS} scale {GAUGE_SCALE} and {GAUGE_SUPERSAMPLE} \
         (box-averaged down); \
         dot matrix pitch {DOT_PX} (and {DENSE_DOT_PX}); \
         ceiling mean {} / p99 {} / max {} per channel",
        parity::CEILING_MEAN,
        parity::CEILING_P99,
        parity::CEILING_MAX,
    );
    let exact = parity_exact();
    if exact {
        println!(
            "TROLLSHELL_PARITY_EXACT=1: a **1:1** case of either kind with a \
             non-zero delta on any channel fails as FAIL(exact), even inside \
             the ceiling above."
        );
    }
    println!(
        "the .x{GAUGE_SUPERSAMPLE} gauge cases are box-averaged down from the shipping \
         upscale and take neither the ceiling nor that pin: every pixel off a \
         rasterisation edge must be bit-identical (FAIL(interior)) and the edge \
         region has its own budget, mean {SUPERSAMPLED_EDGE_MEAN} / max {SUPERSAMPLED_EDGE_MAX} \
         (FAIL(edges)). See `preem_gl::parity`'s `case_verdict`.",
        SUPERSAMPLED_EDGE_MEAN = parity::SUPERSAMPLED_EDGE_MEAN,
        SUPERSAMPLED_EDGE_MAX = parity::SUPERSAMPLED_EDGE_MAX,
    );

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

/// One comparison: a kit widget, a skin, and the state to drive it into.
enum Case {
    /// A `Scope` after its debut batch plus `idle_steps` idle ones.
    Scope {
        style: kit::DisplayStyle,
        /// Extra idle steps after the debut batch, so the phosphor trail — the
        /// one thing the GL arm reimplements as a recurrence — is measured
        /// mid-fade rather than only at full intensity.
        idle_steps: u32,
    },
    /// A `Gauge` with its needle driven into one of three positions (#1143).
    Gauge {
        style: kit::DisplayStyle,
        needle: NeedleAt,
        /// The integer upscale the GL arm renders at. [`GAUGE_SCALE`] compares
        /// pixel against pixel; [`GAUGE_SUPERSAMPLE`] compares a box-averaged
        /// native frame against the kit's logical one.
        scale: u32,
    },
    /// A `DotMatrix` showing one line at one pitch (#1144).
    DotMatrix {
        style: kit::DisplayStyle,
        display: DisplayAt,
    },
}

/// What a dot-matrix case puts on the display.
///
/// Four, chosen to cover what the shader has to get right: the degenerate
/// buffer, the ordinary readout, the font's fallback path, and the pitch at
/// which the CRT comb has to be **re-phased** or it stops being a raster
/// (#1091). Each is the same lattice arithmetic at a different corner of it.
#[derive(Clone, Copy)]
enum DisplayAt {
    /// The empty string: bezel only, no strip, no lit pixel — `2*pad` × `9*dot`.
    /// The one case where `u_data_len` is `0` and the shader must draw the
    /// field rather than sample an unbound texture.
    Blank,
    /// An ordinary readout at the default pitch: the ghost lattice, lit glyphs,
    /// the skin's halo, and the comb where the kit's own golden digests have it.
    Readout,
    /// Accented glyphs, a space and an uncovered char — so the hollow `NOTDEF`
    /// box reaches the strip encoder and the shader end to end.
    Notdef,
    /// The same readout at `MIN_DOT_PX`, where every pixel of a dot sits on the
    /// falloff plateau (a solid block, no rim) **and** the CRT comb is re-phased
    /// onto a 2-row grid. A fixed 4-row comb here is interference, not a raster.
    Dense,
}

impl DisplayAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::Readout => "readout",
            Self::Notdef => "notdef",
            Self::Dense => "dense",
        }
    }

    /// `(line, dot pitch)`.
    fn line(self) -> (&'static str, u32) {
        match self {
            Self::Blank => ("", DOT_PX),
            Self::Readout => ("PREEM 88:88", DOT_PX),
            Self::Notdef => ("\u{e5}\u{e4}\u{f6} \u{1f495}", DOT_PX),
            Self::Dense => ("PREEM 88:88", DENSE_DOT_PX),
        }
    }
}

/// Where a gauge case's needle is when the frame is taken.
///
/// Three positions, chosen to cover what the shader has to get right: the
/// motion-blur fan **off** and **on** (it is the needle's own geometry
/// max-combined, so at rest it must vanish exactly rather than fatten the
/// blade), the lit value arc empty and full, and the overtravel stop.
#[derive(Clone, Copy)]
enum NeedleAt {
    /// Settled at rest, low on the scale: no fan, a short value arc.
    Rest,
    /// Mid-sweep toward full scale: the fan is spread, the arc is partly lit.
    Sweeping,
    /// Slammed to full scale and overshooting into the mechanical stop.
    Pegged,
}

impl NeedleAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Rest => "rest",
            Self::Sweeping => "sweep",
            Self::Pegged => "pegged",
        }
    }

    /// The target to point the needle at, and how many 60 Hz frames to run
    /// before the frame is taken. `None` frames means [`kit::Gauge::settle`] —
    /// parked on the reading with zero velocity, which is what makes the fan
    /// provably absent rather than merely small.
    fn drive(self) -> (f32, Option<u32>) {
        match self {
            Self::Rest => (0.3, None),
            // ~120 ms into a 2 Hz spring: past the halfway point and still
            // moving fast, so every blade of the fan is separated.
            Self::Sweeping => (0.85, Some(7)),
            // Full scale from rest overshoots past 1.0 into the overtravel,
            // which is where the drawn angle is clamped and the physics is not.
            Self::Pegged => (1.0, Some(16)),
        }
    }
}

impl Case {
    /// Which per-kind ceiling this case is held to — see `parity::Kind`.
    fn kind(&self) -> parity::Kind {
        match self {
            Self::Scope { .. } => parity::Kind::Scope,
            Self::Gauge { .. } => parity::Kind::Gauge,
            Self::DotMatrix { .. } => parity::Kind::DotMatrix,
        }
    }

    /// How the two buffers are brought to one grid — see `parity::Sampling`.
    ///
    /// Every scope case, every dot-matrix case and the `scale = 1` gauge cases
    /// compare pixel against pixel. The gauge's shipping-scale cases render
    /// `factor`× larger and are box-averaged down, which is a comparison the
    /// exact pin cannot apply to.
    fn sampling(&self) -> parity::Sampling {
        match self {
            Self::Gauge { scale, .. } if *scale > 1 => parity::Sampling::Supersampled(*scale),
            _ => parity::Sampling::OneToOne,
        }
    }

    /// `(logical cols, logical rows, integer upscale)` — the upscale the **GL**
    /// arm renders at.
    ///
    /// A dot matrix has no upscale at all, so its `1` is a statement rather
    /// than a setting, and its grid is whatever the line and the pitch make —
    /// resolved through the same `dot_matrix_surface` the shell calls, so this
    /// cannot drift from what the area is actually driven with.
    fn geometry(&self) -> (u32, u32, u32) {
        match self {
            Self::Scope { .. } => (SCOPE_COLS, SCOPE_ROWS, SCOPE_SCALE),
            Self::Gauge { scale, .. } => (GAUGE_COLS, GAUGE_ROWS, *scale),
            Self::DotMatrix { style, display } => {
                let (line, dot_px) = display.line();
                let surface = dot_matrix::dot_matrix_surface(
                    dot_matrix_config(*style, dot_px),
                    &dot_matrix::glyphs(line),
                    &kit::palette_snapshot(*style),
                );
                (surface.width, surface.height, 1)
            }
        }
    }

    /// The natural size in logical pixels — what the area is sized to.
    fn natural(&self) -> (u32, u32) {
        let (cols, rows, scale) = self.geometry();
        (cols * scale, rows * scale)
    }

    /// The size the **CPU reference frame** comes out at, which is the natural
    /// size for a 1:1 case and the logical grid for a supersampled one.
    fn reference_scale(&self) -> u32 {
        match self.sampling() {
            parity::Sampling::OneToOne => self.geometry().2,
            parity::Sampling::Supersampled(_) => 1,
        }
    }
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
    hytte::ui::gl_surface::register(gauge::GAUGE, gauge::GAUGE_PIPELINE);
    hytte::ui::gl_surface::register(dot_matrix::DOT_MATRIX, dot_matrix::DOT_MATRIX_PIPELINE);

    let cases: Vec<Case> = skins
        .iter()
        .flat_map(|style| {
            let scopes = [0_u32, 1, 5]
                .into_iter()
                .map(move |idle_steps| Case::Scope {
                    style: *style,
                    idle_steps,
                });
            let gauges = [NeedleAt::Rest, NeedleAt::Sweeping, NeedleAt::Pegged]
                .into_iter()
                .map(move |needle| Case::Gauge {
                    style: *style,
                    needle,
                    scale: GAUGE_SCALE,
                });
            // One supersampled case per skin, at the needle position that puts
            // the most anti-aliased edge on the face: the blade is at an
            // arbitrary angle, the fan is spread across four blades, and the
            // value arc has both of its ends on screen. See
            // [`GAUGE_SUPERSAMPLE`].
            let shipping = std::iter::once(Case::Gauge {
                style: *style,
                needle: NeedleAt::Sweeping,
                scale: GAUGE_SUPERSAMPLE,
            });
            let displays = [
                DisplayAt::Blank,
                DisplayAt::Readout,
                DisplayAt::Notdef,
                DisplayAt::Dense,
            ]
            .into_iter()
            .map(move |display| Case::DotMatrix {
                style: *style,
                display,
            });
            scopes.chain(gauges).chain(shipping).chain(displays)
        })
        .collect();

    let area = GlSurface::new();
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    // The window has to hold the **largest** case, because the size request
    // moves per case (the two kinds run at different grids) and an area GTK
    // could not give its requested size would be compared against a reference
    // of a different shape. `measure` says so out loud if that ever happens.
    let widest = cases.iter().map(|case| case.natural().0).max().unwrap_or(1);
    let tallest = cases.iter().map(|case| case.natural().1).max().unwrap_or(1);
    let width = i32::try_from(widest).unwrap_or(i32::MAX);
    let height = i32::try_from(tallest).unwrap_or(i32::MAX);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("preem_gl_diff")
        .default_width(width + 64)
        .default_height(height + 64)
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
                "PASS all {} case(s) — the 1:1 ones inside the proposed ceiling on every \
                 channel, the box-averaged ones bit-identical off every edge",
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

/// The gauge a case drives, in the state its frame is taken in.
///
/// **One builder for both arms.** The needle's spring is deterministic, so
/// `drive` and `measure` each call this and get the identical state — which is
/// what makes the comparison a comparison of *renderers* rather than of two
/// needles that happen to be near each other. It mirrors `preem_render::build`'s
/// own builder chain, `range` before `set_target` included.
fn gauge_state(config: vocab::GaugeConfig, needle: NeedleAt) -> kit::Gauge {
    let mut dial = kit::Gauge::with_size(config.cols as usize, config.rows as usize)
        .scale(config.scale as usize)
        .sweep_deg(config.sweep_deg)
        .ticks(config.divisions as usize, config.subdivisions as usize)
        .range(config.range.low, config.range.high)
        .frequency(config.frequency_hz)
        .damping(config.damping);
    let (target, frames) = needle.drive();
    dial.set_target(target);
    match frames {
        None => dial.settle(),
        Some(frames) => {
            for _ in 0..frames {
                dial.advance(1.0 / 60.0);
            }
        }
    }
    dial
}

/// The sample batch every scope case stamps — a wave with steep segments, so
/// the polyline join (the part `GL_LINES` would have got wrong) is exercised.
fn samples() -> Vec<f32> {
    (0..32_u8)
        .map(|i| {
            let t = f32::from(i) / 8.0;
            (t.sin() * 0.9 + (t * 3.0).cos() * 0.3).clamp(-1.0, 1.0)
        })
        .collect()
}

/// The wire style reference naming the same skin the kit enum does.
fn style_ref(style: kit::DisplayStyle) -> vocab::StyleRef {
    let name = vocab::StyleName::ALL
        .into_iter()
        .find(|candidate| candidate.name() == style.name())
        .unwrap_or_default();
    vocab::StyleRef::new(name)
}

fn scope_config(style: kit::DisplayStyle) -> vocab::ScopeConfig {
    vocab::ScopeConfig {
        style: style_ref(style),
        cols: SCOPE_COLS,
        rows: SCOPE_ROWS,
        scale: SCOPE_SCALE,
        persistence: PERSISTENCE,
    }
}

fn gauge_config(style: kit::DisplayStyle, scale: u32) -> vocab::GaugeConfig {
    vocab::GaugeConfig {
        style: style_ref(style),
        cols: GAUGE_COLS,
        rows: GAUGE_ROWS,
        scale,
        ..vocab::GaugeConfig::default()
    }
}

fn dot_matrix_config(style: kit::DisplayStyle, dot_px: u32) -> vocab::DotMatrixConfig {
    vocab::DotMatrixConfig {
        style: style_ref(style),
        dot_px,
    }
}

/// One case's name in the transcript and on its evidence files.
fn label(case: &Case) -> String {
    match case {
        Case::Scope { style, idle_steps } => format!("scope.{}.idle{idle_steps}", style.name()),
        // The upscale is in the name only where it is not the 1:1 comparison,
        // so the twelve pinned cases keep the labels #1143's transcripts carry.
        Case::Gauge {
            style,
            needle,
            scale,
        } if *scale == GAUGE_SCALE => format!("gauge.{}.{}", style.name(), needle.name()),
        Case::Gauge {
            style,
            needle,
            scale,
        } => format!("gauge.{}.{}.x{scale}", style.name(), needle.name()),
        Case::DotMatrix { style, display } => {
            format!("dot_matrix.{}.{}", style.name(), display.name())
        }
    }
}

/// Push a case's state at the surface and ask for a frame.
fn drive(area: &GlSurface, case: &Case) {
    let (program, width, height, uniforms) = match case {
        Case::Scope { style, idle_steps } => {
            let batch: std::sync::Arc<[f32]> = std::sync::Arc::from(&samples()[..]);
            // The debut batch is step 0; `idle_steps` more steps carry it into
            // the fade, exactly as `Renderer::ScopeGl::advance` counts them.
            let step_seq = 1 + u64::from(*idle_steps);
            let surface = program::scope_surface(
                scope_config(*style),
                &batch,
                Some(0),
                step_seq,
                &kit::palette_snapshot(*style),
            );
            (
                program::SCOPE,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::Gauge {
            style,
            needle,
            scale,
        } => {
            let config = gauge_config(*style, *scale);
            let dial = gauge_state(config, *needle);
            let surface = gauge::gauge_surface(
                config,
                dial.fraction(),
                dial.needle().velocity(),
                &kit::palette_snapshot(*style),
            );
            (
                gauge::GAUGE,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::DotMatrix { style, display } => {
            let (line, dot_px) = display.line();
            let surface = dot_matrix::dot_matrix_surface(
                dot_matrix_config(*style, dot_px),
                &dot_matrix::glyphs(line),
                &kit::palette_snapshot(*style),
            );
            (
                dot_matrix::DOT_MATRIX,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
    };
    // Per case, because the two kinds run at different grids — see `activate`.
    area.set_size_request(
        i32::try_from(width).unwrap_or(i32::MAX),
        i32::try_from(height).unwrap_or(i32::MAX),
    );
    area.set_state(program, width, height, &std::sync::Arc::new(uniforms));
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
    let upscale = case.reference_scale();

    // The CPU reference: the same state through the kit, which is the oracle.
    // A supersampled case takes the kit's **logical** frame — `reference_scale`
    // is 1 there — because that is what the GL readback is averaged down to.
    let reference = match case {
        Case::Scope { style, idle_steps } => {
            let mut oracle = kit::Scope::with_size(SCOPE_COLS as usize, SCOPE_ROWS as usize)
                .scale(SCOPE_SCALE as usize)
                .persistence(PERSISTENCE);
            oracle.advance(&samples());
            for _ in 0..*idle_steps {
                oracle.advance(&[]);
            }
            oracle.render(*style)
        }
        Case::Gauge { style, needle, .. } => {
            gauge_state(gauge_config(*style, upscale), *needle).render(*style)
        }
        Case::DotMatrix { style, display } => {
            let (line, dot_px) = display.line();
            kit::DotMatrix::new(*style)
                .dot_px(dot_px as usize)
                .render(line)
        }
    };

    let natural = case.natural();
    let expected = (natural.0 * shot.scale, natural.1 * shot.scale);
    if shot.alloc != expected {
        println!(
            "INFO {label}: allocation {}x{} is not the natural size {}x{} — \
             the window manager overrode the size request, so point sampling \
             differs and the numbers below are not a fair comparison",
            shot.alloc.0, shot.alloc.1, expected.0, expected.1
        );
    }

    // A supersampled case is box-averaged onto the kit's grid **before**
    // anything is measured, so every statistic below — the ceiling, the region
    // split, the delta map, the evidence images — is computed on one pair of
    // buffers of one shape (#1148 review, HIGH-2). The device scale folds into
    // the same divide: `factor` device pixels per reference pixel on each axis,
    // averaged in one pass, which leaves the layout at a device scale of 1.
    let (gl_raw, gl_alloc, device_scale) = match case.sampling() {
        parity::Sampling::OneToOne => (
            std::borrow::Cow::Borrowed(&shot.raw[..]),
            shot.alloc,
            shot.scale,
        ),
        parity::Sampling::Supersampled(factor) => {
            let (raw, alloc) = parity::box_downsample(&shot.raw, shot.alloc, factor * shot.scale);
            (std::borrow::Cow::Owned(raw), alloc, 1)
        }
    };

    // `for_capture` rather than a struct literal: the beam tolerance is
    // `parity::peak_row_tolerance`'s to compute, and it is the number that
    // decides `FAIL(beam)` in a transcript pasted on #893. Written out here it
    // drifted — it carried a stray device-scale factor, which made the verdict
    // depend on the monitor the harness ran on.
    let layout = parity::Layout::for_capture(
        gl_alloc,
        (reference.width(), reference.height()),
        device_scale,
        upscale as usize,
    );
    let stats = parity::compare(&gl_raw, reference.data(), layout);
    let deltas = parity::delta_map(&gl_raw, reference.data(), layout);
    let split = parity::regions(
        reference.data(),
        (reference.width(), reference.height()),
        &deltas,
    );
    // **The per-case verdict**: the blank-frame guards on every case, then
    // whichever standard this comparison is held to — #893's ceiling, the
    // scope's peak-row check and the `TROLLSHELL_PARITY_EXACT=1` zero pin at
    // 1:1 (#1143/#1148), the region split for a supersampled case. See
    // `parity::case_verdict`.
    let verdict = parity::case_verdict(&stats, &split, case.kind(), case.sampling(), exact);
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

    print_regions(&split);
    write_evidence(evidence, &label, &gl_raw, layout, &reference, &deltas);

    // #1080's 0-pinned assertion (#1078 review, INFO-1): the ceiling is loose
    // on purpose, for a driver this harness has never measured. Under llvmpipe
    // every 1:1 case of both kinds has come out bit-exact, so the pin says so
    // — and says so only for a comparison that can be, which is what
    // `case_verdict` decides above.
    if verdict == parity::Verdict::NotBitExact {
        println!(
            "      TROLLSHELL_PARITY_EXACT=1 — inside the ceiling but not \
             bit-exact, and llvmpipe has never measured anything but 0 for a \
             1:1 {} case",
            case.kind().label(),
        );
    }

    verdict.is_pass()
}

/// Print the **edge / field / lit** split, the classification aid that lets a
/// transcript from this harness be triaged without a driver in front of you
/// (#1072's four buckets).
///
/// The binning itself moved into `parity::regions` in #1148's review, because
/// the supersampled cases' verdict is a statement about it (everything off an
/// edge must be bit-identical) and because down there it has tests. See that
/// function for what each bin means and what a difference concentrated in one
/// of them says.
fn print_regions(split: &parity::Regions) {
    let show =
        |bin: parity::RegionStats| format!("n={} mean {:.3} max {}", bin.pixels, bin.mean, bin.max);
    println!(
        "      regions: edge[{}]  field{:?}[{}]  lit[{}]",
        show(split.edge),
        split.field_colour,
        show(split.field),
        show(split.lit),
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
    gl: &[u8],
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
        ("gl.ppm", ppm(w, h, &parity::gl_image(gl, layout))),
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
