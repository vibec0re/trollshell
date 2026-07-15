//! Procedural color-LCD face for caw — a cybercrow head drawn straight into a
//! 128×128 RGBA8 buffer for the [`Node::Pixels`](hytte_plugin::proto::Node::Pixels)
//! node the host upscales nearest-neighbor (the chunky "LCD" look). No sprite
//! assets: every part is hard-edged pixel drawing at native resolution.
//!
//! Modeled on `hytte-plugin-pet`'s cat face, but caw is a corvid: an
//! iridescent-dark head with a feather crest, a jutting beak, and eyes that
//! **glow** on chaos (intensity from her `chaos_level`). The mood → face mapping
//! ([`face_params`]) is an exhaustive match on [`Mood`], so adding a mood in
//! `main.rs` fails to compile here until its face is defined.

use crate::Mood;

/// LCD buffer edge length in device pixels (square).
pub(crate) const SIZE: usize = 128;
/// The same edge as the `u32` the wire's `Pixels { width, height }` wants.
pub(crate) const SIZE_U32: u32 = 128;
/// The same edge as `i32`, for signed geometry math.
const DIM: i32 = 128;
/// Radius of the baked rounded-corner cut (screen glass edge).
const CORNER_R: i32 = 12;

const _: () = assert!(SIZE == 128 && SIZE_U32 == 128 && DIM == 128 && CORNER_R < DIM / 2);

/// An opaque RGB color; alpha is always `0xff` on the screen.
type Rgb = [u8; 3];

// ── Palette — an iridescent cybercrow on a warm dark-purple LCD ─────────────
const SCREEN_BG: Rgb = [0x2a, 0x18, 0x3e]; // warm dark purple (screen field)
const SCREEN_EDGE: Rgb = [0x12, 0x0a, 0x1e]; // baked corner cut = bezel dark
const HEAD: Rgb = [0x34, 0x30, 0x4e]; // dark violet-slate corvid body
const HEAD_HI: Rgb = [0x4e, 0x46, 0x76]; // top-of-head violet sheen
const SHEEN_BLUE: Rgb = [0x3e, 0x58, 0x92]; // blue iridescence streak
const SHEEN_TEAL: Rgb = [0x36, 0x74, 0x68]; // teal iridescence streak
const OUTLINE: Rgb = [0x16, 0x12, 0x26]; // near-black silhouette rim
const BEAK: Rgb = [0x14, 0x11, 0x1e]; // near-black beak
const BEAK_HI: Rgb = [0x50, 0x48, 0x66]; // beak highlight ridge (so it reads)
const BEAK_IN: Rgb = [0x7a, 0x30, 0x54]; // open-beak interior (dark maw)
const EYE_RING: Rgb = [0xf2, 0xe8, 0xd0]; // pale corvid eye ring
const PUPIL: Rgb = [0x0e, 0x0a, 0x16]; // dark pupil
const HIGHLIGHT: Rgb = [0xff, 0xf4, 0xfb]; // eye / sparkle glint
const GLOW: Rgb = [0x66, 0xf2, 0xff]; // electric-cyan chaos eye glow
const GLOW_HOT: Rgb = [0xff, 0x5a, 0xc8]; // hot-magenta chaos accent
const SPARKLE: Rgb = [0xff, 0x92, 0xcb]; // pink sparkle
const HEART: Rgb = [0xff, 0x6f, 0xae]; // pink chirp heart
const ZZZ: Rgb = [0xbc, 0xa6, 0xe8]; // pale sleep z's
const DOT: Rgb = [0xd4, 0xbc, 0xee]; // pale scheming dots
const HUFF: Rgb = [0x8f, 0xc9, 0xff]; // pale-blue offended huff mark

/// Render caw's LCD face for `mood` at animation `frame` with a `0..=255`
/// `intensity` (her `chaos_level` scaled) into a fresh `SIZE`×`SIZE` RGBA8
/// buffer (`SIZE*SIZE*4` bytes exactly — the host's validation invariant).
pub(crate) fn render(mood: Mood, frame: usize, intensity: u8) -> Vec<u8> {
    let mut buf = vec![0u8; SIZE * SIZE * 4];
    let face = face_params(mood, frame);
    fill(&mut buf, SCREEN_BG);
    draw_crest(&mut buf, face.crest, frame);
    draw_head(&mut buf);
    draw_sheen(&mut buf, frame);
    draw_eyes(&mut buf, face.eyes, face.look, face.glow, intensity, frame);
    draw_beak(&mut buf, face.beak);
    draw_extra(&mut buf, face.extra, frame, intensity);
    round_corners(&mut buf);
    buf
}

/// One in ~six frames is a blink; a cheap integer hash jitters the cadence and
/// the step-by-`prev` check keeps two blinks from landing back to back. Pure in
/// `frame` so `view` reads no RNG.
fn is_blink(frame: usize) -> bool {
    fn hash(n: usize) -> usize {
        let mut x = n ^ 0x5bd1_e995;
        x = x.wrapping_mul(0x9e37_79b1);
        x ^= x >> 15;
        x
    }
    hash(frame).is_multiple_of(6) && !hash(frame.wrapping_sub(1)).is_multiple_of(6)
}

// ── Expression model ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum EyeShape {
    Round,
    Wide,
    Half,   // smug half-lidded
    Narrow, // offended + angry brow feathers
    Closed, // sleepy
}

#[derive(Clone, Copy)]
enum BeakShape {
    Closed,
    Open,
}

#[derive(Clone, Copy)]
enum Crest {
    Up,      // alert / chaotic
    Ruffled, // splayed (offended)
    Neat,    // relaxed
    Droop,   // sleepy
}

#[derive(Clone, Copy)]
enum Extra {
    None,
    Glow,        // chaos energy bits (scaled by intensity)
    Sparkle,     // gremlin mischief
    SchemingDots(usize),
    Zzz(usize),
    Huff, // offended
    Chirp,
}

struct Face {
    eyes: EyeShape,
    beak: BeakShape,
    crest: Crest,
    extra: Extra,
    /// Horizontal pupil glance, `-1..=1`, for a little life.
    look: i32,
    /// Whether the eyes carry the chaos glow ring.
    glow: bool,
}

/// Map a [`Mood`] (plus the blink sub-cycle and per-frame liveliness) to the
/// face to draw. Exhaustive over `Mood` on purpose.
fn face_params(mood: Mood, frame: usize) -> Face {
    let blink = is_blink(frame);
    let step = (frame % 3) + 1;
    let eyes_or_blink = |e: EyeShape| if blink { EyeShape::Closed } else { e };
    match mood {
        Mood::Chaos => Face {
            eyes: eyes_or_blink(EyeShape::Round),
            beak: BeakShape::Closed,
            crest: Crest::Up,
            extra: Extra::Glow,
            look: [-1, 0, 1, 0][frame % 4],
            glow: true,
        },
        Mood::Gremlin => Face {
            eyes: eyes_or_blink(EyeShape::Wide),
            beak: BeakShape::Open,
            crest: Crest::Up,
            extra: Extra::Sparkle,
            look: [1, 1, -1, -1][frame % 4],
            glow: false,
        },
        Mood::Smug => Face {
            eyes: EyeShape::Half,
            beak: BeakShape::Closed,
            crest: Crest::Neat,
            extra: Extra::None,
            look: 1, // side-eye
            glow: false,
        },
        Mood::Offended => Face {
            eyes: eyes_or_blink(EyeShape::Narrow),
            beak: BeakShape::Closed,
            crest: Crest::Ruffled,
            extra: Extra::Huff,
            look: 0,
            glow: false,
        },
        Mood::Scheming => Face {
            eyes: eyes_or_blink(EyeShape::Round),
            beak: BeakShape::Closed,
            crest: Crest::Up,
            extra: Extra::SchemingDots(step),
            look: -1, // glancing up-and-away
            glow: false,
        },
        Mood::Sleepy => Face {
            eyes: EyeShape::Closed,
            beak: BeakShape::Closed,
            crest: Crest::Droop,
            extra: Extra::Zzz(step),
            look: 0,
            glow: false,
        },
        Mood::Chirp => Face {
            eyes: eyes_or_blink(EyeShape::Wide),
            beak: BeakShape::Open,
            crest: Crest::Neat,
            extra: Extra::Chirp,
            look: 0,
            glow: false,
        },
    }
}

// ── Drawing primitives (pure buffer functions) ─────────────────────────────

fn plot(buf: &mut [u8], x: i32, y: i32, col: Rgb) {
    let (Ok(px), Ok(py)) = (usize::try_from(x), usize::try_from(y)) else {
        return;
    };
    if px >= SIZE || py >= SIZE {
        return;
    }
    let i = (py * SIZE + px) * 4;
    buf[i..i + 3].copy_from_slice(&col);
    buf[i + 3] = 0xff;
}

fn fill(buf: &mut [u8], col: Rgb) {
    for px in buf.chunks_exact_mut(4) {
        px[0..3].copy_from_slice(&col);
        px[3] = 0xff;
    }
}

fn hline(buf: &mut [u8], x0: i32, x1: i32, y: i32, col: Rgb) {
    for x in x0.min(x1)..=x0.max(x1) {
        plot(buf, x, y, col);
    }
}

fn disc(buf: &mut [u8], cx: i32, cy: i32, r: i32, col: Rgb) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                plot(buf, cx + dx, cy + dy, col);
            }
        }
    }
}

/// A hollow ring of radius `r` (a `w`-px-thick annulus), for glow halos.
fn ring(buf: &mut [u8], cx: i32, cy: i32, r: i32, w: i32, col: Rgb) {
    let inner = (r - w).max(0);
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            if d2 <= r * r && d2 > inner * inner {
                plot(buf, cx + dx, cy + dy, col);
            }
        }
    }
}

fn line(buf: &mut [u8], mut x0: i32, mut y0: i32, x1: i32, y1: i32, col: Rgb) {
    let dx = (x1 - x0).abs();
    let dy = -(y1 - y0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        plot(buf, x0, y0, col);
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

fn stroke(buf: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, col: Rgb) {
    line(buf, x0, y0, x1, y1, col);
    line(buf, x0, y0 + 1, x1, y1 + 1, col);
}

#[allow(clippy::many_single_char_names)]
fn fill_tri(buf: &mut [u8], p0: (i32, i32), p1: (i32, i32), p2: (i32, i32), col: Rgb) {
    let edge =
        |a: (i32, i32), b: (i32, i32), px: i32, py: i32| (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0);
    let min_x = p0.0.min(p1.0).min(p2.0);
    let max_x = p0.0.max(p1.0).max(p2.0);
    let min_y = p0.1.min(p1.1).min(p2.1);
    let max_y = p0.1.max(p1.1).max(p2.1);
    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let w0 = edge(p1, p2, px, py);
            let w1 = edge(p2, p0, px, py);
            let w2 = edge(p0, p1, px, py);
            let inside = (w0 >= 0 && w1 >= 0 && w2 >= 0) || (w0 <= 0 && w1 <= 0 && w2 <= 0);
            if inside {
                plot(buf, px, py, col);
            }
        }
    }
}

/// A shallow 2px arc across `±hw` about (`cx`, `cy`). Positive `depth` bows the
/// middle **down** (a ‿); negative bows it **up** (a ∩).
fn arc(buf: &mut [u8], cx: i32, cy: i32, hw: i32, depth: i32, col: Rgb) {
    if hw == 0 {
        return;
    }
    for dx in -hw..=hw {
        let y = cy + depth - depth * dx * dx / (hw * hw);
        plot(buf, cx + dx, y, col);
        plot(buf, cx + dx, y + 1, col);
    }
}

fn round_corners(buf: &mut [u8]) {
    let last = DIM - 1;
    for py in 0..DIM {
        for px in 0..DIM {
            let dx = if px < CORNER_R {
                CORNER_R - px
            } else if px > last - CORNER_R {
                px - (last - CORNER_R)
            } else {
                0
            };
            let dy = if py < CORNER_R {
                CORNER_R - py
            } else if py > last - CORNER_R {
                py - (last - CORNER_R)
            } else {
                0
            };
            if dx * dx + dy * dy > CORNER_R * CORNER_R {
                plot(buf, px, py, SCREEN_EDGE);
            }
        }
    }
}

/// Move point `p` `n` pixels toward `q` on each axis (a cheap inset).
fn shrink(p: (i32, i32), q: (i32, i32), n: i32) -> (i32, i32) {
    (p.0 + (q.0 - p.0).signum() * n, p.1 + (q.1 - p.1).signum() * n)
}

// ── Corvid parts ────────────────────────────────────────────────────────────

const HEAD_CX: i32 = 64;
const HEAD_CY: i32 = 62;
const HEAD_R: i32 = 36;
const EYE_Y: i32 = 58;
const EYE_LX: i32 = 51;
const EYE_RX: i32 = 77;

fn draw_head(buf: &mut [u8]) {
    disc(buf, HEAD_CX, HEAD_CY, HEAD_R + 2, OUTLINE);
    disc(buf, HEAD_CX, HEAD_CY, HEAD_R, HEAD);
    // A brighter violet cap on the crown for form.
    for dy in -HEAD_R..-(HEAD_R / 3) {
        for dx in -HEAD_R..=HEAD_R {
            if dx * dx + dy * dy <= HEAD_R * HEAD_R && dy * dy > dx * dx / 3 {
                plot(buf, HEAD_CX + dx, HEAD_CY + dy, HEAD_HI);
            }
        }
    }
}

/// Two iridescent sheen streaks that drift across the crown with the frame —
/// the oil-slick blue/teal glint on a corvid's black feathers.
fn draw_sheen(buf: &mut [u8], frame: usize) {
    let d = i32::try_from(frame % 4).unwrap_or(0) - 1;
    stroke(buf, 44 + d, 40, 58 + d, 34, SHEEN_BLUE);
    stroke(buf, 70 - d, 34, 84 - d, 40, SHEEN_TEAL);
    plot(buf, 56 + d, 33, SHEEN_TEAL);
    plot(buf, 72 - d, 33, SHEEN_BLUE);
}

/// The head-feather crest: three tufts on the crown, their splay set by mood.
fn draw_crest(buf: &mut [u8], crest: Crest, frame: usize) {
    let wob = i32::try_from(frame % 2).unwrap_or(0);
    // (base_x, tip offset) per tuft; the tip offset is scaled by the mood.
    let tufts: [(i32, (i32, i32)); 3] = match crest {
        Crest::Up => [(52, (-4, -26)), (64, (0, -32)), (76, (4, -26))],
        Crest::Ruffled => [(48, (-12, -20)), (64, (0, -30)), (80, (12, -20))],
        Crest::Neat => [(54, (-2, -18)), (64, (0, -22)), (74, (2, -18))],
        Crest::Droop => [(54, (-8, -8)), (64, (-2, -12)), (74, (2, -10))],
    };
    for (bx, (tdx, tdy)) in tufts {
        let tip = (bx + tdx, HEAD_CY - HEAD_R + tdy + wob);
        fill_tri(buf, (bx - 6, HEAD_CY - HEAD_R + 6), (bx + 6, HEAD_CY - HEAD_R + 6), tip, OUTLINE);
        fill_tri(
            buf,
            shrink((bx - 6, HEAD_CY - HEAD_R + 6), tip, 2),
            shrink((bx + 6, HEAD_CY - HEAD_R + 6), tip, 2),
            shrink(tip, (bx, HEAD_CY - HEAD_R + 6), 2),
            HEAD_HI,
        );
    }
}

fn draw_eyes(buf: &mut [u8], eyes: EyeShape, look: i32, glow: bool, intensity: u8, frame: usize) {
    // Chaos glow: a pulsing cyan halo behind each eye, brighter with intensity.
    if glow {
        let pulse = i32::from(frame.is_multiple_of(2));
        let r = 8 + pulse + i32::from(intensity > 160);
        let halo = if intensity > 200 { GLOW_HOT } else { GLOW };
        ring(buf, EYE_LX, EYE_Y, r, 2, halo);
        ring(buf, EYE_RX, EYE_Y, r, 2, halo);
    }
    match eyes {
        EyeShape::Round => {
            corvid_eye(buf, EYE_LX, EYE_Y, 6, look, glow);
            corvid_eye(buf, EYE_RX, EYE_Y, 6, look, glow);
        }
        EyeShape::Wide => {
            corvid_eye(buf, EYE_LX, EYE_Y, 8, 0, glow);
            corvid_eye(buf, EYE_RX, EYE_Y, 8, 0, glow);
        }
        EyeShape::Half => {
            half_eye(buf, EYE_LX, look);
            half_eye(buf, EYE_RX, look);
        }
        EyeShape::Narrow => {
            disc(buf, EYE_LX, EYE_Y + 1, 4, EYE_RING);
            disc(buf, EYE_RX, EYE_Y + 1, 4, EYE_RING);
            disc(buf, EYE_LX, EYE_Y + 1, 2, PUPIL);
            disc(buf, EYE_RX, EYE_Y + 1, 2, PUPIL);
            // Angry brow feathers slanting down toward the beak.
            stroke(buf, EYE_LX - 8, EYE_Y - 9, EYE_LX + 6, EYE_Y - 3, OUTLINE);
            stroke(buf, EYE_RX - 6, EYE_Y - 3, EYE_RX + 8, EYE_Y - 9, OUTLINE);
        }
        EyeShape::Closed => {
            arc(buf, EYE_LX, EYE_Y, 6, 3, PUPIL);
            arc(buf, EYE_RX, EYE_Y, 6, 3, PUPIL);
        }
    }
}

/// A half-lidded (smug) eye: a low pupil under a heavy drooping lid.
fn half_eye(buf: &mut [u8], cx: i32, look: i32) {
    disc(buf, cx, EYE_Y + 1, 4, EYE_RING);
    disc(buf, cx + look, EYE_Y + 1, 2, PUPIL);
    // Heavy lid across the top of the eye, slanting for the smug read.
    stroke(buf, cx - 6, EYE_Y - 2, cx + 6, EYE_Y - 3, PUPIL);
}

/// A corvid eye: pale ring, dark pupil, a glint (cyan when glowing).
fn corvid_eye(buf: &mut [u8], cx: i32, cy: i32, r: i32, look: i32, glow: bool) {
    disc(buf, cx, cy, r, EYE_RING);
    disc(buf, cx + look, cy, r - 2, PUPIL);
    let glint = if glow { GLOW } else { HIGHLIGHT };
    disc(buf, cx - 1 + look, cy - 1, 1, glint);
}

fn draw_beak(buf: &mut [u8], beak: BeakShape) {
    let (cx, top) = (HEAD_CX, 70);
    match beak {
        BeakShape::Closed => {
            fill_tri(buf, (cx - 8, top), (cx + 8, top), (cx, top + 20), OUTLINE);
            fill_tri(buf, (cx - 6, top + 1), (cx + 6, top + 1), (cx, top + 18), BEAK);
            // Highlight ridge down the culmen so a near-black beak still reads.
            stroke(buf, cx, top + 2, cx, top + 15, BEAK_HI);
            // The gape line (mandible split).
            hline(buf, cx - 6, cx + 6, top + 6, BEAK_HI);
        }
        BeakShape::Open => {
            // Upper mandible.
            fill_tri(buf, (cx - 8, top), (cx + 8, top), (cx, top + 9), OUTLINE);
            fill_tri(buf, (cx - 6, top + 1), (cx + 6, top + 1), (cx, top + 8), BEAK);
            // Dark maw.
            fill_tri(buf, (cx - 5, top + 8), (cx + 5, top + 8), (cx, top + 14), BEAK_IN);
            // Lower mandible.
            fill_tri(buf, (cx - 6, top + 14), (cx + 6, top + 14), (cx, top + 22), OUTLINE);
            fill_tri(buf, (cx - 4, top + 15), (cx + 4, top + 15), (cx, top + 20), BEAK);
            stroke(buf, cx, top + 2, cx, top + 7, BEAK_HI);
        }
    }
}

fn draw_extra(buf: &mut [u8], extra: Extra, frame: usize, intensity: u8) {
    match extra {
        Extra::None => {}
        Extra::Glow => draw_glow_bits(buf, frame, intensity),
        Extra::Sparkle => draw_sparkles(buf, frame),
        Extra::SchemingDots(n) => draw_dots(buf, n),
        Extra::Zzz(n) => draw_zzz(buf, n),
        Extra::Huff => draw_huff(buf, frame),
        Extra::Chirp => draw_chirp(buf, frame),
    }
}

/// Chaos energy: little cyan/magenta glitch pixels flickering around the head,
/// denser with intensity — the "rogue DHCP broadcasting on UDP 67" static.
fn draw_glow_bits(buf: &mut [u8], frame: usize, intensity: u8) {
    let bits = [(20, 34), (108, 30), (16, 74), (112, 70), (26, 100), (102, 104), (64, 16)];
    let n = 2 + (intensity as usize) * (bits.len() - 2) / 255;
    for (i, &(x, y)) in bits.iter().take(n).enumerate() {
        if frame.wrapping_add(i).is_multiple_of(2) {
            let c = if i % 2 == 0 { GLOW } else { GLOW_HOT };
            plot(buf, x, y, c);
            plot(buf, x + 1, y, c);
            plot(buf, x, y + 1, c);
        }
    }
}

fn draw_sparkles(buf: &mut [u8], frame: usize) {
    let stars = [(24, 38), (104, 42), (98, 96)];
    for (i, &(cx, cy)) in stars.iter().enumerate() {
        let s = 3 + i32::from(frame.wrapping_add(i).is_multiple_of(2)) * 2;
        hline(buf, cx - s, cx + s, cy, SPARKLE);
        for dy in -s..=s {
            plot(buf, cx, cy + dy, SPARKLE);
        }
        plot(buf, cx, cy, HIGHLIGHT);
    }
}

fn draw_dots(buf: &mut [u8], n: usize) {
    let dots = [(96, 26), (106, 26), (116, 26)];
    for &(cx, cy) in dots.iter().take(n) {
        disc(buf, cx, cy, 2, DOT);
    }
}

fn draw_zzz(buf: &mut [u8], n: usize) {
    let zs = [(96, 38, 5), (106, 26, 7), (116, 12, 9)];
    for &(x, y, s) in zs.iter().take(n) {
        hline(buf, x, x + s, y, ZZZ);
        line(buf, x + s, y, x, y + s, ZZZ);
        hline(buf, x, x + s, y + s, ZZZ);
    }
}

/// Two pale-blue huff puffs from the beak — indignant *ruffles feathers*.
fn draw_huff(buf: &mut [u8], frame: usize) {
    let d = i32::try_from(frame % 3).unwrap_or(0);
    disc(buf, 42 - d, 96 + d, 3, HUFF);
    disc(buf, 86 + d, 96 + d, 3, HUFF);
}

/// A little pink heart and a sparkle — chirp / <3.
fn draw_chirp(buf: &mut [u8], frame: usize) {
    let (cx, cy) = (100, 40 - i32::try_from(frame % 2).unwrap_or(0));
    disc(buf, cx - 2, cy, 2, HEART);
    disc(buf, cx + 2, cy, 2, HEART);
    fill_tri(buf, (cx - 4, cy + 1), (cx + 4, cy + 1), (cx, cy + 6), HEART);
    plot(buf, 24, 44, HIGHLIGHT);
    hline(buf, 22, 26, 44, SPARKLE);
    for dy in -2..=2 {
        plot(buf, 24, 44 + dy, SPARKLE);
    }
}

#[cfg(test)]
mod tests {
    use super::{SIZE, is_blink, render};
    use crate::Mood;

    const MOODS: [Mood; 7] = [
        Mood::Chaos,
        Mood::Gremlin,
        Mood::Smug,
        Mood::Offended,
        Mood::Scheming,
        Mood::Sleepy,
        Mood::Chirp,
    ];

    /// The host validates `data.len() == width*height*4` and renders nothing
    /// otherwise — every mood at every frame/intensity MUST fill the buffer.
    #[test]
    fn buffer_is_always_exactly_full() {
        for mood in MOODS {
            for frame in 0..40 {
                for intensity in [0u8, 128, 255] {
                    let buf = render(mood, frame, intensity);
                    assert_eq!(
                        buf.len(),
                        SIZE * SIZE * 4,
                        "{mood:?} frame {frame} i{intensity} must be a full 128x128 RGBA8 buffer"
                    );
                }
            }
        }
    }

    #[test]
    fn every_pixel_is_opaque() {
        let buf = render(Mood::Chaos, 0, 200);
        assert!(buf.chunks_exact(4).all(|px| px[3] == 0xff));
    }

    #[test]
    fn moods_render_differently() {
        let chaos = render(Mood::Chaos, 0, 200);
        let sleepy = render(Mood::Sleepy, 0, 0);
        let offended = render(Mood::Offended, 0, 0);
        assert_ne!(chaos, sleepy);
        assert_ne!(chaos, offended);
        assert_ne!(sleepy, offended);
    }

    #[test]
    fn chaos_intensity_changes_the_glow() {
        // More chaos = more glitch bits, so the buffer differs.
        assert_ne!(
            render(Mood::Chaos, 0, 20),
            render(Mood::Chaos, 0, 255),
            "chaos_level must visibly drive the face"
        );
    }

    #[test]
    fn blink_changes_the_face() {
        let open = (0..50).find(|&f| !is_blink(f)).expect("an open frame");
        let shut = (0..50).find(|&f| is_blink(f)).expect("a blink frame");
        assert_ne!(render(Mood::Chirp, open, 0), render(Mood::Chirp, shut, 0));
    }

    #[test]
    fn edge_shapes_stay_in_bounds() {
        for mood in MOODS {
            let _ = render(mood, usize::MAX, 255);
        }
    }
}
