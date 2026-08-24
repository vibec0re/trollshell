//! Pango-markup safety for libadwaita rows (#753, #30).
//!
//! `AdwPreferencesRow:use-markup` defaults to **`TRUE`**, so every
//! `AdwActionRow` / `AdwExpanderRow` renders its *title and subtitle* as Pango
//! markup. A runtime string carrying `&`, `<` or `>` — a Bluetooth alias, a
//! mount path, a systemd unit description — then fails to parse and the field
//! renders **blank**, and a hostile string can inject markup outright. The
//! failure is silent: no warning, just an empty label on a row the user is
//! about to click.
//!
//! Two fixes; pick per call site rather than uniformly:
//!
//! - [`plain_text`] flips the property off. Prefer it when the row wants no
//!   markup at all: it covers title *and* subtitle in one call, and it keeps
//!   covering them when a later `set_title` / `set_subtitle` (typically from a
//!   `bind`) replaces the text — escaping only ever covers the one string you
//!   remembered to wrap.
//! - [`escape`] escapes a single string. Needed where markup *is* wanted
//!   elsewhere in the same row (as `widgets/calendar.rs` does), and it is the
//!   **only** option for `AdwPreferencesGroup`: its title and description
//!   labels are hardcoded `use-markup="True"` in libadwaita's
//!   `adw-preferences-group.ui` template and there is no property to flip.

use hytte::adw::{self, prelude::*};
use hytte::gtk::glib;

/// Render this row's title and subtitle literally instead of as Pango markup.
///
/// Order-independent: `GtkLabel` keeps the string it was handed and re-lays it
/// out when `use-markup` flips, so calling this *after* a builder already set
/// the title is fine.
pub(crate) fn plain_text(row: &impl IsA<adw::PreferencesRow>) {
    row.set_use_markup(false);
}

/// Escape `text` so Pango renders `&`, `<` and `>` literally.
///
/// For labels that have no `use-markup` property to turn off — notably
/// `AdwPreferencesGroup`'s title and description.
pub(crate) fn escape(text: &str) -> String {
    glib::markup_escape_text(text).into()
}

#[cfg(test)]
mod tests {
    use super::escape;

    /// The character that bites in practice: an ampersand anywhere in a
    /// title blanks the whole field.
    #[test]
    fn escapes_ampersand() {
        assert_eq!(escape("Cafe & Co"), "Cafe &amp; Co");
    }

    #[test]
    fn escapes_angle_brackets() {
        assert_eq!(escape("a < b"), "a &lt; b");
        assert_eq!(escape("b > a"), "b &gt; a");
    }

    /// The injection case: a full markup tag must survive as *text*, with no
    /// `<` or `>` left for Pango to treat as markup.
    #[test]
    fn neutralizes_a_span_tag() {
        let escaped = escape(r#"<span foreground="red">Free WiFi</span>"#);
        assert!(
            !escaped.contains('<') && !escaped.contains('>'),
            "markup delimiters survived escaping: {escaped}"
        );
        assert!(escaped.contains("Free WiFi"), "text was lost: {escaped}");
    }

    #[test]
    fn empty_stays_empty() {
        assert_eq!(escape(""), "");
    }

    /// Ordinary text must pass through byte-identical — an escaper that
    /// mangles the common case is worse than the bug.
    #[test]
    fn plain_text_is_unchanged() {
        for s in [
            "Living Room Speaker",
            "wlan0",
            "/home/annika",
            "nginx \u{00b7} pid 1234",
        ] {
            assert_eq!(escape(s), s);
        }
    }
}
