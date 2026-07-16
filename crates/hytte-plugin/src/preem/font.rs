//! The kit's hand-rolled **5×7 pixel font**: glyph bitmaps, metrics, and the
//! word-wrap every text widget shares.
//!
//! Promoted verbatim from the pet's speech bubble (#304 → #356): the pet
//! rendered whole text lines into a `Pixels` strip with exactly this data —
//! it *was* the 8bit textbox, just landlocked `pub(crate)` in one plugin.
//! [`TextBox`](super::TextBox) is the boxed renderer over this module;
//! [`dot_matrix`](super::dot_matrix) reuses the same bitmaps as dot grids.
//! No font files, no deps: every glyph is a `const` bitmap.
//!
//! # Grid
//!
//! Each glyph is a **5-wide × 7-tall** cell, stored as `[u8; 7]` — one byte
//! per row, top row first. Within a row the low 5 bits are the columns,
//! **bit 4 the leftmost** column and bit 0 the rightmost, so a binary
//! literal reads left-to-right as the pixels: `0b01110` is `.###.`. Glyphs
//! advance by `GLYPH_W + SPACING` px and lines by `GLYPH_H + LINE_GAP`.
//!
//! # Coverage
//!
//! Printable ASCII (letters both cases, digits, and common punctuation —
//! `! ? . , : ; ' " ( ) - ~ / …`) plus the accented set a German/Swedish
//! desktop produces: `å ä ö Å Ä Ö ü Ü ß é`. Any uncovered char (emoji
//! included) has no glyph — [`glyph`] returns `None` and renderers draw the
//! dim hollow [`NOTDEF`] box instead, never a panic. The accented uppercase
//! and `å/Å` glyphs are compact approximations — the ring on `å` is a single
//! dot, and the umlaut/ring uppercase bodies are squeezed into five rows to
//! leave room for the diacritic.

/// Glyph cell width in pixels.
pub const GLYPH_W: usize = 5;
/// Glyph cell height in pixels.
pub const GLYPH_H: usize = 7;
/// Gap between adjacent glyphs on a line.
pub const SPACING: usize = 1;
/// Gap between wrapped lines.
pub const LINE_GAP: usize = 2;

/// The `.notdef` fallback for an uncovered char: a hollow 5×7 outline box.
/// Renderers stamp it (usually dimmed) wherever [`glyph`] returns `None`.
pub const NOTDEF: [u8; GLYPH_H] = [
    0b11111, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11111,
];

/// Pixel width of a line of `cols` glyphs (`0` for an empty line).
#[must_use]
pub const fn line_px(cols: usize) -> usize {
    if cols == 0 {
        0
    } else {
        cols * GLYPH_W + (cols - 1) * SPACING
    }
}

/// The largest column count whose rendered line width fits `content_px` —
/// the inverse of [`line_px`]. Clamped to at least 1 so a wrap always has a
/// positive width to break against. `const` so callers can pin widths at
/// compile time.
#[must_use]
pub const fn max_cols_for(content_px: usize) -> usize {
    // Invert `line_px`: cols*GLYPH_W + (cols-1)*SPACING ≤ content_px
    //   ⇒ cols ≤ (content_px + SPACING) / (GLYPH_W + SPACING).
    let cols = (content_px + SPACING) / (GLYPH_W + SPACING);
    if cols == 0 { 1 } else { cols }
}

/// Greedy word-wrap `text` to at most `max_cols` glyph cells per line,
/// breaking a word longer than a full line across lines. Always returns at
/// least one line (the empty string yields a single empty line). The result
/// is capped at `max_lines`; an overflow drops the tail and marks the last
/// kept line with a trailing `…`.
#[must_use]
pub fn wrap(text: &str, max_cols: usize, max_lines: usize) -> Vec<String> {
    let max_cols = max_cols.max(1);
    let max_lines = max_lines.max(1);
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
            // The `…` must fit within max_cols too: if the kept line is
            // already full, drop a char to make room, so the marked line
            // never overruns a fixed-width slot.
            if last.chars().count() >= max_cols {
                *last = last.chars().take(max_cols.saturating_sub(1)).collect();
            }
            last.push('…');
        }
    }
    lines
}

/// The 5×7 bitmap for `c`, or `None` (→ the [`NOTDEF`] box) for an
/// uncovered char. See the module docs for the bit layout.
// A font is data: one match arm per glyph, so the length is inherent (and
// rustfmt spreads each 7-byte bitmap across lines).
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn glyph(c: char) -> Option<&'static [u8; GLYPH_H]> {
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

/// Non-ASCII glyphs: German/Swedish accents plus the ellipsis. Compact
/// approximations (see the module's Coverage note).
fn glyph_extra(c: char) -> Option<&'static [u8; GLYPH_H]> {
    Some(match c {
        // Horizontal ellipsis (U+2026) — also the wrap overflow marker;
        // non-ASCII, so it lives here.
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
    use super::{GLYPH_H, GLYPH_W, NOTDEF, glyph, line_px, max_cols_for, wrap};

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

    /// An uncovered char (e.g. an emoji) maps to no glyph — renderers then
    /// stamp the hollow `NOTDEF` box.
    #[test]
    fn uncovered_chars_have_no_glyph() {
        assert!(glyph('💕').is_none());
        assert!(glyph('☺').is_none());
        assert!(glyph('\u{7f}').is_none());
    }

    /// The `.notdef` box is a hollow outline: full top/bottom rows, edge
    /// columns in between — visibly "a box", never mistaken for a glyph.
    #[test]
    fn notdef_is_a_hollow_box() {
        assert_eq!(NOTDEF[0], 0b11111);
        assert_eq!(NOTDEF[GLYPH_H - 1], 0b11111);
        for row in &NOTDEF[1..GLYPH_H - 1] {
            assert_eq!(*row, 0b10001);
        }
    }

    #[test]
    fn line_px_is_zero_for_empty_and_grows_by_advance() {
        assert_eq!(line_px(0), 0);
        assert_eq!(line_px(1), 5); // one glyph, no trailing gap
        assert_eq!(line_px(2), 11); // 5 + 1 + 5
        assert_eq!(line_px(3), 17); // 5 + 1 + 5 + 1 + 5
    }

    /// `max_cols_for` inverts `line_px`: the widest line that fits is the
    /// one it says, and one more column would overflow.
    #[test]
    fn max_cols_for_inverts_line_px() {
        for cols in 1..12 {
            let px = line_px(cols);
            assert_eq!(max_cols_for(px), cols, "exact fit at {cols} cols");
            assert!(line_px(max_cols_for(px - 1) + 1) > px - 1);
        }
        assert_eq!(max_cols_for(0), 1, "clamped to at least one column");
        assert_eq!(GLYPH_W, 5, "metrics the tests above assume");
    }

    #[test]
    fn wrap_empty_string_is_one_empty_line() {
        assert_eq!(wrap("", 9, 3), vec![String::new()]);
    }

    #[test]
    fn wrap_keeps_an_exactly_full_line_intact() {
        // A word of exactly max_cols cells is a full line: it must fill
        // line 0 without spilling early, and the following word drops down.
        let full = "w".repeat(9);
        let lines = wrap(&format!("{full} tail"), 9, 3);
        assert_eq!(lines[0], full);
        assert_eq!(lines[1], "tail");
    }

    #[test]
    fn wrap_hard_breaks_an_overlong_word() {
        let lines = wrap("supercalifragilisticexpialidocious", 9, 5);
        assert!(lines.len() >= 2, "an overlong word breaks across lines");
        assert!(
            lines.iter().all(|l| l.chars().count() <= 9),
            "no wrapped line exceeds the width: {lines:?}"
        );
    }

    #[test]
    fn wrap_caps_lines_and_marks_the_overflow() {
        // Force more than max_lines lines of single chars.
        let lines = wrap("a b c d e f g h", 1, 3);
        assert_eq!(lines.len(), 3);
        assert!(
            lines.last().unwrap().ends_with('…'),
            "the truncated tail is marked"
        );
        assert!(
            lines.iter().all(|l| l.chars().count() <= 1),
            "the marker still fits the width"
        );
    }

    /// Degenerate wrap parameters are clamped, not panicked on.
    #[test]
    fn wrap_survives_zero_widths() {
        assert_eq!(wrap("hi there", 0, 0).len(), 1);
    }
}
