//! Bluetooth drawer panel — adapter power, scan/discovery, pairing
//! prompts, and per-device rows.
//!
//! Wraps `hytte::services::bluetooth` (`BlueZ` over D-Bus) plus
//! `hytte::services::bluetooth_audio` for the auto-switch toggle. Pair
//! prompts surface inline as a banner above the device list while
//! `BlueZ`'s `Agent1` callback is awaiting user response. A connect / pair /
//! disconnect failure (#1171) surfaces as a toast via
//! `notifications::post_local` — the row click itself stays fire-and-forget,
//! exactly like the equivalent Wi-Fi row in `panels/network/wifi.rs`. (Wi-Fi
//! calls `post_local` straight from the service; Bluetooth routes through a
//! handle because the failure text is assembled where the shared state is,
//! and the toast is then gated on being newer than the binding — see the
//! `built_at` comment in `panel_bluetooth`.)

use std::collections::HashSet;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self};
use hytte::prelude::*;
use hytte::services::bluetooth::{self, Device, PairPrompt, PromptKind};
use hytte::services::bluetooth_audio;
use hytte::services::notifications;

use crate::components::layout::{finish_page, page_box};
use crate::components::markup;

pub fn panel_bluetooth() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    column.append(&build_bluetooth_header());
    column.append(&build_pair_prompt_banner());
    column.append(&build_bluetooth_controls());
    column.append(&build_bluetooth_device_groups());

    // Connect/pair/disconnect failures (#1171) — the row's click handler
    // (`build_device_row`, below) is fire-and-forget, so a failure has no
    // other way to reach the user than this toast. Ignores the closure's own
    // widget param: nothing here is rendered, only forwarded.
    //
    // `built_at` is the freshness gate (#1192 review, MEDIUM-2). The service
    // handle is never cleared and `signal_cloned()` replays, so this
    // binding's *first* poll yields whatever failure is recorded — including
    // one from hours ago, on another monitor, already toasted at the time.
    // A page built now can only be interested in failures that happen from
    // now on; see `bluetooth::ActionError::at` for why this is a timestamp
    // rather than a clear-after-post (which would race between the
    // per-monitor binds).
    let built_at = std::time::Instant::now();
    bind(bluetooth::action_error(), &column, move |_column, err| {
        let Some(err) = err else { return };
        if !err.is_newer_than(built_at) {
            return;
        }
        notifications::post_local(
            "Bluetooth",
            "Bluetooth",
            &err.message,
            notifications::Urgency::Critical,
        );
    });

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

    let combined = map_ref! {
        let devs = bluetooth::devices(),
        let actions = bluetooth::device_actions() => {
            (devs.clone(), actions.clone())
        }
    };

    bind_device_groups(&outer, combined);

    outer.upcast()
}

/// Rebuild the Connected/Paired/Available boxed-list groups from `combined`
/// (device list + in-flight action paths) into `outer`. Split out of
/// [`build_bluetooth_device_groups`] so this `bind` call site's `WeakRef`
/// contract (#772) can be driven with a synthetic signal in tests, the same
/// way `reactive_list` is (#761/#771).
fn bind_device_groups<S>(outer: &gtk::Box, combined: S)
where
    S: Signal<Item = (Vec<Device>, HashSet<String>)> + 'static,
{
    bind(combined, outer, move |outer, (devs, actions)| {
        while let Some(child) = outer.first_child() {
            outer.remove(&child);
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
            outer.append(&group);
        }
    });
}

/// Banner shown above the device list while `BlueZ`'s `Agent1` callback is
/// waiting on the user to accept or reject a pairing. Hidden otherwise.
fn build_pair_prompt_banner() -> gtk::Widget {
    let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    prompt_box.add_css_class("ts-bluetooth-prompt");
    prompt_box.set_visible(false);
    bind_pair_prompt_banner(&prompt_box, bluetooth::pair_prompts());
    prompt_box.upcast()
}

/// Show/hide/populate the pairing-prompt banner from `signal`. Split out of
/// [`build_pair_prompt_banner`] so this `bind` call site's `WeakRef` contract
/// (#772) can be driven with a synthetic signal in tests.
fn bind_pair_prompt_banner<S>(prompt_box: &gtk::Box, signal: S)
where
    S: Signal<Item = Option<PairPrompt>> + 'static,
{
    bind(
        signal,
        prompt_box,
        move |prompt_box, prompt: Option<PairPrompt>| {
            while let Some(child) = prompt_box.first_child() {
                prompt_box.remove(&child);
            }
            let Some(p) = prompt else {
                prompt_box.set_visible(false);
                return;
            };
            prompt_box.set_visible(true);
            populate_pair_prompt(prompt_box, &p);
        },
    );
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
    // The handler's own argument *is* this entry, so taking it costs nothing
    // and a strong `entry.clone()` here would be the `bind`-pin defect one
    // layer down (#224/#831/#1176): the entry's handler list would own the
    // entry, so the prompt row could never be freed and every `prompt()`
    // emission would leak one. `submit_btn`'s capture above is a *different*
    // widget's handler holding this entry (the carve-out) and stays a clone.
    entry.connect_activate(move |entry| submit_entry(entry, numeric_only));
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
///
/// Both buttons dismiss the menu through a **weak** handle on the popover
/// (#1176). A strong `popover.clone()` captured by a handler on one of the
/// popover's own descendants is a refcount cycle GTK never breaks —
/// `popover → pop_box → trust_btn → closure → popover` — so the whole
/// four-widget subtree outlives its row. That matters here more than
/// anywhere else in the tree: `bind_device_groups` rebuilds *every* row on
/// *every* `bluetooth::devices()` emission, and an active scan emits several
/// times a second, so the leak is per device per emission with nobody
/// clicking anything. The idiom is `components/app_picker.rs`'s
/// (`picker_row` takes a `glib::WeakRef<gtk::Popover>`).
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
    let popover_for_trust = popover.downgrade();
    trust_btn.connect_clicked(move |_| {
        bluetooth::set_trusted(&path_t, !was_trusted);
        if let Some(popover) = popover_for_trust.upgrade() {
            popover.popdown();
        }
    });
    pop_box.append(&trust_btn);

    let forget_btn = gtk::Button::with_label("Forget");
    forget_btn.add_css_class("flat");
    forget_btn.add_css_class("destructive-action");
    let path_f = dev.path.clone();
    let popover_for_forget = popover.downgrade();
    forget_btn.connect_clicked(move |_| {
        bluetooth::remove_device(&path_f);
        if let Some(popover) = popover_for_forget.upgrade() {
            popover.popdown();
        }
    });
    pop_box.append(&forget_btn);

    popover.set_child(Some(&pop_box));
    menu_btn.set_popover(Some(&popover));
    menu_btn
}

/// #772 regression coverage: the two hand-rolled `bind` call sites in this
/// file (device groups, pair-prompt banner) must hold their container only
/// weakly, exactly like `reactive_list`'s own #761/#771 regression test.
///
/// Since #1176 it also covers the **other** direction of the same contract:
/// a `connect_*` handler on a widget that captures a strong clone of one of
/// that widget's own ancestors. `bind`'s `WeakRef` is useless if the widget
/// tree underneath it cannot be freed, and a popover dismissed from a button
/// inside itself is a cycle no `bind` is involved in at all — which is
/// exactly why `nix/lint-bind-pins.py` could not see it before #1176.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{
        Device, PairPrompt, bind_device_groups, bind_pair_prompt_banner, build_device_menu,
        build_text_entry_row,
    };
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use std::collections::HashSet;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// `bind_device_groups` must not keep its `outer` container alive by
    /// itself, per the #224 `WeakRef` contract at
    /// `hytte-reactive/src/bind.rs:16-22`. Falsified by reintroducing the
    /// `outer_for_bind` strong clone the apply closure used to capture.
    #[gtk::test]
    fn device_groups_binding_does_not_pin_outer() {
        adw::init().expect("libadwaita init");
        let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let weak = outer.downgrade();
        let combined: Mutable<(Vec<Device>, HashSet<String>)> =
            Mutable::new((Vec::new(), HashSet::new()));
        bind_device_groups(&outer, combined.signal_cloned());
        pump();

        drop(outer);

        assert!(
            weak.upgrade().is_none(),
            "bind_device_groups must not pin its outer container: a strong clone captured by \
             the apply closure (rather than taking the closure's own `outer` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's \
             WeakRef contract"
        );
    }

    /// Same contract for `bind_pair_prompt_banner`'s `prompt_box`. Falsified
    /// by reintroducing the `prompt_box_for_bind` strong clone.
    #[gtk::test]
    fn pair_prompt_banner_binding_does_not_pin_prompt_box() {
        adw::init().expect("libadwaita init");
        let prompt_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
        let weak = prompt_box.downgrade();
        let prompts: Mutable<Option<PairPrompt>> = Mutable::new(None);
        bind_pair_prompt_banner(&prompt_box, prompts.signal_cloned());
        pump();

        drop(prompt_box);

        assert!(
            weak.upgrade().is_none(),
            "bind_pair_prompt_banner must not pin its prompt_box: a strong clone captured by \
             the apply closure (rather than taking the closure's own `prompt_box` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's \
             WeakRef contract"
        );
    }

    /// A device's "⋮" menu must die with its row (#1176 item 1).
    ///
    /// `menu_btn → popover → pop_box → trust_btn` is the ownership chain, and
    /// the Trust/Forget handlers close it back to `popover` when they capture
    /// a strong clone to `popdown()` with. GTK cannot break a refcount cycle,
    /// so every row `bind_device_groups` retires — once per device per
    /// `devices()` emission, several times a second during a scan — leaks the
    /// whole four-widget subtree.
    ///
    /// Falsified by putting either `popover.clone()` back: the popover then
    /// upgrades after its menu button is gone.
    #[gtk::test]
    fn a_device_menu_dies_with_its_button() {
        adw::init().expect("libadwaita init");
        let dev = Device {
            path: "/org/bluez/hci0/dev_AA".to_owned(),
            alias: "Headphones".to_owned(),
            paired: true,
            trusted: true,
            ..Device::default()
        };

        let menu_btn = build_device_menu(&dev, false);
        let popover = menu_btn
            .popover()
            .expect("build_device_menu sets a popover on the menu button");
        let pop_box = popover.child().expect("the popover holds its button box");
        let weak_popover = popover.downgrade();
        let weak_box = pop_box.downgrade();
        drop(popover);
        drop(pop_box);

        drop(menu_btn);
        pump();

        assert!(
            weak_popover.upgrade().is_none(),
            "the device menu's popover must be freed with its menu button: a strong \
             `popover.clone()` captured by the Trust/Forget handlers — which live on buttons \
             *inside* that popover — is a cycle GTK never breaks, so an active scan leaks one \
             of these per device per `devices()` emission (#1176)"
        );
        assert!(
            weak_box.upgrade().is_none(),
            "the popover's whole child subtree must go with it, not just the popover"
        );
    }

    /// The **other direction** of the same weak handle, and the reason it is an
    /// `upgrade()` rather than a no-op: a Trust button whose popover handle
    /// silently resolved to `None` would satisfy
    /// `a_device_menu_dies_with_its_button` above exactly as well as a correct
    /// one does, while the menu simply stopped closing on click. Nothing else
    /// in the tree would notice.
    ///
    /// This is the shape every popdown site in `panels/{clipboard,vpn}.rs` and
    /// `panels/network/{wifi,wired}.rs` shares — same `popover.downgrade()`,
    /// same `upgrade()` guard, same sole strong owner in the `set_popover` one
    /// line down — so one live control covers the idiom. The `wifi`/`wired`/
    /// `vpn` sites additionally cannot be driven in-process: their handlers
    /// call a `wifi::`/`networkd::` command *before* the popdown, and
    /// `get_backend()` aborts the whole test binary with a non-unwinding panic
    /// when no service is registered. `bluetooth::set_trusted` is reachable
    /// because `mark_busy` returns early on an absent `shared_state()`.
    ///
    /// Falsified by replacing the `upgrade()` arm with a no-op: the popover
    /// stays visible after the click.
    #[gtk::test]
    fn the_trust_button_still_dismisses_the_menu() {
        adw::init().expect("libadwaita init");
        let dev = Device {
            path: "/org/bluez/hci0/dev_AA".to_owned(),
            alias: "Headphones".to_owned(),
            paired: true,
            trusted: false,
            ..Device::default()
        };

        let menu_btn = build_device_menu(&dev, false);
        let popover = menu_btn
            .popover()
            .expect("build_device_menu sets a popover on the menu button");
        let pop_box = popover.child().expect("the popover holds its button box");
        let trust_btn = pop_box
            .first_child()
            .expect("Trust leads the menu box")
            .downcast::<gtk::Button>()
            .expect("the first menu entry is a Button");
        assert_eq!(
            trust_btn.label().map(|s| s.to_string()).as_deref(),
            Some("Trust"),
            "an untrusted device's first menu entry is Trust — if this is the wrong button the \
             assertions below would be measuring something else"
        );

        // A popover needs a real toplevel: popping one up with no surface
        // realizes a popover with no window and segfaults rather than failing.
        let window = gtk::Window::new();
        window.set_child(Some(&menu_btn));
        window.present();
        pump();
        popover.popup();
        pump();
        assert!(
            popover.is_visible(),
            "the menu must actually be up before the click, or the assertion below passes \
             vacuously on a popover that was never open"
        );

        trust_btn.emit_clicked();
        pump();

        assert!(
            !popover.is_visible(),
            "the Trust button must still dismiss the menu it lives in: the handler holds the \
             popover through a `glib::WeakRef` (#1176), and a weak handle that never upgraded \
             would leave the menu open forever while every leak test stayed green"
        );

        window.destroy();
    }

    /// The pair-prompt PIN/passkey row must die with its prompt (#1176 item
    /// 3): the entry cloned itself into its **own** `connect_activate`
    /// handler, so the entry's handler list held the entry.
    ///
    /// Falsified by restoring `let entry_for_activate = entry.clone();` and
    /// the `submit_entry(&entry_for_activate, …)` body — the entry then
    /// upgrades after the row is dropped.
    #[gtk::test]
    fn a_pin_entry_dies_with_its_row() {
        adw::init().expect("libadwaita init");
        let row = build_text_entry_row(false);
        let entry = row.first_child().expect("the row leads with its entry");
        let weak_entry = entry.downgrade();
        drop(entry);

        drop(row);
        pump();

        assert!(
            weak_entry.upgrade().is_none(),
            "the PIN entry must be freed with its row: a strong `entry.clone()` captured by the \
             entry's own `connect_activate` handler keeps it alive forever, and a pair prompt \
             rebuilds this row on every `prompt()` emission (#1176)"
        );
    }
}
