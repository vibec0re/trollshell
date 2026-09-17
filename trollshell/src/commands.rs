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
//! # toggle the RIGHT sidebar (#1160, same shape, different verb):
//! busctl --user call mov.vibec0re.trollshell /mov/vibec0re/trollshell \
//!     org.gtk.Actions Activate 'sava{sv}' toggle-sidebar-right 0 0
//! ```
//!
//! ## Monitor resolution
//!
//! The verbs carry no monitor, so they target niri's focused output, read via
//! the shared [`crate::components::focused_output`] cache (also used by the
//! OSD and notification toasts) and handed to the modal/sidebar command
//! helpers, which fall back to any mounted surface when the focused output is
//! unknown.

use hytte::adw;
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
/// - `toggle-sidebar-right` (no arg): flip the right sidebar (#1160) — a no-op
///   while that output's right sidebar has no plugin card mounted.
/// - `dialog-close` (no arg): dismiss the plugin dialog (#1010) — a no-op when
///   none is up.
/// - `toggle-recording` (no arg): start/stop a screen recording (#403).
/// - `open-control-center` (no arg): start (or, if it's already running,
///   focus) the `trollshell-control-center` companion app — the same thing
///   the gear page's "Control Center" row does (#1304).
pub fn install(app: &App) {
    // Wire the shared focused-output cache (idempotent — see its docs) so
    // command handlers below can resolve the focused monitor.
    focused_output::install();
    app.add_action_entries(entries());
}

/// The `GActionEntry`s [`install`] registers, pulled into their own function
/// so [`tests::every_verb_registers_as_a_named_action`] can build them
/// against a plain `adw::Application` instead of the full `hytte_ui::App`
/// lifecycle — `App` exposes no lightweight test constructor (it wraps a
/// running `GApplication` plus the monitor-hotplug `Mutable`), and nothing
/// below actually needs one: every closure either ignores its `&Application`
/// parameter or reaches state through a free function
/// (`focused_output::current`, `sidebar::…`, `recorder::toggle`,
/// `companion::…`), none of which `install` sets up — that's
/// [`focused_output::install`]'s job, called once above.
fn entries() -> Vec<gio::ActionEntry<adw::Application>> {
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

    // The right sidebar (#1158/#1160). A separate verb rather than an argument
    // to `toggle-sidebar`: niri binds a chord to a verb, and `org.gtk.Actions`
    // parameters have to be spelled out in the `busctl` line, so two verbs are
    // two one-line binds while one parameterised verb is two long ones. It is
    // also the only way to reach the right sidebar — it has no bar chip (it may
    // not exist on this machine at all), which is why #219's "otherwise
    // mouse-only" framing is even stronger here.
    //
    // A toggle aimed at a connector whose right sidebar has no plugin card is a
    // no-op with one `debug!` line, decided inside `toggle_right_on_focused` —
    // the epic's "hidden entirely when empty" rule reaches the keybind, not just
    // the surface.
    let toggle_sidebar_right = gio::ActionEntry::builder("toggle-sidebar-right")
        .activate(|_app, _action, _param| {
            let focused = focused_output::current();
            sidebar::toggle_right_on_focused(focused.as_deref());
        })
        .build();

    // The plugin dialog (#1010). No `open`-shaped verb beside it: a dialog is
    // raised by a plugin's own `OpenPage(PluginSelf)` and names a plugin, which
    // a keybind has no way to pick — so the keyboard gets the half it is short
    // of. `Esc` already dismisses while the dialog holds the keyboard
    // exclusively; this reaches it from a bind regardless of focus, and is an
    // inert no-op when nothing is up.
    let dialog_close = gio::ActionEntry::builder("dialog-close")
        .activate(|_app, _action, _param| crate::overlays::dialog::close())
        .build();

    // Screen recording (#403): start if idle, stop if recording. A niri
    // keybind binds this like the others; the region is picked via `slurp`
    // when starting. No monitor resolution needed — the recorder is global.
    let toggle_recording = gio::ActionEntry::builder("toggle-recording")
        .activate(|_app, _action, _param| recorder::toggle())
        .build();

    // The control center companion app (#1304): the same route/launch
    // `panels::settings`'s "Control Center" row uses, so a niri keybind can
    // reach it without a mouse. Resolved fresh on each activation — see
    // `companion::resolve`'s doc for why that's cheap and correct.
    let open_control_center = gio::ActionEntry::builder("open-control-center")
        .activate(|_app, _action, _param| crate::companion::launch(&crate::companion::resolve()))
        .build();

    vec![
        open_page,
        power_menu,
        toggle_sidebar,
        toggle_sidebar_right,
        dialog_close,
        toggle_recording,
        open_control_center,
    ]
}

/// Open the drawer to `page` on the focused output (or any mounted drawer).
fn open_focused_page(page: Page) {
    let focused = focused_output::current();
    modal::open_on_focused(focused.as_deref(), page);
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::entries;
    use hytte::adw::{self, prelude::*};
    use hytte::gtk;

    /// [`entries`]'s verbs actually register as named `GAction`s on a real
    /// `adw::Application`'s action map — the same auto-exported
    /// `org.gtk.Actions` surface niri's `busctl … Activate` calls drive
    /// (module doc). Built against a plain `adw::Application` rather than the
    /// full `hytte_ui::App`/`AppBuilder` lifecycle, since [`entries`] was
    /// split out exactly so this doesn't need one — see its doc.
    ///
    /// Gated behind `system-tests` like `panels::settings`'s GTK test module:
    /// `adw::Application` still touches GTK's type system.
    ///
    /// **Falsification:** drop `open_control_center` from `entries`'s
    /// trailing `vec![…]` (#1304) → this reds,
    /// `lookup_action("open-control-center")` comes back `None`.
    #[gtk::test]
    fn every_verb_registers_as_a_named_action() {
        adw::init().expect("libadwaita init");
        let app = adw::Application::builder()
            .application_id("mov.vibec0re.trollshell.commands-test")
            .build();
        app.add_action_entries(entries());

        for verb in [
            "open-page",
            "power-menu",
            "toggle-sidebar",
            "toggle-sidebar-right",
            "dialog-close",
            "toggle-recording",
            "open-control-center",
        ] {
            assert!(
                app.lookup_action(verb).is_some(),
                "expected a registered action named {verb:?}"
            );
        }
    }
}
