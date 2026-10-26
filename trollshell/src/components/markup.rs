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
/// What the row **displays** does not depend on call order: `GtkLabel` keeps
/// the string it was handed and re-lays it out when `use-markup` flips, so a
/// `set_title` that lands after this call, or a builder that set the title
/// before it, both render literally.
///
/// The **warning** does depend on it. A label parses markup the moment it is
/// handed text, so a hostile string set while `use-markup` is still on logs
/// `Gtk-WARNING: Failed to set text … from markup` once per build, and
/// `.use_markup(false)` in the builder does not help (measured, #1564). Call
/// this before the first non-literal title or subtitle is set: construct the
/// row with `new()` (or a builder carrying no text), call `plain_text`, then
/// `set_title`/`set_subtitle` (`panels/notifications.rs` is the reference).
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

    /// The env var that tells a re-executed test binary it is the child of a
    /// [`child_output`] call.
    const CHILD_ENV: &str = "TROLLSHELL_MARKUP_WARNING_CHILD";

    /// True inside the child process [`child_output`] spawns. A `*_child` test
    /// returns early unless this holds, so in a normal run it is a no-op.
    pub(crate) fn in_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Re-run this test binary on the single test `path` (the path `cargo
    /// test` lists, `module::sub::name`) and return everything it printed.
    ///
    /// Why a child and not an in-process handler: GTK logs through `GLib`'s
    /// *structured* writer, which skips handlers installed with
    /// `g_log_set_handler` (measured: a handler on the `Gtk` domain saw
    /// nothing while the warning was printed), and the only hook that does see
    /// it, `g_log_set_writer_func`, is a once-per-process global that would
    /// leak into every other test in the binary. A child's stderr is the
    /// observable the warning has, and the global state stays the child's.
    ///
    /// A missing warning proves nothing unless the child really put its
    /// hostile text on a row, so every `*_child` body ends with
    /// [`assert_shows`] on that text. A child whose text never reached a label
    /// then fails, and this function's pass check fails the parent with it.
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
    use super::test_support::{self, assert_shows, label_texts};

    /// `&` is a parse error (the field blanks); `<b>` is obeyed (the tag
    /// vanishes). One string carries both failure modes.
    const HOSTILE: &str = "Tom & <b>Jerry</b>";

    fn shows(root: &impl IsA<gtk::Widget>, literal: &str) -> bool {
        label_texts(root.upcast_ref()).iter().any(|t| t == literal)
    }

    const PROBE: &str = "ORDER-PROBE Tom & <span size=\"xx-large\">Jerry</span>";

    /// Child body: the **old** order (title in the builder, `plain_text`
    /// afterwards).
    #[gtk::test]
    fn order_probe_old_child() {
        if !test_support::in_child() {
            return;
        }
        adw::init().expect("libadwaita init");
        let row = adw::ActionRow::builder().title(PROBE).build();
        plain_text(&row);
        assert_shows(&row, PROBE);
    }

    /// Child body: the **new** order (`plain_text` before any text).
    #[gtk::test]
    fn order_probe_new_child() {
        if !test_support::in_child() {
            return;
        }
        adw::init().expect("libadwaita init");
        let row = adw::ActionRow::new();
        plain_text(&row);
        row.set_title(PROBE);
        row.set_subtitle(PROBE);
        assert_shows(&row, PROBE);
    }

    /// **The premise of every no-warning pin in the panels**, so they cannot
    /// go green on a capture that sees nothing: the old order *does* log the
    /// markup warning, and `plain_text` before the text does not (#1564).
    #[gtk::test]
    fn the_capture_sees_the_warning_only_for_the_old_order() {
        if test_support::in_child() {
            return;
        }
        let old =
            test_support::child_output("components::markup::gtk_tests::order_probe_old_child");
        assert!(
            old.contains(test_support::MARKUP_WARNING),
            "premise: the old order must log the warning, or the probe sees nothing: {old}"
        );
        let new =
            test_support::child_output("components::markup::gtk_tests::order_probe_new_child");
        assert!(
            !new.contains(test_support::MARKUP_WARNING),
            "plain_text first must not warn: {new}"
        );
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
