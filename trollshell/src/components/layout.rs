//! Layout primitives every drawer panel uses.
//!
//! `page_box` and `finish_page` wrap the outer column with the standard
//! drawer styling and an `AdwClamp` width cap. `page_grid` is the
//! two-column grid container. `section` (renamed from `panel` to avoid
//! clashing with the `panels/` module name) is the titled card you
//! attach into a grid cell.

use hytte::adw;
use hytte::gtk::{self, prelude::*};

pub(crate) fn page_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("ts-modal-page");
    b
}

/// Wrap a finished page widget (Box or Grid) in an `AdwClamp` so a child
/// reporting a pathological natural width (e.g. an `AdwActionRow` subtitle
/// that ends up holding a long single-line list) can't push the
/// layer-shell modal surface to full-screen width. Belt-and-suspenders
/// against the same class of bug — individual rows should still constrain
/// themselves (multi-line subtitles, `subtitle_lines(0)`, etc.) but this
/// catches the ones that don't.
pub(crate) fn finish_page(content: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let clamp = adw::Clamp::builder()
        .maximum_size(680)
        .tightening_threshold(560)
        .child(content)
        .build();
    clamp.upcast()
}

/// Two-column (or more) grid for rich modal pages. Sections attach via
/// `grid.attach(&section, col, row, 1, 1)`.
pub(crate) fn page_grid() -> gtk::Grid {
    let g = gtk::Grid::new();
    g.add_css_class("ts-modal-page");
    g.add_css_class("ts-page-grid");
    g.set_row_spacing(12);
    g.set_column_spacing(12);
    g.set_column_homogeneous(true);
    g
}

/// Card-style section with a title header. Caller appends content by
/// calling `outer.append(&child)` on the returned Box.
///
/// Renamed from `panel` to avoid the module-name clash with `panels/`.
pub(crate) fn section(title: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 4);
    outer.add_css_class("ts-panel");
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("ts-panel-title");
    title_label.set_xalign(0.0);
    outer.append(&title_label);
    outer
}
