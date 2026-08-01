//! Procedural color-LCD face for the pet (issue #284).
//!
//! Renders the cat's head straight into a 128×128 RGBA8 buffer — no sprite
//! assets — for the [`Node::Pixels`](hytte_plugin::proto::Node::Pixels) node the
//! host materializes with **nearest-neighbor** upscaling. That upscale is what
//! gives the chunky-pixel LCD look; this module only needs to draw hard-edged
//! (never anti-aliased) shapes at native resolution.
//!
//! # Palette — a warm lilac LCD
//!
//! Tuned to the shell's purple/lilac chrome (`@shell_background rgb(28,6,44)`):
//! a soft lilac-pink cat on a slightly-warmer-than-shell dark screen, with pink
//! blush/nose and pale-lilac sleep/think glyphs. Everything is fully opaque —
//! the screen itself is opaque; the bezel/rounding is CSS + a baked corner cut.
//!
//! # Expressions (mood → face)
//!
//! The [`face_params`] mapping is an **exhaustive match on [`Mood`]**, so adding
//! a mood in `main.rs` fails to compile here until its face is defined — that is
//! the "kaomoji frames and LCD faces stay in sync" guarantee.
//!
//! | Mood     | eyes            | mouth         | extras                    |
//! |----------|-----------------|---------------|---------------------------|
//! | Happy    | round, glancing | cat ‿‿        | soft blush                |
//! | Sleepy   | closed ‿        | tiny          | growing `z z Z`, ears droop |
//! | Excited  | wide            | open          | strong blush, sparkles    |
//! | Grumpy   | narrowed + brows| frown         | sweat drop, ears back     |
//! | Thinking | round, look-up  | tiny          | `. .. ...` dots           |
//!
//! # Blink
//!
//! [`is_blink`] is a pure hash of the frame counter (no RNG read in the render
//! path, so [`crate::Pet::view`] stays pure): open-eyed moods shut their eyes
//! for one frame on a blink, roughly one in six ticks and never twice running.
//! Sleepy eyes are already closed.
//!
//! # Scanlines
//!
//! Intentionally **not** baked in. The host's `PixelSurface` overrides `snapshot`
//! and does not chain CSS backgrounds, so a CSS scanline overlay would not paint
//! over the texture; and a 1px baked scanline would moiré under the non-integer
//! nearest-neighbor scale. The chunky upscaled pixels plus the CSS bezel
//! (`.pet-face`) carry the LCD read instead.

use crate::Mood;

/// LCD buffer edge length in device pixels (square).
pub(crate) const SIZE: usize = 128;
/// The same edge as the `u32` the wire's `Pixels { width, height }` wants.
pub(crate) const SIZE_U32: u32 = 128;
/// The same edge as `i32`, for signed geometry math.
const DIM: i32 = 128;
/// Radius of the baked rounded-corner cut (screen glass edge).
const CORNER_R: i32 = 12;

// The three views of the edge length must agree; pin them at compile time.
const _: () = assert!(SIZE == 128 && SIZE_U32 == 128 && DIM == 128 && CORNER_R < DIM / 2);

/// An opaque RGB color; alpha is always `0xff` on the screen.
type Rgb = [u8; 3];

// ── Palette ──────────────────────────────────────────────────────────────
const SCREEN_BG: Rgb = [0x2a, 0x18, 0x3e]; // warm dark purple (screen field)
const SCREEN_EDGE: Rgb = [0x12, 0x0a, 0x1e]; // baked corner cut = bezel dark
const HEAD: Rgb = [0xe9, 0xc9, 0xf0]; // soft lilac-pink fur
const OUTLINE: Rgb = [0x4a, 0x2c, 0x63]; // deep-purple silhouette rim
const INK: Rgb = [0x35, 0x1f, 0x4e]; // eyes / mouth "dark pixels"
const EAR_INNER: Rgb = [0xff, 0x9e, 0xcb]; // pink inner ear
const NOSE: Rgb = [0xff, 0x6f, 0xae]; // pink nose / tongue
const BLUSH: Rgb = [0xff, 0xa9, 0xcf]; // soft cheek blush
const BLUSH_HOT: Rgb = [0xff, 0x82, 0xbe]; // excited cheek blush
const HIGHLIGHT: Rgb = [0xff, 0xf4, 0xfb]; // eye / sparkle glint
const WHISKER: Rgb = [0xb9, 0x99, 0xd6]; // muted lilac whiskers
const ZZZ: Rgb = [0xbc, 0xa6, 0xe8]; // pale sleep z's
const DOT: Rgb = [0xd4, 0xbc, 0xee]; // pale thinking dots
const SPARKLE: Rgb = [0xff, 0x92, 0xcb]; // excited sparkle pink
const SWEAT: Rgb = [0x8f, 0xc9, 0xff]; // pale-blue grumpy sweat

/// Render the LCD face for `mood` at animation `frame` into a fresh
/// `SIZE`×`SIZE` RGBA8 buffer (`SIZE*SIZE*4` bytes exactly — the host's
/// validation invariant).
pub(crate) fn render(mood: Mood, frame: usize) -> Vec<u8> {
    let mut buf = vec![0u8; SIZE * SIZE * 4];
    let face = face_params(mood, frame);
    fill(&mut buf, SCREEN_BG);
    draw_ears(&mut buf, face.ears);
    draw_head(&mut buf);
    draw_blush(&mut buf, face.blush);
    draw_whiskers(&mut buf);
    draw_nose(&mut buf);
    draw_mouth(&mut buf, face.mouth);
    draw_eyes(&mut buf, face.eyes, face.look);
    draw_extra(&mut buf, face.extra, frame);
    round_corners(&mut buf);
    buf
}

/// One in ~six frames is a blink; a cheap integer hash jitters the cadence so
/// it is not a rigid every-Nth tic, and the step-by-`prev` check keeps two
/// blinks from landing back to back. Pure in `frame` — `view` reads no RNG.
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
    Closed,
    Narrow,
}

#[derive(Clone, Copy)]
enum MouthShape {
    Cat,
    Open,
    Frown,
    Tiny,
}

#[derive(Clone, Copy)]
enum Ears {
    Up,
    Droop,
    Back,
}

#[derive(Clone, Copy)]
enum Extra {
    None,
    Zzz(usize),
    Sparkle,
    Sweat,
    Dots(usize),
}

/// Blush intensity: 0 none, 1 soft, 2 hot.
struct Face {
    eyes: EyeShape,
    mouth: MouthShape,
    ears: Ears,
    extra: Extra,
    /// Horizontal pupil glance, `-1..=1`, for a little life.
    look: i32,
    /// Blush level (0/1/2).
    blush: u8,
}

/// Map a [`Mood`] (plus the blink sub-cycle and per-frame liveliness) to the
/// face to draw. Exhaustive over `Mood` on purpose (see the module docs).
fn face_params(mood: Mood, frame: usize) -> Face {
    let blink = is_blink(frame);
    let step = (frame % 3) + 1;
    match mood {
        Mood::Happy => Face {
            eyes: if blink {
                EyeShape::Closed
            } else {
                EyeShape::Round
            },
            mouth: MouthShape::Cat,
            ears: Ears::Up,
            extra: Extra::None,
            look: [-1, 0, 1, 0][frame % 4],
            blush: 1,
        },
        Mood::Sleepy => Face {
            eyes: EyeShape::Closed,
            mouth: MouthShape::Tiny,
            ears: Ears::Droop,
            extra: Extra::Zzz(step),
            look: 0,
            blush: 0,
        },
        Mood::Excited => Face {
            eyes: if blink {
                EyeShape::Closed
            } else {
                EyeShape::Wide
            },
            mouth: MouthShape::Open,
            ears: Ears::Up,
            extra: Extra::Sparkle,
            look: 0,
            blush: 2,
        },
        Mood::Grumpy => Face {
            eyes: if blink {
                EyeShape::Closed
            } else {
                EyeShape::Narrow
            },
            mouth: MouthShape::Frown,
            ears: Ears::Back,
            extra: Extra::Sweat,
            look: 0,
            blush: 0,
        },
        Mood::Thinking => Face {
            eyes: if blink {
                EyeShape::Closed
            } else {
                EyeShape::Round
            },
            mouth: MouthShape::Tiny,
            ears: Ears::Up,
            extra: Extra::Dots(step),
            look: 1,
            blush: 1,
        },
    }
}

// ── Drawing primitives (pure buffer functions, trivially testable) ─────────

/// Paint one opaque pixel, silently clipping anything outside the buffer (so
/// edge-straddling shapes never panic or wrap).
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

/// Flood the whole buffer with one opaque color.
fn fill(buf: &mut [u8], col: Rgb) {
    for px in buf.chunks_exact_mut(4) {
        px[0..3].copy_from_slice(&col);
        px[3] = 0xff;
    }
}

/// Horizontal run, inclusive of both ends.
fn hline(buf: &mut [u8], x0: i32, x1: i32, y: i32, col: Rgb) {
    for x in x0.min(x1)..=x0.max(x1) {
        plot(buf, x, y, col);
    }
}

/// A filled disc of radius `r` centered at (`cx`, `cy`).
fn disc(buf: &mut [u8], cx: i32, cy: i32, r: i32, col: Rgb) {
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy <= r * r {
                plot(buf, cx + dx, cy + dy, col);
            }
        }
    }
}

/// A 1px Bresenham segment.
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

/// A 2px-thick segment (the LCD look wants strokes wider than one pixel).
fn stroke(buf: &mut [u8], x0: i32, y0: i32, x1: i32, y1: i32, col: Rgb) {
    line(buf, x0, y0, x1, y1, col);
    line(buf, x0, y0 + 1, x1, y1 + 1, col);
}

/// A filled triangle, either winding order (edge-sign test over the bbox).
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
/// middle **down** (a ‿ smile); negative bows it **up** (a ∩ frown).
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

/// Round off the four screen corners into the bezel color, so the square
/// texture reads as an inset LCD panel.
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

// ── Cat parts ──────────────────────────────────────────────────────────────

/// Head geometry, shared by the feature placements below.
const HEAD_CX: i32 = 64;
const HEAD_CY: i32 = 70;
const HEAD_R: i32 = 38;
const EYE_Y: i32 = 66;
const EYE_LX: i32 = 50;
const EYE_RX: i32 = 78;

fn draw_head(buf: &mut [u8]) {
    disc(buf, HEAD_CX, HEAD_CY, HEAD_R + 2, OUTLINE);
    disc(buf, HEAD_CX, HEAD_CY, HEAD_R, HEAD);
}

fn draw_ears(buf: &mut [u8], ears: Ears) {
    // Bases sit on the head's upper arc; the tip moves with the mood.
    let (l_tip, r_tip) = match ears {
        Ears::Up => ((38, 20), (90, 20)),
        Ears::Droop => ((28, 34), (100, 34)),
        Ears::Back => ((24, 44), (104, 44)),
    };
    draw_ear(buf, (32, 52), (56, 46), l_tip);
    draw_ear(buf, (72, 46), (96, 52), r_tip);
}

/// One ear: outlined outer triangle, lilac fill, pink inner.
fn draw_ear(buf: &mut [u8], base_a: (i32, i32), base_b: (i32, i32), tip: (i32, i32)) {
    fill_tri(buf, base_a, base_b, tip, OUTLINE);
    fill_tri(
        buf,
        shrink(base_a, tip, 2),
        shrink(base_b, tip, 2),
        shrink(tip, base_a, 2),
        HEAD,
    );
    fill_tri(
        buf,
        shrink(base_a, tip, 6),
        shrink(base_b, tip, 6),
        shrink(tip, base_a, 3),
        EAR_INNER,
    );
}

/// Move point `p` `n` pixels toward `q` on each axis (a cheap inset).
fn shrink(p: (i32, i32), q: (i32, i32), n: i32) -> (i32, i32) {
    (
        p.0 + (q.0 - p.0).signum() * n,
        p.1 + (q.1 - p.1).signum() * n,
    )
}

fn draw_blush(buf: &mut [u8], level: u8) {
    let (col, r) = match level {
        1 => (BLUSH, 5),
        2 => (BLUSH_HOT, 6),
        _ => return,
    };
    disc(buf, 40, 80, r, col);
    disc(buf, 88, 80, r, col);
}

fn draw_whiskers(buf: &mut [u8]) {
    for (ys, ye) in [(78, 79), (84, 84), (90, 88)] {
        line(buf, 18, ys, 40, ye, WHISKER);
        line(buf, DIM - 1 - 18, ys, DIM - 1 - 40, ye, WHISKER);
    }
}

fn draw_nose(buf: &mut [u8]) {
    fill_tri(buf, (60, 78), (68, 78), (64, 83), NOSE);
}

fn draw_mouth(buf: &mut [u8], mouth: MouthShape) {
    let (mx, my) = (64, 86);
    match mouth {
        MouthShape::Cat => {
            arc(buf, mx - 5, my, 5, 3, INK);
            arc(buf, mx + 5, my, 5, 3, INK);
        }
        MouthShape::Open => {
            disc(buf, mx, my + 1, 5, INK);
            disc(buf, mx, my + 3, 2, NOSE);
        }
        MouthShape::Frown => arc(buf, mx, my + 3, 7, -3, INK),
        MouthShape::Tiny => disc(buf, mx, my, 2, INK),
    }
}

fn draw_eyes(buf: &mut [u8], eyes: EyeShape, look: i32) {
    match eyes {
        EyeShape::Round => {
            round_eye(buf, EYE_LX, EYE_Y, 6, look);
            round_eye(buf, EYE_RX, EYE_Y, 6, look);
        }
        EyeShape::Wide => {
            round_eye(buf, EYE_LX, EYE_Y, 8, 0);
            round_eye(buf, EYE_RX, EYE_Y, 8, 0);
        }
        EyeShape::Closed => {
            arc(buf, EYE_LX, EYE_Y, 6, 3, INK);
            arc(buf, EYE_RX, EYE_Y, 6, 3, INK);
        }
        EyeShape::Narrow => {
            round_eye(buf, EYE_LX, EYE_Y + 2, 5, 0);
            round_eye(buf, EYE_RX, EYE_Y + 2, 5, 0);
            // Angry brows: \  /  slanting down toward the nose.
            stroke(buf, EYE_LX - 7, EYE_Y - 8, EYE_LX + 5, EYE_Y - 3, INK);
            stroke(buf, EYE_RX - 5, EYE_Y - 3, EYE_RX + 7, EYE_Y - 8, INK);
        }
    }
}

/// A round eye with a pupil glint offset by the horizontal `look`.
fn round_eye(buf: &mut [u8], cx: i32, cy: i32, r: i32, look: i32) {
    disc(buf, cx, cy, r, INK);
    disc(buf, cx - 2 + look, cy - 2, r / 3, HIGHLIGHT);
}

fn draw_extra(buf: &mut [u8], extra: Extra, frame: usize) {
    match extra {
        Extra::None => {}
        Extra::Zzz(n) => draw_zzz(buf, n),
        Extra::Sparkle => draw_sparkles(buf, frame),
        Extra::Sweat => draw_sweat(buf, frame),
        Extra::Dots(n) => draw_dots(buf, n),
    }
}

/// A rising `z z Z` above the right ear; higher z's are larger.
fn draw_zzz(buf: &mut [u8], n: usize) {
    let zs = [(96, 40, 5), (106, 28, 7), (116, 14, 9)];
    for &(x, y, s) in zs.iter().take(n) {
        hline(buf, x, x + s, y, ZZZ);
        line(buf, x + s, y, x, y + s, ZZZ);
        hline(buf, x, x + s, y + s, ZZZ);
    }
}

/// Four-point sparkles that twinkle: each toggles size with the frame.
fn draw_sparkles(buf: &mut [u8], frame: usize) {
    let stars = [(24, 40), (104, 44), (96, 100)];
    for (i, &(cx, cy)) in stars.iter().enumerate() {
        let s = 3 + i32::from(frame.wrapping_add(i).is_multiple_of(2)) * 2;
        hline(buf, cx - s, cx + s, cy, SPARKLE);
        for dy in -s..=s {
            plot(buf, cx, cy + dy, SPARKLE);
        }
        plot(buf, cx, cy, HIGHLIGHT);
    }
}

/// A pale-blue sweat drop by the left ear, dripping with the frame.
fn draw_sweat(buf: &mut [u8], frame: usize) {
    let cx = 30;
    let cy = 40 + i32::try_from(frame % 3).unwrap_or(0);
    disc(buf, cx, cy, 4, SWEAT);
    fill_tri(buf, (cx - 4, cy), (cx + 4, cy), (cx, cy - 8), SWEAT);
    plot(buf, cx - 1, cy - 1, HIGHLIGHT);
}

/// Thinking `. .. ...` — the first `n` of three dots light up.
fn draw_dots(buf: &mut [u8], n: usize) {
    let dots = [(98, 28), (108, 28), (118, 28)];
    for &(cx, cy) in dots.iter().take(n) {
        disc(buf, cx, cy, 2, DOT);
    }
}

#[cfg(test)]
mod tests {
    use super::{SIZE, is_blink, render};
    use crate::Mood;

    const MOODS: [Mood; 5] = [
        Mood::Happy,
        Mood::Sleepy,
        Mood::Excited,
        Mood::Grumpy,
        Mood::Thinking,
    ];

    /// The host validates `data.len() == width*height*4` and renders nothing
    /// otherwise — so every mood at every frame MUST fill exactly the buffer.
    #[test]
    fn buffer_is_always_exactly_full() {
        for mood in MOODS {
            for frame in 0..40 {
                let buf = render(mood, frame);
                assert_eq!(
                    buf.len(),
                    SIZE * SIZE * 4,
                    "{mood:?} frame {frame} must be a full 128x128 RGBA8 buffer"
                );
            }
        }
    }

    /// Every pixel is opaque — the LCD screen is solid, no stray transparency.
    #[test]
    fn every_pixel_is_opaque() {
        let buf = render(Mood::Happy, 0);
        assert!(buf.chunks_exact(4).all(|px| px[3] == 0xff));
    }

    /// Distinct moods must be visibly different (buffer inequality), so the
    /// expression actually tracks the mood.
    #[test]
    fn moods_render_differently() {
        let happy = render(Mood::Happy, 0);
        let grumpy = render(Mood::Grumpy, 0);
        let sleepy = render(Mood::Sleepy, 0);
        assert_ne!(happy, grumpy, "happy and grumpy faces must differ");
        assert_ne!(happy, sleepy, "happy and sleepy faces must differ");
        assert_ne!(grumpy, sleepy, "grumpy and sleepy faces must differ");
    }

    /// A blink frame differs from an open-eyed frame of the same mood.
    #[test]
    fn blink_changes_the_face() {
        // Happy has open eyes; find an open frame and a blink frame.
        let open = (0..50).find(|&f| !is_blink(f)).expect("an open frame");
        let shut = (0..50).find(|&f| is_blink(f)).expect("a blink frame");
        assert_ne!(
            render(Mood::Happy, open),
            render(Mood::Happy, shut),
            "eyes-open and eyes-shut happy frames must differ"
        );
    }

    /// Blinks are not constant and not doubled up.
    #[test]
    fn blink_cadence_is_sane() {
        let blinks = (0..120).filter(|&f| is_blink(f)).count();
        assert!(
            blinks > 0 && blinks < 60,
            "blinks are occasional ({blinks})"
        );
        assert!(
            !(0..120).any(|f| is_blink(f) && is_blink(f + 1)),
            "never two blinks in a row"
        );
    }

    /// The per-frame animation moves within a mood (e.g. sleepy z-count, happy
    /// glance), so the face is alive between blinks too.
    #[test]
    fn animation_advances_within_a_mood() {
        let frames: Vec<_> = (0..6).map(|f| render(Mood::Sleepy, f)).collect();
        assert!(
            frames.iter().any(|f| *f != frames[0]),
            "sleepy face animates across frames"
        );
    }

    /// Drawing helpers clip: a shape centered off the edge must not panic or
    /// wrap into the opposite side.
    #[test]
    fn edge_shapes_stay_in_bounds() {
        let mut buf = vec![0u8; SIZE * SIZE * 4];
        super::fill(&mut buf, [0, 0, 0]);
        // Discs, arcs, lines, and triangles straddling every edge/corner.
        super::disc(&mut buf, 0, 0, 20, [1, 2, 3]);
        super::disc(&mut buf, 127, 127, 20, [1, 2, 3]);
        super::disc(&mut buf, -10, 64, 20, [1, 2, 3]);
        super::arc(&mut buf, 2, 1, 10, 5, [1, 2, 3]);
        super::line(&mut buf, -5, -5, 140, 140, [1, 2, 3]);
        super::fill_tri(&mut buf, (-8, -8), (140, 4), (4, 140), [1, 2, 3]);
        // Every mood at an extreme frame likewise must not panic.
        for mood in MOODS {
            let _ = render(mood, usize::MAX);
        }
        assert_eq!(buf.len(), SIZE * SIZE * 4);
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
    /// combo in the `MOODS` × `0..40` frames sweep (mood outermost, frame
    /// innermost — the same nesting as [`golden_render_is_byte_identical`]).
    /// The #650 migration is proven byte-for-byte iff that test stays green
    /// **without editing this array**.
    #[rustfmt::skip]
    const GOLDEN: [u64; 200] = [
        0x2927_7dea_40fb_e12a, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe,
        0x2927_7dea_40fb_e12a, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x2927_7dea_40fb_e12a, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe,
        0x2927_7dea_40fb_e12a, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe,
        0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe,
        0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe,
        0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe,
        0x7b08_6f27_19c0_bd76, 0x9b47_262e_d3a8_99fe, 0x4c3b_1fd7_9768_07a6, 0x9b47_262e_d3a8_99fe, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1, 0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0x76b8_da4a_ef05_36c1,
        0xbafa_86ff_642f_1311, 0xfc60_c0b3_4ea3_96f5, 0xf894_1fca_23b6_17e4, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0xf894_1fca_23b6_17e4, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0x4ed6_2931_542a_01e8,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0xf894_1fca_23b6_17e4, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c, 0x952c_e894_6887_a868, 0xd38a_c721_3ba3_8c1c,
        0x1b12_b6ac_131c_7fe9, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0x1b12_b6ac_131c_7fe9, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0x1b12_b6ac_131c_7fe9, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0x1b12_b6ac_131c_7fe9, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891,
        0xc7c7_88f6_fb1d_03ad, 0x3ea5_036f_9e18_76f3, 0xe3f1_d337_b21f_4891, 0xc7c7_88f6_fb1d_03ad, 0x4700_3aab_6cb5_c1f4, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0x4700_3aab_6cb5_c1f4, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0x4700_3aab_6cb5_c1f4, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0x4700_3aab_6cb5_c1f4, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706, 0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8, 0xefd7_bd7b_46f5_1706,
        0xa38f_987c_b550_70b8, 0xfbb7_4ca5_93b6_1fb8,
    ];

    /// Byte-for-byte identity guard for the #650 `preem::Frame` migration:
    /// every mood/frame must still hash to its committed [`GOLDEN`] value. Runs
    /// on both the pre- and post-refactor renderer, so a green run here *is*
    /// the pixel-identity proof (enforced in CI, not merely asserted in the PR
    /// body). Mirrors caw's `golden_render_is_byte_identical` (#365 / PR #378).
    #[test]
    fn golden_render_is_byte_identical() {
        let mut i = 0;
        for mood in MOODS {
            for frame in 0..40 {
                let got = fnv1a_64(&render(mood, frame));
                assert_eq!(got, GOLDEN[i], "byte drift at mood {mood:?} frame {frame}");
                i += 1;
            }
        }
        assert_eq!(i, GOLDEN.len(), "sweep length must match the golden array");
    }

    /// Sleepy is drawn eyes-closed regardless of the blink cycle: a blink frame
    /// and a non-blink frame with the same animation step render identically.
    #[test]
    fn sleepy_ignores_the_blink_cycle() {
        let shut = (0..300).find(|&f| is_blink(f)).expect("a blink frame");
        let open = (0..300)
            .find(|&f| !is_blink(f) && f % 3 == shut % 3)
            .expect("a non-blink frame at the same step");
        assert_eq!(
            render(Mood::Sleepy, shut),
            render(Mood::Sleepy, open),
            "sleepy eyes stay closed whether or not the frame is a blink"
        );
    }
}
