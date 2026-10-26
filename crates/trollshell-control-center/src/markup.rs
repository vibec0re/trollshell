//! Pango-markup safety for the control-center's libadwaita widgets (#1564).
//!
//! The shell has the same module (`trollshell/src/components/markup.rs`, #1544);
//! this crate cannot link the shell, so it carries its own small copy of the
//! two tools. Read that module's doc for the full census; the short version:
//!
//! `AdwPreferencesRow:use-markup` and `AdwToast:use-markup` default to
//! **`TRUE`**, and `AdwPreferencesGroup`'s title and description labels are
//! hardcoded to markup with no property at all. Text we do not author (a place
//! name the user typed, a config key, a systemd error quoting the file, a place name the user typed, a D-Bus error, a plugin's
//! declared mount) containing `&`, `<` or `>` then fails to parse and renders
//! **blank**: the message meant to explain the problem disappears.
//!
//! - [`plain_text`] for rows: turns markup off. Call it **right after
//!   `new()`/`build()` and before the first non-literal text is set**: the
//!   label tries to parse a title the moment it is set, so setting text first
//!   logs a `Gtk-WARNING` on every build (measured in the #1544 review), and
//!   `.use_markup(false)` in a builder does not help.
//! - [`escape`] for everything with no working off switch: toast titles
//!   and group titles/descriptions. [`toast`] wraps the one toast sink so call
//!   sites cannot forget.
//!
//! **`AdwBanner` is not on that list.** Its `use-markup` is documented as
//! opt-in (default off, measured on libadwaita 1.9.3: a banner given `Tom &
//! <b>Jerry</b>` shows exactly that), so a banner title must NOT be escaped:
//! it would show `&amp;`. The #1564 report had this wrong; `main.rs`'s banner
//! test pins the measured behaviour.
//!
//! Do not escape text a row shows through [`plain_text`]: it would display the
//! entities (`&amp;`) literally.

use adw::prelude::*;
use gtk::glib;

/// Render this row's title and subtitle literally instead of as Pango markup.
pub(crate) fn plain_text(row: &impl IsA<adw::PreferencesRow>) {
    row.set_use_markup(false);
}

/// Escape `text` so Pango renders `&`, `<` and `>` literally.
pub(crate) fn escape(text: &str) -> String {
    glib::markup_escape_text(text).into()
}

/// A toast whose title is `text`, shown literally.
pub(crate) fn toast(text: &str) -> adw::Toast {
    adw::Toast::new(&escape(text))
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escape_neutralises_the_markup_characters() {
        assert_eq!(
            escape("Tom & <b>Jerry</b>"),
            "Tom &amp; &lt;b&gt;Jerry&lt;/b&gt;"
        );
    }

    #[test]
    fn escape_leaves_plain_text_alone() {
        assert_eq!(escape("not running: start it"), "not running: start it");
    }
}

/// What a widget **displays**, for the `system-tests` assertions in this crate:
/// `Label::text()` is the text after Pango has parsed any markup, so a test
/// pins the user-visible symptom (`"Tom & <b>Jerry</b>"` showing as `Tom & `
/// or nothing when parsed as markup) rather than a flag.
#[cfg(all(test, feature = "system-tests"))]
pub(crate) mod test_support {
    use gtk::prelude::*;

    /// Every `GtkLabel` text under `root`.
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
            "no label displays {literal:?} literally; labels show {texts:?}"
        );
    }
}
