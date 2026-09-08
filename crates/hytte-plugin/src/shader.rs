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
//! [`Shader::node`] returns an `Option`, and both of its `None`s are deliberate:
//!
//! - **The host has not advertised [`SHADER_VOCAB`]** — see
//!   [`host_speaks_shader`]. Emitting the node anyway would send a variant an
//!   older shell cannot decode, killing the session and putting the SDK into
//!   the redial crash-loop the vocabulary counter exists to prevent (#437).
//!   There is no CPU form to fall back to, so the plugin decides what to render
//!   instead — a label, a `preem` widget, or nothing.
//! - **The source is over [`MAX_SHADER_SOURCE_BYTES`]** — the host refuses it
//!   too (broken-widget placeholder plus a warning), so refusing here turns a
//!   silent blank chip into a value the plugin can branch on. The data cap
//!   ([`MAX_SHADER_DATA_BYTES`]) is refused the same way.
//!
//! Both are testable without a session: [`fits_source_cap`] and
//! [`fits_data_cap`] are the pure predicates, and
//! [`testing::with_shader_support`] forces the negotiation.

use crate::proto::{
    MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, Node, SHADER_VOCAB, ShaderData,
};

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
    /// — the host renders the broken-widget placeholder otherwise, on the same
    /// terms a malformed [`Pixels`](crate::proto::Node::Pixels) buffer gets.
    /// This is deliberately **not** checked here: a builder that silently
    /// reshaped or padded a mismatched buffer would hide the plugin's own bug,
    /// and the host's journal line names both numbers.
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

    /// The node, or `None` if this host cannot draw it or the payload is over a
    /// cap — see the [module docs](self) for both cases.
    #[must_use]
    pub fn node(self) -> Option<Node> {
        if !host_speaks_shader()
            || !fits_source_cap(&self.fragment)
            || !fits_data_cap(self.data.len())
        {
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
        MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, Node, Shader, ShaderData, fits_data_cap,
        fits_source_cap, host_speaks_shader, testing::with_shader_support,
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
    /// **Falsified** by dropping the `fits_data_cap` guard.
    #[test]
    fn the_sdk_refuses_a_buffer_over_the_cap() {
        with_shader_support(true, || {
            assert!(fits_data_cap(MAX_SHADER_DATA_BYTES));
            assert!(!fits_data_cap(MAX_SHADER_DATA_BYTES + 1));
            let over = vec![0u8; MAX_SHADER_DATA_BYTES + 1];
            assert!(
                Shader::new("s", BODY)
                    .data(ShaderData::R8, 1, 1, over)
                    .node()
                    .is_none(),
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

    /// The builder does **not** reshape a mismatched buffer — it hands the host
    /// exactly what the plugin said, so the host's journal line names the
    /// plugin's own numbers instead of a silently padded pair.
    #[test]
    fn a_mismatched_buffer_is_passed_through_not_reshaped() {
        with_shader_support(true, || {
            let node = Shader::new("s", BODY)
                .data(ShaderData::Rgba8, 4, 4, vec![0; 3])
                .node()
                .expect("builds — the shape check is the host's");
            match node {
                Node::Shader {
                    data,
                    data_width,
                    data_height,
                    ..
                } => {
                    assert_eq!(data.len(), 3, "not padded to 64");
                    assert_eq!((data_width, data_height), (4, 4), "not reshaped to fit");
                }
                other => panic!("built {other:?}"),
            }
        });
    }
}
