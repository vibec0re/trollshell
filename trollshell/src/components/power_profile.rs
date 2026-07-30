//! Shared power-profile expander widget, reused by `panels::power` and
//! `panels::settings`.
//!
//! Both panels surface the same `adw::ExpanderRow` backed by the same
//! `power_profiles::state()` signal so they stay in sync without any extra
//! coordination.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::power_profiles::{self, humanize_profile};

/// Build an `adw::ExpanderRow` that shows the active power profile and lets
/// the user switch between available profiles.
///
/// The row hides itself when no power-profiles daemon is running (i.e.
/// `power_profiles::state().available` is empty).
pub(crate) fn build_power_profile_expander() -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title("Power profile").build();

    bind(
        power_profiles::state().map(|s| !s.available.is_empty()),
        &expander,
        gtk::prelude::WidgetExt::set_visible,
    );

    bind(
        power_profiles::state().map(|s| humanize_profile(&s.active)),
        &expander,
        |row, t| row.set_subtitle(&t),
    );

    let icon = gtk::Image::new();
    icon.set_valign(gtk::Align::Center);
    bind(
        power_profiles::state().map(|s| profile_icon_name(&s.active)),
        &icon,
        |w, name| w.set_icon_name(Some(name)),
    );
    expander.add_prefix(&icon);

    let rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let expander_for_bind = expander.clone();
    let rows_for_bind = rows_track.clone();
    bind(power_profiles::state(), &expander, move |_, state| {
        // `take()` ends the borrow before the first `remove()` — a chained
        // `borrow_mut().drain(..)` would keep the cell borrowed for the whole
        // loop, and a synchronous emission re-entering it panics (fatally, from
        // a glib callback).
        for row in rows_for_bind.take() {
            expander_for_bind.remove(&row);
        }
        let mut new_rows = Vec::with_capacity(state.available.len());
        for profile in &state.available {
            let row = adw::ActionRow::builder()
                .title(humanize_profile(profile))
                .activatable(true)
                .build();
            if profile == &state.active {
                let check = gtk::Image::from_icon_name("object-select-symbolic");
                check.set_valign(gtk::Align::Center);
                row.add_suffix(&check);
            }
            let profile_owned = profile.clone();
            row.connect_activated(move |_| {
                power_profiles::set_active(&profile_owned);
            });
            expander_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_for_bind.borrow_mut() = new_rows;
    });

    expander
}

/// Map a power-profile name to its Adwaita symbolic icon.
pub(crate) fn profile_icon_name(active: &str) -> &'static str {
    match active {
        "performance" => "power-profile-performance-symbolic",
        "balanced" => "power-profile-balanced-symbolic",
        "power-saver" => "power-profile-power-saver-symbolic",
        _ => "system-run-symbolic",
    }
}
