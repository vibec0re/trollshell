//! `hytte-gl` — the workspace's **second `unsafe` island** (after `hytte-ecal`),
//! and the only place in the tree that issues an OpenGL call.
//!
//! # Why this crate exists
//!
//! The workspace sets `unsafe_code = "forbid"` at the root, and every raw-GL
//! binding — the `gl` crate `gdk4` already carries, `epoxy`, `glow` — exposes
//! `glClear`/`glDrawArrays`/… as `unsafe fn`. `forbid` cannot be lifted
//! locally, which is why `hytte-ui`'s stage-A probe
//! (`examples/gl_probe.rs`) deliberately issues no GL at all. Stage B has to
//! draw, so it needs an island: a small crate that hand-mirrors the root lints
//! with `unsafe_code = "allow"` and hands everything above it a **safe** API.
//! `hytte-ecal` is the precedent and this follows it exactly, down to the
//! tripwire comment in `Cargo.toml`.
//!
//! The alternatives were weighed on the design spec
//! (`docs/superpowers/specs/2026-09-06-preem-gl-renderer-design.md`): `wgpu`
//! adopting GDK's EGL context is `wgpu_hal::gles::Adapter::new_external`, which
//! is unsafe *and* costs ~100 lock entries; `glow`/`epoxy` mark every entry
//! point `unsafe fn` exactly like `gl` does, so they need this same island and
//! each adds a lock entry `gl` does not. `gl 0.14.0` is **already in
//! `Cargo.lock`** as a `gdk4` dependency, so this crate resolves no new
//! package.
//!
//! # What it is, and what it is not
//!
//! It is deliberately tiny and GTK-free: compile a program with the error text
//! handed back, keep a ping-pong texture pair and an FBO, set uniforms, draw a
//! full-screen triangle or a per-column instanced quad, and get out of the way.
//! It knows nothing about `preem`, about widgets, or about what any of the
//! shaders mean — that vocabulary lives in `hytte-ui`'s `gl_surface` (the
//! pipeline description) and in `trollshell`'s `plugins::preem_gl` (the GLSL).
//!
//! It is **not** a renderer, a scene graph, or a general GL abstraction. Every
//! type here maps one-to-one onto one GL object.
//!
//! # The safety contract
//!
//! Every `unsafe` block in this crate rests on the same three invariants, and
//! callers are given a safe API precisely so they cannot break them:
//!
//! 1. **A context is current on this thread.** [`Gl::current`] is the token
//!    that says so, and it is `!Send`/`!Sync`, so it cannot be smuggled to a
//!    thread where the context is not bound. Every operation takes `&Gl`.
//! 2. **A handle deletes its object only in the context that made it.**
//!    [`Program`], [`Texture`], [`Framebuffer`] and [`VertexArray`] are RAII,
//!    `!Send` for the same reason as the token, and *should* be dropped while
//!    the creating context is still current. `GtkGLArea` does **not**
//!    guarantee that for a subclass: GTK's own `unrealize` makes the context
//!    current, but only after a subclass's override has run, which is why
//!    `hytte-ui`'s widgets call `make_current` themselves before dropping
//!    theirs. A safe `Drop` cannot rely on a caller having done that, so it
//!    **checks**: each handle records the context current when it was created
//!    (glvnd's `eglGetCurrentContext`, through `loader::current_context`), and
//!    `Drop` issues the `glDelete*` only if that same context is current now.
//!    Otherwise it logs and **leaks** the object, because the delete is worse
//!    than the leak both ways it can go wrong: FBOs and VAOs are container
//!    objects GL does not share between contexts (Mesa happens to keep FBO
//!    names share-group-wide; VAO names it keeps per context, as specified),
//!    so a delete under *another* context removes that context's object of
//!    the same name — possibly GTK's own — and a delete with *no* context
//!    current can `abort()` on the libepoxy route (see `loader`). A leaked object lives until its context is
//!    destroyed, and in this tree that is the next thing that happens to it.
//!
//!    "The context that created it" is an `EGLContext` address **paired with
//!    a generation** (#1562): an address can be handed to a new context once
//!    the old one is destroyed, and a handle that outlived its context would
//!    otherwise match the newcomer. A context's owner calls
//!    [`Gl::retire_current_context`] as its last act in that context, which
//!    moves the address to a new generation, so no handle made before it can
//!    match whatever is at that address afterwards. `loader`'s module docs
//!    carry the argument that this is sound and what it does not cover.
//!
//!    The check reaches EGL contexts only. A handle created where the probe
//!    sees none (a GLX context, or a process with no glvnd `libEGL.so.1`)
//!    records no owner, and its `Drop` keeps the unchecked delete — exactly
//!    this crate's behaviour before the check existed, resting on the caller's
//!    `make_current` alone. Using a handle (`bind`, `draw_to`, …) is **not**
//!    checked: those run per frame, inside the owning area's own `render`,
//!    where GTK has made its context current by construction.
//! 3. **Sizes are checked before they reach GL.** Every dimension crossing into
//!    a `glTexStorage2D`/`glViewport` is converted through `i32::try_from` and
//!    refused rather than truncated, because a wrapped negative extent is a
//!    `GL_INVALID_VALUE` at best and a driver crash at worst. Since #977 a
//!    texture extent is also checked against the driver's own
//!    `GL_MAX_TEXTURE_SIZE`, and the allocation itself against `glGetError` —
//!    an extent inside `GLsizei` can still be one this part will not allocate,
//!    and that failure is silent and permanent unless someone asks.
//!
//! # Dialect
//!
//! The bindings are generated from the **desktop** GL registry, but GDK
//! negotiates a **GLES 3.2** context on the reference hardware (#886:
//! `GLAPI(GLES) version=3.2 legacy=false`), and `hytte-ui`'s surface pins
//! `GdkGLAPI::GLES` so the dialect is deterministic rather than
//! driver-dependent. Every function this crate calls is therefore restricted
//! to the **GL 4.x ∩ GLES 3.2** intersection, where the names and enum values
//! are identical; glvnd's dispatch (the route `loader` uses first, #1067)
//! serves both families from one process-wide table, with libepoxy's own
//! `epoxy_<name>` variables as the fallback — see `loader` for which route
//! actually wins on this platform. Shaders are compiled with an explicit version
//! header the caller supplies ([`Program::compile`]) rather than one baked into
//! the source, so the same GLSL body can be re-targeted without editing it.

mod loader;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};

use gl::types::{GLenum, GLint, GLsizei, GLuint};

use loader::ContextId;

/// Anything that can go wrong on the way into GL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The GL entry points could not be resolved — see `loader`. Every
    /// attempted path is named, because "no GL here" and "the soname moved"
    /// are different problems with different fixes.
    Load {
        /// One line per attempted load path, joined with `; `.
        message: String,
    },
    /// A shader failed to compile; the string is the driver's info log,
    /// verbatim. Handed back rather than logged so the caller can put it on the
    /// broken-widget placeholder (#893's trust boundary).
    Compile {
        /// Which stage failed.
        stage: Stage,
        /// The driver's info log.
        log: String,
    },
    /// The program failed to link; the string is the driver's info log.
    Link {
        /// The driver's info log.
        log: String,
    },
    /// A framebuffer was not complete after attaching the texture. Carries the
    /// raw `glCheckFramebufferStatus` value, since the useful ones
    /// (`INCOMPLETE_ATTACHMENT`, `UNSUPPORTED`) are driver-specific triage.
    Framebuffer {
        /// The raw `GL_FRAMEBUFFER_*` status.
        status: u32,
    },
    /// A dimension did not fit GL's `GLsizei`, or was not positive. Refused
    /// rather than truncated — see the safety contract in the module docs.
    Extent {
        /// The offending `width × height`.
        size: (u32, u32),
    },
    /// A dimension is over this implementation's `GL_MAX_TEXTURE_SIZE` (#977).
    ///
    /// Separate from [`Extent`](Error::Extent) because it is a different fact
    /// with a different fix: the number is legal, *this driver* will not take
    /// it, and the limit it did not fit is the useful half of the message.
    TextureSize {
        /// The offending `width × height`.
        size: (u32, u32),
        /// What the driver reported for `GL_MAX_TEXTURE_SIZE`.
        limit: u32,
    },
    /// `glGen*` did not write a name: it left the reserved zero (#1180 item
    /// 4).
    ///
    /// Zero is not "a name that happens to be small" — for a framebuffer it
    /// **is** the window-system-provided framebuffer, so an FBO handle built
    /// over a failed `glGenFramebuffers` would bind the default framebuffer
    /// and every offscreen pass would draw straight into whatever GTK was
    /// compositing, silently. (For a VAO, zero is the default vertex array,
    /// which a core profile refuses to draw with — loud by comparison, but
    /// equally not the object the caller asked for.) `glGen*` has no error
    /// return, so this check is the only place that difference can be caught.
    ///
    /// [`Texture`] and [`Program`] need no such arm and deliberately do not
    /// have one: `Texture::new`'s `glTexStorage2D` against the default
    /// texture object raises `GL_INVALID_OPERATION`, which it already polls
    /// for ([`Storage`](Error::Storage)), and a `glCreateProgram` that
    /// returns zero fails the `LINK_STATUS` read that follows and comes back
    /// as [`Link`](Error::Link).
    Name {
        /// Which object the driver would not name.
        object: &'static str,
    },
    /// `glTexStorage2D` raised a GL error, so the texture object exists with no
    /// storage behind it (#977).
    ///
    /// Immutable-storage allocation is the one call in this crate whose failure
    /// is otherwise **completely silent**: `glGenTextures` succeeds, the id is
    /// valid, every later `glTexSubImage2D` on it raises
    /// `GL_INVALID_OPERATION`, and sampling an incomplete texture unit returns
    /// `vec4(0, 0, 0, 1)` — a black rectangle, no error anywhere, for the life
    /// of the surface. So this is checked even though the crate does not
    /// otherwise poll `glGetError` on the render path (see
    /// [`Gl::take_error`]): it happens once per allocation, not once per draw.
    Storage {
        /// The extent that was asked for.
        size: (u32, u32),
        /// The raw `glGetError` code the driver returned first.
        code: u32,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load { message } => write!(f, "no OpenGL entry points: {message}"),
            Self::Compile { stage, log } => write!(f, "{stage} shader failed to compile: {log}"),
            Self::Link { log } => write!(f, "program failed to link: {log}"),
            Self::Framebuffer { status } => {
                write!(f, "framebuffer incomplete (status {status:#x})")
            }
            Self::Extent { size: (w, h) } => write!(f, "unusable texture extent {w}x{h}"),
            Self::TextureSize {
                size: (w, h),
                limit,
            } => write!(
                f,
                "texture extent {w}x{h} is over this driver's GL_MAX_TEXTURE_SIZE of {limit}"
            ),
            Self::Name { object } => write!(
                f,
                "the driver did not name a new {object} (glGen* left the reserved name 0)"
            ),
            Self::Storage { size: (w, h), code } => write!(
                f,
                "the driver refused storage for a {w}x{h} texture (glGetError {code:#x})"
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Which half of a program a [`Error::Compile`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The vertex shader.
    Vertex,
    /// The fragment shader.
    Fragment,
}

impl Stage {
    /// The GL enum this stage compiles as.
    fn as_gl(self) -> GLenum {
        match self {
            Self::Vertex => gl::VERTEX_SHADER,
            Self::Fragment => gl::FRAGMENT_SHADER,
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Vertex => "vertex",
            Self::Fragment => "fragment",
        })
    }
}

/// Proof that a GL context is current on **this** thread, and the receiver
/// every operation in this crate takes.
///
/// `!Send`/`!Sync` by construction (the `PhantomData<*const ()>`), so it cannot
/// travel to a thread where the context is not bound — which is the first of
/// the module's three safety invariants, made unrepresentable rather than
/// merely documented.
///
/// Constructing one is a *claim*, not a check: GL has no portable "is a context
/// current" query that does not itself need a context. The claim is honest at
/// the only place it is made — inside a `GtkGLArea::render`/`realize` handler,
/// or right after `GLContext::make_current` — and [`Gl::current`] says so.
#[derive(Debug)]
pub struct Gl(PhantomData<*const ()>);

impl Gl {
    /// Take a token for the context the caller has just made current, loading
    /// the entry points on first use.
    ///
    /// Call this **only** from inside a `GtkGLArea` `realize`/`render`/
    /// `unrealize` handler, or immediately after a successful
    /// `GdkGLContext::make_current`. Calling it with no context current is not
    /// unsound on its own — nothing is dereferenced here — but every operation
    /// taken with the resulting token would then be a GL call with no binding,
    /// which drivers answer with an error at best.
    ///
    /// The load is process-wide and memoized: a second area realizing replays
    /// the first one's verdict. See `loader` for why one load serves every
    /// context.
    pub fn current() -> Result<Self, Error> {
        loader::load()?;
        Ok(Self(PhantomData))
    }

    /// Declare the context current now **finished**: no handle created in it
    /// before this call will delete its object after it, under this context
    /// or under any later one EGL puts at the same address (#1562; the
    /// safety contract's invariant 2).
    ///
    /// For the owner of a context, as its **last act in it**: after dropping
    /// every handle it holds (those deletes need the context to still match)
    /// and before letting the context go. `hytte-ui`'s two `GtkGLArea`
    /// subclasses call it at the end of `unrealize`, ahead of GTK's own
    /// `unrealize`, which is where GTK drops the area's reference to the
    /// context. A handle that outlives that point — kept somewhere its owner
    /// did not drop — leaks its object on drop instead of deleting it under
    /// a context that only shares its address.
    ///
    /// Nothing to undo and nothing to fail: when the probe sees no EGL
    /// context (see `loader`), there is no identity to retire and this does
    /// nothing, which leaves such handles on the address-blind behaviour they
    /// already had. Call it with the **owner's** context current — a retire
    /// of some other context does not delete anything wrongly, but it does
    /// make that context's live handles leak on their next drop, so a caller
    /// whose `make_current` may have failed should check first, as
    /// `hytte-ui` does.
    pub fn retire_current_context(&self) {
        if let Some(context) = loader::current_context() {
            loader::retire(context);
        }
    }

    /// The first GL error pending, **draining the whole queue**.
    ///
    /// `glGetError` pops *one* entry: GL keeps a queue and returns
    /// `GL_NO_ERROR` only once it is empty. A single call therefore leaves any
    /// further errors to be picked up by the *next* caller and attributed to
    /// whatever it was doing — which is exactly the misattribution a harness
    /// calls this to avoid. So it loops until the queue is clear and hands back
    /// the first code it saw.
    ///
    /// The loop is bounded because a lost or misbehaving context can report an
    /// error forever; hitting the bound returns the first code like any other
    /// non-empty queue, rather than hanging.
    ///
    /// Not a **per-draw** seam: the render path is written so that a GL error
    /// cannot change what is drawn, and polling `glGetError` per draw is itself
    /// a synchronisation point on some drivers. It is called per *allocation*,
    /// which is a different rate entirely — [`Texture::new`] brackets its
    /// `glTexStorage2D` with it (#977), because a refused immutable-storage
    /// allocation is otherwise completely silent and permanent. The parity
    /// harness calls it too; nothing calls it on the frame path.
    #[must_use]
    pub fn take_error(&self) -> Option<u32> {
        /// Enough to clear any queue a conforming driver keeps, and a bound on
        /// one that never returns `GL_NO_ERROR`.
        const MAX_DRAIN: usize = 64;
        let mut first = None;
        for _ in 0..MAX_DRAIN {
            // SAFETY: a context is current (the `&self` token) and `glGetError`
            // takes no arguments and cannot fail.
            let code = unsafe { gl::GetError() };
            if code == gl::NO_ERROR {
                break;
            }
            first.get_or_insert(code);
        }
        first
    }
}

// ── ownership: which context may delete a handle's object ───────────────────

/// The context a handle is being created in, which its `Drop` will require to
/// be current again before it deletes anything (the safety contract's
/// invariant 2).
///
/// The token is the claim that *a* context is current; the probe is what says
/// *which* one. `None` when the probe can see no context at all — see
/// [`release`] for what a handle with no recorded owner does.
fn owner_now(_gl: &Gl) -> Option<ContextId> {
    loader::current_context()
}

/// The line a handle writes when it leaks its object instead of deleting it.
///
/// Says what was **not** done and why, because the alternative reading — "a
/// GL object leaked" with no cause — sends a reader hunting a missing `Drop`
/// that is not missing.
const LEAKED_OBJECT: &str = "a GL object's handle was dropped while the context that created it \
    was not current, so the object was leaked instead of deleted: a delete here would have hit \
    whatever the current context (if any) holds under the same name. The object lives until its \
    context is destroyed";

/// Run `delete` only if the context current now is `owner` — the whole of a
/// handle's `Drop`, minus the one `glDelete*` call it guards.
///
/// Returns whether `delete` ran. Separate from the four `Drop` impls, and
/// taking the probe as a closure, so the decision is testable with no GL
/// context, no driver and no `gl::*` call at all.
///
/// * **`owner` is `None`**: the probe saw no context when the handle was made
///   (a GLX context, or no glvnd `libEGL.so.1` in the process). There is
///   nothing to compare against, so this keeps the unchecked delete the crate
///   had before the check — and does not ask the probe, so a `Drop` is never
///   the first thing to resolve it.
/// * **`owner` is current**: delete. The normal case, and the one that must
///   keep working — a check that leaked everything would be a GPU memory leak
///   per resize.
/// * **anything else** — another context current, or none, or a context at
///   the owner's address after the owner [retired](Gl::retire_current_context)
///   it (#1562): leak, and say so.
///   The first leak in the process is a `warn`; later ones are `debug`, since
///   one failed `make_current` drops a whole surface's worth of handles at
///   once and they share one cause.
fn release(
    object: &'static str,
    name: GLuint,
    owner: Option<ContextId>,
    current: impl FnOnce() -> Option<ContextId>,
    delete: impl FnOnce(),
) -> bool {
    /// Whether the process has already written its one `warn` for a leak.
    static WARNED: AtomicBool = AtomicBool::new(false);
    let Some(owner) = owner else {
        delete();
        return true;
    };
    let current = current();
    if current == Some(owner) {
        delete();
        return true;
    }
    if WARNED.swap(true, Ordering::Relaxed) {
        tracing::debug!(object, name, ?owner, ?current, "{LEAKED_OBJECT}");
    } else {
        tracing::warn!(
            object,
            name,
            ?owner,
            ?current,
            "{LEAKED_OBJECT} (further leaks are logged at debug level)"
        );
    }
    false
}

// ── programs ────────────────────────────────────────────────────────────────

/// A linked GLSL program, with its uniform locations memoized.
///
/// RAII: `Drop` calls `glDeleteProgram` — under the context that created it
/// only; see the module's safety contract, invariant 2.
#[derive(Debug)]
pub struct Program {
    id: GLuint,
    /// `glGetUniformLocation` is a driver-side string lookup; the render path
    /// sets the same handful of names every frame on every chip, so the
    /// locations are resolved once and kept. A name GL does not know maps to
    /// `-1`, which every `glUniform*` call ignores — the documented "setting an
    /// unused uniform is a no-op" behaviour, which is what lets one uniform bag
    /// feed several passes that each use a subset of it.
    locations: RefCell<HashMap<String, GLint>>,
    /// The context this program was linked in — see [`release`].
    owner: Option<ContextId>,
    _not_send: PhantomData<*const ()>,
}

impl Program {
    /// Compile and link a vertex + fragment pair.
    ///
    /// `version_header` is prepended verbatim to **both** sources and must
    /// carry the `#version` directive (and, for GLES, the `precision`
    /// defaults); the shader bodies deliberately do not, so one body can be
    /// re-targeted. The driver's info log comes back in the error rather than
    /// being logged here, because the caller — not this crate — knows whether a
    /// failed compile is a shell bug or an untrusted plugin's shader.
    pub fn compile(
        gl_ctx: &Gl,
        version_header: &str,
        vertex: &str,
        fragment: &str,
    ) -> Result<Self, Error> {
        let vs = Shader::compile(gl_ctx, Stage::Vertex, version_header, vertex)?;
        let fs = Shader::compile(gl_ctx, Stage::Fragment, version_header, fragment)?;
        // SAFETY: a context is current. `CreateProgram` takes no arguments;
        // `AttachShader`/`LinkProgram` take ids this function just created and
        // still owns. `GetProgramiv` writes one `GLint` through a pointer to a
        // live local.
        let id = unsafe {
            let id = gl::CreateProgram();
            gl::AttachShader(id, vs.id);
            gl::AttachShader(id, fs.id);
            gl::LinkProgram(id);
            id
        };
        let mut status: GLint = 0;
        // SAFETY: `id` is a live program; the out-pointer is a live local.
        unsafe { gl::GetProgramiv(id, gl::LINK_STATUS, &raw mut status) };
        if status == GLint::from(gl::TRUE) {
            return Ok(Self {
                id,
                locations: RefCell::new(HashMap::new()),
                owner: owner_now(gl_ctx),
                _not_send: PhantomData,
            });
        }
        let log = program_info_log(id);
        // SAFETY: `id` is a live program this function created and is
        // abandoning; nothing else holds it.
        unsafe { gl::DeleteProgram(id) };
        Err(Error::Link { log })
    }

    /// Make this the program subsequent draws use.
    pub fn bind(&self, _gl: &Gl) {
        // SAFETY: a context is current and `self.id` is a live linked program.
        unsafe { gl::UseProgram(self.id) };
    }

    /// Set a scalar `int` uniform (also how a `sampler2D` is pointed at a
    /// texture unit). A name the program does not use is a no-op.
    pub fn set_int(&self, _gl: &Gl, name: &str, value: i32) {
        let location = self.location(name);
        // SAFETY: `location` came from this program, which is bound by the
        // caller's `bind`; `-1` is GL's documented "ignore this".
        unsafe { gl::Uniform1i(location, value) };
    }

    /// Set a scalar `float` uniform.
    pub fn set_float(&self, _gl: &Gl, name: &str, value: f32) {
        let location = self.location(name);
        // SAFETY: as `set_int`.
        unsafe { gl::Uniform1f(location, value) };
    }

    /// Set an `ivec2` uniform.
    pub fn set_ivec2(&self, _gl: &Gl, name: &str, value: [i32; 2]) {
        let location = self.location(name);
        // SAFETY: as `set_int`; the pointer is to a live two-element array and
        // the count says one vector.
        unsafe { gl::Uniform2iv(location, 1, value.as_ptr()) };
    }

    /// Set a `vec2` uniform.
    ///
    /// Added for #893's shader widget, whose published interface states
    /// `u_resolution` and `u_data_size` as `vec2` — a pair of pixel counts a
    /// plugin author divides by, where the preem pipeline's own sizes are
    /// integer indices and go through [`set_ivec2`](Self::set_ivec2).
    pub fn set_vec2(&self, _gl: &Gl, name: &str, value: [f32; 2]) {
        let location = self.location(name);
        // SAFETY: as `set_ivec2`, with a two-element float array.
        unsafe { gl::Uniform2fv(location, 1, value.as_ptr()) };
    }

    /// Set a `vec4` uniform.
    pub fn set_vec4(&self, _gl: &Gl, name: &str, value: [f32; 4]) {
        let location = self.location(name);
        // SAFETY: as `set_ivec2`, with a four-element array.
        unsafe { gl::Uniform4fv(location, 1, value.as_ptr()) };
    }

    /// This program's location for `name`, resolved once and memoized.
    ///
    /// **A name with an interior NUL is `-1`, without asking GL.** GL reads a
    /// C string, so it would stop at the NUL and resolve the *prefix* —
    /// `"u_time\0x"` setting `u_time` — and the memo would then file that
    /// answer under the full name. No uniform name can contain a NUL, so the
    /// honest answer is GL's own "no such uniform", which every `glUniform*`
    /// ignores. Not memoized: it is decided without a driver round-trip, so
    /// there is nothing to save, and a caller cannot grow the memo with
    /// names GL never saw.
    fn location(&self, name: &str) -> GLint {
        if let Some(found) = self.locations.borrow().get(name) {
            return *found;
        }
        let Ok(terminated) = CString::new(name) else {
            return -1;
        };
        // SAFETY: `self.id` is a live program and the pointer is to a
        // nul-terminated buffer, with no NUL before its end, that outlives the
        // call.
        let location = unsafe { gl::GetUniformLocation(self.id, terminated.as_ptr()) };
        self.locations
            .borrow_mut()
            .insert(name.to_owned(), location);
        location
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        let id = self.id;
        release("program", id, self.owner, loader::current_context, || {
            // SAFETY: `id` is this handle's own program and nothing else holds
            // it, and `release` runs this only with the context that linked it
            // current — or, for a handle the probe could not attribute, on the
            // caller's word (the module contract, invariant 2).
            unsafe { gl::DeleteProgram(id) };
        });
    }
}

/// One compiled shader stage, alive only long enough to be linked into a
/// [`Program`].
struct Shader {
    id: GLuint,
}

impl Shader {
    fn compile(_gl: &Gl, stage: Stage, version_header: &str, body: &str) -> Result<Self, Error> {
        let source = format!("{version_header}\n{body}");
        // SAFETY: a context is current. `ShaderSource` is handed one source
        // pointer and its exact length, so it never reads past `source`, which
        // outlives the call.
        let id = unsafe {
            let id = gl::CreateShader(stage.as_gl());
            let pointer = source.as_ptr().cast::<i8>();
            let length = GLint::try_from(source.len()).unwrap_or(GLint::MAX);
            gl::ShaderSource(id, 1, &raw const pointer, &raw const length);
            gl::CompileShader(id);
            id
        };
        let mut status: GLint = 0;
        // SAFETY: `id` is a live shader; the out-pointer is a live local.
        unsafe { gl::GetShaderiv(id, gl::COMPILE_STATUS, &raw mut status) };
        if status == GLint::from(gl::TRUE) {
            return Ok(Self { id });
        }
        let log = shader_info_log(id);
        // SAFETY: `id` is this function's own shader and it is being abandoned.
        unsafe { gl::DeleteShader(id) };
        Err(Error::Compile { stage, log })
    }
}

impl Drop for Shader {
    fn drop(&mut self) {
        // SAFETY: `self.id` is this handle's own shader. Deleting an attached
        // shader is legal and defers until the program is deleted, which is
        // exactly the lifetime wanted here. No owner check, unlike the public
        // handles (invariant 2): a `Shader` is private and never outlives the
        // one `Program::compile` call that created it, so it is always dropped
        // under the context that call was made in.
        unsafe { gl::DeleteShader(self.id) };
    }
}

/// The driver's info log for a shader, or an empty string.
fn shader_info_log(id: GLuint) -> String {
    let mut length: GLint = 0;
    // SAFETY: `id` is a live shader; the out-pointer is a live local.
    unsafe { gl::GetShaderiv(id, gl::INFO_LOG_LENGTH, &raw mut length) };
    read_info_log(length, |buffer, capacity, written| {
        // SAFETY: `buffer` has `capacity` bytes of spare capacity (the closure's
        // contract, upheld by `read_info_log`), and GL writes at most that many.
        unsafe { gl::GetShaderInfoLog(id, capacity, written, buffer) };
    })
}

/// The driver's info log for a program, or an empty string.
fn program_info_log(id: GLuint) -> String {
    let mut length: GLint = 0;
    // SAFETY: `id` is a live program; the out-pointer is a live local.
    unsafe { gl::GetProgramiv(id, gl::INFO_LOG_LENGTH, &raw mut length) };
    read_info_log(length, |buffer, capacity, written| {
        // SAFETY: as `shader_info_log`.
        unsafe { gl::GetProgramInfoLog(id, capacity, written, buffer) };
    })
}

/// Shared body of the two info-log readers: allocate `length` bytes, let `fill`
/// write into them, and decode whatever came back lossily.
///
/// `fill` is handed `(buffer, capacity, written)` exactly as GL wants them, and
/// promises to write at most `capacity` bytes at `buffer`.
fn read_info_log(length: GLint, fill: impl FnOnce(*mut i8, GLsizei, *mut GLsizei)) -> String {
    let capacity = usize::try_from(length).unwrap_or(0);
    if capacity == 0 {
        return String::new();
    }
    let mut buffer = vec![0u8; capacity];
    let mut written: GLsizei = 0;
    fill(
        buffer.as_mut_ptr().cast::<i8>(),
        GLsizei::try_from(capacity).unwrap_or(GLsizei::MAX),
        &raw mut written,
    );
    let end = usize::try_from(written).unwrap_or(0).min(capacity);
    String::from_utf8_lossy(&buffer[..end]).trim().to_owned()
}

// ── textures ────────────────────────────────────────────────────────────────

/// The pixel formats this crate can allocate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Single-channel 8-bit **normalized** (`GL_R8`), sampled as a float in
    /// `0.0..=1.0`.
    ///
    /// The design spec named `R8UI` (a single-channel 8-bit *integer* format)
    /// for the phosphor. This is that one decision taken differently, and the
    /// reason is a hard GLES rule rather than a preference: **blending does not
    /// apply to integer colour buffers** (GLES 3.2 § 15.1.3, "blending applies
    /// only if the colour buffer has a fixed-point or floating-point format"),
    /// and the spec's own state→uniform table requires the beam to be combined
    /// with `GL_MAX`, because the kit stamps with `max`, not `+=`. `R8UI` would
    /// therefore need a third texture and a manual max pass to express what one
    /// `GL_MAX` blend expresses here.
    ///
    /// **Nothing is lost to precision.** `GL_R8` stores exactly the integers
    /// `0..=255`; the conversion in is `round(clamp(f, 0, 1) * 255)` and out is
    /// `v / 255.0`, both exact in `f32` over that range, so a shader that reads
    /// `int(round(texture(...).r * 255.0))`, does integer arithmetic, and writes
    /// `float(out) / 255.0` round-trips bit-for-bit. `GL_MAX` on a normalized
    /// format is a max over those same values, and max is monotone, so it agrees
    /// with an integer max on every input.
    R8,
    /// Four-channel 8-bit normalized (`GL_RGBA8`), sampled as four floats in
    /// `0.0..=1.0`, **straight** (non-premultiplied) alpha.
    ///
    /// Added for #893's shader widget, whose data buffer may legitimately be a
    /// grid of colours rather than a grid of intensities. Nothing in the preem
    /// pipeline uses it — that one is exact integer work on [`R8`](Self::R8).
    Rgba8,
    /// Single-channel 32-bit float (`GL_R32F`) — the data texture. Never
    /// filtered (`texelFetch`, or `NEAREST`), so it needs no
    /// `OES_texture_float_linear`.
    R32f,
}

impl Format {
    /// `(sized internal format, transfer format, transfer type)`.
    fn as_gl(self) -> (GLenum, GLenum, GLenum) {
        match self {
            Self::R8 => (gl::R8, gl::RED, gl::UNSIGNED_BYTE),
            Self::Rgba8 => (gl::RGBA8, gl::RGBA, gl::UNSIGNED_BYTE),
            Self::R32f => (gl::R32F, gl::RED, gl::FLOAT),
        }
    }

    /// How many bytes one texel of this format occupies in a CPU-side buffer.
    ///
    /// The unit [`Texture::upload_u8`] and [`Texture::upload_f32`] measure
    /// their input in, and what a caller sizes a staging buffer with.
    #[must_use]
    pub const fn bytes_per_texel(self) -> usize {
        match self {
            Self::R8 => 1,
            Self::Rgba8 | Self::R32f => 4,
        }
    }
}

/// A 2-D texture with immutable storage.
///
/// Always `NEAREST`/`CLAMP_TO_EDGE`: everything this crate draws is a
/// pixel-exact grid, and a filtered read of a phosphor cell is a wrong answer,
/// not a smoother one.
#[derive(Debug)]
pub struct Texture {
    id: GLuint,
    width: u32,
    height: u32,
    format: Format,
    /// The context this texture was allocated in — see [`release`].
    owner: Option<ContextId>,
    _not_send: PhantomData<*const ()>,
}

/// What [`max_texture_size`] answers when the driver's own answer is unusable.
///
/// `GL_MAX_TEXTURE_SIZE` is required to be at least 2048 in GLES 3.x and at
/// least 1024 in every GL profile that has the query at all, so a
/// non-positive answer means the query did not happen (no context, a
/// dispatch stub that resolved to nothing) rather than that the driver
/// really refuses every texture. Refusing every allocation on that basis
/// would turn a *missing measurement* into a blank shell, so the unknown
/// case declines to enforce and leaves the verdict to the `glTexStorage2D`
/// error check below — which needs no query to be right.
const UNKNOWN_MAX_TEXTURE_SIZE: u32 = u32::MAX;

thread_local! {
    /// This thread's memoized `GL_MAX_TEXTURE_SIZE` — see [`max_texture_size`].
    static MAX_TEXTURE_SIZE: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// This implementation's `GL_MAX_TEXTURE_SIZE`, queried **once** per thread.
///
/// # Why once, and why per thread
///
/// It is a property of the implementation, not of a context: every context this
/// process creates comes from the same driver through the same process-wide
/// dispatch table [`loader`] installs once — glvnd's on this platform (#1067),
/// libepoxy's as the fallback — and GTK gives a display one share group. So
/// one query serves every `GdkGLContext`, exactly as one `gl::load_with` does.
/// The memo is per **thread** rather than process-wide because the [`Gl`] token
/// is `!Send` — a second thread with a context of its own has to make its own
/// claim anyway, and a `Cell` on the thread it is read from needs no
/// synchronisation on the path that reads it every allocation.
///
/// A driver answering `<= 0` is treated as *unknown*, not as *zero*: see
/// [`UNKNOWN_MAX_TEXTURE_SIZE`]. **And unknown is not memoized** — only a
/// usable answer is ([`usable_limit`]). The query writes nothing when it is
/// made with no context bound (a glvnd stub on an unbound thread is a no-op),
/// so the first ask on a thread can come back `0` for a reason that is gone
/// by the second. Memoizing it switched the `GL_MAX_TEXTURE_SIZE` check off
/// for the life of the thread, which is the check that turns an over-limit
/// grid into [`Error::TextureSize`] — naming the limit — rather than a bare
/// [`Error::Storage`]. An unusable answer costs one `glGetIntegerv` per
/// allocation instead, and only for as long as it stays unusable.
fn max_texture_size(_gl: &Gl) -> u32 {
    MAX_TEXTURE_SIZE.with(|cached| {
        if let Some(known) = cached.get() {
            return known;
        }
        let mut value: GLint = 0;
        // SAFETY: a context is current (the `&Gl` token). `GetIntegerv` writes
        // one `GLint` through a pointer to a live local, and
        // `GL_MAX_TEXTURE_SIZE` is a single-valued implementation limit in both
        // GL 4.x and GLES 3.x — so exactly one write, into a slot that is big
        // enough for it.
        unsafe {
            gl::GetIntegerv(gl::MAX_TEXTURE_SIZE, &raw mut value);
        }
        let Some(limit) = usable_limit(value) else {
            tracing::debug!(
                value,
                "GL_MAX_TEXTURE_SIZE unusable; not enforcing it this time"
            );
            return UNKNOWN_MAX_TEXTURE_SIZE;
        };
        cached.set(Some(limit));
        tracing::debug!(limit, "GL_MAX_TEXTURE_SIZE");
        limit
    })
}

/// A `GL_MAX_TEXTURE_SIZE` answer worth memoizing, or `None` for one that
/// means the query did not really happen — see [`max_texture_size`].
fn usable_limit(value: GLint) -> Option<u32> {
    u32::try_from(value).ok().filter(|&limit| limit > 0)
}

/// Both extents as positive `GLsizei`s, or the reason they are unusable.
///
/// Split out of [`Texture::new`] as a pure function so the decision — which is
/// the half of #977 a hermetic test can reach, CI having no GL at all — is
/// testable without a context. The order matters: an extent that is zero or
/// does not fit `GLsizei` is [`Error::Extent`] whatever the driver says, so the
/// range check comes first and `limit` is only consulted for a number that was
/// otherwise fine.
fn checked_extent(width: u32, height: u32, limit: u32) -> Result<(GLsizei, GLsizei), Error> {
    let extent = |v: u32| (v > 0).then(|| GLsizei::try_from(v).ok()).flatten();
    let (Some(w), Some(h)) = (extent(width), extent(height)) else {
        return Err(Error::Extent {
            size: (width, height),
        });
    };
    if width > limit || height > limit {
        return Err(Error::TextureSize {
            size: (width, height),
            limit,
        });
    }
    Ok((w, h))
}

impl Texture {
    /// Allocate a `width`×`height` texture with immutable storage.
    ///
    /// Both extents must be positive, fit `GLsizei`, and be within this
    /// driver's `GL_MAX_TEXTURE_SIZE`; anything else is [`Error::Extent`] or
    /// [`Error::TextureSize`] rather than a truncating cast or a doomed
    /// allocation. `glTexStorage2D` is then checked for real — see
    /// [`Error::Storage`] for why that one call gets a `glGetError` when the
    /// render path deliberately does not.
    ///
    /// **The storage this allocates is left undefined, not zeroed**
    /// (`glTexStorage2D`'s own contract). An earlier revision (#1298)
    /// overwrote it with zeroes here as defence in depth, on the theory that
    /// a pipeline sampling a texel no pass has written yet is the hardest
    /// possible bug to reproduce. PR #1299 review MED 2 measured that
    /// defence against its actual cost and dropped it: every caller in this
    /// tree already overwrites the whole texture before sampling it —
    /// verified by filling `preem_gl`'s three auxiliary textures with `0xb7`
    /// instead of leaving them undefined, which left the parity harness at
    /// `PASS all 148`, byte for byte — so the zero fill was pure cost, not
    /// insurance. That cost was not fixed, either: `hytte-ui`'s shader
    /// widget (`shader_surface.rs`) re-creates its data texture whenever a
    /// plugin's wire-controlled `data_size` moves, so a zero fill here would
    /// run on a path an out-of-process plugin can drive every frame, and
    /// `checked_extent`'s own limit is the driver's `GL_MAX_TEXTURE_SIZE` —
    /// not this shell's wire caps — so the fill this constructor used to do
    /// could reach a host allocation in the hundreds of megabytes for a
    /// single call, gated by nothing this crate owns. A caller that
    /// genuinely needs zeroed storage (a seed texture nothing writes before
    /// it is sampled) is expected to upload its own zero buffer through
    /// [`Texture::upload_u8`]/[`Texture::upload_f32`] — the same call every
    /// other caller already makes.
    ///
    /// # Errors
    ///
    /// [`Error::Extent`], [`Error::TextureSize`] or [`Error::Storage`], per
    /// above. Every one of them leaves **no** GL object behind: the failing
    /// paths either allocate nothing or delete what they allocated.
    pub fn new(gl_ctx: &Gl, format: Format, width: u32, height: u32) -> Result<Self, Error> {
        let (w, h) = checked_extent(width, height, max_texture_size(gl_ctx))?;
        let (internal, _, _) = format.as_gl();
        // Anything already queued belongs to whoever queued it; draining first
        // is what makes the check below an answer about *this* allocation
        // rather than about the last thing that went wrong anywhere.
        //
        // **The drain is destructive, and that is accepted, not overlooked.**
        // `glGetError` has no peek, so an error GTK left queued on this
        // shared context is consumed here and its owner will not see it.
        // Nothing in GTK/GSK reads `glGetError` to decide anything outside
        // its debug paths, and the alternative — leaving it queued — is
        // attributing GTK's error to this allocation and refusing a texture
        // that was fine. What is discarded is still written down, at debug.
        if let Some(code) = gl_ctx.take_error() {
            tracing::debug!(
                code,
                "discarded a GL error queued before this texture allocation"
            );
        }
        let mut id: GLuint = 0;
        // SAFETY: a context is current. `GenTextures` writes one id through a
        // pointer to a live local; the rest operate on that id while it is
        // bound, with extents already validated as positive `GLsizei`s within
        // the driver's own limit.
        unsafe {
            gl::GenTextures(1, &raw mut id);
            gl::BindTexture(gl::TEXTURE_2D, id);
            gl::TexStorage2D(gl::TEXTURE_2D, 1, internal, w, h);
        }
        // Before the parameter calls, so nothing else can queue an error that
        // would be read as this allocation's.
        if let Some(code) = gl_ctx.take_error() {
            // SAFETY: a context is current and `id` is the name `GenTextures`
            // just wrote — deleting it is how this path leaves nothing behind.
            unsafe {
                gl::BindTexture(gl::TEXTURE_2D, 0);
                gl::DeleteTextures(1, &raw const id);
            }
            return Err(Error::Storage {
                size: (width, height),
                code,
            });
        }
        // SAFETY: a context is current and `id` is bound, with storage the call
        // above allocated successfully. Every parameter is a documented
        // `GL_TEXTURE_2D` enum pair.
        unsafe {
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_MIN_FILTER,
                gl::NEAREST.cast_signed(),
            );
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_MAG_FILTER,
                gl::NEAREST.cast_signed(),
            );
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_S,
                gl::CLAMP_TO_EDGE.cast_signed(),
            );
            gl::TexParameteri(
                gl::TEXTURE_2D,
                gl::TEXTURE_WRAP_T,
                gl::CLAMP_TO_EDGE.cast_signed(),
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
        Ok(Self {
            id,
            width,
            height,
            format,
            owner: owner_now(gl_ctx),
            _not_send: PhantomData,
        })
    }

    /// This texture's `(width, height)`.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Overwrite the whole texture with `values`, which must be exactly
    /// `width * height` elements of this texture's [`Format`].
    ///
    /// Short input is padded with zero and long input is truncated, rather than
    /// handing GL a buffer smaller than the region it is told to read — the
    /// same defensive posture `PixelSurface` takes with an inconsistent RGBA
    /// buffer.
    pub fn upload_f32(&self, _gl: &Gl, values: &[f32]) {
        debug_assert_eq!(
            self.format,
            Format::R32f,
            "upload_f32 wants an R32F texture"
        );
        let wanted = (self.width as usize) * (self.height as usize);
        let mut staged;
        let data = if values.len() == wanted {
            values
        } else {
            staged = vec![0.0f32; wanted];
            let take = values.len().min(wanted);
            staged[..take].copy_from_slice(&values[..take]);
            &staged
        };
        let (_, transfer, kind) = self.format.as_gl();
        // SAFETY: a context is current, `self.id` is live, and `data` holds
        // exactly `width * height` elements — the region the call names — for
        // the duration of the call.
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.id);
            Self::reset_unpack_state();
            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                GLsizei::try_from(self.width).unwrap_or(0),
                GLsizei::try_from(self.height).unwrap_or(0),
                transfer,
                kind,
                data.as_ptr().cast(),
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
    }

    /// Overwrite the whole texture with `bytes`, for a **byte-typed** format
    /// ([`Format::R8`] or [`Format::Rgba8`]).
    ///
    /// Short input is padded with zero and long input is truncated, exactly as
    /// [`upload_f32`](Self::upload_f32) does and for the same reason: handing GL
    /// a buffer smaller than the region the call names is undefined behaviour,
    /// and the caller here is #893's shader widget, whose buffer arrives from an
    /// out-of-process plugin. The host validates the length before it gets here;
    /// this is the backstop that makes a validation bug a wrong picture rather
    /// than a read past the end.
    ///
    /// The unpack pixel store is put into a known position first — see
    /// `Texture::reset_unpack_state` for which five knobs that is and why
    /// setting `UNPACK_ALIGNMENT` alone (all this used to do) was not enough.
    pub fn upload_u8(&self, _gl: &Gl, bytes: &[u8]) {
        debug_assert_ne!(
            self.format,
            Format::R32f,
            "upload_u8 wants a byte-typed texture"
        );
        let wanted = (self.width as usize) * (self.height as usize) * self.format.bytes_per_texel();
        let mut staged;
        let data = if bytes.len() == wanted {
            bytes
        } else {
            staged = vec![0u8; wanted];
            let take = bytes.len().min(wanted);
            staged[..take].copy_from_slice(&bytes[..take]);
            &staged
        };
        let (_, transfer, kind) = self.format.as_gl();
        // SAFETY: a context is current, `self.id` is live, and `data` holds
        // exactly `width * height * bytes_per_texel` bytes — the region the call
        // names — for the duration of the call.
        unsafe {
            gl::BindTexture(gl::TEXTURE_2D, self.id);
            Self::reset_unpack_state();
            gl::TexSubImage2D(
                gl::TEXTURE_2D,
                0,
                0,
                0,
                GLsizei::try_from(self.width).unwrap_or(0),
                GLsizei::try_from(self.height).unwrap_or(0),
                transfer,
                kind,
                data.as_ptr().cast(),
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
    }

    /// Put **the five unpack pixel-store knobs this crate's uploads
    /// depend on** into the position the two uploads above assume,
    /// immediately before the `glTexSubImage2D` that reads it.
    ///
    /// [`reset_fixed_function_state`]'s argument, one state group over
    /// (#1298): GTK renders its own scene into this same context and nothing
    /// promises what it leaves set, so the unpack state is *put* into a known
    /// position rather than inherited. Setting only `UNPACK_ALIGNMENT` — which
    /// is all these two calls used to do — defends exactly one of the five
    /// knobs that decide which bytes `glTexSubImage2D` actually reads:
    ///
    /// * `UNPACK_ALIGNMENT` — GL's default of 4 reads an odd-width `R8` row
    ///   staggered. Already defended; kept here so all five sit together.
    /// * `UNPACK_SKIP_PIXELS` / `UNPACK_SKIP_ROWS` — a non-zero value
    ///   **offsets the source pointer**, so the upload starts one texel (or one
    ///   row) into its own data and reads that many past the end of the slice.
    ///   This is the one that bites hardest here: every `preem_gl` kind encodes
    ///   its geometry into a 1-D `R32F` strip, so a one-texel slide turns each
    ///   cell's origin into its code.
    /// * `UNPACK_ROW_LENGTH` — the source stride. Inert for the 1-row strips
    ///   this tree uploads (there is no second row to stride to), but not for
    ///   the shader widget's 2-D buffers.
    /// * `GL_PIXEL_UNPACK_BUFFER` — with a buffer bound, the `data` pointer is
    ///   reinterpreted as a **byte offset into that buffer** and the caller's
    ///   slice is never read at all. GTK4 may upload its own textures through
    ///   PBOs on some paths, which is one way this could arrive set — asserted
    ///   from the GL spec, not yet observed in a live session; see
    ///   `docs/live-verify.md`, "#893 stage B", for the item that would turn
    ///   it into an observation.
    ///
    /// **Deliberately out of scope**, named rather than silently absent so
    /// the five above read as a considered list rather than "all of them":
    ///
    /// * `GL_UNPACK_SWAP_BYTES` / `GL_UNPACK_LSB_FIRST` — desktop-GL-only
    ///   knobs with no GLES equivalent. Setting either here would be
    ///   `GL_INVALID_ENUM` on the GLES context this crate is pinned to
    ///   (`gl_surface.rs` negotiates `GLAPI::GLES`), so there is nothing
    ///   portable to defend.
    /// * `GL_UNPACK_IMAGE_HEIGHT` / `GL_UNPACK_SKIP_IMAGES` — 3-D-texture
    ///   knobs (`glTexSubImage3D`'s pixel store, GLES 3.0+). This crate has no
    ///   3-D texture type; nothing here ever issues that call.
    ///
    /// Cheap enough to do unconditionally: one unbind and four enum-only
    /// `glPixelStorei` calls per upload, against a call that already touches
    /// the texture.
    ///
    /// **Not** restored afterwards, deliberately — the same posture
    /// [`reset_fixed_function_state`] takes, and for the same reason. A
    /// save/restore pair would make this crate responsible for state GTK never
    /// promised to leave alone; GTK sets what it needs before its own uploads,
    /// exactly as this does.
    fn reset_unpack_state() {
        // SAFETY: a context is current and every call takes only enum constants
        // or small integers — none dereferences a pointer. The `BindBuffer`
        // names the reserved zero, which is "no buffer bound" rather than an
        // object id, so it cannot disturb a live buffer handle.
        unsafe {
            gl::BindBuffer(gl::PIXEL_UNPACK_BUFFER, 0);
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 0);
            gl::PixelStorei(gl::UNPACK_SKIP_PIXELS, 0);
            gl::PixelStorei(gl::UNPACK_SKIP_ROWS, 0);
        }
    }

    /// **Test-only.** Leave the unpack pixel store dirty in exactly the way
    /// `reset_unpack_state` exists to fix, so a live-driver test can prove
    /// the reset call actually runs rather than merely being present in the
    /// source.
    ///
    /// Gated behind `system-tests` because setting these is the reproduction
    /// of #1298's own bug, not something any shipped path should ever do —
    /// see `hytte-ui`'s `gl_surface` gated tests for the round-trip this
    /// backs.
    #[cfg(feature = "system-tests")]
    pub fn dirty_unpack_state_for_test(_gl: &Gl) {
        // SAFETY: as `reset_unpack_state` — enum constants and small integers
        // only.
        unsafe {
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 4);
            gl::PixelStorei(gl::UNPACK_ROW_LENGTH, 7);
            gl::PixelStorei(gl::UNPACK_SKIP_PIXELS, 3);
            gl::PixelStorei(gl::UNPACK_SKIP_ROWS, 2);
        }
    }

    /// **Test-only.** This texture's raw GL name, so a live-driver test can
    /// ask GL ([`is_texture_for_test`]) whether the object outlived its
    /// handle — the only observable difference between `Drop` deleting it and
    /// leaking it (invariant 2). Gated behind `system-tests` with the other
    /// seams of that kind.
    #[cfg(feature = "system-tests")]
    #[must_use]
    pub fn name_for_test(&self) -> u32 {
        self.id
    }

    /// Bind this texture to sampler `unit`.
    pub fn bind_unit(&self, _gl: &Gl, unit: u32) {
        // SAFETY: a context is current and `self.id` is live. `TEXTURE0 + unit`
        // is within range for any unit a caller in this tree uses (the pipeline
        // caps inputs well below `GL_MAX_TEXTURE_IMAGE_UNITS`'s guaranteed 16).
        unsafe {
            gl::ActiveTexture(gl::TEXTURE0 + unit);
            gl::BindTexture(gl::TEXTURE_2D, self.id);
        }
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        let id = self.id;
        release("texture", id, self.owner, loader::current_context, || {
            // SAFETY: `id` is this handle's own texture, read into a live local
            // for the out-pointer, and `release` runs this only under the
            // context that allocated it (or, for a handle the probe could not
            // attribute, on the caller's word — the module contract,
            // invariant 2).
            unsafe { gl::DeleteTextures(1, &raw const id) };
        });
    }
}

// ── framebuffers ────────────────────────────────────────────────────────────

/// A framebuffer object used as a render target for offscreen passes.
///
/// One FBO is re-attached to whichever [`Texture`] a pass writes, rather than
/// one FBO per texture: attaching is cheap, and a single object keeps the
/// widget's teardown to one delete.
#[derive(Debug)]
pub struct Framebuffer {
    /// **`NonZeroU32`, and that is load-bearing** (#1180 item 4): FBO name 0
    /// is the window-system-provided framebuffer — the one `GtkGLArea` is
    /// compositing — so a handle built over a `glGenFramebuffers` that wrote
    /// nothing would point [`draw_to`](Framebuffer::draw_to) at the visible
    /// surface instead of at a texture, with no error anywhere.
    id: NonZeroU32,
    /// The context this FBO was generated in — see [`release`]. Load-bearing
    /// here in a way it is not for a texture: an FBO is a container object,
    /// so another context's FBO of the same name is a *different object*,
    /// possibly the one GTK composites.
    owner: Option<ContextId>,
    _not_send: PhantomData<*const ()>,
}

impl Framebuffer {
    /// Create an FBO with nothing attached.
    ///
    /// # Errors
    ///
    /// [`Error::Name`] if `glGenFramebuffers` left the reserved name `0`.
    /// `glGen*` reports failure no other way, and for this object in
    /// particular the fallback is not "no framebuffer" but "the default
    /// framebuffer" — see that variant.
    pub fn new(gl_ctx: &Gl) -> Result<Self, Error> {
        let mut id: GLuint = 0;
        // SAFETY: a context is current; the out-pointer is a live local.
        // Whatever GL writes there is only *used* after the `NonZeroU32`
        // conversion below refuses the reserved zero.
        unsafe { gl::GenFramebuffers(1, &raw mut id) };
        Ok(Self {
            id: NonZeroU32::new(id).ok_or(Error::Name {
                object: "framebuffer",
            })?,
            owner: owner_now(gl_ctx),
            _not_send: PhantomData,
        })
    }

    /// Bind this FBO and point colour attachment 0 at `target`, leaving it
    /// bound for the draw that follows and setting the viewport to the
    /// texture's own extent.
    pub fn draw_to(&self, gl_ctx: &Gl, target: &Texture) -> Result<(), Error> {
        // SAFETY: a context is current, `self.id` is a live FBO — non-zero by
        // construction, so this can never bind the default framebuffer — and
        // `target.id` a live 2-D texture with level 0 allocated.
        let status = unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.id.get());
            gl::FramebufferTexture2D(
                gl::FRAMEBUFFER,
                gl::COLOR_ATTACHMENT0,
                gl::TEXTURE_2D,
                target.id,
                0,
            );
            gl::CheckFramebufferStatus(gl::FRAMEBUFFER)
        };
        if status != gl::FRAMEBUFFER_COMPLETE {
            return Err(Error::Framebuffer { status });
        }
        let (w, h) = target.size();
        viewport(gl_ctx, 0, 0, w, h);
        Ok(())
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        let id = self.id.get();
        release(
            "framebuffer",
            id,
            self.owner,
            loader::current_context,
            || {
                // SAFETY: `id` is this handle's own FBO, read out of the
                // `NonZeroU32` into a live local for the out-pointer, and
                // `release` runs this only under the context that generated it —
                // the one context in which `id` names *this* FBO (or, for a handle
                // the probe could not attribute, on the caller's word — the module
                // contract, invariant 2).
                unsafe { gl::DeleteFramebuffers(1, &raw const id) };
            },
        );
    }
}

// Deliberately absent: a `bind_framebuffer` / `current_draw_framebuffer` pair
// for getting back to what `GtkGLArea` was rendering into after the offscreen
// passes. **GTK does not render into framebuffer 0** — a `GtkGLArea` renders
// into its own FBO so GSK can import the result as a texture, so restoring "the
// default framebuffer" by binding `0` paints into nothing at all, which is the
// single most expensive mistake available on this path. The way back is
// `gtk_gl_area_attach_buffers`, which is GTK's own documented answer and needs
// no capture; `hytte-ui`'s `gl_surface` calls it, with that hazard written down
// at the call site. A capture/restore pair here would be a second, worse answer
// to a question GTK has already answered.

// ── vertex arrays and draws ─────────────────────────────────────────────────

/// A vertex array object.
///
/// Every draw in this crate generates its geometry from `gl_VertexID` /
/// `gl_InstanceID` in the vertex shader, so there is nothing to describe — but
/// a core profile still refuses to draw with no VAO bound, so one empty object
/// is kept for the life of the surface.
#[derive(Debug)]
pub struct VertexArray {
    /// `NonZeroU32` for [`Framebuffer::id`]'s reason (#1180 item 4), one
    /// notch less dangerous: VAO name 0 is the default vertex array, which a
    /// core profile refuses to draw with, so a failed `glGenVertexArrays`
    /// costs the draw rather than redirecting it.
    id: NonZeroU32,
    /// The context this VAO was generated in — see [`release`], and
    /// [`Framebuffer::owner`] for why a container object needs it most.
    owner: Option<ContextId>,
    _not_send: PhantomData<*const ()>,
}

impl VertexArray {
    /// Create an empty VAO.
    ///
    /// # Errors
    ///
    /// [`Error::Name`] if `glGenVertexArrays` left the reserved name `0`.
    pub fn new(gl_ctx: &Gl) -> Result<Self, Error> {
        let mut id: GLuint = 0;
        // SAFETY: a context is current; the out-pointer is a live local.
        // Whatever GL writes there is only *used* after the `NonZeroU32`
        // conversion below refuses the reserved zero.
        unsafe { gl::GenVertexArrays(1, &raw mut id) };
        Ok(Self {
            id: NonZeroU32::new(id).ok_or(Error::Name {
                object: "vertex array",
            })?,
            owner: owner_now(gl_ctx),
            _not_send: PhantomData,
        })
    }

    /// Bind it.
    pub fn bind(&self, _gl: &Gl) {
        // SAFETY: a context is current and `self.id` is a live VAO, non-zero
        // by construction, so this never re-binds the default vertex array.
        unsafe { gl::BindVertexArray(self.id.get()) };
    }

    /// **Test-only.** This VAO's raw GL name — [`Texture::name_for_test`]'s
    /// twin, read back through [`is_vertex_array_for_test`]. The VAO rather
    /// than the FBO is the container object a live test can catch the
    /// cross-context hazard on, because Mesa keeps framebuffer names in the
    /// share group's state (two shared contexts never number their FBOs
    /// alike there) but vertex-array names per context, as GL specifies.
    #[cfg(feature = "system-tests")]
    #[must_use]
    pub fn name_for_test(&self) -> u32 {
        self.id.get()
    }
}

impl Drop for VertexArray {
    fn drop(&mut self) {
        let id = self.id.get();
        release(
            "vertex array",
            id,
            self.owner,
            loader::current_context,
            || {
                // SAFETY: `id` is this handle's own VAO, read out of the
                // `NonZeroU32` into a live local for the out-pointer, and `release`
                // runs this only under the context that generated it (or, for a
                // handle the probe could not attribute, on the caller's word — the
                // module contract, invariant 2).
                unsafe { gl::DeleteVertexArrays(1, &raw const id) };
            },
        );
    }
}

/// How a draw combines with what is already in the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blend {
    /// Overwrite. What a decay pass or a blit wants.
    Replace,
    /// Componentwise `max(src, dst)` (`glBlendEquation(GL_MAX)`).
    ///
    /// The kit's phosphor stamp is `phosphor[i] = phosphor[i].max(intensity)`,
    /// so this is not an approximation of it — it is the same operation. See
    /// [`Format::R8`] for why the target must be a normalized format for this
    /// to be available at all.
    Max,
}

/// Set the blend mode for subsequent draws.
pub fn set_blend(_gl: &Gl, blend: Blend) {
    // SAFETY: a context is current; both calls take only enum constants.
    unsafe {
        match blend {
            Blend::Replace => gl::Disable(gl::BLEND),
            Blend::Max => {
                gl::Enable(gl::BLEND);
                gl::BlendEquation(gl::MAX);
                gl::BlendFunc(gl::ONE, gl::ONE);
            }
        }
    }
}

/// Put the fixed-function state this crate depends on into a known position.
///
/// **Everything a pass writes goes through these**, and GTK owns the same
/// context: it renders its own scene into the `GtkGLArea`'s FBO with whatever
/// state its renderer wants, and nothing promises what is left set when a
/// `render` handler is entered. Each of the four below is a defect that would
/// be invisible in code review and obvious only on glass, so they are set once
/// per render rather than assumed:
///
/// * **`GL_DITHER`** — enabled by default in both GL and GLES, and the one
///   thing that can break the exactness argument [`Format::R8`] rests on: a
///   driver that actually dithers an 8-bit fixed-point write may perturb the
///   stored value by one LSB, and a phosphor decayed by `(v * retained) >> 8`
///   cannot survive that — the error compounds every step rather than washing
///   out. Every mainstream desktop driver no-ops dithering at 8 bits per
///   channel, which is not the same as "cannot happen".
/// * **`GL_SCISSOR_TEST`** — a live scissor rect would silently clip an
///   offscreen pass to whatever region GTK last drew. It was only ever off here
///   as a side effect of [`clear`] disabling it, which made the *ordering* of
///   the first clear load-bearing and written down nowhere.
/// * **`glColorMask`** — a masked-off channel writes nothing, so a phosphor
///   pass would read back its own previous contents and a blit would drop a
///   colour. Never set by this crate before, i.e. inherited.
/// * **`GL_DEPTH_TEST`/`GL_STENCIL_TEST`** — the surface asks for neither
///   buffer, but an *enabled* test against an absent buffer is a driver's
///   choice, not a no-op by definition.
///
/// `GL_FRAMEBUFFER_SRGB` is deliberately **not** here: it is not core GLES
/// (only `EXT_sRGB_write_control`), and the surface pins a GLES context, so
/// there is nothing portable to pin. Whether GTK hands us a linear or an sRGB
/// framebuffer is the one colour-space question this design could not settle
/// from the sources, and it is a live-verify item (`docs/live-verify.md`,
/// "#893 stage B" item 1) rather than something this call could fix.
pub fn reset_fixed_function_state(_gl: &Gl) {
    // SAFETY: a context is current and every call takes only enum constants or
    // plain scalars — none dereferences a pointer or names an object id.
    unsafe {
        gl::Disable(gl::DITHER);
        gl::Disable(gl::SCISSOR_TEST);
        gl::Disable(gl::DEPTH_TEST);
        gl::Disable(gl::STENCIL_TEST);
        gl::ColorMask(gl::TRUE, gl::TRUE, gl::TRUE, gl::TRUE);
    }
}

/// Set the viewport, refusing an extent that does not fit `GLsizei`.
pub fn viewport(_gl: &Gl, x: i32, y: i32, width: u32, height: u32) {
    let w = GLsizei::try_from(width).unwrap_or(0);
    let h = GLsizei::try_from(height).unwrap_or(0);
    // SAFETY: a context is current; a zero extent is legal (it draws nothing),
    // which is the fallback a non-representable extent takes.
    unsafe { gl::Viewport(x, y, w, h) };
}

/// Clear the bound framebuffer's colour to `rgba`.
///
/// The scissor is disabled here **as well as** in
/// [`reset_fixed_function_state`], and that repetition is deliberate: a clear
/// is the one operation whose whole contract is "the entire target, whatever
/// was there", so it must not depend on a caller having reset the state first.
/// Before that function existed this call was the *only* place the scissor was
/// touched, which made "`Resources::build` clears before the first pass" a
/// load-bearing ordering that nothing wrote down.
pub fn clear(_gl: &Gl, rgba: [f32; 4]) {
    // SAFETY: a context is current; every call takes only scalars or enums.
    unsafe {
        gl::Disable(gl::SCISSOR_TEST);
        gl::ClearColor(rgba[0], rgba[1], rgba[2], rgba[3]);
        gl::Clear(gl::COLOR_BUFFER_BIT);
    }
}

/// Draw one full-screen triangle.
///
/// A single oversized triangle rather than two triangles: it covers the
/// viewport with no diagonal seam, so a fragment is never rasterised twice —
/// which matters here because the passes that use it are exact per-pixel
/// integer computations, and a doubly-shaded fragment under a `GL_MAX` blend
/// would be a silent inconsistency.
pub fn draw_fullscreen(_gl: &Gl) {
    // SAFETY: a context is current, a program and a VAO are bound (the caller's
    // contract), and the vertex shader generates all three positions from
    // `gl_VertexID`, so no attribute array is read.
    unsafe { gl::DrawArrays(gl::TRIANGLES, 0, 3) };
}

/// Draw `instances` quads, six vertices each, generated from `gl_VertexID` and
/// `gl_InstanceID`.
///
/// The per-column beam span: one instance per logical column, the vertex shader
/// resolving that column's row range and emitting the quad that covers it.
pub fn draw_quads(_gl: &Gl, instances: u32) {
    let Ok(count) = GLsizei::try_from(instances) else {
        return;
    };
    if count == 0 {
        return;
    }
    // SAFETY: as `draw_fullscreen`, with the instance count validated as a
    // non-negative `GLsizei`.
    unsafe { gl::DrawArraysInstanced(gl::TRIANGLES, 0, 6, count) };
}

/// Put the pack pixel store — the mirror image of
/// [`Texture::reset_unpack_state`], one call site's worth of knobs, on the
/// `glReadPixels` side rather than `glTexSubImage2D`'s — into a known
/// position before every readback.
///
/// The same argument, aimed the other way: `GL_PACK_ROW_LENGTH`,
/// `GL_PACK_SKIP_PIXELS` and `GL_PACK_SKIP_ROWS` all move where
/// `glReadPixels` **writes**, and a bound `GL_PIXEL_PACK_BUFFER` retargets the
/// write into that buffer at a byte offset instead of into `out` at all — so
/// a caller that left any of the four set would have `read_rgba8` write
/// outside the `width * height * 4`-byte region its caller allocated, not
/// merely read the wrong bytes. `PACK_SKIP_ROWS` alone was demonstrated as
/// exactly that: a corrupted heap and a `SIGABRT`, not a wrong picture (PR
/// #1299 review HIGH).
///
/// Not restored afterwards, for the same reason
/// [`Texture::reset_unpack_state`] and [`reset_fixed_function_state`] are
/// not: this crate is not responsible for state GTK never promised to leave
/// alone.
fn reset_pack_state() {
    // SAFETY: a context is current and every call takes only enum constants
    // or small integers — none dereferences a pointer. The `BindBuffer` names
    // the reserved zero, which is "no buffer bound" rather than an object id,
    // so it cannot disturb a live buffer handle.
    unsafe {
        gl::BindBuffer(gl::PIXEL_PACK_BUFFER, 0);
        gl::PixelStorei(gl::PACK_ALIGNMENT, 1);
        gl::PixelStorei(gl::PACK_ROW_LENGTH, 0);
        gl::PixelStorei(gl::PACK_SKIP_PIXELS, 0);
        gl::PixelStorei(gl::PACK_SKIP_ROWS, 0);
    }
}

/// **Test-only.** Leave the pack pixel store dirty in exactly the way
/// `reset_pack_state` exists to fix, so a live-driver test can prove the
/// reset call actually runs rather than merely being present in the source.
///
/// Gated behind `system-tests` because setting these is the reproduction of
/// PR #1299's HIGH finding, not something any shipped path should ever do —
/// see `hytte-ui`'s `gl_surface` gated tests for the round-trip this backs.
#[cfg(feature = "system-tests")]
pub fn dirty_pack_state_for_test(_gl: &Gl) {
    // SAFETY: as `reset_pack_state` — enum constants and small integers only.
    unsafe {
        gl::PixelStorei(gl::PACK_ALIGNMENT, 4);
        gl::PixelStorei(gl::PACK_ROW_LENGTH, 7);
        gl::PixelStorei(gl::PACK_SKIP_PIXELS, 3);
        gl::PixelStorei(gl::PACK_SKIP_ROWS, 2);
    }
}

/// **Test-only.** Whether `name` is a texture object in the **current**
/// context (`glIsTexture`) — how a live-driver test tells a handle that
/// deleted its object from one that leaked it (invariant 2).
#[cfg(feature = "system-tests")]
#[must_use]
pub fn is_texture_for_test(_gl: &Gl, name: u32) -> bool {
    // SAFETY: a context is current (the token); `glIsTexture` takes a plain
    // name, dereferences nothing, and answers `GL_FALSE` for one that is not
    // a texture.
    unsafe { gl::IsTexture(name) == gl::TRUE }
}

/// **Test-only.** The `EGLContext` address current on this thread, with no
/// generation, or `None` where the probe sees no context.
///
/// For a live-driver test to report whether the driver actually **reused** an
/// address across a context recreate — the precondition of #1562's hazard,
/// which a test cannot arrange, only observe. Never compared against a
/// handle's owner by anything shipped: the owner check compares the address
/// *and* its generation.
#[cfg(feature = "system-tests")]
#[must_use]
pub fn current_context_address_for_test(_gl: &Gl) -> Option<usize> {
    loader::current_address()
}

/// **Test-only.** The generation an `EGLContext` address is at now — how many
/// times a context there has been retired (#1562).
///
/// So a live-driver test can assert that a widget's `unrealize` retired the
/// context it was about to lose, which no drop can show unless the driver
/// happens to reuse the address. Takes no `Gl` token because it touches no GL
/// state: it reads this crate's own table, and is asked after the context is
/// gone.
#[cfg(feature = "system-tests")]
#[must_use]
pub fn context_generation_for_test(address: usize) -> u64 {
    std::num::NonZeroUsize::new(address).map_or(0, loader::generation_of)
}

/// **Test-only.** Whether `name` is a vertex array object in the **current**
/// context (`glIsVertexArray`) — [`is_texture_for_test`]'s twin, and the one
/// that sees the container-object hazard: another context's VAO of the same
/// name is a different object, so this is asked per context.
#[cfg(feature = "system-tests")]
#[must_use]
pub fn is_vertex_array_for_test(_gl: &Gl, name: u32) -> bool {
    // SAFETY: as `is_texture_for_test`, for `glIsVertexArray`.
    unsafe { gl::IsVertexArray(name) == gl::TRUE }
}

/// Read the bound framebuffer's colour back as RGBA8, row-major, bottom-up (GL
/// order — the caller flips).
///
/// **The parity harness's, never the shell's.** A readback is a full pipeline
/// stall — it is precisely what the design spec rejects for the render path
/// ("rendering to an FBO and `glReadPixels`-ing into `Arc<[u8]>` … would defeat
/// the entire point"). It exists so `hytte-ui`'s `preem_gl_diff` example can
/// measure GL against the CPU kit; nothing on the shell's per-frame path calls
/// it, and nothing should.
#[must_use]
pub fn read_rgba8(_gl: &Gl, width: u32, height: u32) -> Vec<u8> {
    let (Ok(w), Ok(h)) = (GLsizei::try_from(width), GLsizei::try_from(height)) else {
        return Vec::new();
    };
    let len = (width as usize) * (height as usize) * 4;
    let mut out = vec![0u8; len];
    if len == 0 {
        return out;
    }
    reset_pack_state();
    // SAFETY: a context is current, a complete framebuffer is bound, and
    // `out` holds exactly `width * height * 4` bytes — the region `RGBA`/
    // `UNSIGNED_BYTE` at pack alignment 1, row length 0, skip pixels/rows 0
    // and no `PIXEL_PACK_BUFFER` bound writes for a `w`×`h` read. That is
    // only true because `reset_pack_state` was just called: any one of those
    // four inherited knobs would move where this write lands, not just what
    // it contains (PR #1299 review HIGH).
    unsafe {
        gl::ReadPixels(
            0,
            0,
            w,
            h,
            gl::RGBA,
            gl::UNSIGNED_BYTE,
            out.as_mut_ptr().cast(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Blend, ContextId, Error, Format, Framebuffer, HashMap, NonZeroU32, PhantomData, Program,
        RefCell, Stage, UNKNOWN_MAX_TEXTURE_SIZE, VertexArray, checked_extent, loader, release,
        usable_limit,
    };
    use std::cell::Cell;

    /// **#977.** The extent decision, which is the half of the driver check a
    /// hermetic test can reach — CI has no GL at all, so `Texture::new`'s
    /// `glGetError` half is live-verify.
    ///
    /// Three separate facts, and they are different errors on purpose: a zero
    /// or `GLsizei`-overflowing side is [`Error::Extent`] whatever the driver
    /// says; a legal side over the driver's limit is [`Error::TextureSize`],
    /// which carries the limit because the limit is the useful half of the
    /// message; and everything inside both is `Ok`.
    ///
    /// The `32768 × 1` case is #977's own failing input: 32 KiB of `R8` data,
    /// three orders of magnitude under the wire's byte cap, and wider than a
    /// great many parts will allocate. Before this check it returned `Ok` on a
    /// texture with no storage.
    ///
    /// **Falsified** by dropping the `width > limit || height > limit` guard
    /// (the two `TextureSize` assertions become `Ok`), or by moving it above
    /// the range check (the zero case reports the wrong error).
    #[test]
    fn the_extent_check_separates_a_bad_number_from_a_bad_driver_fit() {
        assert_eq!(checked_extent(16, 1, 4096), Ok((16, 1)));
        assert_eq!(checked_extent(4096, 4096, 4096), Ok((4096, 4096)), "at it");

        assert_eq!(
            checked_extent(32768, 1, 4096),
            Err(Error::TextureSize {
                size: (32768, 1),
                limit: 4096
            }),
            "#977's failing input: legal bytes, unallocatable grid",
        );
        assert_eq!(
            checked_extent(1, 8192, 4096),
            Err(Error::TextureSize {
                size: (1, 8192),
                limit: 4096
            }),
            "the height axis is checked too, not only the width",
        );

        for (w, h) in [(0, 1), (1, 0), (0, 0)] {
            assert_eq!(
                checked_extent(w, h, 4096),
                Err(Error::Extent { size: (w, h) }),
                "a zero side is a bad number, not a bad fit",
            );
        }
        let over = u32::MAX;
        assert_eq!(
            checked_extent(over, 1, over),
            Err(Error::Extent { size: (over, 1) }),
            "…and so is one that does not fit GLsizei, even under the limit",
        );
    }

    /// A driver that does not answer the query must not be read as a driver
    /// that refuses every texture — the sentinel declines to enforce and leaves
    /// the verdict to `glTexStorage2D`'s own error.
    ///
    /// **Falsified** by making `max_texture_size` cache a literal `0` for an
    /// unusable answer: every allocation in the shell then fails
    /// `TextureSize`, which is a blank shell built out of a missing
    /// measurement.
    #[test]
    fn an_unknown_driver_limit_refuses_nothing() {
        assert_eq!(
            checked_extent(65_536, 65_536, UNKNOWN_MAX_TEXTURE_SIZE),
            Ok((65_536, 65_536)),
        );
    }

    /// …and an unknown limit is **not memoized**: only a positive answer is.
    /// `max_texture_size` used to cache the sentinel for an unusable answer,
    /// so a first query made with no context bound (which writes nothing and
    /// leaves the out-parameter at `0`) switched the `GL_MAX_TEXTURE_SIZE`
    /// check off for the rest of the thread's life.
    ///
    /// **Falsified** by having `usable_limit` map `0` (or a negative) to the
    /// sentinel instead of `None` — which is exactly what memoizing
    /// "unknown" was.
    #[test]
    fn only_a_usable_driver_limit_is_worth_remembering() {
        assert_eq!(usable_limit(16_384), Some(16_384));
        assert_eq!(usable_limit(1), Some(1));
        assert_eq!(usable_limit(0), None, "the no-context answer");
        assert_eq!(usable_limit(-1), None, "nor a nonsense one");
    }

    /// **Invariant 2.** A handle deletes its object only under the context
    /// that made it, and leaks it otherwise — the decision every `Drop` in
    /// this crate delegates to [`release`], driven here with stand-in
    /// contexts and a delete that only counts.
    ///
    /// The four rows are the whole table: the owning context current (the
    /// normal case, which must keep deleting — a check that leaked here
    /// would be a GPU leak per resize); *another* context current (the
    /// container-object hazard: that context's FBO of the same name is a
    /// different object); *no* context current (a delete that can abort on
    /// the libepoxy route); and no recorded owner, which keeps the unchecked
    /// delete *without asking the probe*, so a `Drop` is never what first
    /// resolves it.
    ///
    /// **Falsified** by reverting any `Drop` to a bare `glDelete*` (not
    /// visible here — that is the gated `hytte-ui` test's job), or here by
    /// comparing anything but the owner (rows 2 and 3 delete), or by asking
    /// the probe before the `None` arm (row 4's probe count goes to 1).
    #[test]
    fn a_handle_deletes_only_under_the_context_that_made_it() {
        let a = ContextId::fake(0xa000);
        let b = ContextId::fake(0xb000);
        let deletes = Cell::new(0_u32);
        let probes = Cell::new(0_u32);
        let delete = || deletes.set(deletes.get() + 1);
        let probe = |answer: Option<ContextId>| {
            let probes = &probes;
            move || {
                probes.set(probes.get() + 1);
                answer
            }
        };

        assert!(release("texture", 1, Some(a), probe(Some(a)), delete));
        assert_eq!(deletes.get(), 1, "the owning context is current: delete");

        assert!(!release("framebuffer", 1, Some(a), probe(Some(b)), delete));
        assert_eq!(
            deletes.get(),
            1,
            "another context is current: its FBO 1 is not ours, so leak",
        );

        assert!(!release("vertex array", 1, Some(a), probe(None), delete));
        assert_eq!(deletes.get(), 1, "no context is current: leak, never abort");

        let asked = probes.get();
        assert!(release("program", 1, None, probe(None), delete));
        assert_eq!(
            deletes.get(),
            2,
            "no owner recorded: the pre-check unchecked delete"
        );
        assert_eq!(
            probes.get(),
            asked,
            "…decided without asking the probe, so no Drop is ever first to resolve it",
        );
    }

    /// **#1562.** The table above's second row, for the one "other context"
    /// an address-only owner check cannot see: the context EGL put at the
    /// owner's address after the owner was destroyed. With the owner retired
    /// first — what `Gl::retire_current_context` does, and what `hytte-ui`'s
    /// surfaces do at the end of `unrealize` — a handle made before the
    /// retire must leak, while a handle the newcomer makes at the same
    /// address must still delete.
    ///
    /// Through [`release`] itself, with the probe answering what
    /// `loader::current_context` would for that address now
    /// ([`loader::identify`]), so the generation is checked where the delete
    /// is decided rather than only in the loader's own table test.
    ///
    /// **Falsified** by `loader::identify` ignoring its table, i.e. #1543's
    /// address-only identity: the first `release` deletes.
    #[test]
    fn a_handle_made_before_its_context_was_retired_is_never_deleted() {
        let address = std::num::NonZeroUsize::new(0x1562_8000).expect("non-zero");
        let deletes = Cell::new(0_u32);
        let delete = || deletes.set(deletes.get() + 1);

        let outlived = loader::identify(address);
        loader::retire(outlived);
        let newcomer = || Some(loader::identify(address));

        assert!(!release(
            "vertex array",
            1,
            Some(outlived),
            newcomer,
            delete
        ));
        assert_eq!(
            deletes.get(),
            0,
            "a handle made in a retired context must not delete under the next context at the \
             same address: VAO 1 there is the newcomer's own",
        );

        let fresh = loader::identify(address);
        assert!(release("vertex array", 1, Some(fresh), newcomer, delete));
        assert_eq!(
            deletes.get(),
            1,
            "…while a handle the newcomer made itself still deletes, or every resize would leak",
        );
    }

    /// The same table, through a **real** `Drop`: handles whose owner no live
    /// context can match, dropped for real on a thread with no context
    /// current, must leak — which here means returning without one `gl::*`
    /// call. Before the owner check this test had to `mem::forget` its
    /// handles (see the one below's history), because their `Drop` issued a
    /// `glDelete*` with no context bound.
    ///
    /// What makes this an assertion rather than a smoke test is the program's
    /// memo: `location` on a NUL-carrying name must answer `-1` **without**
    /// reaching GL — a GL call here would either panic (the `gl` table is not
    /// loaded in this test binary unless the loader test got there first) or
    /// return the no-context stub's `0`, and either way the memo would hold
    /// the name.
    ///
    /// **Falsified** by dropping the NUL check from `Program::location` (the
    /// location is not `-1`, or the memo is not empty, or the call panics).
    #[test]
    fn a_uniform_name_with_a_nul_never_reaches_gl_and_unowned_drops_leak() {
        let program = Program {
            id: 1,
            locations: RefCell::new(HashMap::new()),
            owner: Some(ContextId::fake(0xdead_0000)),
            _not_send: PhantomData,
        };
        assert_eq!(
            program.location("u_time\0junk"),
            -1,
            "GL would stop at the NUL and resolve `u_time`; no uniform is named this",
        );
        assert!(
            program.locations.borrow().is_empty(),
            "…and the memo must not file a prefix's answer under the full name",
        );
        drop(program);
    }

    /// The three formats keep the enum triples the shaders are written against
    /// — a normalized `R8` (so `GL_MAX` blending is available at all), a
    /// straight-alpha `RGBA8` for #893's colour grids, and an unfiltered `R32F`
    /// data texture. Pure table check; no context needed.
    #[test]
    fn formats_map_to_the_enums_the_shaders_assume() {
        assert_eq!(Format::R8.as_gl(), (gl::R8, gl::RED, gl::UNSIGNED_BYTE));
        assert_eq!(
            Format::Rgba8.as_gl(),
            (gl::RGBA8, gl::RGBA, gl::UNSIGNED_BYTE)
        );
        assert_eq!(Format::R32f.as_gl(), (gl::R32F, gl::RED, gl::FLOAT));
    }

    /// The byte width the upload paths measure their input in. Wrong here and a
    /// staging buffer is the wrong size, which is a read past the end rather
    /// than a wrong picture — so it is worth a table of its own.
    ///
    /// **Falsified** by giving `Rgba8` a width of 1: the second assertion goes
    /// red.
    #[test]
    fn texel_widths_match_the_transfer_types() {
        assert_eq!(Format::R8.bytes_per_texel(), 1);
        assert_eq!(Format::Rgba8.bytes_per_texel(), 4);
        assert_eq!(Format::R32f.bytes_per_texel(), 4);
    }

    #[test]
    fn stages_map_to_the_gl_shader_enums() {
        assert_eq!(Stage::Vertex.as_gl(), gl::VERTEX_SHADER);
        assert_eq!(Stage::Fragment.as_gl(), gl::FRAGMENT_SHADER);
    }

    /// Every error prints something a journal line can carry: the driver log
    /// for the two shader failures, and the raw status/extent for the two the
    /// driver has no words for.
    #[test]
    fn errors_carry_their_diagnosis_into_display() {
        let compile = Error::Compile {
            stage: Stage::Fragment,
            log: "0:12: 'foo' : undeclared".to_owned(),
        };
        assert!(compile.to_string().contains("fragment"));
        assert!(compile.to_string().contains("undeclared"));
        assert!(
            Error::Framebuffer { status: 0x8cd6 }
                .to_string()
                .contains("0x8cd6")
        );
        assert!(Error::Extent { size: (0, 48) }.to_string().contains("0x48"));
        let too_big = Error::TextureSize {
            size: (32768, 1),
            limit: 4096,
        }
        .to_string();
        assert!(too_big.contains("32768x1"), "{too_big}");
        assert!(too_big.contains("4096"), "the limit it missed: {too_big}");
        let storage = Error::Storage {
            size: (32768, 1),
            code: 0x0501,
        }
        .to_string();
        assert!(storage.contains("32768x1"), "{storage}");
        assert!(storage.contains("0x501"), "the driver's code: {storage}");
        assert!(
            Error::Load {
                message: "libepoxy.so.0: not found".to_owned()
            }
            .to_string()
            .contains("libepoxy")
        );
        let unnamed = Error::Name {
            object: "framebuffer",
        }
        .to_string();
        assert!(unnamed.contains("framebuffer"), "{unnamed}");
        assert!(
            unnamed.contains('0'),
            "the reserved name is the whole diagnosis: {unnamed}",
        );
    }

    /// **#1180 item 4.** A failed `glGen*` cannot be mistaken for a live
    /// object, because the reserved name `0` is unrepresentable in the
    /// handle.
    ///
    /// `glGenFramebuffers`/`glGenVertexArrays` have no error return: on
    /// failure they leave the out-parameter at `0`, and `0` is not "no
    /// object" — for a framebuffer it is the **window-system-provided** one,
    /// so an FBO handle over a zero would have pointed `draw_to` at the
    /// surface `GtkGLArea` is compositing and every offscreen pass would have
    /// been drawn into the visible window, with no error raised anywhere.
    ///
    /// There is no way to make a driver fail to name an object on demand, so
    /// what is pinned here is the *type*: the field is a [`NonZeroU32`], so
    /// the only way to build either handle is through the constructor that
    /// refuses zero. **Falsified** by widening either `id` back to `GLuint`:
    /// this test stops compiling (`NonZeroU32::new(…)` no longer type-checks
    /// against the field), which is the point — the check cannot be dropped
    /// while the type still holds.
    #[test]
    fn a_framebuffer_and_a_vao_can_only_hold_a_non_zero_name() {
        // An owner no live context can match, so the real `Drop` below leaks
        // (no context is current on a test thread) instead of issuing a
        // `glDelete*` with nothing bound. Before invariant 2's owner check
        // this test had to `mem::forget` both handles to stay inside the
        // module contract; now the contract is the `Drop`'s to keep.
        let owner = Some(ContextId::fake(0x7000));
        let fbo = Framebuffer {
            id: NonZeroU32::new(7).expect("7 is not zero"),
            owner,
            _not_send: PhantomData,
        };
        let vao = VertexArray {
            id: NonZeroU32::new(9).expect("9 is not zero"),
            owner,
            _not_send: PhantomData,
        };
        assert_eq!(fbo.id.get(), 7);
        assert_eq!(vao.id.get(), 9);
        assert!(
            NonZeroU32::new(0).is_none(),
            "…and 0, the name glGen* leaves on failure, has no representation here",
        );
        drop(fbo);
        drop(vao);
    }

    /// `Blend` is a two-state knob and both states are named, so a future third
    /// mode is a compile error at `set_blend` rather than a silent fallthrough.
    #[test]
    fn blend_modes_are_distinct() {
        assert_ne!(Blend::Replace, Blend::Max);
    }
}
