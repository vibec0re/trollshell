//! The shader widget's plugin side (#893): a builder for
//! [`Node::Shader`](crate::proto::Node::Shader), and the negotiation gate that
//! decides whether this host can draw one at all.
//!
//! # What a shader widget is
//!
//! A plugin ships a **fragment shader body** plus a **data buffer**; the shell
//! compiles the body once, keeps the linked program, and per frame re-uploads
//! only the buffer and the uniforms. So the widget's steady-state cost is its
//! data, and a shader that reacts to something — a spectrum, a load average, a
//! temperature — is a small array pushed on a timer.
//!
//! The interface a body is written against is stated in full on
//! [`Node::Shader`](crate::proto::Node::Shader) and is the versioned contract:
//! the plugin writes the body **only** (no `#version`, no `in`/`out`, no
//! `uniform` declarations — the shell prepends all of that), reads `v_uv`,
//! `u_time`, `u_resolution`, `u_scale`, `u_data`, `u_data_size` and the six
//! theme colours, and writes `fragColor`.
//!
//! Two things worth knowing before the first shader, both on that page in full:
//! `fragColor` is **premultiplied** (a half-transparent red is
//! `vec4(0.5, 0.0, 0.0, 0.5)`; opaque output needs no thought), and `u_time`
//! **wraps hourly**, so animate on a period that divides 3600.
//!
//! # A minimal example
//!
//! ```
//! use hytte_plugin::proto::ShaderData;
//! use hytte_plugin::shader::Shader;
//!
//! // A bar graph over eight bytes of data, tinted to the desktop accent.
//! const BARS: &str = "\
//! void main() {
//!     float level = texture(u_data, vec2(v_uv.x, 0.5)).r;
//!     float lit = step(v_uv.y, level);
//!     fragColor = mix(u_bg, u_accent, lit);
//! }
//! ";
//!
//! let levels: Vec<u8> = vec![10, 40, 90, 200, 255, 128, 64, 8];
//! let node = Shader::new("bars", BARS)
//!     .size(128, 32)
//!     .scale(2)
//!     .data(ShaderData::R8, 8, 1, levels)
//!     .node();
//!
//! // `None` when this session's host has not advertised the shader vocabulary
//! // — outside a live session, as here, it always has not.
//! assert!(node.is_none());
//! ```
//!
//! # Why the builder can say no
//!
//! [`Shader::node`] returns an `Option`, and every `None` is deliberate:
//!
//! - **The host has not advertised [`SHADER_VOCAB`]** — see
//!   [`host_speaks_shader`]. Emitting the node anyway would send a variant an
//!   older shell cannot decode, killing the session and putting the SDK into
//!   the redial crash-loop the vocabulary counter exists to prevent (#437).
//!   There is no CPU form to fall back to, so the plugin decides what to render
//!   instead — a label, a `preem` widget, or nothing.
//! - **The source, the buffer, or the grid it describes fails one of the
//!   host's data caps** — the same four
//!   `trollshell/src/plugins/shader_map.rs`'s `refusal` enforces on the node
//!   it receives: the source over [`MAX_SHADER_SOURCE_BYTES`], the buffer
//!   over [`MAX_SHADER_DATA_BYTES`], `data.len()` not equal to `data_width *
//!   data_height * format.bytes_per_texel()`, or a grid side over
//!   [`MAX_SHADER_DATA_EXTENT`] (#1021, mirroring #1020's host-side extent
//!   cap). [`Shader::cap_refusal`] names which, and with what numbers, so
//!   refusing here turns a silent blank chip into a value the plugin can log
//!   or branch on, instead of shipping a node the host quietly replaces with
//!   the broken-widget placeholder.
//!
//! **This SDK-side check is a courtesy to the plugin author, not a security
//! boundary — route 0, the plugin socket itself, is that** (#893's trust
//! boundary; see
//! `docs/superpowers/specs/2026-09-06-preem-gl-renderer-design.md` §"Trust
//! boundary for #893"). The host makes every one of these decisions again,
//! independently, over the bytes it actually received, and does not trust
//! this crate's arithmetic.
//!
//! All of it is testable without a session: [`fits_source_cap`],
//! [`fits_data_cap`], [`fits_data_extent`] and [`Shader::cap_refusal`] are the
//! pure predicates, and [`testing::with_shader_support`] forces the
//! negotiation.

use crate::proto::{
    MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, Node, SHADER_VOCAB, ShaderData,
};
// `MAX_SHADER_DATA_EXTENT` is not among `hytte-plugin-proto`'s root
// re-exports (`proto::MAX_SHADER_DATA_BYTES` et al. above), so it is read
// straight from `wire` — the same module
// `trollshell/src/plugins/shader_map.rs` reads it from — rather than widening
// `hytte-plugin-proto`'s public surface for this one constant.
use hytte_plugin_proto::wire::MAX_SHADER_DATA_EXTENT;

/// Whether this session's host advertised the shader vocabulary (#893) — i.e.
/// whether [`negotiated_vocab`](crate::display::negotiated_vocab) has reached
/// [`SHADER_VOCAB`].
///
/// `false` outside a live session, and `false` against any shell built before
/// #893, because such a host sends no
/// [`Hello`](crate::proto::HostMsg::Hello) at all.
#[must_use]
pub fn host_speaks_shader() -> bool {
    crate::display::negotiated_vocab() >= SHADER_VOCAB
}

/// Whether `fragment` is within the host's source-size cap
/// ([`MAX_SHADER_SOURCE_BYTES`]).
#[must_use]
pub fn fits_source_cap(fragment: &str) -> bool {
    fragment.len() <= MAX_SHADER_SOURCE_BYTES
}

/// Whether a buffer of `len` bytes is within the host's data cap
/// ([`MAX_SHADER_DATA_BYTES`]).
#[must_use]
pub fn fits_data_cap(len: usize) -> bool {
    len <= MAX_SHADER_DATA_BYTES
}

/// Whether `(width, height)` is within the host's per-axis grid-extent cap
/// ([`MAX_SHADER_DATA_EXTENT`]), checked on each side individually.
///
/// The cap [`fits_data_cap`] does not imply: a product says nothing about its
/// factors, so a `32768 × 1` `R8` grid is 32 KiB — three orders of magnitude
/// under [`MAX_SHADER_DATA_BYTES`] — and still wider than a great many
/// drivers' `GL_MAX_TEXTURE_SIZE` (see [`MAX_SHADER_DATA_EXTENT`]'s docs for
/// the full story, #977/#1020).
#[must_use]
pub fn fits_data_extent(width: u32, height: u32) -> bool {
    width <= MAX_SHADER_DATA_EXTENT && height <= MAX_SHADER_DATA_EXTENT
}

/// Why [`Shader::node`] refused to build over a source, buffer, or grid cap —
/// the typed diagnostic behind that `None`, for a plugin author who wants to
/// know *why* rather than only *whether*. Returned by [`Shader::cap_refusal`].
///
/// Mirrors `trollshell/src/plugins/shader_map.rs`'s host-side `Refusal`
/// field-for-field, over the same [`crate::proto`] wire constants and the
/// same [`ShaderData::data_len_ok`] invariant — one source of truth for each
/// number, read from both sides of the socket.
///
/// **This is a courtesy to the plugin author, not a security boundary** — see
/// the [module docs](self). Route 0, the socket itself, is that boundary
/// (#893's trust boundary); the host makes this same decision again,
/// independently, over the bytes it actually received, and does not trust
/// this crate's arithmetic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShaderCapRefusal {
    /// The source is over [`MAX_SHADER_SOURCE_BYTES`].
    SourceTooLarge {
        /// What was given.
        bytes: usize,
    },
    /// The buffer is over [`MAX_SHADER_DATA_BYTES`].
    DataTooLarge {
        /// What was given.
        bytes: usize,
    },
    /// `data.len()` is not `width * height * format.bytes_per_texel()` — the
    /// same invariant a malformed [`Node::Pixels`](crate::proto::Node::Pixels)
    /// buffer trips.
    MalformedData {
        /// What was given.
        len: usize,
        /// The grid claimed, in texels.
        size: (u32, u32),
        /// The format claimed.
        format: ShaderData,
    },
    /// A data-grid side is over [`MAX_SHADER_DATA_EXTENT`], applied to
    /// `data_width`/`data_height` individually — the cap the byte total
    /// does not imply. See [`fits_data_extent`].
    GridTooLarge {
        /// The grid that was claimed, in texels.
        size: (u32, u32),
    },
}

/// Builder for a [`Node::Shader`].
///
/// Built from an id and a fragment body; everything else has a default that
/// draws *something* (a 1×1 zero `R8` grid, a 64×64 surface at 1×), so a shader
/// that ignores `u_data` needs no [`data`](Shader::data) call.
#[derive(Clone, Debug)]
pub struct Shader {
    id: Option<String>,
    width: u32,
    height: u32,
    scale: u32,
    fragment: String,
    data: Vec<u8>,
    format: ShaderData,
    data_width: u32,
    data_height: u32,
    classes: Vec<String>,
    tooltip: Option<String>,
}

impl Shader {
    /// A shader widget keyed by `id`, drawing `fragment`.
    ///
    /// The id is the reconciliation key and is strongly recommended: without
    /// one, a shader that changes position among its siblings is rebuilt, which
    /// throws away the compiled program and restarts `u_time`. Use
    /// [`anonymous`](Shader::anonymous) if there is genuinely nothing to key on.
    #[must_use]
    pub fn new(id: impl Into<String>, fragment: impl Into<String>) -> Self {
        Self {
            id: Some(id.into()),
            width: 64,
            height: 64,
            scale: 1,
            fragment: fragment.into(),
            data: vec![0],
            format: ShaderData::R8,
            data_width: 1,
            data_height: 1,
            classes: Vec::new(),
            tooltip: None,
        }
    }

    /// Drop the reconciliation key. See [`new`](Shader::new) for what it costs.
    #[must_use]
    pub fn anonymous(mut self) -> Self {
        self.id = None;
        self
    }

    /// The widget's logical size in pixels, before [`scale`](Shader::scale).
    #[must_use]
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    /// The integer upscale hint: the natural size becomes `width*scale` ×
    /// `height*scale`, exactly as for [`Node::Pixels`](crate::proto::Node::Pixels).
    /// The value also reaches the shader as `u_scale`.
    #[must_use]
    pub fn scale(mut self, scale: u32) -> Self {
        self.scale = scale;
        self
    }

    /// The data buffer and the grid it describes.
    ///
    /// `data.len()` must be exactly `width * height * format.bytes_per_texel()`
    /// — the same invariant a malformed [`Pixels`](crate::proto::Node::Pixels)
    /// buffer trips. This setter does **not** reshape or pad a mismatched
    /// buffer to fit — that would hide the plugin's own bug — but
    /// [`Shader::node`] does refuse to build one
    /// ([`ShaderCapRefusal::MalformedData`], #1021), naming the exact numbers
    /// given rather than a guessed-at fix, on the same terms the host's own
    /// journal line does.
    #[must_use]
    pub fn data(mut self, format: ShaderData, width: u32, height: u32, data: Vec<u8>) -> Self {
        self.format = format;
        self.data_width = width;
        self.data_height = height;
        self.data = data;
        self
    }

    /// GTK CSS classes, applied verbatim.
    #[must_use]
    pub fn classes<S: Into<String>>(mut self, classes: impl IntoIterator<Item = S>) -> Self {
        self.classes = classes.into_iter().map(Into::into).collect();
        self
    }

    /// Hover text (plain, not markup) — see the tooltip section on
    /// [`Node`](crate::proto::Node).
    ///
    /// Honoured: the host puts it on the surface with `set_tooltip_text`, on
    /// build and on change, and a re-render dropping it clears the hover. (It
    /// was a documented no-op until #968's review caught it — a wire field, this
    /// method and a pinned golden byte, with nothing reading any of them.)
    #[must_use]
    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    /// The specific source/buffer/grid cap this builder would be refused for,
    /// if any — the typed counterpart to [`Shader::node`]'s `None`. See
    /// [`ShaderCapRefusal`] and the [module docs](self) for what this is (a
    /// courtesy diagnostic) and is not (a security boundary).
    ///
    /// Checked in the same order `shader_map::refusal` does over the fields it
    /// shares with this type — cheapest and most fundamental first: source
    /// size, then buffer size, then the shape invariant, then the per-axis
    /// extent.
    #[must_use]
    pub fn cap_refusal(&self) -> Option<ShaderCapRefusal> {
        if self.fragment.len() > MAX_SHADER_SOURCE_BYTES {
            return Some(ShaderCapRefusal::SourceTooLarge {
                bytes: self.fragment.len(),
            });
        }
        if self.data.len() > MAX_SHADER_DATA_BYTES {
            return Some(ShaderCapRefusal::DataTooLarge {
                bytes: self.data.len(),
            });
        }
        if !self
            .format
            .data_len_ok(self.data_width, self.data_height, self.data.len())
        {
            return Some(ShaderCapRefusal::MalformedData {
                len: self.data.len(),
                size: (self.data_width, self.data_height),
                format: self.format,
            });
        }
        if !fits_data_extent(self.data_width, self.data_height) {
            return Some(ShaderCapRefusal::GridTooLarge {
                size: (self.data_width, self.data_height),
            });
        }
        None
    }

    /// The node, or `None` if this host cannot draw it or the payload is over a
    /// cap — see the [module docs](self) for both cases, and
    /// [`Shader::cap_refusal`] for a typed reason in the latter.
    #[must_use]
    pub fn node(self) -> Option<Node> {
        if !host_speaks_shader() || self.cap_refusal().is_some() {
            return None;
        }
        Some(Node::Shader {
            id: self.id,
            width: self.width,
            height: self.height,
            scale: self.scale,
            fragment: self.fragment,
            data: self.data,
            format: self.format,
            data_width: self.data_width,
            data_height: self.data_height,
            classes: self.classes,
            tooltip: self.tooltip,
        })
    }
}

/// Test seam for plugins that assert their shader node's shape.
///
/// A shader node is only built inside a live session against a host that
/// advertised [`SHADER_VOCAB`], which no unit test has — so without this,
/// [`Shader::node`] is `None` in every test and the whole builder is
/// unassertable. The twin of
/// [`display::testing`](crate::display::testing::with_render_mode).
pub mod testing {
    use super::SHADER_VOCAB;

    /// Puts the previous generation back however `f` returns — including by
    /// panic, which is the normal way a failing assertion leaves.
    struct Restore(u16);

    impl Drop for Restore {
        fn drop(&mut self) {
            crate::display::set_negotiated(self.0);
        }
    }

    /// Call `f` with the shader vocabulary forced on or off, then restore the
    /// session's real negotiated generation.
    ///
    /// **Tests only.** Forcing it *on* inside a running plugin would emit a
    /// `Node::Shader` at a host that never advertised it, which cannot decode
    /// the frame, drops the session, and leaves the plugin in exactly the #437
    /// redial crash-loop the negotiation exists to prevent. That is why there is
    /// no plain setter.
    pub fn with_shader_support<T>(supported: bool, f: impl FnOnce() -> T) -> T {
        let _restore = Restore(crate::display::negotiated_vocab());
        crate::display::set_negotiated(if supported { SHADER_VOCAB } else { 0 });
        f()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SHADER_DATA_BYTES, MAX_SHADER_DATA_EXTENT, MAX_SHADER_SOURCE_BYTES, Node, Shader,
        ShaderCapRefusal, ShaderData, fits_data_cap, fits_data_extent, fits_source_cap,
        host_speaks_shader, testing::with_shader_support,
    };

    const BODY: &str = "void main() { fragColor = u_accent; }";

    /// A host that never advertised the vocabulary gets **no** shader node —
    /// the #882 negotiation applied to #893's variant, and the property that
    /// keeps a shader-capable plugin from crash-looping against an older shell.
    ///
    /// **Falsified** by dropping the `host_speaks_shader()` guard from
    /// [`Shader::node`]: the first assertion goes red, and a rebuilt plugin
    /// starts emitting a variant an old host cannot decode.
    #[test]
    fn a_host_that_does_not_speak_shaders_gets_no_node() {
        assert!(!host_speaks_shader(), "no session, no advertisement");
        assert!(Shader::new("s", BODY).node().is_none());

        with_shader_support(true, || {
            assert!(host_speaks_shader());
            assert!(Shader::new("s", BODY).node().is_some());
        });
        with_shader_support(false, || {
            assert!(Shader::new("s", BODY).node().is_none());
        });
    }

    /// **The SDK refuses an over-cap source**, at the same 16 KiB boundary the
    /// host refuses it at: exactly at the cap builds, one byte over does not.
    ///
    /// Refusing here rather than only host-side turns a silent blank chip into
    /// a value the plugin can branch on, and the two limits are the same const.
    ///
    /// **Falsified** by dropping the `fits_source_cap` guard from
    /// [`Shader::node`], or by writing the predicate `<`.
    #[test]
    fn the_sdk_refuses_a_source_over_the_cap() {
        with_shader_support(true, || {
            let at_cap = "x".repeat(MAX_SHADER_SOURCE_BYTES);
            assert!(fits_source_cap(&at_cap));
            assert!(
                Shader::new("s", at_cap).node().is_some(),
                "exactly at the cap still builds",
            );

            let over = "x".repeat(MAX_SHADER_SOURCE_BYTES + 1);
            assert!(!fits_source_cap(&over));
            assert!(
                Shader::new("s", over).node().is_none(),
                "one byte over is refused",
            );
        });
    }

    /// The same, for the 4 MiB data cap.
    ///
    /// **Falsified** by dropping the `fits_data_cap` guard from
    /// [`Shader::cap_refusal`].
    #[test]
    fn the_sdk_refuses_a_buffer_over_the_cap() {
        with_shader_support(true, || {
            assert!(fits_data_cap(MAX_SHADER_DATA_BYTES));
            assert!(!fits_data_cap(MAX_SHADER_DATA_BYTES + 1));
            let over = vec![0u8; MAX_SHADER_DATA_BYTES + 1];
            // width * 1 * 1 == over.len(), so this is a pure bytes-cap
            // refusal — the shape invariant underneath it is satisfied.
            let width = u32::try_from(MAX_SHADER_DATA_BYTES + 1).unwrap();
            let shader = Shader::new("s", BODY).data(ShaderData::R8, width, 1, over);
            assert_eq!(
                shader.cap_refusal(),
                Some(ShaderCapRefusal::DataTooLarge {
                    bytes: MAX_SHADER_DATA_BYTES + 1
                }),
            );
            assert!(shader.node().is_none());
        });
    }

    /// **The SDK refuses a grid side over the per-axis extent cap** (#1021,
    /// mirroring #1020's host-side `Refusal::GridTooLarge` in
    /// `trollshell/src/plugins/shader_map.rs`'s `refusal`): exactly at the cap
    /// builds, one texel over — on either axis — does not. The byte cap does
    /// not imply this one: a `32768×1` `R8` grid is 32 KiB, three orders of
    /// magnitude under [`MAX_SHADER_DATA_BYTES`], and is refused here anyway.
    ///
    /// **Falsified** by dropping the `fits_data_extent` guard from
    /// [`Shader::cap_refusal`], or by writing it `<`: the at-cap assertions
    /// go red.
    #[test]
    fn the_sdk_refuses_a_grid_over_the_extent_cap() {
        with_shader_support(true, || {
            assert!(fits_data_extent(MAX_SHADER_DATA_EXTENT, 1));
            let at_cap = vec![0u8; MAX_SHADER_DATA_EXTENT as usize];
            assert!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, MAX_SHADER_DATA_EXTENT, 1, at_cap.clone())
                    .node()
                    .is_some(),
                "exactly at the cap still builds",
            );
            assert!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, 1, MAX_SHADER_DATA_EXTENT, at_cap)
                    .node()
                    .is_some(),
                "…on either axis",
            );

            assert!(!fits_data_extent(MAX_SHADER_DATA_EXTENT + 1, 1));
            let over = vec![0u8; MAX_SHADER_DATA_EXTENT as usize + 1];
            let shader =
                Shader::new("s", BODY).data(ShaderData::R8, MAX_SHADER_DATA_EXTENT + 1, 1, over);
            assert_eq!(
                shader.cap_refusal(),
                Some(ShaderCapRefusal::GridTooLarge {
                    size: (MAX_SHADER_DATA_EXTENT + 1, 1)
                }),
                "one texel over the width is refused",
            );
            assert!(shader.node().is_none());

            // #977/#1020's own reported input, spelled out: 32 KiB of R8, a
            // legal length for its grid, and unallocatable by many drivers.
            let issue = vec![0u8; 32_768];
            assert_eq!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, 32_768, 1, issue)
                    .cap_refusal(),
                Some(ShaderCapRefusal::GridTooLarge { size: (32_768, 1) }),
            );
        });
    }

    /// The specific at-cap shapes named in #1021 — accepted whole, not just
    /// individually under one cap, but under all three checks at once (bytes,
    /// the shape invariant, and per-axis extent).
    ///
    /// - `4096×1` `R8`: the per-axis extent cap itself, 4 KiB — nowhere near
    ///   the byte cap.
    /// - `1024×1024` `Rgba8`: [`MAX_SHADER_DATA_BYTES`]'s own documented
    ///   example, 4 MiB exactly.
    /// - `2048×2048` `R8`: the byte cap reached in a grid a GPU will actually
    ///   take — the corrected shape of the boundary test #977 fixed on the
    ///   host side (`shader_map.rs`'s `the_data_cap_bites_one_byte_over`,
    ///   which rejected the same buffer laid out `1×MAX_SHADER_DATA_BYTES`).
    #[test]
    fn the_documented_at_cap_shapes_are_all_accepted() {
        with_shader_support(true, || {
            assert!(
                Shader::new("s", BODY)
                    .data(
                        ShaderData::R8,
                        MAX_SHADER_DATA_EXTENT,
                        1,
                        vec![0; MAX_SHADER_DATA_EXTENT as usize],
                    )
                    .node()
                    .is_some(),
                "4096x1 R8",
            );
            assert!(
                Shader::new("s", BODY)
                    .data(ShaderData::Rgba8, 1024, 1024, vec![0; 1024 * 1024 * 4])
                    .node()
                    .is_some(),
                "1024x1024 Rgba8 == MAX_SHADER_DATA_BYTES exactly",
            );
            let side = u32::try_from(MAX_SHADER_DATA_BYTES).unwrap().isqrt();
            assert_eq!(
                side, 2048,
                "documented as the byte cap's square R8 side, and within the extent cap",
            );
            assert!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, side, side, vec![0; MAX_SHADER_DATA_BYTES])
                    .node()
                    .is_some(),
                "2048x2048 R8 == MAX_SHADER_DATA_BYTES exactly",
            );
        });
    }

    /// **Agreement with the host, by construction.**
    /// `trollshell/src/plugins/shader_map.rs` cannot be linked from this crate
    /// (it pulls in the whole GTK/GL shell), so its `Refusal` variants and
    /// boundary shapes are copied here as literal expectations instead of
    /// being asserted directly against its `refusal()`. What actually makes
    /// the two sides agree is that both read the *same*
    /// [`MAX_SHADER_SOURCE_BYTES`] / [`MAX_SHADER_DATA_BYTES`] /
    /// [`MAX_SHADER_DATA_EXTENT`] constants and the same
    /// `ShaderData::data_len_ok` invariant out of `hytte-plugin-proto`'s
    /// `wire` module — there is exactly one copy of each number, and this
    /// test only pins that both sides still consult it the same way.
    ///
    /// Each case below is transcribed from `shader_map.rs`'s own tests:
    /// `the_data_cap_bites_one_byte_over`,
    /// `a_grid_side_over_the_extent_cap_is_refused`, and
    /// `a_buffer_that_does_not_match_its_grid_is_refused`.
    #[test]
    fn sdk_and_host_agree_on_the_boundary_shapes() {
        with_shader_support(true, || {
            // shader_map.rs `the_data_cap_bites_one_byte_over`: one byte over
            // the byte cap is refused for its bytes, even though the shape
            // and extent checks beneath it would also fire — bytes is the
            // more basic fact and is checked first, on both sides.
            let width = u32::try_from(MAX_SHADER_DATA_BYTES + 1).unwrap();
            let over = vec![0u8; MAX_SHADER_DATA_BYTES + 1];
            assert_eq!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, width, 1, over)
                    .cap_refusal(),
                Some(ShaderCapRefusal::DataTooLarge {
                    bytes: MAX_SHADER_DATA_BYTES + 1
                }),
            );

            // shader_map.rs `a_grid_side_over_the_extent_cap_is_refused`: the
            // #977 issue input — 32 KiB of R8, a legal length for its grid,
            // refused anyway.
            let issue = vec![0u8; 32_768];
            assert_eq!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, 32_768, 1, issue)
                    .cap_refusal(),
                Some(ShaderCapRefusal::GridTooLarge { size: (32_768, 1) }),
            );

            // shader_map.rs `a_buffer_that_does_not_match_its_grid_is_refused`:
            // 2 Rgba8 texels claimed (8 bytes wanted), 4 given.
            let four = vec![0u8; 4];
            assert_eq!(
                Shader::new("s", BODY)
                    .data(ShaderData::Rgba8, 2, 1, four)
                    .cap_refusal(),
                Some(ShaderCapRefusal::MalformedData {
                    len: 4,
                    size: (2, 1),
                    format: ShaderData::Rgba8,
                }),
            );
        });
    }

    /// Every builder setter lands in the node it builds, and the defaults are
    /// the documented ones — a 64×64 surface at 1× over a 1×1 zero `R8` grid, so
    /// a shader that ignores `u_data` needs no `data()` call.
    #[test]
    fn the_builder_carries_every_field_through() {
        with_shader_support(true, || {
            let default = Shader::new("d", BODY).node().expect("builds");
            match default {
                Node::Shader {
                    width,
                    height,
                    scale,
                    data,
                    format,
                    data_width,
                    data_height,
                    tooltip,
                    ..
                } => {
                    assert_eq!((width, height, scale), (64, 64, 1));
                    assert_eq!((format, data_width, data_height), (ShaderData::R8, 1, 1));
                    assert_eq!(data, vec![0]);
                    assert_eq!(tooltip, None);
                }
                other => panic!("built {other:?}"),
            }

            let full = Shader::new("full", BODY)
                .size(288, 96)
                .scale(2)
                .data(ShaderData::R32f, 4, 1, vec![0; 16])
                .classes(["ts-shader", "flat"])
                .tooltip("spectrum")
                .node()
                .expect("builds");
            match full {
                Node::Shader {
                    id,
                    width,
                    height,
                    scale,
                    fragment,
                    data,
                    format,
                    data_width,
                    data_height,
                    classes,
                    tooltip,
                } => {
                    assert_eq!(id.as_deref(), Some("full"));
                    assert_eq!((width, height, scale), (288, 96, 2));
                    assert_eq!(fragment, BODY);
                    assert_eq!(data.len(), 16);
                    assert_eq!((format, data_width, data_height), (ShaderData::R32f, 4, 1));
                    assert_eq!(classes, vec!["ts-shader".to_owned(), "flat".to_owned()]);
                    assert_eq!(tooltip.as_deref(), Some("spectrum"));
                }
                other => panic!("built {other:?}"),
            }

            let anon = Shader::new("gone", BODY)
                .anonymous()
                .node()
                .expect("builds");
            match anon {
                Node::Shader { id, .. } => assert_eq!(id, None),
                other => panic!("built {other:?}"),
            }
        });
    }

    /// **The SDK refuses a mismatched buffer, and does not reshape or pad it**
    /// (#1021, mirroring #1020's host-side `Refusal::MalformedData`):
    /// [`ShaderCapRefusal::MalformedData`] names exactly what the plugin
    /// gave — the buffer length and the grid it claimed — rather than a
    /// guessed-at fix, on the same terms the host's own journal line does.
    ///
    /// **Falsified** by dropping the `data_len_ok` guard from
    /// [`Shader::cap_refusal`], or by having [`Shader::data`] silently
    /// pad/truncate the buffer to fit instead: the numbers asserted below
    /// would then belong to a reshaped buffer, not the plugin's own.
    #[test]
    fn a_mismatched_buffer_is_refused_not_reshaped() {
        with_shader_support(true, || {
            let shader = Shader::new("s", BODY).data(ShaderData::Rgba8, 4, 4, vec![0; 3]);
            assert_eq!(
                shader.cap_refusal(),
                Some(ShaderCapRefusal::MalformedData {
                    len: 3,
                    size: (4, 4),
                    format: ShaderData::Rgba8,
                }),
                "names the plugin's own numbers, not a padded/reshaped guess",
            );
            assert!(shader.node().is_none());
        });
    }
}
