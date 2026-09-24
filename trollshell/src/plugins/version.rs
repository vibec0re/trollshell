//! The host's handling of a plugin's self-declared release version (#887).
//!
//! [`Manifest::version`](hytte_plugin_proto::Manifest::version) is whatever
//! the connected process put in its `Register` frame, so it is **untrusted
//! display text**: the host never parses or compares it, and the only thing it
//! does with it is show it in the control-center's Plugins tab (via
//! `Control.ListPluginVersions`). [`sanitize`] is the one gate it passes
//! through on the way into the runtime mirror, applied once at registration.

/// The longest version string the host keeps, in `char`s. Generous for any
/// semver (`0.4.1-rc.2+nixos.abc1234` is 24) while keeping a hostile plugin from
/// widening a settings row with a megabyte of text.
pub(super) const MAX_VERSION_CHARS: usize = 64;

/// Reduce a plugin-declared version to something safe to display, or `None`.
///
/// - Every character [`is_dropped`] names is **removed**, so the text cannot
///   break a row across lines (controls, and the `Zl`/`Zp` separators Pango
///   treats as paragraph breaks), smuggle a terminal escape into a journal
///   line, or display differently from its bytes (bidi overrides/isolates and
///   zero-width format characters) — #1397 review M1.
/// - Surrounding whitespace is trimmed.
/// - The result is capped at [`MAX_VERSION_CHARS`] `char`s (never mid-`char`);
///   an over-long value ends in `…` so the truncation is visible rather than
///   passing for the real version. The cap counts `char`s, not graphemes, so
///   it can split a ZWJ emoji or a combining stack at the cut — cosmetic, and
///   accepted for a 64-char display string.
/// - An absent value, or one that is empty once those characters are gone and
///   the ends trimmed (all-control, whitespace-only, zero-width-only), is
///   `None`, which the control-center renders as "—" exactly like a pre-#887
///   plugin that declared nothing — never a blank cell.
#[must_use]
pub(super) fn sanitize(raw: Option<&str>) -> Option<String> {
    let cleaned: String = raw?.chars().filter(|&c| !is_dropped(c)).collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() <= MAX_VERSION_CHARS {
        return Some(trimmed.to_owned());
    }
    let mut capped: String = trimmed.chars().take(MAX_VERSION_CHARS - 1).collect();
    capped.push('…');
    Some(capped)
}

/// Whether [`sanitize`] drops `c`: anything that is invisible, moves text
/// between lines, or reorders how the rest displays.
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
fn is_dropped(c: char) -> bool {
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
    use super::{MAX_VERSION_CHARS, sanitize};

    /// `"1{c}.2"` for each `c` must come back as `"1.2"`.
    fn assert_each_stripped(chars: impl IntoIterator<Item = char>) {
        for c in chars {
            assert_eq!(
                sanitize(Some(&format!("1{c}.2"))).as_deref(),
                Some("1.2"),
                "U+{:04X} must be stripped",
                u32::from(c),
            );
        }
    }

    // ── #1397 review M1: the non-`Cc` classes ────────────────────────────

    #[test]
    fn line_and_paragraph_separators_are_stripped() {
        assert_eq!(
            sanitize(Some("1.0\u{2028}\u{2029}.1")).as_deref(),
            Some("1.0.1")
        );
    }

    #[test]
    fn bidi_and_zero_width_format_chars_are_stripped() {
        assert_eq!(
            sanitize(Some("1.0.0\u{202e}9\u{2066}")).as_deref(),
            Some("1.0.09")
        );
        assert_eq!(
            sanitize(Some("\u{200b}\u{feff}\u{200d}")),
            None,
            "invisible-only reads as —"
        );
    }

    #[test]
    fn bidi_embeddings_and_overrides_are_stripped() {
        assert_each_stripped('\u{202A}'..='\u{202E}');
    }

    #[test]
    fn bidi_isolates_are_stripped() {
        assert_each_stripped('\u{2066}'..='\u{2069}');
    }

    #[test]
    fn directional_marks_are_stripped() {
        assert_each_stripped(['\u{200E}', '\u{200F}', '\u{061C}']);
    }

    #[test]
    fn zero_width_and_invisible_format_chars_are_stripped() {
        assert_each_stripped([
            '\u{00AD}', '\u{180E}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{2064}',
            '\u{206A}', '\u{206F}', '\u{FEFF}', '\u{FFF9}', '\u{FFFB}',
        ]);
    }

    #[test]
    fn whitespace_only_including_unicode_spaces_is_none() {
        // NBSP, ideographic space, em space: Unicode `White_Space`, trimmed.
        assert_eq!(sanitize(Some(" \u{00A0}\u{3000}\u{2003} ")), None);
        // Whitespace and zero-width mixed is still nothing to show.
        assert_eq!(sanitize(Some(" \u{200B} \u{FEFF} ")), None);
    }

    #[test]
    fn interior_spaces_and_ordinary_text_survive() {
        assert_eq!(
            sanitize(Some("1.0 beta (nixos)")).as_deref(),
            Some("1.0 beta (nixos)")
        );
    }

    #[test]
    fn a_plain_semver_passes_through_unchanged() {
        assert_eq!(sanitize(Some("0.4.1")).as_deref(), Some("0.4.1"));
        assert_eq!(
            sanitize(Some("1.0.0-rc.2+build.7")).as_deref(),
            Some("1.0.0-rc.2+build.7"),
        );
    }

    #[test]
    fn absent_empty_and_all_control_values_are_none() {
        assert_eq!(sanitize(None), None);
        assert_eq!(sanitize(Some("")), None);
        assert_eq!(sanitize(Some("   ")), None);
        assert_eq!(sanitize(Some("\n\t\u{1b}\u{7f}")), None);
    }

    #[test]
    fn control_characters_are_stripped_not_kept() {
        // A newline would split the row; an ESC would start a terminal escape
        // in any journal line that quotes the value; a C1 control is the same
        // hazard one byte later.
        assert_eq!(
            sanitize(Some("0.4\n.1\u{1b}[31m\u{85}")).as_deref(),
            Some("0.4.1[31m"),
        );
        assert_eq!(sanitize(Some("\t 1.2.3 \r\n")).as_deref(), Some("1.2.3"));
    }

    #[test]
    fn an_over_long_value_is_capped_with_a_visible_ellipsis() {
        let exact = "x".repeat(MAX_VERSION_CHARS);
        assert_eq!(sanitize(Some(&exact)).as_deref(), Some(exact.as_str()));

        let long = "y".repeat(MAX_VERSION_CHARS * 100);
        let capped = sanitize(Some(&long)).expect("a long value is kept, capped");
        assert_eq!(capped.chars().count(), MAX_VERSION_CHARS);
        assert!(capped.ends_with('…'), "the cut is visible: {capped:?}");
    }

    #[test]
    fn the_cap_counts_chars_and_never_splits_one() {
        // Multi-byte chars: a byte-indexed cut would panic or split a scalar.
        let long = "é".repeat(MAX_VERSION_CHARS + 5);
        let capped = sanitize(Some(&long)).expect("kept");
        assert_eq!(capped.chars().count(), MAX_VERSION_CHARS);
        assert!(capped.starts_with('é') && capped.ends_with('…'));
    }
}
