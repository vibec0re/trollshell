//! `gl_probe` — the stage-A GL renderer probe for issue #886.
//!
//! Run it **inside a live Niri session**; it opens a window (or, with
//! `--layer`, a layer-shell surface) and prints a copy-pasteable transcript of
//! `PASS`/`FAIL`/`INFO`/`STAT` lines answering the questions #886 stage A asks:
//!
//! * Does `GtkGLArea` get a GL context at all here, which API (GL vs GLES) and
//!   version, and do sibling areas *share* one context or get one each?
//! * What does layer-shell + fractional scaling do to the surface scale a
//!   `GtkGLArea` renders at?
//! * Is the deprecated `GskGLShader` path still alive on this stack's GSK
//!   renderer? (Compiled for real against the live renderer — the definitive
//!   answer, not a docs reading.)
//! * What does a bar's worth of small `GtkGLArea`s cost per frame, versus one
//!   area, versus an equivalent count of animated cairo `GtkDrawingArea`s (the
//!   CPU path `hytte-preem` uses today)?
//! * Does the stack advertise `GL_KHR_robustness` /
//!   `EGL_EXT_create_context_robustness` — #893's hard prerequisite?
//!
//! ```sh
//! nix develop --command cargo run -p hytte-ui --example gl_probe
//! nix develop --command cargo run -p hytte-ui --example gl_probe -- --layer
//! ```
//!
//! # Why this probe issues no OpenGL calls
//!
//! The workspace sets `unsafe_code = "forbid"` (root `Cargo.toml`), and every
//! raw-GL binding — the `gl` crate `gdk4` already carries, `epoxy`, `glow` —
//! exposes `glGetString`/`glClear`/… as `unsafe fn`. `forbid` cannot be locally
//! lifted, and weakening the workspace lint for a probe is not on the table
//! (`hytte-ecal` is the one documented exception, and it exists to wrap a C
//! library, not to draw triangles). So this probe takes the "reduce to what is
//! reachable safely" option: it never calls into GL itself.
//!
//! That costs exactly two things and nothing else:
//!
//! * The areas draw no test pattern — GTK does not clear the framebuffer for
//!   you and its initial contents are transparent, so expect the areas to look
//!   empty. That is correct, not a failure. What is measured is the
//!   *integration* cost, which is the part #886 is actually unsure about:
//!   framebuffer allocation, `make_current`, the texture import, and GSK
//!   compositing, once per area per frame.
//! * `GL_VENDOR`/`GL_RENDERER`/`GL_EXTENSIONS` cannot be read out of the live
//!   context. The probe substitutes two things: it shells out to
//!   `eglinfo`/`es2_info`/`glxinfo` when one is on `PATH` (same driver, so the
//!   extension *inventory* is the same), and `--inspector` opens GTK's own
//!   inspector, whose General page shows the live context's vendor, renderer,
//!   version and full EGL extension list.

use std::cell::{Cell, RefCell};
use std::process::Command;
use std::rc::Rc;

use gtk::gdk;
use gtk::glib;
use gtk::gsk;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, Layer, LayerShell};

/// GSK's shader ABI: `mainImage` with GSK's fixed argument list. Compiling this
/// against the live `GskRenderer` is the on-glass answer to "is `GskGLShader`
/// still usable on our GTK".
const GSK_SHADER_SRC: &str = "\
void mainImage(out vec4 fragColor, in vec2 fragCoord, in vec2 resolution, in vec2 uv) {
  fragColor = vec4(uv.x, uv.y, 0.5, 1.0);
}
";

/// External tools that can dump the driver's EGL/GL extension inventory. Tried
/// in order; the first one on `PATH` wins.
const EXTENSION_TOOLS: &[&str] = &["eglinfo", "es2_info", "glxinfo"];

fn main() -> glib::ExitCode {
    let cfg = match Config::from_args() {
        Ok(cfg) => cfg,
        Err(usage) => {
            println!("{usage}");
            return glib::ExitCode::FAILURE;
        }
    };

    print_header(&cfg);
    // Deliberately before GTK starts: this half of the transcript survives even
    // if the GTK half dies (see `--skip-glshader`).
    probe_robustness_extensions();

    let app = gtk::Application::builder()
        .application_id("mov.vibec0re.hytte.gl-probe")
        .build();
    let activate_cfg = cfg.clone();
    app.connect_activate(move |app| activate(app, &activate_cfg));
    // The probe parses its own argv; hand GTK an empty one so it doesn't choke
    // on `--areas`/`--layer`.
    app.run_with_args::<&str>(&[])
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Which `GdkGLAPI`s the areas are allowed to realize with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiChoice {
    /// Leave GDK's default (whatever the display negotiated).
    Any,
    /// Desktop GL only.
    Gl,
    /// GLES only.
    Gles,
}

#[derive(Clone)]
struct Config {
    /// How many areas the "many" phases build. A bar's worth of preem chips.
    areas: usize,
    /// Measurement window per phase, seconds.
    seconds: f64,
    /// Put the content on a layer-shell surface instead of a normal window.
    layer: bool,
    /// Open GTK's inspector (its General page carries the live GL vendor /
    /// renderer / version / EGL extension list this probe cannot read itself).
    inspector: bool,
    /// Skip the `GskGLShader` compile attempt.
    skip_glshader: bool,
    /// Restrict the areas' GL API.
    api: ApiChoice,
}

const USAGE: &str = "\
gl_probe — #886 stage-A GL renderer probe

USAGE:
  cargo run -p hytte-ui --example gl_probe -- [OPTIONS]

OPTIONS:
  --areas N          areas built by the many-area phases   [default: 3]
  --seconds S        measurement window per phase          [default: 5]
  --layer            host the content on a layer-shell surface
  --api gl|gles|any  restrict the areas' allowed GL API    [default: any]
  --inspector        open GTK's inspector (live GL vendor/renderer/extensions)
  --skip-glshader    skip the GskGLShader compile attempt
  -h, --help         this text";

impl Config {
    fn from_args() -> Result<Self, String> {
        let mut cfg = Self {
            areas: 3,
            seconds: 5.0,
            layer: false,
            inspector: false,
            skip_glshader: false,
            api: ApiChoice::Any,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--areas" => {
                    let v = args
                        .next()
                        .ok_or_else(|| format!("--areas needs a value\n\n{USAGE}"))?;
                    cfg.areas = v
                        .parse()
                        .map_err(|_| format!("--areas: not a number: {v}\n\n{USAGE}"))?;
                }
                "--seconds" => {
                    let v = args
                        .next()
                        .ok_or_else(|| format!("--seconds needs a value\n\n{USAGE}"))?;
                    cfg.seconds = v
                        .parse()
                        .map_err(|_| format!("--seconds: not a number: {v}\n\n{USAGE}"))?;
                }
                "--layer" => cfg.layer = true,
                "--inspector" => cfg.inspector = true,
                "--skip-glshader" => cfg.skip_glshader = true,
                "--api" => {
                    let v = args
                        .next()
                        .ok_or_else(|| format!("--api needs a value\n\n{USAGE}"))?;
                    cfg.api = match v.as_str() {
                        "any" => ApiChoice::Any,
                        "gl" => ApiChoice::Gl,
                        "gles" => ApiChoice::Gles,
                        other => {
                            return Err(format!(
                                "--api: expected gl|gles|any, got {other}\n\n{USAGE}"
                            ));
                        }
                    };
                }
                "-h" | "--help" => return Err(USAGE.to_owned()),
                other => return Err(format!("unknown argument: {other}\n\n{USAGE}")),
            }
        }
        if cfg.areas == 0 {
            return Err(format!("--areas must be >= 1\n\n{USAGE}"));
        }
        Ok(cfg)
    }

    fn allowed_apis(&self) -> Option<gdk::GLAPI> {
        match self.api {
            ApiChoice::Any => None,
            ApiChoice::Gl => Some(gdk::GLAPI::GL),
            ApiChoice::Gles => Some(gdk::GLAPI::GLES),
        }
    }
}

// ---------------------------------------------------------------------------
// Transcript primitives
// ---------------------------------------------------------------------------

fn line(status: &str, key: &str, value: &str) {
    println!("{status:<4}  {key}: {value}");
}

fn pass(key: &str, value: &str) {
    line("PASS", key, value);
}

fn fail(key: &str, value: &str) {
    line("FAIL", key, value);
}

fn info(key: &str, value: &str) {
    line("INFO", key, value);
}

fn stat(key: &str, value: &str) {
    line("STAT", key, value);
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_owned())
}

fn print_header(cfg: &Config) {
    println!("=== gl_probe — #886 stage A ===");
    println!(
        "legend: PASS = capability present / check succeeded · FAIL = check failed \
         (a finding, not a crash) · INFO = measured fact, no pass/fail · \
         STAT = frame-timing measurement"
    );
    println!();
    info(
        "probe.args",
        &std::env::args().skip(1).collect::<Vec<_>>().join(" "),
    );
    info(
        "probe.config",
        &format!(
            "areas={} seconds={} layer={} api={} inspector={} skip_glshader={}",
            cfg.areas,
            cfg.seconds,
            cfg.layer,
            match cfg.api {
                ApiChoice::Any => "any",
                ApiChoice::Gl => "gl",
                ApiChoice::Gles => "gles",
            },
            cfg.inspector,
            cfg.skip_glshader,
        ),
    );
    info(
        "gtk.runtime_version",
        &format!(
            "{}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version()
        ),
    );
    info("gtk.rs_binding", "gtk4 0.11.2, features = [v4_14]");
    info(
        "env.XDG_SESSION_TYPE",
        &env_or("XDG_SESSION_TYPE", "<unset>"),
    );
    info("env.WAYLAND_DISPLAY", &env_or("WAYLAND_DISPLAY", "<unset>"));
    info("env.GSK_RENDERER", &env_or("GSK_RENDERER", "<unset>"));
    info("env.GDK_DEBUG", &env_or("GDK_DEBUG", "<unset>"));
    info("env.GDK_BACKEND", &env_or("GDK_BACKEND", "<unset>"));
    info("env.NIRI_SOCKET", &env_or("NIRI_SOCKET", "<unset>"));
    println!();
}

// ---------------------------------------------------------------------------
// Robustness (#893's hard prerequisite)
// ---------------------------------------------------------------------------

/// The GDK half of the robustness answer is static and needs no hardware: GTK
/// 4.22 has no robust-context API at all — no `GdkGLContext` property, no
/// `create-context` knob, and the string `robust` does not appear anywhere in
/// `libgtk-4.so`. So the driver may well support it while GDK never asks for
/// it. That distinction is the whole finding; print it next to whatever the
/// driver inventory says so the transcript carries both halves.
fn probe_robustness_extensions() {
    println!("-- robustness (#893 prerequisite) --");
    info(
        "gdk.robust_context_api",
        "absent — GdkGLContext exposes no robustness/reset-notification knob in \
         GTK 4.22 (see the #886 stage-A comment)",
    );

    let Some((tool, stdout)) = run_extension_tool() else {
        info(
            "driver.extension_tool",
            &format!("none of {} found on PATH", EXTENSION_TOOLS.join("/")),
        );
        info(
            "driver.extension_manual",
            "run: nix shell nixpkgs#mesa-demos --command eglinfo | grep -i robust",
        );
        println!();
        return;
    };

    info("driver.extension_tool", &tool);
    let mut matches: Vec<String> = stdout
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|tok| tok.to_ascii_lowercase().contains("robust"))
        .map(str::to_owned)
        .collect();
    matches.sort_unstable();
    matches.dedup();

    report_extension(&matches, "EGL_EXT_create_context_robustness");
    report_extension(&matches, "GL_KHR_robustness");
    report_extension(&matches, "GL_ARB_robustness");
    if matches.is_empty() {
        info("driver.robustness_tokens", "<none>");
    } else {
        info("driver.robustness_tokens", &matches.join(" "));
    }
    println!();
}

fn report_extension(matches: &[String], ext: &str) {
    let key = format!("driver.{}", ext.to_ascii_lowercase());
    if matches.iter().any(|m| m == ext) {
        pass(&key, "advertised");
    } else {
        fail(&key, "not advertised by the extension tool's output");
    }
}

fn run_extension_tool() -> Option<(String, String)> {
    for tool in EXTENSION_TOOLS {
        let Ok(out) = Command::new(tool).output() else {
            continue;
        };
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        if text.is_empty() {
            continue;
        }
        return Some(((*tool).to_owned(), text));
    }
    None
}

// ---------------------------------------------------------------------------
// Frame timing
// ---------------------------------------------------------------------------

/// Microseconds → milliseconds. Frame intervals are microsecond magnitudes far
/// below 2^53, so the cast is exact for anything this probe can observe.
#[allow(clippy::cast_precision_loss)]
fn ms(us: i64) -> f64 {
    us as f64 / 1000.0
}

#[derive(Default)]
struct FrameStats {
    intervals_us: Vec<i64>,
    last_us: Option<i64>,
}

impl FrameStats {
    fn record(&mut self, now_us: i64) {
        if let Some(prev) = self.last_us
            && now_us > prev
        {
            self.intervals_us.push(now_us - prev);
        }
        self.last_us = Some(now_us);
    }

    /// `frames avg p50 p95 max jank`, where *jank* counts intervals longer than
    /// twice the median — i.e. dropped frames.
    fn summary(&self) -> Option<String> {
        if self.intervals_us.is_empty() {
            return None;
        }
        let mut sorted = self.intervals_us.clone();
        sorted.sort_unstable();
        let len = i64::try_from(sorted.len()).unwrap_or(i64::MAX);
        let sum: i64 = sorted.iter().sum();
        let p = |pct: usize| sorted[(sorted.len() - 1) * pct / 100];
        let p50 = p(50);
        let jank = sorted.iter().filter(|d| **d > p50 * 2).count();
        Some(format!(
            "frames={} avg={:.2}ms p50={:.2}ms p95={:.2}ms max={:.2}ms jank={}",
            sorted.len(),
            ms(sum / len.max(1)),
            ms(p50),
            ms(p(95)),
            ms(sorted[sorted.len() - 1]),
            jank,
        ))
    }
}

// ---------------------------------------------------------------------------
// Phases
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Static widgets. The floor: what this surface costs per frame with
    /// nothing animating.
    Idle,
    /// N animated cairo `GtkDrawingArea`s — the CPU path `hytte-preem`
    /// rasterises through today.
    Cairo,
    /// One `GtkGLArea`, re-rendered every frame.
    GlOne,
    /// N `GtkGLArea`s, all re-rendered every frame.
    GlMany,
}

impl Phase {
    const ORDER: [Self; 4] = [Self::Idle, Self::Cairo, Self::GlOne, Self::GlMany];

    fn label(self, areas: usize) -> String {
        match self {
            Self::Idle => "idle".to_owned(),
            Self::Cairo => format!("cairo-x{areas}"),
            Self::GlOne => "gl-x1".to_owned(),
            Self::GlMany => format!("gl-x{areas}"),
        }
    }
}

/// What the current phase mounted, so the tick callback can drive it and the
/// phase teardown can interrogate it.
enum Content {
    Static,
    Cairo(Vec<gtk::DrawingArea>),
    Gl(Vec<GlSlot>),
}

struct GlSlot {
    area: gtk::GLArea,
    renders: Rc<Cell<u64>>,
}

struct Probe {
    cfg: Config,
    root: gtk::Box,
    banner: gtk::Label,
    /// Drives the cairo phase's animation.
    anim: Rc<Cell<f64>>,
    phase_idx: usize,
    phase_start_us: Option<i64>,
    stats: FrameStats,
    /// Frame intervals are only recorded after this instant, so a phase's
    /// first-frame allocation spike doesn't land in its own percentiles.
    settle_until_us: i64,
    content: Content,
    env_printed: bool,
    /// `(phase label, timing summary)`, replayed as one block at the end.
    summary: Vec<(String, String)>,
}

const SETTLE_US: i64 = 750_000;

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

fn activate(app: &gtk::Application, cfg: &Config) {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("hytte gl_probe (#886)")
        .default_width(760)
        .default_height(220)
        .build();

    if cfg.layer {
        // The layer + fractional-scale combination is the wrinkle #886 calls
        // out; anchoring top so it lands where a bar would.
        window.init_layer_shell();
        window.set_layer(Layer::Top);
        window.set_namespace(Some("hytte-gl-probe"));
        window.set_anchor(Edge::Top, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, true);
    }

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    outer.set_margin_top(8);
    outer.set_margin_bottom(8);
    outer.set_margin_start(8);
    outer.set_margin_end(8);
    let banner = gtk::Label::new(Some("gl_probe: starting…"));
    banner.set_xalign(0.0);
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    root.set_vexpand(true);
    outer.append(&banner);
    outer.append(&root);
    window.set_child(Some(&outer));

    // The probe holds its own widgets and the widgets hold the tick callback
    // that holds the probe — a cycle. Deliberate and bounded: the process
    // exits when the last phase closes the window.
    let probe = Rc::new(RefCell::new(Probe {
        cfg: cfg.clone(),
        root,
        banner,
        anim: Rc::new(Cell::new(0.0)),
        phase_idx: 0,
        phase_start_us: None,
        stats: FrameStats::default(),
        settle_until_us: 0,
        content: Content::Static,
        env_printed: false,
        summary: Vec::new(),
    }));

    let tick_probe = Rc::clone(&probe);
    window.add_tick_callback(move |win, clock| tick(&tick_probe, win, clock));

    window.present();

    if cfg.inspector {
        gtk::Window::set_interactive_debugging(true);
        info(
            "inspector",
            "opened — its General page lists the live GL vendor / renderer / \
             version and the full EGL extension list",
        );
    }
}

fn tick(
    probe: &Rc<RefCell<Probe>>,
    window: &gtk::ApplicationWindow,
    clock: &gdk::FrameClock,
) -> glib::ControlFlow {
    let now_us = clock.frame_time();
    let mut p = probe.borrow_mut();

    if !p.env_printed {
        p.env_printed = true;
        // The surface exists and has rendered at least once by the first tick,
        // so the renderer and the scale are both real values now.
        print_surface_environment(window, &p.cfg);
        let phase = Phase::ORDER[0];
        start_phase(&mut p, phase, now_us);
        return glib::ControlFlow::Continue;
    }

    if now_us >= p.settle_until_us {
        p.stats.record(now_us);
    }
    drive_phase(&p);

    let started = p.phase_start_us.unwrap_or(now_us);
    // A user-supplied duration in seconds; truncation to whole microseconds is
    // the intent, and the value is bounded by whatever anyone types on a CLI.
    #[allow(clippy::cast_possible_truncation)]
    let budget_us = (p.cfg.seconds * 1_000_000.0) as i64 + SETTLE_US;
    if now_us - started < budget_us {
        return glib::ControlFlow::Continue;
    }

    finish_phase(&mut p);
    p.phase_idx += 1;
    if let Some(next) = Phase::ORDER.get(p.phase_idx).copied() {
        start_phase(&mut p, next, now_us);
        glib::ControlFlow::Continue
    } else {
        print_summary(&p);
        window.close();
        glib::ControlFlow::Break
    }
}

fn start_phase(p: &mut Probe, phase: Phase, now_us: i64) {
    while let Some(child) = p.root.first_child() {
        p.root.remove(&child);
    }
    let label = phase.label(p.cfg.areas);
    p.banner.set_text(&format!("phase: {label}"));
    println!("-- phase {label} --");

    let content = match phase {
        Phase::Idle => {
            for i in 0..p.cfg.areas {
                let l = gtk::Label::new(Some(&format!("idle {i}")));
                l.set_size_request(140, 140);
                p.root.append(&l);
            }
            Content::Static
        }
        Phase::Cairo => Content::Cairo(build_cairo_areas(p)),
        Phase::GlOne => Content::Gl(build_gl_areas(p, 1)),
        Phase::GlMany => Content::Gl(build_gl_areas(p, p.cfg.areas)),
    };
    p.content = content;

    p.stats = FrameStats::default();
    p.phase_start_us = Some(now_us);
    p.settle_until_us = now_us + SETTLE_US;
}

fn build_cairo_areas(p: &Probe) -> Vec<gtk::DrawingArea> {
    (0..p.cfg.areas)
        .map(|i| {
            let da = gtk::DrawingArea::new();
            da.set_size_request(140, 140);
            let anim = Rc::clone(&p.anim);
            da.set_draw_func(move |_, cr, w, h| draw_test_pattern(cr, w, h, anim.get(), i));
            p.root.append(&da);
            da
        })
        .collect()
}

fn build_gl_areas(p: &Probe, count: usize) -> Vec<GlSlot> {
    (0..count)
        .map(|_| {
            let area = gtk::GLArea::new();
            area.set_size_request(140, 140);
            area.set_has_depth_buffer(false);
            area.set_has_stencil_buffer(false);
            area.set_auto_render(false);
            if let Some(apis) = p.cfg.allowed_apis() {
                area.set_allowed_apis(apis);
            }
            let renders = Rc::new(Cell::new(0_u64));
            let counter = Rc::clone(&renders);
            // No GL is issued here — see the module docs. The handler exists to
            // count how often GTK actually ran the render path, which is the
            // evidence that the per-frame cost below is real work.
            area.connect_render(move |_, _| {
                counter.set(counter.get() + 1);
                glib::Propagation::Stop
            });
            p.root.append(&area);
            GlSlot { area, renders }
        })
        .collect()
}

/// A moving bar + arc, cheap enough that the cairo phase measures widget
/// plumbing rather than a deliberately heavy paint.
fn draw_test_pattern(cr: &gtk::cairo::Context, w: i32, h: i32, t: f64, seed: usize) {
    let (fw, fh) = (f64::from(w), f64::from(h));
    // `seed` is a small area index; the cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let phase = t + seed as f64 * 0.4;
    cr.set_source_rgb(0.05, 0.05, 0.08);
    let _ = cr.paint();
    cr.set_source_rgb(0.2, 0.9, 0.6);
    let x = (phase.sin() * 0.5 + 0.5) * (fw - 12.0);
    cr.rectangle(x, 0.0, 12.0, fh);
    let _ = cr.fill();
    cr.set_source_rgb(0.9, 0.4, 0.2);
    cr.arc(fw / 2.0, fh / 2.0, fh / 4.0, phase, phase + 2.0);
    cr.set_line_width(4.0);
    let _ = cr.stroke();
}

fn drive_phase(p: &Probe) {
    match &p.content {
        Content::Static => {}
        Content::Cairo(areas) => {
            p.anim.set(p.anim.get() + 0.05);
            for a in areas {
                a.queue_draw();
            }
        }
        Content::Gl(slots) => {
            for s in slots {
                s.area.queue_render();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

fn print_surface_environment(window: &gtk::ApplicationWindow, cfg: &Config) {
    println!("-- surface & renderer --");
    info(
        "surface.kind",
        if cfg.layer { "layer-shell" } else { "toplevel" },
    );

    if let Some(surface) = window.surface() {
        info("surface.scale", &format!("{:.4}", surface.scale()));
        info("surface.scale_factor", &surface.scale_factor().to_string());
        let fractional = (surface.scale() - f64::from(surface.scale_factor())).abs() > f64::EPSILON;
        info(
            "surface.fractional_scale",
            if fractional {
                "yes — gdk_surface_get_scale() disagrees with the integer scale_factor"
            } else {
                "no — integer scale"
            },
        );
    } else {
        fail("surface", "window mapped without a gdk::Surface");
    }
    info("widget.scale_factor", &window.scale_factor().to_string());

    if let Some(display) = gdk::Display::default() {
        info("gdk.display_backend", display.type_().name());
        for m in display.monitors().iter::<gdk::Monitor>().flatten() {
            let g = m.geometry();
            info(
                "monitor",
                &format!(
                    "{} {}x{}+{}+{} scale={:.4} scale_factor={} refresh={}mHz",
                    m.connector().unwrap_or_else(|| glib::GString::from("?")),
                    g.width(),
                    g.height(),
                    g.x(),
                    g.y(),
                    m.scale(),
                    m.scale_factor(),
                    m.refresh_rate(),
                ),
            );
        }
    }

    let renderer = window.renderer();
    match &renderer {
        Some(r) => info("gsk.renderer", r.type_().name()),
        None => fail("gsk.renderer", "window has no GskRenderer"),
    }

    if cfg.skip_glshader {
        info("gsk.glshader_compile", "skipped (--skip-glshader)");
    } else if let Some(r) = renderer {
        probe_gsk_gl_shader(&r);
    }
    println!();
}

/// The definitive #886 question 1: `GskGLShader` is deprecated since GTK 4.16
/// *because the 4.14 rendering infrastructure never supported it*, but our
/// `gtk4-rs` feature set stops at `v4_14`, so rustc raises no deprecation warning
/// and the type compiles clean. Whether it still *works* is a runtime property
/// of the GSK renderer this session picked — so compile a real shader against
/// the real renderer and record the answer.
fn probe_gsk_gl_shader(renderer: &gsk::Renderer) {
    // If a GTK/mesa bug makes this abort rather than return an error, the
    // transcript up to here is the finding; rerun with --skip-glshader.
    info("gsk.glshader_compile", "attempting…");
    let shader = gsk::GLShader::from_bytes(&glib::Bytes::from_static(GSK_SHADER_SRC.as_bytes()));
    match shader.compile(renderer) {
        Ok(()) => pass(
            "gsk.glshader_compile",
            "compiled — the deprecated GskGLShader path is alive on this renderer",
        ),
        Err(e) => fail(
            "gsk.glshader_compile",
            &format!("{e} — GskGLShader is unusable here; GtkGLArea is the only GL path"),
        ),
    }
}

fn finish_phase(p: &mut Probe) {
    let label = Phase::ORDER[p.phase_idx].label(p.cfg.areas);
    let summary = p.stats.summary();
    match summary {
        Some(s) => {
            stat(&format!("timing.{label}"), &s);
            p.summary.push((label.clone(), s));
        }
        None => info(&format!("timing.{label}"), "no frames sampled"),
    }
    if let Content::Gl(slots) = &p.content {
        report_gl_contexts(slots);
    }
    println!();
}

fn report_gl_contexts(slots: &[GlSlot]) {
    let mut first_ctx: Option<gdk::GLContext> = None;
    for (i, slot) in slots.iter().enumerate() {
        let key = format!("glarea[{i}]");
        info(&format!("{key}.renders"), &slot.renders.get().to_string());
        // The fractional-scale wrinkle in one line. `gtk_gl_area_allocate_
        // buffers` sizes the framebuffer with the *integer*
        // `gtk_widget_get_scale_factor`, while the surface presents at
        // `gdk_surface_get_scale`'s fractional value — so on a 1.5x output the
        // area renders at 2x and GSK resamples the texture down. Print both
        // numbers so the transcript says whether that gap is live here.
        let surface_scale = slot
            .area
            .native()
            .and_then(|n| n.surface())
            .map_or(1.0, |s| s.scale());
        let widget_scale = slot.area.scale_factor();
        let (w, h) = (slot.area.width(), slot.area.height());
        info(
            &format!("{key}.alloc"),
            &format!(
                "{w}x{h} logical · framebuffer {}x{} px (integer scale {widget_scale}) · \
                 presented at surface scale {surface_scale:.4}{}",
                w * widget_scale,
                h * widget_scale,
                if (surface_scale - f64::from(widget_scale)).abs() > f64::EPSILON {
                    " — RESAMPLED (framebuffer scale != surface scale)"
                } else {
                    ""
                },
            ),
        );
        if let Some(err) = slot.area.error() {
            fail(&format!("{key}.context"), &err.to_string());
            continue;
        }
        let Some(ctx) = slot.area.context() else {
            fail(
                &format!("{key}.context"),
                "no GdkGLContext (area never realized)",
            );
            continue;
        };
        let (major, minor) = ctx.version();
        let (req_major, req_minor) = ctx.required_version();
        pass(
            &format!("{key}.context"),
            &format!(
                "created api={:?} version={major}.{minor} legacy={} required={req_major}.{req_minor} allowed={:?}",
                ctx.api(),
                ctx.is_legacy(),
                ctx.allowed_apis(),
            ),
        );
        match &first_ctx {
            None => first_ctx = Some(ctx),
            Some(first) => {
                let shared_key = format!("glarea[0<->{i}].shared");
                if first.is_shared(&ctx) {
                    pass(&shared_key, "contexts share resources");
                } else {
                    info(&shared_key, "contexts do NOT share resources");
                }
            }
        }
    }
    info(
        "glarea.context_count",
        &format!(
            "{} area(s) realized — one GdkGLContext each is GTK's model; \
             `shared` above says whether they can trade textures",
            slots.len()
        ),
    );
}

fn print_summary(p: &Probe) {
    println!("-- summary --");
    for (label, row) in &p.summary {
        stat(&format!("timing.{label}"), row);
    }
    info(
        "verdict.note",
        "compare gl-x1 vs gl-xN vs cairo-xN against idle: the delta is the \
         per-area, per-frame integration cost #886 asks about",
    );
    println!("=== gl_probe done — paste this transcript into issue #886 ===");
}
