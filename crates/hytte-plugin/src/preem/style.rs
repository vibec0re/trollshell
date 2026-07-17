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

use std::sync::atomic::{AtomicU32, Ordering};

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
/// hard-coded per-style default). Called by the SDK runtime; not plugin-facing.
pub(crate) fn set_accent(color: Option<Rgba>) {
    ACCENT.store(pack_accent(color), Ordering::Relaxed);
}

/// The current host accent, or `None` if none was installed this session.
fn accent() -> Option<Rgba> {
    unpack_accent(ACCENT.load(Ordering::Relaxed))
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
}

impl DisplayStyle {
    /// Every style, in the canonical demo-rotation order.
    pub const ALL: [Self; 3] = [Self::Vfd, Self::Lcd, Self::Oled];

    /// The style as a lowercase word (`"vfd"` / `"lcd"` / `"oled"`) — handy
    /// for labels and CSS class suffixes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Vfd => "vfd",
            Self::Lcd => "lcd",
            Self::Oled => "oled",
        }
    }

    /// The style's palette + post-pass parameters, with its **ink** (the lit
    /// color — i.e. the widget's default color) tinted to the host desktop
    /// accent when one was installed this session (#376). `bg`/`ghost`/`bloom`
    /// are the panel's physical character and stay per-style; only the lit ink
    /// follows the accent, which keeps the glow ramp coherent (the bloom
    /// composites toward `ink`). With no accent it is exactly the hard-coded
    /// per-style palette (no regression). An explicit plugin palette
    /// ([`TextBox::colors`](super::TextBox::colors), a hand-built
    /// [`Frame`]) never routes through here, so it always wins.
    pub(crate) fn palette(self) -> Palette {
        self.palette_with_accent(accent())
    }

    /// [`palette`](Self::palette) with the accent passed explicitly — split out
    /// so the default-vs-override resolution is unit-testable without the
    /// process-global.
    fn palette_with_accent(self, accent: Option<Rgba>) -> Palette {
        let mut palette = self.base_palette();
        if let Some(ink) = accent {
            palette.ink = ink;
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
            },
            Self::Lcd => Palette {
                bg: [0xa9, 0xb4, 0x7e, 0xff],
                ink: [0x23, 0x28, 0x1a, 0xff],
                ghost: Some([0x9c, 0xa8, 0x72, 0xff]),
                bloom: None,
            },
            Self::Oled => Palette {
                bg: [0x00, 0x00, 0x00, 0xff],
                ink: [0xe6, 0xf1, 0xff, 0xff],
                ghost: None,
                bloom: Some(Bloom {
                    radius: 1,
                    strength: 120,
                }),
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
}

/// Halo parameters: a `radius` box blur of the lit layer, scaled by
/// `strength`/256 and max-combined under the original intensities.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Bloom {
    pub radius: usize,
    pub strength: u16,
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
    pub(crate) fn composite(&self, frame: &mut Frame, ink: Rgba) {
        for y in 0..self.height {
            for x in 0..self.width {
                let i = self.v[y * self.width + x].min(255);
                if i == 0 {
                    continue;
                }
                let under = frame.at(x, y);
                frame.set(x, y, mix(under, ink, i));
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
    use super::{Bloom, DisplayStyle, Emission, Frame, mix};

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
        e.composite(&mut f, [200, 100, 50, 0xff]);
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
    }

    /// #376: with no host accent, every style keeps its hard-coded ink (no
    /// regression); with an accent, every style's **ink** becomes it while
    /// `bg`/`ghost`/`bloom` stay the per-style panel character. Uses the
    /// explicit-accent seam so it never touches the process-global.
    #[test]
    fn accent_tints_ink_but_leaves_the_panel_character() {
        let accent = [0x9b, 0x59, 0xb6, 0xff];
        for style in DisplayStyle::ALL {
            let base = style.palette_with_accent(None);
            assert_eq!(
                base.ink,
                style.base_palette().ink,
                "{style:?}: no accent keeps the hard-coded ink"
            );

            let tinted = style.palette_with_accent(Some(accent));
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
}
