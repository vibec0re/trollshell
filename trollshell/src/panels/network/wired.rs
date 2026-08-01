//! Left column: saved wired (ethernet) NM connection profiles with
//! activate / deactivate / forget controls.
//!
//! Backed by [`wifi::wired_profiles`] (populated by the NM backend on the same
//! refresh tick as the Wi-Fi state). The whole card hides when there are no
//! saved ethernet profiles — see the visibility bind in [`super::mod`].

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::wifi;

use super::pill_label;
use crate::components::markup;
use crate::components::reactive_list::reactive_list;

pub(super) fn build_wired_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Wired").build();

    // Subtitle tracks the saved-profile count, matching the sibling cards.
    bind(
        wifi::wired_profiles().map(|p| match p.len() {
            0 => "No saved profiles".to_string(),
            1 => "1 saved profile".to_string(),
            n => format!("{n} saved profiles"),
        }),
        &group,
        |g, sub| g.set_description(Some(&sub)),
    );

    // Drain-rebuild the profile rows on each update — same approach the Wi-Fi
    // network list uses (cheap: ethernet profiles are few and change rarely).
    reactive_list(
        &group,
        wifi::wired_profiles(),
        |profile: &wifi::WiredProfile| build_wired_row(profile),
        None::<fn() -> adw::ActionRow>,
    );

    group
}

fn build_wired_row(profile: &wifi::WiredProfile) -> adw::ActionRow {
    let subtitle = match (&profile.device_path, profile.active) {
        (Some(_), true) => "Connected".to_string(),
        (Some(_), false) => "Available".to_string(),
        (None, _) => "No interface".to_string(),
    };

    let row = adw::ActionRow::builder()
        .title(&profile.name)
        .subtitle(subtitle)
        .activatable(false)
        .build();
    // Profile names come from NetworkManager's connection store (#753).
    markup::plain_text(&row);

    let icon = gtk::Image::from_icon_name("network-wired-symbolic");
    row.add_prefix(&icon);

    // State pill, reusing the Wi-Fi connected/known styles.
    if profile.active {
        row.add_suffix(&pill_label("Active", "ts-pill-connected"));
    } else {
        row.add_suffix(&pill_label("Inactive", "ts-pill-known"));
    }

    let menu_btn = build_wired_row_menu(profile);
    row.add_suffix(&menu_btn);

    row
}

fn build_wired_row_menu(profile: &wifi::WiredProfile) -> gtk::MenuButton {
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

    let conn_path = profile.connection_path.clone();
    let device_path = profile.device_path.clone();

    if profile.active {
        // Deactivate needs the device path; only offered when one is resolved.
        if let Some(dev) = device_path.clone() {
            let pop = popover.clone();
            let deactivate_btn = gtk::Button::with_label("Deactivate");
            deactivate_btn.add_css_class("flat");
            deactivate_btn.add_css_class("destructive-action");
            deactivate_btn.connect_clicked(move |_| {
                wifi::wired_deactivate(&dev);
                pop.popdown();
            });
            popover_box.append(&deactivate_btn);
        }
    } else if let Some(dev) = device_path {
        // Activate needs both the connection profile and a target device.
        let pop = popover.clone();
        let conn = conn_path.clone();
        let activate_btn = gtk::Button::with_label("Activate");
        activate_btn.add_css_class("flat");
        activate_btn.connect_clicked(move |_| {
            wifi::wired_activate(&conn, &dev);
            pop.popdown();
        });
        popover_box.append(&activate_btn);
    }

    // Forget is always available — it deletes the saved profile, no device
    // needed.
    let pop_for_forget = popover.clone();
    let forget_conn = conn_path;
    let forget_btn = gtk::Button::with_label("Forget");
    forget_btn.add_css_class("flat");
    forget_btn.add_css_class("destructive-action");
    forget_btn.connect_clicked(move |_| {
        wifi::wired_forget(&forget_conn);
        pop_for_forget.popdown();
    });
    popover_box.append(&forget_btn);

    popover.set_child(Some(&popover_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
}
