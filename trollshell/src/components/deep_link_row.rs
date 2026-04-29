//! `deep_link_row` — an `AdwActionRow` that opens a different drawer
//! page on activation. Used by the Settings panel's "More" group and
//! by the Network panel's "Active connections" drill-down.

use hytte::adw::{self, prelude::*};
use hytte::gtk;

/// Build an `AdwActionRow` that, on activation, swaps every open drawer
/// to `target` via `crate::modal::switch_active`. Used by drawer panels
/// to surface other pages that don't have a dedicated bar chip.
pub(crate) fn deep_link_row(
    title: &str,
    subtitle: Option<&str>,
    icon_name: &str,
    target: crate::modal::Page,
) -> adw::ActionRow {
    let mut builder = adw::ActionRow::builder().title(title).activatable(true);
    if let Some(s) = subtitle {
        builder = builder.subtitle(s);
    }
    let row = builder.build();
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);
    let go_next = gtk::Image::from_icon_name("go-next-symbolic");
    row.add_suffix(&go_next);
    row.connect_activated(move |_| {
        crate::modal::switch_active(target);
    });
    row
}
