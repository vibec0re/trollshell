//! The host half of #893's shader widget: the trust checks a
//! [`wire::Node::Shader`] passes before it becomes a
//! [`UiNode::Shader`](hytte::ui::Node::Shader), and the theme bag that reaches
//! the shader as uniforms.
//!
//! Split out of [`wire_map`](super::wire_map) because it is the one arm of that
//! walk with a *policy* in it. Every other arm is a field-for-field projection;
//! this one asks whether the plugin may draw a shader at all, whether the source
//! and the buffer are within the hygiene caps, and whether the buffer's length
//! matches the grid it claims — and, on any "no", degrades to the broken-widget
//! placeholder rather than dropping the node or the frame.
//!
//! # What is enforced here, and what emphatically is not
//!
//! #893 settled the boundary as **route 0: the plugin socket is the boundary**
//! (`docs/superpowers/specs/2026-09-06-preem-gl-renderer-design.md`, "Trust
//! boundary for #893"). The socket is `0600` inside a `0700` directory under
//! `$XDG_RUNTIME_DIR`, so anything that can send a frame already runs as the
//! user, and a shader is exactly as trusted as the plugin's own native code.
//!
//! So what lives here is a capability check and three cheap shape checks. There
//! is **no source validator**, and that is a measured absence rather than a
//! todo: naga — the one Rust GLSL frontend in reach — cannot parse the ES
//! profile at all (`#version 300/310/320 es` each come back `InvalidVersion` +
//! `InvalidProfile("es")`, reproduced twice independently on #893), so the
//! spec's original route-1 caps on IR size and unbounded loops had no enforcer
//! and were dropped rather than pretended. `glslangValidator` stays what it is:
//! a CI gate over shaders that live in this tree (`nix/lint-glsl.py`), which a
//! plugin's runtime source is not.
//!
//! **Blast radius, stated once so it is not implied anywhere else:** GTK
//! requests no robust GL context and there is one share group per display, so a
//! plugin shader that hangs or resets the GPU takes every GL surface in the
//! shell with it, with no notification and no recovery. Realistic worst case is
//! a shell restart. The upgrade path, if a plugin is ever *not* trusted, is an
//! out-of-process shader host ("route 3") — named in the spec, not built.

use std::sync::Arc;

use hytte::ui::gl_surface::GlValue;
use hytte::ui::shader_surface::{ShaderFormat, ShaderState};
use hytte::ui::Node as UiNode;
use hytte_plugin_proto::wire::{MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, ShaderData};
use hytte_plugin_proto::{Capability, Manifest};
use hytte_preem as kit;

use super::preem_render::{self, Scope, Warned};

/// What a plugin's connection is allowed to render, as far as the mapping pass
/// is concerned.
///
/// One bool today, and a struct anyway: a capability that gates a *node* rather
/// than an [`Effect`](hytte_plugin_proto::Effect) has to travel with the render
/// frame to the GTK thread, and threading a bare `bool` through `SlotRender`,
/// `to_ui_node` and the `Walk` would be unreadable at every call site and
/// unextendable at the next one.
///
/// `Copy`, so passing it down the recursion costs nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Grants {
    /// [`Capability::Shader`] was declared, so [`wire::Node::Shader`] renders.
    pub(super) shader: bool,
}

impl Grants {
    /// The grants a registering plugin's manifest asks for.
    ///
    /// Auto-granted from the manifest, exactly like every other capability —
    /// `session.rs`'s `enforce_capabilities` grants what a manifest declares and
    /// that is the whole policy (see the trust-boundary note in the module
    /// docs). What the capability buys is legibility: a plugin that can run GPU
    /// code says so where the control-center and the audit log can read it.
    pub(super) fn from_manifest(manifest: &Manifest) -> Self {
        Self {
            shader: manifest.capabilities.contains(&Capability::Shader),
        }
    }

    /// No grants — what a tree with no plugin behind it gets. Test-only in
    /// practice, and [`Default`] elsewhere.
    #[cfg(test)]
    pub(super) fn none() -> Self {
        Self::default()
    }
}

/// A borrowed view of a [`wire::Node::Shader`]'s fields, so the checks below
/// take one argument instead of ten.
pub(super) struct ShaderNode<'a> {
    /// The node's reconciliation key, kept on the placeholder too.
    pub(super) id: Option<&'a str>,
    /// Logical size in pixels, **before** `scale`.
    pub(super) width: u32,
    /// Logical size in pixels, before `scale`.
    pub(super) height: u32,
    /// The wire's integer upscale hint, unclamped (`0` still means `1`).
    pub(super) scale: u32,
    /// The fragment body.
    pub(super) fragment: &'a str,
    /// The data buffer.
    pub(super) data: &'a [u8],
    /// How to read the buffer.
    pub(super) format: ShaderData,
    /// The data grid in texels, `(width, height)`.
    pub(super) data_size: (u32, u32),
    /// CSS classes, kept on the placeholder too.
    pub(super) classes: &'a [String],
}

/// Why a shader node will not be drawn.
///
/// A closed enum rather than a `bool` + a log line, because the *decision* is
/// what a hermetic test can assert (CI has no GL and cannot look at the pixels)
/// and because two of these five want different journal lines pointing at
/// different fixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// The plugin's manifest does not declare [`Capability::Shader`].
    NoCapability,
    /// The source is over [`MAX_SHADER_SOURCE_BYTES`].
    SourceTooLarge {
        /// What was sent.
        bytes: usize,
    },
    /// The buffer is over [`MAX_SHADER_DATA_BYTES`].
    DataTooLarge {
        /// What was sent.
        bytes: usize,
    },
    /// `data.len()` is not `data_width * data_height * bytes_per_texel` — the
    /// same invariant a malformed `Pixels` buffer trips.
    MalformedData {
        /// What was sent.
        bytes: usize,
    },
    /// The drawn size or the data grid has a zero side. Not a malformed frame,
    /// but nothing to draw and nothing to sample, so it takes the placeholder
    /// rather than allocating a degenerate texture.
    EmptyGrid,
}

impl Refusal {
    /// Which one-shot diagnostic slot this refusal claims.
    ///
    /// Two slots, not five: "you did not ask for the capability" and "your node
    /// is out of shape" are the two different mistakes with two different fixes,
    /// and a tree that makes both gets told both once. Splitting the three shape
    /// refusals further would spend three of the eight bits
    /// [`Warned`](super::preem_render::Warned) has on one node kind.
    fn slot(self) -> Warned {
        match self {
            Self::NoCapability => Warned::ShaderDenied,
            Self::SourceTooLarge { .. }
            | Self::DataTooLarge { .. }
            | Self::MalformedData { .. }
            | Self::EmptyGrid => Warned::ShaderCap,
        }
    }
}

/// Whether this node may be drawn, and if not, why.
///
/// Pure, so the whole policy is testable without GTK, without GL and without a
/// socket — which matters because every one of these paths ends in "the same
/// empty placeholder", and the pixels cannot tell them apart.
///
/// Order is deliberate: the capability first, because a plugin that may not draw
/// shaders at all should be told *that* rather than told its buffer is the wrong
/// length. After that, cheapest check first.
pub(super) fn refusal(grants: Grants, node: &ShaderNode<'_>) -> Option<Refusal> {
    if !grants.shader {
        return Some(Refusal::NoCapability);
    }
    if node.fragment.len() > MAX_SHADER_SOURCE_BYTES {
        return Some(Refusal::SourceTooLarge {
            bytes: node.fragment.len(),
        });
    }
    if node.data.len() > MAX_SHADER_DATA_BYTES {
        return Some(Refusal::DataTooLarge {
            bytes: node.data.len(),
        });
    }
    if !node
        .format
        .data_len_ok(node.data_size.0, node.data_size.1, node.data.len())
    {
        return Some(Refusal::MalformedData {
            bytes: node.data.len(),
        });
    }
    if node.width == 0 || node.height == 0 || node.data_size.0 == 0 || node.data_size.1 == 0 {
        return Some(Refusal::EmptyGrid);
    }
    None
}

/// Map one shader node, applying [`refusal`] and — where it says yes — building
/// the [`ShaderState`] the widget draws from.
pub(super) fn map_shader(scope: &Scope, grants: Grants, node: &ShaderNode<'_>) -> UiNode {
    if let Some(refused) = refusal(grants, node) {
        warn(scope, node, refused);
        return placeholder(node);
    }
    // The `scale` hint takes the *same* clamp a `Pixels` node's does, so a
    // hostile or buggy plugin cannot request a monster allocation through the
    // one node kind that skipped the check. `0` silently means `1` — the wire
    // contract's documented alias, not worth a warning.
    let scale = super::wire_map::clamp_pixels_scale(node.width, node.height, node.scale);
    UiNode::Shader {
        id: node.id.map(ToOwned::to_owned),
        width: node.width.saturating_mul(scale),
        height: node.height.saturating_mul(scale),
        state: Arc::new(ShaderState {
            fragment: Arc::from(node.fragment),
            data: Arc::from(node.data),
            format: to_ui_format(node.format),
            data_size: node.data_size,
            scale,
            values: theme_values(),
        }),
        classes: node.classes.to_vec(),
    }
}

/// The broken-widget placeholder: an empty surface keeping the node's id and
/// classes, so CSS chrome stays put and a later valid frame updates the same
/// slot rather than rebuilding the tree around it.
///
/// A `Pixels` of `0 × 0` sharing [`preem_render::nothing`]'s one process-wide
/// empty buffer — the same placeholder `wire_map`'s malformed-`Pixels` arm and
/// `preem_render`'s over-cap arm already draw, so a degraded node looks the same
/// whatever degraded it.
fn placeholder(node: &ShaderNode<'_>) -> UiNode {
    UiNode::Pixels {
        id: node.id.map(ToOwned::to_owned),
        width: 0,
        height: 0,
        data: preem_render::nothing(),
        scale: 1,
        classes: node.classes.to_vec(),
    }
}

/// One journal line per refusal *kind* per plugin tree, for the life of the
/// shell — the [`Warned`] latch, on the same terms as the node/depth caps.
///
/// A refused node is refused on **every** frame (the manifest does not change,
/// and neither does a 20 KiB source), so an unlatched warning would be one line
/// per frame per monitor.
fn warn(scope: &Scope, node: &ShaderNode<'_>, refused: Refusal) {
    if !preem_render::warn_once(scope, refused.slot()) {
        return;
    }
    match refused {
        Refusal::NoCapability => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            "plugin rendered a Shader node without declaring Capability::Shader; it draws the \
             broken-widget placeholder and the rest of the tree renders normally. Add `Shader` \
             to the manifest's capabilities (further occurrences in this tree are silenced for \
             the rest of this shell run)",
        ),
        Refusal::SourceTooLarge { bytes } => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            bytes,
            cap = MAX_SHADER_SOURCE_BYTES,
            "plugin Shader source exceeds the host's size cap; rendering the placeholder \
             (further occurrences in this tree are silenced)",
        ),
        Refusal::DataTooLarge { bytes } => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            bytes,
            cap = MAX_SHADER_DATA_BYTES,
            "plugin Shader data buffer exceeds the host's size cap; rendering the placeholder \
             (further occurrences in this tree are silenced)",
        ),
        Refusal::MalformedData { bytes } => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            bytes,
            data_width = node.data_size.0,
            data_height = node.data_size.1,
            format = ?node.format,
            "plugin Shader buffer size != data_width*data_height*bytes_per_texel; rendering \
             the placeholder (further occurrences in this tree are silenced)",
        ),
        Refusal::EmptyGrid => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            width = node.width,
            height = node.height,
            data_width = node.data_size.0,
            data_height = node.data_size.1,
            "plugin Shader has a zero-sized surface or data grid; rendering the placeholder \
             (further occurrences in this tree are silenced)",
        ),
    }
}

/// The wire format onto the reconciler's. Exhaustive, so a format added to
/// either side is a compile error here.
fn to_ui_format(format: ShaderData) -> ShaderFormat {
    match format {
        ShaderData::R8 => ShaderFormat::R8,
        ShaderData::Rgba8 => ShaderFormat::Rgba8,
        ShaderData::R32f => ShaderFormat::R32f,
    }
}

/// The theme colours a shader reads, resolved **per mapping pass** so a desktop
/// accent change re-tints every shader widget with no wire traffic and no plugin
/// restart — the #396/#885 property, inherited rather than reimplemented.
///
/// Every colour comes out of `hytte-preem`'s own resolution
/// ([`kit::palette_snapshot`], [`kit::DisplayStyle::admit_role_ink`]), so the
/// accent/role precedence stays the kit's one implementation across the raster
/// arm, the `Scope` GL arm and this one.
///
/// The skin is the shell's **default** one — a shader node carries no
/// [`StyleRef`](hytte_plugin_proto::preem::StyleRef), and giving it one is
/// #885's follow-up rather than this issue's. What the two ink slots mean:
///
/// - `u_fg` is the skin's **own** lit ink, un-tinted ([`kit::Ink::Base`]) — what
///   a widget that opted out of the accent draws with.
/// - `u_accent` is the same ink **as the desktop accent tints it** (the default
///   path), which is what every un-pinned preem widget on screen is using. With
///   no accent installed, or on a skin whose [`AccentPolicy`] declines to follow
///   one, the two are equal — and that is the honest answer, not a bug.
///
/// The three status roles are resolved off the live theme and then run through
/// the kit's role seam, so they are legible on this skin's ground (#940). A
/// theme that has not resolved them yet — including the hermetic test binary,
/// which has no display — falls back to `u_accent` rather than to black.
fn theme_values() -> Vec<(&'static str, GlValue)> {
    let style = preem_render::default_display_style();
    let palette = kit::palette_snapshot(style);
    let base = kit::with_pins(
        kit::Pins {
            ink: kit::Ink::Base,
            field: None,
        },
        || kit::palette_snapshot(style),
    );
    let roles = preem_render::role_inks();
    let role = |ink: Option<kit::Rgba>| {
        ink.map_or(palette.ink, |ink| style.admit_role_ink(ink, None))
    };
    vec![
        ("u_bg", rgba(palette.bg)),
        ("u_fg", rgba(base.ink)),
        ("u_accent", rgba(palette.ink)),
        ("u_success", rgba(role(roles.success))),
        ("u_warning", rgba(role(roles.warning))),
        ("u_error", rgba(role(roles.error))),
    ]
}

/// One `[u8; 4]` colour as a `vec4` in `0.0..=1.0`, straight alpha.
///
/// Normalized rather than the `0..255` channels the `Scope` blit takes: that
/// pipeline's arithmetic is integer end to end because it has to match the kit
/// byte for byte, and this one has nothing to match — a plugin author writing
/// `mix(u_bg, u_fg, t)` wants GLSL's own units.
fn rgba(color: kit::Rgba) -> GlValue {
    GlValue::Vec4([
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        f32::from(color[3]) / 255.0,
    ])
}

#[cfg(test)]
mod tests {
    use super::{
        Grants, MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, Refusal, ShaderData, ShaderNode,
        Warned, map_shader, refusal, theme_values,
    };
    use crate::plugins::preem_render::{self, Scope};
    use hytte::ui::Node as UiNode;

    /// A well-formed node: a 4-texel `R8` strip on a 144×48 surface.
    fn ok_node<'a>(fragment: &'a str, data: &'a [u8]) -> ShaderNode<'a> {
        ShaderNode {
            id: Some("spectrum"),
            width: 144,
            height: 48,
            scale: 1,
            fragment,
            data,
            format: ShaderData::R8,
            data_size: (u32::try_from(data.len()).unwrap_or(0), 1),
            classes: &[],
        }
    }

    /// The grant that lets a shader through.
    fn granted() -> Grants {
        Grants { shader: true }
    }

    /// **Capability gating.** A `Shader` node from a plugin whose manifest omits
    /// `Capability::Shader` is refused — first, before any shape check, so the
    /// journal names the actual fix.
    ///
    /// **Falsified** by deleting the `if !grants.shader` guard: the first
    /// assertion goes red.
    #[test]
    fn a_shader_without_the_capability_is_refused_first() {
        let data = [1u8, 2, 3, 4];
        let node = ok_node("void main() { fragColor = u_fg; }", &data);
        assert_eq!(refusal(Grants::none(), &node), Some(Refusal::NoCapability));
        assert_eq!(refusal(granted(), &node), None, "granted, it draws");

        // Even a node that is *also* malformed is reported as the capability
        // problem: telling a plugin its buffer is short when it may not draw
        // shaders at all sends it after the wrong bug.
        let mut broken = ok_node("void main() {}", &data);
        broken.data_size = (99, 99);
        assert_eq!(refusal(Grants::none(), &broken), Some(Refusal::NoCapability));
    }

    /// **The 16 KiB source cap**, at the boundary: exactly at the cap draws,
    /// one byte over is refused.
    ///
    /// **Falsified** by dropping the `fragment.len()` check, or by writing it
    /// `>=`: the at-the-cap case then refuses and the assertion goes red.
    #[test]
    fn the_source_cap_bites_one_byte_over() {
        let data = [0u8];
        let at_cap = "x".repeat(MAX_SHADER_SOURCE_BYTES);
        assert_eq!(refusal(granted(), &ok_node(&at_cap, &data)), None);

        let over = "x".repeat(MAX_SHADER_SOURCE_BYTES + 1);
        assert_eq!(
            refusal(granted(), &ok_node(&over, &data)),
            Some(Refusal::SourceTooLarge {
                bytes: MAX_SHADER_SOURCE_BYTES + 1
            }),
        );
    }

    /// **The 4 MiB data cap**, at the boundary. The over-cap buffer is built
    /// with a matching grid so the *length* check is what refuses it and not the
    /// malformed-shape check underneath.
    #[test]
    fn the_data_cap_bites_one_byte_over() {
        let at_cap = vec![0u8; MAX_SHADER_DATA_BYTES];
        let mut node = ok_node("void main() {}", &at_cap);
        node.data_size = (u32::try_from(MAX_SHADER_DATA_BYTES).unwrap(), 1);
        assert_eq!(refusal(granted(), &node), None);

        let over = vec![0u8; MAX_SHADER_DATA_BYTES + 1];
        let mut node = ok_node("void main() {}", &over);
        node.data_size = (u32::try_from(MAX_SHADER_DATA_BYTES + 1).unwrap(), 1);
        assert_eq!(
            refusal(granted(), &node),
            Some(Refusal::DataTooLarge {
                bytes: MAX_SHADER_DATA_BYTES + 1
            }),
        );
    }

    /// The `len == w * h * bytes_per_texel` invariant, per format — the check
    /// that stops the widget handing GL a buffer shorter than the region it
    /// names, and the reason `upload_u8`'s zero-padding is a backstop rather
    /// than the policy.
    ///
    /// **Falsified** by deleting the `data_len_ok` call: the three mismatched
    /// cases become `None`.
    #[test]
    fn a_buffer_that_does_not_match_its_grid_is_refused() {
        let four = [0u8; 4];
        let sixteen = [0u8; 16];

        let mut node = ok_node("void main() {}", &four);
        node.format = ShaderData::Rgba8;
        node.data_size = (1, 1);
        assert_eq!(refusal(granted(), &node), None, "1 Rgba8 texel is 4 bytes");
        node.data_size = (2, 1);
        assert_eq!(
            refusal(granted(), &node),
            Some(Refusal::MalformedData { bytes: 4 }),
            "2 Rgba8 texels want 8",
        );

        let mut node = ok_node("void main() {}", &sixteen);
        node.format = ShaderData::R32f;
        node.data_size = (4, 1);
        assert_eq!(refusal(granted(), &node), None, "4 f32 texels are 16 bytes");
        node.data_size = (4, 2);
        assert_eq!(
            refusal(granted(), &node),
            Some(Refusal::MalformedData { bytes: 16 }),
        );
    }

    /// A zero-sided surface or data grid takes the placeholder rather than a
    /// degenerate texture. Checked *after* the length invariant, because a
    /// `0 × 0` grid with an empty buffer is consistent and would otherwise fall
    /// through to a `Texture::new` that refuses the extent anyway.
    #[test]
    fn a_zero_sided_node_is_refused() {
        let empty: [u8; 0] = [];
        let mut node = ok_node("void main() {}", &empty);
        node.data_size = (0, 0);
        assert_eq!(refusal(granted(), &node), Some(Refusal::EmptyGrid));

        let data = [0u8];
        let mut node = ok_node("void main() {}", &data);
        node.width = 0;
        assert_eq!(refusal(granted(), &node), Some(Refusal::EmptyGrid));
    }

    /// **Every refusal renders the placeholder**, keeping the node's id and
    /// classes, and each refusal *kind* costs exactly one journal line per tree.
    ///
    /// The second half is what makes the latch load-bearing: a refused node is
    /// refused on every frame and every monitor, so an unlatched warning is one
    /// line per frame per monitor for the life of the shell.
    ///
    /// **Falsified** by returning the real node from `map_shader` on a refusal
    /// (the first assertion), or by dropping the `warn_once` guard (the count).
    #[test]
    fn a_refused_shader_renders_the_placeholder_and_warns_once() {
        let classes = ["ts-shader".to_owned()];
        let data = [0u8; 4];
        let mut node = ok_node("void main() {}", &data);
        node.classes = &classes;

        let scope = Scope::detached("shader-refusal-placeholder");
        let before = preem_render::warnings(Warned::ShaderDenied);
        for _ in 0..5 {
            let mapped = map_shader(&scope, Grants::none(), &node);
            assert_eq!(
                mapped,
                UiNode::Pixels {
                    id: Some("spectrum".to_owned()),
                    width: 0,
                    height: 0,
                    data: preem_render::nothing(),
                    scale: 1,
                    classes: classes.to_vec(),
                },
                "the placeholder keeps the id and the classes",
            );
        }
        assert_eq!(
            preem_render::warnings(Warned::ShaderDenied) - before,
            1,
            "five refused frames cost one journal line",
        );
    }

    /// The happy path: a granted, well-formed node becomes a `UiNode::Shader`
    /// whose natural size is the wire size **times the scale hint** — the same
    /// sizing rule `Node::Pixels` follows — and whose state carries the source,
    /// the buffer and the theme bag.
    ///
    /// **Falsified** by dropping the `saturating_mul(scale)`: the size
    /// assertions go red and a `scale: 2` chip renders at 1×.
    #[test]
    fn a_granted_shader_becomes_a_shader_node_at_its_scaled_size() {
        let data = [1u8, 2, 3, 4];
        let mut node = ok_node("void main() { fragColor = u_accent; }", &data);
        node.scale = 3;

        let scope = Scope::detached("shader-happy-path");
        match map_shader(&scope, granted(), &node) {
            UiNode::Shader {
                id,
                width,
                height,
                state,
                classes,
            } => {
                assert_eq!(id.as_deref(), Some("spectrum"));
                assert_eq!((width, height), (144 * 3, 48 * 3), "size × scale");
                assert_eq!(&*state.fragment, "void main() { fragColor = u_accent; }");
                assert_eq!(&*state.data, &data[..]);
                assert_eq!(state.data_size, (4, 1));
                assert_eq!(state.scale, 3, "the shader is told its own scale");
                assert!(classes.is_empty());
            }
            other => panic!("mapped to {other:?}"),
        }
    }

    /// An absurd `scale` is clamped through the **same** helper a `Pixels` node
    /// goes through, rather than being honoured into a monster allocation. The
    /// shader widget is the one node kind that could have skipped this check by
    /// simply not calling it.
    ///
    /// **Falsified** by using `node.scale` directly in `map_shader`.
    #[test]
    fn an_absurd_scale_is_clamped_like_a_pixels_one() {
        let data = [0u8; 4];
        let mut node = ok_node("void main() {}", &data);
        node.scale = 100_000;

        let scope = Scope::detached("shader-scale-clamp");
        match map_shader(&scope, granted(), &node) {
            UiNode::Shader { width, height, .. } => {
                assert!(
                    width <= 16_384 && height <= 16_384,
                    "clamped to the scaled-dimension cap, got {width}x{height}",
                );
            }
            other => panic!("mapped to {other:?}"),
        }
    }

    /// The theme bag names exactly the uniforms the wire contract publishes,
    /// and every colour is a normalized `vec4`.
    ///
    /// The *values* are the kit's and are asserted there; what this pins is the
    /// interface — a name dropped here is a uniform the preamble declares, the
    /// contract promises and nothing ever sets, which reads as an all-black
    /// colour on glass and as nothing at all in a test.
    ///
    /// **Falsified** by deleting any entry from `theme_values`.
    #[test]
    fn the_theme_bag_fills_every_colour_the_contract_promises() {
        let values = theme_values();
        for name in [
            "u_bg",
            "u_fg",
            "u_accent",
            "u_success",
            "u_warning",
            "u_error",
        ] {
            let found = values
                .iter()
                .find(|(key, _)| *key == name)
                .unwrap_or_else(|| panic!("the theme bag must set `{name}`"));
            let hytte::ui::GlValue::Vec4(channels) = found.1 else {
                panic!("`{name}` must be a vec4");
            };
            assert!(
                channels.iter().all(|c| (0.0..=1.0).contains(c)),
                "`{name}` is normalized, got {channels:?}",
            );
        }
        assert_eq!(values.len(), 6, "and nothing else");
    }
}
