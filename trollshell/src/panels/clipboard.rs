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
//! v1 caps the visible list at ~50 entries (enforced upstream in
//! `clipboard::refresh`). No pinning, search, or multi-select.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::clipboard::{self, ClipEntry, ClipKind};

use crate::components::layout::{finish_page, page_box};

pub fn panel_clipboard() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder()
        .title("Clipboard history")
        .build();

    // Track the rows we add so we can remove them before each rebuild —
    // PreferencesGroup has no row-traversal API.
    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let placeholder_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));

    column.append(&group);

    let group_for_bind = group.clone();
    let rows_for_bind = rows_track.clone();
    let placeholder_for_bind = placeholder_track.clone();
    bind(clipboard::history(), &group, move |_, entries| {
        for row in rows_for_bind.borrow_mut().drain(..) {
            group_for_bind.remove(&row);
        }
        if let Some(p) = placeholder_for_bind.borrow_mut().take() {
            group_for_bind.remove(&p);
        }

        if entries.is_empty() {
            // Empty state: a single non-activatable row keeps the visual
            // weight of the boxed list while making it obvious the
            // history is empty (or cliphist isn't running yet).
            let placeholder = adw::ActionRow::builder()
                .title("No clipboard history")
                .subtitle("Copy something, then re-open this page.")
                .activatable(false)
                .build();
            group_for_bind.add(&placeholder);
            *placeholder_for_bind.borrow_mut() = Some(placeholder);
            return;
        }

        let mut new_rows = Vec::with_capacity(entries.len());
        for entry in &entries {
            let row = build_clipboard_row(entry);
            group_for_bind.add(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

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
    let popover_for_delete = popover.clone();
    delete_btn.connect_clicked(move |_| {
        clipboard::delete(id_for_delete);
        popover_for_delete.popdown();
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
