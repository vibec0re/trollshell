//! Bar chip — toggles the per-monitor left sidebar on click. Mirrors the
//! shape of `widgets::settings_chip` (button + symbolic icon + indicator
//! CSS class). Mounts as the leftmost item in `main.rs::build_bar`'s
//! `.left([…])`.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-sidebar-toggle");

    // Bundled Material icon: modern Adwaita dropped `view-sidebar-symbolic`
    // (it lives at `sidebar-show-symbolic` now, but Material's view_sidebar
    // matches the rest of the bar's icon style — see icons/cpu.svg etc.).
    let icon = gtk::Image::from_file(crate::assets::path("icons/view-sidebar.svg"));
    icon.set_pixel_size(crate::scale::scale(16));
    btn.set_child(Some(&icon));

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::overlays::sidebar::toggle(&monitor_for_click);
    });

    btn.upcast()
}

/// #1177: pins this chip's CSS class set as a snapshot, taken **before** the
/// hand-rolled scaffold above is replaced with `components::chip::action_indicator`
/// — the two `add_css_class` calls in [`widget`] must survive that refactor
/// unchanged. Falsified by adding/removing/renaming either class.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use hytte::gtk::{self, prelude::*};
    use hytte::prelude::*;

    use super::widget;

    /// A real `Monitor`, captured from a one-shot `App::run` — the only way to
    /// get one (`hytte_ui::monitor::Monitor::new` is `pub(crate)` to
    /// `hytte-ui`). Cached in a thread-local: `#[gtk::test]` funnels every test
    /// in this binary onto one shared worker thread, mirroring
    /// `widgets::tray`'s `test_monitor`.
    fn test_monitor() -> Monitor {
        thread_local! {
            static CACHED: RefCell<Option<Monitor>> = const { RefCell::new(None) };
        }
        if let Some(m) = CACHED.with(|c| c.borrow().clone()) {
            return m;
        }
        let captured: Rc<RefCell<Option<Monitor>>> = Rc::new(RefCell::new(None));
        let captured_for_body = Rc::clone(&captured);
        App::new("mov.vibec0re.trollshell.test.sidebar-toggle-chip")
            .run(move |app| {
                *captured_for_body.borrow_mut() = app.monitors().into_iter().next();
                app.quit();
            })
            .expect("App::run");
        let monitor = captured.borrow_mut().take().expect(
            "no output under this display server — not expected under xvfb-run, which provides one",
        );
        CACHED.with(|c| *c.borrow_mut() = Some(monitor.clone()));
        monitor
    }

    #[gtk::test]
    fn css_classes_are_ts_indicator_and_ts_sidebar_toggle() {
        let w = widget(&test_monitor());
        // Not an exact-set comparison: an icon-only `GtkButton` also carries
        // GTK's own internal `image-button` class (added synchronously off
        // `set_child`), which is no part of this chip's contract — only the
        // two explicit `add_css_class` calls in `widget()` are.
        assert!(w.has_css_class("ts-indicator"));
        assert!(w.has_css_class("ts-sidebar-toggle"));
    }
}
