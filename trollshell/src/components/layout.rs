//! Layout primitives every drawer panel uses.
//!
//! `page_box` and `finish_page` wrap the outer column with the standard
//! drawer styling and an `AdwClamp` width cap. `page_grid` is the
//! two-column grid container. `section` (renamed from `panel` to avoid
//! clashing with the `panels/` module name) is the titled card you
//! attach into a grid cell. `boxed_list` builds a selection-free
//! `gtk::ListBox` with the Adwaita `boxed-list` style. `toggle_class`
//! adds or removes a single CSS class based on a boolean.

use hytte::adw;
use hytte::gtk::{self, prelude::*};

use crate::scale::scale;

/// Drawer's max content width, in CSS px (`AdwClamp.maximum_size` below, and
/// the per-trigger margin clamp in `modal.rs` that keeps the card from
/// falling off-screen left) — single source so the two never drift apart.
pub(crate) const DRAWER_MAX_WIDTH: i32 = 680;

/// Wider drawer max content width for the Stats multicolumn layout (#508). Two
/// side-by-side history graphs inside the global `DRAWER_MAX_WIDTH` (680) would
/// each squeeze to ~330px — the opposite of #508's "the panel got smaller"
/// complaint — so the multicolumn Stats page opts into this via
/// [`finish_page_clamped`]. `modal.rs`'s centering clamp
/// (`main_margin_for_center`) uses this as its upper bound too, so a card up to
/// this wide still centers correctly under its trigger chip. No other page uses
/// it — every other page stays on `finish_page`/`DRAWER_MAX_WIDTH`.
pub(crate) const DRAWER_MAX_WIDTH_WIDE: i32 = 1080;

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
    // `maximum_size` caps the child allocation; `tightening_threshold` is
    // intentionally set equal to it so `AdwClamp` never over-requests beyond
    // the cap. When threshold < maximum_size, the clamp's natural-width
    // request is `threshold + 3×(maximum_size − threshold)`, which balloons
    // the surface past the card and creates wide lilac side-margins on panels
    // whose content natural width crosses the threshold (network, vpn — #134).
    // With threshold == maximum_size that formula collapses to exactly
    // `maximum_size` and the overshoot disappears. Both values are scaled with
    // the font so the cap grows consistently with the rest of the shell (#114).
    let cap = scale(DRAWER_MAX_WIDTH);
    let clamp = adw::Clamp::builder()
        .maximum_size(cap)
        .tightening_threshold(cap)
        .child(content)
        .build();
    clamp.upcast()
}

/// [`finish_page`] with a caller-chosen max width instead of the global
/// `DRAWER_MAX_WIDTH`. Added for the Stats multicolumn layout (#508), which
/// wants a wider clamp ([`DRAWER_MAX_WIDTH_WIDE`]) than the other pages so its
/// two columns each get a usable width. Same `threshold == maximum_size`
/// no-overshoot trick and font-scaling as [`finish_page`] — see that function's
/// comment for why the two clamp values are equal.
pub(crate) fn finish_page_clamped(content: &impl IsA<gtk::Widget>, max_width: i32) -> gtk::Widget {
    let cap = scale(max_width);
    let clamp = adw::Clamp::builder()
        .maximum_size(cap)
        .tightening_threshold(cap)
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

/// Selection-free `gtk::ListBox` styled with Adwaita's `boxed-list` look.
/// Use `list.append(&row)` to populate it.
pub(crate) fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

/// Add `class` to `widget` when `on` is `true`; remove it otherwise.
pub(crate) fn toggle_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
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
