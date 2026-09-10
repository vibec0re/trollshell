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
//! Both of those bounds are per **source**, and the journal one remembers the
//! last [`WARNED_SOURCES`] of them (a one-entry latch let two broken sources
//! alternating write a line every frame — #968's second review). The case
//! neither bounds is a plugin that *generates* its body wrongly, producing a
//! fresh key every frame: that writes a line and pays a compile per frame, by
//! construction, and no cache keyed on the source can help it. Stated rather
//! than left as "one line for as long as it keeps sending it", which stopped
//! being the whole truth when the latch grew a bound.
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
//! Two things about that clock, both consequences rather than choices, and both
//! stated because a shader author can see them: it **wraps hourly**
//! ([`TIME_WRAP_SECS`] — an `f32` of unwrapped seconds loses a quarter-second of
//! resolution after a month of uptime), and it **restarts on unmap/remap**,
//! because `origin` is dropped with the GL objects in `unrealize` and a remap is
//! genuinely a new first frame.
//!
//! # Colour: premultiplied, because GTK's is
//!
//! The single pass draws under `Blend::Replace`, which is
//! `glDisable(GL_BLEND)` — the shader's `fragColor` lands verbatim in GTK's own
//! framebuffer, and GSK imports that texture as `GDK_MEMORY_DEFAULT`, i.e.
//! **premultiplied**. So the contract asks a plugin for premultiplied output
//! (`vec4(rgb * a, a)`), and that is a fact about the pipeline rather than a
//! preference: the shell never sees the colour to convert it, and adding a
//! second pass to premultiply would cost a full-surface texture round trip to
//! undo something the author can do in one multiply.
//!
//! Unverified here, and stated as such: this sandbox has no GL, so the claim is
//! read off GDK's memory format rather than off a screenshot.
//! `docs/live-verify.md` carries the check that settles it.
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
use hytte_gl as hgl;

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

/// The cache key for one refused data-texture allocation: a 64-bit hash of
/// the grid's `(width, height, format)` (#1023 item 1).
///
/// Sibling of [`source_key`], for the same reason: `warned_data` moved from
/// a bare `Cell<bool>` to a [`WarnLatch`] keyed by this, so a **second,
/// differently shaped** refused grid earns its own journal line instead of
/// being silenced by the first — exactly the bug `warned_compile`'s own doc
/// already names, sixteen lines above where `warned_data` is declared
/// (#1020 review, MEDIUM 1).
fn data_key(width: u32, height: u32, format: ShaderFormat) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut hasher);
    height.hash(&mut hasher);
    format.hash(&mut hasher);
    hasher.finish()
}

/// The line a driver-refused data upload writes to the journal (#1023 item
/// 4).
///
/// Says the surface **keeps whatever it last successfully drew**, not that
/// it "draws nothing" (#1020's own shipped wording, and #1020's review LOW
/// 3): the early return below happens *before* `narrow_to_fit_rect`, the
/// only thing that clears — GTK does not clear for us — so after one good
/// frame a refused upload leaves that frame's picture on screen, frozen,
/// rather than an empty rect.
///
/// This picks *reword* over *clear before returning*, the two remedies the
/// review named, for two reasons. First, the previous frame's pixels are not
/// wrong — only the *update* failed — so clearing would trade a stale-but-
/// correct picture for a flash of nothing on every refused frame, worse for
/// a plugin that sends one bad grid in twenty than for one that sends
/// nothing but bad grids. Second, it is the arm-for-arm consistent choice:
/// the compile-failure and `Ok(None)` arms above this one in `draw` already
/// keep the last frame (true before this PR too, just undocumented), and
/// clearing only the data-upload arm would make one of three sibling
/// failure arms behave differently from the other two for no reason a
/// plugin author could discover from the code.
const DATA_UPLOAD_REFUSED: &str = "a plugin shader's data texture could not be allocated; this \
    frame is skipped and the surface keeps whatever it last successfully drew (nothing, before \
    the first successful frame) until the plugin sends a grid this driver will take (further \
    occurrences of this exact shape are silenced)";

/// The line a failed GL-object allocation writes to the journal.
///
/// Same reasoning as [`DATA_UPLOAD_REFUSED`] (PR #1031 review L2): this arm
/// also returns before `narrow_to_fit_rect`, so it must not claim the widget
/// "draws nothing" either — #1020 shipped it that way, next to the very
/// message this fixes.
const RESOURCES_ALLOCATION_REFUSED: &str = "a plugin shader surface could not allocate its GL \
    objects; the surface keeps whatever it last successfully drew (nothing, before the first \
    successful frame) (further occurrences are silenced)";

/// The line a failed compile writes to the journal.
///
/// Same reasoning as [`DATA_UPLOAD_REFUSED`] (PR #1031 review L2).
const COMPILE_FAILURE_REFUSED: &str = "a plugin's shader did not compile; the surface keeps \
    whatever it last successfully drew (nothing, before the first successful frame) — further \
    frames carrying this same source are silenced; run with RUST_LOG=hytte_ui=debug for the \
    full driver log";

/// The key `draw`'s data-upload arm claims `warned_data` with, over the
/// state the call site actually reads.
///
/// Hoisted out of the call site (PR #1031 review M2) so the composition
/// "`draw` feeds `ShaderState` into `data_key` with the right clamp, not a
/// constant" is itself covered by a test — [`data_key`]'s own
/// three-field test and [`WarnLatch::claim`]'s own keying test each cover
/// half of this in isolation, and neither pins that `draw` calls either of
/// them correctly.
fn data_upload_key(state: &ShaderState) -> u64 {
    data_key(
        state.data_size.0.max(1),
        state.data_size.1.max(1),
        state.format,
    )
}

/// Claim `latch` for `state`'s shape (via [`data_upload_key`]) and log
/// [`DATA_UPLOAD_REFUSED`] once — the whole reporting half of a refused data
/// upload, in one function a test can drive with no GL context (PR #1031
/// review M2).
///
/// **Its only production caller is `imp::upload_data`'s own refusal arm, and
/// it neither takes a `Result` nor returns a "skip" flag any more** (PR
/// #1031 review H1). Both were degrees of freedom at `draw`'s call site, and
/// the second-pass review measured what they cost: re-swallowing the error
/// there — #1020's shipped silence, restored in one line — left `cargo
/// test`, the `system-tests` bucket and `clippy -D warnings` all green,
/// because the only test that observes that line needs a live driver and so
/// runs nowhere. The seam was removed rather than tested: `upload_data`
/// takes the latch, reports for itself, and hands back the texture to sample
/// — `None` when there is none — which turns round 2's L2 (#968 review L7)
/// from a `return` a caller can drop into a `let … else` whose `else` arm
/// must diverge, so that exact one-line mistake no longer compiles.
///
/// **That is a property of this spelling, not of the type** (PR #1031
/// third-pass review LOW 3): `if upload_data(…).is_none() { }` still
/// compiles, still passes every argument including the widget's own latch,
/// and reinstates L7 anyway — the frame draws with `u_data_size` describing
/// a grid that was never bound, forever, because the shape is never
/// advanced so the refusal repeats every frame. The `let … else` shape at
/// the call site is the contract; the `Option` return only makes that shape
/// possible, and does not by itself enforce it.
///
/// `hgl::Error` is plain data a test can construct by hand; only *producing*
/// a genuine refusal — `hgl::Texture::new` really turning a grid down — needs
/// real GL.
fn warn_on_data_upload_failure(
    latch: &RefCell<WarnLatch>,
    state: &ShaderState,
    error: &hgl::Error,
) {
    if latch.borrow_mut().claim(data_upload_key(state)) {
        tracing::warn!(
            %error,
            data_width = state.data_size.0,
            data_height = state.data_size.1,
            "{}",
            DATA_UPLOAD_REFUSED
        );
    }
}

/// The whole refusal arm of `upload_data`'s reallocation: latch-and-log via
/// [`warn_on_data_upload_failure`], plus the *state* half neither that
/// function nor its own test touches — pulled out so the retry contract
/// #977 exists for is observable with no GL context (PR #1031 third-pass
/// review LOW 2).
///
/// `data_shape` is threaded through and never written: that omission **is**
/// the contract this pins. Advancing it here is the pre-#977 bug, restored
/// verbatim — `upload_data`'s own comment at its call site explains why: a
/// `Texture::new` that now fails honestly must leave the shape where it
/// was, so the next frame retries the same allocation instead of sampling a
/// texture with no storage behind it.
fn refuse_data_strip(
    _data_shape: &mut (u32, u32, ShaderFormat),
    data_source: &mut Option<Arc<[u8]>>,
    warned: &RefCell<WarnLatch>,
    state: &ShaderState,
    error: &hgl::Error,
) {
    *data_source = None;
    warn_on_data_upload_failure(warned, state, error);
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
    /// The key **and source** of a source that failed to build. A repeat of that
    /// exact source is refused without touching the driver, so a broken shader
    /// costs one compile rather than one per frame.
    ///
    /// The source rides along for the same reason it does in `held`: a hash is a
    /// fast path, not the whole answer. Keyed on the hash alone, a *different*
    /// source that happened to collide with a latched-failed key would be
    /// refused without ever reaching the driver and without a journal line — a
    /// silently blank widget for a shader that compiles fine. 64-bit `SipHash`
    /// over two sources in one widget's lifetime makes that astronomically
    /// unlikely, which is why it is a one-word guard rather than a redesign, but
    /// "unlikely" is not what the doc above claims.
    failed: Option<(u64, Arc<str>)>,
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
    /// - `Err(Failure { error, key })` — it failed now. The caller logs it
    ///   against `key` and draws nothing; the source is latched, so the next
    ///   frame carrying it takes the `Ok(None)` arm.
    ///
    /// The `key` in the failure is what makes the caller's journal latch
    /// **per source** rather than per surface: a plugin that ships a broken
    /// shader, fixes it, then breaks it differently gets a line for each break.
    fn ensure<E>(
        &mut self,
        fragment: &Arc<str>,
        build: impl FnOnce(&str) -> Result<P, E>,
    ) -> Result<Option<&P>, Failure<E>> {
        let key = source_key(fragment);
        let same = |held_key: &u64, source: &Arc<str>| {
            // `Arc::ptr_eq` first because the shell's per-node state cache hands
            // the same allocation back on a re-map, then the hash, then the
            // bytes — cheapest test first, and the last one is what makes a hash
            // collision a slow path rather than a wrong picture.
            *held_key == key && (Arc::ptr_eq(source, fragment) || **source == **fragment)
        };
        if self
            .held
            .as_ref()
            .is_some_and(|(held_key, source, _)| same(held_key, source))
        {
            return Ok(self.held.as_ref().map(|(_, _, program)| program));
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|(failed_key, source)| same(failed_key, source))
        {
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
                self.failed = Some((key, Arc::clone(fragment)));
                Err(Failure { error, key })
            }
        }
    }
}

/// A build failure, carrying the source key it failed under so the caller can
/// latch its journal line **per source**.
#[derive(Debug, PartialEq, Eq)]
struct Failure<E> {
    /// What the builder said.
    error: E,
    /// [`source_key`] of the source that failed.
    key: u64,
}

/// How many distinct failed sources a [`WarnLatch`] remembers.
///
/// Small and fixed. The latch is a *bound on the log*, and a one-entry MRU is
/// not one: two broken sources alternating each evict the other, so every frame
/// writes a line (#968 second review, M2 residual — measured at 10 lines for 10
/// alternating frames). Eight covers alternation and any realistic hand-written
/// set of broken shaders, in 64 bytes.
///
/// It is deliberately **not** unbounded. A plugin that *generates* its body
/// wrongly — a counter in a comment is enough — produces a fresh key every
/// frame, and remembering them all would be a slow leak keyed by the plugin's
/// own bug. Such a plugin writes a line per frame either way, because it also
/// forces a recompile per frame by construction; the FIFO keeps the memory
/// bounded and the diagnostic honest about what it is seeing.
const WARNED_SOURCES: usize = 8;

/// A journal latch over the last [`WARNED_SOURCES`] failed source keys.
///
/// A bare `bool` was the bug (#968 review M2): set on the first compile failure
/// and never cleared, it silenced the *second* distinct broken shader entirely —
/// the driver was asked, the widget went empty, and nothing was ever logged
/// again. Since `ProgramCache` retries a **changed** source by design, that
/// sequence (break, fix, break differently) is inside the contract rather than
/// exotic, and the diagnostic it swallows is the one the whole
/// "broken shader → placeholder + one warning" story rests on.
///
/// A one-entry MRU was the *second* bug, in the other direction: it bounded the
/// log only while at most one broken source was in play. A bounded FIFO closes
/// both — one line per distinct broken source, with alternation staying quiet.
#[derive(Debug, Default)]
struct WarnLatch {
    /// The keys already reported, oldest first. At most [`WARNED_SOURCES`].
    said: std::collections::VecDeque<u64>,
}

impl WarnLatch {
    /// Whether to write a line for `key`: `true` the first time this key is
    /// seen, `false` for every repeat of a key still remembered.
    ///
    /// A key evicted by [`WARNED_SOURCES`] can be reported a second time. That
    /// is the cost of the bound, and it is the right way round: over-reporting a
    /// shader that has been broken, replaced eight times over and broken again
    /// the same way is a journal line; under-reporting is a blank widget nobody
    /// is told about.
    fn claim(&mut self, key: u64) -> bool {
        if self.said.contains(&key) {
            return false;
        }
        if self.said.len() == WARNED_SOURCES {
            self.said.pop_front();
        }
        self.said.push_back(key);
        true
    }
}

/// The first line of a driver info log, trimmed — what goes in the journal.
///
/// Info logs are multi-line and driver-specific; the first line carries the
/// error and the rest is usually the same message re-stated with a source
/// extract. One line keeps a broken shader a journal *line* rather than a
/// journal *page*, and the full log is one `RUST_LOG=debug` away.
fn first_line(log: &str) -> &str {
    log.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(no driver diagnostic)")
}

mod imp {
    use super::{
        Arc, COMPILE_FAILURE_REFUSED, Cell, Failure, Instant, ProgramCache,
        RESOURCES_ALLOCATION_REFUSED, RefCell, SHADER_PREAMBLE, SHADER_VERT, ShaderState,
        WarnLatch, abandon_gl, first_line, fit_rect, gdk, glib, refuse_data_strip, source_key,
        would_upload, wrapped_seconds,
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
        /// Journal latch for a compile failure, **keyed by source** so a broken
        /// shader costs one line per distinct broken source rather than one per
        /// frame — and so a *second*, different broken source is not silently
        /// swallowed. (`ProgramCache` stops the *compile* repeating; this stops
        /// the *log*.) See [`WarnLatch`].
        warned_compile: RefCell<WarnLatch>,
        /// One-shot latch for "the GL objects could not be allocated at all",
        /// which is not keyed by anything a plugin controls — its own bool, so
        /// it cannot mask a compile failure or be masked by one.
        warned_resources: Cell<bool>,
        /// Journal latch for "the **data texture** could not be allocated"
        /// (#977) — a driver refusing the grid the plugin asked for. **Keyed
        /// by `(width, height, format)`** via [`data_key`] (#1023 item 1),
        /// the same shape as [`warned_compile`] sixteen lines above — not a
        /// bare `Cell<bool>` for the same reason: a plugin that sends a
        /// driver-refused grid, then a good one, then a **different**
        /// driver-refused grid must get a line for the second refusal too,
        /// not permanent silence after the first (#1020 review, MEDIUM 1 —
        /// this field shipped as a bare bool and was exactly that bug).
        ///
        /// A separate latch from `warned_compile`, not merged into it: it is
        /// a different failure with a different fix (reshape the buffer) from
        /// both the build failure above and the compile failure below, and a
        /// shared latch would let whichever fired first swallow the others.
        /// Before #977, a refused allocation returned `false` from
        /// `upload_data` and the frame was skipped in silence — every frame,
        /// for the life of the surface, with a black rect on screen and not one
        /// journal line anywhere in the process.
        warned_data: RefCell<WarnLatch>,
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
                        if !self.warned_resources.replace(true) {
                            tracing::warn!(%error, "{}", RESOURCES_ALLOCATION_REFUSED);
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
            // A failure keeps whatever this surface last successfully drew
            // (nothing, before the first successful frame) and says so once
            // — on that first frame the host has already put the node's id
            // and classes on screen, so what is left looks like the
            // broken-widget placeholder even though this is a different
            // mechanism from it.
            let program = match programs.ensure(&state.fragment, |body| {
                // One `debug!` per **actual compile**, carrying the source key
                // (#968 review L4): `docs/live-verify.md`'s "the same source
                // does not recompile" check asks an operator to watch
                // `RUST_LOG=hytte_ui=debug` for repeated compile activity, and
                // before this there was nothing to see — only failures logged,
                // so the check observed an absence that was unconditional.
                // Now a silent log *is* the evidence, and a source that really
                // did change prints exactly one line with a new hash.
                tracing::debug!(
                    source_key = source_key(body),
                    bytes = body.len(),
                    "compiling a plugin shader"
                );
                hgl::Program::compile(
                    &gl,
                    GLSL_HEADER,
                    SHADER_VERT,
                    &format!("{SHADER_PREAMBLE}{body}"),
                )
            }) {
                Ok(Some(program)) => program,
                Ok(None) => return,
                Err(Failure { error, key }) => {
                    let detail = match &error {
                        hgl::Error::Compile { log, .. } | hgl::Error::Link { log } => {
                            first_line(log).to_owned()
                        }
                        other => other.to_string(),
                    };
                    // Latched by **source**, not by surface: a plugin that
                    // breaks, fixes, then breaks differently gets a line for
                    // each break rather than one for the first and silence
                    // thereafter (#968 review M2).
                    if self.warned_compile.borrow_mut().claim(key) {
                        tracing::warn!(
                            driver = %detail,
                            source_key = key,
                            "{}",
                            COMPILE_FAILURE_REFUSED
                        );
                    }
                    tracing::debug!(%error, source_key = key, "plugin shader compile failure, in full");
                    return;
                }
            };

            // A failed reallocation leaves the *previous* texture bound, whose
            // grid is no longer the one `state.data_size` describes — so the
            // frame is skipped rather than drawn with `u_data_size` lying about
            // what `u_data` holds (#968 review L7). The next frame retries.
            //
            // …and it says so once (#977). A grid the driver will not allocate
            // used to be indistinguishable, from outside the process, from a
            // shader that draws black: no error, no line, every frame, for the
            // life of the surface.
            //
            // The latch goes *in* and the texture to sample comes back, so
            // this arm has no `Result` to swallow and no `return` to forget:
            // on a refusal there is nothing to bind and the `else` has to
            // diverge (PR #1031 review H1/L2).
            let Some(data) = upload_data(
                &gl,
                data,
                data_shape,
                data_source,
                &state,
                &self.warned_data,
            ) else {
                return;
            };

            let Some(viewport) = self.narrow_to_fit_rect(&gl) else {
                return;
            };
            program.bind(&gl);
            self.set_uniforms(&gl, program, &state, viewport);
            data.bind_unit(&gl, 0);
            program.set_int(&gl, "u_data", 0);
            // `Replace` is `glDisable(GL_BLEND)`: the shader's `fragColor` is
            // written verbatim into GTK's framebuffer, which GSK imports as
            // **premultiplied**. That is why the contract asks a plugin for
            // premultiplied output — see the `Node::Shader` docs (#968 review
            // M4). The blend mode is not the place to fix it: the shell never
            // sees the colour, the shader writes it.
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
            program.set_float(gl, "u_time", wrapped_seconds(origin.elapsed()));
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
    ///
    /// Returns **the texture to sample**, or `None` if the driver refused the
    /// (re)allocation and this frame must be skipped — in which case the
    /// refusal has already been latched and logged, once per shape, through
    /// [`super::refuse_data_strip`] (#977, #1023 item 1).
    ///
    /// **Handing the texture back rather than an `Ok`/`Err` is the point**
    /// (PR #1031 review H1/L2): the caller cannot draw without the return
    /// value, so it cannot forget the skip, and taking `warned` as a
    /// parameter leaves it no error to swallow. Both of those were one-line
    /// mutations at `draw`'s call site that the entire gate stack passed,
    /// because that line needs a live driver to reach.
    fn upload_data<'t>(
        gl: &hgl::Gl,
        data: &'t mut hgl::Texture,
        data_shape: &mut (u32, u32, super::ShaderFormat),
        data_source: &mut Option<Arc<[u8]>>,
        state: &ShaderState,
        warned: &RefCell<WarnLatch>,
    ) -> Option<&'t hgl::Texture> {
        let (w, h) = (state.data_size.0.max(1), state.data_size.1.max(1));
        let shape = (w, h, state.format);
        if *data_shape != shape {
            let texture = match hgl::Texture::new(gl, state.format.as_gl(), w, h) {
                Ok(texture) => texture,
                Err(error) => {
                    // **The frame is skipped, not drawn.** The old texture is
                    // still bound and still holds the *old* grid, while
                    // `set_uniforms` would publish `u_data_size` from the new
                    // `state` — one frame sampled against a size that does not
                    // describe what is bound (#968 review L7). Nothing is left
                    // inconsistent: the shape is not advanced, so the next
                    // frame retries the allocation.
                    //
                    // Before #977 this arm was unreachable for the case that
                    // matters: `Texture::new` never asked the driver, so a
                    // `glTexStorage2D` that failed with `GL_INVALID_VALUE`
                    // still returned `Ok` and the shape *was* advanced, onto a
                    // texture with no storage. Now it fails honestly, the
                    // retry is real, and the caller writes one line.
                    //
                    // **Still uncovered on the GL side, stated rather than
                    // implied.** Reaching it needs a live context that refuses
                    // an allocation, and CI has no GL at all — the same honest
                    // gap #954 recorded for `fresh_last_drawn`'s call site.
                    // What *is* covered is the extent decision itself
                    // (`hytte-gl`'s `checked_extent` tests), the host-side
                    // refusal that stops most such grids ever arriving
                    // (`shader_map`'s per-axis cap), and this function's dedup
                    // (`would_upload`'s own test). What *is* structural since
                    // PR #1031 review H1 is that the caller cannot ignore
                    // this: it gets `None`, not an `Err` it may drop, and it
                    // needs the texture this returns in order to draw at all.
                    refuse_data_strip(data_shape, data_source, warned, state, &error);
                    return None;
                }
            };
            *data = texture;
            *data_shape = shape;
            *data_source = None;
        }
        // The re-upload dedup, and the reason `shader_map` caches its
        // `Arc<ShaderState>` per node (#968 review M1): with a fresh `Arc` every
        // mapping pass this could never fire, and the whole data texture went to
        // the GPU on every render even when the bytes had not moved.
        if !would_upload(data_source.as_ref(), &state.data) {
            return Some(data);
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
        Some(data)
    }
}

/// Whether the data texture must be re-uploaded for `incoming`.
///
/// **Public as a test seam**, and only for that: `trollshell`'s `shader_map`
/// drives this exact function over real `map_shader` output to count uploads
/// hermetically, which is the property #968 review M1 is about and which no
/// probe written beside it could establish (a parallel copy of a caching rule
/// agrees with itself by construction). Nothing in a shell calls it.
///
/// Identity, not equality: the reconciler's contract is that a node mapped onto
/// a second monitor, or re-mapped because a sibling moved, hands back the
/// **same** allocation (`shader_map` caches it per node). So a pointer compare
/// is the whole test, and a `false` here is a whole data texture that does not
/// cross the bus.
///
/// A free function so the rule is testable without a GL context — CI has none,
/// and "the same buffer uploads once" is exactly the claim a probe written
/// beside the real code would agree with by construction.
#[must_use]
pub fn would_upload(held: Option<&Arc<[u8]>>, incoming: &Arc<[u8]>) -> bool {
    !held.is_some_and(|held| Arc::ptr_eq(held, incoming))
}

/// The period `u_time` wraps on, in seconds. One hour.
///
/// **This is a resolution fix, not a taste one** (#968 review L6). `u_time` is
/// an `f32`, and `f32`'s ulp grows with magnitude: at 2.6e6 s (~30 days of
/// uptime) it is **0.25 s**, and at ~97 days it is a full second — so a shader
/// animating on `fract(u_time * 0.25)` would visibly judder, then freeze, on a
/// shell that had simply been up a while. Unwrapped seconds are a value whose
/// precision decays with how long the desktop has been running, which is the
/// worst possible failure mode: invisible in every test and every fresh
/// session.
///
/// An hour keeps the ulp at 0.06 ms — three orders of magnitude finer than a
/// frame — and is a round number a shader author can build against: any period
/// that divides 3600 (a second, a minute, four seconds, ten minutes) is
/// continuous across the wrap. A shader with a period that does *not* divide it
/// jumps once an hour; that is the documented cost, stated in the contract, and
/// it is a far smaller one than degrading forever.
///
/// **Nothing can lint that rule**, and the bundled reference shader broke it on
/// its first outing — `crates/hytte-plugin-preem-demo/shaders/spectrum.frag`
/// used `sin(u_time * 0.6)` (period 10.47 s), which stepped the ink colour in
/// one frame at every wrap. It now uses `2π/9` and says why, because the demo is
/// what a plugin author copies. If this constant ever moves, that file is the
/// one to re-check.
const TIME_WRAP_SECS: f64 = 3600.0;

/// Seconds since this surface's first frame, wrapped to [`TIME_WRAP_SECS`].
///
/// Extracted so the wrap is testable: everything else about `u_time` needs a GL
/// context, and "the value stays precise after a month of uptime" is not
/// something a running shell tells you until it is far too late.
#[allow(clippy::cast_possible_truncation)]
fn wrapped_seconds(elapsed: std::time::Duration) -> f32 {
    // `rem_euclid` rather than `%` so the result is never negative for any
    // input, including the zero-length first frame.
    (elapsed.as_secs_f64().rem_euclid(TIME_WRAP_SECS)) as f32
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
        Arc, COMPILE_FAILURE_REFUSED, DATA_UPLOAD_REFUSED, ProgramCache,
        RESOURCES_ALLOCATION_REFUSED, RefCell, SHADER_PREAMBLE, SHADER_VERT, ShaderFormat,
        ShaderState, TIME_WRAP_SECS, WARNED_SOURCES, WarnLatch, data_key, first_line, hgl,
        refuse_data_strip, source_key, warn_on_data_upload_failure, would_upload, wrapped_seconds,
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
            cache
                .ensure(&second, |s| builder.build(s))
                .unwrap()
                .copied(),
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

    // ── #968 review fixes ────────────────────────────────────────────────────

    /// A journal line for `source`, if the shipped composition would write one.
    ///
    /// Drives `ProgramCache::ensure` **and** `WarnLatch::claim` exactly the way
    /// `draw` does — the latch is asked only on the `Err` arm, never for a
    /// source that compiled. Written as a helper because the previous version of
    /// this test called `claim()` for a *good* source to stand in for the fixed
    /// shader in between, which the shell never does; its final assertion then
    /// pinned behaviour production cannot reach (#968 second review).
    fn compile_and_maybe_warn(
        cache: &mut ProgramCache<u32>,
        latch: &mut WarnLatch,
        builder: &Builder,
        source: &str,
        broken: bool,
    ) -> bool {
        let source: Arc<str> = Arc::from(source);
        builder.fail.set(broken);
        match cache.ensure(&source, |s| builder.build(s)) {
            Ok(_) => false,
            Err(failure) => latch.claim(failure.key),
        }
    }

    /// **M2.** The journal latch is keyed by **source**, so a plugin that ships
    /// a broken shader, fixes it, then breaks it *differently* gets a line for
    /// each break — driven through the shipped composition rather than by
    /// poking `claim` directly.
    ///
    /// The bug this replaces was a `Cell<bool>` set on the first failure and
    /// never cleared: the second distinct broken source was compiled, failed,
    /// blanked the widget — and logged nothing, ever again. That sequence is
    /// inside the design's own contract ("a *changed* source recompiles"), and
    /// the diagnostic it swallowed is the one the whole
    /// "broken shader → placeholder + one warning" story rests on.
    ///
    /// **A → good B → A stays silent**, and that is the stated rule rather than
    /// an accident: a successful compile never reaches the latch, so A is still
    /// remembered when it comes back. It was already reported, and the *compile*
    /// is retried either way (`ensure` clears `failed` on the good build).
    ///
    /// **Falsified** by making [`WarnLatch::claim`] a one-way bool (`if
    /// !self.said.is_empty() { return false }`): the "SECOND broken source"
    /// assertion goes red, which is exactly the shipped-bug behaviour.
    #[test]
    fn the_compile_warning_latch_is_per_source_not_per_surface() {
        const A: &str = "void main() { fragColourA = u_fg; }";
        const B: &str = "void main() { fragColor = u_fg; }";
        const C: &str = "void main() { fragColourC = u_fg; }";

        let builder = Builder::default();
        let mut cache: ProgramCache<u32> = ProgramCache::default();
        let mut latch = WarnLatch::default();

        assert!(
            compile_and_maybe_warn(&mut cache, &mut latch, &builder, A, true),
            "the first broken source is reported",
        );
        for _ in 0..8 {
            assert!(
                !compile_and_maybe_warn(&mut cache, &mut latch, &builder, A, true),
                "…and then goes quiet while it persists",
            );
        }
        assert!(
            !compile_and_maybe_warn(&mut cache, &mut latch, &builder, B, false),
            "a source that compiles writes no failure line at all",
        );
        assert!(
            compile_and_maybe_warn(&mut cache, &mut latch, &builder, C, true),
            "a SECOND broken source must be reported, not swallowed",
        );
        assert!(
            !compile_and_maybe_warn(&mut cache, &mut latch, &builder, C, true),
            "…once",
        );
        assert!(
            !compile_and_maybe_warn(&mut cache, &mut latch, &builder, A, true),
            "and A, already reported, stays quiet when it comes back",
        );
    }

    /// **M2 residual (#968 second review).** Two broken sources **alternating**
    /// cost two journal lines in total, not one per frame.
    ///
    /// A one-entry MRU bounded the log only while at most one broken source was
    /// in play: alternation evicted the other key every frame, so every frame
    /// wrote a line — measured at 10 for 10 frames. The bound is what the latch
    /// is *for*, so it has to hold under the case a one-slot cache breaks on.
    ///
    /// **Falsified** by shrinking [`WARNED_SOURCES`] to `1`: the first count is
    /// 10.
    #[test]
    fn two_broken_sources_alternating_cost_two_lines() {
        const A: &str = "void main() { fragColourA = u_fg; }";
        const B: &str = "void main() { fragColourB = u_fg; }";

        let builder = Builder::default();
        let mut cache: ProgramCache<u32> = ProgramCache::default();
        let mut latch = WarnLatch::default();

        let mut lines = 0_u32;
        for frame in 0..10 {
            let source = if frame % 2 == 0 { A } else { B };
            if compile_and_maybe_warn(&mut cache, &mut latch, &builder, source, true) {
                lines += 1;
            }
        }
        assert_eq!(lines, 2, "two distinct broken sources, two lines");

        // The bound holds, and it is the right way round: `WARNED_SOURCES` more
        // distinct broken sources evict A, so A is reported a *second* time
        // rather than blanking the widget in silence.
        for n in 0..WARNED_SOURCES {
            let source = format!("void main() {{ fragColour{n} = u_fg; }}");
            if compile_and_maybe_warn(&mut cache, &mut latch, &builder, &source, true) {
                lines += 1;
            }
        }
        assert_eq!(
            lines as usize,
            2 + WARNED_SOURCES,
            "each new distinct broken source is reported once",
        );
        assert!(
            compile_and_maybe_warn(&mut cache, &mut latch, &builder, A, true),
            "A was evicted by the bound, so it is reported again rather than \
             silently blanking the widget",
        );
    }

    /// **L2.** A source whose hash collides with a latched-**failed** key is
    /// still handed to the driver: `failed` closes the collision with a byte
    /// compare, exactly as `held` does.
    ///
    /// Before this, `failed: Option<u64>` refused a colliding source without
    /// ever compiling it and without a journal line — a silently blank widget
    /// for a shader that is fine. Astronomically unlikely with 64-bit `SipHash`
    /// over two sources in one widget's lifetime, which is why it is a one-word
    /// guard; but the module docs claimed the guard already existed.
    ///
    /// The collision is *simulated* rather than found (finding one is the point
    /// of a cryptographic hash): the cache is put into the exact state a
    /// collision would produce — a `failed` entry whose key equals the incoming
    /// key but whose source does not — and the shipped comparison is asked.
    ///
    /// **Falsified** by comparing only the key in `ensure`'s `failed` arm: the
    /// good source comes back `Ok(None)` with `builds == 0`.
    #[test]
    fn a_failed_key_collision_still_reaches_the_driver() {
        let builder = Builder::default();
        let mut cache: ProgramCache<u32> = ProgramCache::default();
        let good: Arc<str> = Arc::from("void main() { fragColor = u_fg; }");

        // The state a collision would leave: same key, different source.
        cache.failed = Some((source_key(&good), Arc::from("a different source")));

        assert_eq!(
            cache.ensure(&good, |s| builder.build(s)).unwrap().copied(),
            Some(1),
            "a source that merely collides with a failed key must still compile",
        );
        assert_eq!(builder.builds.get(), 1, "the driver was actually asked");
    }

    /// **L7.** `would_upload` is identity, not equality: the same allocation
    /// uploads once however many renders arrive, and a *different* allocation
    /// with the same bytes uploads again.
    ///
    /// The second half is not a wart — it is what makes the rule cheap and
    /// sound. Comparing 4 MiB per render to avoid an upload would cost more than
    /// the upload; the reconciler's contract is that an unchanged node hands
    /// back the same `Arc`, which `shader_map`'s per-node cache now actually
    /// honours (#968 review M1).
    ///
    /// **Falsified** by making `would_upload` return `true` unconditionally: the
    /// counting probe below rises with every frame.
    #[test]
    fn the_same_buffer_uploads_once_however_many_renders_arrive() {
        let shared: Arc<[u8]> = Arc::from(&[1u8, 2, 3, 4][..]);
        let mut held: Option<Arc<[u8]>> = None;
        let mut uploads = 0_u32;

        // Twenty renders of one unchanged frame — two monitors at 10 Hz.
        for _ in 0..20 {
            if would_upload(held.as_ref(), &shared) {
                uploads += 1;
                held = Some(Arc::clone(&shared));
            }
        }
        assert_eq!(uploads, 1, "one upload for twenty renders");

        // A genuinely new frame uploads.
        let next: Arc<[u8]> = Arc::from(&[9u8, 9, 9, 9][..]);
        assert!(would_upload(held.as_ref(), &next));

        // Equal bytes in a distinct allocation upload too, and that is the
        // documented trade rather than a defect.
        let equal: Arc<[u8]> = Arc::from(&[1u8, 2, 3, 4][..]);
        assert!(!Arc::ptr_eq(&shared, &equal));
        assert!(would_upload(Some(&shared), &equal));
    }

    /// The wrap period as an `f32`, for the range assertion below. `3600.0` is
    /// exactly representable, so the cast is lossless — but the lint cannot know
    /// that from a `const`, and burying an `allow` inside the assertion would
    /// hide it from a reader.
    #[allow(clippy::cast_possible_truncation)]
    fn wrap_secs_f32() -> f32 {
        TIME_WRAP_SECS as f32
    }

    /// **L6.** `u_time` wraps hourly, so its `f32` resolution does not decay
    /// with the shell's uptime.
    ///
    /// Unwrapped, `origin.elapsed()` as an `f32` has a 0.25 s ulp after a month
    /// and a 1 s ulp after three — a shader animating on it would judder, then
    /// freeze, on a desktop that had simply been left running. That is a defect
    /// no test and no fresh session can see, which is why the wrap is a constant
    /// with a test rather than a comment.
    ///
    /// **Falsified** by dropping the `rem_euclid`: the month-long case comes
    /// back 2.6e6 and the resolution assertion goes red.
    #[test]
    fn u_time_wraps_hourly_and_keeps_its_resolution() {
        use std::time::Duration;

        assert!((wrapped_seconds(Duration::ZERO) - 0.0).abs() < f32::EPSILON);
        assert!((wrapped_seconds(Duration::from_millis(1500)) - 1.5).abs() < 1e-6);

        // One month of uptime: the value stays inside the hour…
        let month = Duration::from_hours(30 * 24);
        let t = wrapped_seconds(month);
        assert!(
            (0.0..wrap_secs_f32()).contains(&t),
            "wrapped into the period, got {t}",
        );
        // …and a millisecond later is still a *different* number, which is the
        // whole point. Unwrapped, 2.6e6 s has an ulp of 0.25 s and this fails.
        let later = wrapped_seconds(month + Duration::from_millis(1));
        assert_ne!(
            t.to_bits(),
            later.to_bits(),
            "a millisecond must still move u_time after a month of uptime",
        );

        // Exactly on the boundary the clock restarts, continuously for any
        // period that divides the wrap — which is what the contract promises.
        let hour = Duration::from_secs_f64(TIME_WRAP_SECS);
        assert!(wrapped_seconds(hour).abs() < 1e-3);
        assert!((wrapped_seconds(hour + Duration::from_millis(250)) - 0.25).abs() < 1e-3);
    }

    // ── #1023 item 1: the data-upload latch is keyed, not a bare bool ───────

    /// `data_key` really does key on every field it claims to — a collision
    /// between two distinct shapes would silently reunite two latches that
    /// should stay independent.
    ///
    /// **Falsified** by hashing only `width` (or only `height`, or dropping
    /// `format`) in [`data_key`]: one of the `assert_ne!`s below goes red.
    #[test]
    fn data_key_distinguishes_width_height_and_format() {
        let base = data_key(64, 64, ShaderFormat::R8);
        assert_ne!(
            base,
            data_key(65, 64, ShaderFormat::R8),
            "width must matter"
        );
        assert_ne!(
            base,
            data_key(64, 65, ShaderFormat::R8),
            "height must matter"
        );
        assert_ne!(
            base,
            data_key(64, 64, ShaderFormat::Rgba8),
            "format must matter",
        );
        assert_eq!(
            base,
            data_key(64, 64, ShaderFormat::R8),
            "and it is deterministic",
        );
    }

    /// **#1023 item 1 / #1020 review MEDIUM 1.** The data-upload journal
    /// latch is keyed by shape, so a **second, differently shaped** refused
    /// grid gets its own line instead of being swallowed by the first.
    ///
    /// Driven directly against [`WarnLatch::claim`] with [`data_key`]-shaped
    /// keys — the same seam `the_compile_warning_latch_is_per_source_not_per_surface`
    /// drives for `warned_compile` — because reaching the real call site
    /// needs a GL context this crate's hermetic suite does not have; what is
    /// under test here is the keying, which is exactly what the shipped bug
    /// got wrong.
    ///
    /// **Falsified** by reverting `warned_data` to a bare `Cell<bool>` (i.e.
    /// asking this test to drive `Cell<bool>::replace(true)` instead of
    /// `WarnLatch::claim`): the "a second, DIFFERENT refused shape" assertion
    /// goes red — that is the #1020-shipped behaviour this fixes.
    #[test]
    fn a_second_differently_shaped_refused_grid_still_gets_its_own_line() {
        let mut latch = WarnLatch::default();

        let a = data_key(64, 64, ShaderFormat::R8);
        let b = data_key(128, 1, ShaderFormat::Rgba8);

        assert!(latch.claim(a), "the first refused shape is reported");
        for _ in 0..8 {
            assert!(!latch.claim(a), "…and then goes quiet while it persists");
        }
        assert!(
            latch.claim(b),
            "a second, DIFFERENT refused shape must be reported, not swallowed \
             (#1023 item 1; shipped as #1020's MEDIUM 1)",
        );
        assert!(!latch.claim(b), "…once");
        assert!(!latch.claim(a), "and a, already reported, stays quiet");
    }

    // ── #1023 item 4: the refusal message describes what actually happens ──

    /// The data-refusal message says the surface **keeps its last frame**,
    /// not that it "draws nothing" — #1020's shipped wording, and #1020's
    /// review LOW 3: the early return in `draw` happens before
    /// `narrow_to_fit_rect`, the only thing that clears, so a refused upload
    /// after at least one good frame leaves that frame on screen rather than
    /// going blank.
    ///
    /// **Falsified** by reverting the message to #1020's "the widget draws
    /// nothing until the plugin sends a grid this driver will take".
    #[test]
    fn the_data_refusal_message_says_the_last_frame_stays_not_that_nothing_draws() {
        assert!(
            DATA_UPLOAD_REFUSED.contains("keeps whatever it last successfully drew"),
            "message must say the widget's last frame stays up: {DATA_UPLOAD_REFUSED:?}",
        );
        assert!(
            !DATA_UPLOAD_REFUSED.contains("draws nothing"),
            "must not claim the widget goes blank when the last frame is still showing: \
             {DATA_UPLOAD_REFUSED:?}",
        );
    }

    /// **PR #1031 review L2.** `draw`'s other two early-return arms —
    /// resources-allocation failure and compile failure — return before
    /// `narrow_to_fit_rect` exactly like the data-upload arm does, so
    /// [`DATA_UPLOAD_REFUSED`]'s own reasoning for not claiming "draws
    /// nothing" applies to them too. #1020 shipped both of them still
    /// saying it, twenty-five and sixty lines from the site item 4 actually
    /// fixed.
    ///
    /// **Falsified** by reverting either
    /// [`RESOURCES_ALLOCATION_REFUSED`] or [`COMPILE_FAILURE_REFUSED`] to
    /// #1020's wording ("… it draws nothing …" / "… the widget draws
    /// nothing …").
    #[test]
    fn none_of_draws_three_early_return_messages_claim_the_widget_draws_nothing() {
        for msg in [
            DATA_UPLOAD_REFUSED,
            RESOURCES_ALLOCATION_REFUSED,
            COMPILE_FAILURE_REFUSED,
        ] {
            assert!(
                !msg.contains("draws nothing"),
                "none of draw()'s early-return arms may claim the widget draws nothing — \
                 narrow_to_fit_rect (the only thing that clears) is never reached from any of \
                 them (#1023 item 4; PR #1031 review L2): {msg:?}",
            );
            assert!(
                msg.contains("keeps whatever it last successfully drew"),
                "…and each should say what actually happens instead: {msg:?}",
            );
        }
    }

    // ── PR #1031 review M2: the key/warn composition, not just its halves ──

    /// Count the `tracing` events this module emits while `emit` runs.
    ///
    /// A local twin of `gl_surface::tests::counting_events` /
    /// `shader_map::tests::counting_events` — see either's longer doc for
    /// why `tracing_core`'s per-callsite, process-wide `Interest` cache
    /// makes this necessary at all.
    ///
    /// **The warm-up is unconditional, not conditional on today's test
    /// roster** (PR #1031 review M1). It is true *right now* that nothing
    /// else in this crate's suite reaches `hytte_ui::shader_surface`'s
    /// `tracing::warn!` callsites — `draw` is their only production caller
    /// and no test drives it, since this file has no `imp::tests`. That is
    /// exactly the argument `gl_surface`'s twin made one round ago, and the
    /// same round invalidated it by adding a `#[gtk::test]` that drives
    /// `draw` on any machine with a driver: a subscriber-less first hit
    /// caches `Interest::never()` for the callsite process-wide and blanks
    /// the count here with `left: 0, right: 2` (#991/#1014/#1022). The
    /// warm-up costs three lines and one silent `warn!`, and it makes the
    /// helper correct regardless of what the next test in this file does.
    fn counting_events(emit: impl FnOnce()) -> u32 {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU32, Ordering};

        const TARGET: &str = "hytte_ui::shader_surface";
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
        warn_on_data_upload_failure(
            &RefCell::new(WarnLatch::default()),
            &state_with_data_size((0, 0)),
            &hgl::Error::TextureSize {
                size: (0, 0),
                limit: 0,
            },
        );

        let count = StdArc::new(AtomicU32::new(0));
        tracing::subscriber::with_default(Counting(StdArc::clone(&count)), emit);
        count.load(Ordering::Relaxed)
    }

    /// A minimal well-formed `ShaderState` for the composition test below.
    fn state_with_data_size(data_size: (u32, u32)) -> ShaderState {
        ShaderState {
            fragment: Arc::from("void main() {}"),
            data: Arc::from(&[0u8; 4][..]),
            format: ShaderFormat::R8,
            data_size,
            scale: 1,
            values: Vec::new(),
        }
    }

    /// **PR #1031 review M2.** The refusal reaches `warned_data` and a real
    /// journal line through [`warn_on_data_upload_failure`] — the exact
    /// function `draw`'s data-upload arm calls, keyed by
    /// [`data_upload_key`]'s translation of the real `ShaderState` the site
    /// reads, not a constant and not `data_key`/`WarnLatch` exercised in
    /// isolation by this file's other two tests.
    ///
    /// `hgl::Error` is constructed by hand rather than through a real
    /// `Texture::new` refusal — the driver-refusal case itself still needs
    /// real GL, but everything downstream of it, which is what item 1
    /// actually fixed, does not, and this test needs no feature gate and no
    /// display to run. Since the second fix round (review H1) this *is* the
    /// whole reporting half: `upload_data` calls this and returns `None`, so
    /// there is no `Result` and no skip-flag left at `draw`'s call site for
    /// a one-line mutation to drop.
    ///
    /// **Falsified** by replacing [`data_upload_key`]'s body with a
    /// constant (PR #1031 review's own mutation, translated to this now-
    /// hoisted site — the original was `let key = 0_u64;` inline at the old
    /// call site): the "a different shape must cost its own line" count
    /// drops from 2 to 1. Also red on emptying this function's body.
    #[test]
    fn a_data_upload_failure_reaches_the_widgets_own_latch_and_journal_line() {
        let latch = RefCell::new(WarnLatch::default());
        let base = state_with_data_size((100_000, 1));
        let different = state_with_data_size((100_001, 1));
        let err = || hgl::Error::TextureSize {
            size: (100_000, 1),
            limit: 4096,
        };

        let emitted = counting_events(|| {
            warn_on_data_upload_failure(&latch, &base, &err());
            // A repeat of the SAME shape must not re-warn…
            warn_on_data_upload_failure(&latch, &base, &err());
            // …while a DIFFERENT one still earns its own line.
            warn_on_data_upload_failure(&latch, &different, &err());
        });
        assert_eq!(
            emitted, 2,
            "a repeated refusal of the same shape must cost one journal line; a DIFFERENT \
             refused shape must cost its own (#1023 item 1; PR #1031 review M2 — this drives \
             the composition upload_data actually calls, not data_key/WarnLatch in isolation)",
        );
        assert_eq!(
            latch.borrow().said.len(),
            2,
            "…and both shapes are latched, keyed by data_upload_key, so a later repeat of \
             either stays quiet",
        );
    }

    /// **PR #1031 third-pass review LOW 2(b) (#1046).** The refusal arm's
    /// *state* half, pinned with no GL context: on a refused reallocation
    /// the shape must **not** advance, or the next frame skips the retry
    /// and samples a texture with no storage behind it — the pre-#977 bug,
    /// restored verbatim (`upload_data`'s own comment at its call site
    /// explains why).
    ///
    /// **Falsified** by adding `*data_shape = shape;` (the very assignment
    /// the `Ok` arm makes two lines below the refusal) to
    /// [`refuse_data_strip`]: this test's shape assertion goes red while
    /// `cargo test` and workspace clippy both stay green, because the
    /// GL-backed call site this feeds (`imp::upload_data`) needs a live
    /// driver to reach.
    #[test]
    fn refusing_a_data_strip_does_not_advance_its_shape() {
        let mut data_shape = (4_u32, 4_u32, ShaderFormat::R8);
        let mut data_source: Option<Arc<[u8]>> = Some(Arc::from(&[0u8; 4][..]));
        let warned = RefCell::new(WarnLatch::default());
        let state = state_with_data_size((8, 8));
        let error = hgl::Error::TextureSize {
            size: (8, 8),
            limit: 4096,
        };

        refuse_data_strip(&mut data_shape, &mut data_source, &warned, &state, &error);

        assert_eq!(
            data_shape,
            (4, 4, ShaderFormat::R8),
            "a refused reallocation must not advance the shape — the next frame has to retry \
             the same allocation, not sample a texture with no storage behind it (pre-#977 bug)",
        );
        assert!(
            data_source.is_none(),
            "the source must still be forgotten so a later successful upload is not skipped as \
             an unchanged repeat",
        );
    }
}
