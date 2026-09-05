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
pub fn set_accent(color: Option<Rgba>) {
    ACCENT.store(pack_accent(color), Ordering::Relaxed);
}

/// The current host accent, or `None` if none was installed this session.
fn accent() -> Option<Rgba> {
    unpack_accent(ACCENT.load(Ordering::Relaxed))
}

// ── per-render ink (#885) ────────────────────────────────────────────────────

/// Which ink a render should use, for a host that decides **per render** rather
/// than per process.
///
/// [`set_accent`] answers the question once for the whole process, which is
/// exactly right inside a plugin — one process hosts one plugin, and every
/// widget in it wants the same session tint. A *shell* drawing the kit is the
/// other case: it renders many plugins' widgets in one process, each of which
/// asked for its own semantic role, and one of which may have pinned an explicit
/// color (`hytte_plugin_proto::preem::StyleRef`). [`with_ink`] is that
/// per-render answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Ink {
    /// The session default: the [`set_accent`] accent if one is installed,
    /// otherwise the skin's own ink. Exactly what a render outside [`with_ink`]
    /// gets, so a caller with nothing to say can name it.
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

thread_local! {
    /// The ink [`with_ink`] is currently scoping, for **this thread**.
    ///
    /// Thread-local and scoped rather than global and sticky, which is the
    /// difference that matters against [`ACCENT`]: a host renders one widget at
    /// a time on its UI thread, so "the ink for the render in progress" is a
    /// well-defined per-thread value, while "the ink for the process" is not
    /// once more than one widget is in play. A thread that never calls
    /// [`with_ink`] — every plugin, and the kit's own tests — sees
    /// [`Ink::Default`] and behaves exactly as it did before this existed.
    static SCOPED_INK: std::cell::Cell<Ink> = const { std::cell::Cell::new(Ink::Default) };
}

/// Restores the enclosing [`SCOPED_INK`] on the way out — including on an
/// unwind, which is why this is a guard and not a pair of `set` calls around the
/// closure.
struct InkGuard(Ink);

impl Drop for InkGuard {
    fn drop(&mut self) {
        SCOPED_INK.set(self.0);
    }
}

/// Render `body` with `ink` as the lit ink of every kit widget it draws,
/// restoring the previous scope afterwards.
///
/// Host-facing, like [`set_accent`]: a plugin author never calls it (the kit
/// widget entry points take a [`DisplayStyle`] and nothing else, deliberately —
/// see the module docs). A **shell** calls it once around each widget's
/// rasterisation to resolve that widget's own semantic role or pinned color.
///
/// Only the lit **ink** moves. The field, the ghost, the bloom and the CRT pass
/// are the panel's physical character and stay per-skin, exactly as they do
/// under [`set_accent`].
///
/// Nesting is well-defined (the inner scope wins for its duration, the outer one
/// resumes), and the scope is per-thread, so a background thread rasterising in
/// parallel is unaffected by — and does not disturb — this one.
pub fn with_ink<T>(ink: Ink, body: impl FnOnce() -> T) -> T {
    let _guard = InkGuard(SCOPED_INK.replace(ink));
    body()
}

/// The ink scope in force on this thread.
fn scoped_ink() -> Ink {
    SCOPED_INK.get()
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
    /// per-style palette (no regression). An explicit plugin palette
    /// ([`TextBox::colors`](super::TextBox::colors), a hand-built
    /// [`Frame`]) never routes through here, so it always wins.
    ///
    /// A host that decides the ink **per render** rather than per session
    /// ([`with_ink`], #885) overrides the accent for the duration of that
    /// render — the same one field, and the same
    /// everything-else-stays-per-skin rule.
    pub(crate) fn palette(self) -> Palette {
        self.palette_with(scoped_ink(), accent())
    }

    /// [`palette`](Self::palette) with **both** of its inputs passed explicitly:
    /// the per-render scope ([`with_ink`]) and the process accent
    /// ([`set_accent`]) — split out so the resolution is unit-testable without
    /// touching either global.
    ///
    /// The precedence is the whole rule, in one place: an explicit per-render
    /// ink beats the session accent, which beats the skin's own — and
    /// [`Ink::Base`] is how a render says "not even the accent". Only
    /// [`Palette::ink`] is ever touched.
    fn palette_with(self, ink: Ink, accent: Option<Rgba>) -> Palette {
        let mut palette = self.base_palette();
        match ink {
            Ink::Default => {
                if let Some(accent) = accent {
                    palette.ink = accent;
                }
            }
            Ink::Base => {}
            Ink::Fixed([r, g, b, _]) => palette.ink = [r, g, b, 0xff],
        }
        palette
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
    use super::{Bloom, DisplayStyle, Emission, Frame, Ink, mix, scoped_ink, with_ink};

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
    /// regression); with an accent, every style's **ink** becomes it while
    /// `bg`/`ghost`/`bloom` stay the per-style panel character. Uses the
    /// explicit seam so it never touches the process-global.
    #[test]
    fn accent_tints_ink_but_leaves_the_panel_character() {
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.palette_with(Ink::Default, None);
            assert_eq!(
                base.ink,
                style.base_palette().ink,
                "{style:?}: no accent keeps the hard-coded ink"
            );

            let tinted = style.palette_with(Ink::Default, Some(accent));
            assert_eq!(tinted.ink, accent, "{style:?}: accent becomes the ink");
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
    /// accent, and [`Ink::Default`] is the accent — i.e. exactly what a render
    /// outside a scope has always got. Pure seam (`palette_with`), so it never
    /// touches the process-global or the thread-local.
    ///
    /// **Falsified** by collapsing `Ink::Base` into `Ink::Default` in
    /// `palette_with`: `Base ignores the accent` goes red.
    #[test]
    fn a_per_render_ink_beats_the_accent_and_base_beats_both() {
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        let pinned = [0x1a, 0xc0, 0x77, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.base_palette().ink;
            assert_eq!(
                style.palette_with(Ink::Default, Some(accent)).ink,
                accent,
                "{style:?}: no scope is the accent"
            );
            assert_eq!(
                style.palette_with(Ink::Default, None).ink,
                base,
                "{style:?}: no scope and no accent is the skin's own ink"
            );
            assert_eq!(
                style.palette_with(Ink::Base, Some(accent)).ink,
                base,
                "{style:?}: Base ignores the accent"
            );
            assert_eq!(
                style.palette_with(Ink::Fixed(pinned), Some(accent)).ink,
                pinned,
                "{style:?}: a pinned ink beats the accent"
            );
            assert_eq!(
                style.palette_with(Ink::Fixed(pinned), None).ink,
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
            let base = style.palette_with(Ink::Default, None);
            let pinned = style.palette_with(Ink::Fixed([0x12, 0x34, 0x56, 0x00]), None);
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
    /// **Falsified** by dropping the `InkGuard` restore (`with_ink` setting the
    /// cell and returning): the post-scope assertion goes red.
    #[test]
    fn with_ink_scopes_and_nests_and_restores() {
        let outer = [0x11, 0x22, 0x33, 0xff];
        let inner = [0x44, 0x55, 0x66, 0xff];
        assert_eq!(scoped_ink(), Ink::Default, "no scope by default");

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
            scoped_ink(),
            Ink::Default,
            "the scope is gone once `with_ink` returns"
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
