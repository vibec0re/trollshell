//! Pango-markup safety for the control-center's libadwaita widgets (#1564).
//!
//! The shell has the same module (`trollshell/src/components/markup.rs`, #1544);
//! this crate cannot link the shell, so it carries its own small copy of the
//! two tools. Read that module's doc for the full census; the short version:
//!
//! `AdwPreferencesRow:use-markup` and `AdwToast:use-markup` default to
//! **`TRUE`** (both set it in their `init`), and `AdwPreferencesGroup`'s title
//! and description labels are hardcoded to markup with no property at all.
//! Text we do not author (a place name the user typed, a config key or record
//! name, a TOML parse error quoting the file, `systemd-run`'s stderr, a D-Bus
//! error, a plugin's declared mount) containing `&`, `<` or `>` then fails to
//! parse and renders **blank**: the message meant to explain the problem
//! disappears.
//!
//! - [`plain_text`] for rows: turns markup off. Call it **right after
//!   `new()`/`build()` and before the first non-literal text is set**: the
//!   label tries to parse a title the moment it is set, so setting text first
//!   logs a `Gtk-WARNING` on every build (measured in the #1544 review), and
//!   `.use_markup(false)` in a builder does not help.
//! - [`escape`] for group titles and descriptions, which have no off switch.
//! - [`toast`] for the one toast sink, so call sites cannot forget. A toast
//!   *could* turn markup off instead: `AdwToast` is not a widget, and the
//!   label that draws it is created when the toast is shown, with `use-markup`
//!   bound before the title (`adw-toast-widget.c`, `update_title_widget`).
//!   Escaping is the chosen remedy all the same, one rule for every toast.
//!
//! **`AdwBanner` is not on that list: its title is plain text by default**, so
//! a banner title must NOT be escaped (it would show `&amp;`). Adw-1.gir says
//! `use-markup` defaults to `TRUE`, and that is misleading. The `TRUE` is the
//! pspec default in `adw-banner.c:422-425`, which carries no
//! `G_PARAM_CONSTRUCT`, so `GObject` never applies it. The property has no
//! storage of its own: its getter and setter proxy the template's title
//! `GtkLabel` (`adw-banner.c:681-712`, `adw-banner.ui`), which never sets
//! `use-markup` and so keeps `GtkLabel`'s `FALSE`. The class doc says the
//! same ("use `use-markup` to enable it"). Measured on libadwaita 1.9.3: a
//! banner given `Tom & <b>Jerry</b>` shows exactly that, and `main.rs`'s
//! banner test pins it, so a libadwaita that changes this goes red there.
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

    /// The env var that tells a re-executed test binary it is the child of a
    /// [`child_output`] call.
    const CHILD_ENV: &str = "TROLLSHELL_CC_MARKUP_WARNING_CHILD";

    /// True inside the child process [`child_output`] spawns. A `*_child` test
    /// returns early unless this holds, so in a normal run it is a no-op.
    pub(crate) fn in_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Re-run this test binary on the single test `path` (the path `cargo
    /// test` lists, `module::sub::name`) and return everything it printed.
    ///
    /// The shell's `components::markup::test_support` has the same helper
    /// (#1571), for the same reason: GTK logs through `GLib`'s *structured*
    /// writer, which skips handlers installed with `g_log_set_handler`, and
    /// the one hook that does see it, `g_log_set_writer_func`, is a
    /// once-per-process global that would leak into every other test in the
    /// binary. A child's stderr is the observable the warning has, and the
    /// global state stays the child's.
    #[track_caller]
    pub(crate) fn child_output(path: &str) -> String {
        let exe = std::env::current_exe().expect("the test binary's own path");
        let out = std::process::Command::new(exe)
            .args([path, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .output()
            .expect("re-running the test binary");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success() && text.contains("1 passed"),
            "the child run of {path} did not pass exactly one test (a wrong path runs none): \
             {text}"
        );
        text
    }

    /// The `Gtk-WARNING` a label logs when it is handed hostile text while
    /// `use-markup` is still on.
    pub(crate) const MARKUP_WARNING: &str = "from markup due to error parsing markup";
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use adw::prelude::*;

    use super::plain_text;
    use super::test_support::{MARKUP_WARNING, child_output, in_child};

    const PROBE: &str = "ORDER-PROBE Tom & <b>Jerry</b>";

    /// Child body: text in the builder, markup switched off in the same
    /// builder after it — the shape the agents tab used to have.
    #[gtk::test]
    fn order_probe_old_child() {
        if !in_child() {
            return;
        }
        adw::init().expect("libadwaita init");
        drop(
            adw::ActionRow::builder()
                .title(PROBE)
                .subtitle(PROBE)
                .use_markup(false)
                .build(),
        );
    }

    /// Child body: [`plain_text`] before any text.
    #[gtk::test]
    fn order_probe_new_child() {
        if !in_child() {
            return;
        }
        adw::init().expect("libadwaita init");
        let row = adw::ActionRow::new();
        plain_text(&row);
        row.set_title(PROBE);
        row.set_subtitle(PROBE);
    }

    /// **The premise of every no-warning pin in this crate**, so none of them
    /// can go green on a capture that sees nothing: the old order *does* log
    /// the markup warning, and `plain_text` before the text does not (#1564).
    #[gtk::test]
    fn the_capture_sees_the_warning_only_for_the_old_order() {
        if in_child() {
            return;
        }
        let old = child_output("markup::gtk_tests::order_probe_old_child");
        assert!(
            old.contains(MARKUP_WARNING),
            "premise: the old order must log the warning, or the probe sees nothing: {old}"
        );
        let new = child_output("markup::gtk_tests::order_probe_new_child");
        assert!(
            !new.contains(MARKUP_WARNING),
            "plain_text first must not warn: {new}"
        );
    }
}
