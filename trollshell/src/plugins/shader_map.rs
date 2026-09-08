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
//! So what lives here is a capability check, a "does this session still have GL"
//! check, and four cheap shape checks. There
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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hytte::ui::Node as UiNode;
use hytte::ui::gl_surface::GlValue;
use hytte::ui::shader_surface::{ShaderFormat, ShaderState};
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

    /// Every grant — the test spelling of a manifest that declared them all.
    #[cfg(test)]
    pub(super) fn all() -> Self {
        Self { shader: true }
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
    /// Hover text. Reaches the reconciler on the drawn node; a refused node
    /// takes the placeholder, which has no tooltip field — see `placeholder`.
    pub(super) tooltip: Option<&'a str>,
}

/// Why a shader node will not be drawn.
///
/// A closed enum rather than a `bool` + a log line, because the *decision* is
/// what a hermetic test can assert (CI has no GL and cannot look at the pixels)
/// and because they want different journal lines pointing at different fixes.
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
    /// GL has been abandoned for this process — a context failed to create, and
    /// `hytte-ui` latched it.
    ///
    /// The one refusal here that is nobody's mistake, and the one with **no
    /// fallback**: a kit widget has a CPU implementation to fall back to and the
    /// shader widget has none by design (Annika, #893: "EGL should be available
    /// for all targets"). So the node takes the placeholder rather than mounting
    /// a `GtkGLArea` that can never draw — which would also pay for a fresh
    /// failed context per shader node, per monitor, per frame.
    ///
    /// # It is **sticky for the process**, and that is a decision (#968 L3)
    ///
    /// `hytte_ui::gl_surface`'s `ABANDONED` latch is set once and never cleared
    /// (`gl_surface.rs`, inherited from #954). For a kit widget a stale latch
    /// costs a CPU render — the picture is still right. For a shader widget it
    /// costs **the widget, permanently**: one transient context failure (a
    /// hot-plug race, a resume, a startup before `/dev/dri` is ready) blanks
    /// every shader in the session until the shell is restarted.
    ///
    /// Kept sticky anyway, for one reason: **there is nothing to re-try on.**
    /// Clearing the latch needs an event that says a context might now succeed,
    /// and the only honest candidate is `GdkDisplay`'s own open/close — which
    /// #954 owns, which no shader-widget test could reach, and which would make
    /// a failing display re-attempt a context per shader node per frame for
    /// ever. A blank widget plus a journal line naming the restart is the worse
    /// experience and the better failure mode. If a transient failure is ever
    /// actually observed on glass, clearing the latch on `GdkDisplay::opened`
    /// is the fix, and it belongs next to the latch rather than here.
    NoGl,
}

/// Whether this process still has GL, as far as the mapping pass is concerned.
///
/// A two-state enum rather than a `bool` so [`refusal`] cannot be called with
/// the sense inverted, and so it reads at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GlAvailability {
    /// No context failure has been observed.
    Available,
    /// A context failed to create and `hytte-ui` latched it for the process.
    Abandoned,
}

impl GlAvailability {
    /// What `hytte-ui` currently says. GTK-main-thread-local, like the latch
    /// itself.
    fn current() -> Self {
        if hytte::ui::gl_surface::gl_abandoned() {
            Self::Abandoned
        } else {
            Self::Available
        }
    }
}

impl Refusal {
    /// Which one-shot diagnostic slot this refusal claims.
    ///
    /// Two slots, not six: "you did not ask for the capability" and "this node
    /// will not draw" are the two different messages, and a tree that trips both
    /// gets told both once. Splitting the four remaining refusals further would
    /// spend four of the eight bits [`Warned`](super::preem_render::Warned) has
    /// on one node kind — and their journal lines already name which one fired,
    /// which is what an operator actually reads.
    ///
    /// [`NoGl`](Refusal::NoGl) shares the shape slot even though it is nobody's
    /// mistake, because `hytte-ui` has already logged the context failure once
    /// for the process; this line's job is only to say *which widgets* went
    /// quiet as a result.
    fn slot(self) -> Warned {
        match self {
            Self::NoCapability => Warned::ShaderDenied,
            Self::SourceTooLarge { .. }
            | Self::DataTooLarge { .. }
            | Self::MalformedData { .. }
            | Self::EmptyGrid
            | Self::NoGl => Warned::ShaderCap,
        }
    }
}

/// Whether this node may be drawn, and if not, why.
///
/// Pure — `gl` is passed in rather than read from
/// [`gl_abandoned`](hytte::ui::gl_surface::gl_abandoned) here — so the whole
/// policy is testable without GTK, without GL and without a socket, which
/// matters because every one of these paths ends in "the same empty
/// placeholder" and the pixels cannot tell them apart.
///
/// Order is deliberate. The capability comes first, because a plugin that may
/// not draw shaders at all should be told *that* rather than told its buffer is
/// the wrong length, and because it is the one refusal here the plugin author
/// can fix. `gl` comes second for the mirror reason: it is the one nobody can.
/// After that, cheapest check first.
pub(super) fn refusal(
    grants: Grants,
    gl: GlAvailability,
    node: &ShaderNode<'_>,
) -> Option<Refusal> {
    if !grants.shader {
        return Some(Refusal::NoCapability);
    }
    if gl == GlAvailability::Abandoned {
        return Some(Refusal::NoGl);
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
    if let Some(refused) = refusal(grants, GlAvailability::current(), node) {
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
        state: shared_state(scope, node, scale),
        classes: node.classes.to_vec(),
        tooltip: node.tooltip.map(ToOwned::to_owned),
    }
}

/// The [`ShaderState`] for this node — **the same `Arc`** every monitor's
/// mapping pass gets while the node has not changed.
///
/// # Why this cache exists (#968 review M1)
///
/// `to_ui_node` runs once per monitor per frame, and without a cache this
/// allocated a fresh `Arc<ShaderState>`, a fresh `Arc<str>` (copying the source)
/// and a fresh `Arc<[u8]>` (copying the buffer) every single time. Three
/// documented properties were therefore false, measured:
///
/// 1. [`ShaderSurface`](hytte::ui::ShaderSurface)'s data-upload dedup is an
///    `Arc::ptr_eq`, so it **could never fire** — the whole data texture went
///    across the bus on every render, including renders where nothing moved
///    (an accent re-tint, a resize).
/// 2. `set_state`'s documented `Arc::ptr_eq` fast path always fell through to
///    the derived `PartialEq`, comparing the whole fragment *and* the whole
///    buffer — up to 16 KiB + 4 MiB, per monitor, per frame.
/// 3. The mapping itself paid that allocation and memcpy per monitor per frame.
///
/// At the demo's 64 bytes none of it matters; at the caps it is ~160 MB/s of
/// memcpy plus as much memcmp on two monitors at 20 Hz. #911 solved exactly this
/// for `Pixels` by caching the `Arc<[u8]>` per scope; the shader arm did not
/// inherit it, so it does now.
///
/// # What it costs, stated
///
/// One value comparison per mapping pass, plus — on a hit — a [`Scope`] clone,
/// an `id.to_owned()` for the touched set, and two hash lookups. That
/// **replaces** a memcpy of the fragment and the buffer plus the memcmp
/// `set_state` was doing anyway, and removes the GPU upload entirely. Far
/// cheaper for anything but a tiny node; for a 64-byte demo tile the two are
/// within noise of each other, which is the honest way to put it.
///
/// # Anonymous nodes are not cached
///
/// There is no key to cache them under: an id is the reconciliation key, and
/// inventing an ordinal here would hand a node its *neighbour's* state on any
/// insert — the same hazard `Node::Preem`'s `Warned::NoId` warns about. An
/// anonymous shader therefore pays the old cost, which is one more reason
/// [`Node::Shader`](hytte_plugin_proto::wire::Node::Shader)'s own docs recommend
/// an id.
///
/// **Two shader nodes sharing one id in a tree defeat the cache the same way**,
/// and more quietly: each pass overwrites the other's entry, so neither ever
/// hits. The pixels stay correct — the states are rebuilt, not swapped — but the
/// upload dedup never fires. That is the mild end of the same mistake
/// `Warned::DuplicateId` warns about for preem nodes, where two widgets really
/// do collapse onto one renderer; here it costs performance rather than
/// correctness, which is why it is documented rather than diagnosed.
fn shared_state(scope: &Scope, node: &ShaderNode<'_>, scale: u32) -> Arc<ShaderState> {
    let values = theme_values();
    let Some(id) = node.id else {
        return Arc::new(build_state(node, scale, values));
    };
    TOUCHED.with_borrow_mut(|touched| {
        touched
            .entry(scope.clone())
            .or_default()
            .insert(id.to_owned());
    });
    STATES.with_borrow_mut(|states| {
        let per_scope = states.entry(scope.clone()).or_default();
        if let Some(held) = per_scope.get(id)
            && held.scale == scale
            && held.format == to_ui_format(node.format)
            && held.data_size == node.data_size
            && held.values == values
            && &*held.fragment == node.fragment
            && &*held.data == node.data
        {
            return Arc::clone(held);
        }
        let fresh = Arc::new(build_state(node, scale, values));
        per_scope.insert(id.to_owned(), Arc::clone(&fresh));
        fresh
    })
}

/// A fresh state, with no cache involved.
fn build_state(
    node: &ShaderNode<'_>,
    scale: u32,
    values: Vec<(&'static str, GlValue)>,
) -> ShaderState {
    ShaderState {
        fragment: Arc::from(node.fragment),
        data: Arc::from(node.data),
        format: to_ui_format(node.format),
        data_size: node.data_size,
        scale,
        values,
    }
}

thread_local! {
    /// The per-scope, per-node-id shared states — see [`shared_state`].
    ///
    /// GTK-main-thread-only, like every other table in the mapping path.
    ///
    /// # The four release sites, enumerated
    ///
    /// This used to say "the same lifecycle `preem_render`'s instance table
    /// has", which was true of three of that table's four sites and false of
    /// the fourth — and the missing one leaked a whole shader state (source plus
    /// buffer, up to 16 KiB + 4 MiB per node id) per departed plugin for the
    /// life of the shell. Naming a lifecycle by reference is how that got
    /// through, so the sites are listed rather than compared:
    ///
    /// 1. [`end_pass`] — swept per mapping pass; a node that left the tree drops
    ///    its state at the close of the pass that stopped touching it. Called by
    ///    `wire_map::to_ui_node`.
    /// 2. `region::reconcile_region`'s retain loop — a card leaving its region.
    ///    Covered by `card_leaving_its_region_releases_its_shader_states`.
    /// 3. `region::forget_departed_panel_scope` / `forget_previous_panel_scope`
    ///    — the drawer panel's half, refcounted across monitors.
    /// 4. `pump::drive_scope_releaser` — **the one that was missing**. The
    ///    region loops above are monitor-shaped, so with no monitor alive
    ///    (a docked lid closing, every output unplugged) this #921 subscriber is
    ///    the only thing that runs. Covered by
    ///    `a_departing_plugin_releases_its_shader_states_with_no_region_alive`.
    ///
    /// A `forget_scope` on a scope holding nothing is a `HashMap` miss, so every
    /// one of them is unconditional.
    static STATES: RefCell<HashMap<Scope, HashMap<String, Arc<ShaderState>>>> =
        RefCell::new(HashMap::new());

    /// The node ids touched by the pass currently in flight, per scope.
    static TOUCHED: RefCell<HashMap<Scope, HashSet<String>>> = RefCell::new(HashMap::new());
}

/// Open a mapping pass for `scope`: forget what the previous pass touched.
pub(super) fn begin_pass(scope: &Scope) {
    TOUCHED.with_borrow_mut(|touched| {
        touched.entry(scope.clone()).or_default().clear();
    });
}

/// Close a mapping pass for `scope`, dropping the cached state of every shader
/// node that is no longer in the tree.
///
/// Every monitor's pass walks the same tree and touches the same ids, so
/// sweeping per pass is correct rather than fighting the second monitor — the
/// same argument `preem_render::end_pass` makes for its instances.
pub(super) fn end_pass(scope: &Scope) {
    let touched = TOUCHED.with_borrow(|all| all.get(scope).cloned().unwrap_or_default());
    STATES.with_borrow_mut(|states| {
        if let Some(per_scope) = states.get_mut(scope) {
            per_scope.retain(|id, _| touched.contains(id));
            if per_scope.is_empty() {
                states.remove(scope);
            }
        }
    });
    if touched.is_empty() {
        TOUCHED.with_borrow_mut(|all| all.remove(scope));
    }
}

/// Drop everything cached for `scope` — a plugin left its region, or a drawer
/// panel closed. Called beside `preem_render::forget_scope`.
pub(super) fn forget_scope(scope: &Scope) {
    STATES.with_borrow_mut(|states| states.remove(scope));
    TOUCHED.with_borrow_mut(|touched| touched.remove(scope));
}

/// How many shader states `scope` currently holds — `0` once swept.
#[cfg(test)]
pub(super) fn cached_states(scope: &Scope) -> usize {
    STATES.with_borrow(|states| states.get(scope).map_or(0, HashMap::len))
}

/// The broken-widget placeholder: an empty surface keeping the node's id and
/// classes, so CSS chrome stays put and a later valid frame updates the same
/// slot rather than rebuilding the tree around it.
///
/// A `Pixels` of `0 × 0` sharing [`preem_render::nothing`]'s one process-wide
/// empty buffer — the same placeholder `wire_map`'s malformed-`Pixels` arm and
/// `preem_render`'s over-cap arm already draw, so a degraded node looks the same
/// whatever degraded it.
/// The tooltip is deliberately **not** carried onto it: `Node::Pixels` has no
/// tooltip field (#957 gave one to the three variants a chip is made of, and a
/// raster buffer was not among them), so a refused shader loses its hover text
/// along with its picture. Stated rather than silently true — the journal line
/// is where a refused node explains itself, and that is the surface that
/// matters here.
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
        Refusal::NoGl => tracing::warn!(
            plugin = scope.plugin_id(),
            tree = ?scope.role(),
            node = ?node.id,
            "no OpenGL context in this session, so this plugin's Shader nodes render the \
             broken-widget placeholder — unlike a preem widget there is no CPU arm to fall \
             back to, by design, and the latch is sticky for the life of this process, so \
             this needs a shell restart rather than a reconnect. hytte-ui has already \
             logged the context failure itself (further occurrences in this tree are \
             silenced)",
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
    let role =
        |ink: Option<kit::Rgba>| ink.map_or(palette.ink, |ink| style.admit_role_ink(ink, None));
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
        Arc, GlAvailability, Grants, MAX_SHADER_DATA_BYTES, MAX_SHADER_SOURCE_BYTES, Refusal,
        ShaderData, ShaderNode, ShaderState, Warned, begin_pass, cached_states, end_pass,
        forget_scope, map_shader, refusal, theme_values,
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
            tooltip: None,
        }
    }

    /// One full mapping pass over `node`, returning the state it produced.
    ///
    /// Goes through `begin_pass` / `map_shader` / `end_pass` rather than calling
    /// `shared_state` directly, so what the sharing tests measure is the path a
    /// monitor's reconcile actually takes — cache lookup, touch bookkeeping and
    /// sweep included.
    fn mapped_state(scope: &Scope, node: &ShaderNode<'_>) -> Arc<ShaderState> {
        begin_pass(scope);
        let mapped = map_shader(scope, granted(), node);
        end_pass(scope);
        match mapped {
            UiNode::Shader { state, .. } => state,
            other => panic!("mapped to {other:?}"),
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
        assert_eq!(
            refusal(Grants::none(), GlAvailability::Available, &node),
            Some(Refusal::NoCapability)
        );
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            None,
            "granted, it draws"
        );

        // Even a node that is *also* malformed is reported as the capability
        // problem: telling a plugin its buffer is short when it may not draw
        // shaders at all sends it after the wrong bug.
        let mut broken = ok_node("void main() {}", &data);
        broken.data_size = (99, 99);
        assert_eq!(
            refusal(Grants::none(), GlAvailability::Available, &broken),
            Some(Refusal::NoCapability)
        );
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
        assert_eq!(
            refusal(
                granted(),
                GlAvailability::Available,
                &ok_node(&at_cap, &data)
            ),
            None
        );

        let over = "x".repeat(MAX_SHADER_SOURCE_BYTES + 1);
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &ok_node(&over, &data)),
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
        assert_eq!(refusal(granted(), GlAvailability::Available, &node), None);

        let over = vec![0u8; MAX_SHADER_DATA_BYTES + 1];
        let mut node = ok_node("void main() {}", &over);
        node.data_size = (u32::try_from(MAX_SHADER_DATA_BYTES + 1).unwrap(), 1);
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
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
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            None,
            "1 Rgba8 texel is 4 bytes"
        );
        node.data_size = (2, 1);
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            Some(Refusal::MalformedData { bytes: 4 }),
            "2 Rgba8 texels want 8",
        );

        let mut node = ok_node("void main() {}", &sixteen);
        node.format = ShaderData::R32f;
        node.data_size = (4, 1);
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            None,
            "4 f32 texels are 16 bytes"
        );
        node.data_size = (4, 2);
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
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
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            Some(Refusal::EmptyGrid)
        );

        let data = [0u8];
        let mut node = ok_node("void main() {}", &data);
        node.width = 0;
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            Some(Refusal::EmptyGrid)
        );
    }

    /// **No GL, no shader** — and, unlike a kit widget, no CPU arm to fall back
    /// to: the widget is GPU-only by design (#893, Annika: "EGL should be
    /// available for all targets"), so a session whose context failed renders
    /// the placeholder rather than mounting a `GtkGLArea` that can never draw.
    ///
    /// Checked **after** the capability so a plugin missing it is still told the
    /// thing it can fix, and **before** the shape checks so a session with no GL
    /// does not report a buffer length nobody was going to read.
    ///
    /// **Falsified** by deleting the `gl == Abandoned` guard: the first
    /// assertion becomes `None` and the node mounts a doomed GL area on every
    /// frame, on every monitor.
    #[test]
    fn an_abandoned_gl_session_refuses_every_shader() {
        let data = [1u8, 2, 3, 4];
        let node = ok_node("void main() { fragColor = u_fg; }", &data);
        assert_eq!(
            refusal(granted(), GlAvailability::Abandoned, &node),
            Some(Refusal::NoGl),
        );
        assert_eq!(
            refusal(granted(), GlAvailability::Available, &node),
            None,
            "with a context it draws",
        );
        assert_eq!(
            refusal(Grants::none(), GlAvailability::Abandoned, &node),
            Some(Refusal::NoCapability),
            "the plugin-fixable refusal still wins",
        );

        // A malformed node in a GL-less session reports the session, not the
        // buffer: there is nothing to read the buffer *with*.
        let mut broken = ok_node("void main() {}", &data);
        broken.data_size = (99, 99);
        assert_eq!(
            refusal(granted(), GlAvailability::Abandoned, &broken),
            Some(Refusal::NoGl),
        );
    }

    /// **Every refusal renders the placeholder**, keeping the node's id and
    /// classes, and each refusal *kind* costs exactly one journal line per tree.
    ///
    /// The second half is what makes the latch load-bearing: a refused node is
    /// refused on every frame and every monitor, so an unlatched warning is one
    /// line per frame per monitor for the life of the shell.
    ///
    /// **Falsified** by returning the real node from [`map_shader`] on a
    /// refusal (the first assertion), or by deleting the `warn(…)` call
    /// entirely (the count drops to 0).
    ///
    /// **What it does not prove, measured rather than assumed:** the *latch* at
    /// this call site. [`preem_render::warnings_for`] counts the times
    /// `warn_once` **claimed** the latch, not the times a line was written, so
    /// replacing `if warn_once(…) { … }` with `let _ = warn_once(…); …` still
    /// reads 1 — verified green. That convention ("every call site is
    /// `if warn_once(scope, what) { tracing::warn!(…) }` and nothing else") is
    /// stated on `warn_once` itself and held by inspection here, exactly as it
    /// is for the five diagnostics that predate this one.
    ///
    /// Reads [`preem_render::warnings_for`] (this scope only), not
    /// [`preem_render::warnings`] (every scope, process-wide) — #974: this
    /// module's tests don't take `tests.rs`'s `PREEM_INK_LOCK`, and at least
    /// two other tests trip the very same [`Warned::ShaderDenied`] slot under
    /// a different scope without that lock either —
    /// `five_refused_frames_emit_one_event` below, and
    /// `plugins::tests::an_ungranted_shader_degrades_without_taking_its_siblings`
    /// (`tests.rs:8151`, scope `wire-shader-denied`); `pump_tests.rs:394` also
    /// maps a shader node, but grants it, so it does not itself claim this
    /// slot. Any of them can be co-scheduled by `cargo test`'s default
    /// parallel harness. A global delta here would see an unrelated claim land
    /// inside this test's before/after window on an unlucky run and read 2,
    /// not 1 (see the PR body for the measured pre-fix failure rate).
    /// `warnings_for` is keyed by `(Scope, Warned)`, the same pair the latch
    /// itself checks, so it cannot see another scope's claim.
    #[test]
    fn a_refused_shader_renders_the_placeholder_and_warns_once() {
        let classes = ["ts-shader".to_owned()];
        let data = [0u8; 4];
        let mut node = ok_node("void main() {}", &data);
        node.classes = &classes;

        let scope = Scope::detached("shader-refusal-placeholder");
        let before = preem_render::warnings_for(&scope, Warned::ShaderDenied);
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
            preem_render::warnings_for(&scope, Warned::ShaderDenied) - before,
            1,
            "five refused frames cost one journal line",
        );
    }

    /// **Regression guard for #974, deterministic rather than schedule-lucky.**
    ///
    /// #971's reviewer saw `a_refused_shader_renders_the_placeholder_and_warns_once`
    /// fail 1 run in 4 under the full `--workspace --features system-tests`
    /// bucket: this module has no `PREEM_INK_LOCK` (that lock lives in
    /// `tests.rs` and only serialises tests in that file), and at least two
    /// other tests trip the very same [`Warned::ShaderDenied`] slot under a
    /// *different* scope without that lock either —
    /// `five_refused_frames_emit_one_event` below, and
    /// `plugins::tests::an_ungranted_shader_degrades_without_taking_its_siblings`
    /// (`tests.rs:8151`, scope `wire-shader-denied`). Whether any of them
    /// collide depends on which OS threads `cargo test`'s harness happens to
    /// schedule them onto and how their timing overlaps — reproduced 0/10 under
    /// a narrow `shader_map` filter and 0/5 under the reviewer's own
    /// `--workspace` shape in this PR's own measurement, which is the flake
    /// working as advertised rather than the bug being absent.
    ///
    /// This test forces the same collision on purpose instead of hoping for it:
    /// a second real OS thread claims `ShaderDenied` under its own scope, and
    /// `std::thread::scope` joins it — guaranteeing that claim has already
    /// happened — before this thread claims its own and reads the delta. That
    /// makes the pre-fix defect reproducible **on every run**, not 1 in 4.
    ///
    /// **Claims the latch directly (`warn_once`), not through [`map_shader`]**
    /// (review of #991, HIGH 1). An earlier draft routed the helper thread
    /// through `map_shader`, which also emits a `tracing::warn!` on the same
    /// `Refusal::NoCapability` callsite as `five_refused_frames_emit_one_event`
    /// below. `tracing-core`'s per-callsite `Interest` cache has a lock-free
    /// fast path used whenever exactly one `Dispatch` has ever been registered
    /// for the process (`Rebuilder::JustOne`), which this binary's one
    /// `with_default` call (in `five_refused_frames_emit_one_event`) always is;
    /// on that path a subscriber-less thread touching the callsite races the
    /// registration uninterlocked and can cache `Interest::never()` for the
    /// *rest of the process*, silencing the other test's counting subscriber.
    /// Measured: 3/40 hermetic + 4/50 xvfb with the `map_shader`-routed guard,
    /// 0/40 and 0/40 with this version and with the guard deleted entirely.
    /// The latch is this guard's actual subject anyway —
    /// `a_refused_shader_renders_the_placeholder_and_warns_once` above already
    /// covers `map_shader`'s call into it — so going straight to `warn_once`
    /// both fixes the collision and narrows the guard to what it claims to
    /// test.
    ///
    /// Both claims are asserted (review of #991, MEDIUM 1): the helper's
    /// `warn_once` call must itself return `true` (a fresh scope's first
    /// claim), or a helper that silently claims nothing — a future edit that,
    /// say, reused an already-claimed scope — would leave this guard green
    /// with #974's bug still present. `thread::scope` propagates a spawned
    /// closure's panic at the join, so the helper's `assert!` fails the test
    /// exactly as the victim's own would.
    ///
    /// The trailing `bystander` read (review of #991, LOW 1) pins the other
    /// half of the claim this guard makes: [`warnings_for`](preem_render::warnings_for)
    /// is keyed by `(Scope, Warned)`, not just by thread. The victim and the
    /// attacker differ in *both* thread and scope, so on their own they cannot
    /// tell a scope-keyed counter apart from a merely-thread-local one; a third
    /// scope, read from *this* thread, that saw either claim would mean the key
    /// dropped `Scope` and counted by thread alone.
    ///
    /// **Falsified** by reading the process-wide
    /// [`preem_render::warnings`] instead of the per-scope
    /// [`preem_render::warnings_for`]: the assertion goes red on every run,
    /// `left: 2, right: 1` — quoted in the PR, restored after.
    #[test]
    fn a_scope_counts_only_its_own_shader_denied_claims_even_when_another_scope_races_it() {
        let victim = Scope::detached("shader-race-victim");
        let before = preem_render::warnings_for(&victim, Warned::ShaderDenied);

        // A second OS thread claims the very same `Warned::ShaderDenied` slot
        // under a different scope, directly through `warn_once` — no
        // `tracing` event, so no shared callsite for it to perturb. `join`s
        // (via `thread::scope`'s implicit join, which also propagates a
        // panicked `assert!`) before this thread reads its own delta, so the
        // other thread's claim is a fact by the time we read `after`, not a
        // maybe.
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let attacker = Scope::detached("shader-race-attacker");
                assert!(
                    preem_render::warn_once(&attacker, Warned::ShaderDenied),
                    "the attacker's scope is fresh — this must be its first claim",
                );
            });
        });

        assert!(
            preem_render::warn_once(&victim, Warned::ShaderDenied),
            "the victim's scope is fresh — this must be its first claim",
        );

        assert_eq!(
            preem_render::warnings_for(&victim, Warned::ShaderDenied) - before,
            1,
            "the victim's own single claim only — not the concurrent attacker \
             scope's claim of the same diagnostic",
        );

        let bystander = Scope::detached("shader-race-bystander");
        assert_eq!(
            preem_render::warnings_for(&bystander, Warned::ShaderDenied),
            0,
            "a third scope, read from this same thread, must see neither the \
             victim's nor the attacker's claim — the key is (Scope, Warned), \
             not thread alone",
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
                tooltip,
            } => {
                assert_eq!(id.as_deref(), Some("spectrum"));
                assert_eq!(tooltip, None, "this fixture sets none");
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

    // ── #968 review fixes ────────────────────────────────────────────────────

    /// **M1.** Two mapping passes over an unchanged node hand back **the same
    /// `Arc`** — which is what makes the sharing claim in `hytte-ui`'s own doc
    /// comments true, and what lets the surface's `Arc::ptr_eq` guards fire at
    /// all.
    ///
    /// `to_ui_node` runs once per monitor per frame. Before this cache, every
    /// pass allocated a fresh `Arc<ShaderState>` plus a fresh copy of the source
    /// *and* the buffer, so the data-texture upload dedup could never fire, the
    /// `set_state` fast path always fell through to a 16 KiB + 4 MiB memcmp, and
    /// the mapping paid the memcpy — per monitor, per frame.
    ///
    /// **Falsified** by returning `Arc::new(build_state(…))` unconditionally
    /// from [`shared_state`]: every assertion below except the last goes red.
    #[test]
    fn two_mapping_passes_over_an_unchanged_node_share_one_state() {
        let data = [1u8, 2, 3, 4];
        let node = ok_node("void main() { fragColor = u_fg; }", &data);
        let scope = Scope::detached("shader-shared-state");
        forget_scope(&scope);

        let first = mapped_state(&scope, &node);
        let second = mapped_state(&scope, &node);
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second monitor's pass must not re-allocate the state",
        );
        assert!(
            Arc::ptr_eq(&first.data, &second.data),
            "…nor the data buffer, which is what the upload dedup compares",
        );
        assert!(
            Arc::ptr_eq(&first.fragment, &second.fragment),
            "…nor the source, which is what the program cache compares",
        );

        // A node that really changed gets a new state — the other half of the
        // rule, and the half a cache that never invalidated would still pass
        // the first three assertions with.
        let moved = [9u8, 9, 9, 9];
        let next = ok_node("void main() { fragColor = u_fg; }", &moved);
        let third = mapped_state(&scope, &next);
        assert!(!Arc::ptr_eq(&first, &third), "new data, new state");
        forget_scope(&scope);
    }

    /// **M1, the payoff.** Driving `hytte-ui`'s own upload rule with the states
    /// this module produces: twenty renders of an unchanged node cost **one**
    /// data-texture upload, not twenty.
    ///
    /// This is the counting probe the sharing exists for. It runs
    /// `needs_upload`'s shipped logic — `Arc::ptr_eq` — against real
    /// `map_shader` output, so it is red both if the cache stops sharing and if
    /// the surface stops deduping.
    ///
    /// **Falsified** either way: neuter [`shared_state`] (20 uploads) or make
    /// `hytte_ui`'s `needs_upload` unconditional (20 uploads).
    #[test]
    fn an_unchanged_node_uploads_its_data_once_across_many_passes() {
        let data = [7u8; 64];
        let node = ok_node("void main() { fragColor = u_bg; }", &data);
        let scope = Scope::detached("shader-upload-count");
        forget_scope(&scope);

        // What the surface holds, and how many times it would touch the GPU.
        let mut held: Option<Arc<[u8]>> = None;
        let mut uploads = 0_u32;
        for _ in 0..20 {
            let state = mapped_state(&scope, &node);
            if hytte::ui::shader_surface::would_upload(held.as_ref(), &state.data) {
                uploads += 1;
                held = Some(Arc::clone(&state.data));
            }
        }
        assert_eq!(uploads, 1, "one upload for twenty passes");

        // …and a real change costs exactly one more.
        let moved = [8u8; 64];
        let next = ok_node("void main() { fragColor = u_bg; }", &moved);
        for _ in 0..5 {
            let state = mapped_state(&scope, &next);
            if hytte::ui::shader_surface::would_upload(held.as_ref(), &state.data) {
                uploads += 1;
                held = Some(Arc::clone(&state.data));
            }
        }
        assert_eq!(uploads, 2, "one more upload for the change, then quiet");
        forget_scope(&scope);
    }

    /// The cache is swept at the pass boundary, so a node that leaves the tree
    /// does not keep its state — and its buffer — alive for the shell's life.
    ///
    /// **Falsified** by dropping the `retain` in [`end_pass`]: the count stays
    /// at 1 after a pass that touched nothing.
    #[test]
    fn a_departed_node_is_swept_at_the_pass_boundary() {
        let data = [1u8, 2, 3, 4];
        let node = ok_node("void main() {}", &data);
        let scope = Scope::detached("shader-sweep");
        forget_scope(&scope);

        begin_pass(&scope);
        let _ = map_shader(&scope, granted(), &node);
        end_pass(&scope);
        assert_eq!(cached_states(&scope), 1, "the mapped node is cached");

        // A pass in which the node is gone.
        begin_pass(&scope);
        end_pass(&scope);
        assert_eq!(cached_states(&scope), 0, "…and swept when it leaves");

        // `forget_scope` is the other release path (a plugin leaving its
        // region, a drawer panel closing).
        begin_pass(&scope);
        let _ = map_shader(&scope, granted(), &node);
        end_pass(&scope);
        assert_eq!(cached_states(&scope), 1);
        forget_scope(&scope);
        assert_eq!(cached_states(&scope), 0, "forget_scope drops everything");
    }

    /// An **anonymous** node is not cached — there is no key to cache it under,
    /// and inventing an ordinal would hand a node its neighbour's state on any
    /// insert. Stated as a test so the cost is a decision rather than a
    /// surprise.
    #[test]
    fn an_anonymous_node_is_not_cached() {
        let data = [1u8, 2, 3, 4];
        let mut node = ok_node("void main() {}", &data);
        node.id = None;
        let scope = Scope::detached("shader-anonymous");
        forget_scope(&scope);

        let first = mapped_state(&scope, &node);
        let second = mapped_state(&scope, &node);
        assert!(
            !Arc::ptr_eq(&first, &second),
            "no id, no key, no sharing — and the docs say so",
        );
        assert_eq!(cached_states(&scope), 0, "and nothing is retained");
        forget_scope(&scope);
    }

    /// **M3.** The tooltip reaches the reconciler node rather than being dropped
    /// on the floor — it was a wire field, an SDK builder method and a pinned
    /// golden byte with no effect at all.
    ///
    /// **Falsified** by restoring `tooltip: _` in `wire_map`'s arm, or by
    /// dropping the field from `map_shader`'s `UiNode::Shader`.
    #[test]
    fn the_tooltip_reaches_the_reconciler_node() {
        let data = [1u8, 2, 3, 4];
        let mut node = ok_node("void main() {}", &data);
        node.tooltip = Some("audio spectrum");
        let scope = Scope::detached("shader-tooltip");
        forget_scope(&scope);

        match map_shader(&scope, granted(), &node) {
            UiNode::Shader { tooltip, .. } => {
                assert_eq!(tooltip.as_deref(), Some("audio spectrum"));
            }
            other => panic!("mapped to {other:?}"),
        }
        forget_scope(&scope);
    }

    /// **L1.** Every `vec4` the published preamble declares is a colour the
    /// theme bag actually sets — the drift the two hardcoded name lists cannot
    /// see between them.
    ///
    /// Supplied by the #968 review. Without it, adding `uniform vec4 u_dim;` to
    /// `SHADER_PREAMBLE` alone stays green in both existing tests *and* compiles
    /// in the lint, and the uniform silently reaches every shader as `vec4(0)`.
    ///
    /// **Falsified** by adding a `vec4` to the preamble without a bag entry, or
    /// by deleting one from `theme_values`.
    #[test]
    fn the_theme_bag_covers_every_vec4_the_preamble_declares() {
        use hytte::ui::shader_surface::SHADER_PREAMBLE;
        let declared: Vec<&str> = SHADER_PREAMBLE
            .lines()
            .filter_map(|l| l.trim().strip_prefix("uniform vec4 "))
            .filter_map(|l| l.strip_suffix(';'))
            .collect();
        let bag: Vec<&str> = theme_values().iter().map(|(n, _)| *n).collect();
        assert!(
            !declared.is_empty(),
            "the parse found nothing — preamble moved?"
        );
        for name in &declared {
            assert!(
                bag.contains(name),
                "the preamble declares `{name}` and nothing sets it"
            );
        }
        assert_eq!(bag.len(), declared.len(), "…and the bag sets nothing extra");
    }

    /// **The latch, counted where it is written.** Five refused frames emit
    /// **one** `tracing` event.
    ///
    /// Supplied by the #968 review, and it closes the gap the PR body documented
    /// honestly: `a_refused_shader_renders_the_placeholder_and_warns_once`
    /// counts latches *claimed* (the counter lives inside `warn_once`), so it
    /// stays green when a call site claims the latch and then logs
    /// unconditionally. This counts events *emitted*, so it does not.
    ///
    /// **Falsified** by replacing `if warn_once(…) { … }` with
    /// `let _ = warn_once(…); …` — verified red, `left: 5, right: 1`.
    #[test]
    fn five_refused_frames_emit_one_event() {
        use std::sync::Arc as StdArc;
        use std::sync::atomic::{AtomicU32, Ordering};

        const TARGET: &str = "trollshell::plugins::shader_map";
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

        let data = [0u8; 4];
        let node = ok_node("void main() {}", &data);
        let scope = Scope::detached("shader-warn-once-events");
        let count = StdArc::new(AtomicU32::new(0));
        tracing::subscriber::with_default(Counting(StdArc::clone(&count)), || {
            for _ in 0..5 {
                let _ = map_shader(&scope, Grants::none(), &node);
            }
        });
        assert_eq!(
            count.load(Ordering::Relaxed),
            1,
            "five refused frames must write one journal line, not five",
        );
    }
}
