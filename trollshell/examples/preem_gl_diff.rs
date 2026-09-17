//! `preem_gl_diff` — the GL/CPU parity harness for #893 stage B.
//!
//! Renders the same kit-widget state through **both** arms and prints the
//! per-channel delta, so the ceiling the spec proposes — mean ≤ 2/255,
//! p99 ≤ 8/255, max ≤ 32/255 — is a measurement rather than a hope.
//!
//! Five kinds since #1152, each four skins wide: the `Scope` (three fade
//! depths), the `Gauge` (three needle positions, plus one at the **shipping**
//! upscale and one **stretched**), the `DotMatrix` (five displays, plus one
//! stretched), the `Marquee` (five scroll phases, plus one stretched, plus one
//! per skin at a window
//! width where the centred origin and the bezel diverge — #1209 review,
//! MEDIUM-1) and the `TextBox` (four configurations, plus one stretched) —
//! **100** cases. Every case but the stretched and shipping-upscale ones runs
//! 1:1, where the GL arm's native grid and the kit's logical one are the same
//! number and the two can be compared pixel against pixel; that is where
//! `TROLLSHELL_PARITY_EXACT=1` pins all five
//! kinds at zero.
//!
//! What each kind's 1:1 cases *vary* is its own arithmetic. The dot matrix has
//! no `scale` on the widget at all — the dot pitch is its size knob (#1091) —
//! so its five vary the *line* and the *pitch* (see `DisplayAt`). The marquee's
//! five vary the **scroll phase**, because the phase is the widget: the shader
//! has no offset uniform (#839 made a sub-dot position inexpressible), so a
//! step is a different set of lit columns rather than a shifted sample (see
//! `TickerAt`). Its sixth 1:1 case holds the phase and varies the **window**
//! instead, so the centred origin no longer coincides with the bezel — see
//! `TICKER_ORIGIN_WINDOW_PX`. The text box's four vary the wrap, the upscale,
//! the corner radius and the palette pins (see `BubbleAt`).
//!
//! # The supersampled cases, and the standard they answer to
//!
//! Two case shapes render the GL arm at a higher resolution than the kit can:
//!
//! * The **fourth gauge case per skin** runs at `scale = 2`, which is
//!   `GaugeConfig::default()` and therefore every dial on the glass (#1148
//!   review, HIGH-2).
//! * **One case per skin on each of the other three kinds** takes an ordinary
//!   state and gives the *area* twice its natural size, which is what layout
//!   does to a chip in a container wider than its grid (`GlSurface::measure`
//!   asks for a minimum of `0` on purpose) — and what a `scale_factor >= 2`
//!   screen does to every chip on it, since `GlSurface`'s allocation is in
//!   **device** pixels. See [`STRETCH`].
//! * The **fifth gauge case per skin** (#1090's second report) is that same
//!   stretch applied to a dial: the grid held at `GAUGE_SCALE` and the area
//!   asked for at twice it, so the *only* thing between the kit's picture and
//!   the readback is the blit. It is the one case shape that measures a blit's
//!   own resampling in isolation, and until it existed the gauge's answer to it
//!   was a bit-exact nearest-neighbour replication — `max |Δ| 0` against the
//!   oracle, 100.0 % flat blocks, and a stair-stepped arc on the glass.
//!
//! None can be compared naively — the GL arm is rasterising at twice the
//! density *on purpose*, which is the whole of #1090's, #1144's and #1152's fix
//! — so the harness box-averages the native readback back down to the kit's
//! logical grid (`parity::box_downsample`) and then holds it to the split that
//! difference is supposed to have: **every pixel off a rasterisation edge is
//! bit-identical to the kit's** (`FAIL(interior)`), and the edge region is
//! inside a per-kind budget calibrated from measurement (`FAIL(edges)`).
//! Measured on llvmpipe, all sixteen land exactly there — field and lit
//! interiors at `max |Δ| 0`, an edge mean of 4.4 to 10.6 on the dot matrix, 4.7
//! to 10.8 on the marquee, 6.1 to 9.4 on the gauge and 0.0 to 14.0 on the text
//! box — which is a sharper statement than #893's ceiling could make
//! about them, and one #893's ceiling itself would fail (a dial is nearly a
//! third edge pixels, and on a dot matrix every lit pixel is one). A dropped
//! half-pixel offset, an unscaled length, a doubled mask pitch, a mis-scaled
//! bloom, a corner arc half a pixel off its buffer's edge or a lattice that
//! stopped being evaluated at the fragment's own position is what breaks it.
//! See `preem_gl::gauge`, `preem_gl::dot_matrix`, `preem_gl::textbox` and
//! `preem_gl::parity`'s `Kind`/`Sampling`/`case_verdict`.
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
//! All three are at the **reference** grid, after `box_downsample`. With
//! `PREEM_GL_DIFF_NATIVE` set it also writes `<case>.native.ppm`, the readback
//! at the size it was really rendered — off by default because it is a
//! duplicate for a 1:1 case, and the only way to *look at* a defect that lives
//! in the extra resolution. See [`write_native_frame`].
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
//! Measured under llvmpipe on 2026-09-12 (Mesa 26.2.2, GLES 3.2), every one of
//! the **84 1:1** cases came out **bit-exact**: max |Δ| 0 of 255 on R, G and B.
//! Treat any non-zero number from this harness as real. The sixteen
//! supersampled cases are not in that count and never could be — see above for
//! what they answer to instead.
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
//! Since #1148's review it pins every kind, not the scope alone — all five of
//! them since #1152, each on its own llvmpipe measurement. The
//! supersampled cases are unaffected either way: their verdict is the region
//! split above, which is already exact where exactness is meaningful and does
//! not depend on this variable at all.
//!
//! # Recovering frames from a failing CI run (#1310)
//!
//! `checkPhase` never produces `$out` for a failing derivation, so the
//! `.gl.ppm`/`.cpu.ppm` files [`write_evidence`] puts under
//! `PREEM_GL_DIFF_OUT` (`$out/parity` in `nix/checks/system-tests.nix`) are
//! unreachable for exactly the runs that matter — #1298's two Ice Lake reds
//! could only be studied through the per-case verdict line, not the frames.
//! So on every `FAIL(*)` verdict (never on `PASS` — a green run's log does not
//! grow) this binary now also base64-encodes that case's two evidence images
//! into its own stdout, between `=== preem-frame <case> {gl,cpu} {begin,end}
//! ===` marker lines, wrapped at [`BASE64_WRAP`] columns — the exact bytes
//! [`write_evidence`] wrote to disk, so the decoded file is byte-identical.
//! Capped at [`FRAME_DUMP_CAP`] cases per run, with one "not dumped" line
//! naming how many more failing cases were skipped, so a catastrophic run
//! cannot blow the job-log budget.
//!
//! Fetch the transcript — `gh run view <id> --log` for a live run, `gh api
//! repos/<owner>/<repo>/actions/jobs/<job-id>/logs` for one already gone from
//! the run list — and decode it back into files with:
//!
//! ```sh
//! gh run view <id> --log | \
//!   cargo run -p trollshell --example preem_gl_diff -- --decode --out frames
//! ```
//!
//! `--decode` reads a file path instead of stdin if one is given, and strips
//! GitHub's own `<job>\t<step>\t<timestamp> ` line prefix before matching a
//! marker (see [`strip_github_log_prefix`]), so a plain local transcript
//! (saved with `| tee`, no prefix at all) decodes the same way. See
//! [`run_decode`]/[`decode_frame_log`] for the format the marker pairs use
//! and [`dump_case_frames`] for what emits them.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use hytte::gtk::{self, glib, prelude::*};
use hytte::ui::gl_surface::{GlProgram, GlSurface};
use hytte_plugin_proto::preem as vocab;
use hytte_preem as kit;

// The shell's own pipeline, GLSL and uniform mapping — see the module docs on
// why this is a `#[path]` include and not a second copy.
#[path = "../src/plugins/preem_gl/program.rs"]
mod program;
// Which kinds have a GL arm, and their registration (#1211). `parity` and
// `cases` both `use super::kind::Kind`, which is what this module resolves to
// from either of them here.
#[path = "../src/plugins/preem_gl/kind.rs"]
mod kind;
// The delta statistics and the verdict. Also a `#[path]` include, and for a
// second reason on top of the first: `cargo test` does not run `#[test]`s
// inside an example (examples default to `test = false`), so the arithmetic
// that decides #893's ceiling lives in the shell's tree — `#[cfg(test)]`-
// mounted there — where `cargo test -p trollshell --lib` actually runs it.
#[path = "../src/plugins/preem_gl/parity.rs"]
mod parity;
// The case list `cases_for` builds, `#[path]`-included the same way and for
// the same reason (#1211): `plugins::tests`' `kind_enumeration` module reads
// this exact function's output, so the harness has to run the same code
// rather than a copy of it.
#[path = "../src/plugins/preem_gl/cases.rs"]
mod cases;
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
// The two text kinds (#1152). The marquee reaches `dot_matrix` through
// `super::dot_matrix::…` — it registers that very pipeline — which resolves
// here for the same reason `gauge`'s `super::program::…` does.
#[path = "../src/plugins/preem_gl/marquee.rs"]
mod marquee;
#[path = "../src/plugins/preem_gl/textbox.rs"]
mod textbox;
// The meter (#1153), included the same way and for the same reason.
#[path = "../src/plugins/preem_gl/led_strip.rs"]
mod led_strip;
// The board (#1155), included the same way and for the same reason.
#[path = "../src/plugins/preem_gl/flip_board.rs"]
mod flip_board;
// The readout (#1154), included the same way and for the same reason.
#[path = "../src/plugins/preem_gl/seven_seg.rs"]
mod seven_seg;
// The panel (#1156), included the same way and for the same reason. The one
// kind here that no plugin can send: `panels::stats` builds its surface from
// this very mapping.
#[path = "../src/plugins/preem_gl/led_matrix.rs"]
mod led_matrix;

// The case list itself lives in `cases` (#1211) — see its module docs. `Case`
// keeps its variants' field types (`DisplayAt`/`TickerAt`/`BubbleAt`/
// `NeedleAt`) there too; their `impl`s (`.name()`/`.line()`/`.spec()`) stay
// below, since an inherent impl only has to share a crate with its type, not
// a file.
use cases::{BoardAt, BubbleAt, DisplayAt, MeterAt, NeedleAt, PanelAt, ReadoutAt, TickerAt};
use cases::{
    Case, GAUGE_SCALE, GAUGE_SUPERSAMPLE, METER_LEDS, PANEL_CORES, STRETCH, TICKER_WINDOW_PX,
    cases_for,
};

/// Cards in a **flip board** case's row — `HH:MM:SS`, the kit's own
/// `DEFAULT_CELLS`, and the width every [`BoardAt`] text is written for.
///
/// Here rather than in `cases` because nothing in the case list itself reads
/// it: a `Case::FlipBoard` names a state, and how many cards that state is
/// drawn on is the harness's own choice — the same place `SCOPE_COLS` and
/// `GAUGE_COLS` live.
const FLIP_CELLS: usize = 8;

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
// [`GAUGE_SCALE`] (the upscale the **1:1** gauge cases run at) and
// [`GAUGE_SUPERSAMPLE`] (the upscale the **supersampled** ones run at,
// box-averaged back down through `parity::box_downsample` and held to
// `parity::case_verdict`'s supersampled standard) moved to `cases` (#1211)
// along with the rest of the case list. `GAUGE_SUPERSAMPLE` is
// `GaugeConfig`'s own default, what every dial on the glass actually uses
// (#1148 review, HIGH-2); without it nothing in CI ever rendered the gauge
// in the configuration it ships in.

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
/// `MAX_DOT_PX` — see [`DisplayAt::Coarse`], which is there for the one
/// arithmetic the other four pitches cannot reach.
const COARSE_DOT_PX: u32 = 8;

// [`STRETCH`] (how much bigger than the kit's buffer the **stretched**
// cases size their area — the one case shape that measures the
// improvement rather than the agreement), [`TICKER_WINDOW_PX`] (the window
// the **marquee** cases run at) and [`TICKER_ORIGIN_WINDOW_PX`] (the window
// **one marquee case per skin** runs at instead, closing the #1209 review's
// MEDIUM-1: `origin_x = pad + floor(r/2)` where `r = (window_px - 2*pad) mod
// dot`, and at 96 px with the default pitch that remainder is 0 — `origin_x`
// collides with the bezel for every other case, so nothing could tell the
// centred grid from the bezel apart; 98 keeps the same 22-cell grid and
// moves `origin_x` to `5`) all moved to `cases` (#1211), along with the rest
// of the case list. `GlSurface::measure` requests a minimum of `0` on both
// axes on purpose ("so CSS/layout can scale the surface above its grid
// size, which is the whole LCD look"), so a stretched case is a real
// configuration and not a contrived one — it is deliberately not held to
// the bit-exact pin or #893's ceiling, only to a bit-identical interior plus
// an edge budget (`parity::case_verdict`).

/// The wrap width the **textbox** cases run at, in glyph cells — wide enough
/// that a sentence wraps to three lines and narrow enough that the hugging
/// cases are visibly narrower than the fixed ones.
const BUBBLE_COLS: u32 = 11;

/// Wrapped-line cap for every textbox case — the kit's own default, and what
/// [`BubbleAt::Wrapped`] is measured against filling.
const BUBBLE_LINES: u32 = 3;

/// The ink [`BubbleAt::Pinned`] pins and the `.notdef` it names — the pet's and
/// caw's own configuration (#884/#885) minus its field pin.
///
/// **The field is deliberately left to the skin.** Pinning all three makes the
/// rendered box skin-*independent*, and four identical comparisons carry one
/// case's worth of information — measured: with the field pinned too, all four
/// `textbox.*.pinned` cases reported byte-for-byte the same statistics. What a
/// pinned field reaches is `u_bg`, and that is covered hermetically by
/// `preem_gl::textbox`'s `the_baked_palette_reaches_the_uniforms`, which drives
/// all three pins at once.
const PINNED_INK: [u8; 4] = [0xf0, 0xd0, 0xff, 0xff];
const PINNED_NOTDEF: [u8; 4] = [0x80, 0x60, 0xa0, 0xff];

/// How many failing cases' `.gl.ppm`/`.cpu.ppm` pairs get base64-dumped into
/// the transcript in one run (#1310) — see [`dump_case_frames`] and the
/// module docs' "Recovering frames from a failing CI run" section. Chosen so
/// the worst-seen run (four failing cases, #1298) is nowhere near the cap
/// while a run where every case fails cannot blow the job-log budget:
/// measured against a real 160-case run, the largest evidence file of any
/// case is a `seven_seg.*.digits` `.ppm` at 85 274 bytes (28 420 px × 3 B),
/// ~113.7 KiB base64 before line breaks — so the worst possible case (both
/// its `.gl.ppm` and `.cpu.ppm` at that size) is on the order of 230 KiB, and
/// eight of those is under 2 MiB.
const FRAME_DUMP_CAP: u32 = 8;

/// Base64 line width for [`dump_case_frames`]'s marker blocks — the
/// historical MIME/PEM wrap column. The job log and a `gh`-fetched
/// transcript are both plain text either way, so this is purely for a human
/// scrolling past one on the way to something else.
const BASE64_WRAP: usize = 76;

/// Whether any case failed, for [`main`]'s exit status.
///
/// A process-global rather than a value threaded out of `activate`, because
/// there is nowhere to thread it *to*: `GApplication::run` returns the
/// application's own status, which nothing here sets, and the `activate`
/// callback returns `()`. `Relaxed` is enough — it is written on the GTK main
/// thread and read after `run` returns on the same thread.
static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many cases actually reached a verdict, for [`main`]'s exit status
/// (#1150 review, MEDIUM-1).
///
/// **A run that measured nothing must not exit 0.** Twice in six runs on
/// #1144's branch this binary printed its header and exited **0** with no case
/// lines, no `-- summary --` and no evidence file — a second, byte-identical
/// invocation printed all of them. CI survives that on
/// `nix/checks/system-tests.nix`'s `*.gl.ppm` count alone; a human following
/// `docs/live-verify.md` reads a silent exit 0 as a pass, and this branch adds
/// six live-verify items that lean on it. So the exit status now answers "did
/// this run measure anything" as well as "did anything fail".
///
/// The double-`activate` this counter was originally watching for is
/// **#1151**, latched shut in `main`'s `connect_activate` closure (see
/// [`activate_once`]) so a second signal can no longer stand up a second
/// `Runner`. This counter stays regardless, deliberately independent of that
/// one cause: it is the detector for *any* session that ends before its first
/// verdict, whatever produces that shape next.
static VERDICTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn main() -> glib::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `--decode` is a standalone mode (#1310): it never touches GL or GTK, so
    // it is dispatched before `parse_skins` — a decode invocation carries no
    // `--skins` and needs none of the harness below.
    if args.first().map(String::as_str) == Some("--decode") {
        return match run_decode(&args[1..]) {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(why) => {
                eprintln!("preem_gl_diff --decode: {why}");
                glib::ExitCode::FAILURE
            }
        };
    }
    let skins = match parse_skins(&args) {
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
         (box-averaged down), one dial at s{STRETCH} (the grid held at \
         {GAUGE_SCALE}, the area {STRETCH}x it); \
         dot matrix pitch {DOT_PX} (and {DENSE_DOT_PX}, {COARSE_DOT_PX}, \
         one readout at x{STRETCH}); \
         ceiling mean {} / p99 {} / max {} per channel",
        parity::CEILING_MEAN,
        parity::CEILING_P99,
        parity::CEILING_MAX,
    );
    let exact = parity_exact();
    if exact {
        println!(
            "TROLLSHELL_PARITY_EXACT=1: a **1:1** case of any kind with a \
             non-zero delta on any channel fails as FAIL(exact), even inside \
             the ceiling above."
        );
    }
    println!(
        "the .x{GAUGE_SUPERSAMPLE} gauge cases and the .x{STRETCH} dot-matrix ones are \
         box-averaged down from a denser render and take neither the ceiling nor that \
         pin: every pixel off a rasterisation edge must be bit-identical \
         (FAIL(interior)) and the edge region has a per-kind budget, gauge mean \
         {gauge_mean} / max {gauge_max}, dot matrix mean {dots_mean} / max {dots_max} \
         (FAIL(edges)). See `preem_gl::parity`'s `case_verdict`.",
        gauge_mean = parity::Kind::Gauge.edge_budget().mean,
        gauge_max = parity::Kind::Gauge.edge_budget().max,
        dots_mean = parity::Kind::DotMatrix.edge_budget().mean,
        dots_max = parity::Kind::DotMatrix.edge_budget().max,
    );

    let app = gtk::Application::builder()
        .application_id("mov.vibec0re.trollshell.preem-gl-diff")
        .build();
    // #1151: `activate` can fire a second time on one process — measured as
    // `GApplication`'s own machinery, not this harness (the `connect_activate`
    // call right below is the *only* place in this file that can trigger the
    // signal; nothing here calls `.activate()` or emits it by name). A second
    // `Runner` sharing the `GlSurface`'s GL pool with the first, interleaved
    // on the same main loop, is exactly the shape #1072's wrong-framebuffer
    // readback came from, so the second call is latched into a no-op rather
    // than left to luck — see [`activate_once`] for the pure predicate this
    // is built on.
    let activated = Cell::new(false);
    app.connect_activate(move |app| {
        if !activate_once(&activated) {
            println!(
                "INFO: activate fired again on this process (#1151) — ignoring, \
                 one Runner per GL pool"
            );
            return;
        }
        activate(app, &skins, exact);
    });
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
    // …and a run that reached **no** verdict at all is a failure too, however
    // it got there: an emptied case list, or the session ending before the
    // first case (#1151). See [`VERDICTS`].
    if VERDICTS.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        println!(
            "FAIL(nothing-measured): the runner reported 0 case verdicts — \
             this run measured nothing and is not a pass"
        );
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
  cargo run -p trollshell --example preem_gl_diff -- [--skins vfd,lcd,oled,crt]
  cargo run -p trollshell --example preem_gl_diff -- --decode [--out DIR] [LOGFILE]
      recover a failing case's frames dumped into the job log (#1310);
      reads LOGFILE, or stdin if none is given";
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

// `Case` and its four supporting enums (`TickerAt`, `BubbleAt`, `DisplayAt`,
// `NeedleAt`) moved to `cases` (#1211) — see that module's docs and the
// `use cases::{…}` near the top of this file. Their `impl`s stay below, next
// to the code that actually drives a kit widget from them: an inherent impl
// only has to share a crate with its type, not a file.

/// One textbox case's box, spelled out: the knobs each [`BubbleAt`] turns, and
/// the text it shows.
///
/// A struct rather than a tuple because five of the seven fields differ between
/// cases and a positional `(…)` of that width is unreadable at the call site.
#[derive(Clone, Copy)]
struct Bubble {
    text: &'static str,
    scale: u32,
    corner: u32,
    pad: u32,
    fixed_width: bool,
    /// Whether the field/ink pins and the explicit `.notdef` are in play.
    pinned: bool,
}

// `DisplayAt` moved to `cases` (#1211) too — see above. `impl DisplayAt`
// stays here, next to the pitch constants (`DOT_PX`/`DENSE_DOT_PX`/
// `COARSE_DOT_PX`) its `.line()` reads.

impl DisplayAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Blank => "blank",
            Self::Readout => "readout",
            Self::Notdef => "notdef",
            Self::Dense => "dense",
            Self::Coarse => "coarse",
        }
    }

    /// `(line, dot pitch)`.
    fn line(self) -> (&'static str, u32) {
        match self {
            Self::Blank => ("", DOT_PX),
            Self::Readout => ("PREEM 88:88", DOT_PX),
            Self::Notdef => ("\u{e5}\u{e4}\u{f6} \u{1f495}", DOT_PX),
            Self::Dense => ("PREEM 88:88", DENSE_DOT_PX),
            // Shorter than the others on purpose: at `MAX_DOT_PX` the eleven
            // characters the rest carry would make a 528 px strip, and the only
            // thing this case is here to move is the bezel's own short side.
            Self::Coarse => ("88:88", COARSE_DOT_PX),
        }
    }
}

/// The message that overflows a [`TICKER_WINDOW_PX`] window at the default
/// pitch, so it scrolls — and whose period is long enough that all three
/// [`TickerAt::Scrolled`] phases land in different parts of it.
const TICKER_LONG: &str = "PREEM RASTER KIT ~ SCROLLING TICKER ~ ";

// [`TICKER_SEAM_PHASE`] — the phase that lands inside the loop seam —
// moved to `cases` (#1211). `TICKER_LONG` rasterises to 223 bitmap columns
// and the kit appends a `GLYPH_W + SPACING` gap, so a 22-cell grid starting
// there straddles the message's tail and the blank seam.

impl TickerAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> String {
        match self {
            Self::Empty => "empty".to_owned(),
            Self::Held => "held".to_owned(),
            Self::Scrolled(offset) => format!("phase{offset}"),
        }
    }

    /// `(message, scroll offset in dots)`.
    fn line(self) -> (&'static str, usize) {
        match self {
            Self::Empty => ("", 0),
            // Four characters is 4 columns against a 22-cell grid, so the kit
            // holds it; the offset is deliberately non-zero to prove the hold
            // ignores it.
            Self::Held => ("HI 8", 9),
            Self::Scrolled(offset) => (TICKER_LONG, offset),
        }
    }
}

impl BubbleAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::OneLine => "line",
            Self::Wrapped => "wrapped",
            Self::Pinned => "pinned",
        }
    }

    /// The box this case renders, knob by knob.
    fn spec(self) -> Bubble {
        match self {
            Self::Empty => Bubble {
                text: "",
                scale: 1,
                corner: 2,
                pad: 3,
                fixed_width: false,
                pinned: false,
            },
            Self::OneLine => Bubble {
                text: "mrrp!",
                scale: 2,
                corner: 2,
                pad: 3,
                fixed_width: false,
                pinned: false,
            },
            Self::Wrapped => Bubble {
                text: "the quick brown fox jumps over the lazy dog",
                scale: 2,
                corner: 2,
                pad: 3,
                fixed_width: true,
                pinned: false,
            },
            // `pad` below the corner radius on purpose: the cut then reaches
            // into the glyph block, which is the only way to render the kit's
            // "a glyph pixel is stamped *after* the field and does not consult
            // the corner" rule.
            //
            // `scale = 1` deliberately, because this is also the case the
            // **stretched** comparison uses. The kit's `scale` replicates each
            // logical pixel into a `scale²` block, and `parity::regions`
            // classifies the reference's own structure with a 4-neighbour rule
            // — so at `scale = 2` the middle of a replicated block has four
            // equal neighbours and lands in an *interior* bin even though it
            // sits on the arc. A one-pixel shape difference there would be
            // reported as `FAIL(interior)` rather than measured against the
            // edge budget, which would be the gate mis-classifying the very
            // improvement it exists to bound. `OneLine` and `Wrapped` carry the
            // `scale = 2` coverage, bit-exact, where the snap makes it exact.
            Self::Pinned => Bubble {
                text: "hi \u{1f495} ok",
                scale: 1,
                corner: 5,
                pad: 2,
                fixed_width: true,
                pinned: true,
            },
        }
    }
}

/// The kit `TextBox` a bubble case renders, with its palette already baked —
/// which for this widget means **built inside the pin scope**, exactly as
/// `preem_render::build` does it.
///
/// One builder for both arms, for [`gauge_state`]'s reason: the CPU reference
/// and the GL mapping each call this, so the comparison is of two renderers
/// rather than of two boxes that happen to be configured alike.
fn bubble_box(style: kit::DisplayStyle, bubble: BubbleAt) -> kit::TextBox {
    let spec = bubble.spec();
    let pins = if spec.pinned {
        kit::Pins {
            ink: kit::Ink::Fixed(PINNED_INK),
            field: None,
        }
    } else {
        kit::Pins {
            ink: kit::Ink::Default,
            field: None,
        }
    };
    kit::with_pins(pins, || {
        let boxed = kit::TextBox::styled(style)
            .cols(BUBBLE_COLS as usize)
            .max_lines(BUBBLE_LINES as usize)
            .pad(spec.pad as usize)
            .corner(spec.corner as usize)
            .scale(spec.scale as usize)
            .fixed_width(spec.fixed_width);
        if spec.pinned {
            boxed.notdef(PINNED_NOTDEF)
        } else {
            boxed
        }
    })
}

/// The kit `MarqueeStrip` a ticker case renders. One builder for both arms, for
/// [`bubble_box`]'s reason. `window_px` is [`Case::Marquee`]'s own field, not
/// always [`TICKER_WINDOW_PX`] — see [`TICKER_ORIGIN_WINDOW_PX`].
fn ticker_strip(style: kit::DisplayStyle, ticker: TickerAt, window_px: u32) -> kit::MarqueeStrip {
    let (text, _) = ticker.line();
    kit::Marquee::new(style)
        .window_px(window_px as usize)
        .dot_px(DOT_PX as usize)
        .render(text)
}

// `NeedleAt` moved to `cases` (#1211) too — see above. `impl NeedleAt`
// stays here, next to `gauge_state`, which is the only thing that reads it.

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

impl MeterAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Half => "half",
            Self::Full => "full",
            Self::PeakAbove => "peakabove",
            Self::PeakAt => "peakat",
            Self::PeakInside => "peakinside",
        }
    }

    /// `(level, peak)` — the two numbers the widget is entirely a function of,
    /// already folded the way the shell's pump hands them to both arms.
    ///
    /// `0.35`/`0.8` puts the dot four segments clear of the lit run at
    /// [`METER_LEDS`], which is wider than the widest bloom (radius 3 over an
    /// 11 px pitch) so the two halos are genuinely separate on the glass.
    /// `0.6`/`0.6` is the **adjacent** arrangement: `lit_count` rounds to 14
    /// segments and `peak_led` ceils onto index 14, so the dot sits on the
    /// first *unlit* segment — the boundary case where the two emissions are
    /// adjacent and the cap composites next to ink the level pass laid down,
    /// never over it (#1293 item 2 — `cases.rs`'s `MeterAt::PeakAt` doc used
    /// to claim these two overlap). `0.9`/`0.5` is the arrangement that
    /// actually does: `lit_count` rounds `0.9 × 24` to 22 and `peak_led`
    /// ceils `0.5 × 24` onto index 11, well inside `0..21`, so `mix_kit(under
    /// = ink, cap, dot)` genuinely composites the cap over ink the level pass
    /// already laid down — the one arrangement no case drove before.
    fn reading(self) -> (f32, f32) {
        match self {
            Self::Empty => (0.0, 0.0),
            Self::Half => (0.5, 0.0),
            Self::Full => (1.0, 0.0),
            Self::PeakAbove => (0.35, 0.8),
            Self::PeakAt => (0.6, 0.6),
            Self::PeakInside => (0.9, 0.5),
        }
    }
}

/// The kit `LedStrip` a meter case renders. One builder for both arms, for
/// [`bubble_box`]'s reason.
///
/// `leds` is [`METER_LEDS`] for every case but the one at a second segment
/// count (#1293 item 2).
fn meter_strip(style: kit::DisplayStyle, leds: usize) -> kit::LedStrip {
    kit::LedStrip::new(style).leds(leds)
}

impl PanelAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Ramp => "ramp",
            Self::Full => "full",
            Self::Lonely => "lonely",
            Self::Ragged => "ragged",
            Self::Single => "single",
            Self::Style => "style",
        }
    }

    /// How many lamps this case's panel holds.
    fn cores(self) -> usize {
        match self {
            Self::Single => 1,
            // The ragged case is the shell's own `rows = 3` shape at 64 cores:
            // 22 columns, 66 slots, two of them spare. See `PanelAt::Ragged`.
            Self::Ragged => 64,
            _ => PANEL_CORES,
        }
    }

    /// The **fixture** brightness grid, deliberately not a live reading — one
    /// level per lamp, as `panels::stats` feeds the kit.
    #[allow(clippy::cast_precision_loss)]
    fn levels(self) -> Vec<f32> {
        let cores = self.cores();
        match self {
            Self::Dark => vec![0.0; cores],
            Self::Full => vec![1.0; cores],
            Self::Lonely => {
                let mut levels = vec![0.0; cores];
                levels[cores / 2] = 1.0;
                levels
            }
            // The degenerate panel's one lamp at a partial intensity, so it is
            // not just another all-or-nothing frame.
            Self::Single => vec![0.61],
            // A ramp that never lands on 0 or 1 at either end, so every lamp
            // carries a *different* partial intensity — the state the meter's
            // all-or-nothing segments cannot be in.
            Self::Ramp | Self::Style | Self::Ragged => (0..cores)
                .map(|i| (i as f32 + 0.5) / cores as f32)
                .collect(),
        }
    }
}

/// The kit `LedMatrix` a panel case renders, in the shape and dressing
/// `panels::stats`' `core_led_matrix_for` builds. One builder for both arms,
/// for [`bubble_box`]'s reason.
fn panel_grid(style: kit::DisplayStyle, at: PanelAt) -> kit::LedMatrix {
    let cores = at.cores();
    match at {
        // The pinned-row shape `core-leds.toml`'s `rows = 3` produces, with the
        // ragged tail left blank — the one case where `ghost_slots`' second arm
        // and the spare slots' ink clamp reach a driver.
        PanelAt::Ragged => kit::LedMatrix::new(style, cores.div_ceil(3), 3)
            .color(kit::ColorMap::Heat)
            .fill(kit::Fill::Blank),
        // The single-ink path `LedMatrix::render` takes verbatim, which every
        // other kit widget shares.
        PanelAt::Style => kit::LedMatrix::wide(style, cores).color(kit::ColorMap::Style),
        // Everything else runs the shipped default: the automatic wide
        // rectangle, `ColorMap::Heat`, `Fill::Spare`.
        _ => kit::LedMatrix::wide(style, cores).color(kit::ColorMap::Heat),
    }
}

impl ReadoutAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Blank => "blank",
            Self::Eights => "eights",
            Self::Clock => "clock",
            Self::Digits => "digits",
            Self::Pinned => "pinned",
        }
    }

    /// The text this case reads out — the widget is entirely a function of it
    /// and the skin, since `kit::seven_seg` is pure.
    fn text(self) -> &'static str {
        match self {
            Self::Empty => "",
            Self::Blank => " ",
            Self::Eights => "88",
            Self::Clock => "12:34",
            Self::Digits => "9876543210",
            Self::Pinned => "-8",
        }
    }

    /// The palette scope this case renders in — `Ink::Fixed` for the pinned
    /// one, the skin's own default otherwise.
    fn pins(self) -> kit::Pins {
        kit::Pins {
            ink: match self {
                Self::Pinned => kit::Ink::Fixed(PINNED_INK),
                _ => kit::Ink::Default,
            },
            field: None,
        }
    }
}

/// The palette snapshot a readout case maps from, resolved **inside** its own
/// pin scope so the mapping sees exactly what the kit render below will.
///
/// One helper for both arms, for [`bubble_box`]'s reason: a `seven_seg` resolves
/// its palette at *render* time (unlike a `TextBox`, which bakes at
/// construction), so both calls have to sit in the scope rather than one.
fn readout_palette(style: kit::DisplayStyle, readout: ReadoutAt) -> kit::PaletteSnapshot {
    kit::with_pins(readout.pins(), || kit::palette_snapshot(style))
}

/// The CPU kit's own readout at this case's text and pin scope.
fn readout_reference(style: kit::DisplayStyle, readout: ReadoutAt) -> kit::Frame {
    kit::with_pins(readout.pins(), || kit::seven_seg(readout.text(), style))
}

/// How far into a cell's transition [`BoardAt::Rolling`] takes the board's
/// clock, as a fraction of the mechanism's own `default_duration_secs`.
///
/// `0.79` rather than a round number because the two mechanisms stagger
/// differently and this is the one fraction that puts both in an interesting
/// place. A **nixie** bank has no ripple, so every tube is at `p = 0.79`: both
/// cathodes alight, the outgoing one collapsing fast and the incoming one
/// nearly struck. A **split flap** ripples at `DEFAULT_STAGGER_SECS`, so the
/// eight cards come out at eight different phases — six of them mid-fall, at
/// `p` from `0.79` down to `0.07`, spanning horizontal (`p = 1/sqrt(2)`) so
/// both the `falling_up` and the `falling down` branches are on screen at once,
/// and the last two still waiting out their stagger at `p = 0`.
const ROLLING_FRACTION: f32 = 0.79;

impl BoardAt {
    /// The word in the case label and on its evidence files.
    fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Rest => "rest",
            Self::HalfFlip => "halfflip",
            Self::Rolling => "rolling",
            Self::Notdef => "notdef",
            Self::Pinned => "pinned",
        }
    }

    /// `(what the row was resting on, what it was told to show, how far into
    /// one transition the clock is)` — the whole of a board's state, since a
    /// cell's progress is a closed-form function of the clock.
    fn change(self) -> (&'static str, &'static str, f32) {
        match self {
            // No cells at all, so nothing to say and nowhere to say it.
            Self::Empty => ("", "", 0.0),
            Self::Rest => ("12:34:56", "12:34:56", 0.0),
            // One card changed on an unstaggered board, so its clock is the
            // board's and half a duration is exactly half a flip.
            Self::HalfFlip => ("12:34:56", "12:34:57", 0.5),
            Self::Rolling | Self::Pinned => ("00:00:00", "12:34:56", ROLLING_FRACTION),
            // Eight cards that are not on the drum, all in flight: the kit's
            // notdef box on every leaf.
            Self::Notdef => ("12:34:56", "########", ROLLING_FRACTION),
        }
    }

    /// The palette scope this case renders in — `Ink::Fixed` for the pinned
    /// one, the skin's own default otherwise.
    fn pins(self) -> kit::Pins {
        kit::Pins {
            ink: match self {
                Self::Pinned => kit::Ink::Fixed(PINNED_INK),
                _ => kit::Ink::Default,
            },
            field: None,
        }
    }
}

/// The kit `FlipBoard` a board case renders, driven into this case's state.
/// One builder for both arms, for [`bubble_box`]'s reason — a duplicated
/// builder is an oracle that agrees with the code by construction.
///
/// The builder chain's order is load-bearing and the kit says so: `cells`
/// rebuilds the row blank, so it comes first; `stagger_secs` is a config knob
/// and has to be set before any text; and `settle` between the two `set_text`s
/// is what makes the second one a *change* with a clock rather than a
/// continuation.
fn flip_board_state(mechanism: kit::Mechanism, at: BoardAt, scale: u32) -> kit::FlipBoard {
    let cells = match at {
        BoardAt::Empty => 0,
        _ => FLIP_CELLS,
    };
    let mut board = kit::FlipBoard::new(mechanism)
        .cells(cells)
        .scale(scale.max(1) as usize);
    if matches!(at, BoardAt::HalfFlip) {
        // The ripple would put this one changed card at its own stagger
        // offset; without it the card's clock *is* the board's.
        board = board.stagger_secs(0.0);
    }
    let (from, to, fraction) = at.change();
    board.set_text(from);
    board.settle();
    board.set_text(to);
    board.advance(mechanism.default_duration_secs() * fraction);
    board
}

/// The palette snapshot a board case maps from, resolved **inside** its own pin
/// scope so the mapping sees exactly what the kit render below will.
///
/// One helper for both arms, for [`readout_palette`]'s reason: a `FlipBoard`
/// resolves its palette at *render* time, so both calls have to sit in the
/// scope rather than one.
fn board_palette(style: kit::DisplayStyle, at: BoardAt) -> kit::PaletteSnapshot {
    kit::with_pins(at.pins(), || kit::palette_snapshot(style))
}

/// The CPU kit's own board at this case's state and pin scope — the *same*
/// builder the mapping resolved its geometry from.
fn board_reference(
    style: kit::DisplayStyle,
    mechanism: kit::Mechanism,
    at: BoardAt,
    scale: u32,
) -> kit::Frame {
    kit::with_pins(at.pins(), || {
        flip_board_state(mechanism, at, scale).render(style)
    })
}

/// The GL node payload a board case drives the surface with.
fn board_surface(
    style: kit::DisplayStyle,
    mechanism: kit::Mechanism,
    at: BoardAt,
    scale: u32,
) -> program::KitSurface {
    let board = flip_board_state(mechanism, at, scale);
    flip_board::flip_board_surface(&flip_board::cards(&board), &board_palette(style, at))
}

/// The GL node payload a panel case drives the surface with — the same
/// `panel_grid` builder the CPU reference renders, so a disagreement is between
/// renderers and not between two panels.
fn panel_surface(style: kit::DisplayStyle, at: PanelAt, scale: u32) -> program::KitSurface {
    led_matrix::led_matrix_surface(
        &panel_grid(style, at),
        &at.levels(),
        scale,
        &kit::palette_snapshot(style),
    )
}

/// The wire config a meter case maps from — the same segment count the kit
/// builder above takes, so the two arms cannot end up on different strips.
fn led_strip_config(style: kit::DisplayStyle, leds: usize) -> vocab::LedStripConfig {
    vocab::LedStripConfig {
        style: style_ref(style),
        leds: u32::try_from(leds).unwrap_or(u32::MAX),
        // Deliberately `None`: the shell's pump folds a declared `PeakHoldConfig`
        // into the peak it hands *both* arms, so a harness case's peak is a
        // number, not a decay policy. `MeterAt::reading` is that number.
        peak_hold: None,
    }
}

impl Case {
    // `Case::kind` — which per-kind ceiling a case is held to — moved to
    // `cases` (#1211), next to the enum itself; `plugins::tests`'
    // `kind_enumeration` module calls it directly. Every call site here
    // (`case.kind()`) is unchanged: same method name, same return type
    // (`cases`'s `Kind` and `parity`'s are the one type, re-exported).

    /// How the two buffers are brought to one grid — see `parity::Sampling`.
    ///
    /// Every scope case, every dot-matrix case and the `scale = 1` gauge cases
    /// compare pixel against pixel. The gauge's shipping-scale cases render
    /// `factor`× larger and are box-averaged down, which is a comparison the
    /// exact pin cannot apply to.
    fn sampling(&self) -> parity::Sampling {
        match self {
            // The gauge is the one kind carrying both knobs, so its factor is
            // the **product**: `scale` denser grid × `stretch` bigger
            // allocation, either of which puts the readback above the kit's
            // logical grid. Every other kind uses exactly one of the two.
            Self::Gauge { scale, stretch, .. } if scale * stretch > 1 => {
                parity::Sampling::Supersampled(scale * stretch)
            }
            // The two kinds the **kit** renders at two resolutions: their
            // `scale` is `Frame::upscale`'s, so a `scale > 1` case is the kit
            // rasterising once and replicating against the GL arm resolving the
            // same geometry at every device pixel.
            Self::FlipBoard { scale, .. } | Self::LedMatrix { scale, .. } if *scale > 1 => {
                parity::Sampling::Supersampled(*scale)
            }
            Self::DotMatrix { stretch, .. }
            | Self::Marquee { stretch, .. }
            | Self::TextBox { stretch, .. }
            | Self::LedStrip { stretch, .. }
            | Self::SevenSeg { stretch, .. }
                if *stretch > 1 =>
            {
                parity::Sampling::Supersampled(*stretch)
            }
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
            // The third element is what `natural` multiplies the logical grid
            // by to size the **area**, and for this kind that is both knobs:
            // `scale` is already inside the grid the offscreen passes run at
            // (`gauge_surface` multiplies it in), and `stretch` is the extra
            // room layout gives the chip on top of it.
            Self::Gauge { scale, stretch, .. } => (GAUGE_COLS, GAUGE_ROWS, scale * stretch),
            Self::DotMatrix {
                style,
                display,
                stretch,
            } => {
                let (line, dot_px) = display.line();
                let surface = dot_matrix::dot_matrix_surface(
                    dot_matrix_config(*style, dot_px),
                    &dot_matrix::glyphs(line),
                    &kit::palette_snapshot(*style),
                );
                (surface.width, surface.height, (*stretch).max(1))
            }
            // Both text kinds resolve their grid the dot matrix's way: through
            // the very mapping the shell calls, so it cannot drift from what
            // the area is driven with. Neither has an upscale to report here —
            // the ticker's pitch *is* its size knob and the box's `scale` is
            // already baked into the buffer the kit ships — so the third
            // element is the stretch, exactly as the dot matrix's is.
            Self::Marquee {
                style,
                ticker,
                stretch,
                window_px,
            } => {
                let strip = ticker_strip(*style, *ticker, *window_px);
                let surface = marquee::marquee_surface(
                    &strip,
                    &marquee::window(&strip, ticker.line().1),
                    &kit::palette_snapshot(*style),
                );
                (surface.width, surface.height, (*stretch).max(1))
            }
            Self::TextBox {
                style,
                bubble,
                stretch,
            } => {
                let boxed = bubble_box(*style, *bubble);
                let layout = boxed.layout(bubble.spec().text);
                let surface = textbox::textbox_surface(&layout, &textbox::block(&layout));
                (surface.width, surface.height, (*stretch).max(1))
            }
            // The meter resolves its grid the same way, through the very
            // mapping the shell calls. It has no upscale either — the segment
            // metrics are its size knob — so the third element is the stretch.
            Self::LedStrip {
                style,
                meter,
                stretch,
                leds,
            } => {
                let (level, peak) = meter.reading();
                let surface = led_strip::led_strip_surface(
                    led_strip_config(*style, *leds),
                    level,
                    peak,
                    &kit::palette_snapshot(*style),
                );
                (surface.width, surface.height, (*stretch).max(1))
            }
            // The readout resolves its grid the same way, through the very
            // mapping the shell calls. It has no upscale either — the cell
            // metrics are its size knob — so the third element is the stretch.
            Self::SevenSeg {
                style,
                readout,
                stretch,
            } => {
                let surface = seven_seg::seven_seg_surface(
                    &seven_seg::readout(readout.text()),
                    &readout_palette(*style, *readout),
                );
                (surface.width, surface.height, (*stretch).max(1))
            }
            // The board resolves its grid through the very mapping the shell
            // calls too — but the third element is the kit's own **upscale**,
            // not a stretch: `FlipBoard::render` rasterises into the grid and
            // `Frame::upscale` replicates it, so the natural size is
            // `grid × scale` and `cols`/`rows` here are the pre-upscale buffer
            // (the scope's shape). Reading it off `uniforms.grid` rather than
            // off `surface.width` is what keeps that true.
            Self::FlipBoard {
                style,
                mechanism,
                board,
                scale,
            } => {
                let surface = board_surface(*style, *mechanism, *board, *scale);
                (
                    surface.uniforms.grid.0,
                    surface.uniforms.grid.1,
                    (*scale).max(1),
                )
            }
            // The panel resolves its grid through the very mapping the shell
            // calls too, and — like the board — the third element is an
            // **upscale** rather than a stretch: the kit rasterises into the
            // grid and `PixelSurface::set_scale` replicates it, so the natural
            // size is `grid × scale`. Reading it off `uniforms.grid` rather
            // than off `surface.width` is what keeps that true.
            Self::LedMatrix {
                style,
                panel,
                scale,
            } => {
                let surface = panel_surface(*style, *panel, *scale);
                (
                    surface.uniforms.grid.0,
                    surface.uniforms.grid.1,
                    (*scale).max(1),
                )
            }
        }
    }

    /// The natural size in logical pixels — what the area is sized to.
    ///
    /// For a supersampled case that is the *GL* arm's size, above the CPU
    /// reference's: the gauge renders `scale ×` larger internally and the
    /// stretched dot matrix is simply handed a bigger allocation. Either way
    /// the readback is box-averaged back onto the reference grid before
    /// anything is compared — see [`Case::sampling`].
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

/// Whether this call to the `activate` handler is the first one for `latch`
/// (#1151).
///
/// **True** exactly once for a given `Cell`, starting from a fresh
/// `Cell::new(false)`; **false** on every call after that. Pure and GL-free
/// on purpose — [`the_second_activate_is_latched`] exercises it without a
/// display, even though the thing it guards (`main`'s `connect_activate`
/// closure) cannot be constructed without one.
fn activate_once(latch: &Cell<bool>) -> bool {
    !latch.replace(true)
}

thread_local! {
    /// Pipelines this run's driver has refused to compile or link — the
    /// harness's own record of what [`install_build_refusal_reporter`]'s
    /// handler has already printed, and what [`verdict_label`] reads back to
    /// relabel that pipeline's cases (#1325 item 2).
    ///
    /// A `Vec` and a linear scan, not a `HashSet`: `kind::Kind::ALL` names
    /// nine programs total and this fires at most once per program per run,
    /// the same trade `preem_gl::mod`'s own `REFUSED` latch documents for the
    /// shell's build-refusal record.
    static REPORTED_REFUSALS: RefCell<Vec<GlProgram>> = const { RefCell::new(Vec::new()) };
}

/// Install this harness's own build-refusal reporter (#1325 item 2).
///
/// Without this, a driver that refuses to compile or link a pipeline is
/// invisible here: `hytte-ui`'s own diagnostic
/// (`gl_surface.rs`'s `report_build_refusal`) is a `tracing::warn!` this
/// binary never subscribes to, and [`hytte::ui::gl_surface::refuse_build`]'s
/// host hook — the one place the driver's info log actually reaches Rust —
/// has nothing installed here to receive it (`trollshell`'s shell binary
/// installs its own handler in `plugins::preem_gl::install`, and that is a
/// different process). Every case of the refused pipeline then read back an
/// untouched, transparent framebuffer and reported `FAIL(nothing)` with
/// nothing in the log to say why — the whole cost #1325 measured building
/// #1155: a run of 44 cases to bisect down to one reserved GLSL identifier.
///
/// Printed **once per program**, not once per `(grid, program)` refusal
/// [`hytte::ui::gl_surface`]'s own latch is keyed on: a compile or link
/// failure is a fact about the source, so a pipeline this driver refuses at
/// one grid it refuses at every grid this run asks for (a stretched or
/// supersampled case shares its kind's program under a different grid), and
/// repeating the same info log once per case would bury the one line worth
/// reading. And it prints **synchronously**, from inside the render callback
/// that first hits the refusal — which runs before [`capture`] returns to the
/// case that triggered it — so it lands ahead of that case's own verdict
/// line, named by program.
fn install_build_refusal_reporter() {
    hytte::ui::gl_surface::set_build_refusal_handler(|program, grid, reason| {
        let first = REPORTED_REFUSALS.with_borrow_mut(|seen| {
            if seen.contains(&program) {
                false
            } else {
                seen.push(program);
                true
            }
        });
        if first {
            println!(
                "COMPILE REFUSED {} (first refused at {}x{}): {reason}",
                program.0, grid.0, grid.1,
            );
        }
    });
}

fn activate(app: &gtk::Application, skins: &[kit::DisplayStyle], exact: bool) {
    // Installed before anything registers or draws a single pipeline, so it
    // is armed for the very first case — see its own docs.
    install_build_refusal_reporter();
    // The same registration `preem_gl::install` does in the shell — since
    // #1211, both loop over `kind::Kind::ALL`, so this is the same code
    // rather than a second hand-kept copy of it.
    for kind in kind::Kind::ALL {
        let (program, pipeline) = kind.gl_seam();
        hytte::ui::gl_surface::register(program, pipeline);
    }

    let cases = cases_for(skins);

    let area = GlSurface::new();
    area.set_halign(gtk::Align::Center);
    area.set_valign(gtk::Align::Center);
    // The window has to hold the **largest** case, because the size request
    // moves per case (the kinds run at different grids) and an area GTK
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
        dumped: Cell::new(0),
        frames_skipped: Cell::new(0),
    });

    glib::timeout_add_local(std::time::Duration::from_millis(60), {
        let area = area.clone();
        let app = app.clone();
        move || runner.step(&area, &app)
    });
}

// `cases_for` itself moved to `preem_gl::cases` (#1211) — see that module's
// docs. `plugins::tests`' `kind_enumeration` module calls the same function
// this harness does, so the case count it checks against `Kind::ALL` and
// `nix/checks/system-tests.nix` is never a hand-kept mirror of the real
// list.

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
    /// How many failing cases have had their frames base64-dumped into the
    /// transcript so far — see [`FRAME_DUMP_CAP`].
    dumped: Cell<u32>,
    /// How many failing cases exceeded [`FRAME_DUMP_CAP`] and were **not**
    /// dumped — [`Runner::summary`] reports this count in one line.
    frames_skipped: Cell<u32>,
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
                    (Some(a), Ok(b)) if a.raw == b.raw && a.alloc == b.alloc => measure(
                        case,
                        &b,
                        &self.evidence,
                        self.exact,
                        &self.dumped,
                        &self.frames_skipped,
                    ),
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
        VERDICTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
        // **An empty case list is a failure, not a pass** (#1150 review,
        // MEDIUM-1). Without this, a `--skins` regression that silently
        // emptied the list would print `PASS all 0 case(s)` and exit 0 — the
        // exact shape `nix/checks/system-tests.nix`'s evidence count exists to
        // catch, and the one a reviewer running this by hand would miss.
        if self.cases.is_empty() {
            println!(
                "FAIL(empty): no cases to measure — a harness that measured nothing \
                 has not agreed with anything"
            );
            FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            println!("=== preem_gl_diff done — paste this into issue #893 ===");
            return;
        }
        if self.failures.get() == 0 {
            // The supersampled cases are counted here — they are cases that
            // can fail like any other — but they answer to a different
            // standard, so the sentence says which of the two things it is
            // claiming about how many. See `parity::case_verdict`.
            let supersampled = self
                .cases
                .iter()
                .filter(|case| case.sampling() != parity::Sampling::OneToOne)
                .count();
            // The flatness ceiling (`parity::with_native_flatness`) only ever
            // *asserts* under `TROLLSHELL_PARITY_EXACT=1` — without it, the
            // per-case verdict above still prints the measured fraction but
            // never fails on it (#1238's review, LOW). Saying "inside their
            // ceiling" regardless would claim a plain run checked something it
            // only ever printed.
            let flatness = if self.exact {
                // "case", not "kind", since #1155's review: the ceiling is
                // resolved per case, because one kind's two mechanisms answer
                // it differently. See `cases::Case::flat_block_ceiling`.
                "inside their native-frame flatness ceiling where their case states one"
            } else {
                "with their native-frame flatness printed, not asserted, since \
                 TROLLSHELL_PARITY_EXACT=1 was not set"
            };
            println!(
                "PASS all {} case(s) — {} 1:1 ones inside the proposed ceiling on every \
                 channel, {supersampled} box-averaged ones bit-identical off every edge, \
                 inside their edge budget and {flatness}",
                self.cases.len(),
                self.cases.len() - supersampled,
            );
        } else {
            println!(
                "FAIL {} of {} case(s) — see the per-case verdict above",
                self.failures.get(),
                self.cases.len()
            );
            // #1310: past `FRAME_DUMP_CAP` dumped cases, every further failing
            // case is counted here instead of dumped, in this one line, so a
            // run where every case fails cannot blow the job log.
            let skipped = self.frames_skipped.get();
            if skipped > 0 {
                println!(
                    "…and {skipped} more failing case(s), not dumped — \
                     FRAME_DUMP_CAP is {FRAME_DUMP_CAP} case(s) per run"
                );
            }
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
///
/// The `too_many_lines` allow is `cases_for`'s, for its reason: this is one
/// flat `match` arm per case shape, nothing nested, and it crossed the ceiling
/// when #1090's second round added the stretched gauge's. Splitting it would
/// put half the naming scheme somewhere else, which is worse to read and worse
/// to review than a long `match` — and the one property that matters here is
/// that no two shapes can name the same evidence file, which only a reader
/// seeing every arm at once can check.
#[allow(clippy::too_many_lines)]
fn label(case: &Case) -> String {
    match case {
        Case::Scope { style, idle_steps } => format!("scope.{}.idle{idle_steps}", style.name()),
        // The upscale is in the name only where it is not the 1:1 comparison,
        // so the pinned cases keep the labels #1143's and #1144's transcripts
        // carry — and the stretched case names its factor with an `s` rather
        // than that `x`, because the two knobs are different resolutions (see
        // `cases::Case::Gauge`) and at `STRETCH == GAUGE_SUPERSAMPLE` an `x` on
        // both would collide on the evidence files.
        Case::Gauge {
            style,
            needle,
            stretch,
            ..
        } if *stretch > 1 => format!("gauge.{}.{}.s{stretch}", style.name(), needle.name()),
        Case::Gauge {
            style,
            needle,
            scale,
            ..
        } if *scale == GAUGE_SCALE => format!("gauge.{}.{}", style.name(), needle.name()),
        Case::Gauge {
            style,
            needle,
            scale,
            ..
        } => format!("gauge.{}.{}.x{scale}", style.name(), needle.name()),
        Case::DotMatrix {
            style,
            display,
            stretch,
        } if *stretch > 1 => format!("dot_matrix.{}.{}x{stretch}", style.name(), display.name()),
        Case::DotMatrix { style, display, .. } => {
            format!("dot_matrix.{}.{}", style.name(), display.name())
        }
        Case::Marquee {
            style,
            ticker,
            stretch,
            ..
        } if *stretch > 1 => format!("marquee.{}.{}x{stretch}", style.name(), ticker.name()),
        // The odd-origin case (#1209 review, MEDIUM-1) — named for the window
        // it runs at, the same way the stretch above names itself for its
        // factor, so its evidence files don't collide with the same skin's
        // default-window case.
        Case::Marquee {
            style,
            ticker,
            window_px,
            ..
        } if *window_px != TICKER_WINDOW_PX => {
            format!("marquee.{}.{}w{window_px}", style.name(), ticker.name())
        }
        Case::Marquee { style, ticker, .. } => {
            format!("marquee.{}.{}", style.name(), ticker.name())
        }
        Case::TextBox {
            style,
            bubble,
            stretch,
        } if *stretch > 1 => format!("textbox.{}.{}x{stretch}", style.name(), bubble.name()),
        Case::TextBox { style, bubble, .. } => {
            format!("textbox.{}.{}", style.name(), bubble.name())
        }
        // The second-segment-count case (#1293 item 2) names its `leds` so
        // its evidence files don't collide with the same skin/meter's
        // `METER_LEDS` case — the odd-origin marquee's naming shape.
        Case::LedStrip {
            style, meter, leds, ..
        } if *leds != METER_LEDS => {
            format!("led_strip.{}.{}.leds{leds}", style.name(), meter.name())
        }
        Case::LedStrip {
            style,
            meter,
            stretch,
            ..
        } if *stretch > 1 => format!("led_strip.{}.{}x{stretch}", style.name(), meter.name()),
        Case::LedStrip { style, meter, .. } => {
            format!("led_strip.{}.{}", style.name(), meter.name())
        }
        Case::SevenSeg {
            style,
            readout,
            stretch,
        } if *stretch > 1 => format!("seven_seg.{}.{}x{stretch}", style.name(), readout.name()),
        Case::SevenSeg { style, readout, .. } => {
            format!("seven_seg.{}.{}", style.name(), readout.name())
        }
        Case::FlipBoard {
            style,
            mechanism,
            board,
            scale,
        } if *scale > 1 => format!(
            "flip_board.{}.{}.{}x{scale}",
            style.name(),
            mechanism.name(),
            board.name()
        ),
        Case::FlipBoard {
            style,
            mechanism,
            board,
            ..
        } => format!(
            "flip_board.{}.{}.{}",
            style.name(),
            mechanism.name(),
            board.name()
        ),
        Case::LedMatrix {
            style,
            panel,
            scale,
        } if *scale > 1 => format!("led_matrix.{}.{}x{scale}", style.name(), panel.name()),
        Case::LedMatrix { style, panel, .. } => {
            format!("led_matrix.{}.{}", style.name(), panel.name())
        }
    }
}

/// Push a case's state at the surface and ask for a frame.
///
/// The `too_many_lines` allow is the case list's, not this function's: it is
/// one flat arm per kind with no nesting between them, and it crossed the
/// ceiling when #1153 added the sixth. The same trade `preem_render::advance`
/// states — splitting it would put half the drive table somewhere else.
#[allow(clippy::too_many_lines)]
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
            ..
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
        Case::DotMatrix { style, display, .. } => {
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
        Case::Marquee {
            style,
            ticker,
            window_px,
            ..
        } => {
            let strip = ticker_strip(*style, *ticker, *window_px);
            let surface = marquee::marquee_surface(
                &strip,
                &marquee::window(&strip, ticker.line().1),
                &kit::palette_snapshot(*style),
            );
            (
                marquee::MARQUEE,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::TextBox { style, bubble, .. } => {
            let boxed = bubble_box(*style, *bubble);
            let layout = boxed.layout(bubble.spec().text);
            let surface = textbox::textbox_surface(&layout, &textbox::block(&layout));
            (
                textbox::TEXTBOX,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::LedStrip {
            style, meter, leds, ..
        } => {
            let (level, peak) = meter.reading();
            let surface = led_strip::led_strip_surface(
                led_strip_config(*style, *leds),
                level,
                peak,
                &kit::palette_snapshot(*style),
            );
            (
                led_strip::LED_STRIP,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::SevenSeg { style, readout, .. } => {
            let surface = seven_seg::seven_seg_surface(
                &seven_seg::readout(readout.text()),
                &readout_palette(*style, *readout),
            );
            (
                seven_seg::SEVEN_SEG,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::FlipBoard {
            style,
            mechanism,
            board,
            scale,
        } => {
            let surface = board_surface(*style, *mechanism, *board, *scale);
            (
                flip_board::FLIP_BOARD,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
        Case::LedMatrix {
            style,
            panel,
            scale,
        } => {
            let surface = panel_surface(*style, *panel, *scale);
            (
                led_matrix::LED_MATRIX,
                surface.width,
                surface.height,
                surface.uniforms,
            )
        }
    };
    // Per case, because the kinds run at different grids — see `activate`. The
    // **requested** size, not the grid: a stretched case (#1144) deliberately
    // asks for more room than the surface's natural size so the blit rasterises
    // the lattice at a higher resolution, which is what `Case::stretch`
    // exists to arrange and what `measure` box-averages back down.
    let (request_w, request_h) = case.natural();
    area.set_size_request(
        i32::try_from(request_w).unwrap_or(i32::MAX),
        i32::try_from(request_h).unwrap_or(i32::MAX),
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

/// The word this harness prints for `verdict` on a case of `kind`.
///
/// [`parity::Verdict::label`], unless `kind`'s own pipeline is one this run's
/// driver has already refused to compile or link
/// ([`install_build_refusal_reporter`] has printed that refusal's info log,
/// once, ahead of every case it touches) — in which case the transcript says
/// `FAIL(compile)` rather than `FAIL(nothing)`, so a reader does not have to
/// work out on their own that a run of blank-frame failures is one cause
/// repeated rather than N independently blank draws (#1325 item 2).
///
/// Deliberately **not** a new [`parity::Verdict`] variant: the comparison
/// itself still reports [`parity::Verdict::RendersNothing`] — an all-zero
/// readback is exactly that, whatever caused it — so the pass/fail count, the
/// evidence dump and every test that pins `RendersNothing` against a
/// synthetic all-zero capture stay exactly as they were. Only the
/// transcript's word for it changes, and only for the one verdict a refused
/// pipeline can actually produce: a surface the driver refused to build keeps
/// "whatever it last successfully drew" (`hytte-ui`'s own words — nothing,
/// before a first successful frame), never a non-black flat fill, so
/// `UndrawnFramebuffer` and every other `Verdict` are left untouched here.
fn verdict_label(verdict: parity::Verdict, kind: kind::Kind) -> &'static str {
    if verdict == parity::Verdict::RendersNothing && pipeline_refused(kind) {
        "FAIL(compile)"
    } else {
        verdict.label()
    }
}

/// Whether this run's driver has refused to build `kind`'s own pipeline —
/// [`install_build_refusal_reporter`]'s record, read back by
/// [`verdict_label`].
fn pipeline_refused(kind: kind::Kind) -> bool {
    let (program, _) = kind.gl_seam();
    REPORTED_REFUSALS.with_borrow(|seen| seen.contains(&program))
}

/// Build the CPU reference, compare, print the per-channel deltas and the
/// worst pixel, and write the evidence images. Returns whether the case passed
/// — see [`parity::Verdict`] for the five ways it can fail.
///
/// On a failing verdict, also dumps the case's two evidence images as base64
/// into the transcript (#1310) — `dumped`/`skipped` are [`Runner::dumped`]/
/// [`Runner::frames_skipped`], threaded through rather than read off a
/// `Runner` so this stays callable (and tested) without one.
///
/// `too_many_lines` for [`drive`]'s reason: one flat arm per kind builds the
/// oracle, and everything after that `match` is a single linear sequence of
/// prints.
#[allow(clippy::too_many_lines)]
fn measure(
    case: &Case,
    shot: &Capture,
    evidence: &std::path::Path,
    exact: bool,
    dumped: &Cell<u32>,
    skipped: &Cell<u32>,
) -> bool {
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
        Case::DotMatrix { style, display, .. } => {
            let (line, dot_px) = display.line();
            kit::DotMatrix::new(*style)
                .dot_px(dot_px as usize)
                .render(line)
        }
        // The kit's own window at this phase — the *same* strip the mapping
        // read its geometry off, so a disagreement here is a disagreement
        // between renderers and not between two tickers.
        Case::Marquee {
            style,
            ticker,
            window_px,
            ..
        } => ticker_strip(*style, *ticker, *window_px).window(ticker.line().1),
        // The box's `scale` is already in these bytes (the kit upscales at the
        // end of `render`), which is why `reference_scale` has nothing to do
        // for this kind either — see `Case::geometry`.
        Case::TextBox { style, bubble, .. } => {
            bubble_box(*style, *bubble).render(bubble.spec().text)
        }
        // The kit's own meter at this reading — the *same* builder the mapping
        // resolved its grid from, so a disagreement here is a disagreement
        // between renderers and not between two strips.
        Case::LedStrip {
            style, meter, leds, ..
        } => {
            let (level, peak) = meter.reading();
            meter_strip(*style, *leds).render(level, peak)
        }
        // The kit's own readout at this text, in this case's pin scope — the
        // *same* scope the mapping resolved its palette in, so a disagreement
        // here is a disagreement between renderers and not between palettes.
        Case::SevenSeg { style, readout, .. } => readout_reference(*style, *readout),
        // The kit's own board at this state, in this case's pin scope, at the
        // scale `reference_scale` decided — `1` for a supersampled case, since
        // that is the grid the GL readback is averaged back down to, and the
        // case's own upscale for a 1:1 one.
        Case::FlipBoard {
            style,
            mechanism,
            board,
            ..
        } => board_reference(*style, *mechanism, *board, upscale),
        // The kit's own panel at this fixture grid — the *same* builder the
        // mapping resolved its grid from, so a disagreement here is a
        // disagreement between renderers and not between two panels. The
        // `upscale` is `PixelSurface::set_scale`'s nearest-neighbour
        // replication, which is what the CPU arm on the glass actually does
        // with this frame, so the reference reproduces it with the kit's own
        // `Frame::upscale` rather than with a second copy of that rule.
        Case::LedMatrix { style, panel, .. } => panel_grid(*style, *panel)
            .render(&panel.levels())
            .upscale(upscale as usize),
    };

    let requested = case.natural();
    let expected = (requested.0 * shot.scale, requested.1 * shot.scale);
    if shot.alloc != expected {
        println!(
            "INFO {label}: allocation {}x{} is not the requested size {}x{} — \
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
    // Asked of the **native** readback, before the average that would erase it:
    // "is this denser render actually denser, or is it a grid-resolution
    // quantity replicated across each block?" (#1238's review). The one check
    // here that never looks at the oracle, and the only one that can see a
    // halo read at the kit's grid — see `parity::flat_block_fraction`.
    let flatness = parity::flat_block_fraction(&shot.raw, shot.alloc, case.sampling(), shot.scale);
    write_native_frame(&label, shot);
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
    let verdict = parity::with_native_flatness(
        parity::case_verdict(&stats, &split, case.kind(), case.sampling(), exact),
        // Per **case**, not per kind (#1155 review): the flip board's two
        // mechanisms answer this one differently and correctly, and only the
        // case knows which it is. Every other kind's answer is its kind's.
        case.flat_block_ceiling(),
        flatness,
        exact,
    );
    println!(
        "{} {label}: worst channel mean {:.3} p99 {:.0} max {:.0} of 255 \
         over {} px; peak-row mismatches {}/{}",
        verdict_label(verdict, case.kind()),
        stats.worst_mean(),
        stats.worst_p99(),
        stats.worst_max(),
        stats.pixels,
        stats.peak_row_mismatches,
        reference.width(),
    );
    print_channels(&stats, case.sampling());
    print_regions(&split);
    print_flatness(case.flat_block_ceiling(), flatness);
    let (gl_ppm, cpu_ppm) = write_evidence(evidence, &label, &gl_raw, layout, &reference, &deltas);

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

    // #1310: never on a `PASS` — a green run's log must not grow — and capped
    // at `FRAME_DUMP_CAP` even among the failing ones. See
    // [`should_dump_frames`] for the pure predicate this is built on.
    if should_dump_frames(verdict.is_pass(), dumped, skipped) {
        dump_case_frames(&label, &gl_ppm, &cpu_ppm);
    }

    verdict.is_pass()
}

/// Print the three per-channel distributions and the worst pixel.
///
/// Split out of [`measure`], which is at the workspace's `too_many_lines`
/// ceiling — and this is the part of it that is about the transcript rather
/// than about the comparison.
fn print_channels(stats: &parity::Stats, sampling: parity::Sampling) {
    for (channel, name) in stats.channels.iter().zip(parity::CHANNELS) {
        println!(
            "      {name}: mean {:.3} p99 {:.0} max {:.0} of 255{}",
            channel.mean,
            channel.p99,
            channel.max,
            // The ceiling is only a case's contract at 1:1; a supersampled
            // case answers to the region split instead, so the annotation
            // would be pointing at a number nothing is judging.
            if channel.inside_ceiling() || sampling != parity::Sampling::OneToOne {
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

/// Print the **native-frame flatness** of a supersampled case, with the
/// ceiling its kind is held to under `TROLLSHELL_PARITY_EXACT=1` (#1238's
/// review).
///
/// Printed on every supersampled case, including the kinds that state no
/// ceiling, because the number is the calibration: whoever adds a ceiling for
/// the gauge or the text box needs to see what those frames measure first, and
/// a statistic that is only printed where it is already asserted cannot tell
/// them. See `parity::flat_block_fraction`.
fn print_flatness(ceiling: Option<f64>, fraction: Option<f64>) {
    let Some(fraction) = fraction else {
        return;
    };
    match ceiling {
        Some(ceiling) => println!(
            "      native flat blocks {:.1}% of the supersampled frame (ceiling {:.1}%, \
             asserted under TROLLSHELL_PARITY_EXACT=1)",
            fraction * 100.0,
            ceiling * 100.0,
        ),
        None => println!(
            "      native flat blocks {:.1}% of the supersampled frame (no ceiling for this case)",
            fraction * 100.0,
        ),
    }
}

/// Write the three evidence files for one case: what GL drew, what the kit
/// drew, and where they disagree. Returns the `.gl.ppm`/`.cpu.ppm` bytes it
/// wrote, so a failing case's caller ([`measure`]) can dump the **exact**
/// same bytes into the job log (#1310) rather than re-deriving them and
/// risking the two falling out of step.
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
) -> (Vec<u8>, Vec<u8>) {
    let (w, h) = (reference.width(), reference.height());
    let mut cpu = Vec::with_capacity(w * h * 3);
    for pixel in reference.data().chunks_exact(4) {
        cpu.extend_from_slice(&pixel[..3]);
    }
    let gl_ppm = ppm(w, h, &parity::gl_image(gl, layout));
    let cpu_ppm = ppm(w, h, &cpu);
    let delta_pgm = pgm(w, h, deltas);
    let files: [(&str, &Vec<u8>); 3] = [
        ("gl.ppm", &gl_ppm),
        ("cpu.ppm", &cpu_ppm),
        ("delta.pgm", &delta_pgm),
    ];
    for (suffix, bytes) in files {
        let path = dir.join(format!("{label}.{suffix}"));
        if let Err(why) = std::fs::write(&path, bytes) {
            println!("INFO {label}: could not write {} — {why}", path.display());
        }
    }
    (gl_ppm, cpu_ppm)
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

// --- #1310: dumping and recovering a failing case's evidence frames -------
//
// The job log is the only place a failing sandboxed `nix flake check` run
// leaves anything reachable — `checkPhase` never produces `$out` for a
// failing derivation — so a `FAIL(*)` verdict base64-encodes that case's two
// `.ppm` files straight into stdout, and `--decode` reverses it. Both
// directions are hand-rolled (RFC 4648 standard alphabet, padded) rather than
// pulling in a crate: this workspace has no other reason to depend on one,
// and the format is small enough that encode and decode are each a handful
// of pure, cheaply-tested lines. See the module docs' "Recovering frames from
// a failing CI run" section for the operator-facing shape.

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 4648 standard base64, padded — plain ASCII, one line.
/// [`wrap_base64`] is what breaks it into [`BASE64_WRAP`]-column lines.
fn base64_encode(data: &[u8]) -> String {
    /// The alphabet character for one 6-bit group of a 24-bit accumulator.
    fn sextet(n: u32, shift: u32) -> char {
        char::from(BASE64_ALPHABET[usize::try_from(n >> shift & 0x3f).unwrap_or(0)])
    }
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = chunk.get(1).copied().unwrap_or(0);
        let b2 = chunk.get(2).copied().unwrap_or(0);
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8) | u32::from(b2);
        out.push(sextet(n, 18));
        out.push(sextet(n, 12));
        out.push(if chunk.len() > 1 { sextet(n, 6) } else { '=' });
        out.push(if chunk.len() > 2 { sextet(n, 0) } else { '=' });
    }
    out
}

/// Split an already-encoded base64 string into [`BASE64_WRAP`]-column lines.
/// `encoded` is pure ASCII by construction ([`base64_encode`]'s alphabet), so
/// slicing on byte offsets never lands inside a multi-byte character.
fn wrap_base64(encoded: &str) -> impl Iterator<Item = &str> {
    encoded
        .as_bytes()
        .chunks(BASE64_WRAP)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64_encode only emits ASCII"))
}

/// The value of one base64 alphabet character, or `None` for `=` (padding,
/// handled by the caller) and anything else.
fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// The inverse of [`base64_encode`]. Whitespace (the newlines [`wrap_base64`]
/// introduced, and any `\r` a Windows-authored log carries) is stripped
/// before decoding, so a wrapped, saved-and-reopened block round-trips.
fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes: Vec<u8> = encoded
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "base64 block has {} non-whitespace byte(s), not a multiple of 4",
            bytes.len()
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for group in bytes.chunks_exact(4) {
        // Valid padding is only `XX==`, `XXX=` or `XXXX` — anything else
        // (a `=` earlier than that, or a single `=` followed by a non-`=`)
        // is malformed rather than merely padded.
        let pad = match (group[2] == b'=', group[3] == b'=') {
            (true, true) => 2,
            (false, true) => 1,
            (false, false) => 0,
            (true, false) => return Err("misplaced '=' padding in a base64 group".to_owned()),
        };
        if group[0] == b'=' || group[1] == b'=' {
            return Err("misplaced '=' padding in a base64 group".to_owned());
        }
        let mut n: u32 = 0;
        for &byte in group {
            let value = if byte == b'=' {
                0
            } else {
                base64_value(byte).ok_or_else(|| format!("invalid base64 byte {byte:#x}"))?
            };
            n = (n << 6) | u32::from(value);
        }
        let triple = n.to_be_bytes();
        match pad {
            0 => out.extend_from_slice(&triple[1..4]),
            1 => out.extend_from_slice(&triple[1..3]),
            _ => out.push(triple[1]),
        }
    }
    Ok(out)
}

/// One `=== preem-frame <case> <kind> <state> ===` marker line.
fn frame_marker(case: &str, kind: &str, state: &str) -> String {
    format!("=== preem-frame {case} {kind} {state} ===")
}

/// Whether one case's frames should be dumped into the transcript, updating
/// `dumped`/`skipped` either way (#1310) — **never** on a passing verdict, so
/// a green run's log does not grow at all, and capped at [`FRAME_DUMP_CAP`]
/// among the failing ones, past which every further case is only counted
/// (`skipped`), not dumped. Pure and GL-free on purpose, the same shape
/// [`activate_once`] is — `frame_dump_tests` exercises both rules without a
/// capture in sight.
fn should_dump_frames(passed: bool, dumped: &Cell<u32>, skipped: &Cell<u32>) -> bool {
    if passed {
        return false;
    }
    if dumped.get() < FRAME_DUMP_CAP {
        dumped.set(dumped.get() + 1);
        true
    } else {
        skipped.set(skipped.get() + 1);
        false
    }
}

/// Write the **readback at the size it was actually rendered**, as
/// `<case>.native.ppm` beside the other evidence — only when
/// `PREEM_GL_DIFF_NATIVE` is set, since for a 1:1 case it is a duplicate of
/// `<case>.gl.ppm` and for a supersampled one it is `factor²` times the bytes.
///
/// **The one buffer the evidence files otherwise never carry**, and the reason
/// this exists: every statistic and every image above is computed *after*
/// `box_downsample` has brought the readback onto the kit's grid, so a defect
/// that lives entirely in the extra resolution is reported as a number
/// (`flat_block_fraction`) and nothing a reader can look at. #1090's second
/// round is exactly that shape — a stretched dial came back bit-identical to
/// the oracle on every gate while being a visibly stair-stepped replication on
/// the glass, and the staircase is only in *these* bytes.
///
/// Bottom-up RGBA8 in, top-down RGB `P6` out — the flip every consumer would
/// otherwise have to know about (`magick <f> -flip …`).
fn write_native_frame(label: &str, shot: &Capture) {
    if std::env::var_os("PREEM_GL_DIFF_NATIVE").is_none() {
        return;
    }
    let (width, height) = shot.alloc;
    let stride = (width as usize).saturating_mul(4);
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for row in (0..height as usize).rev() {
        let start = row.saturating_mul(stride);
        let Some(line) = shot.raw.get(start..start + stride) else {
            return;
        };
        for texel in line.chunks_exact(4) {
            ppm.extend_from_slice(&texel[..3]);
        }
    }
    let mut path = out_dir();
    path.push(format!("{label}.native.ppm"));
    if let Err(error) = std::fs::write(&path, &ppm) {
        println!("INFO {label}: could not write {}: {error}", path.display());
    }
}

/// Base64-dump a failing case's two evidence images into the transcript,
/// between marker lines `decode_frame_log` can find again — see the module
/// docs' "Recovering frames from a failing CI run" section. Called from
/// [`measure`], already behind the `FRAME_DUMP_CAP` gate.
fn dump_case_frames(label: &str, gl_ppm: &[u8], cpu_ppm: &[u8]) {
    for (kind, bytes) in [("gl", gl_ppm), ("cpu", cpu_ppm)] {
        println!("{}", frame_marker(label, kind, "begin"));
        for line in wrap_base64(&base64_encode(bytes)) {
            println!("{line}");
        }
        println!("{}", frame_marker(label, kind, "end"));
    }
}

/// Strip GitHub's own log-fetch line prefix, if present:
/// `<job>\t<step>\t<timestamp> <rest>` — the shape both `gh run view --log`
/// and the REST `.../logs` route the issue names emit, one line per log
/// line. The marker and base64 content this file prints never contains a
/// tab, so the **last** tab in a line is unambiguously the one GitHub
/// inserted before the timestamp; stripping through the first space after it
/// then drops the timestamp itself. A line with no tab (a plain local
/// transcript, saved with e.g. `| tee`) is returned unchanged — critically,
/// *without* also stripping to the first space, which would mangle a marker
/// line's own spaces (`=== preem-frame gauge.vfd.rest gl begin ===`).
fn strip_github_log_prefix(line: &str) -> &str {
    match line.rsplit_once('\t') {
        Some((_, after_tab)) => after_tab
            .split_once(' ')
            .map_or(after_tab, |(_, rest)| rest),
        None => line,
    }
}

/// Parse a (prefix-stripped) line as a `preem-frame` marker, returning
/// `(case, kind, state)`. `case` is whatever sits between `preem-frame ` and
/// the trailing `<kind> <state> ===`, so a case label may not itself contain
/// a space — true of every label [`label`] produces.
fn parse_marker(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line
        .strip_prefix("=== preem-frame ")?
        .strip_suffix(" ===")?;
    let mut parts = rest.rsplitn(3, ' ');
    let state = parts.next()?;
    let kind = parts.next()?;
    let case = parts.next()?;
    Some((case, kind, state))
}

/// Decode every `preem-frame` marker pair out of a job-log transcript (or a
/// plain local one — see [`strip_github_log_prefix`]), returning `(case,
/// kind, bytes)` triples in the order their `begin` markers appeared. The
/// inverse of [`dump_case_frames`].
fn decode_frame_log(text: &str) -> Result<Vec<(String, String, Vec<u8>)>, String> {
    let mut frames = Vec::new();
    let mut open: Option<(String, String, String)> = None;
    for raw_line in text.lines() {
        let line = strip_github_log_prefix(raw_line);
        match parse_marker(line) {
            Some((case, kind, "begin")) => {
                if let Some((open_case, open_kind, _)) = &open {
                    return Err(format!(
                        "nested begin marker for {case} {kind} while \
                         {open_case} {open_kind} was still open"
                    ));
                }
                open = Some((case.to_owned(), kind.to_owned(), String::new()));
            }
            Some((case, kind, "end")) => {
                let Some((open_case, open_kind, base64)) = open.take() else {
                    return Err(format!(
                        "end marker for {case} {kind} with no matching begin"
                    ));
                };
                if open_case != case || open_kind != kind {
                    return Err(format!(
                        "end marker {case} {kind} does not match open {open_case} {open_kind}"
                    ));
                }
                let bytes = base64_decode(&base64)?;
                frames.push((case.to_owned(), kind.to_owned(), bytes));
            }
            Some((case, kind, other)) => {
                return Err(format!("unknown marker state {other:?} for {case} {kind}"));
            }
            None => {
                if let Some((_, _, base64)) = &mut open {
                    base64.push_str(line.trim());
                }
            }
        }
    }
    if let Some((case, kind, _)) = open {
        return Err(format!(
            "unterminated {case} {kind} block — missing an end marker"
        ));
    }
    Ok(frames)
}

/// `--decode [--out DIR] [LOGFILE]` — reads `LOGFILE`, or stdin if none is
/// given, decodes every marker pair with [`decode_frame_log`], and writes
/// `<case>.gl.ppm`/`<case>.cpu.ppm` under `DIR` (`frames/` if `--out` is not
/// given).
fn run_decode(args: &[String]) -> Result<(), String> {
    let mut out_dir = std::path::PathBuf::from("frames");
    let mut path: Option<&str> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--out" => {
                let dir = rest.next().ok_or("--out needs a directory argument")?;
                out_dir = std::path::PathBuf::from(dir);
            }
            other if path.is_none() => path = Some(other),
            other => return Err(format!("unexpected argument {other:?}")),
        }
    }
    let text = if let Some(path) = path {
        std::fs::read_to_string(path).map_err(|why| format!("reading {path}: {why}"))?
    } else {
        use std::io::Read as _;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|why| format!("reading stdin: {why}"))?;
        buf
    };
    let frames = decode_frame_log(&text)?;
    if frames.is_empty() {
        println!("no preem-frame markers found — nothing to decode");
        return Ok(());
    }
    std::fs::create_dir_all(&out_dir)
        .map_err(|why| format!("creating {}: {why}", out_dir.display()))?;
    for (case, kind, bytes) in &frames {
        let file = out_dir.join(format!("{case}.{kind}.ppm"));
        std::fs::write(&file, bytes).map_err(|why| format!("writing {}: {why}", file.display()))?;
        println!("wrote {} ({} bytes)", file.display(), bytes.len());
    }
    Ok(())
}

/// #1151: `activate` can fire twice on one process — measured on `origin/main`
/// (10 runs against a real session bus, 6 more with the bus env cleared) as
/// zero reproductions locally, which is consistent with the issue's own
/// "intermittently" framing. What did settle where the second call comes
/// from: `connect_activate` in `main` is the *only* place in this file that
/// can trigger the signal — nothing here calls `.activate()` or emits it by
/// name — so a second call has to be `GApplication`'s own machinery, not this
/// harness re-entering itself. [`activate_once`] is what makes the second
/// call a no-op regardless of which of `GApplication`'s internal paths ends
/// up producing it.
///
/// `cargo test` does not run `#[test]`s inside an example by default
/// (examples default to `test = false`, the same reason `preem_gl::parity`
/// lives in the shell's own tree rather than inline here) — but an *explicit*
/// `--example preem_gl_diff` selector overrides that and compiles this module
/// with `cfg(test)`, the way any other test target would: `cargo test -p
/// trollshell --example preem_gl_diff --features system-tests` runs it.
#[cfg(test)]
mod activate_latch_tests {
    use super::activate_once;
    use std::cell::Cell;

    /// **The first call runs; every call after it is a no-op.**
    ///
    /// Falsified by deleting the latch (`activate_once` always returning
    /// `true`): the second `assert!` goes red, the same shape a live run
    /// would show as two `-- summary --` blocks instead of one.
    #[test]
    fn the_second_activate_is_latched() {
        let latch = Cell::new(false);
        assert!(activate_once(&latch), "the first call must run");
        assert!(!activate_once(&latch), "the second call must be a no-op");
        assert!(!activate_once(&latch), "a third call is still a no-op");
    }
}

/// #1310: the failing-case evidence dump and its decoder. Everything here is
/// pure and GL-free — no capture, no display — the same shape
/// `activate_latch_tests` above is, and runs for the same reason (`cargo
/// test -p trollshell --example preem_gl_diff`, an explicit `--example`
/// selector, is what turns this example's `#[test]`s on at all).
#[cfg(test)]
mod frame_dump_tests {
    use super::{
        FRAME_DUMP_CAP, base64_decode, base64_encode, decode_frame_log, frame_marker,
        should_dump_frames, wrap_base64,
    };
    use std::cell::Cell;

    /// A small synthetic frame: a real PPM header plus pixel bytes that span
    /// the full `0x00..=0xff` range, so the round trip is exercised on binary
    /// data and not just printable ASCII. The decoder never parses netpbm —
    /// it only has to move bytes — so nothing here needs a real image.
    fn synthetic_ppm(seed: u8) -> Vec<u8> {
        let mut bytes = b"P6\n2 2\n255\n".to_vec();
        for i in 0..12u16 {
            bytes.push(seed.wrapping_add(u8::try_from(i * 23).unwrap_or(0)));
        }
        bytes
    }

    /// What [`super::dump_case_frames`] would print for one case, built the
    /// same way it is, so the roundtrip test below feeds
    /// [`decode_frame_log`] exactly what a real failing run would.
    fn dump_to_string(label: &str, gl: &[u8], cpu: &[u8]) -> String {
        let mut out = String::new();
        for (kind, bytes) in [("gl", gl), ("cpu", cpu)] {
            out.push_str(&frame_marker(label, kind, "begin"));
            out.push('\n');
            for line in wrap_base64(&base64_encode(bytes)) {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str(&frame_marker(label, kind, "end"));
            out.push('\n');
        }
        out
    }

    /// **The decoder recovers byte-identical frames**, in order, for both
    /// halves of a case.
    ///
    /// Falsified (see the PR body for the paste) by corrupting
    /// [`wrap_base64`] to drop the last character of each wrapped line: the
    /// `assert_eq!` on the recovered bytes goes red instead of the decode
    /// merely erroring, which is what proves this test looks at the
    /// **bytes**, not just "did decoding not crash".
    #[test]
    fn the_decoder_recovers_byte_identical_frames() {
        let gl = synthetic_ppm(0x10);
        let cpu = synthetic_ppm(0xa0);
        let log = dump_to_string("scope.crt.idle0", &gl, &cpu);
        let frames = decode_frame_log(&log).expect("well-formed markers must decode");
        assert_eq!(
            frames,
            vec![
                ("scope.crt.idle0".to_owned(), "gl".to_owned(), gl),
                ("scope.crt.idle0".to_owned(), "cpu".to_owned(), cpu),
            ]
        );
    }

    /// A line carrying GitHub's own log-fetch prefix (`gh run view --log`'s
    /// shape: `<job>\t<step>\t<timestamp> <rest>`) decodes exactly like the
    /// unprefixed transcript — including on a marker line, whose own spaces
    /// must survive the strip.
    #[test]
    fn a_github_log_prefix_is_stripped_before_matching() {
        let gl = synthetic_ppm(0x01);
        let cpu = synthetic_ppm(0xfe);
        let log = dump_to_string("gauge.vfd.rest.x2", &gl, &cpu);
        let prefixed: String = log
            .lines()
            .map(|line| format!("flake-check\tsystem-tests\t2026-09-15T10:00:00.0000000Z {line}\n"))
            .collect();
        let frames = decode_frame_log(&prefixed).expect("a GH-prefixed log must still decode");
        assert_eq!(
            frames,
            vec![
                ("gauge.vfd.rest.x2".to_owned(), "gl".to_owned(), gl),
                ("gauge.vfd.rest.x2".to_owned(), "cpu".to_owned(), cpu),
            ]
        );
    }

    /// **A passing verdict never dumps and never touches either counter** —
    /// the rule that keeps a green run's log from growing at all.
    #[test]
    fn a_pass_verdict_emits_no_markers() {
        let dumped = Cell::new(0);
        let skipped = Cell::new(0);
        for _ in 0..3 {
            assert!(!should_dump_frames(true, &dumped, &skipped));
        }
        assert_eq!(dumped.get(), 0, "a pass must never count toward the cap");
        assert_eq!(
            skipped.get(),
            0,
            "a pass must never count toward the overflow line"
        );
    }

    /// **The cap**: the first [`FRAME_DUMP_CAP`] failing cases in a run are
    /// dumped, and every one after that is counted in `skipped` instead —
    /// the number [`super::Runner::summary`]'s "not dumped" line prints.
    #[test]
    fn failing_cases_past_the_cap_are_counted_not_dumped() {
        let dumped = Cell::new(0);
        let skipped = Cell::new(0);
        let extra = 3;
        let dumped_count = (0..FRAME_DUMP_CAP + extra)
            .filter(|_| should_dump_frames(false, &dumped, &skipped))
            .count();
        assert_eq!(u32::try_from(dumped_count).unwrap_or(0), FRAME_DUMP_CAP);
        assert_eq!(dumped.get(), FRAME_DUMP_CAP);
        assert_eq!(skipped.get(), extra);
    }

    /// A base64 block one character short of a full group is rejected rather
    /// than silently decoding to truncated bytes.
    #[test]
    fn a_truncated_base64_block_is_rejected() {
        let mut broken = base64_encode(&synthetic_ppm(0x55));
        broken.pop();
        assert!(base64_decode(&broken).is_err());
    }

    /// An `end` marker with no `begin` — e.g. a job log the runner cut off
    /// mid-block — is a decode error, not a silently-empty result.
    #[test]
    fn an_unopened_end_marker_is_rejected() {
        let log = format!("{}\n", frame_marker("scope.crt.idle0", "gl", "end"));
        assert!(decode_frame_log(&log).is_err());
    }
}
