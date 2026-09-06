//! Display styles: one palette + post-pass per skin, shared by every kit
//! widget — the styles are **data over one renderer**, never per-style
//! drawing code (#356's design stance).
//!
//! Internally the widgets render in two layers: the *ghost* layer (unlit
//! elements, painted flat into the [`Frame`]) and the *lit* layer — an
//! [`Emission`] intensity grid the widget stamps shapes into. The emission
//! then gets the style's optional [`Bloom`] (a box-blur halo max-combined
//! under the original, so peaks never dim) and is composited toward the
//! palette's ink. That split is what makes VFD phosphor glow, LCD ghost
//! cells, and OLED true-black bloom all fall out of the same code path.
//!
//! [`Crt`](DisplayStyle::Crt) adds one more stage to that same path: an
//! optional [`Mask`] the composite multiplies into the lit layer, so the CRT
//! look is a **pass** every skin inherits rather than a skin of its own (#397).

use std::sync::atomic::{AtomicU32, Ordering};

use super::contrast;
use super::dot_matrix::{DOT, PAD};
use super::frame::{Frame, Rgba};

/// The host-resolved desktop accent (#376), packed `[r, g, b, 0xff]`
/// big-endian into a `u32`; `0` = unset. A resolved accent is always stored
/// opaque, so it can never pack to `0` — that makes `0` a safe "no accent"
/// sentinel. Set once per session by the transport runtime from
/// [`HostMsg::Accent`](hytte_plugin_proto::HostMsg::Accent) and read at render
/// time by [`DisplayStyle::palette`]. It is a process-global because a plugin
/// process hosts exactly one plugin (one `run`), and threading it out-of-band
/// keeps the widget entry points ([`dot_matrix`](super::dot_matrix),
/// [`seven_seg`](super::seven_seg), [`TextBox`](super::TextBox)) signature-free
/// of an accent argument.
static ACCENT: AtomicU32 = AtomicU32::new(0);

/// Pack an optional accent into the [`ACCENT`] sentinel word, forcing alpha
/// opaque (the ink is always drawn opaque). `None` → `0` (unset).
fn pack_accent(color: Option<Rgba>) -> u32 {
    color.map_or(0, |[r, g, b, _]| u32::from_be_bytes([r, g, b, 0xff]))
}

/// Unpack the [`ACCENT`] sentinel word back to an opaque accent, or `None`
/// when unset (`0`).
fn unpack_accent(packed: u32) -> Option<Rgba> {
    (packed != 0).then(|| packed.to_be_bytes())
}

/// Install the host-resolved desktop accent as the kit's default widget tint
/// (#376), or clear it with `None` (older host / resolution failed, keeping the
/// hard-coded per-style default). Called by the SDK runtime (and by any other
/// host embedding the kit); not plugin-facing. `pub` only so the crate root can
/// re-export it — `style` itself is a private module.
///
/// Installing an accent is a *request*, not an assignment: each skin decides
/// how far it can follow one against its own field ([`AccentPolicy`], #928), so
/// a light accent does not erase the reflective LCD's dark ink.
pub fn set_accent(color: Option<Rgba>) {
    ACCENT.store(pack_accent(color), Ordering::Relaxed);
}

/// The current host accent, or `None` if none was installed this session.
fn accent() -> Option<Rgba> {
    unpack_accent(ACCENT.load(Ordering::Relaxed))
}

// ── per-render palette (#885) ────────────────────────────────────────────────

/// Which ink a render should use, for a host that decides **per render** rather
/// than per process.
///
/// [`set_accent`] answers the question once for the whole process, which is
/// exactly right inside a plugin — one process hosts one plugin, and every
/// widget in it wants the same session tint. A *shell* drawing the kit is the
/// other case: it renders many plugins' widgets in one process, each of which
/// asked for its own semantic role, and one of which may have pinned an explicit
/// color (`hytte_plugin_proto::preem::StyleRef`). [`with_pins`] is that
/// per-render answer, and this is its ink half.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Ink {
    /// The session default: the [`set_accent`] accent if one is installed,
    /// otherwise the skin's own ink. Exactly what a render outside [`with_pins`]
    /// gets, so a caller with nothing to say can name it.
    ///
    /// *What* the accent becomes on the way in is the **skin's** call, not the
    /// caller's (#928): a dark-panel skin takes it verbatim, while the
    /// reflective [`Lcd`](DisplayStyle::Lcd) admits it only as far as its light
    /// field can carry it. This is the one ink variant a skin may adjust — see
    /// [`AccentPolicy`].
    #[default]
    Default,
    /// The skin's own hard-coded ink, **ignoring** any installed accent — the
    /// way to hold one widget still while the desktop re-tints around it.
    Base,
    /// This exact color, ignoring any installed accent. Forced opaque on the way
    /// in, like the accent: a kit widget is a screen, and its ink is what a lit
    /// pixel reaches.
    Fixed(Rgba),
}

/// The palette slots a host can pin for the duration of one render — the widened
/// form of [`Ink`] that #885 settled on.
///
/// [`with_ink`] shipped first and pinned only the lit ink, on the reading that
/// the skin *is* the vocabulary for everything else. #884 then measured the two
/// plugins that actually needed a pin (the `pet` and `caw` speech bubbles) and
/// found each sets three colors — a field, an ink and a `.notdef` box — so an
/// ink-only scope could not carry either. Annika settled the widening on #885.
///
/// [`field`](Self::field) is the panel background every widget floods first, so
/// it belongs here, with the ink, where all eight widgets can honor it. A
/// `TextBox`'s third color does **not**: nothing else draws a notdef box, so it
/// is a builder knob on that widget ([`TextBox::notdef`](super::TextBox::notdef))
/// rather than a palette slot.
///
/// What is deliberately *not* pinnable, and stays the skin's on every widget:
/// the ghost, the bloom and the CRT mask. Those are the panel's physical
/// character, and a pinned widget is still meant to read as the same device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Pins {
    /// The lit ink — see [`Ink`].
    pub ink: Ink,
    /// The screen field, or `None` to leave it to the skin. Forced opaque, like
    /// [`Ink::Fixed`]: the field is the opaque ground a widget floods before it
    /// draws anything.
    pub field: Option<Rgba>,
}

impl Pins {
    /// The scope [`with_ink`] opens: one ink, the skin's own field.
    const fn ink(ink: Ink) -> Self {
        Self { ink, field: None }
    }
}

impl From<Ink> for Pins {
    fn from(ink: Ink) -> Self {
        Self::ink(ink)
    }
}

thread_local! {
    /// The palette [`with_pins`] is currently scoping, for **this thread**.
    ///
    /// Thread-local and scoped rather than global and sticky, which is the
    /// difference that matters against [`ACCENT`]: a host renders one widget at
    /// a time on its UI thread, so "the palette for the render in progress" is a
    /// well-defined per-thread value, while "the palette for the process" is not
    /// once more than one widget is in play. A thread that never calls
    /// [`with_pins`] — every plugin, and the kit's own tests — sees
    /// [`Pins::default`] ([`Ink::Default`], no field) and behaves exactly as it
    /// did before this existed.
    static SCOPED_PINS: std::cell::Cell<Pins> =
        const { std::cell::Cell::new(Pins::ink(Ink::Default)) };
}

/// Restores the enclosing [`SCOPED_PINS`] on the way out — including on an
/// unwind, which is why this is a guard and not a pair of `set` calls around the
/// closure.
struct PinGuard(Pins);

impl Drop for PinGuard {
    fn drop(&mut self) {
        SCOPED_PINS.set(self.0);
    }
}

/// Render `body` with `pins` overriding the palette of every kit widget it
/// draws, restoring the previous scope afterwards.
///
/// Host-facing, like [`set_accent`]: a plugin author never calls it (the kit
/// widget entry points take a [`DisplayStyle`] and nothing else, deliberately —
/// see the module docs). A **shell** calls it once around each widget's
/// rasterisation to resolve that widget's own semantic role or pinned colors.
///
/// Only the slots [`Pins`] names move. The ghost, the bloom and the CRT pass are
/// the panel's physical character and stay per-skin, exactly as they do under
/// [`set_accent`].
///
/// Nesting is well-defined (the inner scope wins for its duration, the outer one
/// resumes), and the scope is per-thread, so a background thread rasterising in
/// parallel is unaffected by — and does not disturb — this one.
pub fn with_pins<T>(pins: Pins, body: impl FnOnce() -> T) -> T {
    let _guard = PinGuard(SCOPED_PINS.replace(pins));
    body()
}

/// [`with_pins`] with only the ink named — the pre-#885-widening entry point,
/// kept because it is the honest signature for every caller that pins one color
/// (the SDK's `neutral()`, a role the shell resolved) and because it makes the
/// widening provably additive: it is exactly `with_pins` with no field, so every
/// call site and test written against it renders the same bytes.
pub fn with_ink<T>(ink: Ink, body: impl FnOnce() -> T) -> T {
    with_pins(Pins::ink(ink), body)
}

/// The palette scope in force on this thread.
fn scoped_pins() -> Pins {
    SCOPED_PINS.get()
}

// ── per-skin contrast policy (#928) ─────────────────────────────────────────

/// What a skin does with an ink it did not choose — the session accent, under
/// [`Ink::Default`].
///
/// The accent is the *desktop's* color. It is resolved with no knowledge of
/// which skin it will land on, and it is not resolved again per widget, so
/// whether it can be **read** is a question only the skin is in a position to
/// answer — the skin is the half of the pair that owns the field. That is the
/// whole of #928's design: the skin owns its contrast.
///
/// Three of the four skins are dark panels lit by bright elements, and a
/// desktop accent is a saturated mid-to-light color, so dropping it straight in
/// is exactly right and is the point of #376. The reflective
/// [`Lcd`](DisplayStyle::Lcd) is the one panel *lighter* than its own ink:
/// there a light accent lands on a light field and the text disappears, which
/// is what Annika's live verify on #881 found — *"lcd background skin
/// (greenish) has light tinted foreground so it's not readable"*.
///
/// The policy governs the accent path and **only** the accent path.
/// [`Ink::Base`] is the skin's own ink and needs no guarding; [`Ink::Fixed`] is
/// a color a caller stated on purpose and wins unconditionally, because the
/// kit cannot tell an author's deliberate pin from a color the host resolved
/// for it — both arrive as the same variant — and silently darkening a pin
/// would be the worse failure of the two.
enum AccentPolicy {
    /// Take the accent verbatim, at every luminance. The dark-panel skins:
    /// [`Vfd`](DisplayStyle::Vfd), [`Oled`](DisplayStyle::Oled) and
    /// [`Crt`](DisplayStyle::Crt).
    ///
    /// This is *not* a claim that every color reads on those fields — a
    /// near-black accent on OLED's true black would not — only that the case
    /// #928 reports does not arise there and that #376's behaviour on them is
    /// deliberately untouched. If a dark accent ever needs guarding, it is a
    /// second variant here and nowhere else.
    AsGiven,
    /// Admit the accent only as far as the skin's own field can carry it:
    /// [`admit`] mixes it toward the skin's ink until the WCAG contrast ratio
    /// against that field reaches [`contrast::AA_TEXT`]. The reflective
    /// [`Lcd`](DisplayStyle::Lcd).
    TintToLegible,
}

/// How many stops [`admit`] tries along the accent → skin-ink ramp, inclusive
/// of both ends.
///
/// 65 stops put the mix within `255 / 64 ≈ 4` units of the ideal on each
/// channel — well under a visible step — and cost ~65 luminance evaluations
/// per widget render, against 256 for a stop at every representable `t`.
/// [`palette`](DisplayStyle::palette) is resolved once per widget per frame,
/// never per pixel, so this is noise either way; the coarser ramp is simply the
/// honest amount of work for the precision anyone can see.
const ADMIT_STOPS: u16 = 64;

/// Mix `accent` toward `toward` until it clears `min_ratio` against `field`,
/// and return the first stop that does — forced opaque, like every other
/// palette slot.
///
/// **Total and deterministic.** The ramp's last stop is `toward` itself (each
/// skin's own ink, which its own palette already reads against its own field),
/// so a legible answer always exists; `unwrap_or` is the unreachable arm that
/// keeps the function total anyway, and it degrades to the strongest thing the
/// skin has rather than to the accent.
///
/// The scan is **linear, not a bisection**, and that is load-bearing. Luminance
/// along the ramp is monotonic — every channel moves monotonically, and both
/// the sRGB transfer function and the luminance sum are monotonic in each — but
/// the contrast *ratio* is not: it collapses to 1.0 wherever the ramp crosses
/// the field's own luminance and climbs again past it. A white accent on the
/// LCD's field does exactly that (white is lighter than the field, the skin's
/// ink is far darker), so a bisection would probe into the dip and conclude
/// there is no answer. 65 stops is cheap enough that the honest scan wins.
fn admit(accent: Rgba, toward: Rgba, field: Rgba, min_ratio: f32) -> Rgba {
    let [r, g, b, _] = (0..=ADMIT_STOPS)
        .map(|stop| mix(accent, toward, stop * 255 / ADMIT_STOPS))
        .find(|candidate| contrast::ratio(*candidate, field) >= min_ratio)
        .unwrap_or(toward);
    [r, g, b, 0xff]
}

/// The retro display skin a kit widget renders in. Palettes + post-passes
/// over one shared renderer per widget (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DisplayStyle {
    /// Vacuum fluorescent: pale cyan on near-black, phosphor glow bleeding
    /// off every lit pixel, the barest ghost of unlit elements.
    Vfd,
    /// Reflective LCD: dark ink on an olive field, faint ghost cells behind
    /// the unlit elements, no glow (reflective displays don't bloom).
    ///
    /// The kit's one skin whose **field is lighter than its ink**, which makes
    /// it the only one that has to defend itself against the desktop accent
    /// (#928): its [`AccentPolicy`] is
    /// [`TintToLegible`](AccentPolicy::TintToLegible), so an accent is admitted
    /// as a *darkened* tint rather than replacing the ink outright. A pinned
    /// [`Ink::Fixed`] still wins — pin white here and you get white.
    Lcd,
    /// OLED: white-blue on true black, a tight per-pixel bloom, and **no**
    /// ghosting — an off OLED pixel emits nothing (#354).
    Oled,
    /// Phosphor CRT: P31-green on a near-black tube face, a broad phosphor
    /// bloom, and the raster itself — a scanline comb plus a curved-glass
    /// vignette ([`Mask`]) multiplied into the lit layer at composite time
    /// (#397).
    ///
    /// This is the kit's one **pass**: nothing about it is per-skin, so the
    /// marquee, scope, gauge, LED strip, flip board and static matrix all
    /// render through the tube with no code of their own. Like OLED it has no
    /// ghost — a CRT has no unlit cell structure; the scanlines carry the
    /// hardware-texture role that ghost dots play on VFD.
    Crt,
}

impl DisplayStyle {
    /// Every style, in the canonical demo-rotation order.
    pub const ALL: [Self; 4] = [Self::Vfd, Self::Lcd, Self::Oled, Self::Crt];

    /// The style as a lowercase word (`"vfd"` / `"lcd"` / `"oled"` / `"crt"`)
    /// — handy for labels and CSS class suffixes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vfd => "vfd",
            Self::Lcd => "lcd",
            Self::Oled => "oled",
            Self::Crt => "crt",
        }
    }

    /// The style's palette + post-pass parameters, with its **ink** (the lit
    /// color — i.e. the widget's default color) tinted to the host desktop
    /// accent when one was installed this session (#376).
    /// `bg`/`ghost`/`bloom`/`mask` are the panel's physical character and stay
    /// per-style; only the lit ink follows the accent, which keeps the glow
    /// ramp coherent (the bloom composites toward `ink`, and the mask scales
    /// how far each pixel gets there). With no accent it is exactly the hard-coded
    /// per-style palette (no regression). How far the accent is followed is the
    /// skin's own call ([`AccentPolicy`], #928) — verbatim on the dark panels,
    /// darkened to stay legible on the reflective
    /// [`Lcd`](DisplayStyle::Lcd). An explicit plugin palette
    /// ([`TextBox::colors`](super::TextBox::colors), a hand-built
    /// [`Frame`]) never routes through here, so it always wins.
    ///
    /// A host that decides the palette **per render** rather than per session
    /// ([`with_pins`], #885) overrides the accent for the duration of that
    /// render — the ink, optionally the field, and nothing else. That is the
    /// route a *wire* palette pin takes: it reaches the same two slots the
    /// `colors()` hatch sets, without stepping outside `palette()` the way the
    /// hatch does.
    pub(crate) fn palette(self) -> Palette {
        self.palette_with(scoped_pins(), accent())
    }

    /// [`palette`](Self::palette) with **both** of its inputs passed explicitly:
    /// the per-render scope ([`with_pins`]) and the process accent
    /// ([`set_accent`]) — split out so the resolution is unit-testable without
    /// touching either global.
    ///
    /// The precedence is the whole rule, in one place: an explicit per-render
    /// ink beats the session accent, which beats the skin's own — and
    /// [`Ink::Base`] is how a render says "not even the accent". The field is
    /// orthogonal: nothing else can move it (there is no field accent), so a
    /// pin either replaces it or the skin keeps it. [`Palette::ghost`],
    /// [`Palette::bloom`] and [`Palette::mask`] are never touched.
    ///
    /// The one thing precedence does **not** settle is what the accent looks
    /// like once it arrives, and that is the skin's ([`AccentPolicy`], #928).
    /// The policy is evaluated against the skin's **own** field, before the
    /// field pin below is applied — deliberately, and for two reasons. First it
    /// keeps the two pins orthogonal, exactly as the paragraph above promises:
    /// the resolved ink is a function of `(skin, pins.ink, accent)` and a field
    /// pin cannot move it. Second, a skin's contrast policy is a statement
    /// about the panel it ships, and a host that replaced the ground has left
    /// that panel behind — the ground is now the host's, and so is the job of
    /// keeping an ink on it (which is why #884's two speech bubbles pin both
    /// slots together). The consequence worth knowing: pinning a *dark* field
    /// on the LCD while leaving the ink to the desktop still darkens the
    /// accent, because the skin is answering for its own olive field. Pin the
    /// ink too, or name [`Ink::Base`].
    fn palette_with(self, pins: Pins, accent: Option<Rgba>) -> Palette {
        let mut palette = self.base_palette();
        match pins.ink {
            Ink::Default => {
                if let Some(accent) = accent {
                    palette.ink = match self.accent_policy() {
                        AccentPolicy::AsGiven => accent,
                        // Measured against `palette.bg` while it is still the
                        // **skin's own** field: the `pins.field` arm below has
                        // not run yet, and that ordering is the design, not an
                        // accident — see the rustdoc above.
                        AccentPolicy::TintToLegible => {
                            admit(accent, palette.ink, palette.bg, contrast::AA_TEXT)
                        }
                    };
                }
            }
            Ink::Base => {}
            Ink::Fixed([r, g, b, _]) => palette.ink = [r, g, b, 0xff],
        }
        if let Some([r, g, b, _]) = pins.field {
            palette.bg = [r, g, b, 0xff];
        }
        palette
    }

    /// What this skin does with an ink it did not choose — see
    /// [`AccentPolicy`], which carries the reasoning.
    ///
    /// Matched exhaustively and per variant rather than by a `_` arm, so a
    /// fifth skin cannot be added without stating its answer.
    fn accent_policy(self) -> AccentPolicy {
        match self {
            Self::Vfd | Self::Oled | Self::Crt => AccentPolicy::AsGiven,
            Self::Lcd => AccentPolicy::TintToLegible,
        }
    }

    /// The hard-coded per-style palette — the kit's default look before any
    /// accent tint.
    fn base_palette(self) -> Palette {
        match self {
            Self::Vfd => Palette {
                bg: [0x04, 0x0a, 0x0e, 0xff],
                ink: [0x8d, 0xf5, 0xff, 0xff],
                ghost: Some([0x0c, 0x1a, 0x1f, 0xff]),
                bloom: Some(Bloom {
                    radius: 2,
                    strength: 150,
                }),
                mask: None,
            },
            Self::Lcd => Palette {
                bg: [0xa9, 0xb4, 0x7e, 0xff],
                ink: [0x23, 0x28, 0x1a, 0xff],
                ghost: Some([0x9c, 0xa8, 0x72, 0xff]),
                bloom: None,
                mask: None,
            },
            Self::Oled => Palette {
                bg: [0x00, 0x00, 0x00, 0xff],
                ink: [0xe6, 0xf1, 0xff, 0xff],
                ghost: None,
                bloom: Some(Bloom {
                    radius: 1,
                    strength: 120,
                }),
                mask: None,
            },
            Self::Crt => Palette {
                // The unlit tube face: near-black, but not OLED's *true* black
                // — a phosphor screen with the beam off still reflects the room
                // off its glass, and the residual green cast is the coating's.
                bg: [0x03, 0x07, 0x05, 0xff],
                // P31 (the "G-Y" oscilloscope/terminal phosphor): peak emission
                // around 525 nm with a broad long-wavelength tail, which lands
                // between pure green and spring green once it is squeezed into
                // sRGB — greener and cooler than a P1 terminal green, and
                // nowhere near VFD's cyan. Follows the session accent exactly
                // like every other skin's ink (#376).
                ink: [0x5c, 0xff, 0x82, 0xff],
                // No ghost: a dark CRT pixel is unexcited phosphor, and there
                // is no cell structure behind it to show through. The comb in
                // `mask` is what carries the hardware texture here.
                ghost: None,
                // Wider and stronger than VFD's. VFD light is fluorescence off
                // a flat anode segment a fraction of a millimetre behind the
                // glass, so its halo is tight (radius 2 ≈ half a `DOT` pitch).
                // A CRT's is phosphor grain excited by a beam spot that is
                // itself gaussian, re-scattered through several millimetres of
                // thick faceplate glass: radius 3 ≈ three quarters of a `DOT`,
                // and 190/256 ≈ 0.74 of the blurred energy against VFD's
                // 150/256 ≈ 0.59, because much more of a CRT's apparent
                // brightness *is* the halo.
                bloom: Some(Bloom {
                    radius: 3,
                    strength: 190,
                }),
                mask: Some(Mask::CRT),
            },
        }
    }
}

/// A style's render parameters. All colors are opaque — kit display widgets
/// promise fully opaque frames (they are *screens*, not sprites).
pub(crate) struct Palette {
    /// The screen field every widget floods first.
    pub bg: Rgba,
    /// Lit ink at full intensity; partial intensity mixes toward it.
    pub ink: Rgba,
    /// Unlit elements, painted flat — `None` skips the ghost pass entirely
    /// (the OLED case).
    pub ghost: Option<Rgba>,
    /// The post-pass halo — `None` for glow-free skins (the LCD case).
    pub bloom: Option<Bloom>,
    /// The screen-space attenuation multiplied into the lit layer at
    /// composite time — `None` for every skin but the CRT (#397).
    pub mask: Option<Mask>,
}

/// Halo parameters: a `radius` box blur of the lit layer, scaled by
/// `strength`/256 and max-combined under the original intensities.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bloom {
    pub radius: usize,
    pub strength: u16,
}

// ── The CRT pass (#397) ──────────────────────────────────────────────────────

/// Fixed-point one for a mask factor, in 256ths — the same convention
/// [`Bloom::strength`] uses, so the kit's two post-passes read alike and both
/// stay inside `u32`.
const MASK_ONE: u32 = 256;

/// Fixed-point one for the vignette's normalized screen coordinates. 1024
/// resolves the falloff to ~0.1 % of a screen half-width — far finer than the
/// [`MASK_ONE`] levels the factor is finally quantized to — while keeping
/// `u² + v²` inside 22 bits for any buffer the kit can produce.
const COORD_ONE: i64 = 1024;

/// Corner radius of the curved-glass mask, as a divisor of the buffer's
/// **short** side. Short side rather than long, so a wide flat strip (the
/// 268×36 marquee) gets a corner in proportion to the glass it actually has
/// instead of a radius wider than the strip is tall.
const CORNER_DIV: usize = 6;

/// Width of the vignette's edge ramp, as a divisor of the buffer's short side.
/// 9 puts the ramp at exactly [`PAD`] px on the kit's 36 px dot strips, so
/// there it spends itself inside the bezel and never touches a dot; on the
/// taller surfaces (scope, gauge, boards) it reaches a few rows into the
/// picture, which is where a real tube's edge falloff is actually visible.
const BAND_DIV: usize = 9;

/// The comb's phase is stated relative to buffer row 0, and it only lands in
/// the dot grid's seams because every dot surface puts its grid origin at
/// [`PAD`] and `PAD` is a whole number of [`DOT`]s. If that ever stops holding
/// the comb silently walks onto the dot cores, so pin it here rather than in a
/// test that could be deleted.
const _: () = assert!(PAD.is_multiple_of(DOT));

/// The CRT pass's two screen-space masks, multiplied into the lit layer by
/// [`Emission::composite`].
///
/// Both are **pure functions of `(x, y, width, height)`** — no state, no
/// clock. Temporal phosphor persistence is deliberately not here: the
/// [`Scope`](super::Scope) owns its own decay, and a stateful shared pass
/// would make every widget's render depend on how often it happened to be
/// called.
///
/// They attenuate the *lit* layer only, never the field: an unlit CRT shows
/// neither scanlines nor a vignette, which is exactly what the hardware does
/// — the raster is a property of the beam, not of the glass.
///
/// # Why no barrel distortion
///
/// Curvature reads here as a **vignette**, not as a warp. Resampling the frame
/// would trade the kit's pixel-exactness — the fixed-grid discipline of
/// #839/#843/#846, where nothing is ever sampled off its dot — for a gimmick,
/// and would put a blur under every glyph edge in the kit. Both masks are
/// per-pixel multiplies over the buffer that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mask {
    /// Scanline comb pitch, in emission rows.
    pitch: usize,
    /// Comb phase: a row is dimmed when `y % pitch == phase`.
    phase: usize,
    /// Light kept on a comb row, in 256ths.
    scanline_keep: u32,
    /// Light kept at the screen's extreme corner, in 256ths — the vignette's
    /// depth.
    corner_keep: u32,
}

impl Mask {
    /// The CRT tube.
    ///
    /// **Pitch** is [`DOT`], the kit's virtual pixel: one dark line per dot
    /// row is the coarsest comb that still reads as a raster rather than as a
    /// stripe pattern.
    ///
    /// **Phase** is `DOT - 1`, the last row of each dot cell. The dot falloff
    /// is `[25, 120, 120, 25]` down a cell, so rows `1` and `2` are the bright
    /// core and rows `0` and `DOT - 1` are the dim rim either side of the seam
    /// between two dot rows: the comb has to sit on one of those two, and the
    /// falloff is vertically symmetric, so they are equivalent optically.
    /// `DOT - 1` is taken because it reads as the dark gap *below* each
    /// illuminated line, which is where a raster's retrace sits.
    ///
    /// **`scanline_keep` = 150/256 ≈ 0.59**: a 41 % dip, one row in four, so
    /// the comb is unmistakable while the surface loses only
    /// `(3 + 0.586)/4 ≈ 10 %` of its mean light. Blacking the row out entirely
    /// would cost 25 % and read as a shutter, not a raster.
    ///
    /// **`corner_keep` = 115/256 ≈ 0.45**: the far corner of the glass keeps
    /// under half its light. On the 268×36 strips that puts the *ends* of the
    /// ticker at ~0.73 — a clear falloff you read as curvature, without the
    /// last characters becoming unreadable.
    const CRT: Self = Self {
        pitch: DOT,
        phase: DOT - 1,
        scanline_keep: 150,
        corner_keep: 115,
    };

    /// Resolve the pass for one `w`×`h` buffer — everything that depends on
    /// the geometry rather than on the pixel, done once per composite. See
    /// [`MaskCols`] for why the two little tables are worth their allocation.
    fn columns(self, w: usize, h: usize) -> MaskCols {
        let short = w.min(h);
        let band = short / BAND_DIV;
        MaskCols {
            u2: (0..w).map(|x| centered(x, w).pow(2)).collect(),
            ramp: (0..=band)
                .map(|d| {
                    MASK_ONE * u32::try_from(d).unwrap_or(0)
                        / u32::try_from(band.max(1)).unwrap_or(1)
                })
                .collect(),
            w,
            h,
            radius: short / CORNER_DIV,
            band,
            depth: i64::from(MASK_ONE - self.corner_keep),
            pitch: self.pitch,
            phase: self.phase,
            scanline_keep: self.scanline_keep,
        }
    }

    /// The pass's attenuation at (`x`, `y`) on a `w`×`h` buffer, in 256ths
    /// ([`MASK_ONE`] = untouched). The whole mask in one call — the seam the
    /// unit tests drive; the composite goes through
    /// [`columns`](Self::columns) and [`MaskCols::row`] instead, so nothing
    /// per-geometry is recomputed per pixel.
    #[cfg(test)]
    fn keep(self, x: usize, y: usize, w: usize, h: usize) -> u32 {
        self.columns(w, h).row(y).keep(x)
    }
}

/// [`Mask`] resolved for one buffer geometry.
///
/// The two `Vec`s exist to keep the promise that the pass is **O(pixels)
/// multiplies**. Written the obvious way it is O(pixels) *divides* — one to
/// normalize `x` across the screen, one to resolve the edge ramp — and an
/// integer divide is tens of cycles. Both divisors are geometry-constant, so
/// both become a lookup: one table of `w` entries and one of `band + 1`, built
/// once per composite beside the frame buffer's own (much larger) allocation.
///
/// Measured `--release` on the kit's reference geometry, a 268×36 marquee
/// window (the #401/#844 precedent, ~105 µs/frame on VFD):
///
/// | stage                                  | ns/frame |
/// |----------------------------------------|---------:|
/// | mask alone, on a fully lit 228×36 grid |  ~36 000 |
/// | …the same before these two tables      |  ~64 000 |
/// | marquee window, VFD (radius-2 bloom)   | ~108 000 |
/// | marquee window, CRT (radius-3 + pass)  | ~165 000 |
/// | static `dot_matrix`, VFD               | ~156 000 |
/// | static `dot_matrix`, CRT               | ~172 000 |
///
/// So the pass costs ~28 µs of mask plus ~29 µs of wider bloom on the marquee,
/// and only ~16 µs all-in on the static matrix — where the CRT's missing ghost
/// pass pays for most of it. The mask is ~4 ns per lit pixel and touches
/// nothing else: no second sweep of the buffer, no blur beyond the one
/// [`Emission::bloom`] the skin already asked for.
struct MaskCols {
    /// Per column: the vignette's horizontal coordinate, squared.
    u2: Vec<i64>,
    /// The edge ramp resolved: `MASK_ONE * d / band` for `d` in `0..=band`.
    /// Always at least one entry, so an unrampable buffer reads `ramp[0]`.
    ramp: Vec<u32>,
    /// Buffer extents.
    w: usize,
    h: usize,
    /// Rounded-glass corner radius, in px.
    radius: usize,
    /// Edge-ramp width, in px; `0` means the buffer is too small to have an
    /// edge distinct from its middle, and the ramp is skipped.
    band: usize,
    /// Light the vignette takes at the extreme corner, in 256ths.
    depth: i64,
    /// The comb, carried through.
    pitch: usize,
    phase: usize,
    scanline_keep: u32,
}

impl MaskCols {
    /// The row-constant half of the pass at row `y`: the comb factor plus the
    /// vignette's vertical terms, hoisted out of the column loop.
    fn row(&self, y: usize) -> MaskRow<'_> {
        MaskRow {
            cols: self,
            comb: if self.pitch != 0 && y % self.pitch == self.phase {
                self.scanline_keep
            } else {
                MASK_ONE
            },
            v2: centered(y, self.h).pow(2),
            ey: y.min(self.h.saturating_sub(1).saturating_sub(y)),
        }
    }
}

/// One row of a [`MaskCols`], ready to be evaluated per column.
#[derive(Clone, Copy)]
struct MaskRow<'a> {
    cols: &'a MaskCols,
    /// The scanline comb's factor for this row, in 256ths.
    comb: u32,
    /// The vignette's vertical coordinate, squared, in `COORD_ONE²`.
    v2: i64,
    /// Rows from here to the nearer horizontal edge.
    ey: usize,
}

impl MaskRow<'_> {
    /// The pass's attenuation at column `x`, in 256ths. Every division here is
    /// by a power-of-two constant, so it compiles to a shift.
    fn keep(self, x: usize) -> u32 {
        let cols = self.cols;

        // Radial falloff. `u`/`v` are the pixel's centre in normalized screen
        // coordinates (±COORD_ONE at the outer faces), so `r2` is 0 at the
        // middle of the glass and exactly 1 at a corner pixel's outer face.
        // Linear in r² rather than in r: it leaves the middle of the picture
        // visibly flat and puts the falloff where the glass actually curves.
        let r2 = i64::midpoint(cols.u2.get(x).copied().unwrap_or(0), self.v2);
        let radial = (i64::from(MASK_ONE) - (cols.depth * r2) / (COORD_ONE * COORD_ONE))
            .clamp(0, i64::from(MASK_ONE));

        // Curved glass. Distance to the boundary of the rounded rectangle
        // inscribed in the buffer, ramped to black over the outer `band` px —
        // a straight run along the sides, and the corner arc where both insets
        // are inside the radius. This arc is what makes a *corner* darker than
        // an edge beside it, which a border ramp alone cannot do.
        let edge = if cols.band == 0 {
            MASK_ONE
        } else {
            let ex = x.min(cols.w.saturating_sub(1).saturating_sub(x));
            let d = self.rounded_rect_distance(ex).min(cols.band);
            cols.ramp.get(d).copied().unwrap_or(MASK_ONE)
        };

        // Multiplicative, all three: light the glass takes at the rim is not
        // handed back on a scanline, and vice versa.
        let radial = u32::try_from(radial).unwrap_or(MASK_ONE);
        radial * edge / MASK_ONE * self.comb / MASK_ONE
    }

    /// Distance in px from a pixel `ex` columns / [`ey`](Self::ey) rows inside
    /// the buffer to the rounded rectangle's boundary, `0` on or outside it.
    fn rounded_rect_distance(self, ex: usize) -> usize {
        let radius = self.cols.radius;
        if ex < radius && self.ey < radius {
            let (dx, dy) = (as_i64(radius - ex), as_i64(radius - self.ey));
            let inside = as_i64(radius) - (dx * dx + dy * dy).isqrt();
            usize::try_from(inside).unwrap_or(0)
        } else {
            ex.min(self.ey)
        }
    }

    /// Attenuate one emission intensity.
    fn apply(self, x: usize, i: u16) -> u16 {
        let v = u32::from(i) * self.keep(x) / MASK_ONE;
        u16::try_from(v).unwrap_or(255)
    }
}

/// A `usize` as `i64`, saturating — the kit's buffers are a few hundred px on
/// a side, so this never actually saturates; it exists to keep the mask math
/// cast-free of `as`.
fn as_i64(v: usize) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The centred, normalized coordinate of device index `i` on an extent of `n`,
/// in [`COORD_ONE`] units: about `-COORD_ONE` at the buffer's near face,
/// `+COORD_ONE` at the far one, `0` at the exact middle. Doubled internally so
/// the *pixel centre* lands on the axis — an odd extent gets a true middle
/// pixel, an even one straddles, and neither is off by half a pixel.
fn centered(i: usize, n: usize) -> i64 {
    if n == 0 {
        return 0;
    }
    let (i, n) = (as_i64(i), as_i64(n));
    ((2 * i + 1 - n) * COORD_ONE) / n
}

/// Mix `a` toward `b` by `t`/255 (0 ⇒ `a`, 255 ⇒ `b`), channel-wise with
/// rounding. Pure integer math — renders stay bit-deterministic.
pub(crate) fn mix(a: Rgba, b: Rgba, t: u16) -> Rgba {
    let t = u32::from(t.min(255));
    let mut out = [0u8; 4];
    for (o, (&av, &bv)) in out.iter_mut().zip(a.iter().zip(&b)) {
        let v = (u32::from(av) * (255 - t) + u32::from(bv) * t + 127) / 255;
        *o = u8::try_from(v).unwrap_or(u8::MAX);
    }
    out
}

/// The lit layer: a per-pixel intensity grid (`0..=255`) the widgets stamp
/// shapes into, bloom, then composite toward the palette ink.
pub(crate) struct Emission {
    width: usize,
    height: usize,
    v: Vec<u16>,
}

impl Emission {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            v: vec![0u16; width * height],
        }
    }

    /// Add `amount` of light at (`x`, `y`), saturating at 255 and silently
    /// clipping out-of-bounds (same contract as [`Frame::plot`]).
    pub(crate) fn add(&mut self, x: usize, y: usize, amount: u16) {
        if x >= self.width || y >= self.height {
            return;
        }
        let px = &mut self.v[y * self.width + x];
        *px = (*px + amount.min(255)).min(255);
    }

    /// Apply a halo: box-blur the grid by `bloom.radius`, scale by
    /// `bloom.strength`/256, and **max-combine** with the original — lit
    /// pixels never dim, dark neighbors pick up spill.
    pub(crate) fn bloom(&mut self, bloom: Bloom) {
        if bloom.radius == 0 || bloom.strength == 0 || self.width == 0 || self.height == 0 {
            return;
        }
        let blurred = box_blur(&self.v, self.width, self.height, bloom.radius);
        for (px, b) in self.v.iter_mut().zip(&blurred) {
            let halo = (u32::from(*b) * u32::from(bloom.strength) / 256).min(255);
            *px = (*px).max(u16::try_from(halo).unwrap_or(255));
        }
    }

    /// Paint the lit layer onto `frame`: each pixel with intensity `i > 0`
    /// mixes the pixel already there (field or ghost) toward `ink` by
    /// `i`/255.
    ///
    /// `mask` is the style's optional screen-space attenuation (the CRT pass,
    /// #397), folded into this same loop rather than run as a second sweep:
    /// the composite already visits every pixel, so the pass costs a handful
    /// of integer multiplies per **lit** pixel and not one extra allocation or
    /// pass over the buffer. Unlit pixels are skipped before the mask is
    /// consulted, which is why an unlit screen shows no scanlines.
    ///
    /// Every skin routes its lit layer through here, so passing the palette's
    /// mask along is all a skin ever does about the CRT.
    pub(crate) fn composite(&self, frame: &mut Frame, ink: Rgba, mask: Option<Mask>) {
        self.composite_with(frame, mask, |_, _| ink);
    }

    /// [`composite`](Self::composite) with the ink resolved **per pixel** — the
    /// colour axis (#857).
    ///
    /// This is the single generalisation the axis needed. `composite` is not a
    /// second implementation of this loop; it *is* this loop, called with a
    /// closure that ignores its coordinates and returns the one palette ink, so
    /// the single-ink path every other kit widget takes — marquee, scope,
    /// gauge, seven-seg, split-flap, dot-matrix, LED strip — is unchanged down
    /// to the byte.
    ///
    /// That claim is pinned by `tests/single_ink_golden.rs`, whose digests were
    /// captured from the tree *before* this split and are the only thing that
    /// can prove it: asserting here that `composite` and `composite_with` agree
    /// would be a tautology the compiler already enforces, while those digests
    /// are a function of the whole path (ghost pass, stamp, bloom, mask, `mix`
    /// rounding, palette) and move if any part of it changes.
    ///
    /// Because the generalisation lands *here*, on the shared path, per-cell
    /// colour **composes** with everything the path already does rather than
    /// bypassing it: the [`Bloom`] has already been folded into the emission
    /// before this runs, and the CRT [`Mask`]'s comb and vignette still
    /// attenuate every pixel on the way through. A heat-mapped panel is a
    /// heat-mapped panel *with* scanlines, which is the point of keeping
    /// [`ColorMap`](super::ColorMap) off [`DisplayStyle`].
    ///
    /// `ink_at` is consulted only for pixels that survive both the "is it lit"
    /// and "did the mask extinguish it" tests, so an unlit surface costs
    /// nothing extra and the map never runs on a pixel it cannot colour.
    pub(crate) fn composite_with(
        &self,
        frame: &mut Frame,
        mask: Option<Mask>,
        ink_at: impl Fn(usize, usize) -> Rgba,
    ) {
        // Resolve the mask for this geometry once, before either loop.
        let cols = mask.map(|m| m.columns(self.width, self.height));
        for y in 0..self.height {
            // Hoist the row-constant half of the mask out of the column loop.
            let row = cols.as_ref().map(|c| c.row(y));
            for x in 0..self.width {
                let i = self.v[y * self.width + x].min(255);
                if i == 0 {
                    continue;
                }
                let i = match row {
                    Some(r) => r.apply(x, i),
                    None => i,
                };
                if i == 0 {
                    continue;
                }
                let under = frame.at(x, y);
                frame.set(x, y, mix(under, ink_at(x, y), i));
            }
        }
    }
}

/// A separable box blur (horizontal then vertical pass) over a `w`×`h`
/// intensity grid. The window is a constant `2r + 1` even where it clips at
/// an edge, so edges dim slightly — invisible under the padded frames the
/// widgets render, and it keeps the math branch-free.
fn box_blur(src: &[u16], w: usize, h: usize, r: usize) -> Vec<u16> {
    let win = u32::try_from(2 * r + 1).unwrap_or(1);
    let mut tmp = vec![0u16; src.len()];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0u32;
            for xx in x.saturating_sub(r)..=(x + r).min(w - 1) {
                sum += u32::from(src[y * w + xx]);
            }
            tmp[y * w + x] = u16::try_from(sum / win).unwrap_or(u16::MAX);
        }
    }
    let mut out = vec![0u16; src.len()];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0u32;
            for yy in y.saturating_sub(r)..=(y + r).min(h - 1) {
                sum += u32::from(tmp[yy * w + x]);
            }
            out[y * w + x] = u16::try_from(sum / win).unwrap_or(u16::MAX);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Bloom, DisplayStyle, Emission, Frame, Ink, Pins, admit, contrast, mix, scoped_pins,
        with_ink, with_pins,
    };

    #[test]
    fn mix_hits_both_endpoints_exactly() {
        let a = [10, 20, 30, 0xff];
        let b = [200, 150, 100, 0xff];
        assert_eq!(mix(a, b, 0), a);
        assert_eq!(mix(a, b, 255), b);
        // Oversaturated t clamps to the far endpoint.
        assert_eq!(mix(a, b, 999), b);
        // The midpoint rounds, staying between the endpoints per channel.
        let m = mix(a, b, 128);
        for k in 0..4 {
            assert!(m[k] >= a[k].min(b[k]) && m[k] <= a[k].max(b[k]));
        }
    }

    #[test]
    fn emission_add_saturates_and_clips() {
        let mut e = Emission::new(2, 2);
        e.add(0, 0, 200);
        e.add(0, 0, 200);
        assert_eq!(e.v[0], 255, "light saturates at 255");
        e.add(5, 0, 200); // out of bounds: silent
        e.add(0, 9, 200);
        assert_eq!(e.v.iter().filter(|&&v| v > 0).count(), 1);
    }

    /// Bloom spreads light outward but never dims a lit pixel.
    #[test]
    fn bloom_spreads_without_dimming_peaks() {
        let mut e = Emission::new(9, 9);
        e.add(4, 4, 255);
        e.bloom(Bloom {
            radius: 2,
            strength: 200,
        });
        assert_eq!(e.v[4 * 9 + 4], 255, "the peak stays at full intensity");
        assert!(e.v[4 * 9 + 5] > 0, "a neighbor picks up spill");
        assert!(e.v[0] == 0, "far corners stay dark at radius 2");
        assert!(e.v.iter().all(|&v| v <= 255), "intensities stay in range");
    }

    #[test]
    fn composite_mixes_toward_ink_only_where_lit() {
        let mut e = Emission::new(2, 1);
        e.add(1, 0, 255);
        let mut f = Frame::filled(2, 1, [10, 10, 10, 0xff]);
        e.composite(&mut f, [200, 100, 50, 0xff], None);
        assert_eq!(f.get(0, 0), Some([10, 10, 10, 0xff]), "unlit px untouched");
        assert_eq!(f.get(1, 0), Some([200, 100, 50, 0xff]), "full-lit px = ink");
    }

    /// The style contract the widgets rely on: LCD ghosts but never glows,
    /// OLED glows but never ghosts, VFD does both.
    #[test]
    fn palettes_keep_the_ghost_and_glow_promises() {
        let vfd = DisplayStyle::Vfd.palette();
        assert!(vfd.ghost.is_some() && vfd.bloom.is_some());
        let lcd = DisplayStyle::Lcd.palette();
        assert!(lcd.ghost.is_some() && lcd.bloom.is_none());
        let oled = DisplayStyle::Oled.palette();
        assert!(oled.ghost.is_none() && oled.bloom.is_some());
        assert_eq!(oled.bg, [0, 0, 0, 0xff], "OLED black is true black");
        for style in DisplayStyle::ALL {
            let p = style.palette();
            assert_eq!(p.bg[3], 0xff, "{style:?} field is opaque");
            assert_eq!(p.ink[3], 0xff, "{style:?} ink is opaque");
        }
    }

    #[test]
    fn style_names_are_stable() {
        assert_eq!(DisplayStyle::Vfd.name(), "vfd");
        assert_eq!(DisplayStyle::Lcd.name(), "lcd");
        assert_eq!(DisplayStyle::Oled.name(), "oled");
        assert_eq!(DisplayStyle::Crt.name(), "crt");
    }

    /// #376: with no host accent, every style keeps its hard-coded ink (no
    /// regression); with an accent, every style's **ink** follows it while
    /// `bg`/`ghost`/`bloom` stay the per-style panel character. Uses the
    /// explicit seam so it never touches the process-global.
    ///
    /// The accent below is the mid purple #376 has always been tested with, and
    /// it is *not* legible on the LCD's olive field (2.1:1), so since #928 the
    /// two families answer differently: the dark panels take it verbatim, the
    /// LCD darkens it. The expectation is written out per skin rather than
    /// through the policy, so this test cannot agree with a broken policy by
    /// construction.
    #[test]
    fn accent_tints_ink_but_leaves_the_panel_character() {
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.palette_with(Ink::Default.into(), None);
            assert_eq!(
                base.ink,
                style.base_palette().ink,
                "{style:?}: no accent keeps the hard-coded ink"
            );

            let tinted = style.palette_with(Ink::Default.into(), Some(accent));
            match style {
                DisplayStyle::Lcd => {
                    assert_ne!(
                        tinted.ink, accent,
                        "Lcd: a light accent must not land verbatim on a light field (#928)"
                    );
                    assert!(
                        contrast::ratio(tinted.ink, tinted.bg) >= contrast::AA_TEXT,
                        "Lcd: the admitted ink must be legible, got {}",
                        contrast::ratio(tinted.ink, tinted.bg)
                    );
                }
                DisplayStyle::Vfd | DisplayStyle::Oled | DisplayStyle::Crt => assert_eq!(
                    tinted.ink, accent,
                    "{style:?}: a dark panel takes the accent verbatim (#376, unchanged by #928)"
                ),
            }
            assert_eq!(tinted.bg, base.bg, "{style:?}: field is per-style");
            assert_eq!(tinted.ghost, base.ghost, "{style:?}: ghost is per-style");
            assert!(
                tinted.bloom.is_some() == base.bloom.is_some(),
                "{style:?}: bloom presence is per-style"
            );
        }
    }

    /// An explicit accent overrides the ink even when it equals a sentinel-ish
    /// value; the pack/unpack keeps `None` distinct from any opaque color and
    /// forces the stored ink opaque.
    #[test]
    fn accent_pack_round_trips_and_forces_opaque() {
        use super::{pack_accent, unpack_accent};
        assert_eq!(pack_accent(None), 0, "None is the unset sentinel");
        assert_eq!(unpack_accent(0), None);
        // A transparent input still stores opaque (the ink is always drawn opaque).
        assert_eq!(
            unpack_accent(pack_accent(Some([0x12, 0x34, 0x56, 0x00]))),
            Some([0x12, 0x34, 0x56, 0xff])
        );
        // A fully opaque black is a real accent, never confused with "unset".
        assert_eq!(
            unpack_accent(pack_accent(Some([0, 0, 0, 0xff]))),
            Some([0, 0, 0, 0xff])
        );
    }

    // ── per-render ink (#885) ────────────────────────────────────────────────

    /// The precedence rule, stated once over every skin: a per-render
    /// [`Ink::Fixed`] beats the session accent, [`Ink::Base`] refuses even the
    /// accent, and [`Ink::Default`] follows the accent — i.e. exactly what a
    /// render outside a scope has always got. Pure seam (`palette_with`), so it
    /// never touches the process-global or the thread-local.
    ///
    /// Precedence is about which *input* the ink comes from, and #928 did not
    /// move any of it. What #928 added sits underneath the `Ink::Default` arm —
    /// how far the skin follows the accent it was handed — so the one assertion
    /// here that reads the accent back is per-family; every other row is
    /// unchanged, and `Base`/`Fixed` are untouched on all four skins.
    ///
    /// **Falsified** by collapsing `Ink::Base` into `Ink::Default` in
    /// `palette_with`: `Base ignores the accent` goes red.
    #[test]
    fn a_per_render_ink_beats_the_accent_and_base_beats_both() {
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        let pinned = [0x1a, 0xc0, 0x77, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.base_palette().ink;
            let unscoped = style.palette_with(Ink::Default.into(), Some(accent)).ink;
            match style {
                DisplayStyle::Lcd => assert_ne!(
                    unscoped, base,
                    "Lcd: no scope still follows the accent — darkened, not discarded (#928)"
                ),
                DisplayStyle::Vfd | DisplayStyle::Oled | DisplayStyle::Crt => {
                    assert_eq!(unscoped, accent, "{style:?}: no scope is the accent");
                }
            }
            assert_eq!(
                style.palette_with(Ink::Default.into(), None).ink,
                base,
                "{style:?}: no scope and no accent is the skin's own ink"
            );
            assert_eq!(
                style.palette_with(Ink::Base.into(), Some(accent)).ink,
                base,
                "{style:?}: Base ignores the accent"
            );
            assert_eq!(
                style
                    .palette_with(Ink::Fixed(pinned).into(), Some(accent))
                    .ink,
                pinned,
                "{style:?}: a pinned ink beats the accent"
            );
            assert_eq!(
                style.palette_with(Ink::Fixed(pinned).into(), None).ink,
                pinned,
                "{style:?}: …and stands in for one that was never installed"
            );
        }
    }

    /// A per-render ink moves **only** the ink — the field, the ghost, the bloom
    /// and the CRT pass are the panel's physical character, so a pinned widget
    /// still reads as the same device. Alpha is forced opaque, like the accent's.
    ///
    /// **Falsified** by also assigning `palette.bg` in the `Ink::Fixed` arm: the
    /// "field is per-skin" assertion goes red.
    #[test]
    fn a_per_render_ink_moves_the_ink_and_nothing_else() {
        for style in DisplayStyle::ALL {
            let base = style.palette_with(Ink::Default.into(), None);
            let pinned = style.palette_with(Ink::Fixed([0x12, 0x34, 0x56, 0x00]).into(), None);
            assert_eq!(
                pinned.ink,
                [0x12, 0x34, 0x56, 0xff],
                "{style:?}: a translucent pin still draws opaque"
            );
            assert_eq!(pinned.bg, base.bg, "{style:?}: field is per-skin");
            assert_eq!(pinned.ghost, base.ghost, "{style:?}: ghost is per-skin");
            assert_eq!(
                pinned.bloom.is_some(),
                base.bloom.is_some(),
                "{style:?}: bloom is per-skin"
            );
            assert_eq!(
                pinned.mask.is_some(),
                base.mask.is_some(),
                "{style:?}: the CRT pass is per-skin"
            );
        }
    }

    /// [`with_ink`] is a *scope*: it applies to the renders inside it, nests,
    /// and leaves nothing behind. Uses the thread-local directly rather than the
    /// process accent, so it is safe against every other test in this binary.
    ///
    /// **Falsified** by dropping the `PinGuard` restore (`with_pins` setting the
    /// cell and returning): the post-scope assertion goes red.
    #[test]
    fn with_ink_scopes_and_nests_and_restores() {
        let outer = [0x11, 0x22, 0x33, 0xff];
        let inner = [0x44, 0x55, 0x66, 0xff];
        assert_eq!(scoped_pins(), Pins::default(), "no scope by default");

        let (seen_outer, seen_inner) = with_ink(Ink::Fixed(outer), || {
            let seen_outer = DisplayStyle::Vfd.palette().ink;
            let seen_inner = with_ink(Ink::Fixed(inner), || DisplayStyle::Vfd.palette().ink);
            assert_eq!(
                DisplayStyle::Vfd.palette().ink,
                outer,
                "the outer scope resumes when the inner one ends"
            );
            (seen_outer, seen_inner)
        });

        assert_eq!(seen_outer, outer, "a render inside the scope uses its ink");
        assert_eq!(seen_inner, inner, "the inner scope wins while it is open");
        assert_eq!(
            scoped_pins(),
            Pins::default(),
            "the scope is gone once `with_ink` returns"
        );
    }

    // ── the palette widening (#885, settled on #884's two consumers) ─────────

    /// A per-render **field** moves only [`Palette::bg`]: the ink stays whatever
    /// the ink rule said, and the ghost, the bloom and the CRT pass stay the
    /// skin's. Alpha is forced opaque, like the ink's — the field is the ground
    /// a widget floods, and a screen is not a sprite.
    ///
    /// **Falsified** by dropping the `pins.field` arm from `palette_with` (the
    /// "a pinned field becomes the ground" assertion goes red), and — the other
    /// direction — by having that arm also assign `palette.ink`, which turns
    /// "the ink is untouched by a field pin" red.
    #[test]
    fn a_per_render_field_moves_the_ground_and_nothing_else() {
        let field = [0x3a, 0x22, 0x50, 0x00];
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.palette_with(Pins::default(), Some(accent));
            let pinned = style.palette_with(
                Pins {
                    ink: Ink::Default,
                    field: Some(field),
                },
                Some(accent),
            );
            assert_eq!(
                pinned.bg,
                [0x3a, 0x22, 0x50, 0xff],
                "{style:?}: a pinned field becomes the ground, opaque"
            );
            assert_ne!(
                pinned.bg, base.bg,
                "{style:?}: …and it actually moved, or the assertion above is vacuous"
            );
            assert_eq!(
                pinned.ink, base.ink,
                "{style:?}: the ink is untouched by a field pin"
            );
            assert_eq!(pinned.ghost, base.ghost, "{style:?}: ghost is per-skin");
            assert_eq!(
                pinned.bloom.is_some(),
                base.bloom.is_some(),
                "{style:?}: bloom is per-skin"
            );
            assert_eq!(
                pinned.mask.is_some(),
                base.mask.is_some(),
                "{style:?}: the CRT pass is per-skin"
            );
        }
    }

    /// The two pins are independent: naming both moves both, and naming both is
    /// what #884's speech bubbles need (a lilac field under a bright-lilac ink,
    /// neither of which any skin has).
    #[test]
    fn the_two_pins_compose() {
        let field = [0x3a, 0x22, 0x50, 0xff];
        let ink = [0xf0, 0xe0, 0xf8, 0xff];
        let pins = Pins {
            ink: Ink::Fixed(ink),
            field: Some(field),
        };
        for style in DisplayStyle::ALL {
            let p = style.palette_with(pins, Some([0x9b, 0x59, 0xb6, 0xff]));
            assert_eq!(p.bg, field, "{style:?}: the field pin holds");
            assert_eq!(
                p.ink, ink,
                "{style:?}: …beside the ink pin, over the accent"
            );
        }
    }

    /// The widening is **additive**: [`with_ink`] is exactly [`with_pins`] with
    /// no field, so every pre-widening call site (the SDK's raster arm, the
    /// shell's role resolution) resolves to the same palette it always did.
    ///
    /// This test is the **sole** guard on that equivalence — a point worth
    /// stating, because the obvious candidate is not. #912's ten
    /// `palette_with(Ink::…)` assertions above cannot constrain it: both sides
    /// of every one of them goes through `Ink::…​.into()`, so a field smuggled
    /// into [`Pins::ink`] would cancel out and leave them all green.
    ///
    /// Both directions matter, and the second is the one review found unmeasured
    /// at `3b13ce32`:
    ///
    /// 1. `with_ink` must not *add* a field. **Falsified** by giving
    ///    [`Pins::ink`] any field but `None` — the equality goes red on every
    ///    skin whose field that is not.
    /// 2. `with_ink` must not *inherit* one. A scope **replaces**; it does not
    ///    merge with the scope it nests inside. The docs on [`with_pins`] say
    ///    "the inner scope wins for its duration", and before this assertion
    ///    existed, changing `with_ink` to pass `field: scoped_pins().field` was
    ///    invisible to the whole 193-test kit suite. **Falsified** by exactly
    ///    that change.
    ///
    /// Nothing in the tree nests the two today (the shell and the SDK call
    /// `with_pins` exclusively, and `with_ink` survives as the one-slot
    /// spelling), which is precisely why the guarantee needs a test rather than
    /// a call site.
    #[test]
    fn with_ink_is_with_pins_and_no_field() {
        let pinned = [0x1a, 0xc0, 0x77, 0xff];
        let outer_field = [0x3a, 0x22, 0x50, 0xff];
        for style in DisplayStyle::ALL {
            for ink in [Ink::Default, Ink::Base, Ink::Fixed(pinned)] {
                let via_ink = with_ink(ink, || style.palette());
                let via_pins = with_pins(Pins { ink, field: None }, || style.palette());
                assert_eq!(via_ink.ink, via_pins.ink, "{style:?} {ink:?}: ink");
                assert_eq!(via_ink.bg, via_pins.bg, "{style:?} {ink:?}: field");
                assert_eq!(
                    via_ink.bg,
                    style.base_palette().bg,
                    "{style:?} {ink:?}: an ink-only scope leaves the skin's field alone",
                );

                // …and the same inside an enclosing field scope: `with_ink`
                // replaces it with "no field" rather than inheriting it.
                let nested = with_pins(
                    Pins {
                        ink: Ink::Default,
                        field: Some(outer_field),
                    },
                    || {
                        let outer = style.palette().bg;
                        let inner = with_ink(ink, || style.palette().bg);
                        let resumed = style.palette().bg;
                        (outer, inner, resumed)
                    },
                );
                assert_eq!(
                    nested.0, outer_field,
                    "{style:?} {ink:?}: the enclosing scope's field is in force around it",
                );
                assert_eq!(
                    nested.1,
                    style.base_palette().bg,
                    "{style:?} {ink:?}: an inner `with_ink` replaces the field, it does not inherit",
                );
                assert_eq!(
                    nested.2, outer_field,
                    "{style:?} {ink:?}: …and the enclosing field resumes when it returns",
                );
            }
        }
    }

    /// The three pins disagree about the alpha byte, on purpose, and this is
    /// where that is written down as behaviour rather than prose.
    ///
    /// [`Pins::field`] and [`Ink::Fixed`] are **palette** slots and a kit palette
    /// is opaque by construction (a preem widget is a screen, not a sprite), so
    /// both force `0xff`. `TextBox::notdef` is not a palette slot — no palette
    /// carries a notdef — so it writes the quad through unchanged, exactly as
    /// the `colors()` hatch it mirrors always has, and a translucent value
    /// really does punch holes where an uncovered char draws.
    ///
    /// Both arms of the SDK seam take the same path for each slot, so this
    /// asymmetry costs no parity; it is a footgun worth measuring, not a bug.
    /// Review at `3b13ce32` found it undocumented and unmeasured, with
    /// `Rgba`'s own rustdoc claiming the opposite general rule.
    ///
    /// **Falsified** three ways: dropping the `0xff` from `palette_with`'s field
    /// arm or its `Ink::Fixed` arm reds the first two assertions, and forcing
    /// `TextBox::notdef` opaque reds the third.
    #[test]
    fn the_two_palette_pins_force_opaque_and_the_notdef_slot_does_not() {
        let translucent = [0x3a, 0x22, 0x50, 0x00];
        let p = DisplayStyle::Vfd.palette_with(
            Pins {
                ink: Ink::Fixed(translucent),
                field: Some(translucent),
            },
            None,
        );
        assert_eq!(p.bg[3], 0xff, "a pinned field is drawn opaque");
        assert_eq!(p.ink[3], 0xff, "…and so is a pinned ink");

        // The notdef box is the counterexample: an uncovered char draws it, and
        // the alpha it was given is the alpha in the buffer. `TextBox::new`'s
        // own defaults are white-on-black, so both boxes below differ *only* in
        // how their third color was set.
        let uncovered = "\u{1F63A}";
        let by_slot = crate::TextBox::new()
            .cols(4)
            .notdef(translucent)
            .render(uncovered);
        let holes = by_slot
            .data()
            .chunks_exact(4)
            .filter(|px| px[3] == 0x00 && px[..3] == translucent[..3])
            .count();
        assert!(
            holes > 0,
            "a translucent notdef reaches the buffer unchanged — {holes} see-through pixels",
        );

        // …and the same value through the kit's own hatch does the same thing,
        // which is the point: the wire pin mirrors `colors()` rather than the
        // palette. (The first two args are `TextBox::new`'s defaults, so the
        // only difference between the two boxes is the setter.)
        let by_hatch = crate::TextBox::new()
            .cols(4)
            .colors(
                [0x00, 0x00, 0x00, 0xff],
                [0xff, 0xff, 0xff, 0xff],
                translucent,
            )
            .render(uncovered);
        assert_eq!(
            by_slot.data(),
            by_hatch.data(),
            "`notdef()` and `colors()`'s third slot must stay the same slot",
        );
    }

    /// The end-to-end claim for the field, on a real widget: two renders of the
    /// same dot-matrix strip under two pinned fields differ, and the pinned one
    /// floods that exact color. The ink is held at [`Ink::Base`] so the process
    /// accent (which another test in this binary may have installed) cannot be
    /// what the difference measures.
    #[test]
    fn two_pinned_fields_render_differently() {
        let lilac = [0x3a, 0x22, 0x50, 0xff];
        let olive = [0xa9, 0xb4, 0x7e, 0xff];
        let flooded = |field| {
            with_pins(
                Pins {
                    ink: Ink::Base,
                    field: Some(field),
                },
                || dot_matrix("8", DisplayStyle::Oled),
            )
        };
        let (l, o) = (flooded(lilac), flooded(olive));
        assert_ne!(l, o, "two pinned fields must not render the same");
        assert!(
            l.data().chunks_exact(4).any(|px| px == lilac),
            "an unlit pixel carries the pinned field exactly, not something derived from it"
        );
    }

    /// The end-to-end claim the shell rests on: two renders of the *same* widget
    /// under two different pinned inks produce different pixels, and the pinned
    /// one carries that exact color on a fully-lit dot — the same shape as
    /// `the_accent_reaches_the_shells_own_preem_surfaces` in the shell, but here
    /// with no process-global involved.
    #[test]
    fn two_pinned_inks_render_differently() {
        let teal = [0x11, 0x99, 0xaa, 0xff];
        let rose = [0xdd, 0x22, 0x66, 0xff];
        let lit = |ink| with_ink(Ink::Fixed(ink), || dot_matrix("8", DisplayStyle::Oled));
        let (t, r) = (lit(teal), lit(rose));
        assert_ne!(t, r, "two pinned inks must not render the same");
        assert!(
            t.data().chunks_exact(4).any(|px| px == teal),
            "a fully-lit dot carries the pinned ink exactly, not something derived from it"
        );
    }

    // ── the skin's own contrast policy (#928) ────────────────────────────────

    /// The accent sweep the #928 tests share: the named colors the issue calls
    /// out, plus 50 deterministic pseudo-random RGB triples.
    ///
    /// A seeded xorshift rather than a `rand` dependency — the kit has exactly
    /// one dependency and wants no more — and *seeded* rather than
    /// entropy-fed, so a failure here reproduces from the test name alone
    /// instead of once in a hundred CI runs.
    fn accent_sweep() -> Vec<Rgba> {
        let mut sweep = vec![
            // White is literally what Annika saw on glass, and the worst case:
            // no color contrasts less against a light field.
            [0xff, 0xff, 0xff, 0xff],
            [0xd0, 0xd0, 0xd0, 0xff], // light grey
            [0xf7, 0xe6, 0x9c, 0xff], // pastel yellow
            [0x00, 0xff, 0x00, 0xff], // pure green — bright, and near the field's hue
            [0xa9, 0xb4, 0x7e, 0xff], // the LCD's own field: 1:1 against itself
            [0x00, 0x00, 0x00, 0xff], // black — already legible, must pass through
        ];
        let mut state: u32 = 0x928c_0107;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            u8::try_from(state & 0xff).unwrap_or(0)
        };
        sweep.extend((0..50).map(|_| [next(), next(), next(), 0xff]));
        sweep
    }

    /// **#928's guarantee.** On the LCD an unpinned ink clears WCAG AA (4.5:1)
    /// against the LCD's field for *any* accent a desktop can hand it.
    ///
    /// A property over a sweep rather than a table of expected bytes, because
    /// the claim is about every color and not about the six that happen to be
    /// interesting: a policy that special-cased white would pass a table and
    /// fail here on the random tail.
    ///
    /// **Falsified** by reverting the `Ink::Default` arm to `palette.ink =
    /// accent` — `main`'s accent-replaces-ink behaviour, i.e. the bug: red on
    /// the first sweep entry, white, at 2.21:1.
    #[test]
    fn the_lcd_admits_no_accent_it_cannot_carry() {
        let style = DisplayStyle::Lcd;
        let field = style.base_palette().bg;
        for accent in accent_sweep() {
            let p = style.palette_with(Pins::default(), Some(accent));
            let ratio = contrast::ratio(p.ink, p.bg);
            assert!(
                ratio >= contrast::AA_TEXT,
                "accent {accent:?} became ink {:?} on field {:?} at {ratio:.2}:1, below AA",
                p.ink,
                p.bg
            );
            assert_eq!(p.bg, field, "{accent:?}: the field itself never moves");
            assert_eq!(p.ink[3], 0xff, "{accent:?}: an admitted ink is opaque");
        }
    }

    /// Policy **(b)**, not (a): the accent is *darkened*, never discarded.
    ///
    /// This is the assertion that says the ramp earns its keep — hard-coding
    /// the skin's own ink would satisfy the guarantee above and lose every
    /// trace of the desktop's color. Each accent here is one the LCD must
    /// adjust, and the admitted ink is neither endpoint: not the raw accent
    /// (or it would be illegible) and not the skin's own ink (or the tint would
    /// be gone).
    ///
    /// The hue claim is stated as channel *ordering*, not as bytes: a mix
    /// toward a fixed target preserves the sign of every channel difference, so
    /// a blue-dominant accent stays blue-dominant no matter how many stops the
    /// ramp has. Pinning the bytes would pin [`ADMIT_STOPS`](super::ADMIT_STOPS),
    /// which is an implementation detail this test has no business owning.
    ///
    /// **Falsified** by policy (a) — an `Lcd` arm returning `palette.ink`
    /// unchanged for every accent: "the tint survived" goes red on all four.
    #[test]
    fn the_lcd_darkens_an_accent_rather_than_discarding_it() {
        let style = DisplayStyle::Lcd;
        let base = style.base_palette().ink;
        for accent in [
            [0xff, 0xff, 0xff, 0xff], // white
            [0x00, 0x00, 0xff, 0xff], // pure blue
            [0xdc, 0x14, 0x3c, 0xff], // crimson
            [0x00, 0xff, 0x00, 0xff], // pure green
        ] {
            let ink = style.palette_with(Pins::default(), Some(accent)).ink;
            assert_ne!(ink, accent, "{accent:?}: it had to move to become legible");
            assert_ne!(
                ink, base,
                "{accent:?}: the tint survived — this is (b), not (a)"
            );
            for c in 0..3 {
                let (lo, hi) = (accent[c].min(base[c]), accent[c].max(base[c]));
                assert!(
                    (lo..=hi).contains(&ink[c]),
                    "{accent:?}: channel {c} left the accent→ink ramp at {}",
                    ink[c]
                );
            }
        }

        // The dominant channel is the hue's signature, and it survives.
        let dominant = |c: Rgba| {
            (0..3)
                .max_by_key(|&i| c[i])
                .expect("three channels is not an empty range")
        };
        for accent in [
            [0x00, 0x00, 0xff, 0xff],
            [0xdc, 0x14, 0x3c, 0xff],
            [0x00, 0xff, 0x00, 0xff],
        ] {
            let ink = style.palette_with(Pins::default(), Some(accent)).ink;
            assert_eq!(
                dominant(ink),
                dominant(accent),
                "{accent:?} became {ink:?}: the dominant channel is the hue"
            );
        }

        // …and an accent that is already legible is not touched at all: the
        // ramp's first stop is the accent itself.
        let black = [0x00, 0x00, 0x00, 0xff];
        assert_eq!(
            style.palette_with(Pins::default(), Some(black)).ink,
            black,
            "a dark accent already clears the bar and passes straight through"
        );
    }

    /// **#928 changes nothing on the dark-panel skins.** Their
    /// [`AccentPolicy`](super::AccentPolicy) is
    /// [`AsGiven`](super::AccentPolicy::AsGiven), whose arm is the very
    /// assignment `main` made unconditionally, so their palettes are
    /// byte-identical before and after — for *every* accent, which is what a
    /// sweep says and a spot check does not.
    ///
    /// Stated against the accent bytes rather than against a recording of
    /// `main`'s output on purpose: "the accent lands verbatim and nothing else
    /// moves" **is** `main`'s behaviour, exactly and completely — the
    /// pre-#928 arm had no other effect there could be a recording of. The two
    /// other ink variants are asserted in the same loop, since a policy that
    /// leaked into `Base` or `Fixed` would break these skins too.
    ///
    /// **Falsified** by giving any of the three `TintToLegible`: every accent
    /// the LCD would have darkened goes red on that skin.
    #[test]
    fn the_dark_skins_take_every_accent_verbatim_exactly_as_before() {
        let pin = [0x12, 0x34, 0x56, 0xff];
        for style in [DisplayStyle::Vfd, DisplayStyle::Oled, DisplayStyle::Crt] {
            let base = style.base_palette();
            for accent in accent_sweep() {
                let p = style.palette_with(Pins::default(), Some(accent));
                assert_eq!(p.ink, accent, "{style:?}: accent {accent:?} lands verbatim");
                assert_eq!(p.bg, base.bg, "{style:?}: the field is per-skin");
                assert_eq!(p.ghost, base.ghost, "{style:?}: the ghost is per-skin");
                assert_eq!(
                    p.bloom.is_some(),
                    base.bloom.is_some(),
                    "{style:?}: the bloom is per-skin"
                );
                assert_eq!(
                    p.mask.is_some(),
                    base.mask.is_some(),
                    "{style:?}: the CRT pass is per-skin"
                );
                assert_eq!(
                    style.palette_with(Ink::Base.into(), Some(accent)).ink,
                    base.ink,
                    "{style:?}: Base still refuses accent {accent:?}"
                );
                assert_eq!(
                    style.palette_with(Ink::Fixed(pin).into(), Some(accent)).ink,
                    pin,
                    "{style:?}: a pin still beats accent {accent:?}"
                );
            }
        }
    }

    /// The pin is **unconditional** — and that is the contract even when it is
    /// a bad idea. Pin white on the LCD and you get white, at 2.2:1, unreadable;
    /// the consequence on glass belongs to whoever wrote the pin.
    ///
    /// It has to be this way. The kit cannot tell an author's `.ink(…)` from a
    /// color a host resolved for a semantic role — the shell hands both over as
    /// [`Ink::Fixed`] (`preem_render::ink_for`) — so a policy reaching into
    /// `Fixed` would silently rewrite deliberate palettes, #884's two speech
    /// bubbles among them, which pin their ink *and* their field precisely so
    /// the skin stops having an opinion. Guarding a stated color is the worse
    /// of the two failures.
    ///
    /// **Falsified** by routing `Ink::Fixed` through `admit` on the LCD: the
    /// white pin comes back darkened and the equality goes red.
    #[test]
    fn a_pinned_ink_beats_the_lcd_policy_even_when_it_is_illegible() {
        let style = DisplayStyle::Lcd;
        let white = [0xff, 0xff, 0xff, 0xff];
        let p = style.palette_with(Ink::Fixed(white).into(), Some([0x9b, 0x59, 0xb6, 0xff]));
        assert_eq!(p.ink, white, "a pin is a pin, legible or not");
        assert!(
            contrast::ratio(p.ink, p.bg) < contrast::AA_TEXT,
            "…and this pin is deliberately illegible — the control that says the equality \
             above is about the pin winning and not about the color happening to be fine",
        );

        // `Ink::Base` is never adjusted either: the skin's own ink needs no
        // guarding against the skin's own field, which is the premise the ramp
        // terminates on.
        assert_eq!(
            style.palette_with(Ink::Base.into(), Some(white)).ink,
            style.base_palette().ink,
            "Base is the skin's own ink, accent or no accent"
        );
    }

    /// [`admit`](super::admit) is **total**: when nothing on the ramp clears the
    /// bar it returns the target rather than looping, panicking, or handing back
    /// the illegible accent.
    ///
    /// Unreachable through `palette_with` — every skin's own ink clears its own
    /// field, which is what makes the LCD guarantee unconditional — so it is
    /// asserted directly, on a field chosen to defeat the whole ramp. Mid grey
    /// (`0x81`, relative luminance ≈ 0.22) sits *between* white and the LCD's
    /// ink, so both endpoints fail (≈3.89:1 each) and the ratio only dips
    /// further in between, where the ramp crosses the field's own luminance.
    /// That dip is also why the scan is linear rather than a bisection.
    ///
    /// **Falsified** by `unwrap_or(accent)`: the fallback returns white, the
    /// least legible thing on the ramp, instead of the strongest ink the skin
    /// has.
    #[test]
    fn admit_falls_back_to_the_target_when_the_ramp_cannot_clear_the_bar() {
        let accent = [0xff, 0xff, 0xff, 0xff];
        let toward = DisplayStyle::Lcd.base_palette().ink;
        let hopeless = [0x81, 0x81, 0x81, 0xff];
        assert!(
            contrast::ratio(accent, hopeless) < contrast::AA_TEXT
                && contrast::ratio(toward, hopeless) < contrast::AA_TEXT,
            "the premise: neither endpoint of the ramp clears this field"
        );
        assert_eq!(
            admit(accent, toward, hopeless, contrast::AA_TEXT),
            toward,
            "a ramp with no legible stop degrades to the skin's own ink"
        );

        // The ramp's last stop *is* the target — the reason the fallback is
        // unreachable in practice. Asserted through a field the target clears
        // and the accent does not.
        let field = DisplayStyle::Lcd.base_palette().bg;
        assert_eq!(
            admit(toward, toward, field, contrast::AA_TEXT),
            toward,
            "an accent already equal to the skin's ink is returned as itself"
        );
    }

    // ── The CRT pass (#397) ──────────────────────────────────────────────────
    //
    // The pass is falsified against two fair strawmen, both built here as real
    // `Mask` values rather than described in prose:
    //
    // * `UNPHASED` — the same comb, moved onto a dot's bright core instead of
    //   the seam between two dot rows. `the_comb_never_eats_a_glyph_pixel` is
    //   the only test that rejects it; the per-skin comb statistic below
    //   cheerfully passes with it, which is exactly why both tests exist.
    // * `RECTANGULAR` — the vignette reduced to distance-from-the-nearest-edge
    //   (no radial term, no corner arc). Rejected by
    //   `a_corner_is_darker_than_an_edge_at_the_same_inset`; every other
    //   vignette test passes with it, including monotonicity.
    //
    // A third, `COMB_FREE`, is not a strawman but the reference the phase
    // tests difference against: the pass with its comb switched off.

    use super::super::dot_matrix::{dot_matrix, lit_dot};
    use super::super::font;
    use super::super::gauge::Gauge;
    use super::super::led_strip::LedStrip;
    use super::super::marquee::Marquee;
    use super::super::scope::Scope;
    use super::super::seven_seg::seven_seg;
    use super::super::split_flap::{FlipBoard, Mechanism};
    use super::{DOT, MASK_ONE, Mask, PAD, Rgba};

    /// The pass with its comb switched off — the reference the phase tests
    /// difference the real pass against.
    const COMB_FREE: Mask = Mask {
        pitch: DOT,
        phase: DOT - 1,
        scanline_keep: MASK_ONE,
        corner_keep: 115,
    };

    /// **Strawman.** The same comb, one row up: on a dot's bright core rather
    /// than in the seam below it.
    const UNPHASED: Mask = Mask {
        pitch: DOT,
        phase: DOT - 2,
        scanline_keep: 150,
        corner_keep: 115,
    };

    /// **Strawman.** The vignette reduced to its edge ramp — a rounded-rect
    /// band and nothing else, with no radial falloff. It is the shape you get
    /// by reading "vignette" as "darken the border": flat everywhere more than
    /// a band inside the glass, so a 268 px ticker's far end is as bright as
    /// its middle and a corner is no darker than an edge beside it.
    const NO_RADIAL: Mask = Mask {
        pitch: DOT,
        phase: DOT - 1,
        scanline_keep: MASK_ONE,
        corner_keep: MASK_ONE,
    };

    /// Relative luminance, integer and monotone — enough to order two pixels
    /// of the same hue, which is all these tests ask of it.
    fn luma(px: Rgba) -> u32 {
        (u32::from(px[0]) * 77 + u32::from(px[1]) * 150 + u32::from(px[2]) * 29) / 256
    }

    /// A frame of `n` dot-matrix cells' worth of grid, every dot lit with the
    /// kit's own [`lit_dot`] painter — the real dot hardware, so the comb's
    /// phase is checked against the geometry it actually has to miss.
    fn lit_dot_grid(cells: usize) -> (Emission, usize, usize) {
        let w = 2 * PAD + cells * font::GLYPH_W * DOT;
        let h = 2 * PAD + font::GLYPH_H * DOT;
        let mut e = Emission::new(w, h);
        for cell in 0..cells {
            for row in 0..font::GLYPH_H {
                for col in 0..font::GLYPH_W {
                    lit_dot(
                        &mut e,
                        PAD + (cell * font::GLYPH_W + col) * DOT,
                        PAD + row * DOT,
                    );
                }
            }
        }
        (e, w, h)
    }

    /// Composite `e` onto a flat field through `mask` — the seam the whole pass
    /// lives at, with nothing else in the way.
    fn through(e: &Emission, w: usize, h: usize, mask: Option<Mask>) -> Frame {
        let mut f = Frame::filled(w, h, DisplayStyle::Crt.base_palette().bg);
        e.composite(&mut f, DisplayStyle::Crt.base_palette().ink, mask);
        f
    }

    /// **(a) Scanline phase.** Every dot of a real dot grid, composited through
    /// the pass, comes out **bit-identical** to a comb-free pass on all three
    /// of a dot cell's non-seam rows — the two bright core rows included — and
    /// strictly dimmer on the one seam row. The comb lands between the dot
    /// rows, never on them.
    ///
    /// The dot falloff is `[25, 120, 120, 25]` down a cell, so cell-local rows
    /// `1`/`2` are the bright core and `0`/`DOT - 1` are the dim rim either
    /// side of the seam. The pass takes `DOT - 1`; the `UNPHASED` strawman
    /// takes `DOT - 2`, a core row, and this is the test that rejects it.
    #[test]
    fn the_comb_never_eats_a_glyph_pixel() {
        let (e, w, h) = lit_dot_grid(3);
        let reference = through(&e, w, h, Some(COMB_FREE));
        let pass = through(&e, w, h, Some(Mask::CRT));
        let strawman = through(&e, w, h, Some(UNPHASED));

        let bg = DisplayStyle::Crt.base_palette().bg;
        let mut dimmed = 0;
        for row in 0..font::GLYPH_H {
            for local in 0..DOT {
                let y = PAD + row * DOT + local;
                for x in 0..w {
                    if local != DOT - 1 {
                        assert_eq!(
                            pass.at(x, y),
                            reference.at(x, y),
                            "the comb touched dot row {row}'s local row {local} at x={x}"
                        );
                    } else if reference.at(x, y) != bg {
                        assert!(
                            luma(pass.at(x, y)) < luma(reference.at(x, y)),
                            "the seam row {y} is not dimmed at x={x}"
                        );
                        dimmed += 1;
                    }
                }
            }
        }
        assert!(dimmed > 0, "the comb dimmed nothing at all");

        // Falsification: the unphased comb eats a bright core row — the whole
        // difference between a raster and a scratched screen.
        let core_y = PAD + DOT - 2;
        assert!(
            (0..w).any(|x| luma(strawman.at(x, core_y)) < luma(reference.at(x, core_y))),
            "the unphased strawman was supposed to dim a core row"
        );
    }

    /// **(b) Vignette monotonicity.** Attenuation is exactly full at the middle
    /// of the glass and never rises on the way out of it — along the middle
    /// row, the middle column, and the diagonal to each of the four corners,
    /// which land at zero. Measured comb-free, since the comb is deliberately
    /// *not* monotone in `y`.
    #[test]
    fn the_vignette_is_full_at_the_centre_and_only_falls_outward() {
        for (w, h) in [(65, 65), (269, 37), (145, 49)] {
            let keep = |x: usize, y: usize| COMB_FREE.keep(x, y, w, h);
            let (cx, cy) = (w / 2, h / 2);
            assert_eq!(keep(cx, cy), MASK_ONE, "{w}x{h}: the middle is untouched");

            // Straight out to each edge, and diagonally to each corner.
            let walk = |mut path: Vec<(usize, usize)>| {
                path.dedup();
                for pair in path.windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    assert!(
                        keep(b.0, b.1) <= keep(a.0, a.1),
                        "{w}x{h}: {a:?}→{b:?} brightened on the way out"
                    );
                }
                let last = *path.last().unwrap();
                assert!(keep(last.0, last.1) < MASK_ONE, "{w}x{h}: {last:?} is rim");
            };
            walk((0..=cx).rev().map(|x| (x, cy)).collect());
            walk((cx..w).map(|x| (x, cy)).collect());
            walk((0..=cy).rev().map(|y| (cx, y)).collect());
            walk((cy..h).map(|y| (cx, y)).collect());
            for (tx, ty) in [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)] {
                let steps = cx.max(cy).max(1);
                walk(
                    (0..=steps)
                        .map(|s| (lerp(cx, tx, s, steps), lerp(cy, ty, s, steps)))
                        .collect(),
                );
                assert_eq!(keep(tx, ty), 0, "{w}x{h}: the corner ({tx},{ty}) is dark");
            }
        }
    }

    /// `a → b` at step `s` of `steps`, in whole pixels.
    fn lerp(a: usize, b: usize, s: usize, steps: usize) -> usize {
        let [a, b, s, steps] = [a, b, s, steps].map(super::as_i64);
        usize::try_from(a + (b - a) * s / steps).unwrap_or(0)
    }

    /// **(b), falsified.** The curvature is *curved*, in both the ways a
    /// darken-the-border strawman is not: a corner is darker than either edge
    /// beside it at the same inset, and the falloff reaches the middle of the
    /// picture rather than stopping at the rim — the far end of a 268 px
    /// ticker is visibly dimmer than its centre, which is the whole reason the
    /// menu asked for a curvature *vignette*.
    ///
    /// [`NO_RADIAL`] passes every other vignette test in this module,
    /// monotonicity included. These two assertions are the ones that reject it.
    #[test]
    fn the_vignette_curves_rather_than_just_darkening_the_border() {
        let (w, h) = (269, 37);
        let inset = 4;
        let corner = |m: Mask| m.keep(inset, inset, w, h);
        let top_edge = |m: Mask| m.keep(w / 2, inset, w, h);
        let left_edge = |m: Mask| m.keep(inset, h / 2, w, h);
        let far_end = |m: Mask| m.keep(w - 1 - inset, h / 2, w, h);
        let middle = |m: Mask| m.keep(w / 2, h / 2, w, h);

        assert!(
            corner(COMB_FREE) < top_edge(COMB_FREE),
            "corner vs top edge"
        );
        assert!(
            corner(COMB_FREE) < left_edge(COMB_FREE),
            "corner vs left edge"
        );
        assert!(far_end(COMB_FREE) < middle(COMB_FREE), "the ends fall off");

        // Falsification: to a border-darkening strawman these are all the same
        // pixel, because past the band it has nothing left to say.
        assert_eq!(corner(NO_RADIAL), top_edge(NO_RADIAL));
        assert_eq!(corner(NO_RADIAL), left_edge(NO_RADIAL));
        assert_eq!(far_end(NO_RADIAL), middle(NO_RADIAL));
    }

    /// **(c) Determinism / purity.** The mask is a pure function of
    /// `(x, y, width, height)`: evaluating the whole field in a scrambled order
    /// reproduces the row-major table exactly, and every skin renders the same
    /// bytes twice running. Nothing here can carry state or read a clock — a
    /// stateful pass would fail this on the second call.
    #[test]
    fn the_pass_is_pure_and_every_skin_renders_deterministically() {
        let (w, h) = (61, 43);
        let table: Vec<u32> = (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .map(|(x, y)| Mask::CRT.keep(x, y, w, h))
            .collect();
        // A stride coprime with w*h visits every cell exactly once, in an order
        // nothing about the implementation could have been tuned for.
        let n = w * h;
        for k in 0..n {
            let i = (k * 977) % n;
            assert_eq!(Mask::CRT.keep(i % w, i / w, w, h), table[i], "cell {i}");
        }

        assert_eq!(
            surfaces(DisplayStyle::Crt),
            surfaces(DisplayStyle::Crt),
            "every kit surface renders the same bytes twice running"
        );
    }

    /// **(d) Pass, not skin.** The mask only ever takes light away from what a
    /// skin already stamped: it never lights a pixel the unmasked composite
    /// left alone, and every pixel it touches lands between the field and the
    /// unmasked value. Whatever the pass does to a skin, the skin's own lit
    /// geometry is what it does it *to*.
    #[test]
    fn the_mask_only_attenuates_light_the_skin_already_stamped() {
        let (emission, width, height) = lit_dot_grid(4);
        let bg = DisplayStyle::Crt.base_palette().bg;
        let bare = through(&emission, width, height, None);
        let masked = through(&emission, width, height, Some(Mask::CRT));
        for y in 0..height {
            for x in 0..width {
                let (unmasked_px, masked_px) = (bare.at(x, y), masked.at(x, y));
                if unmasked_px == bg {
                    assert_eq!(masked_px, bg, "({x},{y}): the mask lit an unlit pixel");
                }
                for ch in 0..4 {
                    let lo = unmasked_px[ch].min(bg[ch]);
                    let hi = unmasked_px[ch].max(bg[ch]);
                    assert!(
                        masked_px[ch] >= lo && masked_px[ch] <= hi,
                        "({x},{y}) ch{ch}: {} is outside [{lo}, {hi}]",
                        masked_px[ch]
                    );
                }
            }
        }
    }

    /// One frame per kit surface, rendered in `style` at 1:1 (no widget-level
    /// upscale), so a device row is a virtual pixel row on every one of them.
    fn surfaces(style: DisplayStyle) -> Vec<(&'static str, Frame)> {
        let mut scope = Scope::with_size(144, 48).scale(1);
        scope.advance(&[0.0, 0.55, 0.9, 0.3, -0.4, -0.85, -0.2, 0.6]);
        let mut gauge = Gauge::with_size(144, 64).scale(1);
        gauge.set_target(0.72);
        gauge.settle();
        let mut flap = FlipBoard::new(Mechanism::SplitFlap).cells(4).scale(1);
        flap.set_text("1234");
        flap.settle();
        let mut nixie = FlipBoard::new(Mechanism::Nixie).cells(4).scale(1);
        nixie.set_text("5678");
        nixie.settle();
        vec![
            ("dot_matrix", dot_matrix("PREEM 88", style)),
            ("seven_seg", seven_seg("12:34", style)),
            (
                "marquee",
                Marquee::new(style)
                    .window_px(268)
                    .render("PREEM RASTER KIT ~ CRT ~ ")
                    .window(0),
            ),
            ("led_strip", LedStrip::new(style).render(0.8, 0.95)),
            ("scope", scope.render(style)),
            ("gauge", gauge.render(style)),
            ("split_flap", flap.render(style)),
            ("nixie", nixie.render(style)),
        ]
    }

    /// Light emitted on row `y`: the frame's luminance above the field's, so
    /// the unlit screen contributes nothing and the figure is comparable
    /// between two skins with different fields.
    fn row_light(f: &Frame, bg: Rgba, y: usize) -> u64 {
        (0..f.width())
            .map(|x| u64::from(luma(f.at(x, y)).saturating_sub(luma(bg))))
            .sum()
    }

    /// The comb statistic: light on the comb's rows over light on the rows
    /// bracketing them, as an exact `(numerator, denominator)` pair. A *ratio*,
    /// so it cancels the two skins' different ink brightness and reports only
    /// vertical structure at the comb's pitch and phase — which is what makes
    /// it comparable across skins whose content is otherwise identical.
    fn comb_ratio(f: &Frame, bg: Rgba) -> (u128, u128) {
        let (mut on, mut off, mut n) = (0u128, 0u128, 0u32);
        for y in 1..f.height().saturating_sub(1) {
            if y % DOT != DOT - 1 {
                continue;
            }
            let bracket = u128::from(row_light(f, bg, y - 1) + row_light(f, bg, y + 1));
            if bracket == 0 {
                continue;
            }
            on += 2 * u128::from(row_light(f, bg, y));
            off += bracket;
            n += 1;
        }
        assert!(n >= 3, "only {n} lit comb rows — not enough to measure");
        (on, off)
    }

    /// **(d) Every skin gets the pass, with no skin-side code.** For all eight
    /// kit surfaces, the CRT render carries markedly more structure at the
    /// comb's pitch and phase than the same surface's VFD render does. Same
    /// widget, same content, same geometry — the only thing that changed is the
    /// style, and the comb shows up in every one of them.
    ///
    /// A ratio-of-ratios, so it is blind to the two skins' different ink and
    /// field; and conservative, since the CRT's wider bloom smooths *away*
    /// vertical structure, pushing its ratio back toward 1.
    #[test]
    fn every_skin_gets_the_comb() {
        let crt_bg = DisplayStyle::Crt.palette().bg;
        let vfd_bg = DisplayStyle::Vfd.palette().bg;
        for ((name, crt), (_, vfd)) in surfaces(DisplayStyle::Crt)
            .into_iter()
            .zip(surfaces(DisplayStyle::Vfd))
        {
            let (con, coff) = comb_ratio(&crt, crt_bg);
            let (von, voff) = comb_ratio(&vfd, vfd_bg);
            // crt_ratio < 0.85 * vfd_ratio, cross-multiplied so the comparison
            // stays exact integer arithmetic.
            assert!(
                con * voff * 100 < von * coff * 85,
                "{name}: comb ratio {con}/{coff} is not below 0.85 × {von}/{voff}"
            );
        }
    }

    /// The CRT skin is a phosphor tube: near-black but not OLED-black, no ghost
    /// structure, a halo wider *and* stronger than VFD's fluorescence, and the
    /// pass itself. The `#376` accent contract it shares with every other skin
    /// is covered by `accent_tints_ink_but_leaves_the_panel_character` above,
    /// which walks `ALL`.
    #[test]
    fn the_crt_is_a_phosphor_tube_with_a_pass() {
        let crt = DisplayStyle::Crt.base_palette();
        let vfd = DisplayStyle::Vfd.base_palette();
        assert!(
            crt.ghost.is_none(),
            "a dark CRT pixel has nothing behind it"
        );
        assert!(
            crt.mask.is_some(),
            "the CRT is the skin that carries the pass"
        );
        assert!(
            DisplayStyle::ALL
                .iter()
                .filter(|s| s.palette().mask.is_some())
                .count()
                == 1,
            "the pass rides exactly one skin"
        );
        let (Some(cb), Some(vb)) = (crt.bloom, vfd.bloom) else {
            panic!("both glowing skins bloom");
        };
        assert!(
            cb.radius > vb.radius,
            "phosphor spreads wider than a VFD anode"
        );
        assert!(cb.strength > vb.strength, "and carries more of the light");
        assert!(crt.bg != [0, 0, 0, 0xff], "the tube face is not true black");
        assert!(luma(crt.bg) < 16, "…but it is near enough to black");
        assert!(
            crt.ink[1] > crt.ink[0] && crt.ink[1] > crt.ink[2],
            "P31 is green"
        );
    }

    /// **(e) Exhaustiveness.** The CRT is in the canonical rotation, last —
    /// so `DisplayStyle::ALL`-driven consumers (the demo's skin cycle, every
    /// skin's own `ALL` loop) pick it up with no list to maintain.
    #[test]
    fn the_crt_is_in_the_canonical_rotation() {
        assert_eq!(DisplayStyle::ALL.len(), 4);
        assert_eq!(DisplayStyle::ALL[3], DisplayStyle::Crt);
        assert!(DisplayStyle::ALL.contains(&DisplayStyle::Crt));
    }
}
