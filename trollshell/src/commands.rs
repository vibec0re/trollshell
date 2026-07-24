//! Command surface: `gio::ActionEntry`s registered on the `adw::Application`
//! so niri keybinds can drive shell surfaces that are otherwise mouse-only
//! (open a drawer page, the power menu, toggle the sidebar) — see #219.
//!
//! ## Why `GActions`, not a second owned bus name
//!
//! The `adw::Application` already owns `mov.vibec0re.trollshell` on the session
//! bus (single-instance `GApplication`), and a `GApplication` auto-exports its
//! own action group over `org.gtk.Actions` at the object path
//! `/mov/vibec0re/trollshell`. Registering actions there reuses that name — no
//! second `own_name`, no `#[zbus::interface]`, and the handlers fire on the
//! GTK main thread, so there's no thread hop to reach the drawer/sidebar state
//! (both live thread-local on the main thread).
//!
//! niri invokes a verb with `busctl` against `org.gtk.Actions.Activate`:
//!
//! ```sh
//! # open the power menu drawer (open-page takes a string arg):
//! busctl --user call mov.vibec0re.trollshell /mov/vibec0re/trollshell \
//!     org.gtk.Actions Activate 'sava{sv}' open-page 1 s power-menu 0
//! # toggle the sidebar (no arg):
//! busctl --user call mov.vibec0re.trollshell /mov/vibec0re/trollshell \
//!     org.gtk.Actions Activate 'sava{sv}' toggle-sidebar 0 0
//! ```
//!
//! ## Monitor resolution
//!
//! The verbs carry no monitor, so they target niri's focused output, read via
//! the shared [`crate::components::focused_output`] cache (also used by the
//! OSD and notification toasts) and handed to the modal/sidebar command
//! helpers, which fall back to any mounted surface when the focused output is
//! unknown.

use hytte::gtk::{gio, glib};
use hytte::prelude::*;
use hytte::services::recorder;

use crate::components::focused_output;
use crate::modal::{self, Page};
use crate::overlays::sidebar;

/// Register the shell command `GActions` on `app` and start tracking niri's
/// focused output. Call once from the body closure (post-activate, after the
/// niri service is registered).
///
/// Verbs:
/// - `open-page` (string arg): open the drawer to the named [`Page`]
///   (`Page::stack_name` token, e.g. `"media"`, `"power-menu"`).
/// - `power-menu` (no arg): convenience alias for `open-page("power-menu")`.
/// - `toggle-sidebar` (no arg): flip the left sidebar.
/// - `toggle-recording` (no arg): start/stop a screen recording (#403).
pub fn install(app: &App) {
    // Wire the shared focused-output cache (idempotent — see its docs) so
    // command handlers below can resolve the focused monitor.
    focused_output::install();

    let open_page = gio::ActionEntry::builder("open-page")
        .parameter_type(Some(glib::VariantTy::STRING))
        .activate(|_app, _action, param| {
            let Some(name) = param.and_then(glib::Variant::str) else {
                tracing::warn!("open-page: missing or non-string parameter");
                return;
            };
            let Some(page) = Page::from_stack_name(name) else {
                tracing::warn!(page = name, "open-page: unknown page name");
                return;
            };
            open_focused_page(page);
        })
        .build();

    // Redundant with `open-page("power-menu")`, kept as a trivial ergonomic
    // alias so a keybind can bind the power menu without an argument.
    let power_menu = gio::ActionEntry::builder("power-menu")
        .activate(|_app, _action, _param| open_focused_page(Page::PowerMenu))
        .build();

    let toggle_sidebar = gio::ActionEntry::builder("toggle-sidebar")
        .activate(|_app, _action, _param| {
            let focused = focused_output::current();
            sidebar::toggle_on_focused(focused.as_deref());
        })
        .build();

    // Screen recording (#403): start if idle, stop if recording. A niri
    // keybind binds this like the others; the region is picked via `slurp`
    // when starting. No monitor resolution needed — the recorder is global.
    let toggle_recording = gio::ActionEntry::builder("toggle-recording")
        .activate(|_app, _action, _param| recorder::toggle())
        .build();

    app.add_action_entries([open_page, power_menu, toggle_sidebar, toggle_recording]);
}

/// Open the drawer to `page` on the focused output (or any mounted drawer).
fn open_focused_page(page: Page) {
    let focused = focused_output::current();
    modal::open_on_focused(focused.as_deref(), page);
}
