//! Drawer panel for clipboard history, backed by `cliphist`. Capture happens
//! out-of-process under two `wl-paste --watch cliphist store` units (text +
//! image); see `etc/cliphist/README.md`.
//!
//! On page open, `crate::modal::on_page_show` invokes `clipboard::refresh()`,
//! which re-runs `cliphist list` off the GTK thread and updates the
//! `clipboard::history()` signal. The bind below rebuilds the row list
//! whenever that signal fires.
//!
//! Click an entry to re-paste it: the service pipes
//! `cliphist decode <id> | wl-copy`, then we dismiss the drawer so the
//! next Ctrl-V lands in whatever app the user was in.
//!
//! Each row's `⋮` menu carries a destructive "Delete entry" action wired to
//! `clipboard::delete(id)`. That service call prunes the entry from the
//! cached snapshot synchronously before it shells out, so the row leaves an
//! open drawer on click rather than on the next reopen — see `delete`'s docs
//! for why a plain `refresh()` was not enough.
//!
//! v1 caps the visible list at ~50 entries (enforced upstream in
//! `clipboard::refresh`). No pinning, search, or multi-select.

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::services::clipboard::{self, ClipEntry, ClipKind};

use crate::components::layout::{finish_page, page_box};
use crate::components::reactive_list::reactive_list;

pub fn panel_clipboard() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder()
        .title("Clipboard history")
        .build();

    column.append(&group);

    reactive_list(
        &group,
        clipboard::history(),
        |entry: &ClipEntry| build_clipboard_row(entry),
        // Empty state: a single non-activatable row keeps the visual weight of
        // the boxed list while making it obvious the history is empty (or
        // cliphist isn't running yet).
        Some(|| {
            adw::ActionRow::builder()
                .title("No clipboard history")
                .subtitle("Copy something, then re-open this page.")
                .activatable(false)
                .build()
        }),
    );

    finish_page(&column)
}

fn build_clipboard_row(entry: &ClipEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&entry.preview)
        .activatable(true)
        .build();
    row.set_title_lines(1);

    let icon_name = match entry.kind {
        ClipKind::Image => "image-x-generic-symbolic",
        ClipKind::Text => "edit-paste-symbolic",
    };
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);

    // ⋮ menu button: destructive "Delete entry" lives here, not on the row's
    // primary click target — destructive actions belong in a popover so they
    // can't be misclicked while reaching for paste.
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.add_css_class("flat");
    menu_btn.set_tooltip_text(Some("More actions"));

    let popover = gtk::Popover::new();
    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover_box.set_margin_top(4);
    popover_box.set_margin_bottom(4);
    popover_box.set_margin_start(4);
    popover_box.set_margin_end(4);

    let delete_btn = gtk::Button::with_label("Delete entry");
    delete_btn.add_css_class("flat");
    delete_btn.add_css_class("destructive-action");
    let id_for_delete = entry.id;
    // Weak, not a strong clone (#1176): `delete_btn` is a descendant of the
    // popover it dismisses (`popover → popover_box → delete_btn`), so a
    // strong capture here closes a refcount cycle GTK never breaks and every
    // row `reactive_list` retires leaks its whole menu subtree. Same idiom as
    // `panels/bluetooth.rs`'s device menu and `components/app_picker.rs`.
    let popover_for_delete = popover.downgrade();
    delete_btn.connect_clicked(move |_| {
        clipboard::delete(id_for_delete);
        if let Some(popover) = popover_for_delete.upgrade() {
            popover.popdown();
        }
    });
    popover_box.append(&delete_btn);
    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    row.add_suffix(&menu_btn);

    let id = entry.id;
    row.connect_activated(move |_| {
        clipboard::paste_entry(id);
        crate::modal::dismiss_all();
    });

    row
}

/// #1176 regression coverage. `reactive_list` rebuilds this whole list on
/// every `clipboard::history()` emission, so a row that cannot be freed is a
/// leak per entry per refresh — and the row's "⋮" menu is exactly that shape:
/// the Delete button lives *inside* the popover it dismisses, so a strong
/// `popover.clone()` in its handler closes a cycle GTK never breaks.
///
/// Needs a real display server (`adw::ActionRow` and `gtk::Popover` have to be
/// constructible), hence the `system-tests` gate.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{ClipEntry, ClipKind, build_clipboard_row};
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Falsified by restoring `let popover_for_delete = popover.clone();` and
    /// the bare `popover_for_delete.popdown()` body: the popover then upgrades
    /// long after its row is gone.
    #[gtk::test]
    fn a_clipboard_row_menu_dies_with_its_row() {
        adw::init().expect("libadwaita init");
        let entry = ClipEntry {
            id: 7,
            preview: "hello".to_owned(),
            kind: ClipKind::Text,
        };

        let row = build_clipboard_row(&entry);
        let menu_btn = find_menu_button(row.upcast_ref())
            .expect("the row carries a ⋮ menu button in its suffix");
        let popover = menu_btn
            .popover()
            .expect("the menu button carries the actions popover");
        let weak_popover = popover.downgrade();
        let weak_box = popover
            .child()
            .expect("the popover holds its button box")
            .downgrade();
        drop(popover);
        drop(menu_btn);

        drop(row);
        pump();

        assert!(
            weak_popover.upgrade().is_none(),
            "the row's ⋮ popover must be freed with the row: a strong `popover.clone()` captured \
             by the Delete handler — which lives on a button *inside* that popover — is a cycle \
             GTK never breaks, so every `clipboard::history()` refresh leaks one menu subtree \
             per entry (#1176)"
        );
        assert!(
            weak_box.upgrade().is_none(),
            "the popover's child box must go with it, not just the popover"
        );
    }

    /// Depth-first search for the `gtk::MenuButton` in a row's suffix box —
    /// `AdwActionRow` wraps suffixes in boxes whose exact nesting is libadwaita's
    /// business, so walking for the type is sturdier than indexing children.
    fn find_menu_button(widget: &gtk::Widget) -> Option<gtk::MenuButton> {
        if let Ok(btn) = widget.clone().downcast::<gtk::MenuButton>() {
            return Some(btn);
        }
        let mut child = widget.first_child();
        while let Some(w) = child {
            if let Some(found) = find_menu_button(&w) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }
}
