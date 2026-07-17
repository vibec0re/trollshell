//! Procedural color-LCD face for caw — a cybercrow drawn straight into a
//! 128×128 RGBA8 buffer for the [`Node::Pixels`](hytte_plugin::proto::Node::Pixels)
//! node the host upscales nearest-neighbor (the chunky "LCD" look). No sprite
//! assets: every part is hard-edged pixel drawing at native resolution.
//!
//! Unlike the pet's front-on cat ball, caw is a **corvid bust in 3/4 view**:
//! near-black plumage over shoulders, a long heavy beak jutting down-left, a
//! ragged crest, throat hackles, iridescent sheen streaks, and eyes that
//! **glow** on chaos (intensity from her `chaos_level`). The mood → face
//! mapping ([`face_params`]) is an exhaustive match on [`Mood`], so adding a
//! mood in `main.rs` fails to compile here until its face is defined.

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

// ── Palette — a near-black crow popping off a purple LCD field ──────────────
const SCREEN_BG: Rgb = [0x31, 0x1e, 0x4e]; // purple screen field (lighter than the bird)
const SCREEN_EDGE: Rgb = [0x12, 0x0a, 0x1e]; // baked corner cut = bezel dark (CSS matches)
const FEATHER: Rgb = [0x20, 0x1c, 0x2e]; // near-black charcoal plumage
const FEATHER_HI: Rgb = [0x38, 0x32, 0x52]; // crown / shoulder feather light
const RIM: Rgb = [0x5c, 0x52, 0x8e]; // cool rim-light along the silhouette
const SHEEN_BLUE: Rgb = [0x46, 0x6c, 0xba]; // blue iridescence streak
const SHEEN_TEAL: Rgb = [0x3c, 0x92, 0x84]; // teal iridescence streak
const OUTLINE: Rgb = [0x0c, 0x09, 0x16]; // darkest silhouette rim
const BEAK: Rgb = [0x3c, 0x3a, 0x4c]; // graphite beak (lighter than the feathers)
const BEAK_HI: Rgb = [0x8e, 0x8a, 0xa8]; // culmen ridge highlight
const BEAK_IN: Rgb = [0x60, 0x24, 0x42]; // open-beak interior (dark maw)
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
    draw_body(&mut buf);
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
    Glow,    // chaos energy bits (scaled by intensity)
    Sparkle, // gremlin mischief
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

/// Whether (`dx`, `dy`) lies inside an origin-centered ellipse with radii
/// (`rx`, `ry`). `i64` keeps the cross-multiplied comparison overflow-free.
fn in_ellipse(dx: i32, dy: i32, rx: i32, ry: i32) -> bool {
    if rx <= 0 || ry <= 0 {
        return false;
    }
    let (dx, dy, rx, ry) = (i64::from(dx), i64::from(dy), i64::from(rx), i64::from(ry));
    dx * dx * ry * ry + dy * dy * rx * rx <= rx * rx * ry * ry
}

/// A filled axis-aligned ellipse — heads and shoulders are longer than tall.
fn ellipse(buf: &mut [u8], cx: i32, cy: i32, rx: i32, ry: i32, col: Rgb) {
    for dy in -ry..=ry {
        for dx in -rx..=rx {
            if in_ellipse(dx, dy, rx, ry) {
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
    let edge = |a: (i32, i32), b: (i32, i32), px: i32, py: i32| {
        (b.0 - a.0) * (py - a.1) - (b.1 - a.1) * (px - a.0)
    };
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
    (
        p.0 + (q.0 - p.0).signum() * n,
        p.1 + (q.1 - p.1).signum() * n,
    )
}

// ── Corvid parts ────────────────────────────────────────────────────────────
//
// She's a bust in 3/4 view, facing viewer-left: head slightly right of center,
// the beak jutting down-left past the silhouette, shoulders filling the bottom
// of the frame. All coordinates below share these anchors.

const HEAD_CX: i32 = 66;
const HEAD_CY: i32 = 50;
const HEAD_RX: i32 = 32;
const HEAD_RY: i32 = 29;
const EYE_Y: i32 = 44;
const EYE_LX: i32 = 50;
const EYE_RX: i32 = 80;
/// Where the upper beak meets the face (between and below the eyes).
const BEAK_BASE: (i32, i32) = (60, 48);
/// Corner of the mouth on the cheek.
const BEAK_GAPE: (i32, i32) = (74, 66);

/// Shoulders + breast: a wide dark mass rising to meet the head (no floating
/// ball), with a folded-wing seam and a few breast-feather ticks so it reads
/// as plumage.
fn draw_body(buf: &mut [u8]) {
    ellipse(buf, 74, 122, 54, 42, OUTLINE);
    ellipse(buf, 74, 124, 52, 40, FEATHER);
    // Folded wing: darker seams sweeping over the right shoulder.
    stroke(buf, 60, 96, 100, 104, OUTLINE);
    stroke(buf, 70, 108, 104, 118, OUTLINE);
    // Breast feather ticks.
    for &(x, y) in &[(46, 108), (56, 118), (42, 120)] {
        hline(buf, x, x + 4, y, FEATHER_HI);
    }
    // Shoulder rim-light where the screen glow catches the wing.
    stroke(buf, 92, 92, 112, 104, RIM);
}

/// Skull + face: an ellipse with a cool rim-light along the upper-left
/// silhouette (so the black bird separates from the dark screen) and shaggy
/// throat hackles where head meets breast.
fn draw_head(buf: &mut [u8]) {
    ellipse(buf, HEAD_CX, HEAD_CY, HEAD_RX + 2, HEAD_RY + 2, OUTLINE);
    ellipse(buf, HEAD_CX, HEAD_CY, HEAD_RX, HEAD_RY, FEATHER);
    // Crown light: a softer feather tone over the top third of the skull.
    for dy in -HEAD_RY..-(HEAD_RY / 2) {
        for dx in -HEAD_RX..=HEAD_RX {
            if in_ellipse(dx, dy, HEAD_RX, HEAD_RY)
                && !in_ellipse(dx, dy + 6, HEAD_RX - 8, HEAD_RY - 8)
            {
                plot(buf, HEAD_CX + dx, HEAD_CY + dy, FEATHER_HI);
            }
        }
    }
    // Rim-light band hugging the inside of the upper-left silhouette.
    for dy in -HEAD_RY..=0 {
        for dx in -HEAD_RX..0 {
            if in_ellipse(dx, dy, HEAD_RX, HEAD_RY)
                && !in_ellipse(dx, dy, HEAD_RX - 2, HEAD_RY - 2)
                && dy < -HEAD_RY / 4
            {
                plot(buf, HEAD_CX + dx, HEAD_CY + dy, RIM);
            }
        }
    }
    // Throat hackles: ragged little feather spikes off the jaw into the breast.
    for &(bx, by, len) in &[(48, 72, 7), (58, 77, 9), (69, 79, 8), (80, 75, 6)] {
        fill_tri(buf, (bx - 3, by), (bx + 3, by), (bx, by + len), OUTLINE);
        fill_tri(buf, (bx - 2, by), (bx + 2, by), (bx, by + len - 2), FEATHER);
    }
}

/// Iridescent sheen streaks that drift across the crown and shoulder with the
/// frame — the oil-slick blue/teal glint on a corvid's black feathers.
fn draw_sheen(buf: &mut [u8], frame: usize) {
    let d = i32::try_from(frame % 4).unwrap_or(0) - 1;
    stroke(buf, 52 + d, 32, 66 + d, 27, SHEEN_BLUE);
    stroke(buf, 74 - d, 27, 88 - d, 33, SHEEN_TEAL);
    plot(buf, 62 + d, 26, SHEEN_TEAL);
    plot(buf, 80 - d, 25, SHEEN_BLUE);
    // A glint on the folded wing too.
    stroke(buf, 84 - d, 118, 98 - d, 122, SHEEN_BLUE);
}

/// The head-feather crest: four ragged tufts along the crown, their splay set
/// by mood — short and hugging the skull, not antenna horns.
fn draw_crest(buf: &mut [u8], crest: Crest, frame: usize) {
    let wob = i32::try_from(frame % 2).unwrap_or(0);
    // (base_x, base_y, tip offset) per tuft; crows keep it low and scruffy.
    let tufts: [(i32, i32, (i32, i32)); 4] = match crest {
        Crest::Up => [
            (46, 30, (-7, -16)),
            (58, 25, (-3, -19)),
            (70, 24, (3, -19)),
            (82, 29, (8, -15)),
        ],
        Crest::Ruffled => [
            (44, 31, (-13, -12)),
            (57, 25, (-6, -17)),
            (71, 24, (6, -17)),
            (84, 30, (14, -11)),
        ],
        Crest::Neat => [
            (48, 29, (-3, -11)),
            (59, 25, (-1, -13)),
            (70, 24, (1, -13)),
            (81, 28, (4, -11)),
        ],
        Crest::Droop => [
            (48, 29, (-8, -4)),
            (59, 25, (-5, -7)),
            (70, 24, (4, -7)),
            (81, 28, (9, -3)),
        ],
    };
    for (bx, by, (tdx, tdy)) in tufts {
        let tip = (bx + tdx, by + tdy + wob);
        // Solid near-black feathers (light cores read as antennae, not plumage).
        fill_tri(buf, (bx - 7, by + 5), (bx + 7, by + 5), tip, OUTLINE);
        fill_tri(
            buf,
            shrink((bx - 7, by + 5), tip, 1),
            shrink((bx + 7, by + 5), tip, 1),
            shrink(tip, (bx, by + 5), 1),
            FEATHER,
        );
    }
}

fn draw_eyes(buf: &mut [u8], eyes: EyeShape, look: i32, glow: bool, intensity: u8, frame: usize) {
    // Chaos glow: a pulsing cyan halo behind each eye, brighter with intensity.
    if glow {
        let pulse = i32::from(frame.is_multiple_of(2));
        let r = 7 + pulse + i32::from(intensity > 160);
        let halo = if intensity > 200 { GLOW_HOT } else { GLOW };
        ring(buf, EYE_LX, EYE_Y, r, 2, halo);
        ring(buf, EYE_RX, EYE_Y, r, 2, halo);
    }
    match eyes {
        EyeShape::Round => {
            corvid_eye(buf, EYE_LX, EYE_Y, 5, look, glow);
            corvid_eye(buf, EYE_RX, EYE_Y, 5, look, glow);
        }
        EyeShape::Wide => {
            corvid_eye(buf, EYE_LX, EYE_Y, 7, 0, glow);
            corvid_eye(buf, EYE_RX, EYE_Y, 7, 0, glow);
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
            // Angry brow feathers slanting down toward the beak base.
            stroke(buf, EYE_LX - 7, EYE_Y - 8, EYE_LX + 5, EYE_Y - 3, OUTLINE);
            stroke(buf, EYE_RX - 5, EYE_Y - 3, EYE_RX + 7, EYE_Y - 8, OUTLINE);
        }
        EyeShape::Closed => {
            arc(buf, EYE_LX, EYE_Y, 5, 3, PUPIL);
            arc(buf, EYE_RX, EYE_Y, 5, 3, PUPIL);
        }
    }
}

/// A half-lidded (smug) eye: a low pupil under a heavy drooping lid.
fn half_eye(buf: &mut [u8], cx: i32, look: i32) {
    disc(buf, cx, EYE_Y + 1, 4, EYE_RING);
    disc(buf, cx + look, EYE_Y + 1, 2, PUPIL);
    // Heavy lid across the top of the eye, slanting for the smug read.
    stroke(buf, cx - 5, EYE_Y - 2, cx + 5, EYE_Y - 3, PUPIL);
}

/// A corvid eye: pale ring, dark pupil filling most of it, a glint (cyan when
/// glowing). Beady on purpose — big irises read owl, not crow.
fn corvid_eye(buf: &mut [u8], cx: i32, cy: i32, r: i32, look: i32, glow: bool) {
    disc(buf, cx, cy, r, EYE_RING);
    disc(buf, cx + look, cy, r - 2, PUPIL);
    let glint = if glow { GLOW } else { HIGHLIGHT };
    disc(buf, cx - 1 + look, cy - 1, 1, glint);
}

/// The beak — the feature that makes her a crow: a long, heavy graphite wedge
/// from between the eyes jutting down-left past the head silhouette, with a
/// bright culmen ridge, a gape line, and nares (nostril dots) at the base.
fn draw_beak(buf: &mut [u8], beak: BeakShape) {
    match beak {
        BeakShape::Closed => {
            let tip = (16, 84);
            // One outlined wedge, then the mandible split drawn over it.
            fill_tri(buf, BEAK_BASE, BEAK_GAPE, tip, OUTLINE);
            fill_tri(
                buf,
                shrink(BEAK_BASE, BEAK_GAPE, 2),
                shrink(BEAK_GAPE, BEAK_BASE, 2),
                shrink(tip, BEAK_GAPE, 3),
                BEAK,
            );
            // Culmen ridge along the top edge so the beak catches light.
            line(
                buf,
                BEAK_BASE.0,
                BEAK_BASE.1 + 2,
                tip.0 + 3,
                tip.1 - 2,
                BEAK_HI,
            );
            // Gape line from the mouth corner to just short of the tip.
            line(
                buf,
                BEAK_GAPE.0 - 3,
                BEAK_GAPE.1 - 1,
                tip.0 + 5,
                tip.1 - 1,
                OUTLINE,
            );
            // Nares: two dark nostril dots near the base.
            plot(buf, 52, 54, OUTLINE);
            plot(buf, 47, 58, OUTLINE);
        }
        BeakShape::Open => {
            // Both mandibles hinge at the mouth corner so the open beak stays
            // one connected shape: upper tilts up-left, lower drops down-left,
            // dark maw wedge between them.
            let hinge = (72, 63);
            let up_tip = (14, 70);
            let low_tip = (30, 96);
            fill_tri(buf, (70, 63), (32, 73), (40, 84), BEAK_IN);
            // Upper mandible.
            fill_tri(buf, (60, 45), hinge, up_tip, OUTLINE);
            fill_tri(
                buf,
                shrink((60, 45), hinge, 2),
                shrink(hinge, (60, 45), 2),
                shrink(up_tip, hinge, 3),
                BEAK,
            );
            // Lower mandible.
            fill_tri(buf, hinge, (77, 71), low_tip, OUTLINE);
            fill_tri(
                buf,
                shrink(hinge, (77, 71), 1),
                shrink((77, 71), hinge, 1),
                shrink(low_tip, (77, 71), 2),
                BEAK,
            );
            line(buf, 60, 47, up_tip.0 + 3, up_tip.1 - 1, BEAK_HI);
            plot(buf, 52, 52, OUTLINE);
            plot(buf, 47, 56, OUTLINE);
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
    let bits = [
        (22, 24),
        (108, 22),
        (14, 56),
        (114, 58),
        (30, 106),
        (110, 92),
        (68, 10),
    ];
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
    let stars = [(24, 22), (106, 34), (104, 96)];
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
    let dots = [(98, 20), (108, 20), (118, 20)];
    for &(cx, cy) in dots.iter().take(n) {
        disc(buf, cx, cy, 2, DOT);
    }
}

fn draw_zzz(buf: &mut [u8], n: usize) {
    let zs = [(98, 34, 5), (106, 22, 7), (114, 8, 9)];
    for &(x, y, s) in zs.iter().take(n) {
        hline(buf, x, x + s, y, ZZZ);
        line(buf, x + s, y, x, y + s, ZZZ);
        hline(buf, x, x + s, y + s, ZZZ);
    }
}

/// Two pale-blue huff puffs off the beak tip — indignant *ruffles feathers*.
fn draw_huff(buf: &mut [u8], frame: usize) {
    let d = i32::try_from(frame % 3).unwrap_or(0);
    disc(buf, 12 - d, 70 - d, 3, HUFF);
    disc(buf, 22 - d, 62 - d, 2, HUFF);
}

/// A little pink heart and a sparkle — chirp / <3.
fn draw_chirp(buf: &mut [u8], frame: usize) {
    let (cx, cy) = (104, 30 - i32::try_from(frame % 2).unwrap_or(0));
    disc(buf, cx - 2, cy, 2, HEART);
    disc(buf, cx + 2, cy, 2, HEART);
    fill_tri(buf, (cx - 4, cy + 1), (cx + 4, cy + 1), (cx, cy + 6), HEART);
    plot(buf, 24, 30, HIGHLIGHT);
    hline(buf, 22, 26, 30, SPARKLE);
    for dy in -2..=2 {
        plot(buf, 24, 30 + dy, SPARKLE);
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

    /// FNV-1a-64 over raw bytes — a deterministic, version-stable digest
    /// (unlike `std::hash::DefaultHasher`, whose `SipHash` keys/output are not
    /// stable across Rust releases), implemented inline so the golden needs no
    /// hashing dependency.
    fn fnv1a_64(bytes: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
        }
        h
    }

    /// Golden digests captured from the pre-`preem::Frame` renderer, one per
    /// combo in the `MOODS` × `0..40` frames × `[0, 128, 255]` intensity sweep
    /// (mood outermost, intensity innermost — the same nesting as
    /// [`golden_render_is_byte_identical`]). The #365 migration is proven
    /// byte-for-byte iff that test stays green **without editing this array**.
    #[rustfmt::skip]
    const GOLDEN: [u64; 840] = [
        0xe65b_b25a_4336_9fa8, 0xfdd5_b7a5_8df6_fee2, 0x670e_3de6_f863_cee5, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9825_4efd_5bd1_b85e, 0xb37e_e542_4057_93b4, 0xd0ac_5991_d0a5_8c33, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0x68b7_4cd6_fd97_de69, 0x7b3c_f113_419f_3945, 0x7d3a_9466_f34f_4035,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0xe65b_b25a_4336_9fa8, 0xfdd5_b7a5_8df6_fee2, 0x670e_3de6_f863_cee5, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x745d_63db_a3ff_e3b8, 0xe313_4bd5_228d_a9b2, 0x0cdb_3688_306f_3d15, 0xecac_51bb_2e8f_dae1, 0x8396_28c6_af0c_d6ad, 0x6f7c_b8a1_dc2b_80c5,
        0x9973_239d_2633_3396, 0xac0c_048b_d7fe_1ebc, 0xd9b4_b208_94dc_cf8b, 0x3a79_a975_8de0_0f89, 0x623c_407f_1e76_5ee5, 0x722b_f555_defb_7bad,
        0x60ec_8de1_6d89_020a, 0x60ec_8de1_6d89_020a, 0x60ec_8de1_6d89_020a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0x4a5c_6b6f_6b7b_5548, 0x4a5c_6b6f_6b7b_5548, 0x4a5c_6b6f_6b7b_5548, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x8505_9f89_5037_885a, 0x8505_9f89_5037_885a, 0x8505_9f89_5037_885a,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x60ec_8de1_6d89_020a, 0x60ec_8de1_6d89_020a, 0x60ec_8de1_6d89_020a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0x24c5_3728_b674_ff4a, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba, 0xf5f1_8fcf_9c4f_d4ba,
        0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0xcb38_cf04_c085_84b0, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582, 0x70e1_d8b7_8756_f582,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x60e9_354f_6672_c125, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1, 0x8dd7_3dbf_88a8_4ae1,
        0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x962b_0c63_c340_2e1f, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9, 0x134b_83df_821a_40b9,
        0x74f2_5076_7fca_4525, 0x74f2_5076_7fca_4525, 0x74f2_5076_7fca_4525, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2,
        0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62,
        0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6,
        0xfc0b_a632_dcd0_7f27, 0xfc0b_a632_dcd0_7f27, 0xfc0b_a632_dcd0_7f27, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de,
        0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0xd668_ae4b_0920_9416, 0xd668_ae4b_0920_9416, 0xd668_ae4b_0920_9416,
        0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522,
        0x74f2_5076_7fca_4525, 0x74f2_5076_7fca_4525, 0x74f2_5076_7fca_4525, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2,
        0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62,
        0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6,
        0xbff9_08c7_068d_799f, 0xbff9_08c7_068d_799f, 0xbff9_08c7_068d_799f, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de,
        0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0x9af5_68f1_faba_b7a6, 0x9af5_68f1_faba_b7a6, 0x9af5_68f1_faba_b7a6,
        0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522,
        0xe4e8_5082_0a6c_7345, 0xe4e8_5082_0a6c_7345, 0xe4e8_5082_0a6c_7345, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2,
        0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62,
        0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xa2e3_d795_725f_86d9, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6, 0xdbf0_3779_f0fa_8ea6,
        0xbff9_08c7_068d_799f, 0xbff9_08c7_068d_799f, 0xbff9_08c7_068d_799f, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de, 0x2aa0_b42e_c4b2_77de,
        0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0x3f76_7aeb_8d87_98cd, 0x9af5_68f1_faba_b7a6, 0x9af5_68f1_faba_b7a6, 0x9af5_68f1_faba_b7a6,
        0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x9203_0fd7_db7c_1d6b, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522, 0x3371_901c_44c9_2522,
        0xe4e8_5082_0a6c_7345, 0xe4e8_5082_0a6c_7345, 0xe4e8_5082_0a6c_7345, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2, 0xb87f_be8a_69cc_e5b2,
        0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0x632d_d203_9288_4a37, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62, 0xc24e_86fb_6b8c_0f62,
        0x2500_c1c4_3f49_f4c9, 0x2500_c1c4_3f49_f4c9, 0x2500_c1c4_3f49_f4c9, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1,
        0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2,
        0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e,
        0xfb20_190a_e78b_8bab, 0xfb20_190a_e78b_8bab, 0xfb20_190a_e78b_8bab, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9,
        0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0x014e_842b_6561_7066, 0x014e_842b_6561_7066, 0x014e_842b_6561_7066,
        0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae,
        0x2500_c1c4_3f49_f4c9, 0x2500_c1c4_3f49_f4c9, 0x2500_c1c4_3f49_f4c9, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1,
        0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2,
        0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e,
        0x1c10_040d_ed7c_e7f7, 0x1c10_040d_ed7c_e7f7, 0x1c10_040d_ed7c_e7f7, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9,
        0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0x2ce6_d6c3_c7a4_79f2, 0x2ce6_d6c3_c7a4_79f2, 0x2ce6_d6c3_c7a4_79f2,
        0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae,
        0xfe4b_302f_ba66_4aad, 0xfe4b_302f_ba66_4aad, 0xfe4b_302f_ba66_4aad, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1,
        0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2,
        0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x5374_1888_950d_11aa, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e, 0x4d89_92f9_e550_2b9e,
        0x1c10_040d_ed7c_e7f7, 0x1c10_040d_ed7c_e7f7, 0x1c10_040d_ed7c_e7f7, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9, 0xcd4f_3238_8486_cfe9,
        0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0xdacd_ac77_505c_a891, 0x2ce6_d6c3_c7a4_79f2, 0x2ce6_d6c3_c7a4_79f2, 0x2ce6_d6c3_c7a4_79f2,
        0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x21f5_ef10_254e_b79c, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae, 0x51eb_4295_ae64_7eae,
        0xfe4b_302f_ba66_4aad, 0xfe4b_302f_ba66_4aad, 0xfe4b_302f_ba66_4aad, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1, 0x4b3d_a2ca_542f_d9c1,
        0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x428f_a781_321b_65fb, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2, 0x494f_d2bf_4564_15b2,
        0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459,
        0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619,
        0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11,
        0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671,
        0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31,
        0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9,
        0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459,
        0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619,
        0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11,
        0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671,
        0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31,
        0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9,
        0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459,
        0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619,
        0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x6ea0_ec01_5bc0_a305, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11, 0x56ce_8645_e2e5_be11,
        0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x2f58_f665_7821_5503, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671, 0x998c_e379_36f0_c671,
        0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0xb170_7d56_d694_adfd, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31, 0x3883_59a8_83f9_7d31,
        0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x57c8_632a_f845_8a97, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9, 0x793c_e384_b79c_d7a9,
        0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x8060_f9cd_d448_6261, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459, 0x7584_530f_515c_b459,
        0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0xe6d7_4599_7800_06bf, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619, 0x7b10_9ea4_54bd_9619,
        0x015d_8b81_b0c0_9074, 0x015d_8b81_b0c0_9074, 0x015d_8b81_b0c0_9074, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0xc734_a373_b9f9_147e, 0xc734_a373_b9f9_147e, 0xc734_a373_b9f9_147e, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0x65ff_ffbd_b050_fae4, 0x65ff_ffbd_b050_fae4, 0x65ff_ffbd_b050_fae4,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0x015d_8b81_b0c0_9074, 0x015d_8b81_b0c0_9074, 0x015d_8b81_b0c0_9074, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
        0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xc224_8606_f316_4c8c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c, 0xd43c_8397_9a1d_cb5c,
        0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x396d_e4f2_66f7_4256, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74, 0x97b1_328d_f1d6_ed74,
    ];

    /// Byte-for-byte identity guard for the #365 `preem::Frame` migration:
    /// every mood/frame/intensity frame must still hash to its committed
    /// [`GOLDEN`] value. Runs on both the pre- and post-refactor renderer, so a
    /// green run here *is* the pixel-identity proof (enforced in CI, not merely
    /// asserted in the PR body).
    #[test]
    fn golden_render_is_byte_identical() {
        let mut i = 0;
        for mood in MOODS {
            for frame in 0..40 {
                for intensity in [0u8, 128, 255] {
                    let got = fnv1a_64(&render(mood, frame, intensity));
                    assert_eq!(
                        got, GOLDEN[i],
                        "byte drift at mood {mood:?} frame {frame} intensity {intensity}"
                    );
                    i += 1;
                }
            }
        }
        assert_eq!(i, GOLDEN.len(), "sweep length must match the golden array");
    }
}
