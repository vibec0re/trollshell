//! Screenshot bar chip — click opens niri's own interactive screenshot UI
//! (region/window selection is niri's UI, not trollshell's).
//!
//! Fire-and-forget: the click only sends `niri::screenshot()`. The resulting
//! "Screenshot saved" toast is wired **once, globally** in `main.rs` (not
//! here) — a per-monitor subscription in this widget would fire one toast
//! per bar on a multi-monitor setup for a single capture.
//!
//! That toast **does** carry Open/Copy action buttons (whenever niri wrote a
//! file — a clipboard-only capture has nothing to open). The local-action
//! dispatch path they need — `notifications::post_local_with_actions` +
//! `invoke_action`'s local-callback branch, which #220's triage flagged as
//! missing — shipped in #283, so a self-posted toast is no longer limited to
//! the outward-only `ActionInvoked` broadcast. See `main.rs`'s
//! `install_screenshot_toast` for the wiring and
//! `notifications::invoke_action`'s "Local dispatch" section for the
//! mechanism.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

pub fn widget(_monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-screenshot");
    btn.set_tooltip_text(Some("Take a screenshot"));

    let icon = gtk::Image::from_icon_name("camera-photo-symbolic");
    btn.set_child(Some(&icon));

    btn.connect_clicked(|_| niri::screenshot());

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
        App::new("mov.vibec0re.trollshell.test.screenshot-chip")
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
    fn css_classes_are_ts_indicator_and_ts_screenshot() {
        let w = widget(&test_monitor());
        // Not an exact-set comparison: an icon-only `GtkButton` also carries
        // GTK's own internal `image-button` class (added synchronously off
        // `set_child`), which is no part of this chip's contract — only the
        // two explicit `add_css_class` calls in `widget()` are.
        assert!(w.has_css_class("ts-indicator"));
        assert!(w.has_css_class("ts-screenshot"));
    }
}
