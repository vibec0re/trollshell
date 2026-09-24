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
/// - Every control character (`char::is_control` — newlines, tabs, escape
///   sequences' `ESC`, C1 controls) is **dropped**, so the text cannot break a
///   row across lines or smuggle a terminal escape into a journal line.
/// - Surrounding whitespace is trimmed.
/// - The result is capped at [`MAX_VERSION_CHARS`] `char`s (never mid-`char`);
///   an over-long value ends in `…` so the truncation is visible rather than
///   passing for the real version.
/// - An absent, empty or all-control value is `None`, which the control-center
///   renders as "—" exactly like a pre-#887 plugin that declared nothing.
#[must_use]
pub(super) fn sanitize(raw: Option<&str>) -> Option<String> {
    let cleaned: String = raw?.chars().filter(|c| !c.is_control()).collect();
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

#[cfg(test)]
mod tests {
    use super::{MAX_VERSION_CHARS, sanitize};

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
