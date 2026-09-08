//! Drawer panel showing notification history grouped per app, plus a
//! Do-Not-Disturb toggle that suppresses future toasts.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Local};

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::Signal;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::{dnd, notifications, notifications_mute};

use crate::components::layout::{finish_page, page_box};
use crate::components::notif_actions;

pub fn panel_notifications() -> gtk::Widget {
    let column = page_box();

    // Do-Not-Disturb toggle. When on, non-critical toasts are suppressed;
    // history below still records every notification.
    let dnd_group = adw::PreferencesGroup::new();
    let dnd_row = adw::ActionRow::builder()
        .title("Do Not Disturb")
        .subtitle("Suppress toast popups; history still records.")
        .build();
    let dnd_switch = gtk::Switch::new();
    dnd_switch.set_valign(gtk::Align::Center);
    bind_two_way(dnd::enabled(), &dnd_switch, gtk::Switch::set_active, |w| {
        w.connect_active_notify(|sw| dnd::set_enabled(sw.is_active()))
    });
    dnd_row.add_suffix(&dnd_switch);
    dnd_row.set_activatable_widget(Some(&dnd_switch));
    dnd_group.add(&dnd_row);
    column.append(&dnd_group);

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header.set_margin_top(6);
    header.set_margin_bottom(6);
    let title = gtk::Label::new(Some("History"));
    title.add_css_class("ts-popup-headline");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    header.append(&title);
    let clear_btn = gtk::Button::with_label("Clear all");
    clear_btn.add_css_class("ts-notif-clear-btn");
    clear_btn.connect_clicked(|_| notifications::clear_history());
    header.append(&clear_btn);
    column.append(&header);

    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_vexpand(true);
    // Design-baseline px, routed through `scale()` so this floor tracks font
    // size / text-scaling too (#708) — unlike the two `set_max_content_height`
    // siblings above, this is a *minimum*, so leaving it raw was the more
    // annoying bypass: at large text scale it can force the history list
    // taller than its content actually needs.
    scrolled.set_min_content_height(crate::scale::scale(380));
    scrolled.add_css_class("ts-notif-history");

    // Group entries by app_name into per-app AdwExpanderRows. Each app row's
    // mute switch controls notifications_mute for future TOASTS only —
    // history always records.
    let groups_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    scrolled.set_child(Some(&groups_box));
    column.append(&scrolled);

    // Track the per-app ExpanderRows from the previous bind emission so we can
    // restore each row's `is_expanded()` state across the clear+rebuild —
    // otherwise an arriving notification collapses every open app row.
    let current_rows: Rc<RefCell<HashMap<String, adw::ExpanderRow>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let combined = map_ref! {
        let entries = notifications::history(),
        let muted = notifications_mute::muted_apps() => {
            (entries.clone(), muted.clone())
        }
    };
    bind_history_groups(&groups_box, combined, &current_rows);

    finish_page(&column)
}

/// Rebuild the per-app history groups inside `groups_box` from `signal`,
/// tracking the live `adw::ExpanderRow`s in `current_rows` so each row's
/// expanded state survives the clear+rebuild.
///
/// Split out of [`panel_notifications`] so this `bind` call site's `WeakRef`
/// contract (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven with a
/// synthetic signal in tests, the same extraction #772 made for
/// `bind_device_groups`. The builder reads `notifications::history()` and
/// `notifications_mute::muted_apps()` inline, both of which `.expect()`
/// without a registered `Registry` (#831).
fn bind_history_groups<S>(
    groups_box: &gtk::Box,
    signal: S,
    current_rows: &Rc<RefCell<HashMap<String, adw::ExpanderRow>>>,
) where
    S: Signal<Item = (Vec<notifications::HistoryEntry>, HashSet<String>)> + 'static,
{
    let current_rows = current_rows.clone();
    bind(signal, groups_box, move |groups_box, (entries, muted)| {
        // Stash prior expand-state keyed by app_name before teardown.
        let prior_expanded: HashMap<String, bool> = current_rows
            .borrow()
            .iter()
            .map(|(name, row)| (name.clone(), row.is_expanded()))
            .collect();
        current_rows.borrow_mut().clear();
        while let Some(child) = groups_box.first_child() {
            groups_box.remove(&child);
        }
        if entries.is_empty() {
            let group = adw::PreferencesGroup::new();
            let empty = adw::ActionRow::builder().title("No notifications").build();
            group.add(&empty);
            groups_box.append(&group);
            return;
        }
        // Group entries by app_name, preserving newest-first ordering by
        // walking entries (already newest-first) and pushing into per-app
        // Vec<&HistoryEntry> on first sighting.
        let mut order: Vec<String> = Vec::new();
        let mut buckets: HashMap<String, Vec<&notifications::HistoryEntry>> = HashMap::new();
        for entry in &entries {
            // freedesktop spec allows empty `app_name`; substitute "Unknown"
            // so we don't render a blank ExpanderRow or persist "" to the
            // muted-apps file when the user toggles its switch.
            let key = if entry.app_name.trim().is_empty() {
                "Unknown".to_string()
            } else {
                entry.app_name.clone()
            };
            if !buckets.contains_key(&key) {
                order.push(key.clone());
            }
            buckets.entry(key).or_default().push(entry);
        }
        let group = adw::PreferencesGroup::new();
        for app in &order {
            let bucket = buckets.get(app).expect("bucket present for tracked app");
            let row = build_history_app_row(app, bucket, &muted);
            if prior_expanded.get(app).copied().unwrap_or(false) {
                row.set_expanded(true);
            }
            group.add(&row);
            current_rows.borrow_mut().insert(app.clone(), row);
        }
        groups_box.append(&group);
    });
}

/// Build the `AdwExpanderRow` for a single app's history bucket.
fn build_history_app_row(
    app: &str,
    entries: &[&notifications::HistoryEntry],
    muted: &HashSet<String>,
) -> adw::ExpanderRow {
    const MAX_PER_APP: usize = 20;

    let row = adw::ExpanderRow::builder().title(app).build();
    let count = entries.len();
    if let Some(latest) = entries.first() {
        let subtitle = if count == 1 {
            latest.summary.clone()
        } else {
            format!("{} · {} entries", latest.summary, count)
        };
        row.set_subtitle(&subtitle);
    }

    // Per-app mute switch.
    let mute_switch = gtk::Switch::new();
    mute_switch.set_valign(gtk::Align::Center);
    mute_switch.set_tooltip_text(Some("Mute toasts from this app"));
    mute_switch.set_active(muted.contains(app));
    let app_for_bind = app.to_string();
    let app_for_handler = app.to_string();
    bind_two_way(
        notifications_mute::muted_apps().map(move |m| m.contains(&app_for_bind)),
        &mute_switch,
        gtk::Switch::set_active,
        move |w| {
            w.connect_active_notify(move |sw| {
                notifications_mute::set_app_muted(&app_for_handler, sw.is_active());
            })
        },
    );
    row.add_suffix(&mute_switch);

    for entry in entries.iter().take(MAX_PER_APP) {
        row.add_row(&build_history_action_row(entry));
    }

    row
}

fn build_history_action_row(entry: &notifications::HistoryEntry) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(&entry.summary).build();
    if !entry.body.is_empty() {
        row.set_subtitle(&entry.body);
    }
    if entry.urgency == notifications::Urgency::Critical {
        row.add_css_class("critical");
    }

    // Time stamp on the left side as a prefix (small label).
    let time_label = gtk::Label::new(Some(&fmt_notif_time(entry.dismissed_at)));
    time_label.add_css_class("dim-label");
    time_label.set_valign(gtk::Align::Center);
    row.add_prefix(&time_label);

    // Action buttons (cap at 3 — same as toasts). The reserved `default`
    // action is excluded — it's not meant to render as a button (see
    // `notif_actions`).
    let mut visible = notif_actions::visible_actions(&entry.actions).peekable();
    if visible.peek().is_some() {
        let actions_box = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        actions_box.set_valign(gtk::Align::Center);
        for action in visible.take(3) {
            let btn = gtk::Button::with_label(&action.label);
            btn.add_css_class("flat");
            let id = entry.id;
            let key = action.key.clone();
            btn.connect_clicked(move |_| {
                notifications::invoke_action(id, &key);
            });
            actions_box.append(&btn);
        }
        row.add_suffix(&actions_box);
    }

    row
}

fn fmt_notif_time(unix_secs: u64) -> String {
    let dt =
        DateTime::<Local>::from(std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix_secs));
    dt.format("%H:%M").to_string()
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&gtk::Box` `bind` hands it rather than a strong
/// clone captured from the enclosing scope, or the binding keeps the box
/// alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
///
/// The emission driven here is the **empty** one: a non-empty history builds
/// per-app rows through [`build_history_app_row`], whose mute switch is a
/// `bind_two_way` on `notifications_mute::muted_apps()` — a service accessor
/// that `.expect()`s without a registered `Registry`. The empty branch is
/// still an emission of the same closure into the same widget, which is what
/// both assertions below are about.
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use hytte::services::notifications::HistoryEntry;

    use super::bind_history_groups;

    type Emission = (Vec<HistoryEntry>, HashSet<String>);

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn history_groups_binding_applies_a_value() {
        adw::init().expect("libadwaita init");
        let groups_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let current_rows: Rc<RefCell<HashMap<String, adw::ExpanderRow>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let history: Mutable<Emission> = Mutable::new((Vec::new(), HashSet::new()));
        bind_history_groups(&groups_box, history.signal_cloned(), &current_rows);
        pump();

        assert!(
            groups_box.first_child().is_some(),
            "the empty-history emission must reach the box as the \"No notifications\" group"
        );
    }

    /// Falsified by reintroducing the `groups_for_signal` strong clone the
    /// apply closure used to capture: with it, `drop(groups_box)` is not the
    /// last strong ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn history_groups_binding_does_not_pin_groups_box() {
        adw::init().expect("libadwaita init");
        let groups_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let weak = groups_box.downgrade();
        let current_rows: Rc<RefCell<HashMap<String, adw::ExpanderRow>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let history: Mutable<Emission> = Mutable::new((Vec::new(), HashSet::new()));
        bind_history_groups(&groups_box, history.signal_cloned(), &current_rows);
        pump();

        drop(groups_box);

        assert!(
            weak.upgrade().is_none(),
            "bind_history_groups must not pin its groups_box: a strong clone captured by the \
             apply closure (rather than taking the closure's own `&gtk::Box` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's WeakRef \
             contract"
        );
    }
}
