//! Text rules a host and its plugins must agree on byte for byte.

/// Whether `c` is invisible, moves text between lines, or reorders how the
/// rest displays — the shared rule for which characters a host drops from, or
/// refuses in, plugin-supplied text.
///
/// `std` has no general-category API, so the non-`Cc` classes are listed by
/// code point — the `Cf` characters a display string plausibly meets, plus
/// `Zl`/`Zp`:
///
/// - `Cc` — [`char::is_control`]: C0/C1 controls, incl. `\n`, `\r`, `ESC` and
///   `U+0085` NEL.
/// - `Zl`/`Zp` — `U+2028` LINE SEPARATOR, `U+2029` PARAGRAPH SEPARATOR.
/// - bidi — `U+061C` ALM, `U+200E`/`U+200F` LRM/RLM, `U+202A`–`U+202E`
///   embeddings/overrides, `U+2066`–`U+2069` isolates.
/// - zero-width / invisible — `U+00AD` soft hyphen, `U+180E`, `U+200B`–`U+200D`
///   (ZWSP/ZWNJ/ZWJ), `U+2060`–`U+2064` (word joiner, invisible operators),
///   `U+206A`–`U+206F` (deprecated format controls), `U+FEFF` (BOM/ZWNBSP),
///   `U+FFF9`–`U+FFFB` (interlinear annotation).
#[must_use]
pub fn is_invisible_or_control(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

#[cfg(test)]
mod tests {
    use super::is_invisible_or_control;

    /// Every listed range (inclusive), pinned independently of the function's
    /// own `matches!`.
    const RANGES: &[(char, char)] = &[
        ('\u{00AD}', '\u{00AD}'),
        ('\u{061C}', '\u{061C}'),
        ('\u{180E}', '\u{180E}'),
        ('\u{200B}', '\u{200F}'),
        ('\u{2028}', '\u{202E}'),
        ('\u{2060}', '\u{2064}'),
        ('\u{2066}', '\u{206F}'),
        ('\u{FEFF}', '\u{FEFF}'),
        ('\u{FFF9}', '\u{FFFB}'),
    ];

    fn neighbour(c: char, up: bool) -> char {
        let u = u32::from(c);
        char::from_u32(if up { u + 1 } else { u - 1 }).expect("scalar value")
    }

    fn in_a_range(c: char) -> bool {
        RANGES.iter().any(|&(lo, hi)| (lo..=hi).contains(&c))
    }

    #[test]
    fn every_listed_range_is_dropped_throughout() {
        for &(lo, hi) in RANGES {
            for u in u32::from(lo)..=u32::from(hi) {
                let c = char::from_u32(u).expect("scalar value");
                assert!(is_invisible_or_control(c), "U+{u:04X} must be dropped");
            }
        }
    }

    #[test]
    fn the_code_points_just_outside_each_range_are_kept() {
        for &(lo, hi) in RANGES {
            for c in [neighbour(lo, false), neighbour(hi, true)] {
                // A neighbour that is a control or sits in another listed
                // range is legitimately dropped; every other one is kept.
                let expected = c.is_control() || in_a_range(c);
                assert_eq!(
                    is_invisible_or_control(c),
                    expected,
                    "U+{:04X} next to U+{:04X}..=U+{:04X}",
                    u32::from(c),
                    u32::from(lo),
                    u32::from(hi)
                );
            }
        }
        // The neighbours that are genuinely outside every class, by name.
        for c in [
            '\u{00AC}', '\u{00AE}', '\u{061B}', '\u{061D}', '\u{200A}', '\u{2010}', '\u{205F}',
            '\u{2065}', '\u{FEFE}', '\u{FFF8}', '\u{FFFC}',
        ] {
            assert!(
                !is_invisible_or_control(c),
                "U+{:04X} must be kept",
                u32::from(c)
            );
        }
    }

    #[test]
    fn controls_are_dropped() {
        for c in [
            '\0', '\n', '\r', '\t', '\u{1B}', '\u{7F}', '\u{85}', '\u{9F}',
        ] {
            assert!(is_invisible_or_control(c), "U+{:04X}", u32::from(c));
        }
    }

    #[test]
    fn ordinary_characters_are_kept() {
        for c in ['a', 'Z', '0', ' ', 'é', '字', '😀', '\u{00A0}'] {
            assert!(!is_invisible_or_control(c), "U+{:04X}", u32::from(c));
        }
    }
}
