//! `ShaderSurface` — a `gtk::GLArea` subclass that runs a **plugin-supplied**
//! fragment shader over a **plugin-supplied** data buffer, for the reconciler's
//! [`Node::Shader`](crate::widget_tree::Node::Shader) (#893).
//!
//! # How it differs from [`GlSurface`](crate::gl_surface)
//!
//! [`GlSurface`] draws a pipeline the *host* registered: several passes of
//! `&'static str` GLSL over a ping-pong accumulator, named by a
//! [`GlProgram`](crate::gl_surface::GlProgram) key. Everything about it is known
//! at compile time except the uniforms.
//!
//! This one is the opposite. There is exactly **one** pass, it writes straight
//! to the screen, it carries no accumulator and no auxiliary textures — and its
//! fragment source arrives at runtime, as a `String` off a socket. That is the
//! whole reason it is a second widget rather than a mode of the first: the
//! pipeline vocabulary next door is `Copy` and `'static` by design, and making
//! it own runtime strings would push an allocation and a lifetime into the arm
//! that draws the shell's own chips sixty times a second.
//!
//! # Compile once, upload per frame
//!
//! The issue's own framing, and the property everything here is arranged
//! around: **the source is compiled once and the program is kept; a frame is a
//! data upload.** The cache is keyed by a hash of the assembled source (header +
//! preamble + body) and holds the source itself alongside, so the hash is the
//! fast path and the string compare closes the collision — see
//! [`ProgramCache::ensure`]. It lives on the *instance* and dies with it: a
//! process-wide cache would have to outlive the GL context that linked its
//! programs, and program objects are not portable across share groups the way a
//! `&'static str` key is.
//!
//! A source that fails to compile is remembered as failed, by the same key, so
//! a broken shader costs **one** compile and one journal line for as long as the
//! plugin keeps sending it — not one per frame. A *changed* source recompiles,
//! synchronously, inside the render callback: there is no window in which the
//! widget is "compiling", so nothing has to hold a previous frame across one.
//!
//! # When it repaints
//!
//! On state change, and only on state change — see the
//! [`Node::Shader`](crate::widget_tree::Node::Shader) contract. There is no tick
//! callback here on purpose: a self-driving frame clock would run arbitrary
//! plugin-supplied GPU code at the compositor's rate for as long as the widget
//! is mapped, whether or not anything moved. `u_time` is sampled when a render
//! happens, so a plugin that wants motion pushes data on a timer and gets both.
//!
//! # Trust
//!
//! The source is untrusted input in the ordinary sense — it comes off the wire —
//! but **not** in the security sense, and #893 settled that explicitly: the
//! plugin socket is `0600` in a `0700` directory under `$XDG_RUNTIME_DIR`, so
//! anything that can send one of these already runs as the user. There is no
//! validator here, because there is none to have (naga cannot parse GLSL ES at
//! all). What this module does is *hygiene*: it refuses nothing and validates
//! nothing, and the host — `trollshell`'s `plugins::shader_map` — applies the
//! capability check and the two size caps before a node ever reaches here.
//! A shader that hangs the GPU takes the whole shell with it, which is the same
//! trust the plugin's own native code already has.
//!
//! # Sizing
//!
//! Measured exactly like [`PixelSurface`](crate::PixelSurface) and
//! [`GlSurface`]: the node's `width`/`height` are the natural size, the minimum
//! is `0` on both axes so CSS can scale it, the height is aspect-locked for the
//! width it is offered, and the draw is letterboxed into the largest
//! natural-aspect rect that fits — through the very same
//! [`fit_rect`](crate::gl_surface) the other two use, so a shader chip and a
//! raster chip of the same shape pad identically instead of one distorting.

use std::cell::{Cell, RefCell};
use std::hash::{Hash as _, Hasher as _};
use std::sync::Arc;
use std::time::Instant;

use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

use crate::gl_surface::{GLSL_HEADER, GlValue, abandon_gl, fit_rect};

/// The vertex stage: one attribute-less full-viewport triangle. A plugin never
/// supplies this — the wire contract is fragment-only.
const SHADER_VERT: &str = include_str!("shader_fullscreen.vert");

/// The interface declarations spliced between [`GLSL_HEADER`] and a plugin's
/// fragment body, and **the versioned half of the contract** documented on
/// [`Node::Shader`](crate::widget_tree::Node::Shader).
///
/// Declared here rather than left to the plugin for two reasons. A body that had
/// to spell out its own `uniform`s would drift from what the host actually sets,
/// silently, one name at a time; and `nix/lint-glsl.py` can splice *this exact
/// text* ahead of a tree-owned `.frag` and compile it, which is what makes the
/// bundled demo's shader a CI-checked artifact rather than a string nobody reads
/// until it is on glass.
///
/// A raw string so the GLSL reads as GLSL — and so the lint's regex has no
/// escapes to undo. Keep it that way.
///
/// Every name here is set on every draw, so a body may read any of them; a body
/// that *re-declares* one is a duplicate-declaration compile error, which is the
/// loud failure rather than a quiet override.
pub const SHADER_PREAMBLE: &str = r"
in vec2 v_uv;
out vec4 fragColor;

uniform float u_time;
uniform vec2 u_resolution;
uniform float u_scale;
uniform sampler2D u_data;
uniform vec2 u_data_size;

uniform vec4 u_bg;
uniform vec4 u_fg;
uniform vec4 u_accent;
uniform vec4 u_success;
uniform vec4 u_warning;
uniform vec4 u_error;
";

/// How the host reads a shader widget's data buffer into `u_data`.
///
/// The `hytte-ui` mirror of `hytte_plugin_proto::wire::ShaderData` — this crate
/// links no proto, so the host maps one onto the other in `wire_map`, which is
/// where every other wire ⇄ reconciler projection lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ShaderFormat {
    /// One byte per texel, read in `.r` as `0.0..=1.0`.
    R8,
    /// Four bytes per texel, `[R, G, B, A]`, straight alpha, each `0.0..=1.0`.
    Rgba8,
    /// One little-endian `f32` per texel, read in `.r` verbatim.
    R32f,
}

impl ShaderFormat {
    /// Bytes per texel in the CPU-side buffer.
    #[must_use]
    pub const fn bytes_per_texel(self) -> usize {
        match self {
            Self::R8 => 1,
            Self::Rgba8 | Self::R32f => 4,
        }
    }

    fn as_gl(self) -> hytte_gl::Format {
        match self {
            Self::R8 => hytte_gl::Format::R8,
            Self::Rgba8 => hytte_gl::Format::Rgba8,
            Self::R32f => hytte_gl::Format::R32f,
        }
    }
}

/// Everything one shader render needs: the source, the data, and the host's
/// theme bag.
///
/// Plain data with a derived `PartialEq`, because the reconciler dedups on it —
/// see [`ShaderSurface::set_state`]. The two big fields are `Arc`s so mapping
/// one frame onto a second monitor costs a refcount and settles on a pointer
/// compare.
#[derive(Clone, Debug, PartialEq)]
pub struct ShaderState {
    /// The plugin's fragment **body**, without `#version` and without the
    /// interface declarations — [`SHADER_PREAMBLE`] supplies those.
    pub fragment: Arc<str>,
    /// The data buffer, `data_size.0 * data_size.1 * format.bytes_per_texel()`
    /// bytes. The host validates that before it gets here; a buffer that is
    /// short anyway is zero-padded rather than read past.
    pub data: Arc<[u8]>,
    /// How to read [`data`](Self::data).
    pub format: ShaderFormat,
    /// The data grid, `(width, height)` in texels. Published as `u_data_size`.
    pub data_size: (u32, u32),
    /// The node's integer upscale hint, published as `u_scale`. Never `0` — the
    /// host maps the wire's `0`-means-`1` alias before it gets here.
    pub scale: u32,
    /// The theme colours, applied on every draw. `&'static str` names because
    /// they are the contract's fixed set (`u_bg`, `u_fg`, `u_accent`, the three
    /// status roles); a name the program does not declare resolves to location
    /// `-1`, which GL ignores.
    pub values: Vec<(&'static str, GlValue)>,
}

/// The cache key for one assembled program: a 64-bit hash of the header, the
/// preamble and the body, in that order.
///
/// The header and the preamble are hashed in — not just the body — because they
/// are *part of the source*: a build that changed the dialect or added a uniform
/// while a plugin kept sending the same body must recompile, and keying on the
/// body alone would silently keep the old program for the life of the widget.
fn source_key(fragment: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    GLSL_HEADER.hash(&mut hasher);
    SHADER_PREAMBLE.hash(&mut hasher);
    fragment.hash(&mut hasher);
    hasher.finish()
}

/// The compiled-program cache: **one** program, keyed by its source.
///
/// Generic over the program type so the reuse rule can be tested for real
/// against a counting builder rather than restated by a parallel probe — CI has
/// no GL, and "the same source does not recompile" is exactly the kind of claim
/// a parallel probe agrees with by construction.
#[derive(Debug)]
struct ProgramCache<P> {
    /// The linked program, its key, and the source it was linked from. The
    /// source is kept so the hash is a fast path rather than the whole answer.
    held: Option<(u64, Arc<str>, P)>,
    /// The key of a source that failed to build. A repeat of that exact source
    /// is refused without touching the driver, so a broken shader costs one
    /// compile and one journal line rather than one of each per frame.
    failed: Option<u64>,
}

impl<P> Default for ProgramCache<P> {
    fn default() -> Self {
        Self {
            held: None,
            failed: None,
        }
    }
}

impl<P> ProgramCache<P> {
    /// The program for `fragment`, building it with `build` if this is a source
    /// the cache has not linked.
    ///
    /// - `Ok(Some(program))` — reused, or freshly built.
    /// - `Ok(None)` — **this exact source already failed**; nothing to draw and
    ///   nothing to say (the caller said it the first time).
    /// - `Err(error)` — it failed now. The caller logs once and draws nothing;
    ///   the key is latched, so the next frame takes the `Ok(None)` arm.
    fn ensure<E>(
        &mut self,
        fragment: &Arc<str>,
        build: impl FnOnce(&str) -> Result<P, E>,
    ) -> Result<Option<&P>, E> {
        let key = source_key(fragment);
        // The reuse rule. `Arc::ptr_eq` first because the shell's per-instance
        // state cache hands the same allocation back on a re-map, then the hash,
        // then the bytes — cheapest test first, and the last one is what makes a
        // hash collision a slow path rather than a wrong picture.
        if self
            .held
            .as_ref()
            .is_some_and(|(held_key, source, _)| {
                *held_key == key && (Arc::ptr_eq(source, fragment) || **source == **fragment)
            })
        {
            return Ok(self.held.as_ref().map(|(_, _, program)| program));
        }
        if self.failed == Some(key) {
            return Ok(None);
        }
        // A new source: drop the old program *before* building, so a widget
        // whose plugin rewrites its shader does not hold two at once.
        self.held = None;
        match build(fragment) {
            Ok(program) => {
                self.failed = None;
                let held = self.held.insert((key, Arc::clone(fragment), program));
                Ok(Some(&held.2))
            }
            Err(error) => {
                self.failed = Some(key);
                Err(error)
            }
        }
    }
}

/// The first line of a driver info log, trimmed — what goes in the journal.
///
/// Info logs are multi-line and driver-specific; the first line carries the
/// error and the rest is usually the same message re-stated with a source
/// extract. One line keeps a broken shader a journal *line* rather than a
/// journal *page*, and the full log is one `RUST_LOG=debug` away.
fn first_line(log: &str) -> &str {
    log.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("(no driver diagnostic)")
}

mod imp {
    use super::{
        Arc, Cell, Instant, ProgramCache, RefCell, SHADER_PREAMBLE, SHADER_VERT, ShaderState,
        abandon_gl, first_line, fit_rect, gdk, glib,
    };
    use crate::gl_surface::{GLSL_HEADER, GlValue};
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use hytte_gl as hgl;

    /// The GL objects one surface owns, all created against its own context.
    struct Resources {
        programs: ProgramCache<hgl::Program>,
        /// The data texture, sized to the last uploaded grid (at least 1×1, so a
        /// shader never samples an unbound unit).
        data: hgl::Texture,
        /// The grid and format `data` was allocated for.
        data_shape: (u32, u32, super::ShaderFormat),
        /// Set when `data` holds exactly this allocation, so a re-render with
        /// the same `Arc` re-uploads nothing.
        data_source: Option<Arc<[u8]>>,
        vao: hgl::VertexArray,
    }

    #[derive(Default)]
    pub struct ShaderSurface {
        state: RefCell<Option<Arc<ShaderState>>>,
        /// Natural (logical) size in pixels, honored by `measure`.
        nat_width: Cell<i32>,
        nat_height: Cell<i32>,
        /// GL objects, built on the first render that has a context.
        resources: RefCell<Option<Resources>>,
        /// When this surface first drew, for `u_time`. Set on the first render
        /// rather than at construction: a widget built during a bar rebuild and
        /// mapped a second later would otherwise open mid-animation.
        origin: Cell<Option<Instant>>,
        /// One-shot latch for a compile failure, so a broken shader costs one
        /// journal line per surface and not one per frame. (`ProgramCache`
        /// already stops the *compile* repeating; this stops the *log*.)
        warned_compile: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ShaderSurface {
        const NAME: &'static str = "HytteShaderSurface";
        type Type = super::ShaderSurface;
        type ParentType = gtk::GLArea;
    }

    impl ObjectImpl for ShaderSurface {}

    impl WidgetImpl for ShaderSurface {
        /// Height-for-width, exactly like `PixelSurface` and `GlSurface`.
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
            (0, natural.max(0), -1, -1)
        }

        /// Realize through GTK, then find out whether it actually got a context
        /// — the same one place `GlSurface` can ask, for the same reason.
        fn realize(&self) {
            self.parent_realize();
            let obj = self.obj();
            if let Some(error) = obj.error() {
                abandon_gl(&error.to_string());
            } else if obj.context().is_none() {
                abandon_gl("GtkGLArea realized without a GdkGLContext");
            }
        }

        /// Drop every GL object **before** handing back to GTK, with the context
        /// explicitly made current first — the handles' `Drop` calls
        /// `glDelete*`, and GTK's own `unrealize` runs after this body.
        fn unrealize(&self) {
            let obj = self.obj();
            if obj.error().is_none() && obj.context().is_some() {
                obj.make_current();
            }
            self.resources.replace(None);
            self.origin.set(None);
            self.parent_unrealize();
        }
    }

    impl GLAreaImpl for ShaderSurface {
        fn render(&self, _context: &gdk::GLContext) -> glib::Propagation {
            self.draw();
            glib::Propagation::Stop
        }
    }

    impl ShaderSurface {
        /// Adopt new state, returning `(needs_render, needs_resize)`.
        ///
        /// The dedup is the reconciler's contract, identical to `GlSurface`'s:
        /// `to_ui_node` re-maps every node in a mailbox whenever anything in it
        /// moves, so an unchanged surface must cost nothing at all.
        pub(super) fn set_state(
            &self,
            width: u32,
            height: u32,
            state: &Arc<ShaderState>,
        ) -> (bool, bool) {
            let unchanged = self
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
            self.state.replace(Some(Arc::clone(state)));
            self.nat_width.set(w);
            self.nat_height.set(h);
            (true, resized)
        }

        /// The whole render: ensure the program, upload the data, draw one
        /// triangle into the letterboxed fit rect.
        fn draw(&self) {
            let state = self.state.borrow().clone();
            let Some(state) = state else { return };
            let Ok(gl) = hgl::Gl::current() else {
                abandon_gl("the GL entry points could not be resolved");
                return;
            };
            // GTK renders its own scene into this same context and nothing
            // promises what it leaves set — same discipline as `GlSurface`.
            hgl::reset_fixed_function_state(&gl);

            let mut held = self.resources.borrow_mut();
            if held.is_none() {
                match Resources::build(&gl) {
                    Ok(resources) => *held = Some(resources),
                    Err(error) => {
                        if !self.warned_compile.replace(true) {
                            tracing::warn!(
                                %error,
                                "a plugin shader surface could not allocate its GL objects; \
                                 it draws nothing (further occurrences are silenced)"
                            );
                        }
                        return;
                    }
                }
            }
            // Disjoint field borrows: `ensure` needs the cache mutably while the
            // program it returns is still live, and the draw below needs the
            // texture and the VAO at the same time. Destructuring is what makes
            // those three borrows provably separate.
            let Some(Resources {
                programs,
                data,
                data_shape,
                data_source,
                vao,
            }) = held.as_mut()
            else {
                return;
            };

            // Compile once, keep the program: the whole point of the widget.
            // A failure draws nothing and says so once — the host has already
            // put the node's id and classes on screen, so what is left is an
            // empty rect, which is the broken-widget placeholder's own look.
            let program = match programs.ensure(&state.fragment, |body| {
                hgl::Program::compile(
                    &gl,
                    GLSL_HEADER,
                    SHADER_VERT,
                    &format!("{SHADER_PREAMBLE}{body}"),
                )
            }) {
                Ok(Some(program)) => program,
                Ok(None) => return,
                Err(error) => {
                    let detail = match &error {
                        hgl::Error::Compile { log, .. } | hgl::Error::Link { log } => {
                            first_line(log).to_owned()
                        }
                        other => other.to_string(),
                    };
                    if !self.warned_compile.replace(true) {
                        tracing::warn!(
                            driver = %detail,
                            "a plugin's shader did not compile; the widget draws nothing \
                             (further occurrences on this surface are silenced — run with \
                             RUST_LOG=hytte_ui=debug for the full driver log)"
                        );
                    }
                    tracing::debug!(%error, "plugin shader compile failure, in full");
                    return;
                }
            };

            upload_data(&gl, data, data_shape, data_source, &state);

            let Some(viewport) = self.narrow_to_fit_rect(&gl) else {
                return;
            };
            program.bind(&gl);
            self.set_uniforms(&gl, program, &state, viewport);
            data.bind_unit(&gl, 0);
            program.set_int(&gl, "u_data", 0);
            // Straight alpha over whatever is behind the surface, which is what
            // the wire contract promises a plugin author.
            hgl::set_blend(&gl, hgl::Blend::Replace);
            vao.bind(&gl);
            hgl::draw_fullscreen(&gl);
        }

        /// Attach GTK's framebuffer, clear the whole allocation, then narrow the
        /// viewport to the letterboxed fit rect — returning that rect's size, or
        /// `None` when there is nothing to draw into.
        ///
        /// The clear covers the **whole** allocation before the narrowing, so
        /// the letterbox padding is transparent rather than whatever the last
        /// frame left there; GTK does not clear for us.
        fn narrow_to_fit_rect(&self, gl: &hgl::Gl) -> Option<(u32, u32)> {
            let obj = self.obj();
            // GTK renders into its own FBO so GSK can import the result, so
            // "the default framebuffer" is emphatically not 0. `attach_buffers`
            // is the documented way back to it.
            obj.attach_buffers();
            let scale = obj.scale_factor().max(1);
            let alloc_w = obj.width().saturating_mul(scale);
            let alloc_h = obj.height().saturating_mul(scale);
            hgl::viewport(
                gl,
                0,
                0,
                u32::try_from(alloc_w).unwrap_or(0),
                u32::try_from(alloc_h).unwrap_or(0),
            );
            hgl::set_blend(gl, hgl::Blend::Replace);
            hgl::clear(gl, [0.0, 0.0, 0.0, 0.0]);
            let (rect_x, rect_y, rect_w, rect_h) = fit_rect(
                alloc_w,
                alloc_h,
                u32::try_from(self.nat_width.get()).unwrap_or(0),
                u32::try_from(self.nat_height.get()).unwrap_or(0),
            );
            if rect_w == 0 || rect_h == 0 {
                return None;
            }
            hgl::viewport(gl, rect_x, rect_y, rect_w, rect_h);
            Some((rect_w, rect_h))
        }

        /// Publish the contract's uniforms: the four the surface owns, then the
        /// host's theme bag.
        ///
        /// A name the program does not declare resolves to location `-1`, which
        /// GL ignores — so a body that reads three of the thirteen costs nothing
        /// for the other ten.
        fn set_uniforms(
            &self,
            gl: &hgl::Gl,
            program: &hgl::Program,
            state: &ShaderState,
            viewport: (u32, u32),
        ) {
            let origin = self.origin.get().unwrap_or_else(|| {
                let now = Instant::now();
                self.origin.set(Some(now));
                now
            });
            // `as f32` on an `f64` of seconds: a wall-clock duration since this
            // surface's first frame, so small and positive for any session, and
            // `f32` is the type the uniform carries anyway.
            #[allow(clippy::cast_possible_truncation)]
            let seconds = origin.elapsed().as_secs_f64() as f32;
            program.set_float(gl, "u_time", seconds);
            program.set_vec2(gl, "u_resolution", [f32_of(viewport.0), f32_of(viewport.1)]);
            program.set_float(gl, "u_scale", f32_of(state.scale.max(1)));
            program.set_vec2(
                gl,
                "u_data_size",
                [f32_of(state.data_size.0), f32_of(state.data_size.1)],
            );
            for (name, value) in &state.values {
                match *value {
                    GlValue::Int(v) => program.set_int(gl, name, v),
                    GlValue::Float(v) => program.set_float(gl, name, v),
                    GlValue::Ivec2(v) => program.set_ivec2(gl, name, v),
                    GlValue::Vec4(v) => program.set_vec4(gl, name, v),
                }
            }
        }
    }

    /// A small unsigned count as an `f32`, for the `vec2`/`float` uniforms the
    /// contract publishes. Every value that reaches this is a pixel count or a
    /// texel count, both far inside `f32`'s exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    fn f32_of(value: u32) -> f32 {
        value as f32
    }

    impl Resources {
        fn build(gl: &hgl::Gl) -> Result<Self, hgl::Error> {
            Ok(Self {
                programs: ProgramCache::default(),
                data: hgl::Texture::new(gl, hgl::Format::R8, 1, 1)?,
                data_shape: (1, 1, super::ShaderFormat::R8),
                data_source: None,
                vao: hgl::VertexArray::new(gl),
            })
        }

    }

    /// Re-allocate the data texture if the grid or the format moved, then
    /// re-upload if this is not the allocation we already hold.
    ///
    /// A free function over the three fields it touches rather than a method on
    /// `Resources`, so the compiled program borrowed out of the same struct
    /// stays live across it — see the destructuring in `draw`.
    fn upload_data(
        gl: &hgl::Gl,
        data: &mut hgl::Texture,
        data_shape: &mut (u32, u32, super::ShaderFormat),
        data_source: &mut Option<Arc<[u8]>>,
        state: &ShaderState,
    ) {
        let (w, h) = (state.data_size.0.max(1), state.data_size.1.max(1));
        let shape = (w, h, state.format);
        if *data_shape != shape {
            let Ok(texture) = hgl::Texture::new(gl, state.format.as_gl(), w, h) else {
                // Keep the 1×1 fallback bound rather than leaving the unit
                // unbound: a shader samples something defined either way.
                *data_source = None;
                return;
            };
            *data = texture;
            *data_shape = shape;
            *data_source = None;
        }
        if data_source
            .as_ref()
            .is_some_and(|held| Arc::ptr_eq(held, &state.data))
        {
            return;
        }
        match state.format {
            super::ShaderFormat::R8 | super::ShaderFormat::Rgba8 => {
                data.upload_u8(gl, &state.data);
            }
            super::ShaderFormat::R32f => {
                // The wire carries little-endian bytes (stated in the contract,
                // so a big-endian plugin means the same thing); GL wants native
                // floats. One decode per upload, and only on an upload that
                // actually happens.
                let floats: Vec<f32> = state
                    .data
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
                data.upload_f32(gl, &floats);
            }
        }
        *data_source = Some(Arc::clone(&state.data));
    }
}

glib::wrapper! {
    /// A `gtk::GLArea` running a plugin-supplied fragment shader. See the
    /// [module docs](self).
    pub struct ShaderSurface(ObjectSubclass<imp::ShaderSurface>)
        @extends gtk::GLArea, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ShaderSurface {
    /// Build an empty surface (draws nothing until
    /// [`set_state`](Self::set_state)).
    ///
    /// `auto_render` is off — it renders when its state moves, not on every
    /// frame the compositor asks for — and the allowed API is pinned to
    /// **GLES**, so the dialect a plugin writes against is decided here rather
    /// than negotiated per driver. Depth and stencil are off: nothing a shader
    /// widget draws is three-dimensional.
    #[must_use]
    pub fn new() -> Self {
        let surface: Self = glib::Object::new();
        surface.set_auto_render(false);
        surface.set_has_depth_buffer(false);
        surface.set_has_stencil_buffer(false);
        surface.set_allowed_apis(gdk::GLAPI::GLES);
        surface
    }

    /// Point the surface at `state`, at a natural size of `width`×`height`
    /// logical pixels.
    ///
    /// A call carrying an equal state is a **no-op**: no render queued, no
    /// resize, nothing touched. The fast path is `Arc::ptr_eq`, which is what a
    /// node mapped onto a second monitor hits.
    pub fn set_state(&self, width: u32, height: u32, state: &Arc<ShaderState>) {
        let (render, resize) = self.imp().set_state(width, height, state);
        if resize {
            self.queue_resize();
        }
        if render {
            self.queue_render();
        }
    }

    /// Whether this surface's own context failed to be created.
    #[must_use]
    pub fn has_error(&self) -> bool {
        self.error().is_some()
    }
}

impl Default for ShaderSurface {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Arc, ProgramCache, SHADER_PREAMBLE, SHADER_VERT, ShaderFormat, first_line, source_key,
    };

    /// A counting builder, standing in for `hgl::Program::compile`. The cache is
    /// generic precisely so this drives **the shipped `ensure`**, not a
    /// re-statement of it — CI has no GL, and a parallel probe of a caching rule
    /// agrees with itself by construction.
    #[derive(Default)]
    struct Builder {
        builds: std::cell::Cell<u32>,
        fail: std::cell::Cell<bool>,
    }

    impl Builder {
        fn build(&self, _source: &str) -> Result<u32, &'static str> {
            self.builds.set(self.builds.get() + 1);
            if self.fail.get() {
                Err("ERROR: 0:3: 'fragColor' : undeclared identifier")
            } else {
                Ok(self.builds.get())
            }
        }
    }

    /// **The compile-once rule** (#893's headline claim): the same source is
    /// compiled once however many frames arrive, whether it comes back in the
    /// same allocation or an equal one.
    ///
    /// **Falsified** by deleting the `held` comparison in
    /// [`ProgramCache::ensure`] (or making it always `false`): the builder count
    /// rises with every frame and the first assertion goes red.
    #[test]
    fn the_same_source_is_compiled_once() {
        let builder = Builder::default();
        let mut cache: ProgramCache<u32> = ProgramCache::default();
        let source: Arc<str> = Arc::from("void main() { fragColor = u_fg; }");

        for _ in 0..8 {
            let program = cache
                .ensure(&source, |s| builder.build(s))
                .expect("compiles")
                .copied();
            assert_eq!(program, Some(1), "the same program, every frame");
        }
        assert_eq!(builder.builds.get(), 1, "one compile for eight frames");

        // A *different allocation* carrying the same text is still the same
        // source — which is the case a plugin that rebuilds its view every tick
        // actually produces.
        let equal: Arc<str> = Arc::from("void main() { fragColor = u_fg; }");
        assert!(!Arc::ptr_eq(&source, &equal), "distinct allocations");
        let program = cache
            .ensure(&equal, |s| builder.build(s))
            .expect("compiles")
            .copied();
        assert_eq!(program, Some(1));
        assert_eq!(builder.builds.get(), 1, "equal text does not recompile");
    }

    /// A changed source **does** recompile — the other half of the rule, and the
    /// half a cache that never invalidated would still pass the first test with.
    ///
    /// **Falsified** by keying the cache on nothing (returning the held program
    /// unconditionally): the second `assert_eq` sees the stale program.
    #[test]
    fn a_changed_source_recompiles() {
        let builder = Builder::default();
        let mut cache: ProgramCache<u32> = ProgramCache::default();

        let first: Arc<str> = Arc::from("void main() { fragColor = u_fg; }");
        let second: Arc<str> = Arc::from("void main() { fragColor = u_accent; }");

        assert_eq!(
            cache.ensure(&first, |s| builder.build(s)).unwrap().copied(),
            Some(1)
        );
        assert_eq!(
            cache.ensure(&second, |s| builder.build(s)).unwrap().copied(),
            Some(2),
            "a new source links a new program"
        );
        assert_eq!(builder.builds.get(), 2);
        // …and going back recompiles too: the cache holds one program, not a
        // map. Stated rather than left implicit, because a reader could
        // reasonably expect an LRU here and there isn't one.
        assert_eq!(
            cache.ensure(&first, |s| builder.build(s)).unwrap().copied(),
            Some(3),
        );
    }

    /// A source that failed to compile is **not** retried, so a broken shader
    /// costs one compile for as long as the plugin keeps sending it — and a
    /// plugin that then fixes it recovers on the next frame.
    ///
    /// **Falsified** by dropping the `failed` latch: the builder count rises
    /// with every frame of a broken shader.
    #[test]
    fn a_failed_source_is_not_retried_but_a_fixed_one_recovers() {
        let builder = Builder::default();
        builder.fail.set(true);
        let mut cache: ProgramCache<u32> = ProgramCache::default();
        let broken: Arc<str> = Arc::from("void main() { fragColour = u_fg; }");

        assert!(
            cache.ensure(&broken, |s| builder.build(s)).is_err(),
            "the first frame reports the failure",
        );
        for _ in 0..8 {
            assert_eq!(
                cache.ensure(&broken, |s| builder.build(s)),
                Ok(None),
                "afterwards it is a known-bad source, silently",
            );
        }
        assert_eq!(builder.builds.get(), 1, "one compile for nine frames");

        builder.fail.set(false);
        let fixed: Arc<str> = Arc::from("void main() { fragColor = u_fg; }");
        assert_eq!(
            cache.ensure(&fixed, |s| builder.build(s)).unwrap().copied(),
            Some(2),
            "a corrected source compiles",
        );
    }

    /// The cache key covers the **assembled** source, not just the plugin's
    /// body: a build that changes the header or the preamble must invalidate
    /// every program, and a key over the body alone silently would not.
    ///
    /// It cannot assert "the header changed ⇒ the key changed" directly (the
    /// header is a `const`), so it asserts the property that makes that true:
    /// the key is a function of all three, and two bodies that differ get
    /// different keys.
    ///
    /// **Falsified** by hashing only `fragment` in [`source_key`]: the first
    /// assertion still passes, so the second — which pins that the header text
    /// is *in* the hashed material — is the one that goes red.
    #[test]
    fn the_cache_key_covers_the_assembled_source() {
        assert_ne!(source_key("a"), source_key("b"), "bodies key apart");
        assert_eq!(source_key("a"), source_key("a"), "and are deterministic");

        // The same material, hashed the same way, by hand: if `source_key`
        // stopped folding the header and the preamble in, this diverges.
        let by_hand = {
            use std::hash::{Hash as _, Hasher as _};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            crate::gl_surface::GLSL_HEADER.hash(&mut hasher);
            SHADER_PREAMBLE.hash(&mut hasher);
            "a".hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(source_key("a"), by_hand, "header + preamble + body");
    }

    /// The preamble is the versioned contract, so the names it declares are
    /// asserted rather than left to a doc comment three files away: the host
    /// sets every one of them on every draw, and a plugin's body reads them.
    ///
    /// **Falsified** by deleting any uniform line from [`SHADER_PREAMBLE`].
    #[test]
    fn the_preamble_declares_every_contract_name() {
        for name in [
            "in vec2 v_uv;",
            "out vec4 fragColor;",
            "uniform float u_time;",
            "uniform vec2 u_resolution;",
            "uniform float u_scale;",
            "uniform sampler2D u_data;",
            "uniform vec2 u_data_size;",
            "uniform vec4 u_bg;",
            "uniform vec4 u_fg;",
            "uniform vec4 u_accent;",
            "uniform vec4 u_success;",
            "uniform vec4 u_warning;",
            "uniform vec4 u_error;",
        ] {
            assert!(
                SHADER_PREAMBLE.contains(name),
                "the preamble must declare `{name}`",
            );
        }
        // No `#version` here — the header carries it, and two would not compile.
        assert!(!SHADER_PREAMBLE.contains("#version"));
        assert!(
            !SHADER_VERT.contains("#version"),
            "the vertex stage takes the same header",
        );
    }

    /// The texel widths the host sizes a buffer with, and the mapping onto
    /// `hytte-gl`'s formats. Wrong here and a data upload reads the wrong
    /// number of bytes.
    #[test]
    fn formats_carry_their_texel_width() {
        assert_eq!(ShaderFormat::R8.bytes_per_texel(), 1);
        assert_eq!(ShaderFormat::Rgba8.bytes_per_texel(), 4);
        assert_eq!(ShaderFormat::R32f.bytes_per_texel(), 4);
        assert_eq!(ShaderFormat::R8.as_gl(), hytte_gl::Format::R8);
        assert_eq!(ShaderFormat::Rgba8.as_gl(), hytte_gl::Format::Rgba8);
        assert_eq!(ShaderFormat::R32f.as_gl(), hytte_gl::Format::R32f);
    }

    /// The journal line takes the driver's first *non-empty* line, and says
    /// something rather than nothing for a driver that returns an empty log.
    #[test]
    fn the_driver_diagnostic_is_one_line() {
        assert_eq!(
            first_line("ERROR: 0:3: 'x' : undeclared\nERROR: 1 compilation error.\n"),
            "ERROR: 0:3: 'x' : undeclared",
        );
        assert_eq!(first_line("\n\n  padded  \nsecond\n"), "padded");
        assert_eq!(first_line(""), "(no driver diagnostic)");
        assert_eq!(first_line("   \n "), "(no driver diagnostic)");
    }
}
