//! Bar chip → opens the [`Page::Workspaces`](crate::modal::Page::Workspaces)
//! drawer page (#1108, off epic #1071 §5).
//!
//! Icon-only, no live state of its own — modeled on `widgets::settings_chip`,
//! the closest sibling (a chip that only opens a drawer page, no signal
//! binding, no dynamic tooltip). `view-grid-symbolic` reads as "one column
//! per monitor" the way the Workspaces page itself lays out.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator(
        "ts-workspace-manager",
        crate::modal::Page::Workspaces,
        monitor,
    );
    btn.set_tooltip_text(Some("Workspaces"));

    let icon = gtk::Image::from_icon_name("view-grid-symbolic");
    icon.set_pixel_size(crate::scale::scale(16));
    btn.set_child(Some(&icon));

    btn.upcast()
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::widget;
    use hytte::gtk::{self, prelude::*};
    use hytte::prelude::*;
    use std::cell::RefCell;

    thread_local! {
        /// Mirrors `widgets::tasks`'s `TEST_MONITOR` cache — see that
        /// module's doc comment for why this is cached rather than re-run
        /// per test (`App::run`'s process-global side effects).
        static TEST_MONITOR: RefCell<Option<Monitor>> = const { RefCell::new(None) };
    }

    fn test_monitor() -> Monitor {
        if let Some(monitor) = TEST_MONITOR.with(|cell| cell.borrow().clone()) {
            return monitor;
        }
        App::new("mov.vibec0re.trollshell.test.workspace-manager")
            .run(|app| {
                let first = app.monitors().first().cloned();
                TEST_MONITOR.with(|cell| *cell.borrow_mut() = first);
                app.quit();
            })
            .expect("App::run");
        TEST_MONITOR
            .with(|cell| cell.borrow().clone())
            .expect("the display server must report at least one output; `xvfb-run` provides one")
    }

    /// The chip's own shape: an icon-only, focusable, clickable button
    /// carrying both the shared `ts-indicator` scaffold class and its own
    /// `ts-workspace-manager` class, with the "Workspaces" tooltip #1108
    /// asks for. What the click actually does (`modal::toggle` opening
    /// `Page::Workspaces`, and that page filling the drawer's width cap) is
    /// covered where the drawer internals it depends on are reachable —
    /// `modal.rs`'s own `gtk_tests` module.
    #[gtk::test]
    fn chip_is_an_icon_only_workspaces_indicator() {
        let monitor = test_monitor();
        let widget = widget(&monitor);
        let btn = widget
            .downcast::<gtk::Button>()
            .expect("workspace_manager::widget returns a Button");

        assert!(btn.has_css_class("ts-indicator"));
        assert!(btn.has_css_class("ts-workspace-manager"));
        assert_eq!(btn.tooltip_text().as_deref(), Some("Workspaces"));
        assert!(
            btn.child().is_some_and(|c| c.is::<gtk::Image>()),
            "chip child must be the icon, not a label"
        );
    }
}
