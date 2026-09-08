//! `GlSurface` — a `gtk::GLArea` subclass that runs a **host-registered**
//! shader pipeline over plain-data uniforms, for the reconciler's
//! [`Node::GlSurface`](crate::widget_tree::Node::GlSurface) (#893 stage B).
//!
//! # What this module knows, and what it deliberately does not
//!
//! It knows about *pipelines*: an ordered list of passes, each naming a
//! vertex/fragment pair, where it writes, what it reads, how it blends, and
//! which of two attribute-less draws it issues. It knows that some passes run
//! **once per animation step** over a ping-pong accumulator and some run
//! **once per render**, and it knows how to skip the step passes when nothing
//! has advanced.
//!
//! It knows nothing about `preem`, about an oscilloscope, or about phosphor.
//! The GLSL, the pipeline declaration and the state → uniform mapping all live
//! in `trollshell`'s `plugins::preem_gl`, registered here at shell startup
//! under a [`GlProgram`] name. That split is the whole reason
//! [`GlUniforms`] is a bag of named values and an optional `f32` strip rather
//! than a struct with a `persistence` field: this crate is the reconciler's,
//! not the kit's.
//!
//! # Why a new node instead of reading the GPU back
//!
//! The obvious way to reuse the existing machinery is to render into an FBO,
//! `glReadPixels` into an `Arc<[u8]>` and hand it to
//! [`Node::Pixels`](crate::widget_tree::Node::Pixels). That would leave the
//! reconciler untouched and defeat the entire point: a readback is a full
//! pipeline stall, per chip, per frame. So the frame reaches the reconciler as
//! **state**, and the GPU keeps the pixels.
//!
//! # The idempotence rule
//!
//! GTK calls `render` whenever it needs the texture — a resize, a
//! re-composite, a window remap — **not** only when we asked. A pipeline whose
//! step passes ran unconditionally would therefore decay a phosphor on every
//! re-composite, at a rate set by how often the compositor happened to ask.
//!
//! [`GlUniforms::step_seq`] closes that: it is a monotonic count of animation
//! steps since the state was built, the surface remembers the last one it drew,
//! and a render runs exactly `step_seq - last_drawn` step passes. Zero on a
//! repeat render, so it just re-runs the frame passes and re-blits. A
//! `step_seq` that went *backwards* means the host rebuilt the state from
//! scratch (a config change), so the accumulator is cleared and the count
//! restarts.
//!
//! # When the context fails
//!
//! `GtkGLArea` reports a failed context as `GLArea::error()` after realize, and
//! there is no way to ask in advance. So there is no process-wide probe: the
//! surface finds out when it realizes, latches
//! [`gl_abandoned`], and calls the host's
//! [`set_context_failure_handler`] hook exactly once. The host — which is the
//! only party that knows whether a CPU implementation exists — decides what to
//! do about it; `trollshell`'s preem renderer rebuilds every GL scope onto the
//! CPU kit inside the hook and asks for one re-map, rather than waiting for a
//! mapping pass that a settled widget's parked clock may never deliver.
//!
//! The latch is process-wide (thread-local on the GTK thread) once the *first*
//! failure is observed, which is one word narrower than the spec's "per
//! instance": a context failure is a property of the display connection, not of
//! a widget, so a second area asking would only fail the same way and pay for a
//! second failed context to find out. The *diagnostic* stays once-only either
//! way, which is what the spec's "warned once" was protecting.
//!
//! # Sizing and fractional scale
//!
//! The surface measures exactly like [`PixelSurface`](crate::PixelSurface): the
//! node's `width`/`height` are its natural size, the minimum is `0` on both
//! axes so CSS can scale it, and the height is requested aspect-locked for the
//! width it is offered. GTK then allocates the framebuffer at logical size ×
//! the **integer** `scale_factor`, exactly as `gtk_gl_area_allocate_buffers`
//! does, and the blit pass point-samples the logical grid into whatever it got
//! — the nearest-neighbour discipline `PixelSurface` uses, moved into a
//! fragment shader.
//!
//! The latent hazard the design spec documents rather than fixes: on a
//! fractionally-scaled output the `GLArea` renders at the next integer scale and
//! GSK resamples down, while `PixelSurface` (which never reads `scale_factor`)
//! does not. All three reference outputs are integer-scaled, so it has never
//! fired. If it ever does, snap chip sizes to the integer grid.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Most step passes one `render` will replay, however far behind it is.
///
/// The host bounds catch-up on its own side (`trollshell`'s
/// `MAX_CATCHUP_STEPS` = 8 per frame-clock tick), but that bound is per *tick*
/// while `last_drawn` is per *surface*, so it does not carry: a surface that
/// was unmapped for a minute while its state kept advancing comes back owing
/// the whole absence. Replaying that honestly would burn it in decay passes for
/// a trail that is fully faded either way, so it catches up as far as this and
/// skips the rest — the same trade `Steps::owed` makes upstream.
///
/// A **freshly built** accumulator is the other half of that story and is not
/// this constant's job: it has no trail to catch up to at all, and starts level
/// with the state instead. See [`fresh_last_drawn`].
const MAX_STEPS_PER_RENDER: u64 = 64;

/// Sampler uniform names, by input index. A fixed table rather than a
/// `format!` per pass per frame: this is the hot path, and four inputs is
/// already one more than any pipeline in the tree uses.
const SAMPLER_NAMES: [&str; 4] = ["u_tex0", "u_tex1", "u_tex2", "u_tex3"];

/// The GLSL version header prepended to every shader this module compiles.
///
/// `#version 320 es` because the surface pins [`gdk::GLAPI::GLES`] (see
/// [`GlSurface::new`]), which makes the dialect a decision rather than a
/// negotiation outcome — GDK negotiated GLES 3.2 on the reference hardware
/// anyway (#886), but "anyway" is not a contract a shader can be written
/// against.
///
/// The `precision` defaults are **required, not defensive**. ES gives a
/// fragment shader no default `float` precision at all (it is a compile error
/// to use one without declaring it) and defaults `int` to `mediump`, which is
/// only guaranteed ±32767 — and the `Scope`'s CRT mask alone squares a
/// ±1024 coordinate, reaching ~10⁶. Without `precision highp int` the mask
/// arithmetic is free to wrap on a conforming driver, silently, on exactly the
/// integer recurrences the bit-exactness argument rests on.
pub(crate) const GLSL_HEADER: &str =
    "#version 320 es\nprecision highp float;\nprecision highp int;\nprecision highp sampler2D;";

// ── the pipeline vocabulary ─────────────────────────────────────────────────

/// A registered shader pipeline's name.
///
/// A `Copy` newtype over a `&'static str` rather than an enum, and that is
/// deliberate: an enum listing the programs would have to live *here*, which
/// would make this crate name the kit widgets it is not supposed to know
/// about. A name is the narrowest thing that can travel on a
/// [`Node`](crate::widget_tree::Node) and still be `Copy`, `Eq` and `Hash`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlProgram(pub &'static str);

/// Where a pass writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlTarget {
    /// The ping-pong accumulator — the cross-frame state a stepping pipeline
    /// carries. A **step** pass targeting this writes the back buffer while
    /// [`GlInput::Accumulator`] reads the front; the pair is swapped once, at
    /// the end of each step, so several passes can build one step's result
    /// (a decay pass then a `Max`-blended stamp, say) without seeing each
    /// other's writes through the read slot.
    ///
    /// A **frame** pass may not target this — it would be a read-write feedback
    /// loop on the texture the next step reads — and one that does is skipped
    /// with a `debug_assert`.
    Accumulator,
    /// One of the pipeline's auxiliary grid-sized textures, by index. Scratch
    /// for multi-pass work that is *not* carried between frames — a separable
    /// blur's two halves, for instance.
    Aux(u8),
    /// The `GtkGLArea`'s own framebuffer, at the letterboxed fit rect. Always
    /// the last pass.
    Screen,
}

/// What a pass reads, bound to `u_tex0`, `u_tex1`, … in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlInput {
    /// The accumulator's **front** buffer — the previous step's result during
    /// a step pass, the current state during a frame pass.
    Accumulator,
    /// One of the auxiliary textures, by index.
    Aux(u8),
    /// The 1-D `R32F` strip built from [`GlUniforms::data`], `u_data_len`
    /// texels wide and one tall. Absent data binds a 1×1 zero texture, so a
    /// shader never samples an unbound unit.
    Data,
}

/// How a pass combines with what is already in its target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlBlend {
    /// Overwrite.
    Replace,
    /// Componentwise `max(src, dst)`. See `hytte_gl::Blend::Max` for why the
    /// accumulator must be a normalized format for this to exist at all.
    Max,
}

/// The geometry a pass draws, all of it generated in the vertex shader from
/// `gl_VertexID`/`gl_InstanceID` — there are no vertex buffers anywhere in
/// this module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlDraw {
    /// One oversized triangle covering the viewport. No diagonal seam, so no
    /// fragment is ever rasterised twice — which matters when the pass is an
    /// exact integer computation under a `Max` blend.
    FullScreen,
    /// One quad instance per **grid column** (`u_grid.x` of them), for a pass
    /// whose vertex shader resolves each column's own extent.
    PerColumn,
}

/// One shader pass.
#[derive(Clone, Copy, Debug)]
pub struct GlPass {
    /// Vertex shader body, **without** a `#version` line — see [`GLSL_HEADER`].
    pub vertex: &'static str,
    /// Fragment shader body, without a `#version` line.
    pub fragment: &'static str,
    /// Where it writes.
    pub target: GlTarget,
    /// What it reads, in `u_tex0…` order.
    pub inputs: &'static [GlInput],
    /// How it combines with the target.
    pub blend: GlBlend,
    /// What it draws.
    pub draw: GlDraw,
}

/// A registered pipeline: the passes, and how much scratch they need.
#[derive(Clone, Copy, Debug)]
pub struct GlPipeline {
    /// How many auxiliary grid-sized textures the passes address. Anything
    /// past this is a `debug_assert` and a skipped pass.
    pub aux: u8,
    /// Passes replayed once per animation step, in order, with the accumulator
    /// swapped at the end of each step.
    pub step: &'static [GlPass],
    /// Passes run once per render, after the steps, ending at
    /// [`GlTarget::Screen`].
    pub frame: &'static [GlPass],
}

/// One named uniform value.
///
/// A closed set rather than a `&[u8]` blob so the reconciler's structural
/// equality — which is what decides whether a re-render touches GTK at all —
/// is a real comparison and not a byte compare over padding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GlValue {
    /// `int`.
    Int(i32),
    /// `float`.
    Float(f32),
    /// `ivec2`.
    Ivec2([i32; 2]),
    /// `vec4`.
    Vec4([f32; 4]),
}

/// Everything one render needs that is not the pipeline: the uniforms, the
/// optional data strip, the logical grid, and how far the animation has run.
///
/// Plain data with a derived `PartialEq`, because the reconciler dedups on it —
/// see [`GlSurface::set_state`]. Producers are expected to hand only finite
/// floats (the shell's wire clamp guarantees that before a value ever reaches
/// here); a `NaN` would simply make the state unequal to itself and cost a
/// redundant `queue_render` per pass, which is the conservative failure.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct GlUniforms {
    /// Named scalar/vector uniforms, applied to **every** pass. A name a pass's
    /// program does not declare resolves to location `-1`, which GL ignores —
    /// so one bag can feed passes that each use a subset of it.
    ///
    /// The surface adds five of its own on top, which a bag must not shadow:
    /// `u_grid`, `u_viewport`, `u_data_len`, `u_step_back`, and the `u_tex0…`
    /// samplers.
    pub values: Vec<(&'static str, GlValue)>,
    /// The 1-D `R32F` data strip, uploaded as `u_data_len` texels. `None`
    /// binds a 1×1 zero texture and sets `u_data_len` to `0`.
    pub data: Option<Arc<[f32]>>,
    /// The **logical** grid the offscreen passes run at, `(cols, rows)` —
    /// pre-upscale, and the size of the accumulator and every auxiliary
    /// texture. Published to shaders as `u_grid`.
    pub grid: (u32, u32),
    /// Monotonic count of animation steps since this state was built. See the
    /// idempotence rule in the module docs.
    pub step_seq: u64,
}

// ── the program registry and the failure hook ───────────────────────────────

thread_local! {
    /// Pipelines by name, registered at host startup. GTK-main-thread-only,
    /// like every other table in this crate.
    static PROGRAMS: RefCell<HashMap<GlProgram, GlPipeline>> = RefCell::new(HashMap::new());

    /// The host's "a GL context could not be created" hook, called at most
    /// once for the process. Boxed rather than a `fn` pointer so a host can
    /// close over its own state.
    #[allow(clippy::type_complexity)]
    static ON_CONTEXT_FAILURE: RefCell<Option<Box<dyn Fn(&str)>>> = const { RefCell::new(None) };

    /// Latched once any surface fails to get a context. Never cleared — see
    /// the module docs on why this is process-wide rather than per instance.
    static ABANDONED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Register `pipeline` under `program`, replacing any previous registration.
///
/// Call once per program at host startup, on the GTK main thread, before any
/// [`Node::GlSurface`](crate::widget_tree::Node::GlSurface) naming it is
/// reconciled. A surface whose program is unregistered draws nothing and says
/// so once, rather than failing the render.
pub fn register(program: GlProgram, pipeline: GlPipeline) {
    PROGRAMS.with_borrow_mut(|programs| programs.insert(program, pipeline));
}

/// Install the host's context-failure hook — called with a human-readable
/// reason the first time any surface cannot get a GL context, and never again.
///
/// The hook is where a host decides what a failure *means*, which this crate
/// cannot: `trollshell` uses it to rebuild every GL `Scope` renderer onto the
/// CPU kit there and then, and to ask for one re-map — because a settled
/// widget's parked clock never delivers a mapping pass to do it on.
pub fn set_context_failure_handler(handler: impl Fn(&str) + 'static) {
    ON_CONTEXT_FAILURE.with_borrow_mut(|slot| *slot = Some(Box::new(handler)));
}

/// Whether GL has been abandoned for this process — see [`abandon_gl`].
#[must_use]
pub fn gl_abandoned() -> bool {
    ABANDONED.with_borrow(Option::is_some)
}

/// Abandon the GL path for this process, running the host's hook if this is the
/// first time.
///
/// Called by [`GlSurface`] itself when a context fails to create. Public
/// because it is also the seam a host tests its own CPU fallback through: a
/// display server with no GL is not something a hermetic test can arrange, but
/// "behave as if the context had failed" is exactly one call.
///
/// **The latch is set before the hook runs, and that ordering is load-bearing
/// — do not reverse it.** A host's handler will typically rebuild whatever it
/// had on GL, and it decides what to rebuild *onto* by asking
/// [`gl_abandoned`]. If the flag were only set afterwards, every one of those
/// rebuilds would resolve back to GL and the handler would silently accomplish
/// nothing. `trollshell`'s `preem_render::rebuild_gl_renderers_on_cpu` is
/// exactly that shape.
pub fn abandon_gl(reason: &str) {
    let first = ABANDONED.with_borrow_mut(|slot| {
        if slot.is_some() {
            return false;
        }
        *slot = Some(reason.to_owned());
        true
    });
    if !first {
        return;
    }
    tracing::warn!(
        reason,
        "no OpenGL context for a GlSurface; falling back to the CPU renderer for the rest of \
         this session (further occurrences are silenced)"
    );
    // Taken out of the slot for the duration of the call: a hook is arbitrary
    // host code and may (legitimately) re-enter this module.
    let handler = ON_CONTEXT_FAILURE.with_borrow_mut(Option::take);
    if let Some(handler) = handler {
        handler(reason);
        ON_CONTEXT_FAILURE.with_borrow_mut(|slot| *slot = Some(handler));
    }
}

/// What one render owes the accumulator: how many step passes to replay, and
/// whether to wipe it first.
///
/// **This is the idempotence rule, extracted so it can be tested without a GL
/// context** — which matters because CI has none, and because a repeat render
/// and a stepping one produce the same pixels, so nothing downstream could tell
/// them apart. Three cases:
///
/// - `step_seq == last_drawn` → **zero** passes. GTK re-rendering for a resize
///   or a re-composite must not advance the animation; without this the
///   phosphor would decay at whatever rate the compositor happened to ask.
/// - `step_seq > last_drawn` → that many passes, clamped to
///   [`MAX_STEPS_PER_RENDER`]. The clamp is the only thing bounding a surface
///   that was unmapped while its state kept advancing.
/// - `step_seq < last_drawn` → the host rebuilt the state from scratch (a
///   config change restarts the count), so the accumulator is stale: wipe it
///   and replay from zero rather than computing a negative count.
fn steps_owed(last_drawn: u64, step_seq: u64) -> (u64, bool) {
    if step_seq < last_drawn {
        return (step_seq.min(MAX_STEPS_PER_RENDER), true);
    }
    ((step_seq - last_drawn).min(MAX_STEPS_PER_RENDER), false)
}

/// What `last_drawn` starts at for a surface whose accumulator has just been
/// created (or recreated for a new grid): **level with the newest step**, so
/// the first render replays exactly one.
///
/// Not zero, and that is the whole point. A [`GlPipeline`]'s step passes are
/// replayed to reconstruct a *trail*, and a freshly-cleared accumulator has no
/// trail to reconstruct — but the host's state does not restart with the
/// surface. The renderer instance is shared across mounts while the surface is
/// per monitor, so a monitor hot-plug hands a brand-new surface a `step_seq`
/// that has been running for as long as the shell has. Starting at zero made
/// the first render replay [`MAX_STEPS_PER_RENDER`] steps against an empty
/// buffer, and a step pass whose per-step input has already scrolled out of the
/// state does not draw *nothing* — the `Scope`'s beam flatlines on the axis and
/// stamps it at full intensity — so the new monitor's chip opened with a bright
/// band across the centre, sixty-three times over, that the CPU arm never draws.
///
/// One step, because the newest one is the only one whose input the state still
/// carries. `step_seq == 0` (a state that has never advanced) stays at zero and
/// replays nothing.
fn fresh_last_drawn(step_seq: u64) -> u64 {
    step_seq.saturating_sub(1)
}

/// The largest buffer-aspect rect that fits `alloc`, centered — the letterbox
/// backstop, and the same rule
/// [`PixelSurface`](crate::PixelSurface)'s `fit_rect` applies, so a GL chip and
/// a raster chip in the same card pad identically rather than one of them
/// distorting.
///
/// Returns `(x, y, w, h)` in framebuffer pixels; a degenerate input yields a
/// zero rect, which draws nothing.
pub(crate) fn fit_rect(alloc_w: i32, alloc_h: i32, buf_w: u32, buf_h: u32) -> (i32, i32, u32, u32) {
    let (aw, ah) = (i64::from(alloc_w), i64::from(alloc_h));
    let (bw, bh) = (i64::from(buf_w), i64::from(buf_h));
    if aw <= 0 || ah <= 0 || bw <= 0 || bh <= 0 {
        return (0, 0, 0, 0);
    }
    // Scale to the tighter axis, in integers: `w = min(aw, ah * bw / bh)`.
    let (w, h) = if aw * bh <= ah * bw {
        (aw, aw * bh / bw)
    } else {
        (ah * bw / bh, ah)
    };
    let x = (aw - w) / 2;
    let y = (ah - h) / 2;
    (
        i32::try_from(x).unwrap_or(0),
        i32::try_from(y).unwrap_or(0),
        u32::try_from(w).unwrap_or(0),
        u32::try_from(h).unwrap_or(0),
    )
}

/// The identity previously-built [`imp::Resources`] must match `grid` and
/// `program` to be reused for a render — extracted out of `ensure_resources`
/// so the reuse decision itself is unit-tested without a GL context, the same
/// way [`steps_owed`] is.
///
/// **`grid` alone was the pre-#979 key.** `program` is an explicit mutable
/// prop — `GlSurface::set_state` accepts a new [`GlProgram`] and
/// `widget_tree`'s `update_in_place` repoints an existing `GlSurface` at it in
/// place — so a node that keeps its id and grid while changing `program` used
/// to draw the *old* pipeline's compiled shaders: latent while `preem_gl`
/// registers exactly one pipeline, live the moment a second one does (#979).
fn resources_reusable(
    built_grid: (u32, u32),
    built_program: GlProgram,
    grid: (u32, u32),
    program: GlProgram,
) -> bool {
    built_grid == grid && built_program == program
}

mod imp {
    use super::{
        GLSL_HEADER, GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram, GlTarget, GlUniforms,
        GlValue, PROGRAMS, SAMPLER_NAMES, abandon_gl, fit_rect, fresh_last_drawn, gdk, glib,
        resources_reusable, steps_owed,
    };
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use hytte_gl as hgl;
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;

    /// The GL objects one surface owns, all created against its own context.
    ///
    /// Programs are compiled **per surface** rather than shared across the
    /// display's share group. Contexts do share (#886 measured
    /// `glarea[0<->1].shared` PASS), and programs and textures are shareable
    /// objects, but VAOs and FBOs are not — so a shared cache would have to
    /// split the pipeline's objects into two lifetimes with two owners, to
    /// save a handful of milliseconds of one-time compilation at bar build.
    /// Not worth the class of bug it opens.
    struct Resources {
        programs: Vec<hgl::Program>,
        /// Index into `programs` for each step pass, then each frame pass.
        step_programs: Vec<usize>,
        frame_programs: Vec<usize>,
        /// The ping-pong pair; `front` is the index of the readable one.
        accumulator: [hgl::Texture; 2],
        front: usize,
        aux: Vec<hgl::Texture>,
        /// The 1-D `R32F` strip, sized to the last uploaded data length (at
        /// least 1×1, so a shader never samples an unbound unit).
        data: hgl::Texture,
        data_len: u32,
        /// Set when `data` holds exactly this allocation, so a re-render with
        /// the same `Arc` re-uploads nothing.
        data_source: Option<Arc<[f32]>>,
        framebuffer: hgl::Framebuffer,
        vao: hgl::VertexArray,
        /// The grid **and** the program these objects were compiled for —
        /// together the whole reuse key `ensure_resources` checks (see
        /// [`super::resources_reusable`]). `program` names *which* pipeline
        /// `programs` holds compiled shaders for; without it a program swap
        /// at a constant grid was indistinguishable from an unchanged render
        /// and kept the old pipeline's shaders (#979).
        grid: (u32, u32),
        program: GlProgram,
    }

    #[derive(Default)]
    pub struct GlSurface {
        /// The program name and uniforms last handed to `set_state`.
        program: Cell<Option<GlProgram>>,
        state: RefCell<Option<Arc<GlUniforms>>>,
        /// Natural (logical) size in pixels, honored by `measure`.
        nat_width: Cell<i32>,
        nat_height: Cell<i32>,
        /// GL objects, built on the first render that has a context.
        resources: RefCell<Option<Resources>>,
        /// The last `step_seq` the accumulator has been advanced to.
        last_drawn: Cell<u64>,
        /// One-shot latch for "this program is not registered".
        warned_unregistered: Cell<bool>,
        /// One-shot latch for "a pass would not compile", so a broken shader
        /// costs one journal line and not one per frame.
        warned_build: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for GlSurface {
        const NAME: &'static str = "HytteGlSurface";
        type Type = super::GlSurface;
        type ParentType = gtk::GLArea;
    }

    impl ObjectImpl for GlSurface {}

    impl WidgetImpl for GlSurface {
        /// Height-for-width, exactly like `PixelSurface`: the grid carries an
        /// aspect ratio and layout should request that *shape*, not a fixed box.
        fn request_mode(&self) -> gtk::SizeRequestMode {
            gtk::SizeRequestMode::HeightForWidth
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let bw = self.nat_width.get();
            let bh = self.nat_height.get();
            let natural = if orientation == gtk::Orientation::Horizontal {
                bw
            } else if for_size > 0 && bw > 0 {
                let h = i64::from(for_size) * i64::from(bh) / i64::from(bw);
                i32::try_from(h).unwrap_or(i32::MAX)
            } else {
                bh
            };
            // min = 0 on both axes so CSS/layout can scale the surface above
            // its grid size, which is the whole LCD look.
            (0, natural.max(0), -1, -1)
        }

        /// Realize through GTK, then find out whether it actually got a
        /// context. This is the only place that can know: `GtkGLArea` creates
        /// the context during realize and reports a failure as `error()`, with
        /// no way to ask beforehand.
        fn realize(&self) {
            self.parent_realize();
            let obj = self.obj();
            if let Some(error) = obj.error() {
                abandon_gl(&error.to_string());
            } else if obj.context().is_none() {
                abandon_gl("GtkGLArea realized without a GdkGLContext");
            }
        }

        /// Drop every GL object **before** handing back to GTK, with the
        /// context explicitly made current first.
        ///
        /// The handles' `Drop` calls `glDelete*`, which needs a current
        /// context. GTK makes one current inside its own `unrealize`, but that
        /// runs *after* this override's body, so the `make_current` here is
        /// what actually holds the crate's contract.
        fn unrealize(&self) {
            let obj = self.obj();
            if obj.error().is_none() && obj.context().is_some() {
                obj.make_current();
            }
            self.resources.replace(None);
            self.last_drawn.set(0);
            self.parent_unrealize();
        }
    }

    impl GLAreaImpl for GlSurface {
        fn render(&self, _context: &gdk::GLContext) -> glib::Propagation {
            self.draw();
            glib::Propagation::Stop
        }
    }

    impl GlSurface {
        /// Adopt a new program + state, returning whether GTK needs telling.
        ///
        /// `(needs_render, needs_resize)`. The dedup is the reconciler's
        /// contract: `to_ui_node` runs once per monitor per frame and re-maps
        /// every node in a mailbox whenever anything in it moves, so an
        /// unchanged surface must cost nothing at all — the same rule
        /// `PixelSurface`'s #902 guard follows, with `Arc::ptr_eq` as the fast
        /// path the shell's shared-state cache actually hits.
        pub(super) fn set_state(
            &self,
            program: GlProgram,
            width: u32,
            height: u32,
            state: &Arc<GlUniforms>,
        ) -> (bool, bool) {
            let same_program = self.program.get() == Some(program);
            let unchanged = same_program
                && self
                    .state
                    .borrow()
                    .as_ref()
                    .is_some_and(|held| Arc::ptr_eq(held, state) || **held == **state);
            let w = i32::try_from(width).unwrap_or(i32::MAX);
            let h = i32::try_from(height).unwrap_or(i32::MAX);
            let resized = (self.nat_width.get(), self.nat_height.get()) != (w, h);
            if unchanged && !resized {
                return (false, false);
            }
            self.program.set(Some(program));
            self.state.replace(Some(Arc::clone(state)));
            self.nat_width.set(w);
            self.nat_height.set(h);
            (true, resized)
        }

        /// The whole render: ensure resources, replay the outstanding steps,
        /// run the frame passes.
        fn draw(&self) {
            let Some(program) = self.program.get() else {
                return;
            };
            let state = self.state.borrow().clone();
            let Some(state) = state else { return };
            let Some(pipeline) = PROGRAMS.with_borrow(|programs| programs.get(&program).copied())
            else {
                if !self.warned_unregistered.replace(true) {
                    tracing::warn!(
                        program = program.0,
                        "no GL pipeline registered under that name; the surface draws nothing"
                    );
                }
                return;
            };
            let Ok(gl) = hgl::Gl::current() else {
                abandon_gl("the GL entry points could not be resolved");
                return;
            };
            // Before anything is written. GTK renders its own scene into this
            // same context and nothing promises what it leaves set, so dither,
            // scissor, colour mask and the depth/stencil tests are put into a
            // known position rather than inherited — see
            // `hytte_gl::reset_fixed_function_state` for what each one would
            // cost us. Five enum-only calls per render.
            hgl::reset_fixed_function_state(&gl);

            if !self.ensure_resources(&gl, &pipeline, program, state.grid, state.step_seq) {
                return;
            }
            let mut held = self.resources.borrow_mut();
            let Some(resources) = held.as_mut() else {
                return;
            };
            resources.upload_data(&gl, state.data.as_ref());

            // The idempotence rule, decided by `steps_owed` — see there.
            let (steps, reset) = steps_owed(self.last_drawn.get(), state.step_seq);
            if reset {
                resources.clear_accumulator(&gl);
            }
            // The steps replayed are always the **newest** `steps` of them, so
            // a surface that fell far enough behind to hit the clamp catches up
            // on what is current rather than on ancient history.
            for back in (0..steps).rev() {
                for (slot, pass) in pipeline.step.iter().enumerate() {
                    let program = resources.step_programs.get(slot).copied();
                    resources.run(&gl, pass, program, &state, self.obj().as_ref(), true, back);
                }
                resources.front = 1 - resources.front;
            }
            self.last_drawn.set(state.step_seq);

            for (slot, pass) in pipeline.frame.iter().enumerate() {
                let program = resources.frame_programs.get(slot).copied();
                resources.run(&gl, pass, program, &state, self.obj().as_ref(), false, 0);
            }
        }

        /// Build or re-size the GL objects; `false` means the surface cannot
        /// draw and the caller should give up for this frame.
        fn ensure_resources(
            &self,
            gl: &hgl::Gl,
            pipeline: &GlPipeline,
            program: GlProgram,
            grid: (u32, u32),
            step_seq: u64,
        ) -> bool {
            if let Some(resources) = self.resources.borrow().as_ref()
                && resources_reusable(resources.grid, resources.program, grid, program)
            {
                return true;
            }
            // A grid change **or** a program change means new textures /
            // freshly compiled shaders, which means a cleared accumulator —
            // so the step count restarts with it, at `fresh_last_drawn`
            // rather than at zero. See there, and see `resources_reusable`
            // for why `program` is part of this decision too (#979).
            match Resources::build(gl, pipeline, program, grid) {
                Ok(resources) => {
                    self.resources.replace(Some(resources));
                    self.last_drawn.set(fresh_last_drawn(step_seq));
                    true
                }
                Err(error) => {
                    if !self.warned_build.replace(true) {
                        tracing::warn!(
                            %error,
                            "a GL pipeline could not be built; the surface draws nothing \
                             (further occurrences are silenced)"
                        );
                    }
                    self.resources.replace(None);
                    false
                }
            }
        }
    }

    impl Resources {
        /// Compile every pass and allocate the grid-sized textures.
        fn build(
            gl: &hgl::Gl,
            pipeline: &GlPipeline,
            program: GlProgram,
            grid: (u32, u32),
        ) -> Result<Self, hgl::Error> {
            let (cols, rows) = (grid.0.max(1), grid.1.max(1));
            let mut programs = Vec::new();
            let mut compile = |pass: &GlPass| -> Result<usize, hgl::Error> {
                programs.push(hgl::Program::compile(
                    gl,
                    GLSL_HEADER,
                    pass.vertex,
                    pass.fragment,
                )?);
                Ok(programs.len() - 1)
            };
            let step_programs = pipeline
                .step
                .iter()
                .map(&mut compile)
                .collect::<Result<Vec<_>, _>>()?;
            let frame_programs = pipeline
                .frame
                .iter()
                .map(&mut compile)
                .collect::<Result<Vec<_>, _>>()?;
            let accumulator = [
                hgl::Texture::new(gl, hgl::Format::R8, cols, rows)?,
                hgl::Texture::new(gl, hgl::Format::R8, cols, rows)?,
            ];
            let aux = (0..pipeline.aux)
                .map(|_| hgl::Texture::new(gl, hgl::Format::R8, cols, rows))
                .collect::<Result<Vec<_>, _>>()?;
            let data = hgl::Texture::new(gl, hgl::Format::R32f, 1, 1)?;
            let resources = Self {
                programs,
                step_programs,
                frame_programs,
                accumulator,
                front: 0,
                aux,
                data,
                data_len: 0,
                data_source: None,
                framebuffer: hgl::Framebuffer::new(gl),
                vao: hgl::VertexArray::new(gl),
                grid: (cols, rows),
                program,
            };
            resources.clear_accumulator(gl);
            Ok(resources)
        }

        /// Zero both halves of the ping-pong pair — a fresh screen.
        fn clear_accumulator(&self, gl: &hgl::Gl) {
            for texture in &self.accumulator {
                if self.framebuffer.draw_to(gl, texture).is_ok() {
                    hgl::set_blend(gl, hgl::Blend::Replace);
                    hgl::clear(gl, [0.0, 0.0, 0.0, 0.0]);
                }
            }
        }

        /// Re-upload the data strip if it is not the allocation we already hold.
        fn upload_data(&mut self, gl: &hgl::Gl, data: Option<&Arc<[f32]>>) {
            let Some(data) = data else {
                self.data_source = None;
                self.data_len = 0;
                return;
            };
            if self
                .data_source
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, data))
            {
                return;
            }
            let len = u32::try_from(data.len()).unwrap_or(u32::MAX).max(1);
            if len != self.data_len.max(1) || self.data_len == 0 {
                let Ok(texture) = hgl::Texture::new(gl, hgl::Format::R32f, len, 1) else {
                    self.data_source = None;
                    self.data_len = 0;
                    return;
                };
                self.data = texture;
            }
            self.data.upload_f32(gl, data);
            self.data_len = u32::try_from(data.len()).unwrap_or(u32::MAX);
            self.data_source = Some(Arc::clone(data));
        }

        /// Run one pass with the program compiled for it.
        ///
        /// `program` is the index the caller's `enumerate` resolved out of
        /// `step_programs`/`frame_programs`; `None` (a pipeline that grew a
        /// pass since these resources were built) draws nothing rather than
        /// running a neighbouring pass's shader.
        ///
        /// `step_back` is how many steps before the newest this pass is
        /// replaying — `0` for the last one, and `0` for every frame pass. It
        /// reaches the shader as `u_step_back` so a pipeline whose per-step
        /// input differs between steps (a sample batch stamped once and then
        /// decayed, say) can tell them apart. Counted **backwards** so it never
        /// grows: an absolute step index would outrun the `int` a uniform
        /// carries after a few years of continuous animation.
        #[allow(clippy::too_many_arguments)]
        fn run(
            &self,
            gl: &hgl::Gl,
            pass: &GlPass,
            program: Option<usize>,
            state: &GlUniforms,
            area: &super::GlSurface,
            stepping: bool,
            step_back: u64,
        ) {
            let Some(program) = program.and_then(|slot| self.programs.get(slot)) else {
                return;
            };
            let target = match pass.target {
                GlTarget::Accumulator => {
                    debug_assert!(stepping, "a frame pass may not target the accumulator");
                    if !stepping {
                        return;
                    }
                    Some(&self.accumulator[1 - self.front])
                }
                GlTarget::Aux(slot) => {
                    let texture = self.aux.get(usize::from(slot));
                    debug_assert!(
                        texture.is_some(),
                        "pipeline addressed an undeclared aux slot"
                    );
                    texture
                }
                GlTarget::Screen => None,
            };
            let viewport = if let Some(texture) = target {
                if self.framebuffer.draw_to(gl, texture).is_err() {
                    return;
                }
                texture.size()
            } else {
                // GTK renders into its own FBO so GSK can import the result, so
                // "the default framebuffer" is emphatically not 0.
                // `attach_buffers` is the documented way back to it.
                area.attach_buffers();
                // The framebuffer is logical size × the **integer**
                // `scale_factor`, which is what `gtk_gl_area_allocate_buffers`
                // itself allocates — see the module docs on fractional scale.
                let scale = area.scale_factor().max(1);
                let alloc_w = area.width().saturating_mul(scale);
                let alloc_h = area.height().saturating_mul(scale);
                hgl::viewport(
                    gl,
                    0,
                    0,
                    u32::try_from(alloc_w).unwrap_or(0),
                    u32::try_from(alloc_h).unwrap_or(0),
                );
                // Clear the *whole* allocation before narrowing to the fit rect,
                // so the letterbox padding is transparent rather than whatever
                // the last frame left there. GTK does not clear for us.
                hgl::set_blend(gl, hgl::Blend::Replace);
                hgl::clear(gl, [0.0, 0.0, 0.0, 0.0]);
                let (x, y, w, h) = fit_rect(alloc_w, alloc_h, self.grid.0, self.grid.1);
                if w == 0 || h == 0 {
                    return;
                }
                hgl::viewport(gl, x, y, w, h);
                (w, h)
            };

            program.bind(gl);
            self.bind_inputs(gl, program, pass.inputs);
            program.set_ivec2(
                gl,
                "u_grid",
                [
                    i32::try_from(self.grid.0).unwrap_or(i32::MAX),
                    i32::try_from(self.grid.1).unwrap_or(i32::MAX),
                ],
            );
            program.set_ivec2(
                gl,
                "u_viewport",
                [
                    i32::try_from(viewport.0).unwrap_or(i32::MAX),
                    i32::try_from(viewport.1).unwrap_or(i32::MAX),
                ],
            );
            program.set_int(gl, "u_data_len", i32::try_from(self.data_len).unwrap_or(0));
            program.set_int(
                gl,
                "u_step_back",
                i32::try_from(step_back).unwrap_or(i32::MAX),
            );
            for (name, value) in &state.values {
                match *value {
                    GlValue::Int(v) => program.set_int(gl, name, v),
                    GlValue::Float(v) => program.set_float(gl, name, v),
                    GlValue::Ivec2(v) => program.set_ivec2(gl, name, v),
                    GlValue::Vec4(v) => program.set_vec4(gl, name, v),
                }
            }
            hgl::set_blend(
                gl,
                match pass.blend {
                    GlBlend::Replace => hgl::Blend::Replace,
                    GlBlend::Max => hgl::Blend::Max,
                },
            );
            self.vao.bind(gl);
            match pass.draw {
                GlDraw::FullScreen => hgl::draw_fullscreen(gl),
                GlDraw::PerColumn => hgl::draw_quads(gl, self.grid.0),
            }
            hgl::set_blend(gl, hgl::Blend::Replace);
        }

        /// Bind each declared input to its `u_texN` unit.
        fn bind_inputs(&self, gl: &hgl::Gl, program: &hgl::Program, inputs: &[GlInput]) {
            for (unit, input) in inputs.iter().enumerate() {
                let Some(name) = SAMPLER_NAMES.get(unit) else {
                    debug_assert!(false, "a pass declared more inputs than there are samplers");
                    return;
                };
                let texture = match *input {
                    GlInput::Accumulator => Some(&self.accumulator[self.front]),
                    GlInput::Aux(slot) => self.aux.get(usize::from(slot)),
                    GlInput::Data => Some(&self.data),
                };
                let Some(texture) = texture else {
                    debug_assert!(false, "a pass read an undeclared aux slot");
                    continue;
                };
                let unit = u32::try_from(unit).unwrap_or(0);
                texture.bind_unit(gl, unit);
                program.set_int(gl, name, i32::try_from(unit).unwrap_or(0));
            }
        }
    }

    // ── #979: a real GL context, so the actual rebuild decision is exercised ──
    //
    // Nested inside `mod imp` (rather than the file's outer `#[cfg(test)] mod
    // tests`) so it can call the private `ensure_resources` and read
    // `Resources::programs` directly — the reuse *decision* is covered
    // hermetically by `resources_reusable`'s own tests above; this is the one
    // place that proves `ensure_resources` actually wires that decision to a
    // real rebuild.
    #[cfg(all(test, feature = "system-tests"))]
    mod tests {
        use super::{GlBlend, GlDraw, GlPass, GlPipeline, GlProgram, GlSurface, GlTarget, gdk, hgl};
        use gtk::prelude::*;

        // This test never calls `run` — only `ensure_resources` /
        // `Resources::build` — so the two pipelines below only need to differ
        // in how many programs they compile; their passes are never drawn.
        const VERTEX: &str = "
            void main() {
                vec2 p = vec2(
                    float((gl_VertexID & 1) << 2) - 1.0,
                    float((gl_VertexID & 2) << 1) - 1.0
                );
                gl_Position = vec4(p, 0.0, 1.0);
            }";
        const FRAGMENT: &str = "
            out vec4 frag_color;
            void main() {
                frag_color = vec4(1.0);
            }";
        const PASS: GlPass = GlPass {
            vertex: VERTEX,
            fragment: FRAGMENT,
            target: GlTarget::Screen,
            inputs: &[],
            blend: GlBlend::Replace,
            draw: GlDraw::FullScreen,
        };
        const ONE_PASS: [GlPass; 1] = [PASS];
        const TWO_PASSES: [GlPass; 2] = [PASS, PASS];

        /// A real, made-current `GdkGLContext`, realized (never mapped/shown —
        /// `GtkGLArea` creates its context in `realize`, same as the module
        /// docs say) without needing a live main loop.
        ///
        /// `None` when this display cannot produce one. That is expected in
        /// the `nix flake check` sandbox, which ships no mesa at all (see the
        /// #893 design spec's "CI has no GL"); a local `xvfb-run` may or may
        /// not have a software GL driver available either.
        fn real_gl() -> Option<(gtk::Window, gtk::GLArea, hgl::Gl)> {
            let window = gtk::Window::new();
            let area = gtk::GLArea::new();
            area.set_allowed_apis(gdk::GLAPI::GLES);
            window.set_child(Some(&area));
            gtk::prelude::WidgetExt::realize(&window);
            area.realize();
            if area.error().is_some() {
                return None;
            }
            area.make_current();
            let gl = hgl::Gl::current().ok()?;
            Some((window, area, gl))
        }

        /// **#979.** `ensure_resources` must rebuild — not reuse — when the
        /// program changes at a constant grid. Registers two pipelines that
        /// compile a different number of programs, builds `Resources` for the
        /// first, then asks for the second at the *same* grid: the pre-#979
        /// code (keyed on `grid` alone) would have reported the first
        /// `Resources` reusable and kept its one compiled program.
        ///
        /// **Falsified** by reverting `ensure_resources`'s condition to
        /// `resources.grid == grid` (dropping the program from the key): the
        /// final assertion goes red, seeing `1` program instead of `2`.
        #[gtk::test]
        fn a_program_change_at_a_constant_grid_rebuilds_resources() {
            let Some((_window, _area, gl)) = real_gl() else {
                eprintln!(
                    "skipping a_program_change_at_a_constant_grid_rebuilds_resources: no GL \
                     context on this display — expected under the sandboxed `nix flake check` \
                     runner, which has no mesa in its closure"
                );
                return;
            };

            let one = GlPipeline {
                aux: 0,
                step: &[],
                frame: &ONE_PASS,
            };
            let two = GlPipeline {
                aux: 0,
                step: &[],
                frame: &TWO_PASSES,
            };
            let prog_one = GlProgram("gl_surface_test.one_pass");
            let prog_two = GlProgram("gl_surface_test.two_pass");

            let surface = GlSurface::default();
            assert!(
                surface.ensure_resources(&gl, &one, prog_one, (4, 4), 0),
                "the first build must succeed"
            );
            assert_eq!(
                surface.resources.borrow().as_ref().unwrap().programs.len(),
                1,
                "the one-pass pipeline compiles exactly one program",
            );

            // Constant grid, different program.
            assert!(surface.ensure_resources(&gl, &two, prog_two, (4, 4), 0));
            assert_eq!(
                surface.resources.borrow().as_ref().unwrap().programs.len(),
                2,
                "a program swap at a constant grid must rebuild Resources, not reuse the \
                 one-pass pipeline's compiled shaders (#979)",
            );
        }
    }
}

glib::wrapper! {
    /// A `gtk::GLArea` running a host-registered shader pipeline. See the
    /// [module docs](self).
    pub struct GlSurface(ObjectSubclass<imp::GlSurface>)
        @extends gtk::GLArea, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl GlSurface {
    /// Build an empty surface (draws nothing until
    /// [`set_state`](Self::set_state)).
    ///
    /// `auto_render` is off — the surface renders when its state moves, not on
    /// every frame the compositor asks for — and the allowed API is pinned to
    /// **GLES**, so the shader dialect is decided here rather than negotiated
    /// per driver. Depth and stencil buffers are off: nothing this draws is
    /// three-dimensional.
    #[must_use]
    pub fn new() -> Self {
        let surface: Self = glib::Object::new();
        surface.set_auto_render(false);
        surface.set_has_depth_buffer(false);
        surface.set_has_stencil_buffer(false);
        surface.set_allowed_apis(gdk::GLAPI::GLES);
        surface
    }

    /// Point the surface at `program` with `state`, at a natural size of
    /// `width`×`height` logical pixels.
    ///
    /// A call carrying the same program and an equal state is a **no-op**: no
    /// render is queued, no resize, nothing is touched. The fast path is
    /// `Arc::ptr_eq`, which is what the shell's per-instance state cache
    /// actually hits when one node is mapped onto a second monitor.
    pub fn set_state(&self, program: GlProgram, width: u32, height: u32, state: &Arc<GlUniforms>) {
        let (render, resize) = self.imp().set_state(program, width, height, state);
        if resize {
            self.queue_resize();
        }
        if render {
            self.queue_render();
        }
    }

    /// Whether this surface's own context failed to be created.
    ///
    /// Per instance, unlike [`gl_abandoned`] — kept because "did *this* area
    /// fail" is the question a test or an inspector asks, while the host's
    /// fallback decision is the process-wide one.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.error().is_some()
    }
}

impl Default for GlSurface {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GlProgram, GlUniforms, GlValue, MAX_STEPS_PER_RENDER, abandon_gl, fit_rect,
        fresh_last_drawn, gl_abandoned, resources_reusable, steps_owed,
    };
    use std::sync::Arc;

    /// **The idempotence rule** (#893's "the draw must be idempotent"), which
    /// is otherwise untestable: CI has no GL, and a repeat render draws exactly
    /// the pixels a stepping one does, so nothing downstream can tell them
    /// apart.
    ///
    /// GTK calls `render` whenever it needs the texture — a resize, a
    /// re-composite, a remap — so a pipeline that ran its step passes
    /// unconditionally would decay a phosphor at whatever rate the compositor
    /// happened to ask at.
    ///
    /// **Falsified** by deleting the `step_seq - last_drawn` subtraction and
    /// always answering `1`: the first assertion goes red.
    #[test]
    fn a_repeat_render_owes_no_steps() {
        assert_eq!(steps_owed(0, 0), (0, false), "a fresh surface at rest");
        assert_eq!(
            steps_owed(7, 7),
            (0, false),
            "a re-composite advances nothing"
        );
    }

    /// An advanced state replays exactly the steps it moved by — and no more
    /// than [`MAX_STEPS_PER_RENDER`], which is the only thing bounding a
    /// surface that was unmapped while its state kept running.
    #[test]
    fn an_advanced_state_replays_its_steps_up_to_the_clamp() {
        assert_eq!(steps_owed(7, 8), (1, false));
        assert_eq!(steps_owed(0, 5), (5, false));
        assert_eq!(
            steps_owed(0, MAX_STEPS_PER_RENDER + 1_000),
            (MAX_STEPS_PER_RENDER, false),
            "catch-up is clamped, not replayed"
        );
    }

    /// A surface built against a long-running state replays **one** step, not
    /// sixty-four. The accumulator it was just handed is black, so there is no
    /// trail to catch up *to*, and every replayed step past the newest one
    /// would stamp a flatline the CPU arm never drew — a bright axis band on a
    /// hot-plugged monitor, because the renderer instance is shared across
    /// mounts and the surface is not.
    ///
    /// **Falsified** by neutering [`fresh_last_drawn`] itself — to `0`, or to
    /// any expression that ignores its argument.
    ///
    /// **What this does *not* cover, stated rather than implied:** the one call
    /// site, `imp::GlSurface::ensure_resources`. Restoring
    /// `self.last_drawn.set(0)` there leaves this test green — verified, and
    /// green with zero warnings for a variant that keeps every binding used
    /// (`set(fresh_last_drawn(step_seq.min(1)))` is the same bug and lints
    /// clean). That line lives in `mod imp` behind a live `GdkGLContext` and
    /// there is no hermetic way to reach it, so it is uncovered — the same
    /// honest gap as the parity harness's exit status. An earlier version of
    /// this comment claimed the call-site mutation as the falsification, which
    /// was simply false; in a tree where these claims are the review currency,
    /// a wrong one costs more than a missing one.
    #[test]
    fn a_fresh_surface_does_not_replay_an_absence_it_has_no_trail_for() {
        assert_eq!(steps_owed(fresh_last_drawn(9_000), 9_000), (1, false));
        assert_eq!(
            steps_owed(fresh_last_drawn(1), 1),
            (1, false),
            "the debut batch is the one step there is",
        );
        assert_eq!(
            steps_owed(fresh_last_drawn(0), 0),
            (0, false),
            "a state that never advanced replays nothing",
        );
        // What it replaced, for contrast: the whole clamp against a black
        // buffer, every step of it stamping a flatline.
        assert_eq!(steps_owed(0, 9_000), (MAX_STEPS_PER_RENDER, false));
    }

    /// A `step_seq` that went backwards means the host rebuilt the state (a
    /// config change restarts the count), so the accumulator is stale and must
    /// be wiped rather than replayed from a negative delta.
    ///
    /// **Falsified** by returning `false` for the reset flag: a rebuilt scope
    /// would resume on the *previous* config's phosphor.
    #[test]
    fn a_rewound_step_seq_wipes_the_accumulator() {
        assert_eq!(
            steps_owed(40, 0),
            (0, true),
            "rebuilt at rest: wipe, no steps"
        );
        assert_eq!(steps_owed(40, 3), (3, true), "rebuilt and already advanced");
    }

    /// The letterbox agrees with `PixelSurface`'s: the largest grid-aspect rect
    /// that fits, centered, with the slack split between the two sides.
    ///
    /// Integer arithmetic on purpose — the GL viewport is in whole framebuffer
    /// pixels, and a float `fit_rect` rounded differently on the two axes would
    /// put a GL chip and a raster chip of the same shape one pixel apart in the
    /// same card.
    #[test]
    fn the_letterbox_centers_the_grid_aspect_rect() {
        // Exact fit: the whole allocation, no padding.
        assert_eq!(fit_rect(288, 96, 288, 96), (0, 0, 288, 96));
        // Too tall: width-limited, padded top and bottom.
        assert_eq!(fit_rect(288, 200, 288, 96), (0, 52, 288, 96));
        // Too wide: height-limited, padded left and right.
        assert_eq!(fit_rect(600, 96, 288, 96), (156, 0, 288, 96));
        // Degenerate inputs draw nothing rather than dividing by zero.
        assert_eq!(fit_rect(0, 96, 288, 96), (0, 0, 0, 0));
        assert_eq!(fit_rect(288, 96, 288, 0), (0, 0, 0, 0));
    }

    /// `GlUniforms` compares by value, which is what the reconciler's dedup
    /// rests on — and the `Arc` in `data` compares by *contents*, so two equal
    /// batches in distinct allocations still settle.
    #[test]
    fn uniform_bags_compare_by_value() {
        let a = GlUniforms {
            values: vec![("u_retained", GlValue::Int(184))],
            data: Some(Arc::from(&[0.25f32, -0.5][..])),
            grid: (144, 48),
            step_seq: 7,
        };
        let mut b = a.clone();
        b.data = Some(Arc::from(&[0.25f32, -0.5][..]));
        assert_eq!(a, b, "equal contents in distinct allocations compare equal");
        b.step_seq = 8;
        assert_ne!(a, b, "step_seq is part of the state");
        let mut c = a.clone();
        c.values = vec![("u_retained", GlValue::Int(185))];
        assert_ne!(a, c, "a uniform value is part of the state");
        let mut d = a.clone();
        d.grid = (144, 49);
        assert_ne!(a, d, "the grid is part of the state");
    }

    /// A program name is a plain `Copy` key: cheap to put on a node, and
    /// distinct names never collide.
    #[test]
    fn program_names_are_copy_keys() {
        let scope = GlProgram("preem.scope");
        let other = GlProgram("preem.gauge");
        assert_eq!(scope, GlProgram("preem.scope"));
        assert_ne!(scope, other);
    }

    /// **#979**: `ensure_resources`' reuse check keys previously-built
    /// `Resources` on the grid **and** the program, not the grid alone. A
    /// program is a mutable prop (`GlSurface::set_state` can hand a node a new
    /// one; `widget_tree`'s `update_in_place` repoints an existing surface at
    /// it in place), so a grid-only key would reuse the old pipeline's
    /// compiled shaders after a program swap — silently, since the surface
    /// still has a `Resources` and `ensure_resources` would report success.
    ///
    /// **Falsified** by dropping the `built_program == program` conjunct from
    /// `resources_reusable` (the pre-#979 behavior): the second assertion goes
    /// red — a same-grid, different-program pair reads as reusable again.
    #[test]
    fn a_program_change_at_a_constant_grid_is_not_reusable() {
        let scope = GlProgram("preem.scope");
        let gauge = GlProgram("preem.gauge");
        assert!(
            resources_reusable((288, 96), scope, (288, 96), scope),
            "same grid, same program: reuse"
        );
        assert!(
            !resources_reusable((288, 96), scope, (288, 96), gauge),
            "constant grid, changed program: must rebuild, not reuse the old pipeline"
        );
        assert!(
            !resources_reusable((288, 96), scope, (144, 96), scope),
            "a grid change alone must still rebuild — unchanged pre-#979 behavior"
        );
    }

    /// The abandon latch is one-way and idempotent — the property the host's
    /// CPU fallback is written against.
    ///
    /// Thread-local, and `cargo test` gives each test its own thread, so this
    /// cannot reach another test's view of it.
    #[test]
    fn abandoning_gl_latches_once() {
        assert!(!gl_abandoned(), "a fresh thread has not abandoned GL");
        abandon_gl("no context in this test");
        assert!(gl_abandoned());
        abandon_gl("a second, different reason");
        assert!(gl_abandoned(), "the latch never clears");
    }
}
