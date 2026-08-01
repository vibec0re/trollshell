//! Bluetooth drawer panel — adapter power, scan/discovery, pairing
//! prompts, and per-device rows.
//!
//! Wraps `hytte::services::bluetooth` (`BlueZ` over D-Bus) plus
//! `hytte::services::bluetooth_audio` for the auto-switch toggle. Pair
//! prompts surface inline as a banner above the device list while
//! `BlueZ`'s `Agent1` callback is awaiting user response.

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device, PairPrompt, PromptKind};
use hytte::services::bluetooth_audio;

use crate::components::layout::{finish_page, page_box};
use crate::components::markup;

pub fn panel_bluetooth() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    column.append(&build_bluetooth_header());
    column.append(&build_pair_prompt_banner());
    column.append(&build_bluetooth_controls());
    column.append(&build_bluetooth_device_groups());

    finish_page(&column)
}

/// Top header row: "Bluetooth" title with adapter name as subtitle and a
/// proper `GtkSwitch` for Power. Wrapped in an `AdwPreferencesGroup` so it gets
/// the boxed-list look every other row uses.
fn build_bluetooth_header() -> gtk::Widget {
    let group = adw::PreferencesGroup::new();

    let row = adw::ActionRow::builder().title("Bluetooth").build();
    bind(
        bluetooth::adapter().map(|a| match a {
            Some(ad) => ad.name,
            None => "No Bluetooth adapter".to_string(),
        }),
        &row,
        |w, name| w.set_subtitle(&name),
    );

    let power_switch = gtk::Switch::new();
    power_switch.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some()),
        &power_switch,
        gtk::prelude::WidgetExt::set_sensitive,
    );
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &power_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth::set_powered(sw.is_active())),
    );

    row.add_suffix(&power_switch);
    row.set_activatable_widget(Some(&power_switch));
    group.add(&row);

    group.upcast()
}

/// Adapter sub-controls: Discoverable + Scan. Disabled when the adapter is
/// off or absent, so toggling Power is the obvious entry point.
fn build_bluetooth_controls() -> gtk::Widget {
    let group = adw::PreferencesGroup::new();
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.powered)),
        &group,
        gtk::prelude::WidgetExt::set_sensitive,
    );

    // Discoverable
    let disc_row = adw::ActionRow::builder().title("Discoverable").build();
    bind(
        bluetooth::adapter().map(|a| match a {
            Some(ad) if ad.discoverable && !ad.name.is_empty() => {
                format!("Visible as \u{201c}{}\u{201d}", ad.name)
            }
            Some(ad) if ad.discoverable => "Visible to other devices".to_string(),
            _ => "Hidden from other devices".to_string(),
        }),
        &disc_row,
        |row, text| row.set_subtitle(&text),
    );
    let disc_switch = gtk::Switch::new();
    disc_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discoverable)),
        &disc_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth::set_discoverable(sw.is_active())),
    );
    disc_row.add_suffix(&disc_switch);
    disc_row.set_activatable_widget(Some(&disc_switch));
    group.add(&disc_row);

    // Auto-switch audio: when a BT audio device connects, make it the
    // default pipewire sink (and restore the previous one on disconnect).
    let auto_row = adw::ActionRow::builder()
        .title("Auto-switch audio")
        .subtitle("Use Bluetooth audio devices when they connect")
        .build();
    let auto_switch = gtk::Switch::new();
    auto_switch.set_valign(gtk::Align::Center);
    bind_two_way(
        bluetooth_audio::auto_switch_enabled(),
        &auto_switch,
        gtk::Switch::set_active,
        |w| w.connect_active_notify(|sw| bluetooth_audio::set_auto_switch_enabled(sw.is_active())),
    );
    auto_row.add_suffix(&auto_switch);
    auto_row.set_activatable_widget(Some(&auto_switch));
    group.add(&auto_row);

    // Scan with inline spinner showing live progress.
    let scan_row = adw::ActionRow::builder().title("Scan for devices").build();

    let spinner = gtk::Spinner::new();
    spinner.set_valign(gtk::Align::Center);
    bind(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &spinner,
        |w, on| {
            w.set_spinning(on);
            w.set_visible(on);
        },
    );
    scan_row.add_suffix(&spinner);

    let scan_btn = gtk::ToggleButton::new();
    scan_btn.set_valign(gtk::Align::Center);
    bind_two_way(
        bluetooth::adapter().map(|a| a.is_some_and(|ad| ad.discovering)),
        &scan_btn,
        |w, discovering| {
            w.set_active(discovering);
            w.set_label(if discovering { "Stop" } else { "Scan" });
        },
        |w| {
            w.connect_toggled(|btn| {
                if btn.is_active() {
                    bluetooth::start_discovery();
                } else {
                    bluetooth::stop_discovery();
                }
            })
        },
    );
    scan_row.add_suffix(&scan_btn);
    scan_row.set_activatable_widget(Some(&scan_btn));
    group.add(&scan_row);

    group.upcast()
}

/// Container holding three boxed-list groups (Connected / Paired /
/// Available). Each group is rebuilt on every `devices()`/`device_actions()`
/// emission. Empty groups are omitted entirely so the page doesn't show
/// dangling section headers.
fn build_bluetooth_device_groups() -> gtk::Widget {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let outer_for_bind = outer.clone();

    let combined = map_ref! {
        let devs = bluetooth::devices(),
        let actions = bluetooth::device_actions() => {
            (devs.clone(), actions.clone())
        }
    };

    bind(combined, &outer, move |_, (devs, actions)| {
        while let Some(child) = outer_for_bind.first_child() {
            outer_for_bind.remove(&child);
        }
        let mut connected = Vec::new();
        let mut paired = Vec::new();
        let mut available = Vec::new();
        for dev in &devs {
            if dev.connected {
                connected.push(dev);
            } else if dev.paired {
                paired.push(dev);
            } else {
                available.push(dev);
            }
        }
        for (title, group_devs) in [
            ("Connected", connected),
            ("Paired", paired),
            ("Available", available),
        ] {
            if group_devs.is_empty() {
                continue;
            }
            let group = adw::PreferencesGroup::builder().title(title).build();
            for dev in &group_devs {
                let is_busy = actions.contains(&dev.path);
                let row = build_device_row(dev, is_busy);
                group.add(&row);
            }
            outer_for_bind.append(&group);
        }
    });

    outer.upcast()
}

/// Banner shown above the device list while `BlueZ`'s `Agent1` callback is
/// waiting on the user to accept or reject a pairing. Hidden otherwise.
fn build_pair_prompt_banner() -> gtk::Widget {
    let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    prompt_box.add_css_class("ts-bluetooth-prompt");
    prompt_box.set_visible(false);
    let prompt_box_for_bind = prompt_box.clone();
    bind(
        bluetooth::pair_prompts(),
        &prompt_box,
        move |_, prompt: Option<PairPrompt>| {
            while let Some(child) = prompt_box_for_bind.first_child() {
                prompt_box_for_bind.remove(&child);
            }
            let Some(p) = prompt else {
                prompt_box_for_bind.set_visible(false);
                return;
            };
            prompt_box_for_bind.set_visible(true);
            populate_pair_prompt(&prompt_box_for_bind, &p);
        },
    );
    prompt_box.upcast()
}

fn populate_pair_prompt(container: &gtk::Box, p: &PairPrompt) {
    let title = gtk::Label::new(Some(&format!("Pair with {}?", p.alias)));
    title.set_xalign(0.0);
    title.add_css_class("ts-bluetooth-prompt-title");
    container.append(&title);

    let detail_text = match (p.kind, p.passkey) {
        (PromptKind::ConfirmPasskey, Some(code)) => {
            format!("Code: {code:06}\nMatch this on the other device, then Confirm.")
        }
        (PromptKind::ConfirmPasskey, None) => "Confirm pairing.".to_string(),
        (PromptKind::Authorize, _) => "Allow this device to pair with you.".to_string(),
        (PromptKind::EnterPinCode, _) => "Enter the PIN shown on the other device.".to_string(),
        (PromptKind::EnterPasskey, _) => {
            "Enter the numeric passkey shown on the other device.".to_string()
        }
    };
    let detail = gtk::Label::new(Some(&detail_text));
    detail.set_xalign(0.0);
    detail.set_wrap(true);
    detail.add_css_class("ts-bluetooth-prompt-detail");
    container.append(&detail);

    match p.kind {
        PromptKind::ConfirmPasskey | PromptKind::Authorize => {
            container.append(&build_yes_no_row());
        }
        PromptKind::EnterPinCode => {
            container.append(&build_text_entry_row(false));
        }
        PromptKind::EnterPasskey => {
            container.append(&build_text_entry_row(true));
        }
    }
}

fn build_yes_no_row() -> gtk::Box {
    let btn_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let confirm_btn = gtk::Button::with_label("Confirm");
    confirm_btn.add_css_class("suggested-action");
    confirm_btn.connect_clicked(|_| bluetooth::respond_to_prompt(true));
    let reject_btn = gtk::Button::with_label("Reject");
    reject_btn.add_css_class("destructive-action");
    reject_btn.connect_clicked(|_| bluetooth::respond_to_prompt(false));
    btn_row.append(&confirm_btn);
    btn_row.append(&reject_btn);
    btn_row
}

/// Entry + Submit/Cancel row for legacy `RequestPinCode` / `RequestPasskey`.
/// `numeric_only` switches input filtering so passkey entries can't contain
/// non-digits — `BlueZ` would reject the malformed value anyway.
fn build_text_entry_row(numeric_only: bool) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let entry = gtk::Entry::new();
    entry.set_hexpand(true);
    if numeric_only {
        entry.set_input_purpose(gtk::InputPurpose::Digits);
        entry.set_max_length(6);
        entry.set_placeholder_text(Some("0–999999"));
    } else {
        entry.set_max_length(16);
        entry.set_placeholder_text(Some("PIN"));
    }
    entry.set_activates_default(false);
    row.append(&entry);

    let submit_btn = gtk::Button::with_label("Submit");
    submit_btn.add_css_class("suggested-action");
    let entry_for_submit = entry.clone();
    submit_btn.connect_clicked(move |_| submit_entry(&entry_for_submit, numeric_only));
    let entry_for_activate = entry.clone();
    entry.connect_activate(move |_| submit_entry(&entry_for_activate, numeric_only));
    row.append(&submit_btn);

    let cancel_btn = gtk::Button::with_label("Cancel");
    cancel_btn.add_css_class("destructive-action");
    cancel_btn.connect_clicked(|_| bluetooth::respond_to_prompt(false));
    row.append(&cancel_btn);

    row
}

fn submit_entry(entry: &gtk::Entry, numeric_only: bool) {
    let text = entry.text().to_string();
    if numeric_only {
        match text.trim().parse::<u32>() {
            Ok(n) => bluetooth::submit_passkey(n),
            // Empty / non-numeric → reject so `BlueZ` doesn't see junk.
            Err(_) => bluetooth::respond_to_prompt(false),
        }
    } else {
        bluetooth::submit_pin(text);
    }
}

fn build_device_row(dev: &Device, is_busy: bool) -> adw::ActionRow {
    let subtitle = if dev.connected {
        "Connected"
    } else if dev.paired {
        "Paired"
    } else {
        "Tap to pair"
    };
    let row = adw::ActionRow::builder()
        .title(&dev.alias)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    // The alias is chosen by whoever is broadcasting, so anyone in radio
    // range controls this title — and the row is activatable (it is what you
    // tap to pair). Markup off rather than escaped: nothing in this row wants
    // markup, and it covers the subtitle too (#753, cf. #30).
    markup::plain_text(&row);
    row.set_sensitive(!is_busy);
    if !dev.address.is_empty() {
        row.set_tooltip_text(Some(&dev.address));
    }

    // Prefix: device icon, or spinner while a D-Bus call is in flight.
    if is_busy {
        let spinner = gtk::Spinner::new();
        spinner.set_spinning(true);
        row.add_prefix(&spinner);
    } else {
        let icon_name = if dev.icon.is_empty() {
            "bluetooth-symbolic"
        } else {
            &dev.icon
        };
        let img = gtk::Image::from_icon_name(icon_name);
        row.add_prefix(&img);
    }

    // Battery suffix when reported.
    if let Some(pct) = dev.battery {
        let battery_lbl = gtk::Label::new(Some(&format!("{pct}%")));
        battery_lbl.add_css_class("dim-label");
        battery_lbl.set_tooltip_text(Some(&format!("Battery {pct}%")));
        row.add_suffix(&battery_lbl);
    }

    // Trust indicator: read-only star at row level. Actual toggle lives in
    // the ⋮ popover so it can't be hit by accident while reaching for
    // connect/disconnect.
    if dev.paired && dev.trusted {
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.set_tooltip_text(Some("Trusted — auto-reconnects"));
        star.add_css_class("dim-label");
        row.add_suffix(&star);
    }

    // ⋮ menu with Trust/Untrust + Forget. Only present on paired devices.
    if dev.paired {
        row.add_suffix(&build_device_menu(dev, is_busy));
    }

    // Row click → primary action (pair/connect/disconnect). Trust + Forget
    // are deliberately *not* reachable from this gesture so a misclick
    // can't untrust or unpair.
    let path = dev.path.clone();
    let connected = dev.connected;
    let paired = dev.paired;
    row.connect_activated(move |_| {
        if connected {
            bluetooth::disconnect_device(&path);
        } else if paired {
            bluetooth::connect_device(&path);
        } else {
            bluetooth::pair_device(&path);
        }
    });

    row
}

/// Per-device "⋮" popover menu. Holds Trust/Untrust and Forget so the
/// row's primary activation gesture can stay focused on connect/pair
/// without surfacing destructive controls in the click target.
fn build_device_menu(dev: &Device, is_busy: bool) -> gtk::MenuButton {
    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("view-more-symbolic");
    menu_btn.add_css_class("flat");
    menu_btn.set_valign(gtk::Align::Center);
    menu_btn.set_sensitive(!is_busy);
    menu_btn.set_tooltip_text(Some("Device options"));

    let popover = gtk::Popover::new();
    let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    pop_box.set_margin_start(6);
    pop_box.set_margin_end(6);
    pop_box.set_margin_top(6);
    pop_box.set_margin_bottom(6);

    let trust_lbl = if dev.trusted { "Untrust" } else { "Trust" };
    let trust_btn = gtk::Button::with_label(trust_lbl);
    trust_btn.add_css_class("flat");
    let path_t = dev.path.clone();
    let was_trusted = dev.trusted;
    let popover_for_trust = popover.clone();
    trust_btn.connect_clicked(move |_| {
        bluetooth::set_trusted(&path_t, !was_trusted);
        popover_for_trust.popdown();
    });
    pop_box.append(&trust_btn);

    let forget_btn = gtk::Button::with_label("Forget");
    forget_btn.add_css_class("flat");
    forget_btn.add_css_class("destructive-action");
    let path_f = dev.path.clone();
    let popover_for_forget = popover.clone();
    forget_btn.connect_clicked(move |_| {
        bluetooth::remove_device(&path_f);
        popover_for_forget.popdown();
    });
    pop_box.append(&forget_btn);

    popover.set_child(Some(&pop_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
}
