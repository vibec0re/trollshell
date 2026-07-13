//! `build_history_row` — the row primitive used by the stats panels and by
//! the per-interface rows in `panel_network`. Returns
//! `[name 80px | sparkline hexpand | value 80px]` as a plain `gtk::Box`
//! (not an `AdwActionRow`) so the sparkline takes the row's full width.

use hytte::gtk::{self, prelude::*};
use hytte::ui::Sparkline;

/// Build a `[name | Sparkline | value]` row styled `.ts-history-row`.
/// Returns the box, the Sparkline (caller pushes samples), and the
/// value label (caller binds text on it).
pub(crate) fn build_history_row(name: &str) -> (gtk::Box, Sparkline, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-history-row");

    let name_label = gtk::Label::new(Some(name));
    name_label.add_css_class("ts-stat-name");
    name_label.set_xalign(0.0);
    name_label.set_size_request(crate::scale::scale(80), -1);
    row.append(&name_label);

    let spark = Sparkline::new(60);
    spark.widget().set_hexpand(true);
    row.append(spark.widget());

    let value_label = gtk::Label::new(None);
    value_label.add_css_class("ts-stat-value");
    value_label.set_xalign(1.0);
    value_label.set_size_request(crate::scale::scale(80), -1);
    row.append(&value_label);

    (row, spark, value_label)
}
