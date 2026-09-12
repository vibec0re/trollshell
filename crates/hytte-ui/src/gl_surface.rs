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
use hytte_gl as hgl;

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

/// Register `pipeline` under `program`.
///
/// Call **once per program at host startup**, on the GTK main thread, before
/// any [`Node::GlSurface`](crate::widget_tree::Node::GlSurface) naming it is
/// reconciled. A surface whose program is unregistered keeps whatever it last
/// successfully drew and says so once (see [`PROGRAM_UNREGISTERED_REFUSED`]),
/// rather than failing the render.
///
/// **A later call replaces the entry in this table, but it does not retract a
/// refusal any surface has already latched against that name** (PR #1199
/// review, LOW 6). A surface's [`BuildKey`] is `((cols, rows), GlProgram)` —
/// the program *name*, not the pipeline behind it — so a pipeline that was
/// refused, fixed, and re-registered under the same name is never handed back
/// to the driver: the surface still believes that key will not build.
///
/// This is a documented limit, not a bug, because there is nothing here to
/// fix it with: the latches live per surface, in widget state this
/// module-level table cannot reach, and adding a registry generation to the
/// key would invalidate every surface's latch on a call that in practice
/// happens once, before any surface exists. The one call site in the tree
/// (`trollshell/src/plugins/preem_gl/mod.rs`) registers a `'static` pipeline
/// at startup and never replaces it. **If a host ever needs live
/// re-registration, the honest fix is to key the latch on the pipeline
/// identity rather than to hope; until then, treat this as
/// register-once-never-replace.** (Unrealizing a surface does clear its
/// latch — see `imp::GlSurface::unrealize` — so "re-register and re-map" is
/// the workaround that exists today.)
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

/// The identity of one pipeline build: what `Resources::build` was asked for.
///
/// The whole input to that call, so "this exact build was already refused" is
/// a value comparison rather than a judgement call. Compared by value rather
/// than hashed (unlike `shader_surface`'s source key, which stands in for a
/// megabyte of GLSL) because it is two `u32`s and a `&'static str`: there is
/// no collision question to close.
///
/// **The whole input to that call *as the surface can see it*.** The
/// `GlProgram` is a name, and [`register`] resolves names against a
/// process-wide table this key does not read, so a pipeline replaced under a
/// name a surface has already latched is not retried — see `register`'s doc,
/// which narrows the contract to register-once-never-replace rather than
/// pretending otherwise (PR #1199 review, LOW 6).
type BuildKey = ((u32, u32), GlProgram);

/// How many distinct refused build keys a [`RefusedBuilds`] remembers.
///
/// Sized and argued exactly like [`WARNED_LENGTHS`] and
/// `shader_surface::WARNED_SOURCES`, and for the same measured reason: a
/// one-slot latch is not a bound at all when two keys alternate, since each
/// evicts the other and every frame goes back to the driver. Eight covers
/// alternation and any realistic set of grids one chip cycles through, in 128
/// bytes.
const REFUSED_BUILDS: usize = 8;

/// The pipeline builds this surface has already asked the driver for and been
/// refused (#1180 item 2).
///
/// **This latches the failure, not the warning.** Before it, a refused build
/// set a one-shot `warned_build` bool: the journal went quiet, `resources`
/// was left `None`, and `ensure_resources` therefore recompiled every pass of
/// the pipeline — five shader pairs for the `Scope` — inside the render
/// callback, on the GTK main thread, on every frame, for the life of the
/// surface. Silenced, not stopped.
///
/// A key is retried only when it leaves the latch, which is what
/// [`REFUSED_BUILDS`] eviction is for: a build refused, eight distinct other
/// builds refused after it, and the first is worth asking about again (a
/// driver that was out of memory may not be). A *successful* build does not
/// clear it, deliberately — compiling the same GLSL for the same grid in the
/// same context is deterministic, so retrying a remembered refusal can only
/// fail the same way, and clearing on success is exactly what would make an
/// alternating good/bad pair recompile per frame again.
///
/// **…and "in the same context" is a real condition, so the latch ends with
/// the context** (PR #1199 review, MEDIUM 1). The determinism argued above is
/// the whole justification for never retrying, and it holds only while the
/// `GdkGLContext` that refused is the one being asked. A context lost and
/// remade — a re-parent, a hot-plug, a driver reset — is a *different* driver
/// state, and the one case where a retry could legitimately succeed. Shipped,
/// this latch outlived every such recreate: `imp::GlSurface::unrealize`
/// dropped `resources` and `last_drawn` (both per context) and left the keys
/// standing, so a surface refused once under a degraded context stayed blank
/// for the life of the process with `remember` returning `false` — not even a
/// second journal line to say why. [`RefusedBuilds::clear`] is what
/// `unrealize` now calls; the sibling widget got this right by construction,
/// because `shader_surface`'s equivalent latch lives *inside* the resources
/// that are dropped there.
#[derive(Debug, Default)]
struct RefusedBuilds {
    /// The refused keys, oldest first. At most [`REFUSED_BUILDS`].
    keys: std::collections::VecDeque<BuildKey>,
}

impl RefusedBuilds {
    /// Whether `key` is a build this surface already knows will not build.
    fn refused(&self, key: BuildKey) -> bool {
        self.keys.contains(&key)
    }

    /// Forget every refusal: the context they were measured against is gone.
    ///
    /// Called from `unrealize` only — see the type's doc for why a context
    /// boundary is the one thing that un-latches a key wholesale, while a
    /// successful build deliberately does not.
    fn clear(&mut self) {
        self.keys.clear();
    }

    /// Remember `key` as refused, returning whether it is news — which is
    /// also whether to write the journal line, so the log is bounded by the
    /// same latch that bounds the compiles rather than by a second one that
    /// could disagree with it.
    fn remember(&mut self, key: BuildKey) -> bool {
        if self.keys.contains(&key) {
            return false;
        }
        // `>=` for the reason `WarnLatch::claim` spells out: `==` reads as
        // "unbounded" the moment the bound is ever set to zero.
        if self.keys.len() >= REFUSED_BUILDS {
            self.keys.pop_front();
        }
        self.keys.push_back(key);
        true
    }
}

/// A [`WarnLatch`] key for a program *name* (PR #1199 review, NIT 1).
///
/// `WarnLatch` keys on a `u64` because its other two call sites key on a
/// length and a framebuffer status; a `GlProgram` is a `&'static str`, so it
/// is hashed to fit — the same `DefaultHasher` `shader_surface::source_key`
/// uses, and with far less riding on it: a collision here costs one journal
/// line about a program nobody registered, not a silently blank widget.
fn program_key(program: GlProgram) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    program.0.hash(&mut hasher);
    hasher.finish()
}

/// How many distinct refused data-strip lengths a [`WarnLatch`] remembers.
///
/// Mirrors `shader_surface::WARNED_SOURCES` and its rationale: a one-entry
/// latch lets two differently-sized refusals alternating write a line every
/// frame. This call site is not the shader widget's, but the failure shape
/// — and the fix — are identical.
const WARNED_LENGTHS: usize = 8;

/// A journal latch over the last [`WARNED_LENGTHS`] refused data-strip
/// lengths (#1023 item 3).
///
/// A local twin of `shader_surface::WarnLatch` rather than a shared type:
/// nothing outside this file needs it, and duplicating ~15 lines here keeps
/// this module's failure-reporting self-contained instead of reversing
/// `shader_surface`'s existing dependency on this module (it imports
/// [`fit_rect`], [`abandon_gl`] and [`GLSL_HEADER`] from here).
#[derive(Debug, Default)]
struct WarnLatch {
    /// The lengths already reported, oldest first. At most [`WARNED_LENGTHS`].
    said: std::collections::VecDeque<u64>,
}

impl WarnLatch {
    /// Whether to write a line for `key`: `true` the first time this key is
    /// seen, `false` for every repeat of a key still remembered.
    fn claim(&mut self, key: u64) -> bool {
        if self.said.contains(&key) {
            return false;
        }
        // `>=`, not `==` (PR #1048 fix round, INFO): `==` is exactly what let
        // `WARNED_LENGTHS = 0` pass as an unbounded latch pre-#1046 — the
        // empty deque's `len()` is `0`, so `== 0` was already true and
        // `pop_front` never ran. `>=` makes that class of mistake
        // unreachable instead of merely tested for.
        if self.said.len() >= WARNED_LENGTHS {
            self.said.pop_front();
        }
        self.said.push_back(key);
        true
    }
}

/// A failed data-strip reallocation, carrying the length it failed at so the
/// caller can latch its journal line **per length** (#1023 item 3).
///
/// Mirrors `shader_surface::Failure`: [`imp::Resources::upload_data`] used to
/// swallow this error entirely (`let Ok(texture) = … else { … return; }`,
/// no log, no latch) — the sibling of `shader_surface`'s own MEDIUM 1
/// (#1020 review LOW 2). #977 widened what the underlying
/// `hgl::Texture::new` call can fail with from an unreachable `Extent` to
/// a real `TextureSize`/`Storage`, so this call site can now fail for a
/// genuine reason and used to say nothing at all when it did.
///
/// Top-level rather than nested in `imp` (moved there in PR #1031's fix
/// round, review M3) so [`warn_on_data_failure`]'s own hermetic test — a
/// sibling of `imp::tests`, not a descendant of it — can construct one by
/// hand without a GL context.
struct DataFailure {
    /// What the driver said.
    error: hgl::Error,
    /// The data length `Texture::new` was asked to allocate for.
    len: u32,
}

/// Claim `latch` for `failure`'s length and log it once — the whole
/// reporting half of a refused data-strip (re)allocation, in one function a
/// test can drive with no GL context (PR #1031 review M3).
///
/// **Its only production caller is
/// [`imp::Resources::upload_data`](imp)'s own refusal arm**, which is the
/// point: the second-pass review (PR #1031 review H1) measured that while
/// `upload_data` handed a `Result` *back to `draw`*, the shipped bug could be
/// reinstated in one line at that call site — `let _ =
/// resources.upload_data(…);` — with `cargo test`, the `system-tests`
/// bucket and `clippy -D warnings` all green, because the only test that
/// observed the call site needs a live driver and therefore runs nowhere.
/// So the seam was removed rather than tested: `upload_data` takes the latch
/// and reports for itself, `draw` gets no `Result`, and there is no longer a
/// spelling of "drop the error" available at the call site (remedy (a) of
/// that finding). What is left at the call site is *which* latch is passed,
/// and passing anything but the widget's own `warned_data` makes that field
/// unread — `dead_code`, which is a `-D warnings` gate.
///
/// `DataFailure` is plain data, so a test constructs one by hand and calls
/// this function with exactly the arguments the refusal arm passes; only
/// *producing* a genuine one — proving `Texture::new` really refuses an
/// over-limit allocation, and that `upload_data` routes it here — still needs
/// a live driver (`imp::tests::a_refused_length_maps_to_its_own_data_failure`,
/// gated on `system-tests`).
fn warn_on_data_failure(latch: &RefCell<WarnLatch>, failure: &DataFailure) {
    let DataFailure { error, len } = failure;
    if latch.borrow_mut().claim(u64::from(*len)) {
        tracing::warn!(%error, len, "{}", DATA_STRIP_REFUSED);
    }
}

/// The whole refusal arm of `Resources::upload_data`'s reallocation — the
/// **state** half of the promise [`DATA_STRIP_REFUSED`] makes, pulled out so
/// it is observable with no GL context (PR #1031 third-pass review LOW 2).
///
/// Resets `data_source` then `data_len` before reporting, so the strip
/// really does read as empty (`u_data_len = 0`, matching the message) and
/// the next call retries rather than short-circuiting on `Arc::ptr_eq`
/// against data that was never actually uploaded. See
/// `refusing_a_data_strip_zeros_its_length_and_forgets_the_source` for the
/// test that drives this directly.
///
/// **What is pinned is this helper, not the arm that calls it** (PR #1048
/// fix round, LOW 2): the test constructs a [`DataFailure`] and calls this
/// function by hand — it cannot see whether `upload_data`'s `Err` arm
/// writes anything to `self.data_len` *after* this call returns. Today it
/// does not (the call is immediately followed by `return`), but that is a
/// property of the call site, not of this function, and closing that gap
/// hermetically needs the GL-backed half — `Texture::new` actually failing
/// — which is #1036's, not this one's. `data_len` genuinely has to be
/// `&mut` here (this is the one place that zeros it), so unlike
/// `shader_surface`'s `_data_shape` (its `refuse_data_grid` twin), this
/// contract cannot be moved from a test into the type system: there is no
/// way to take `data_len` by shared reference and still do the reset it
/// exists to do.
fn refuse_data_strip(
    data_source: &mut Option<Arc<[f32]>>,
    data_len: &mut u32,
    warned: &RefCell<WarnLatch>,
    failure: &DataFailure,
) {
    *data_source = None;
    *data_len = 0;
    warn_on_data_failure(warned, failure);
}

/// The line a driver-refused data-strip (re)allocation writes to the
/// journal.
///
/// Says the strip **reads as empty** (`u_data_len = 0`), not that it "keeps
/// whatever it last held" (PR #1031 review L3): on this path
/// `Resources::upload_data` sets `self.data_len = 0` and the frame is still
/// drawn (unlike `ShaderSurface`, which returns) — so
/// `program.set_int(gl, "u_data_len", 0)` runs every frame until a length
/// this driver will take arrives, and that is the documented meaning of
/// "no data" (see [`GlUniforms::data`]'s own doc: `None` binds a 1×1 zero
/// texture and sets `u_data_len` to `0` — a refused length reads the same
/// way).
const DATA_STRIP_REFUSED: &str = "a GL surface's data strip could not be (re)allocated; this \
    frame's data upload is skipped and the strip reads as empty (u_data_len = 0) until a length \
    this driver will take arrives (further occurrences of this length are silenced)";

/// Claim `latch` for `error`'s framebuffer status and log it once — the whole
/// reporting half of a refused render target (#1180 item 3), in one function
/// a test can drive with no GL context.
///
/// Sibling of [`warn_on_data_failure`], down to taking the latch as a
/// parameter for the same reason: the only thing left at the call site is
/// *which* latch is passed, and passing anything but the widget's own
/// `warned_target` makes that field unread — `dead_code`, which is a
/// `-D warnings` gate.
fn warn_on_target_failure(latch: &RefCell<WarnLatch>, error: &hgl::Error) {
    if latch.borrow_mut().claim(framebuffer_status_key(error)) {
        tracing::warn!(%error, "{}", RENDER_TARGET_REFUSED);
    }
}

/// The [`WarnLatch`] key for a refused render target: the raw
/// `GL_FRAMEBUFFER_*` status.
///
/// `Framebuffer::draw_to` returns [`hgl::Error::Framebuffer`] and nothing
/// else, so the fallback arm is unreachable today — it is `0` rather than a
/// panic because widening that function's error type must cost a shared
/// journal line, not a crashed shell.
fn framebuffer_status_key(error: &hgl::Error) -> u64 {
    match error {
        hgl::Error::Framebuffer { status } => u64::from(*status),
        _ => 0,
    }
}

/// How far `last_drawn` may advance when a step replay stops early (#1180
/// item 3).
///
/// `owed` is what [`steps_owed`] asked for and `done` is how many of those
/// replays actually ran, so the surface lands on the step it really reached:
/// all of them is `step_seq`, none of them leaves `last_drawn` where the
/// clamp put it, and a partial replay keeps the remainder owed for the next
/// render.
///
/// Pure, and separate from `draw`, for [`steps_owed`]'s reason: the arithmetic
/// is the whole contract and CI cannot reach the arm that exercises it (a
/// driver has to refuse a framebuffer first).
fn last_drawn_after(step_seq: u64, owed: u64, done: u64) -> u64 {
    step_seq.saturating_sub(owed.saturating_sub(done))
}

/// The line a refused render target writes to the journal.
///
/// Says what is **held**, not just what failed: the pass did not run, so the
/// step it belongs to is not counted as drawn and the next render owes it
/// again. Listed alongside the two early-return messages below in
/// `neither_early_return_message_claims_the_surface_draws_nothing`, since it
/// makes the same promise.
const RENDER_TARGET_REFUSED: &str = "a GL pass's render target would not attach; that pass did \
    not run, the step it belongs to is not counted as drawn, and the surface keeps whatever it \
    last successfully drew (further occurrences of this framebuffer status are silenced)";

/// The line an unregistered program name writes to the journal.
///
/// Says the surface **keeps whatever it last successfully drew**, not that it
/// "draws nothing" (PR #1031 review L1, the `gl_surface` half of round 1's
/// L2): this is an early return out of `draw`, and the only thing that clears
/// is `Resources::run`'s [`GlTarget::Screen`] arm — *"GTK does not clear for
/// us"* — which this arm never reaches. A surface that drew fine and is then
/// re-pointed by `set_state` at a name no host registered keeps the old
/// picture frozen on screen; saying it "draws nothing" sends a reader looking
/// for a blank rect that is not there.
const PROGRAM_UNREGISTERED_REFUSED: &str = "no GL pipeline registered under that name; the \
    surface keeps whatever it last successfully drew (nothing, before the first successful \
    frame)";

/// The line a pipeline that will not build writes to the journal.
///
/// Same reasoning as [`PROGRAM_UNREGISTERED_REFUSED`]: `ensure_resources`
/// returns `false`, `draw` gives up on that, and neither reaches a clear.
///
/// Says the build is **not retried**, not merely that further lines are
/// silenced (#1180 item 2): silence was the whole bug. The old wording was
/// accurate about the journal and wrong about the machine — the pipeline was
/// recompiled on every frame behind it.
const PIPELINE_BUILD_REFUSED: &str = "a GL pipeline could not be built; the surface keeps \
    whatever it last successfully drew (nothing, before the first successful frame) and does \
    not rebuild this pipeline again until its program or grid changes";

mod imp {
    use super::{
        BuildKey, DataFailure, GLSL_HEADER, GlBlend, GlDraw, GlInput, GlPass, GlPipeline,
        GlProgram, GlTarget, GlUniforms, GlValue, PIPELINE_BUILD_REFUSED,
        PROGRAM_UNREGISTERED_REFUSED, PROGRAMS, RefusedBuilds, SAMPLER_NAMES, WarnLatch,
        abandon_gl, fit_rect, fresh_last_drawn, gdk, glib, last_drawn_after, program_key,
        refuse_data_strip, resources_reusable, steps_owed, warn_on_target_failure,
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
        /// Journal latch for "this program is not registered", **keyed by the
        /// program name** via [`program_key`] (PR #1199 review, NIT 1).
        ///
        /// A bare `Cell<bool>` before, and the last one in this file: two
        /// different unregistered names cost one line between them, so the
        /// second — a genuinely different fact, with a different missing
        /// `register` call behind it — was swallowed by the first for the
        /// life of the surface. Log-only either way (this arm recompiles
        /// nothing), which is why it is a nit rather than a defect, but it is
        /// the shape the rest of this round converted and it costs one
        /// [`WarnLatch`].
        ///
        /// **Not** cleared by `unrealize`, unlike `refused_builds`: a
        /// program's absence from [`PROGRAMS`] is a fact about the process,
        /// not about this surface's context, so a re-realise is not news
        /// about it.
        warned_unregistered: RefCell<WarnLatch>,
        /// The builds this surface has been refused, keyed by `(grid,
        /// program)` — so a broken shader costs **one compile** and one
        /// journal line, not one of each per frame (#1180 item 2). This
        /// replaced a bare `warned_build: Cell<bool>`, which silenced the
        /// line and left the recompile running; see [`RefusedBuilds`].
        ///
        /// **Per context, and therefore cleared in [`Self::unrealize`]** (PR
        /// #1199 review, MEDIUM 1) — it belongs with `resources` and
        /// `last_drawn` above, not with the journal latches below.
        refused_builds: RefCell<RefusedBuilds>,
        /// Journal latch for "a pass's render target would not attach"
        /// (#1180 item 3), keyed by the raw `GL_FRAMEBUFFER_*` status the
        /// driver reported — for [`WarnLatch`]'s usual reason: an
        /// `INCOMPLETE_ATTACHMENT` and an `UNSUPPORTED` are different triage,
        /// and a bare bool would let the first one silence the second for the
        /// life of the surface. This failure used to be dropped on the floor
        /// entirely: no log, no latch, and the accumulator's step count
        /// advanced over the pass that never ran.
        warned_target: RefCell<WarnLatch>,
        /// Journal latch for "the data strip's texture could not be
        /// (re)allocated" (#1023 item 3), keyed **by the refused length** via
        /// [`WarnLatch`] — not a bare bool, so a second, differently sized
        /// refusal still gets its own line instead of being swallowed by the
        /// first (the same shape `shader_surface`'s `warned_data` fix takes;
        /// see [`DataFailure`]).
        warned_data: RefCell<WarnLatch>,
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
        /// context explicitly made current first, and forget everything this
        /// surface only knows about *that* context.
        ///
        /// The handles' `Drop` calls `glDelete*`, which needs a current
        /// context. GTK makes one current inside its own `unrealize`, but that
        /// runs *after* this override's body, so the `make_current` here is
        /// what actually holds the crate's contract.
        ///
        /// **Three things are per context, and all three are reset here**
        /// (PR #1199 review, MEDIUM 1): `resources` (GL objects), `last_drawn`
        /// (a step count against an accumulator that no longer exists) and
        /// `refused_builds` — the last of which shipped standing. Its whole
        /// argument for never retrying is that the same GLSL at the same grid
        /// compiles the same way *in the same context*; once the context is
        /// gone that argument is gone with it, and a surface refused under a
        /// context that came up degraded would otherwise never be offered to
        /// the healthy one that replaced it.
        ///
        /// The two journal latches (`warned_target`, `warned_data`) are
        /// deliberately **not** cleared. They gate only the log — neither
        /// failure behind them is latched, both are retried on the very next
        /// render regardless — so clearing them would buy a duplicate line per
        /// re-realise and no retry that was not already happening.
        /// `warned_unregistered` stays for a different reason: a program's
        /// absence from [`PROGRAMS`] is a fact about the process, not about
        /// this surface's context, so a re-realise is not news about it.
        fn unrealize(&self) {
            let obj = self.obj();
            if obj.error().is_none() && obj.context().is_some() {
                obj.make_current();
            }
            self.resources.replace(None);
            self.last_drawn.set(0);
            self.refused_builds.borrow_mut().clear();
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

        /// Whether the build this surface is *currently* asking for is one
        /// the driver has already refused — see
        /// [`GlSurface::build_refused`](super::GlSurface::build_refused).
        ///
        /// The key is rebuilt from the live program and the live state's
        /// grid, deliberately, rather than answered from "the latch holds
        /// anything at all": a surface repointed at a pipeline that builds
        /// fine is not refused, even though the key that refused is still
        /// remembered against the grid it failed at.
        pub(super) fn build_refused(&self) -> bool {
            let Some(program) = self.program.get() else {
                return false;
            };
            let Some(grid) = self.state.borrow().as_ref().map(|state| state.grid) else {
                return false;
            };
            self.refused_builds.borrow().refused((grid, program))
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
                if self
                    .warned_unregistered
                    .borrow_mut()
                    .claim(program_key(program))
                {
                    tracing::warn!(program = program.0, "{}", PROGRAM_UNREGISTERED_REFUSED);
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
            // The latch goes *in*; nothing comes back. `upload_data` used to
            // hand its `Result` up to here, and re-swallowing it at this one
            // line reinstated the shipped bug with every gate green (PR #1031
            // review H1) — the only test that observes this line needs a live
            // driver and runs in no environment this repo has. There is now
            // no `Result` here to drop; see `warn_on_data_failure`.
            resources.upload_data(&gl, state.data.as_ref(), &self.warned_data);

            // The idempotence rule, decided by `steps_owed` — see there.
            let (steps, reset) = steps_owed(self.last_drawn.get(), state.step_seq);
            if reset && let Err(error) = resources.clear_accumulator(&gl) {
                // Nothing was wiped, so nothing may be replayed onto it:
                // `last_drawn` stays ahead of `step_seq` and the next render
                // tries the wipe again. See `RENDER_TARGET_REFUSED`.
                warn_on_target_failure(&self.warned_target, &error);
                return;
            }
            // The steps replayed are always the **newest** `steps` of them, so
            // a surface that fell far enough behind to hit the clamp catches up
            // on what is current rather than on ancient history.
            //
            // **A step that did not run is not counted as drawn** (#1180 item
            // 3). `Framebuffer::draw_to` can refuse — an incomplete
            // framebuffer is a real, if rare, driver answer — and its `Err`
            // used to be dropped inside `run` with no log and no latch while
            // the ping-pong pair flipped and `last_drawn` jumped to
            // `step_seq` regardless. The accumulator then carried a hole the
            // surface believed it had filled, permanently: `steps_owed` never
            // asks for a step twice. Now the replay stops at the first
            // refusal, the pair does not flip on the step that failed, and
            // `last_drawn` lands on the last step that really ran
            // (`last_drawn_after`), so the next render owes the rest.
            let mut drawn = 0_u64;
            'steps: for back in (0..steps).rev() {
                for (slot, pass) in pipeline.step.iter().enumerate() {
                    let program = resources.step_programs.get(slot).copied();
                    if let Err(error) =
                        resources.run(&gl, pass, program, &state, self.obj().as_ref(), true, back)
                    {
                        warn_on_target_failure(&self.warned_target, &error);
                        break 'steps;
                    }
                }
                resources.front = 1 - resources.front;
                drawn += 1;
            }
            self.last_drawn
                .set(last_drawn_after(state.step_seq, steps, drawn));

            for (slot, pass) in pipeline.frame.iter().enumerate() {
                let program = resources.frame_programs.get(slot).copied();
                // A frame pass advances nothing, so there is no state to hold
                // — but the passes are ordered (a blur reads what the pass
                // before it wrote), so the rest of the frame is abandoned
                // rather than drawn over a target that was never written.
                if let Err(error) =
                    resources.run(&gl, pass, program, &state, self.obj().as_ref(), false, 0)
                {
                    warn_on_target_failure(&self.warned_target, &error);
                    break;
                }
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
            // **The failure is latched, not just its warning** (#1180 item
            // 2): a build this surface has already been refused is not
            // handed to the driver a second time. Without this the arm
            // below silenced the journal line and nothing else — `resources`
            // stayed `None`, so every frame recompiled every pass of the
            // pipeline inside the render callback, forever. See
            // [`RefusedBuilds`] for what un-latches a key.
            let key: BuildKey = (grid, program);
            if self.refused_builds.borrow().refused(key) {
                return false;
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
                    if self.refused_builds.borrow_mut().remember(key) {
                        tracing::warn!(
                            %error,
                            program = program.0,
                            grid = format!("{}x{}", grid.0, grid.1),
                            "{}",
                            PIPELINE_BUILD_REFUSED
                        );
                    }
                    self.resources.replace(None);
                    false
                }
            }
        }
    }

    #[cfg(all(test, feature = "system-tests"))]
    thread_local! {
        /// How many times [`Resources::build`] has asked the driver, on this
        /// thread — the seam
        /// `a_refused_pipeline_is_built_once_not_once_per_frame` reads
        /// (#1180 item 2).
        ///
        /// A counter rather than a builder seam: "the compile did not
        /// happen" has no observable trace otherwise (a refused compile
        /// raises no `glGetError` and leaves no object), and unlike
        /// `shader_surface`'s `ProgramCache` this call is not generic over
        /// its builder — it allocates textures and an FBO against a live
        /// context, so a counting stand-in would be a parallel probe
        /// agreeing with itself. Compiled **only** into the gated test
        /// build: nothing it touches exists in a shipped binary.
        static BUILD_ATTEMPTS: Cell<u32> = const { Cell::new(0) };
    }

    impl Resources {
        /// Compile every pass and allocate the grid-sized textures.
        fn build(
            gl: &hgl::Gl,
            pipeline: &GlPipeline,
            program: GlProgram,
            grid: (u32, u32),
        ) -> Result<Self, hgl::Error> {
            #[cfg(all(test, feature = "system-tests"))]
            BUILD_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
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
                framebuffer: hgl::Framebuffer::new(gl)?,
                vao: hgl::VertexArray::new(gl)?,
                grid: (cols, rows),
                program,
            };
            resources.clear_accumulator(gl)?;
            Ok(resources)
        }

        /// Zero both halves of the ping-pong pair — a fresh screen.
        ///
        /// # Errors
        ///
        /// The driver's, if either half cannot be attached as a render
        /// target. **Propagated rather than skipped** (#1180 item 3): this
        /// used to be `if draw_to(…).is_ok()`, so a refusal left whatever
        /// `glTexStorage2D` had put in the texture — undefined contents, not
        /// zeroes — and every caller carried on as though the screen were
        /// fresh. `Resources::build`'s `?` turns that into a refused build
        /// (which is latched, so it is asked once), and `draw`'s reset arm
        /// holds `last_drawn` so the wipe is retried instead of being
        /// replayed onto.
        fn clear_accumulator(&self, gl: &hgl::Gl) -> Result<(), hgl::Error> {
            for texture in &self.accumulator {
                self.framebuffer.draw_to(gl, texture)?;
                hgl::set_blend(gl, hgl::Blend::Replace);
                hgl::clear(gl, [0.0, 0.0, 0.0, 0.0]);
            }
            Ok(())
        }

        /// Re-upload the data strip if it is not the allocation we already
        /// hold, reporting a driver refusal into `warned` on the way out
        /// (#1023 item 3).
        ///
        /// **`warned` is a parameter, and there is no `Result`, deliberately**
        /// (PR #1031 review H1). This used to return
        /// `Result<(), DataFailure>` for `draw` to route into the widget's
        /// `warned_data`; that call site is unreachable without a live GL
        /// driver, so every gate this repository runs stayed green when the
        /// error was re-swallowed there — the exact bug #1023 item 3 fixed.
        /// Taking the latch instead removes the seam rather than testing it:
        /// the refusal arm below is the only place the `Err` exists, and it
        /// hands it straight to [`super::refuse_data_strip`], which has its
        /// own hermetic test.
        ///
        /// On a refusal `self.data_len` is reset to `0` first, so the next
        /// call retries rather than sampling a texture whose shape does not
        /// match what `data_len` claims (the same discipline
        /// `shader_surface::upload_data` follows for its own retry), and the
        /// frame is still drawn — with `u_data_len = 0`, which is the
        /// documented meaning of "no data".
        fn upload_data(
            &mut self,
            gl: &hgl::Gl,
            data: Option<&Arc<[f32]>>,
            warned: &RefCell<WarnLatch>,
        ) {
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
                let texture = match hgl::Texture::new(gl, hgl::Format::R32f, len, 1) {
                    Ok(texture) => texture,
                    Err(error) => {
                        refuse_data_strip(
                            &mut self.data_source,
                            &mut self.data_len,
                            warned,
                            &DataFailure { error, len },
                        );
                        return;
                    }
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
        ///
        /// # Errors
        ///
        /// The driver's, when the pass's offscreen target cannot be attached
        /// to the FBO (#1180 item 3). **Only** that: every other way this
        /// returns early — a pass with no compiled program, an undeclared aux
        /// slot, a zero-sized fit rect — is `Ok(())`, because each of those is
        /// "there was nothing to draw", not "the draw was refused". The
        /// caller's state machine turns on that distinction: an `Err` is what
        /// stops the accumulator's step count from advancing over a step that
        /// never ran.
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
        ) -> Result<(), hgl::Error> {
            let Some(program) = program.and_then(|slot| self.programs.get(slot)) else {
                return Ok(());
            };
            let target = match pass.target {
                GlTarget::Accumulator => {
                    debug_assert!(stepping, "a frame pass may not target the accumulator");
                    if !stepping {
                        return Ok(());
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
                // The one refusal that is a refusal: propagated, so the caller
                // can hold the state this pass was supposed to advance
                // (#1180 item 3). It used to be `if …is_err() { return; }`.
                self.framebuffer.draw_to(gl, texture)?;
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
                    // Nothing to draw into, which is not a refusal.
                    return Ok(());
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
            Ok(())
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
        use super::{
            Arc, BUILD_ATTEMPTS, GlBlend, GlDraw, GlInput, GlPass, GlPipeline, GlProgram,
            GlSurface, GlTarget, GlUniforms, GlValue, PROGRAMS, RefCell, Resources, WarnLatch, gdk,
            glib, hgl,
        };
        use gtk::prelude::*;
        use gtk::subclass::prelude::ObjectSubclassIsExt;

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
        /// `Err` carries **which of the two exits fired**, because they are
        /// different facts with different fixes and the skip message is the
        /// only place anyone reads them. This used to be an `Option` and the
        /// three skips below asserted a cause nobody had checked — *"no GL
        /// context on this display, expected under the sandboxed runner, which
        /// has no mesa"*. That was false in every environment it was run in:
        /// GDK had a context, `area.error()` was `None`, and the skip was
        /// `hgl::Gl::current()` failing because `hytte-gl`'s loader resolved
        /// nothing (#1067). Naming the exit is what turns the next such
        /// failure into a one-line issue.
        fn real_gl() -> Result<(gtk::Window, gtk::GLArea, hgl::Gl), String> {
            let window = gtk::Window::new();
            let area = gtk::GLArea::new();
            area.set_allowed_apis(gdk::GLAPI::GLES);
            window.set_child(Some(&area));
            gtk::prelude::WidgetExt::realize(&window);
            area.realize();
            if let Some(error) = area.error() {
                return Err(format!("GtkGLArea could not create a context: {error}"));
            }
            area.make_current();
            let gl = hgl::Gl::current()
                .map_err(|error| format!("GDK made a context current, but {error}"))?;
            Ok((window, area, gl))
        }

        /// `real_gl()`, but honours `TROLLSHELL_REQUIRE_GL` the way
        /// `TROLLSHELL_REQUIRE_ICON_THEME` gates
        /// `every_icon_name_exists_in_the_adwaita_theme_on_the_search_path`
        /// (`hytte-plugin-niri-layouts/src/plugin.rs`): a skip is
        /// indistinguishable from a pass in captured output, so the build that
        /// means this to gate (CI's `system-tests` check, since #1036) sets
        /// `TROLLSHELL_REQUIRE_GL=1` and a missing/refused context then
        /// **fails**, naming the reason `real_gl()` measured, rather than
        /// skipping quietly. Without it (a bare `cargo test` outside that
        /// check) it still skips — failing a run that could never have
        /// answered the question helps nobody.
        fn real_gl_or_skip(test_name: &str) -> Option<(gtk::Window, gtk::GLArea, hgl::Gl)> {
            match real_gl() {
                Ok(live) => Some(live),
                Err(why) => {
                    let required =
                        std::env::var_os("TROLLSHELL_REQUIRE_GL").is_some_and(|want| want == "1");
                    assert!(
                        !required,
                        "TROLLSHELL_REQUIRE_GL=1, but no GL context is available for \
                         {test_name}: {why}"
                    );
                    eprintln!("SKIPPED {test_name}: {why}");
                    None
                }
            }
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
            let Some((_window, _area, gl)) =
                real_gl_or_skip("a_program_change_at_a_constant_grid_rebuilds_resources")
            else {
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

        /// **#1023 item 3 / PR #1031 review M3+H1.** `Resources::upload_data`
        /// routes a driver-refused allocation into the latch it was handed,
        /// keyed by the length that failed — the one half of the fix that
        /// genuinely needs a live driver. The other half — that a refusal
        /// becomes a latched journal line — is covered hermetically, with no
        /// GL context at all, by `warn_on_data_failure`'s own test in this
        /// file's outer `#[cfg(test)] mod tests`: this test's job is only to
        /// prove that a real driver refusal reaches it.
        ///
        /// Asserts on the **latch**, not on a returned `Result`: since PR
        /// #1031's second fix round `upload_data` takes the latch and reports
        /// for itself, precisely so `draw` cannot drop an error it is never
        /// handed (review H1).
        ///
        /// A length far over any real `GL_MAX_TEXTURE_SIZE` (the GLES 3.x
        /// floor is 2048; even a high-end desktop part tops out at 16384 or
        /// 32768) reaches `hgl::Texture::new`'s `Err(TextureSize)` arm
        /// deterministically, on whatever limit this driver actually reports
        /// — the same "the decision is pure, only the query is not"
        /// separation `hytte_gl::checked_extent`'s own hermetic tests rely
        /// on.
        ///
        /// **Falsified** by deleting the `warn_on_data_failure` call from
        /// `upload_data`'s refusal arm: the latch claims nothing and both
        /// assertions below go red. (That is the one mutation left in this
        /// path that the hermetic suite cannot see — it needs this driver.)
        #[gtk::test]
        fn a_refused_length_maps_to_its_own_data_failure() {
            // Comfortably over any real driver's GL_MAX_TEXTURE_SIZE.
            const OVER: usize = 100_000;

            let Some((_window, _area, gl)) =
                real_gl_or_skip("a_refused_length_maps_to_its_own_data_failure")
            else {
                return;
            };

            let pipeline = GlPipeline {
                aux: 0,
                step: &[],
                frame: &[],
            };
            let program = GlProgram("gl_surface_test.data_strip");
            let mut resources = Resources::build(&gl, &pipeline, program, (4, 4))
                .expect("a small grid always builds");

            let warned = RefCell::new(WarnLatch::default());

            let a: Arc<[f32]> = Arc::from(vec![0.0_f32; OVER]);
            resources.upload_data(&gl, Some(&a), &warned);
            assert_eq!(
                warned.borrow().said.iter().copied().collect::<Vec<_>>(),
                vec![u64::try_from(OVER).unwrap()],
                "a {OVER}-texel strip must be refused on any real driver, and the refusal must \
                 be latched under the length that failed",
            );

            // A second, DIFFERENT over-limit length is latched under ITS OWN
            // length — not swallowed by the first refusal, and not a stale
            // one left over from it.
            let b: Arc<[f32]> = Arc::from(vec![0.0_f32; OVER + 1]);
            resources.upload_data(&gl, Some(&b), &warned);
            assert_eq!(
                warned.borrow().said.iter().copied().collect::<Vec<_>>(),
                vec![
                    u64::try_from(OVER).unwrap(),
                    u64::try_from(OVER + 1).unwrap()
                ],
                "a second, differently sized refusal must earn its own latch slot",
            );
        }

        /// **PR #1031 review M3/H1.** Neither
        /// `a_refused_length_maps_to_its_own_data_failure` above (drives
        /// `Resources::upload_data` directly, with a latch of its own) nor
        /// `warn_on_data_failure_reaches_the_latch_and_warns_once_per_length`
        /// (hermetic, drives `warn_on_data_failure` directly) observes
        /// `draw`'s own call site — that it passes the widget's **own**
        /// `warned_data` and not some other latch. This one does: it drives
        /// `draw()` itself through `set_state`, with a real driver-refused
        /// grid, and reads the result back out of that field.
        ///
        /// It is *not* what stops the shipped bug coming back: this test
        /// needs a live driver and therefore runs in no environment this
        /// repository has (review H1 measured that), so the call site was
        /// restructured until the bug had no spelling left there — see
        /// [`super::warn_on_data_failure`]. This test is the belt to that
        /// structural brace, and earns its keep the day a mesa-bearing
        /// `system-tests` closure lands.
        ///
        /// `draw()` is called directly on a bare `imp::GlSurface::default()`
        /// (never through `render`/a mapped `GtkGLArea`) — safe here because
        /// the registered pipeline's `step`/`frame` are both empty, so the
        /// only place `draw` would call `self.obj()` (inside
        /// `Resources::run`, for each step/frame pass) is never reached; the
        /// data-upload block runs and returns well before that loop.
        ///
        /// **Falsified** by handing `upload_data` a throwaway latch at
        /// `draw`'s call site instead of `&self.warned_data`: the assertion
        /// below goes red. (`dead_code` on the then-unread field catches that
        /// same mutation without a driver, which is why the structural fix is
        /// the load-bearing one.)
        #[gtk::test]
        fn draw_routes_a_refused_upload_into_its_own_warned_data_latch() {
            const OVER: usize = 100_000;

            // `_gl` only proves a context exists; `draw()` re-resolves its own
            // current context via `hgl::Gl::current()`, and dropping the
            // handle does not un-current it — `_window`/`_area` are what keep
            // that alive.
            let Some((_window, _area, _gl)) =
                real_gl_or_skip("draw_routes_a_refused_upload_into_its_own_warned_data_latch")
            else {
                return;
            };

            let pipeline = GlPipeline {
                aux: 0,
                step: &[],
                frame: &[],
            };
            let program = GlProgram("gl_surface_test.draw_wiring");
            PROGRAMS.with_borrow_mut(|programs| {
                programs.insert(program, pipeline);
            });

            let surface = GlSurface::default();
            let state = Arc::new(GlUniforms {
                values: Vec::new(),
                data: Some(Arc::from(vec![0.0_f32; OVER])),
                grid: (4, 4),
                step_seq: 0,
            });
            surface.set_state(program, 4, 4, &state);
            surface.draw();

            assert_eq!(
                surface.warned_data.borrow().said.len(),
                1,
                "draw() must route a refused upload through warn_on_data_failure into its own \
                 warned_data latch",
            );
        }

        /// **#1180 item 2.** A pipeline the driver will not build is asked
        /// for **once**, not once per frame.
        ///
        /// The shipped code latched the *warning* (`warned_build:
        /// Cell<bool>`) and nothing else: `resources` stayed `None`, so
        /// `ensure_resources` handed the same broken GLSL back to the driver
        /// on every single render — five shader pairs per frame for the
        /// `Scope`, synchronously, on the GTK main thread — with the journal
        /// silent about it after the first line. Silenced, not stopped.
        ///
        /// Counting the driver asks is the only way to see this: a refused
        /// compile raises no `glGetError` and leaves no GL object behind, so
        /// the *absence* of a second compile has no other trace. Hence
        /// [`BUILD_ATTEMPTS`], which exists only in this gated test build.
        ///
        /// The eviction half of the bound is covered hermetically by
        /// `RefusedBuilds`' own tests in the outer module; this is the one
        /// place the decision is wired to a real driver refusal.
        ///
        /// **Falsified** by deleting the `refused_builds.borrow().refused(key)`
        /// early return in `ensure_resources`: the count goes to 10.
        #[gtk::test]
        fn a_refused_pipeline_is_built_once_not_once_per_frame() {
            // A fragment stage no driver will compile.
            const BROKEN: GlPass = GlPass {
                vertex: VERTEX,
                fragment: "void main() { this is not GLSL }",
                target: GlTarget::Screen,
                inputs: &[],
                blend: GlBlend::Replace,
                draw: GlDraw::FullScreen,
            };
            const BROKEN_PASS: [GlPass; 1] = [BROKEN];

            let Some((_window, _area, gl)) =
                real_gl_or_skip("a_refused_pipeline_is_built_once_not_once_per_frame")
            else {
                return;
            };

            let pipeline = GlPipeline {
                aux: 0,
                step: &[],
                frame: &BROKEN_PASS,
            };
            let program = GlProgram("gl_surface_test.will_not_build");

            let surface = GlSurface::default();
            BUILD_ATTEMPTS.set(0);
            for frame in 0..10 {
                assert!(
                    !surface.ensure_resources(&gl, &pipeline, program, (4, 4), frame),
                    "a pipeline that will not build can never report resources ready",
                );
            }

            assert_eq!(
                BUILD_ATTEMPTS.get(),
                1,
                "a refused pipeline must be handed to the driver once, not once per frame \
                 (#1180 item 2)",
            );
            assert!(
                surface.refused_builds.borrow().refused(((4, 4), program)),
                "…because the refusal itself is latched, keyed by (grid, program)",
            );

            // The key really is the whole input: the same broken program at a
            // *different* grid is a build this surface has not been refused
            // yet, so it is asked once more and then latched too.
            assert!(!surface.ensure_resources(&gl, &pipeline, program, (8, 4), 0));
            assert_eq!(BUILD_ATTEMPTS.get(), 2, "a new grid is a new question");
            assert!(!surface.ensure_resources(&gl, &pipeline, program, (8, 4), 1));
            assert_eq!(
                BUILD_ATTEMPTS.get(),
                2,
                "…asked exactly once, like the first"
            );
        }

        /// **PR #1199 review, MEDIUM 1.** A refusal latched under one
        /// `GdkGLContext` must not survive that context: after an unrealize /
        /// re-realize the driver is asked again, and a second refusal writes a
        /// second journal line.
        ///
        /// [`RefusedBuilds`]' own doc rests the never-retry rule on
        /// determinism "*in the same context*". Shipped, `unrealize` dropped
        /// `resources` and `last_drawn` and left the keys standing — so the
        /// one condition the rule names was the one thing nothing checked. A
        /// driver that refused a compile once (out of memory at login, a
        /// context that came up degraded) refused it under every later
        /// context too, with `remember` returning `false`, i.e. permanently
        /// and silently blank.
        ///
        /// Taking the surface out of its window and putting it back is the
        /// real trigger, not a stand-in: GTK unrealizes an unparented widget
        /// synchronously and `GtkGLArea` creates a **fresh** context on the
        /// way back in. (The other trigger — a context lost and remade by the
        /// driver — cannot be arranged from a test at all.)
        ///
        /// Asserted on [`BUILD_ATTEMPTS`] rather than on the journal because
        /// the two ride the same latch by construction: `remember` returns
        /// whether to write the line, so a second ask *is* a second line. The
        /// latch itself is checked directly on both sides of the recreate.
        ///
        /// **Falsified** by deleting `refused_builds.borrow_mut().clear()`
        /// from `unrealize`: the count stays at 1, which is what the review
        /// measured against the shipped code.
        #[gtk::test]
        fn a_refused_pipeline_is_asked_again_on_a_fresh_context() {
            const BROKEN: GlPass = GlPass {
                vertex: VERTEX,
                fragment: "void main() { this is not GLSL }",
                target: GlTarget::Screen,
                inputs: &[],
                blend: GlBlend::Replace,
                draw: GlDraw::FullScreen,
            };
            const BROKEN_PASS: [GlPass; 1] = [BROKEN];

            let Some((window, surface, _gl)) =
                realised_surface_or_skip("a_refused_pipeline_is_asked_again_on_a_fresh_context")
            else {
                return;
            };

            let program = GlProgram("gl_surface_test.will_not_build_realised");
            PROGRAMS.with_borrow_mut(|programs| {
                programs.insert(
                    program,
                    GlPipeline {
                        aux: 0,
                        step: &[],
                        frame: &BROKEN_PASS,
                    },
                );
            });
            let state = Arc::new(GlUniforms {
                values: Vec::new(),
                data: None,
                grid: (4, 4),
                step_seq: 0,
            });
            surface.set_state(program, 4, 4, &state);

            BUILD_ATTEMPTS.set(0);
            surface.imp().draw();
            surface.imp().draw();
            assert_eq!(
                BUILD_ATTEMPTS.get(),
                1,
                "the refusal is latched within one context (#1180 item 2)",
            );
            assert!(
                surface
                    .imp()
                    .refused_builds
                    .borrow()
                    .refused(((4, 4), program)),
                "…keyed by (grid, program)",
            );
            assert!(
                surface.build_refused(),
                "…and the host can see it: `build_refused` answers for the program and grid \
                 the surface is currently pointed at (PR #1199 review, LOW 5)",
            );

            // Out of the window: GTK unroots, which unrealizes, which is the
            // only place the per-context state is dropped. The local `surface`
            // is what keeps the widget alive across this.
            window.set_child(None::<&gtk::Widget>);
            assert!(
                !surface.is_realized(),
                "unparenting a realised widget must unrealize it — the premise of this test",
            );
            assert!(
                surface.imp().refused_builds.borrow().keys.is_empty(),
                "unrealize must forget refusals measured against a context that is gone \
                 (PR #1199 review, MEDIUM 1)",
            );
            assert!(
                !surface.build_refused(),
                "…so a host that fell back to its CPU kit on the refusal may offer GL to the \
                 context that replaces it",
            );

            // …and back in, onto a context GTK creates fresh.
            window.set_child(Some(&surface));
            for _ in 0..1000 {
                if surface.is_realized() && surface.width() > 0 {
                    break;
                }
                if !glib::MainContext::default().iteration(false) {
                    break;
                }
            }
            assert!(
                surface.is_realized() && surface.error().is_none(),
                "the surface must come back with a context of its own",
            );
            surface.make_current();
            surface.imp().draw();

            assert_eq!(
                BUILD_ATTEMPTS.get(),
                2,
                "a fresh context is a fresh question: the pipeline must be offered to it, and \
                 the refusal reported again (PR #1199 review, MEDIUM 1)",
            );
            assert!(
                surface
                    .imp()
                    .refused_builds
                    .borrow()
                    .refused(((4, 4), program)),
                "…and re-latched against the new context, so it is still asked only once",
            );

            window.destroy();
        }

        /// **#1180 item 9.** A **realised widget** draws a real pipeline end
        /// to end: the `GlSurface` subclass, in a window, with the context
        /// GTK made for it, through `draw` — every pass, both targets.
        ///
        /// Everything else in this module tests a piece: `ensure_resources`
        /// against a bare `imp::GlSurface::default()` (which has no
        /// `self.obj()`, so it can only be driven with an empty pipeline),
        /// the pure decisions on their own, the uniform bag by value. Until
        /// this, nothing had ever put the widget on a display and let it
        /// render — so `realize`'s context check, the `GlTarget::Screen`
        /// arm's `attach_buffers`/letterbox, `bind_inputs`, the accumulator
        /// ping-pong and every `glUniform*` call ran for the first time on
        /// Annika's laptop rather than in CI.
        ///
        /// The assertion is the driver's own verdict: after a complete
        /// render, [`hgl::Gl::take_error`] must be empty. That covers a class
        /// nothing else here can see — a viewport computed negative, a
        /// sampler bound to a unit that was never set, an
        /// incomplete-framebuffer attach — each of which draws *something*
        /// (usually black) and would otherwise ship green.
        ///
        /// **Falsified, and measured rather than assumed** (llvmpipe, this
        /// crate's own `system-tests` env): deleting `program.bind(gl)` from
        /// `Resources::run` — so every `glUniform*` that follows is set with
        /// no program of ours in use — turns `take_error` into
        /// `Some(1282)`, `GL_INVALID_OPERATION`, and this test red.
        ///
        /// One mutation that does **not** fire, recorded so nobody re-adds
        /// the assertion it would suggest: dropping `self.vao.bind(gl)` stays
        /// green. A core *desktop* profile refuses to draw with vertex array
        /// 0, but this surface pins GLES (`GlSurface::new`), where the
        /// default vertex array is a legal object — so the VAO here is a
        /// portability handle, not something the driver will complain about
        /// losing.
        #[gtk::test]
        fn a_realised_surface_renders_every_pass_without_a_gl_error() {
            const STEP_FRAGMENT: &str = "
                out vec4 frag_color;
                void main() {
                    frag_color = vec4(1.0);
                }";
            // Reads the accumulator through the sampler the host binds, so
            // the input plumbing is exercised rather than assumed.
            const BLIT_FRAGMENT: &str = "
                uniform sampler2D u_tex0;
                uniform ivec2 u_grid;
                out vec4 frag_color;
                void main() {
                    float v = texelFetch(u_tex0, ivec2(0, 0), 0).r;
                    frag_color = vec4(v, v, v, 1.0) * float(u_grid.x > 0);
                }";
            const STEP: [GlPass; 1] = [GlPass {
                vertex: VERTEX,
                fragment: STEP_FRAGMENT,
                target: GlTarget::Accumulator,
                inputs: &[],
                blend: GlBlend::Max,
                draw: GlDraw::FullScreen,
            }];
            const FRAME: [GlPass; 1] = [GlPass {
                vertex: VERTEX,
                fragment: BLIT_FRAGMENT,
                target: GlTarget::Screen,
                inputs: &[GlInput::Accumulator],
                blend: GlBlend::Replace,
                draw: GlDraw::FullScreen,
            }];

            let Some((window, surface, gl)) = realised_surface_or_skip(
                "a_realised_surface_renders_every_pass_without_a_gl_error",
            ) else {
                return;
            };

            let program = GlProgram("gl_surface_test.realised");
            PROGRAMS.with_borrow_mut(|programs| {
                programs.insert(
                    program,
                    GlPipeline {
                        aux: 0,
                        step: &STEP,
                        frame: &FRAME,
                    },
                );
            });

            let state = Arc::new(GlUniforms {
                values: vec![("u_unused_by_this_pipeline", GlValue::Float(0.5))],
                data: Some(Arc::from(vec![0.25_f32, 0.5, 0.75])),
                grid: (8, 4),
                step_seq: 1,
            });
            surface.set_state(program, 8, 4, &state);

            // Anything the fixture or GTK's own scene left queued belongs to
            // them; the drain is what makes the check below an answer about
            // this render.
            let _ = gl.take_error();
            surface.imp().draw();

            assert!(
                surface.imp().resources.borrow().is_some(),
                "a realised surface must have built its GL objects",
            );
            assert_eq!(
                surface.imp().last_drawn.get(),
                1,
                "…and replayed the one step the state owed",
            );
            assert_eq!(
                gl.take_error(),
                None,
                "a complete render over a real driver must raise no GL error",
            );

            // A second render with the same state advances nothing (the
            // idempotence rule) and must still be clean.
            surface.imp().draw();
            assert_eq!(surface.imp().last_drawn.get(), 1, "a repeat render is idle");
            assert_eq!(gl.take_error(), None, "and just as clean");

            window.destroy();
        }

        /// A realised [`super::super::GlSurface`] widget in a presented
        /// window, its own context current, or a skip naming why.
        ///
        /// The widget's context, not a stand-in `gtk::GLArea`'s: `draw`
        /// reaches `self.obj()` for `attach_buffers` and the allocation, so
        /// the thing under test has to be the real widget in a real
        /// allocation. Honours `TROLLSHELL_REQUIRE_GL` exactly as
        /// [`real_gl_or_skip`] does.
        fn realised_surface_or_skip(
            test_name: &str,
        ) -> Option<(gtk::Window, super::super::GlSurface, hgl::Gl)> {
            let window = gtk::Window::new();
            let surface = super::super::GlSurface::new();
            window.set_default_size(64, 32);
            window.set_child(Some(&surface));
            window.present();
            // A presented toplevel realises and allocates on this display;
            // the bound keeps a display that will not do so from hanging the
            // suite.
            for _ in 0..1000 {
                if surface.is_realized() && surface.width() > 0 {
                    break;
                }
                if !glib::MainContext::default().iteration(false) {
                    break;
                }
            }

            let why = if let Some(error) = surface.error() {
                format!("the GlSurface could not create a context: {error}")
            } else if !surface.is_realized() || surface.width() <= 0 {
                format!(
                    "the surface never realised with an allocation (realized={}, width={})",
                    surface.is_realized(),
                    surface.width()
                )
            } else {
                surface.make_current();
                match hgl::Gl::current() {
                    Ok(gl) => return Some((window, surface, gl)),
                    Err(error) => format!("GDK made a context current, but {error}"),
                }
            };

            window.destroy();
            let required =
                std::env::var_os("TROLLSHELL_REQUIRE_GL").is_some_and(|want| want == "1");
            assert!(
                !required,
                "TROLLSHELL_REQUIRE_GL=1, but no realised GL surface is available for \
                 {test_name}: {why}"
            );
            eprintln!("SKIPPED {test_name}: {why}");
            None
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

    /// Whether the driver has **refused to build** the pipeline this surface
    /// is currently pointed at — the program and grid its last
    /// [`set_state`](Self::set_state) named (PR #1199 review, LOW 5).
    ///
    /// This is a third failure, and until now the host had no way to see it.
    /// [`abandon_gl`] covers a failed *context* and [`GlPipeline`]'s absence
    /// covers "this kind has no GL arm" — the two cases #893 says the CPU kit
    /// exists for — but a context that comes up fine and then will not
    /// *compile* a particular pipeline is neither, so the chip simply stayed
    /// blank. #1180 item 2 made that permanent rather than a per-frame
    /// recompile, which is the right answer for the driver and the wrong one
    /// for the widget: a refusal that is asked once is also a refusal nobody
    /// is ever going to retract on its own.
    ///
    /// Reported per instance and polled rather than pushed, because the only
    /// consumer is a host that already runs a pump: `trollshell`'s
    /// `plugins::pump` ticks every render, and reading a `Cell`-shaped answer
    /// there costs nothing, while a signal would need a `Mutable` in a crate
    /// that deliberately has none.
    ///
    /// **The host half is #1180 part 2**, not this commit:
    /// `trollshell/src/plugins/preem_render.rs` is where "GL refused this
    /// instance" becomes a per-instance swap to the CPU kit with one `warn!`,
    /// alongside the existing `preem_gl::arm() == Arm::Cpu` swap, and that
    /// file is being rewritten by PR #1193 (dot matrix) at the same time.
    /// Answering the question here is the half that can land without a
    /// conflict; nothing in the tree reads it yet.
    ///
    /// Goes `false` again when the surface is unrealized — the latch is per
    /// `GdkGLContext` (see `imp::GlSurface::unrealize`), so a host that
    /// switched to the CPU kit on a refusal may offer GL to a fresh context.
    #[must_use]
    pub fn build_refused(&self) -> bool {
        self.imp().build_refused()
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
        BuildKey, DATA_STRIP_REFUSED, DataFailure, GlProgram, GlUniforms, GlValue,
        MAX_STEPS_PER_RENDER, PIPELINE_BUILD_REFUSED, PROGRAM_UNREGISTERED_REFUSED, REFUSED_BUILDS,
        RENDER_TARGET_REFUSED, RefusedBuilds, WARNED_LENGTHS, WarnLatch, abandon_gl, fit_rect,
        framebuffer_status_key, fresh_last_drawn, gl_abandoned, hgl, last_drawn_after, program_key,
        refuse_data_strip, resources_reusable, steps_owed, warn_on_data_failure,
        warn_on_target_failure,
    };
    use std::cell::RefCell;
    use std::sync::Arc;

    /// **#1180 item 3.** A replay that stops early leaves the steps it did
    /// not run still owed.
    ///
    /// This is the arithmetic that makes "hold the state" true. `draw` used
    /// to set `last_drawn` to `step_seq` unconditionally, right after a loop
    /// whose passes could each have been refused and dropped in silence — so
    /// a step that never ran was permanently counted as drawn, because
    /// `steps_owed` never asks for a step twice.
    ///
    /// **Falsified** by returning `step_seq` unconditionally (the shipped
    /// behaviour): every assertion but the first goes red.
    #[test]
    fn a_step_replay_that_stops_early_still_owes_the_rest() {
        assert_eq!(
            last_drawn_after(10, 3, 3),
            10,
            "a complete replay lands on the newest step"
        );
        assert_eq!(
            last_drawn_after(10, 3, 0),
            7,
            "a replay that ran nothing advances nothing: the three are still owed"
        );
        assert_eq!(
            last_drawn_after(10, 3, 2),
            9,
            "…and a partial one owes exactly the remainder"
        );
        assert_eq!(
            last_drawn_after(0, 0, 0),
            0,
            "a surface at rest owes nothing and advances nothing"
        );
        // The clamp case: `steps_owed` caps a surface that fell far behind, so
        // `owed` can be smaller than `step_seq - last_drawn`. Nothing here may
        // underflow on it.
        assert_eq!(
            last_drawn_after(2, MAX_STEPS_PER_RENDER, 0),
            0,
            "saturating, so a clamped replay cannot wrap the step count",
        );
    }

    /// **#1180 item 3.** The refused-target latch is keyed by the framebuffer
    /// status, so a second, *different* refusal is not swallowed by the
    /// first — the shape #1020's MEDIUM 1 and #1023 item 1 each settled for
    /// the latch next door.
    ///
    /// **Falsified** by keying [`framebuffer_status_key`] on a constant: the
    /// second claim returns `false` and the `UNSUPPORTED` refusal is never
    /// reported.
    #[test]
    fn a_second_differently_refused_render_target_still_gets_its_own_line() {
        let latch = RefCell::new(WarnLatch::default());
        let incomplete = hgl::Error::Framebuffer { status: 0x8CD6 };
        let unsupported = hgl::Error::Framebuffer { status: 0x8CDD };

        assert_ne!(
            framebuffer_status_key(&incomplete),
            framebuffer_status_key(&unsupported),
            "two different GL_FRAMEBUFFER_* statuses are two different facts",
        );

        warn_on_target_failure(&latch, &incomplete);
        assert_eq!(
            latch.borrow().said.len(),
            1,
            "the first refusal is reported"
        );
        warn_on_target_failure(&latch, &incomplete);
        assert_eq!(latch.borrow().said.len(), 1, "…once");
        warn_on_target_failure(&latch, &unsupported);
        assert_eq!(
            latch.borrow().said.len(),
            2,
            "a different framebuffer status must get its own line, not be silenced by the first",
        );
    }

    /// **PR #1199 review, NIT 1.** The unregistered-program latch is keyed by
    /// the program *name*, so a second, different missing registration gets
    /// its own line.
    ///
    /// This field shipped as the file's last bare `Cell<bool>`: two
    /// unregistered names cost one line between them, and the second — a
    /// different missing `register` call, with a different fix — was
    /// swallowed for the life of the surface. Log-only (the arm recompiles
    /// nothing), which is the whole reason it is a nit.
    ///
    /// **Falsified** by keying [`program_key`] on a constant: the third
    /// claim returns `false`.
    #[test]
    fn two_unregistered_program_names_each_get_their_own_line() {
        let mut latch = WarnLatch::default();

        assert_ne!(
            program_key(GlProgram("preem.scope")),
            program_key(GlProgram("preem.dot_matrix")),
            "two program names are two different facts",
        );

        assert!(
            latch.claim(program_key(GlProgram("preem.scope"))),
            "the first unregistered program is reported",
        );
        assert!(!latch.claim(program_key(GlProgram("preem.scope"))), "…once");
        assert!(
            latch.claim(program_key(GlProgram("preem.dot_matrix"))),
            "a different unregistered program must get its own line, not be silenced by the \
             first (PR #1199 review, NIT 1)",
        );
    }

    /// **#1180 item 2.** The refusal latch is keyed by the **whole** build
    /// input, and bounded — the two properties `ensure_resources` rests on.
    ///
    /// A one-slot latch would be no bound at all: two refused keys
    /// alternating (a chip flapping between two grids with a shader that
    /// will not compile at either) would each evict the other, and every
    /// frame would go back to the driver — the same measured shape as
    /// `shader_surface`'s `WARNED_SOURCES` residual (#968 second review).
    ///
    /// **Falsified** by shrinking [`REFUSED_BUILDS`] to `1`: the alternating
    /// pair below reports both keys as news on every round, so `asked` ends
    /// at 10 instead of 2.
    #[test]
    fn a_refused_build_is_remembered_by_grid_and_program_and_the_latch_is_bounded() {
        let scope = GlProgram("preem.scope");
        let gauge = GlProgram("preem.gauge");
        let mut refused = RefusedBuilds::default();

        assert!(!refused.refused(((4, 4), scope)), "nothing is refused yet");
        assert!(
            refused.remember(((4, 4), scope)),
            "the first refusal is news"
        );
        assert!(!refused.remember(((4, 4), scope)), "…and only once");
        assert!(refused.refused(((4, 4), scope)));

        // Both halves of the key matter: a different grid and a different
        // program are each a build this latch has not seen.
        assert!(!refused.refused(((8, 4), scope)), "a different grid");
        assert!(!refused.refused(((4, 4), gauge)), "a different program");

        // Two keys alternating cost two driver asks in total, not one per
        // round — the thing a one-slot latch gets wrong.
        let mut refused = RefusedBuilds::default();
        let mut asked = 0_u32;
        for round in 0..10 {
            let key: BuildKey = if round % 2 == 0 {
                ((4, 4), scope)
            } else {
                ((8, 4), scope)
            };
            if !refused.refused(key) {
                asked += 1;
                refused.remember(key);
            }
        }
        assert_eq!(asked, 2, "two distinct refused builds, two driver asks");

        // The bound holds, and the right way round: REFUSED_BUILDS further
        // distinct refusals evict the first, so it is asked again rather
        // than being refused forever on a driver that may have moved on.
        for n in 0..REFUSED_BUILDS {
            let grid = (u32::try_from(n).unwrap_or(0) + 16, 4);
            refused.remember((grid, scope));
        }
        assert!(
            !refused.refused(((4, 4), scope)),
            "an evicted key is asked again — the cost of a bound, and the safe direction",
        );
    }

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

    /// **#1023 item 3.** The data-strip refusal latch is keyed by length, so
    /// a second, differently sized refusal is not swallowed by the first —
    /// hermetic, driving `WarnLatch::claim` directly with the same shape of
    /// keys `Resources::upload_data`'s caller feeds it (`u64::from(len)`).
    ///
    /// This pins the keying primitive alone; `warn_on_data_failure_reaches_the_latch_and_warns_once_per_length`
    /// below pins the composition — that `draw()`'s actual call site
    /// reaches this latch and a real journal line, not a copy built for the
    /// test's convenience (PR #1031 review M3). The GL-backed proof that a
    /// real driver refusal produces the `Err` in the first place lives next
    /// to `Resources::upload_data` itself in `imp::tests`
    /// (`a_refused_length_maps_to_its_own_data_failure`, needs a context
    /// this crate's hermetic suite does not have).
    ///
    /// **Falsified** by reverting `imp::GlSurface::warned_data` to a bare
    /// `Cell<bool>`: the "a second, DIFFERENT refused length" assertion goes
    /// red.
    #[test]
    fn a_second_differently_sized_refused_data_strip_still_gets_its_own_line() {
        let mut latch = WarnLatch::default();

        let a = u64::from(100_000_u32);
        let b = u64::from(100_001_u32);

        assert!(latch.claim(a), "the first refused length is reported");
        for _ in 0..8 {
            assert!(!latch.claim(a), "…and then goes quiet while it persists");
        }
        assert!(
            latch.claim(b),
            "a second, DIFFERENT refused length must be reported, not swallowed (#1023 item 3)",
        );
        assert!(!latch.claim(b), "…once");
        assert!(!latch.claim(a), "and a, already reported, stays quiet");
    }

    /// Count the `tracing` events this module emits while `emit` runs.
    ///
    /// `tracing_core` caches an `Interest` per callsite, process-wide,
    /// decided by whichever thread reaches it first — the same reason
    /// `shader_map::tests::counting_events` (`trollshell`) exists in the
    /// longer form; this crate cannot depend on that one, so it is
    /// duplicated rather than shared.
    ///
    /// **The warm-up below is load-bearing, and the reason it is here is a
    /// correction** (PR #1031 review M1). The first version of this helper
    /// skipped it, arguing that `imp::tests` is "a separate,
    /// `system-tests`-gated binary target" that "never reaches
    /// `warn_on_data_failure`". Both halves were false: `mod imp { … #[cfg]
    /// mod tests … }` compiles into this crate's *one* lib-test binary (182
    /// tests under `--features system-tests`, 83 without — same target), and
    /// `imp::tests::draw_routes_a_refused_upload_into_its_own_warned_data_latch`
    /// drives `draw`, which on any machine with a GL driver reaches
    /// [`warn_on_data_failure`]'s `warn!` **with no subscriber installed**,
    /// on the GTK test thread. A subscriber-less first hit caches
    /// `Interest::never()` for that callsite process-wide
    /// (`tracing_core::callsite::register` → `Rebuilder::JustOne` →
    /// `dispatcher::get_default` on the registering thread), which would
    /// blank this helper's count with `left: 0, right: 2` — #991/#1014/#1022,
    /// a flake this repository has already paid for four times, and one that
    /// would fire on a developer's glass and never in CI.
    ///
    /// So: touch the callsites *before* `with_default` installs the
    /// subscriber, so its `Dispatch::new` rebuilds their `Interest` against
    /// it. Costs one silent `warn!` per call and needs no discipline from any
    /// other test.
    fn counting_events(emit: impl FnOnce()) -> u32 {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU32, Ordering};

        const TARGET: &str = "hytte_ui::gl_surface";
        struct Counting(StdArc<AtomicU32>);
        impl tracing::Subscriber for Counting {
            fn enabled(&self, meta: &tracing::Metadata<'_>) -> bool {
                meta.target().starts_with(TARGET)
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::Id {
                tracing::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::Id, _: &tracing::Id) {}
            fn event(&self, event: &tracing::Event<'_>) {
                if event.metadata().target().starts_with(TARGET) {
                    self.0.fetch_add(1, Ordering::Relaxed);
                }
            }
            fn enter(&self, _: &tracing::Id) {}
            fn exit(&self, _: &tracing::Id) {}
        }

        // Warm-up: register this module's `warn!` callsites while no
        // subscriber is installed, so the `Dispatch::new` below rebuilds
        // their `Interest` against the counting one. See this function's doc.
        warn_on_data_failure(
            &RefCell::new(WarnLatch::default()),
            &DataFailure {
                error: hgl::Error::TextureSize {
                    size: (0, 0),
                    limit: 0,
                },
                len: u32::MAX,
            },
        );

        let count = StdArc::new(AtomicU32::new(0));
        tracing::subscriber::with_default(Counting(StdArc::clone(&count)), emit);
        count.load(Ordering::Relaxed)
    }

    /// **PR #1031 review M3/H1.** A refusal reaches the latch **and** a real
    /// journal line — through [`warn_on_data_failure`], with exactly the
    /// arguments `Resources::upload_data`'s refusal arm passes it
    /// (`&self.warned_data`-shaped latch, `&DataFailure { error, len }`), not
    /// a `WarnLatch` driven bare for the test's own convenience.
    ///
    /// Since the second fix round this *is* the whole reporting half:
    /// `upload_data` takes the latch and calls this, `draw` is handed no
    /// `Result`, and the one-line re-swallow the review found at that call
    /// site no longer type-checks. `DataFailure` is constructed by hand
    /// rather than through a real `Texture::new` refusal — producing a
    /// genuine one still needs real GL
    /// (`imp::tests::a_refused_length_maps_to_its_own_data_failure`, gated on
    /// `system-tests` + a live driver, which this sandbox does not have) —
    /// but everything downstream of it, which is what item 3 actually fixed,
    /// does not.
    ///
    /// **Falsified** by emptying this function's body (`let _ = (latch,
    /// failure);`): `left: 0, right: 2`. Also red under a one-slot
    /// `WarnLatch`, and under a latch keyed by anything but the length.
    #[test]
    fn warn_on_data_failure_reaches_the_latch_and_warns_once_per_length() {
        let latch = RefCell::new(WarnLatch::default());
        let refusal = |len: u32| DataFailure {
            error: hgl::Error::TextureSize {
                size: (len, 1),
                limit: 4096,
            },
            len,
        };

        let emitted = counting_events(|| {
            warn_on_data_failure(&latch, &refusal(100_000));
            // A repeat of the SAME length must not re-warn.
            warn_on_data_failure(&latch, &refusal(100_000));
            // A DIFFERENT length must still get its own line.
            warn_on_data_failure(&latch, &refusal(100_001));
        });
        assert_eq!(
            emitted, 2,
            "a repeated refusal of the same length must cost one journal line; a different \
             refused length must cost its own",
        );
        assert_eq!(
            latch.borrow().said.iter().copied().collect::<Vec<_>>(),
            vec![100_000_u64, 100_001_u64],
            "…and each is latched under the length that failed, so a later repeat stays quiet",
        );
    }

    /// **PR #1031 review L1** (round 1's L2, the `gl_surface` half). Neither
    /// of `draw`'s early-return arms may claim the surface "draws nothing":
    /// the only clear in this module is `Resources::run`'s
    /// `GlTarget::Screen` arm, and neither arm reaches it, so what is really
    /// on screen is the last good frame.
    ///
    /// [`DATA_STRIP_REFUSED`] is deliberately **not** in this list: that path
    /// does not return, the frame is drawn, and its own test asserts it says
    /// so (`u_data_len = 0`). Between the two files this PR edits there are
    /// five such messages; `shader_surface`'s
    /// `none_of_draws_three_early_return_messages_claim_the_widget_draws_nothing`
    /// pins the other three.
    ///
    /// **Falsified** by reverting either const to #1020's wording.
    #[test]
    fn neither_early_return_message_claims_the_surface_draws_nothing() {
        for msg in [
            PROGRAM_UNREGISTERED_REFUSED,
            PIPELINE_BUILD_REFUSED,
            RENDER_TARGET_REFUSED,
        ] {
            assert!(
                !msg.contains("draws nothing"),
                "an early return out of draw() leaves the last frame up, so no such message may \
                 claim the surface draws nothing (PR #1031 review L1): {msg:?}",
            );
            assert!(
                msg.contains("keeps whatever it last successfully drew"),
                "…and each should say what actually happens instead: {msg:?}",
            );
        }
    }

    /// **PR #1031 review L3.** The data-strip refusal message says the
    /// strip **reads as empty** (`u_data_len = 0`), not that it "keeps
    /// whatever it last held" — #1031's own shipped wording. On this path
    /// `Resources::upload_data` zeroes `data_len`, the frame is still drawn
    /// (unlike `ShaderSurface`, which returns), and `u_data_len = 0` is
    /// the documented meaning of "no data" — so a plugin author reading the
    /// old wording would wrongly believe stale samples were still being
    /// read.
    ///
    /// **Falsified** by reverting [`DATA_STRIP_REFUSED`] to "…the strip
    /// keeps whatever it last held…".
    #[test]
    fn the_data_strip_refusal_message_says_it_reads_as_empty_not_that_it_keeps_its_contents() {
        assert!(
            DATA_STRIP_REFUSED.contains("reads as empty (u_data_len = 0)"),
            "message must say the strip reads as empty: {DATA_STRIP_REFUSED:?}",
        );
        assert!(
            !DATA_STRIP_REFUSED.contains("keeps whatever it last held"),
            "must not claim stale samples are still being read: {DATA_STRIP_REFUSED:?}",
        );
    }

    /// **PR #1031 third-pass review LOW 1 (#1046), tightened by the #1048
    /// fix round.** The FIFO bound this PR added to `WarnLatch` is what
    /// makes eviction possible at all — with no clear-on-success anywhere in
    /// this module, eviction is the *only* path back to a second line for a
    /// length already reported. A port of
    /// `shader_surface::two_broken_sources_alternating_cost_two_lines_and_two_compiles`,
    /// the twin this bound shipped without a test for.
    ///
    /// `a_second_differently_sized_refused_data_strip_still_gets_its_own_line`
    /// looks like it would cover this and does not: it claims the same key
    /// repeatedly, and a repeat claim returns early without pushing, so
    /// `said` never grows past one entry and the FIFO is never filled.
    ///
    /// **Two-sided and literal-anchored** (PR #1048 fix round, LOW 1): the
    /// original version looped `0..WARNED_LENGTHS`, so it moved with the
    /// constant and stayed green at `WARNED_LENGTHS` shrunk to `3` or grown
    /// to `9` — it pinned only that eviction happens, not the bound's
    /// value. This version fills the latch to exactly one short of the
    /// bound (asserting `a` still latched there), then makes the claim that
    /// fills it, then evicts. `BOUND` is a literal, checked against
    /// [`WARNED_LENGTHS`] rather than substituted for it, so a shrink or
    /// grow of the real constant is exactly what makes the `assert_eq!`
    /// fail — the same "pin external wire units as literals" discipline
    /// applied to an internal bound.
    ///
    /// **Falsified** by setting `WARNED_LENGTHS` to `0`: `self.said.len() >=
    /// WARNED_LENGTHS` is already true on the empty latch, so `pop_front`
    /// runs (and pops nothing) and `a` is evicted on the very first
    /// follow-up claim — the `assert_eq!` against `BOUND` catches the
    /// mismatch immediately. Also falsified by `WARNED_LENGTHS` set to `7`
    /// or `9`: the `assert_eq!` fails either way, where the old loop-form
    /// test stayed green.
    #[test]
    fn a_length_evicted_by_the_bound_is_reported_again() {
        const BOUND: usize = 8;
        assert_eq!(
            WARNED_LENGTHS, BOUND,
            "this test's arithmetic is written for a bound of 8 — update BOUND (and the \
             arithmetic below) if WARNED_LENGTHS changes",
        );

        let mut latch = WarnLatch::default();

        let a = u64::from(100_000_u32);
        assert!(latch.claim(a), "the first refused length is reported");

        // One short of the bound: `a` must still be the oldest entry, not
        // yet evicted.
        for n in 0..BOUND - 1 {
            let key = (200_000 + n) as u64;
            assert!(
                latch.claim(key),
                "each new distinct length is reported once"
            );
        }
        assert!(
            !latch.claim(a),
            "a must still be latched one short of the bound — the FIFO holds BOUND entries, \
             and only BOUND - 1 have been added since a",
        );

        // The claim that fills the bound: the *next* insert is the one that
        // evicts `a`, not this one.
        assert!(
            latch.claim(300_000),
            "the length that fills the bound is itself new, so it is reported",
        );

        assert!(
            latch.claim(a),
            "a was evicted by the bound, so it is reported again rather than silently blanking \
             the widget",
        );
    }

    /// **PR #1031 third-pass review LOW 2(a) (#1046).** [`refuse_data_strip`]'s
    /// state half, pinned with no GL context: [`DATA_STRIP_REFUSED`]
    /// promises the strip "reads as empty (`u_data_len = 0`)", and that is
    /// this helper actually zeroing `data_len` — not merely the sentence
    /// claiming it. It also forgets `data_source`, so a later successful
    /// upload is not skipped as an unchanged repeat.
    ///
    /// **This pins the helper, not the arm that calls it** (PR #1048 fix
    /// round, LOW 2) — see [`refuse_data_strip`]'s own doc for why that
    /// residual gap (a live driver has to refuse an allocation to exercise
    /// `upload_data`'s actual `Err` arm) is inherent to the boundary #1046
    /// drew, not something this test can close on its own.
    ///
    /// **Falsified** by deleting `*data_len = 0;` from `refuse_data_strip`:
    /// this test's `data_len` assertion goes red while `cargo test` and
    /// workspace clippy both stay green, because nothing downstream of the
    /// swallowed length is reachable without a live driver.
    #[test]
    fn refusing_a_data_strip_zeros_its_length_and_forgets_the_source() {
        let mut data_source: Option<Arc<[f32]>> = Some(Arc::from(&[1.0_f32, 2.0, 3.0][..]));
        let mut data_len = 100;
        let warned = RefCell::new(WarnLatch::default());
        let failure = DataFailure {
            error: hgl::Error::TextureSize {
                size: (100, 1),
                limit: 4096,
            },
            len: 100,
        };

        refuse_data_strip(&mut data_source, &mut data_len, &warned, &failure);

        assert_eq!(
            data_len, 0,
            "u_data_len must actually become 0 — DATA_STRIP_REFUSED says the strip reads as \
             empty",
        );
        assert!(
            data_source.is_none(),
            "the source must be forgotten too, so a later successful upload is not skipped as \
             an unchanged repeat",
        );
    }
}
