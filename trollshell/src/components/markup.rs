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
//! - `plain_text` flips the property off. Prefer it when the row wants no
//!   markup at all: it covers title *and* subtitle in one call, and it keeps
//!   covering them when a later `set_title` / `set_subtitle` (typically from a
//!   `bind`) replaces the text — escaping only ever covers the one string you
//!   remembered to wrap.
//! - `escape` escapes a single string. Needed where markup *is* wanted
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

/// What a row **displays**, for the `system-tests` markup assertions every
/// `plain_text` call site in the panels pins its fix with (#753's rule).
///
/// Asserted on the text the row's own `GtkLabel`s show — `Label::text()` is
/// the text *after* Pango has parsed any markup — rather than on the
/// `use-markup` flag, so a test pins the user-visible symptom: with markup on,
/// `"Tom & Jerry"` fails to parse and the label shows nothing, and
/// `"<b>hi</b>"` shows `hi`. Needs a display server to construct the rows.
#[cfg(all(test, feature = "system-tests"))]
pub(crate) mod test_support {
    use hytte::gtk::{self, prelude::*};

    /// Every `GtkLabel` text under `root` — a row's title/subtitle labels live
    /// in libadwaita's template, so walk for them rather than name them.
    pub(crate) fn label_texts(root: &gtk::Widget) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(w) = stack.pop() {
            if let Some(label) = w.downcast_ref::<gtk::Label>() {
                out.push(label.text().to_string());
            }
            let mut child = w.first_child();
            while let Some(c) = child {
                child = c.next_sibling();
                stack.push(c);
            }
        }
        out
    }

    /// Assert some label under `root` displays `literal` byte for byte.
    #[track_caller]
    pub(crate) fn assert_shows(root: &impl IsA<gtk::Widget>, literal: &str) {
        let texts = label_texts(root.upcast_ref());
        assert!(
            texts.iter().any(|t| t == literal),
            "no label displays {literal:?} literally — the row parsed it as Pango markup; \
             labels show {texts:?}"
        );
    }
}

/// `plain_text` itself, on each row type the panels hand runtime text to.
///
/// The negative control is what makes the positive half mean something: the
/// same row **without** the call must fail to show the string, or the
/// assertion could pass on a libadwaita that stopped defaulting `use-markup`
/// to `TRUE` and the call sites' tests would be pinning nothing.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    use super::plain_text;
    use super::test_support::{assert_shows, label_texts};

    /// `&` is a parse error (the field blanks); `<b>` is obeyed (the tag
    /// vanishes). One string carries both failure modes.
    const HOSTILE: &str = "Tom & <b>Jerry</b>";

    fn shows(root: &impl IsA<gtk::Widget>, literal: &str) -> bool {
        label_texts(root.upcast_ref()).iter().any(|t| t == literal)
    }

    #[gtk::test]
    fn an_action_row_shows_title_and_subtitle_literally() {
        adw::init().expect("libadwaita init");
        let row = adw::ActionRow::builder()
            .title(HOSTILE)
            .subtitle(HOSTILE)
            .build();
        assert!(
            !shows(&row, HOSTILE),
            "premise: libadwaita parses an ActionRow's title as markup by default"
        );
        plain_text(&row);
        assert_shows(&row, HOSTILE);
        assert!(!row.uses_markup());
    }

    /// The order-independence the doc above promises: a `set_subtitle` that
    /// lands *after* the call (a `bind` re-firing) is covered too.
    #[gtk::test]
    fn a_switch_row_covers_a_subtitle_set_after_the_call() {
        adw::init().expect("libadwaita init");
        let row = adw::SwitchRow::builder().title("Keep awake").build();
        plain_text(&row);
        row.set_subtitle(HOSTILE);
        assert_shows(&row, HOSTILE);
    }

    #[gtk::test]
    fn an_expander_row_shows_its_title_literally() {
        adw::init().expect("libadwaita init");
        let row = adw::ExpanderRow::builder().title(HOSTILE).build();
        plain_text(&row);
        assert_shows(&row, HOSTILE);
    }
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
