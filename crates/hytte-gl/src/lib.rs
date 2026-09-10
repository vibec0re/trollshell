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
//! 2. **Handles outlive nothing.** [`Program`], [`Texture`], [`Framebuffer`]
//!    and [`VertexArray`] are RAII: `Drop` deletes the GL object. They are
//!    `!Send` for the same reason as the token, and **must be dropped while
//!    the creating context is still current**. `GtkGLArea` guarantees exactly
//!    that — it makes the context current before emitting `unrealize` — which
//!    is where `hytte-ui`'s widget drops them.
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
//! are identical; libepoxy exports both families from one dispatch table, so
//! one loader serves both. Shaders are compiled with an explicit version
//! header the caller supplies ([`Program::compile`]) rather than one baked into
//! the source, so the same GLSL body can be re-targeted without editing it.

mod loader;

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use gl::types::{GLenum, GLint, GLsizei, GLuint};

/// Anything that can go wrong on the way into GL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The GL entry points could not be resolved — see [`loader`]. Every
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
    /// the first one's verdict. See [`loader`] for why one load serves every
    /// context.
    pub fn current() -> Result<Self, Error> {
        loader::load()?;
        Ok(Self(PhantomData))
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

// ── programs ────────────────────────────────────────────────────────────────

/// A linked GLSL program, with its uniform locations memoized.
///
/// RAII: `Drop` calls `glDeleteProgram`. See the module's safety contract for
/// when that must happen.
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
    fn location(&self, name: &str) -> GLint {
        if let Some(found) = self.locations.borrow().get(name) {
            return *found;
        }
        let mut terminated = String::with_capacity(name.len() + 1);
        terminated.push_str(name);
        terminated.push('\0');
        // SAFETY: `self.id` is a live program and the pointer is to a
        // nul-terminated buffer that outlives the call.
        let location = unsafe { gl::GetUniformLocation(self.id, terminated.as_ptr().cast::<i8>()) };
        self.locations
            .borrow_mut()
            .insert(name.to_owned(), location);
        location
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        // SAFETY: `self.id` is this handle's own program and nothing else holds
        // it. The module's contract requires the creating context to still be
        // current here — which is what `GtkGLArea::unrealize` guarantees.
        unsafe { gl::DeleteProgram(self.id) };
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
        // exactly the lifetime wanted here.
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
/// process creates comes from the same driver through the same libepoxy
/// dispatch table (see [`loader`]), and GTK gives a display one share group. So
/// one query serves every `GdkGLContext`, exactly as one `gl::load_with` does.
/// The memo is per **thread** rather than process-wide because the [`Gl`] token
/// is `!Send` — a second thread with a context of its own has to make its own
/// claim anyway, and a `Cell` on the thread it is read from needs no
/// synchronisation on the path that reads it every allocation.
///
/// A driver answering `<= 0` is treated as *unknown*, not as *zero*: see
/// [`UNKNOWN_MAX_TEXTURE_SIZE`].
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
        let limit = u32::try_from(value).unwrap_or(0);
        let limit = if limit == 0 {
            UNKNOWN_MAX_TEXTURE_SIZE
        } else {
            limit
        };
        cached.set(Some(limit));
        tracing::debug!(limit, "GL_MAX_TEXTURE_SIZE");
        limit
    })
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
        let _ = gl_ctx.take_error();
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
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
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
    /// `UNPACK_ALIGNMENT` is set to 1 — an `R8` row of odd width is not
    /// 4-aligned, and GL's default of 4 would read the rows staggered.
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
            gl::PixelStorei(gl::UNPACK_ALIGNMENT, 1);
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
        // SAFETY: `self.id` is this handle's own texture; see the module
        // contract on when a handle may be dropped.
        unsafe { gl::DeleteTextures(1, &raw const self.id) };
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
    id: GLuint,
    _not_send: PhantomData<*const ()>,
}

impl Framebuffer {
    /// Create an FBO with nothing attached.
    #[must_use]
    pub fn new(_gl: &Gl) -> Self {
        let mut id: GLuint = 0;
        // SAFETY: a context is current; the out-pointer is a live local.
        unsafe { gl::GenFramebuffers(1, &raw mut id) };
        Self {
            id,
            _not_send: PhantomData,
        }
    }

    /// Bind this FBO and point colour attachment 0 at `target`, leaving it
    /// bound for the draw that follows and setting the viewport to the
    /// texture's own extent.
    pub fn draw_to(&self, gl_ctx: &Gl, target: &Texture) -> Result<(), Error> {
        // SAFETY: a context is current, `self.id` is a live FBO and `target.id`
        // a live 2-D texture with level 0 allocated.
        let status = unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.id);
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
        // SAFETY: `self.id` is this handle's own FBO; see the module contract.
        unsafe { gl::DeleteFramebuffers(1, &raw const self.id) };
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
    id: GLuint,
    _not_send: PhantomData<*const ()>,
}

impl VertexArray {
    /// Create an empty VAO.
    #[must_use]
    pub fn new(_gl: &Gl) -> Self {
        let mut id: GLuint = 0;
        // SAFETY: a context is current; the out-pointer is a live local.
        unsafe { gl::GenVertexArrays(1, &raw mut id) };
        Self {
            id,
            _not_send: PhantomData,
        }
    }

    /// Bind it.
    pub fn bind(&self, _gl: &Gl) {
        // SAFETY: a context is current and `self.id` is a live VAO.
        unsafe { gl::BindVertexArray(self.id) };
    }
}

impl Drop for VertexArray {
    fn drop(&mut self) {
        // SAFETY: `self.id` is this handle's own VAO; see the module contract.
        unsafe { gl::DeleteVertexArrays(1, &raw const self.id) };
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
    // SAFETY: a context is current, a complete framebuffer is bound, and `out`
    // holds exactly `width * height * 4` bytes — the region `RGBA`/
    // `UNSIGNED_BYTE` at pack alignment 1 writes for a `w`×`h` read.
    unsafe {
        gl::PixelStorei(gl::PACK_ALIGNMENT, 1);
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
    use super::{Blend, Error, Format, Stage, UNKNOWN_MAX_TEXTURE_SIZE, checked_extent};

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
    }

    /// `Blend` is a two-state knob and both states are named, so a future third
    /// mode is a compile error at `set_blend` rather than a silent fallthrough.
    #[test]
    fn blend_modes_are_distinct() {
        assert_ne!(Blend::Replace, Blend::Max);
    }
}
