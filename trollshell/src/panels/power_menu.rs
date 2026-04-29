//! Drawer panel with system-power actions. Distinct from [`super::power`]
//! (the battery + brightness panel); this one is the lock / logout /
//! suspend / reboot / shutdown menu, ordered most-common at top,
//! most-destructive at bottom. Each row is an `AdwActionRow` whose
//! activation fires the action and dismisses the drawer.

use hytte::adw::{self, prelude::*};
use hytte::gtk;

use crate::components::layout::{finish_page, page_box};

pub fn panel_power_menu() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::new();

    group.add(&power_action_row(
        "Lock",
        "Lock the screen",
        "system-lock-screen-symbolic",
        None,
        || {
            hytte::services::screensaver::lock();
        },
    ));

    group.add(&power_action_row(
        "Logout",
        "End the niri session",
        "system-log-out-symbolic",
        None,
        || {
            // niri's `quit` shows its own confirmation overlay, which is the
            // right UX for a destructive session-end action. Pass `true` to
            // suppress it if this row should be the single point of
            // confirmation.
            hytte::services::niri::quit(false);
        },
    ));

    group.add(&power_action_row(
        "Suspend",
        "Sleep until next interaction",
        "system-suspend-symbolic",
        None,
        || {
            hytte::services::logind::suspend();
        },
    ));

    group.add(&power_action_row(
        "Reboot",
        "Restart the system",
        "system-reboot-symbolic",
        None,
        || {
            hytte::services::logind::reboot();
        },
    ));

    group.add(&power_action_row(
        "Shutdown",
        "Power off",
        "system-shutdown-symbolic",
        Some("destructive-action"),
        || {
            hytte::services::logind::poweroff();
        },
    ));

    column.append(&group);

    finish_page(&column)
}

/// Build one power-menu action row. `css_class` is for variants like
/// `destructive-action` on Shutdown. The callback runs on activation; the
/// drawer is dismissed afterwards so the user sees their action take effect.
fn power_action_row(
    title: &str,
    subtitle: &str,
    icon_name: &str,
    css_class: Option<&str>,
    on_activate: impl Fn() + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    let icon = gtk::Image::from_icon_name(icon_name);
    row.add_prefix(&icon);
    if let Some(class) = css_class {
        row.add_css_class(class);
    }
    row.connect_activated(move |_| {
        on_activate();
        crate::modal::dismiss_all();
    });
    row
}
