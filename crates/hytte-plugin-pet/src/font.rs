//! A tiny hand-rolled **5×7 pixel font** for the pet's speech bubble (#304).
//!
//! "font boring — needs more 8bit" → the bubble line is rendered as *pixels*,
//! not a `gtk::Label`, and emitted as a [`Node::Pixels`](hytte_plugin::proto::Node::Pixels)
//! the host upscales nearest-neighbor — so the text comes out chunky and crisp,
//! the same LCD idiom as the face. No font files, no new deps: every glyph is a
//! `const` bitmap.
//!
//! # Grid
//!
//! Each glyph is a **5-wide × 7-tall** cell, stored as `[u8; 7]` — one byte per
//! row, top row first. Within a row the low 5 bits are the columns, **bit 4 the
//! leftmost** column and bit 0 the rightmost, so a binary literal reads
//! left-to-right as the pixels: `0b01110` is `.###.`. Glyphs advance by
//! `GLYPH_W + 1` px (1 px inter-letter gap) and lines by `GLYPH_H + LINE_GAP`.
//!
//! # Coverage
//!
//! Printable ASCII (letters both cases, digits, and the punctuation the bubble
//! actually uses — `! ? . , : ; ' " ( ) - ~ / …`) plus the accented set the
//! pet's German/Swedish desktop produces and [`crate::brain`] never strips:
//! `å ä ö Å Ä Ö ü Ü ß é`. (The brain's `sanitize` drops emoji, so `💕`-class
//! codepoints never reach the bubble — and any uncovered char, emoji included,
//! renders as a **dim hollow box** ([`glyph`] returns `None`), never a panic.)
//! The accented uppercase and `å/Å` glyphs are compact approximations — the ring
//! on `å` is drawn as a single dot, and the umlaut/ring uppercase bodies are
//! squeezed into five rows to leave room for the diacritic.

use hytte_plugin::proto::Node;

/// Glyph cell width in pixels.
const GLYPH_W: usize = 5;
/// Glyph cell height in pixels.
const GLYPH_H: usize = 7;
/// Gap between adjacent glyphs on a line.
const SPACING: usize = 1;
/// Gap between wrapped lines.
const LINE_GAP: usize = 2;
/// Transparent padding around the text block, inside the bubble background.
const PAD: usize = 3;
/// Radius of the (chunky) rounded-corner cut on the baked bubble background.
const CORNER: usize = 2;
/// Integer pixel-scale baked into the bubble buffer (#323 — bigger pixel font,
/// multiline). A `Row` packs the bubble at its **natural** buffer width (1×), so
/// the host does no upscaling of its own — bigger chunky pixels have to come from
/// the buffer itself. [`render`] draws the glyphs at 1× then nearest-neighbor
/// upscales the whole buffer by this factor, so each `5×7` glyph paints as a
/// crisp `5·SCALE × 7·SCALE` block.
const SCALE: usize = 2;

/// Target **on-screen** pixel width of the bubble's slot beside the face in the
/// compact row (#313). The sidebar card is 320 px; inside its ~12 px padding
/// (~296 px) the 128 px LCD face plus its bezel/border eats ~143 px, so this is
/// roughly the width left for the bubble. The buffer is packed at 1× in the row,
/// so the **pre-scale** buffer must fit `BUBBLE_SLOT_PX / SCALE` and the ×[`SCALE`]
/// upscale lands it back near this on-screen width.
const BUBBLE_SLOT_PX: usize = 126;

/// Wrap width, in glyph cells: the widest line whose *pre-scale* buffer — text
/// plus the two [`PAD`] borders — still fits `BUBBLE_SLOT_PX / SCALE`, so after
/// the ×[`SCALE`] upscale the bubble stays within its slot beside the face. The
/// narrower cell count (vs. an unscaled bubble) is deliberate: it wraps the pet's
/// lines *down* into [`MAX_LINES`] chunky rows instead of growing sideways.
const MAX_COLS: usize = max_cols_for(BUBBLE_SLOT_PX / SCALE);

/// The largest column count whose rendered line width (both [`PAD`] borders
/// included) fits `slot_px` — the inverse of [`line_px`] with the padding
/// removed. `const` so [`MAX_COLS`] stays a compile-time constant; clamped to at
/// least 1 so [`wrap`] always has a positive width to break against.
const fn max_cols_for(slot_px: usize) -> usize {
    // Invert `line_px`: cols*GLYPH_W + (cols-1)*SPACING ≤ slot_px - 2*PAD
    //   ⇒ cols ≤ (content + SPACING) / (GLYPH_W + SPACING).
    let content = slot_px.saturating_sub(2 * PAD);
    let cols = (content + SPACING) / (GLYPH_W + SPACING);
    if cols == 0 { 1 } else { cols }
}
/// Hard cap on wrapped lines; an overflow is truncated with a trailing `…`.
const MAX_LINES: usize = 3;

/// Bubble background: a lilac a touch lighter than the face's screen field
/// (`SCREEN_BG` in `face.rs`), opaque. Corners are cut to transparent.
const BUBBLE_BG: [u8; 4] = [0x3a, 0x22, 0x50, 0xff];
/// Text ink: bright lilac, matching the face's fur/highlight family.
const INK: [u8; 4] = [0xf0, 0xe0, 0xf8, 0xff];
/// The `.notdef` box for an uncovered char — a dim lilac outline.
const NOTDEF: [u8; 4] = [0x6c, 0x4e, 0x86, 0xff];
/// Fully transparent (the letterbox/corner backdrop).
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Render `line` into a chunky-pixel speech bubble [`Node::Pixels`]. The buffer
/// is a **fixed-width** slot (background baked in, transparent rounded corners),
/// upscaled ×[`SCALE`] for the 8-bit look — so it no longer resizes as the
/// message length changes (#323); longer text wraps down into more rows instead.
pub(crate) fn bubble_node(line: &str, id: &str, classes: Vec<String>) -> Node {
    let (width, height, data) = render(line);
    Node::Pixels {
        id: Some(id.to_owned()),
        width,
        height,
        data,
        classes,
    }
}

/// Render `text` into a `(width, height, RGBA8)` bubble buffer. The buffer
/// satisfies the host's `len == width * height * 4` invariant for every input
/// (including the empty string).
///
/// The width is **fixed** at the full [`MAX_COLS`] slot regardless of the text,
/// so the bubble stays a constant on-screen width per message (#323 — it used to
/// hug the text and jump around on every poke). Only the height varies, with the
/// wrapped line count. The whole thing is nearest-neighbor–upscaled ×[`SCALE`]
/// last, since the row packs it at 1× (see [`SCALE`]).
fn render(text: &str) -> (u32, u32, Vec<u8>) {
    let lines = wrap(text, MAX_COLS, MAX_LINES);
    // Fixed slot width — the full MAX_COLS line — so the bubble doesn't resize
    // with the message. Every wrapped line is ≤ MAX_COLS, so text always fits.
    let content_w = line_px(MAX_COLS);
    let n_lines = lines.len().max(1);
    let buf_w = (2 * PAD + content_w).max(1);
    let buf_h = 2 * PAD + n_lines * GLYPH_H + (n_lines - 1) * LINE_GAP;

    let mut buf = vec![0u8; buf_w * buf_h * 4];
    fill_background(&mut buf, buf_w, buf_h);
    for (row, line) in lines.iter().enumerate() {
        let oy = PAD + row * (GLYPH_H + LINE_GAP);
        for (col, ch) in line.chars().enumerate() {
            let ox = PAD + col * (GLYPH_W + SPACING);
            blit_glyph(&mut buf, buf_w, buf_h, ox, oy, ch);
        }
    }

    let (buf_w, buf_h, buf) = upscale(&buf, buf_w, buf_h, SCALE);
    (
        u32::try_from(buf_w).unwrap_or(0),
        u32::try_from(buf_h).unwrap_or(0),
        buf,
    )
}

/// Nearest-neighbor–upscale an RGBA8 buffer by an integer `factor`, replicating
/// each source pixel into a `factor × factor` block. `factor <= 1` is a copy.
/// Preserves the `len == w * h * 4` invariant and every exact pixel value.
fn upscale(src: &[u8], w: usize, h: usize, factor: usize) -> (usize, usize, Vec<u8>) {
    if factor <= 1 {
        return (w, h, src.to_vec());
    }
    let (dw, dh) = (w * factor, h * factor);
    let mut dst = vec![0u8; dw * dh * 4];
    for y in 0..h {
        for x in 0..w {
            let src_px = &src[(y * w + x) * 4..(y * w + x) * 4 + 4];
            for sy in 0..factor {
                for sx in 0..factor {
                    let didx = ((y * factor + sy) * dw + (x * factor + sx)) * 4;
                    dst[didx..didx + 4].copy_from_slice(src_px);
                }
            }
        }
    }
    (dw, dh, dst)
}

/// Pixel width of a line of `cols` glyphs (`0` for an empty line).
fn line_px(cols: usize) -> usize {
    if cols == 0 {
        0
    } else {
        cols * GLYPH_W + (cols - 1) * SPACING
    }
}

/// Greedy word-wrap `text` to at most `max_cols` glyph cells per line, breaking
/// a word longer than a full line across lines. Always returns at least one
/// line (the empty string yields a single empty line). The result is capped at
/// `max_lines`; an overflow drops the tail and marks the last kept line with a
/// trailing `…`.
fn wrap(text: &str, max_cols: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_cols = 0usize;

    for word in text.split_whitespace() {
        let mut chars: Vec<char> = word.chars().collect();
        // Hard-break a word wider than a whole line.
        while chars.len() > max_cols {
            if cur_cols > 0 {
                lines.push(std::mem::take(&mut cur));
                cur_cols = 0;
            }
            lines.push(chars[..max_cols].iter().collect());
            chars.drain(..max_cols);
        }
        let wlen = chars.len();
        if wlen == 0 {
            continue;
        }
        let with_word = if cur_cols == 0 {
            wlen
        } else {
            cur_cols + 1 + wlen
        };
        if with_word > max_cols && cur_cols > 0 {
            lines.push(std::mem::take(&mut cur));
            cur_cols = 0;
        }
        if cur_cols > 0 {
            cur.push(' ');
            cur_cols += 1;
        }
        cur.extend(&chars);
        cur_cols += wlen;
    }
    if cur_cols > 0 || lines.is_empty() {
        lines.push(cur);
    }

    if lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            // The `…` must fit within max_cols too: if the kept line is already
            // full, drop a char to make room, so the marked line never overruns
            // the (now fixed-width) slot.
            if last.chars().count() >= max_cols {
                *last = last.chars().take(max_cols.saturating_sub(1)).collect();
            }
            last.push('…');
        }
    }
    lines
}

/// Paint the opaque bubble field with (chunky) rounded corners cut to
/// transparent, so the upscaled buffer reads as a soft speech bubble.
fn fill_background(buf: &mut [u8], width: usize, height: usize) {
    let (last_x, last_y) = (width - 1, height - 1);
    for y in 0..height {
        for x in 0..width {
            let dx = corner_delta(x, last_x);
            let dy = corner_delta(y, last_y);
            let col = if dx * dx + dy * dy <= CORNER * CORNER {
                BUBBLE_BG
            } else {
                CLEAR
            };
            put(buf, width, height, x, y, col);
        }
    }
}

/// Distance a coordinate `v` pokes into the [`CORNER`] margin at either end of a
/// `0..=last` span (`0` in the middle) — the rounded-corner test in
/// [`fill_background`].
fn corner_delta(v: usize, last: usize) -> usize {
    CORNER
        .saturating_sub(v)
        .max((v + CORNER).saturating_sub(last))
}

/// Blit one glyph at cell origin (`ox`, `oy`); an uncovered char draws the dim
/// `.notdef` outline box instead.
fn blit_glyph(buf: &mut [u8], width: usize, height: usize, ox: usize, oy: usize, ch: char) {
    if let Some(rows) = glyph(ch) {
        for (ry, &bits) in rows.iter().enumerate() {
            for cx in 0..GLYPH_W {
                if (bits >> (GLYPH_W - 1 - cx)) & 1 == 1 {
                    put(buf, width, height, ox + cx, oy + ry, INK);
                }
            }
        }
    } else {
        for cx in 0..GLYPH_W {
            put(buf, width, height, ox + cx, oy, NOTDEF);
            put(buf, width, height, ox + cx, oy + GLYPH_H - 1, NOTDEF);
        }
        for ry in 0..GLYPH_H {
            put(buf, width, height, ox, oy + ry, NOTDEF);
            put(buf, width, height, ox + GLYPH_W - 1, oy + ry, NOTDEF);
        }
    }
}

/// Set one RGBA pixel, silently clipping out-of-bounds (edge glyphs never
/// panic).
fn put(buf: &mut [u8], width: usize, height: usize, x: usize, y: usize, rgba: [u8; 4]) {
    if x >= width || y >= height {
        return;
    }
    let idx = (y * width + x) * 4;
    buf[idx..idx + 4].copy_from_slice(&rgba);
}

/// The 5×7 bitmap for `c`, or `None` (→ the `.notdef` box) for an uncovered
/// char. See the module docs for the bit layout.
// A font is data: one match arm per glyph, so the length is inherent (and
// rustfmt spreads each 7-byte bitmap across lines).
#[allow(clippy::too_many_lines)]
fn glyph(c: char) -> Option<&'static [u8; GLYPH_H]> {
    if !c.is_ascii() {
        return glyph_extra(c);
    }
    Some(match c {
        ' ' => &[0, 0, 0, 0, 0, 0, 0],
        '!' => &[
            0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00000, 0b00100,
        ],
        '"' => &[
            0b10100, 0b10100, 0b10100, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        '\'' => &[
            0b00100, 0b00100, 0b01000, 0b00000, 0b00000, 0b00000, 0b00000,
        ],
        '(' => &[
            0b00010, 0b00100, 0b01000, 0b01000, 0b01000, 0b00100, 0b00010,
        ],
        ')' => &[
            0b01000, 0b00100, 0b00010, 0b00010, 0b00010, 0b00100, 0b01000,
        ],
        ',' => &[
            0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b00100, 0b01000,
        ],
        '-' => &[
            0b00000, 0b00000, 0b00000, 0b01110, 0b00000, 0b00000, 0b00000,
        ],
        '.' => &[
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b01100, 0b01100,
        ],
        '/' => &[
            0b00001, 0b00010, 0b00100, 0b00100, 0b00100, 0b01000, 0b10000,
        ],
        ':' => &[
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b01100, 0b00000,
        ],
        ';' => &[
            0b00000, 0b01100, 0b01100, 0b00000, 0b01100, 0b00100, 0b01000,
        ],
        '?' => &[
            0b01110, 0b10001, 0b00001, 0b00110, 0b00100, 0b00000, 0b00100,
        ],
        '~' => &[
            0b00000, 0b00000, 0b01101, 0b10110, 0b00000, 0b00000, 0b00000,
        ],
        '0' => &[
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => &[
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => &[
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => &[
            0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110,
        ],
        '4' => &[
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => &[
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => &[
            0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => &[
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => &[
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => &[
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100,
        ],
        'A' => &[
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => &[
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => &[
            0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100,
        ],
        'E' => &[
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => &[
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => &[
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111,
        ],
        'H' => &[
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => &[
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'J' => &[
            0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100,
        ],
        'K' => &[
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => &[
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => &[
            0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001,
        ],
        'N' => &[
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => &[
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => &[
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => &[
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => &[
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => &[
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => &[
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => &[
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => &[
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001,
        ],
        'X' => &[
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => &[
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => &[
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        'a' => &[
            0b00000, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
        ],
        'b' => &[
            0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'c' => &[
            0b00000, 0b00000, 0b01110, 0b10001, 0b10000, 0b10001, 0b01110,
        ],
        'd' => &[
            0b00001, 0b00001, 0b01111, 0b10001, 0b10001, 0b10001, 0b01111,
        ],
        'e' => &[
            0b00000, 0b00000, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110,
        ],
        'f' => &[
            0b00110, 0b01001, 0b01000, 0b11100, 0b01000, 0b01000, 0b01000,
        ],
        'g' => &[
            0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
        ],
        'h' => &[
            0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001,
        ],
        'i' => &[
            0b00100, 0b00000, 0b01100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'j' => &[
            0b00010, 0b00000, 0b00110, 0b00010, 0b00010, 0b10010, 0b01100,
        ],
        'k' => &[
            0b10000, 0b10000, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010,
        ],
        'l' => &[
            0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'm' => &[
            0b00000, 0b00000, 0b11010, 0b10101, 0b10101, 0b10001, 0b10001,
        ],
        'n' => &[
            0b00000, 0b00000, 0b11110, 0b10001, 0b10001, 0b10001, 0b10001,
        ],
        'o' => &[
            0b00000, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'p' => &[
            0b00000, 0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000,
        ],
        'q' => &[
            0b00000, 0b01111, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001,
        ],
        'r' => &[
            0b00000, 0b00000, 0b10110, 0b11001, 0b10000, 0b10000, 0b10000,
        ],
        's' => &[
            0b00000, 0b00000, 0b01111, 0b10000, 0b01110, 0b00001, 0b11110,
        ],
        't' => &[
            0b01000, 0b01000, 0b11100, 0b01000, 0b01000, 0b01001, 0b00110,
        ],
        'u' => &[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b10001, 0b01111,
        ],
        'v' => &[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'w' => &[
            0b00000, 0b00000, 0b10001, 0b10001, 0b10101, 0b10101, 0b01010,
        ],
        'x' => &[
            0b00000, 0b00000, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001,
        ],
        'y' => &[
            0b00000, 0b10001, 0b10001, 0b10001, 0b01111, 0b00001, 0b01110,
        ],
        'z' => &[
            0b00000, 0b00000, 0b11111, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        _ => return None,
    })
}

/// Non-ASCII glyphs the pet's persona realistically emits: German/Swedish
/// accents. Compact approximations (see the module's Coverage note).
fn glyph_extra(c: char) -> Option<&'static [u8; GLYPH_H]> {
    Some(match c {
        // Horizontal ellipsis (U+2026) — the pet's canned lines and the wrap
        // overflow marker both use it; non-ASCII, so it lives here.
        '…' => &[
            0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b10101, 0b10101,
        ],
        'ä' => &[
            0b01010, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
        ],
        'ö' => &[
            0b01010, 0b00000, 0b01110, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'ü' => &[
            0b01010, 0b00000, 0b10001, 0b10001, 0b10001, 0b10001, 0b01111,
        ],
        'å' => &[
            0b00100, 0b00000, 0b01110, 0b00001, 0b01111, 0b10001, 0b01111,
        ],
        'é' => &[
            0b00010, 0b00100, 0b01110, 0b10001, 0b11111, 0b10000, 0b01110,
        ],
        'Ä' => &[
            0b01010, 0b00000, 0b01110, 0b10001, 0b11111, 0b10001, 0b10001,
        ],
        // Ö/Ü use squared bodies so they read as uppercase and stay visually
        // (and bitwise) distinct from their lowercase ö/ü at this size.
        'Ö' => &[
            0b01010, 0b00000, 0b11111, 0b10001, 0b10001, 0b10001, 0b11111,
        ],
        'Ü' => &[
            0b01010, 0b00000, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
        ],
        'Å' => &[
            0b00100, 0b00000, 0b01110, 0b10001, 0b11111, 0b10001, 0b10001,
        ],
        'ß' => &[
            0b01100, 0b10010, 0b10010, 0b10100, 0b10010, 0b10010, 0b10100,
        ],
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{GLYPH_H, MAX_COLS, MAX_LINES, glyph, line_px, render, wrap};
    use crate::brain::{self, ThinkKind, ThinkReq};

    /// The buffer invariant the host enforces: `len == width * height * 4`, for
    /// every canned line and every mood word, plus the accent/box torture set.
    #[test]
    fn every_bubble_buffer_satisfies_the_host_invariant() {
        let mut lines: Vec<String> = Vec::new();
        // Sweep the brain's canned pools through every context.
        for kind in [ThinkKind::Poke, ThinkKind::Idle] {
            for pokes in [0, 1, crate::GRUMPY_AT] {
                for mood in ["happy", "sleepy", "excited", "grumpy", "thinking"] {
                    let req = ThinkReq {
                        kind,
                        hour: 2,
                        mood,
                        pokes,
                    };
                    for step in 0..8 {
                        lines.push(brain::canned(req, step));
                    }
                }
            }
        }
        // Accents that survive sanitize, an emoji that must box-fallback, and
        // the pathological inputs.
        lines.push("smörgåsbord ölçäÜ é ßÅÄÖ".to_owned());
        lines.push("💕 unmapped: ☺ \u{1F63A}".to_owned());
        lines.push(String::new());
        lines.push("a".repeat(80));
        lines.push("supercalifragilisticexpialidocious".to_owned());

        for line in &lines {
            let (w, h, data) = render(line);
            assert_eq!(
                data.len(),
                w as usize * h as usize * 4,
                "buffer for {line:?} must be w*h*4"
            );
            assert!(w > 0 && h > 0, "buffer for {line:?} is non-degenerate");
        }
    }

    /// Every char the font claims to cover has a real (non-`None`) glyph.
    #[test]
    fn covered_chars_have_glyphs() {
        let covered = "abcdefghijklmnopqrstuvwxyz\
                       ABCDEFGHIJKLMNOPQRSTUVWXYZ\
                       0123456789 !?.,:;'\"()-~/…åäöÅÄÖüÜßé";
        for c in covered.chars() {
            assert!(glyph(c).is_some(), "expected a glyph for {c:?}");
        }
    }

    /// An uncovered char (e.g. the heart emoji) box-falls-back rather than
    /// mapping to a glyph — and rendering it never panics.
    #[test]
    fn uncovered_chars_fall_back_to_the_box() {
        assert!(glyph('💕').is_none());
        assert!(glyph('☺').is_none());
        let (_, _, data) = render("💕");
        assert!(!data.is_empty());
    }

    /// A known short string renders a deterministic, non-empty buffer with ink.
    #[test]
    fn a_known_string_renders_deterministically_with_ink() {
        let a = render("mrrp!");
        let b = render("mrrp!");
        assert_eq!(a, b, "render is pure");
        // Some pixel is opaque ink (alpha 0xff on a non-background pixel).
        let (_, _, data) = a;
        assert!(
            data.chunks_exact(4).any(|px| px == super::INK),
            "the text draws ink pixels"
        );
    }

    #[test]
    fn line_px_is_zero_for_empty_and_grows_by_advance() {
        assert_eq!(line_px(0), 0);
        assert_eq!(line_px(1), 5); // one glyph, no trailing gap
        assert_eq!(line_px(2), 11); // 5 + 1 + 5
        assert_eq!(line_px(3), 17); // 5 + 1 + 5 + 1 + 5
    }

    #[test]
    fn wrap_empty_string_is_one_empty_line() {
        assert_eq!(wrap("", MAX_COLS, MAX_LINES), vec![String::new()]);
    }

    #[test]
    fn wrap_keeps_an_exactly_full_line_intact() {
        // A word of exactly MAX_COLS cells is a full line: it must fill line 0
        // without spilling early, and the following word drops to line 1.
        let full = "w".repeat(MAX_COLS);
        let lines = wrap(&format!("{full} tail"), MAX_COLS, MAX_LINES);
        assert_eq!(lines[0], full);
        assert_eq!(lines[1], "tail");
    }

    #[test]
    fn wrap_hard_breaks_an_overlong_word() {
        let lines = wrap("supercalifragilisticexpialidocious", MAX_COLS, MAX_LINES);
        assert!(lines.len() >= 2, "an overlong word breaks across lines");
        assert!(
            lines.iter().all(|l| l.chars().count() <= MAX_COLS),
            "no wrapped line exceeds the width: {lines:?}"
        );
    }

    #[test]
    fn wrap_caps_lines_and_marks_the_overflow() {
        // Force more than MAX_LINES lines of single chars.
        let text = "a b c d e f g h";
        let lines = wrap(text, 1, MAX_LINES);
        assert_eq!(lines.len(), MAX_LINES);
        assert!(
            lines.last().unwrap().ends_with('…'),
            "the truncated tail is marked"
        );
    }

    /// Rendering a long line wraps to multiple rows — the buffer grows taller
    /// than a single glyph line.
    #[test]
    fn a_wrapped_line_is_taller_than_one_row() {
        let (_, one_h, _) = render("hi");
        let (_, many_h, _) = render(&"word ".repeat(12));
        assert!(
            many_h > one_h,
            "wrapped text is taller ({many_h} > {one_h})"
        );
        assert!(one_h >= u32::try_from(GLYPH_H).unwrap());
    }
}
