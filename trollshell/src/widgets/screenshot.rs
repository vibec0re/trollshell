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
