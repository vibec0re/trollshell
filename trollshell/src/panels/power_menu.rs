//! Drawer panel with system-power actions. Distinct from [`super::power`]
//! (the battery + brightness panel); this one is the lock / logout /
//! suspend / reboot / shutdown menu, ordered most-common at top,
//! most-destructive at bottom. Each row is an `AdwActionRow` whose
//! activation fires the action and dismisses the drawer.
//!
//! Reboot and Shutdown are `confirm: true` rows: a single misclick on the
//! bottom two rows shouldn't power off the machine. The first activation
//! arms the row (retitled, `destructive-action`, auto-reverts after a few
//! seconds); the second activation within that window runs the real
//! action. See [`power_action_row`] for the arm/disarm mechanics.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib};

use crate::components::layout::{finish_page, page_box};

/// How long an armed confirm row stays armed before reverting to its idle
/// title/subtitle.
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);
/// Subtitle shown on a freshly-armed confirm row.
const ARMED_SUBTITLE: &str = "Click again \u{2014} reverts in 3 s";

pub fn panel_power_menu() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::new();

    group.add(&power_action_row(
        "Lock",
        "Lock the screen",
        "system-lock-screen-symbolic",
        None,
        false,
        || {
            hytte::services::screensaver::lock();
        },
    ));

    group.add(&power_action_row(
        "Logout",
        "End the niri session",
        "system-log-out-symbolic",
        None,
        false,
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
        // `system-suspend-symbolic` was dropped from Adwaita; the crescent-moon
        // weather glyph is the conventional "sleep" stand-in.
        "weather-clear-night-symbolic",
        None,
        false,
        || {
            hytte::services::logind::suspend();
        },
    ));

    group.add(&power_action_row(
        "Reboot",
        "Restart the system",
        "system-reboot-symbolic",
        None,
        true,
        || {
            hytte::services::logind::reboot();
        },
    ));

    group.add(&power_action_row(
        "Shutdown",
        "Power off",
        "system-shutdown-symbolic",
        Some("destructive-action"),
        true,
        || {
            hytte::services::logind::poweroff();
        },
    ));

    column.append(&group);

    finish_page(&column)
}

/// Per-row arm state for a `confirm: true` row, shared between the
/// activate handler, the revert timeout, and the unmap disarm handler.
/// Held in an `Rc` rather than captured by any of those closures'
/// enclosing widget, so there's no row→closure→row refcount cycle (the
/// row itself is always taken as the handler's argument, per GTK
/// convention).
#[derive(Default)]
struct ConfirmState {
    armed: Cell<bool>,
    /// The pending revert timeout, if the row is currently armed. Always
    /// cancelled before being replaced or cleared, so at most one revert
    /// timeout is ever in flight for a given row.
    timeout: Cell<Option<glib::SourceId>>,
}

impl ConfirmState {
    /// Cancel any pending revert timeout. Idempotent.
    fn cancel_timeout(&self) {
        if let Some(source) = self.timeout.take() {
            source.remove();
        }
    }
}

/// Build one power-menu action row. `css_class` is for variants like
/// `destructive-action` on Shutdown (applied unconditionally, independent
/// of `confirm`).
///
/// When `confirm` is true, the row is two-stage: the first activation
/// arms it (retitled to "Confirm {title, lowercased}?", subtitled with
/// the revert window, marked `destructive-action`) instead of running
/// `on_activate`; the second activation within [`CONFIRM_WINDOW`] runs
/// `on_activate` and dismisses the drawer, same as a non-confirm row. The
/// row disarms — cancelling any pending revert timeout — on its own
/// `unmap`, which fires both when the drawer is closed (the drawer window
/// hides on retract-finish) and when the stack navigates away from this
/// page, so a stale armed row never survives close/reopen. That's a
/// widget-local seam rather than a hook into `modal`'s per-monitor drawer
/// state, which isn't reachable from here (this panel is built
/// monitor-agnostically).
fn power_action_row(
    title: &str,
    subtitle: &str,
    icon_name: &str,
    css_class: Option<&str>,
    confirm: bool,
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

    if !confirm {
        row.connect_activated(move |_| {
            on_activate();
            crate::modal::dismiss_all();
        });
        return row;
    }

    // `css_class` may already be `destructive-action` (Shutdown); only add
    // and later remove it on arm/revert if it isn't already a permanent
    // fixture of the row.
    let base_destructive = css_class == Some("destructive-action");
    let idle_title = title.to_string();
    let idle_subtitle = subtitle.to_string();
    let armed_title = format!("Confirm {}?", title.to_lowercase());

    let state = Rc::new(ConfirmState::default());

    let state_for_activate = state.clone();
    // Separate clones for the activate closure (and the revert timeout it
    // spawns) — `idle_title`/`idle_subtitle` themselves are moved into the
    // `connect_unmap` closure below, so the activate closure can't just
    // move the originals too.
    let idle_title_for_activate = idle_title.clone();
    let idle_subtitle_for_activate = idle_subtitle.clone();
    row.connect_activated(move |row| {
        if state_for_activate.armed.replace(false) {
            // Second click within the window: disarm and run the action
            // for real.
            state_for_activate.cancel_timeout();
            on_activate();
            crate::modal::dismiss_all();
            return;
        }

        // First click: arm. Retitle, mark destructive, and (re-)schedule
        // the revert — cancel any stale timeout first so re-arming never
        // stacks timeouts.
        state_for_activate.armed.set(true);
        row.set_title(&armed_title);
        row.set_subtitle(ARMED_SUBTITLE);
        if !base_destructive {
            row.add_css_class("destructive-action");
        }

        state_for_activate.cancel_timeout();
        let state_for_timeout = state_for_activate.clone();
        let row_weak = row.downgrade();
        let idle_title_for_timeout = idle_title_for_activate.clone();
        let idle_subtitle_for_timeout = idle_subtitle_for_activate.clone();
        let source = glib::timeout_add_local_once(CONFIRM_WINDOW, move || {
            state_for_timeout.armed.set(false);
            // The `_once` source has already fired and removed itself; just
            // clear our reference to it (not `.remove()` — that would try
            // to remove an already-gone source).
            state_for_timeout.timeout.set(None);
            if let Some(row) = row_weak.upgrade() {
                row.set_title(&idle_title_for_timeout);
                row.set_subtitle(&idle_subtitle_for_timeout);
                if !base_destructive {
                    row.remove_css_class("destructive-action");
                }
            }
        });
        state_for_activate.timeout.set(Some(source));
    });

    let state_for_unmap = state.clone();
    row.connect_unmap(move |row| {
        state_for_unmap.cancel_timeout();
        if state_for_unmap.armed.replace(false) {
            row.set_title(&idle_title);
            row.set_subtitle(&idle_subtitle);
            if !base_destructive {
                row.remove_css_class("destructive-action");
            }
        }
    });

    row
}
