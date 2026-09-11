//! The **Add app** desktop-entry picker — #1071 §5, phase 4.
//!
//! A searchable list of the installed applications, offered from the Edit
//! sub-page's *Add app* button; picking a row appends that entry's id to the
//! stack being edited. Nothing like it existed in the tree — §2's table lists it
//! under *"not in the tree yet and built by this epic"* — so this is the whole
//! of it.
//!
//! ## The seam
//!
//! [`picker_popover`] takes its rows as a plain `Vec<PickerEntry>` rather than
//! reading `gio::AppInfo::all()` itself, and [`add_app_button`] is the one-line
//! wrapper that supplies the real ones. That is what makes §5's *"filtered to
//! `NoDisplay=false`"* falsifiable: `gio::AppInfo` is a `GObject` interface with
//! no constructor a test can reach, so a picker that filtered `AppInfo`s
//! directly could only ever be checked against whatever happens to be installed
//! on the machine running the suite — which is to say, not checked. With the
//! seam a `#[gtk::test]` hands it one hidden entry and one visible one and reads
//! back which rows exist.
//!
//! The filtering itself is [`crate::components::desktop_entry::filtered`], pure
//! and separately tested; this module is the widget around it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::gtk::{self, glib, pango, prelude::*};

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::desktop_entry::{PickerEntry, filtered};

/// CSS class on the picker's popover, for its sizing.
const PICKER_CLASS: &str = "ts-ws-picker";

/// CSS class on one offered entry's row — the hook a test counts.
const PICKER_ROW_CLASS: &str = "ts-ws-picker-row";

/// Shown when the search matches nothing.
const NO_MATCH_HINT: &str = "No application matches";

/// Design-baseline height of the scrolling row list, in CSS px, before
/// [`crate::scale::scale`]. The list is long (every installed application), so
/// this is a budget rather than a fit: tall enough to show a handful of matches
/// without the popover covering the drawer it was opened from.
const PICKER_HEIGHT: i32 = 320;

/// The **Add app** button, with the real installed-application list behind it.
///
/// `on_pick` is handed the chosen entry's desktop id — without `.desktop`,
/// which is the spelling `workspaces.toml` stores and niri reports.
pub(crate) fn add_app_button(on_pick: impl Fn(&str) + 'static) -> gtk::Widget {
    let button = gtk::MenuButton::builder()
        .label("Add app\u{2026}")
        .always_show_arrow(false)
        .build();
    button.add_css_class("flat");
    button.add_css_class("ts-ws-add-app");
    // Built once, when the button is built: `AppInfo::all()` is a directory
    // scan of every `applications/` dir on the system, and doing it per popover
    // open would cost that scan on every click. An application installed while
    // the drawer is open is not offered until the page is rebuilt, which the
    // config poll and the niri event stream both do often.
    button.set_popover(Some(&picker_popover(
        crate::components::desktop_entry::installed(),
        on_pick,
    )));
    button.upcast()
}

/// [`add_app_button`]'s popover with its rows injected, so a test can drive the
/// picker without depending on what is installed.
pub(crate) fn picker_popover(
    entries: Vec<PickerEntry>,
    on_pick: impl Fn(&str) + 'static,
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.add_css_class(PICKER_CLASS);

    let column = gtk::Box::new(gtk::Orientation::Vertical, 6);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search applications\u{2026}")
        .hexpand(true)
        .build();
    search.add_css_class("ts-ws-picker-search");
    column.append(&search);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(crate::scale::scale(PICKER_HEIGHT))
        .child(&list)
        .build();
    column.append(&scroller);
    popover.set_child(Some(&column));

    // One cache for the popover's whole life: the same application can be
    // re-rendered on every keystroke as the search narrows and widens, and each
    // render resolves an icon.
    let meta_cache: MetaCache = Rc::new(RefCell::new(HashMap::new()));
    let entries = Rc::new(entries);
    let on_pick = Rc::new(on_pick);

    let rebuild = {
        let entries = Rc::clone(&entries);
        let on_pick = Rc::clone(&on_pick);
        let popover = popover.downgrade();
        move |list: &gtk::ListBox, query: &str| {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let matching = filtered(&entries, query);
            if matching.is_empty() {
                let empty = gtk::Label::new(Some(NO_MATCH_HINT));
                empty.add_css_class("ts-ws-empty");
                empty.set_xalign(0.0);
                list.append(&empty);
                return;
            }
            for entry in matching {
                list.append(&picker_row(
                    entry,
                    &meta_cache,
                    Rc::clone(&on_pick),
                    popover.clone(),
                ));
            }
        }
    };

    rebuild(&list, "");
    // The list is taken from the closure's own capture rather than re-derived
    // from the entry: `SearchEntry` has no path to its sibling, and the list is
    // a child of the popover this closure's lifetime is already bounded by.
    let list_for_search = list.clone();
    search.connect_search_changed(move |search| {
        rebuild(&list_for_search, search.text().as_str());
    });

    // Opening the popover should land the cursor in the search field, which is
    // the only reason anyone opens it.
    popover.connect_show(move |_| {
        search.grab_focus();
    });
    popover
}

/// One offered application.
fn picker_row(
    entry: &PickerEntry,
    meta_cache: &MetaCache,
    on_pick: Rc<impl Fn(&str) + 'static>,
    popover: glib::WeakRef<gtk::Popover>,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.add_css_class(PICKER_ROW_CLASS);
    row.set_activatable(true);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    body.set_margin_top(4);
    body.set_margin_bottom(4);
    body.set_margin_start(8);
    body.set_margin_end(8);

    // Resolve into a local first: an argument-position `borrow_mut()` lives for
    // the whole enclosing statement, so inlining this into the GTK call would
    // hold the `RefMut` across something that can synchronously re-enter
    // (#643/#663/#832).
    let meta = resolve_app_meta(&entry.id, &mut meta_cache.borrow_mut());
    let icon = meta.map_or_else(fallback_icon, |m| m.icon.unwrap_or_else(fallback_icon));
    let image = gtk::Image::from_gicon(&icon);
    image.set_icon_size(gtk::IconSize::Normal);
    body.append(&image);

    // Plain `gtk::Label`s: a display name and a desktop id both come from files
    // this shell does not own, and `use-markup` is off on a plain label — the
    // #30/#753 rule the rest of the page follows.
    let name = gtk::Label::new(Some(&entry.name));
    name.set_xalign(0.0);
    name.set_ellipsize(pango::EllipsizeMode::End);
    name.add_css_class("ts-ws-picker-name");

    let id = gtk::Label::new(Some(&entry.id));
    id.set_xalign(0.0);
    id.set_ellipsize(pango::EllipsizeMode::End);
    id.add_css_class("dim-label");
    id.add_css_class("ts-ws-picker-id");

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
    labels.set_hexpand(true);
    labels.append(&name);
    labels.append(&id);
    body.append(&labels);

    row.set_child(Some(&body));

    let picked = entry.id.clone();
    row.connect_activate(move |_| {
        on_pick(&picked);
        if let Some(popover) = popover.upgrade() {
            popover.popdown();
        }
    });
    row
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{PICKER_ROW_CLASS, picker_popover};
    use crate::components::desktop_entry::PickerEntry;
    use hytte::adw;
    use hytte::gtk::{self, prelude::*};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Drive the GTK main loop until `done()` holds, or `ms` of wall clock has
    /// passed.
    ///
    /// Needed here because `GtkSearchEntry`'s `search-changed` is **delayed** —
    /// that is the difference between it and a plain `changed`, and it is why
    /// the picker uses it: the list is every installed application, and
    /// rebuilding it on each of five keystrokes of "fire" would rescan and
    /// re-resolve icons five times. So a `pump()` after `set_text` sees nothing;
    /// the timer has to actually fire.
    fn pump_until(ms: u64, done: impl Fn() -> bool) {
        let expired = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = expired.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            flag.set(true);
        });
        while !expired.get() && !done() {
            gtk::glib::MainContext::default().iteration(true);
        }
    }

    fn entry(id: &str, name: &str, show: bool) -> PickerEntry {
        PickerEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            show,
        }
    }

    fn by_class(root: &impl IsA<gtk::Widget>, class: &str) -> Vec<gtk::Widget> {
        fn walk(widget: &gtk::Widget, class: &str, out: &mut Vec<gtk::Widget>) {
            if widget.has_css_class(class) {
                out.push(widget.clone());
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                walk(&c, class, out);
                child = c.next_sibling();
            }
        }
        let mut out = Vec::new();
        walk(root.upcast_ref(), class, &mut out);
        out
    }

    /// The ids of the rows the picker is currently offering, in order.
    fn offered(popover: &gtk::Popover) -> Vec<String> {
        let Some(child) = popover.child() else {
            return Vec::new();
        };
        by_class(&child, PICKER_ROW_CLASS)
            .into_iter()
            .map(|row| {
                by_class(&row, "ts-ws-picker-id")
                    .into_iter()
                    .find_map(|w| w.downcast::<gtk::Label>().ok())
                    .map(|l| l.text().to_string())
                    .unwrap_or_default()
            })
            .collect()
    }

    fn search(popover: &gtk::Popover) -> gtk::SearchEntry {
        let child = popover.child().expect("the popover has a child");
        by_class(&child, "ts-ws-picker-search")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::SearchEntry>().ok())
            .expect("the picker has a search field")
    }

    fn sample() -> Vec<PickerEntry> {
        vec![
            entry("org.mozilla.firefox", "Firefox", true),
            entry("org.gnome.Nautilus", "Files", true),
            entry("nvidia-settings", "NVIDIA Settings", false),
            entry("Alacritty", "Alacritty", true),
        ]
    }

    /// §5: the list is *"filtered to `NoDisplay=false`"* — and it is the widget
    /// that must honour it, not merely the pure predicate beside it.
    ///
    /// Falsified by dropping the `show` filter in `desktop_entry::filtered`.
    #[gtk::test]
    fn the_picker_never_offers_a_hidden_entry() {
        adw::init().expect("libadwaita init");
        let popover = picker_popover(sample(), |_| {});
        pump();
        let ids = offered(&popover);
        assert_eq!(
            ids,
            ["org.mozilla.firefox", "org.gnome.Nautilus", "Alacritty"],
            "a NoDisplay entry reached the list"
        );
    }

    /// §5: *searchable*. Typing narrows the rendered rows, not merely the
    /// predicate's return value.
    #[gtk::test]
    fn typing_narrows_the_offered_rows() {
        adw::init().expect("libadwaita init");
        let popover = picker_popover(sample(), |_| {});
        pump();
        assert_eq!(offered(&popover).len(), 3);

        let search = search(&popover);
        /// Type `query` and wait for the debounced `search-changed` to land.
        fn typed(popover: &gtk::Popover, search: &gtk::SearchEntry, query: &str) -> Vec<String> {
            search.set_text(query);
            let before = offered(popover);
            pump_until(2000, || offered(popover) != before);
            offered(popover)
        }

        assert_eq!(
            typed(&popover, &search, "fire"),
            ["org.mozilla.firefox"],
            "typing did not narrow the rendered rows"
        );

        // By id, for an application whose display name shares nothing with it.
        assert_eq!(typed(&popover, &search, "nautilus"), ["org.gnome.Nautilus"]);

        // Clearing it brings the whole (still-filtered) list back — and the
        // hidden entry is still not among them.
        assert_eq!(
            typed(&popover, &search, ""),
            ["org.mozilla.firefox", "org.gnome.Nautilus", "Alacritty"]
        );

        // A query nothing matches renders no rows at all rather than a stale
        // list.
        assert!(typed(&popover, &search, "no-such-application").is_empty());
    }

    /// §5: *"selecting appends an app to the stack"* — the row hands its **id**
    /// back, which is the spelling `workspaces.toml` stores.
    #[gtk::test]
    fn activating_a_row_reports_its_desktop_id() {
        adw::init().expect("libadwaita init");
        let picked: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&picked);
        let popover = picker_popover(sample(), move |id| sink.borrow_mut().push(id.to_owned()));
        pump();

        let child = popover.child().expect("the popover has a child");
        let rows = by_class(&child, PICKER_ROW_CLASS);
        let row = rows
            .into_iter()
            .find_map(|w| w.downcast::<gtk::ListBoxRow>().ok())
            .expect("at least one row");
        row.emit_activate();
        pump();

        assert_eq!(&*picked.borrow(), &["org.mozilla.firefox".to_owned()]);
    }
}
