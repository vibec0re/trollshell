//! `preem_gl_diff` — the GL/CPU parity harness for #893 stage B.
//!
//! Renders the same kit-widget state through **both** arms and prints the
//! per-channel delta, so the ceiling the spec proposes — mean ≤ 2/255,
//! p99 ≤ 8/255, max ≤ 32/255 — is a measurement rather than a hope.
//!
//! Five kinds since #1152, each four skins wide: the `Scope` (three fade
//! depths), the `Gauge` (three needle positions, plus one at the **shipping**
//! upscale), the `DotMatrix` (five displays, plus one stretched), the `Marquee`
//! (five scroll phases, plus one stretched) and the `TextBox` (four
//! configurations, plus one stretched) — **96** cases. Every case but the
//! stretched and shipping-upscale ones runs 1:1, where the GL arm's native grid
//! and the kit's logical one are the same number and the two can be compared
//! pixel against pixel; that is where `TROLLSHELL_PARITY_EXACT=1` pins all five
//! kinds at zero.
//!
//! What each kind's 1:1 cases *vary* is its own arithmetic. The dot matrix has
//! no `scale` on the widget at all — the dot pitch is its size knob (#1091) —
//! so its five vary the *line* and the *pitch* (see `DisplayAt`). The marquee's
//! five vary the **scroll phase**, because the phase is the widget: the shader
//! has no offset uniform (#839 made a sub-dot position inexpressible), so a
//! step is a different set of lit columns rather than a shifted sample (see
//! `TickerAt`). The text box's four vary the wrap, the upscale, the corner
//! radius and the palette pins (see `BubbleAt`).
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
//! the **80 1:1** cases came out **bit-exact**: max |Δ| 0 of 255 on R, G and B.
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
// The two text kinds (#1152). The marquee reaches `dot_matrix` through
// `super::dot_matrix::…` — it registers that very pipeline — which resolves
// here for the same reason `gauge`'s `super::program::…` does.
#[path = "../src/plugins/preem_gl/marquee.rs"]
mod marquee;
#[path = "../src/plugins/preem_gl/textbox.rs"]
mod textbox;

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
/// `MAX_DOT_PX` — see [`DisplayAt::Coarse`], which is there for the one
/// arithmetic the other four pitches cannot reach.
const COARSE_DOT_PX: u32 = 8;

/// How much bigger than the kit's buffer the **stretched** dot-matrix case
/// sizes its area — the one case that measures the improvement rather than the
/// agreement.
///
/// `GlSurface::measure` requests a minimum of `0` on both axes on purpose ("so
/// CSS/layout can scale the surface above its grid size, which is the whole LCD
/// look"), so this is a real configuration and not a contrived one: it is what
/// a readout in a container wider than its natural size gets. At `2` the GL arm
/// rasterises the dot lattice into four screen pixels per kit pixel; the
/// readback is box-averaged back down to the kit's grid before the comparison,
/// so what is measured is "the same picture, drawn with more samples".
///
/// It is deliberately **not** held to the bit-exact pin, nor to #893's
/// ceiling: it is box-averaged onto the kit's grid and then held to a
/// bit-identical interior plus an edge budget — see
/// [`Case::sampling`] and `parity::case_verdict`.
const STRETCH: u32 = 2;

/// The window the **marquee** cases run at, in final buffer pixels — wide
/// enough that a 22-cell grid fits at the default pitch (so a short message can
/// hold and a long one must scroll) and narrow enough to keep the whole
/// comparison on screen beside the other kinds.
const TICKER_WINDOW_PX: u32 = 96;

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
/// The underlying double-`activate` is **#1151**, not fixed here: this counter
/// is the detector, deliberately independent of whatever is causing a session
/// to end early, so it stays useful if the cause changes.
static VERDICTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

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
        /// How many times the area's size request exceeds the kit's buffer —
        /// see [`STRETCH`]. `1` for every case but the stretched one.
        stretch: u32,
    },
    /// A `Marquee` at one scroll phase (#1152).
    Marquee {
        style: kit::DisplayStyle,
        ticker: TickerAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
    },
    /// A `TextBox` in one configuration (#1152).
    TextBox {
        style: kit::DisplayStyle,
        bubble: BubbleAt,
        /// As [`Case::DotMatrix`]'s — `1` for every case but the stretched one.
        stretch: u32,
    },
}

/// What a marquee case is showing, and where the message has scrolled to.
///
/// Five, and the choice is the issue's: the degenerate display, the
/// [hold rule](kit::MarqueeStrip::scrolls), and a scrolling message at **three**
/// phases. Three rather than one because the whole widget is the phase — the
/// shader has no offset uniform at all (#839 made a sub-dot position
/// inexpressible, so a step is a different set of lit columns) — and because
/// the three chosen below are the three shapes the wrap can take.
#[derive(Clone, Copy)]
enum TickerAt {
    /// The empty string: the bezel and the **fixed** ghost grid, with nothing
    /// lit. `u_data_len` is the grid's width and every texel is `0`, which is
    /// the one case that separates "no strip" from "a blank strip" — the kit
    /// paints the ghost lattice either way, so a shader that refused to draw on
    /// an all-zero grid would go dark here and nowhere else.
    Empty,
    /// A message that fits the grid, so the kit **holds** it static and ignores
    /// the offset entirely. Left-aligned on the grid, and the phase below can
    /// never move it.
    Held,
    /// A message wider than the grid, at scroll phase `offset` **in dots**.
    ///
    /// The three the case list uses are `0` (the message's head at the grid's
    /// first column), `7` (mid-message, so every visible column is a glyph
    /// column of a *different* character than at `0`) and one that lands inside
    /// the **loop seam** — the blank gap the kit appends after the message —
    /// where the grid is part message and part nothing, which is the one phase
    /// where `window_columns`' `bitmap.get` returns `None` for some columns and
    /// not others.
    Scrolled(usize),
}

/// What a textbox case is showing.
///
/// Four, the issue's list: the degenerate box, one line, a message wrapped to
/// the last row it is allowed, and the pinned-palette configuration the pet's
/// and caw's bubbles actually ship in.
#[derive(Clone, Copy)]
enum BubbleAt {
    /// The empty string, hugging: the `max(1)`-wide degenerate buffer, no
    /// cells at all (`u_cols == 0`, no strip), and the **only** case at the
    /// kit's native `scale = 1` — so `u_upscale == 1` is rendered somewhere
    /// rather than only reasoned about.
    Empty,
    /// One short line at `scale = 2`, hugging — the pet's own bubble.
    OneLine,
    /// A sentence wrapped to exactly [`BUBBLE_LINES`] rows with the kit's
    /// trailing `…`, in a **fixed-width** slot: three lines of different
    /// lengths, so the block's blank padding cells are what keep every line
    /// after the first at the right strip offset.
    Wrapped,
    /// A pinned ink, an explicit `.notdef` and an uncovered char, over a corner
    /// cut wide enough to reach the glyph block — #884/#885's configuration
    /// (see [`PINNED_INK`] for the one pin it deliberately leaves out), and the
    /// one that renders the ink, the notdef box and the largest arc in one
    /// frame.
    Pinned,
}

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

/// What a dot-matrix case puts on the display.
///
/// Five, chosen to cover what the shader has to get right: the degenerate
/// buffer, the ordinary readout, the font's fallback path, and each end of the
/// pitch clamp — the one where the CRT comb has to be **re-phased** or it stops
/// being a raster (#1091), and the one where the vignette's band is a different
/// number of pixels than any other case makes it. Each is the same lattice
/// arithmetic at a different corner of it.
#[derive(Clone, Copy)]
enum DisplayAt {
    /// The empty string: bezel only, no strip, no lit pixel — `2*pad` × `9*dot`.
    /// The one case where `u_data_len` is `0` and the shader must draw the
    /// field rather than sample an unbound texture.
    ///
    /// **On the OLED this case carries no information, and that is worth
    /// knowing rather than hiding** (#1150 review, MEDIUM-3). The kit's frame
    /// there is 8×72 of pure black (`style.rs`'s OLED `bg` is `0, 0, 0` with no
    /// ghost), so both sides are flat *and* black: `verdict_for` excuses both
    /// blank guards by design — a flat reference is not evidence of an undrawn
    /// framebuffer — and every delta is 0 for any renderer that outputs black.
    /// Measured: under an all-black blit it is the one dot-matrix case that
    /// still says `PASS`. It is kept rather than special-cased because the
    /// other three skins' blank cases *do* detect, and a case list that varies
    /// by skin is a worse thing to reason about than one case that is
    /// vacuously green on one skin.
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
    /// The same readout at `MAX_DOT_PX`, the other end of the clamp — and the
    /// only case whose short side is **not** a multiple of both 8 and 9
    /// (#1150 review, MEDIUM-2).
    ///
    /// It is here for the CRT vignette rather than for the dots. The mask's
    /// edge ramp is `band = shortSide / BAND_DIV`, and `BAND_DIV` is one of the
    /// five constants the shader declared with no Rust counterpart. The review
    /// measured that `9 -> 8` shipped green across the whole tree, and the
    /// reason was arithmetic rather than a loose detector: every case's short
    /// side was `9 * dot` for `dot` in `{2, 4}`, and `36/9 == 36/8 == 4`,
    /// `18/9 == 18/8 == 2`. At `dot = 8` the short side is 72, where
    /// `72/9 == 8` and `72/8 == 9` — a one-pixel-wider ramp all the way round
    /// the bezel, against the kit's own composite, on a case that is pinned
    /// bit-exact. `3`, `5` and `7` do **not** work here and were checked:
    /// `27/8`, `45/8` and `63/8` all floor back onto `k`.
    ///
    /// It also gets the upper clamp rendered at all, which nothing did before.
    Coarse,
}

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

/// The phase that lands inside the loop seam. `TICKER_LONG` rasterises to 223
/// bitmap columns and the kit appends a `GLYPH_W + SPACING` gap, so a 22-cell
/// grid starting here straddles the message's tail and the blank seam.
const TICKER_SEAM_PHASE: usize = 215;

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
/// [`bubble_box`]'s reason.
fn ticker_strip(style: kit::DisplayStyle, ticker: TickerAt) -> kit::MarqueeStrip {
    let (text, _) = ticker.line();
    kit::Marquee::new(style)
        .window_px(TICKER_WINDOW_PX as usize)
        .dot_px(DOT_PX as usize)
        .render(text)
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
            Self::Marquee { .. } => parity::Kind::Marquee,
            Self::TextBox { .. } => parity::Kind::TextBox,
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
            Self::DotMatrix { stretch, .. }
            | Self::Marquee { stretch, .. }
            | Self::TextBox { stretch, .. }
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
            Self::Gauge { scale, .. } => (GAUGE_COLS, GAUGE_ROWS, *scale),
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
            } => {
                let strip = ticker_strip(*style, *ticker);
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

fn activate(app: &gtk::Application, skins: &[kit::DisplayStyle], exact: bool) {
    // The same registration `plugins::install` does in the shell, with the same
    // pipeline constant — the harness drives the shipping pipeline, not a copy.
    hytte::ui::gl_surface::register(program::SCOPE, program::SCOPE_PIPELINE);
    hytte::ui::gl_surface::register(gauge::GAUGE, gauge::GAUGE_PIPELINE);
    hytte::ui::gl_surface::register(dot_matrix::DOT_MATRIX, dot_matrix::DOT_MATRIX_PIPELINE);
    hytte::ui::gl_surface::register(marquee::MARQUEE, marquee::MARQUEE_PIPELINE);
    hytte::ui::gl_surface::register(textbox::TEXTBOX, textbox::TEXTBOX_PIPELINE);

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
    });

    glib::timeout_add_local(std::time::Duration::from_millis(60), {
        let area = area.clone();
        let app = app.clone();
        move || runner.step(&area, &app)
    });
}

/// Every case this run measures, in transcript order — four skins' worth of
/// each kind's own state list.
///
/// Split out of [`activate`] so the **case list** is one findable thing rather
/// than a third of a long function that also builds a window and arms a
/// timeout. `nix/checks/system-tests.nix` asserts this list's length by
/// counting evidence files; if you change it, change the number there in the
/// same commit.
fn cases_for(skins: &[kit::DisplayStyle]) -> Vec<Case> {
    skins
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
                DisplayAt::Coarse,
            ]
            .into_iter()
            .map(move |display| Case::DotMatrix {
                style: *style,
                display,
                stretch: 1,
            });
            // …and the same readout given more room than its natural size,
            // which is where the improvement actually lives (#1144).
            let stretched = std::iter::once(Case::DotMatrix {
                style: *style,
                display: DisplayAt::Readout,
                stretch: STRETCH,
            });
            // The ticker (#1152): the degenerate display, the hold rule, and a
            // scrolling message at three phases — the head, mid-message, and
            // one straddling the loop seam. See [`TickerAt`].
            let tickers = [
                TickerAt::Empty,
                TickerAt::Held,
                TickerAt::Scrolled(0),
                TickerAt::Scrolled(7),
                TickerAt::Scrolled(TICKER_SEAM_PHASE),
            ]
            .into_iter()
            .map(move |ticker| Case::Marquee {
                style: *style,
                ticker,
                stretch: 1,
            });
            // …and the same mid-message phase given more room than its natural
            // size, which is where this arm's improvement lives — the dot
            // lattice at the screen's resolution rather than a magnified table.
            let stretched_ticker = std::iter::once(Case::Marquee {
                style: *style,
                ticker: TickerAt::Scrolled(7),
                stretch: STRETCH,
            });
            // The bubble (#1152): the degenerate box, one line, a message
            // wrapped to the last row, and the pinned-palette configuration.
            let bubbles = [
                BubbleAt::Empty,
                BubbleAt::OneLine,
                BubbleAt::Wrapped,
                BubbleAt::Pinned,
            ]
            .into_iter()
            .map(move |bubble| Case::TextBox {
                style: *style,
                bubble,
                stretch: 1,
            });
            // …and the widest corner given more room than its natural size,
            // which is where *this* arm's improvement lives: the cut is an arc
            // at the screen's resolution rather than a replicated logical-pixel
            // mask, and `Pinned`'s radius-5 corner is the one that shows it.
            let stretched_bubble = std::iter::once(Case::TextBox {
                style: *style,
                bubble: BubbleAt::Pinned,
                stretch: STRETCH,
            });
            scopes
                .chain(gauges)
                .chain(shipping)
                .chain(displays)
                .chain(stretched)
                .chain(tickers)
                .chain(stretched_ticker)
                .chain(bubbles)
                .chain(stretched_bubble)
        })
        .collect()
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
            println!(
                "PASS all {} case(s) — {} 1:1 ones inside the proposed ceiling on every \
                 channel, {supersampled} box-averaged ones bit-identical off every edge \
                 and inside their edge budget",
                self.cases.len(),
                self.cases.len() - supersampled,
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
        // so the pinned cases keep the labels #1143's and #1144's transcripts
        // carry.
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
        } if *stretch > 1 => format!("marquee.{}.{}x{stretch}", style.name(), ticker.name()),
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
        Case::Marquee { style, ticker, .. } => {
            let strip = ticker_strip(*style, *ticker);
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
        Case::DotMatrix { style, display, .. } => {
            let (line, dot_px) = display.line();
            kit::DotMatrix::new(*style)
                .dot_px(dot_px as usize)
                .render(line)
        }
        // The kit's own window at this phase — the *same* strip the mapping
        // read its geometry off, so a disagreement here is a disagreement
        // between renderers and not between two tickers.
        Case::Marquee { style, ticker, .. } => {
            ticker_strip(*style, *ticker).window(ticker.line().1)
        }
        // The box's `scale` is already in these bytes (the kit upscales at the
        // end of `render`), which is why `reference_scale` has nothing to do
        // for this kind either — see `Case::geometry`.
        Case::TextBox { style, bubble, .. } => {
            bubble_box(*style, *bubble).render(bubble.spec().text)
        }
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
    print_channels(&stats, case.sampling());
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
