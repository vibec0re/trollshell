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
//!    `GL_INVALID_VALUE` at best and a driver crash at worst.
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

    /// Whether GL reported an error since the last check, draining the queue.
    ///
    /// A debugging seam, not a control-flow one: the render path is written so
    /// that a GL error cannot change what is drawn, and polling `glGetError`
    /// per draw is itself a synchronisation point on some drivers. The parity
    /// harness calls it; the shell does not.
    #[must_use]
    pub fn take_error(&self) -> Option<u32> {
        // SAFETY: a context is current (the `&self` token) and `glGetError`
        // takes no arguments and cannot fail.
        let code = unsafe { gl::GetError() };
        (code != gl::NO_ERROR).then_some(code)
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
        let location =
            unsafe { gl::GetUniformLocation(self.id, terminated.as_ptr().cast::<i8>()) };
        self.locations.borrow_mut().insert(name.to_owned(), location);
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
fn read_info_log(
    length: GLint,
    fill: impl FnOnce(*mut i8, GLsizei, *mut GLsizei),
) -> String {
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
    /// Single-channel 32-bit float (`GL_R32F`) — the 1-D data texture. Never
    /// filtered (`texelFetch` only), so it needs no `OES_texture_float_linear`.
    R32f,
}

impl Format {
    /// `(sized internal format, transfer format, transfer type)`.
    fn as_gl(self) -> (GLenum, GLenum, GLenum) {
        match self {
            Self::R8 => (gl::R8, gl::RED, gl::UNSIGNED_BYTE),
            Self::R32f => (gl::R32F, gl::RED, gl::FLOAT),
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

impl Texture {
    /// Allocate a `width`×`height` texture with immutable storage.
    ///
    /// Both extents must be positive and fit `GLsizei`; anything else is
    /// [`Error::Extent`] rather than a truncating cast.
    pub fn new(_gl: &Gl, format: Format, width: u32, height: u32) -> Result<Self, Error> {
        let extent = |v: u32| (v > 0).then(|| GLsizei::try_from(v).ok()).flatten();
        let (Some(w), Some(h)) = (extent(width), extent(height)) else {
            return Err(Error::Extent {
                size: (width, height),
            });
        };
        let (internal, _, _) = format.as_gl();
        let mut id: GLuint = 0;
        // SAFETY: a context is current. `GenTextures` writes one id through a
        // pointer to a live local; the rest operate on that id while it is
        // bound, with extents already validated as positive `GLsizei`s.
        unsafe {
            gl::GenTextures(1, &raw mut id);
            gl::BindTexture(gl::TEXTURE_2D, id);
            gl::TexStorage2D(gl::TEXTURE_2D, 1, internal, w, h);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::NEAREST.cast_signed());
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::NEAREST.cast_signed());
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
        debug_assert_eq!(self.format, Format::R32f, "upload_f32 wants an R32F texture");
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

/// Bind framebuffer `id` — in practice the one `GtkGLArea` was rendering into,
/// captured with [`current_draw_framebuffer`] before the offscreen passes.
///
/// **GTK does not render into framebuffer 0.** A `GtkGLArea` renders into its
/// own FBO so GSK can import the result as a texture, so restoring "the default
/// framebuffer" by binding `0` paints into nothing at all. That is the single
/// most expensive mistake available on this path, which is why the capture and
/// the restore are both named here rather than left to the caller's memory.
pub fn bind_framebuffer(_gl: &Gl, id: u32) {
    // SAFETY: a context is current. `id` came from `GL_FRAMEBUFFER_BINDING` in
    // this same context, so it names a live FBO (or 0, which is always legal).
    unsafe { gl::BindFramebuffer(gl::FRAMEBUFFER, id) };
}

/// The framebuffer currently bound for drawing — see [`bind_framebuffer`].
#[must_use]
pub fn current_draw_framebuffer(_gl: &Gl) -> u32 {
    let mut id: GLint = 0;
    // SAFETY: a context is current; `FRAMEBUFFER_BINDING` writes exactly one
    // `GLint` through a pointer to a live local.
    unsafe { gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &raw mut id) };
    u32::try_from(id).unwrap_or(0)
}

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

/// Set the viewport, refusing an extent that does not fit `GLsizei`.
pub fn viewport(_gl: &Gl, x: i32, y: i32, width: u32, height: u32) {
    let w = GLsizei::try_from(width).unwrap_or(0);
    let h = GLsizei::try_from(height).unwrap_or(0);
    // SAFETY: a context is current; a zero extent is legal (it draws nothing),
    // which is the fallback a non-representable extent takes.
    unsafe { gl::Viewport(x, y, w, h) };
}

/// Clear the bound framebuffer's colour to `rgba`.
pub fn clear(_gl: &Gl, rgba: [f32; 4]) {
    // SAFETY: a context is current; both calls take only scalars.
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
    use super::{Blend, Error, Format, Stage};

    /// The two formats keep the enum triples the shaders are written against —
    /// a normalized `R8` (so `GL_MAX` blending is available at all) and an
    /// unfiltered `R32F` data strip. Pure table check; no context needed.
    #[test]
    fn formats_map_to_the_enums_the_shaders_assume() {
        assert_eq!(Format::R8.as_gl(), (gl::R8, gl::RED, gl::UNSIGNED_BYTE));
        assert_eq!(Format::R32f.as_gl(), (gl::R32F, gl::RED, gl::FLOAT));
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
