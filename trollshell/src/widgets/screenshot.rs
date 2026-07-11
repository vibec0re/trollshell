//! Screenshot bar chip — click opens niri's own interactive screenshot UI
//! (region/window selection is niri's UI, not trollshell's).
//!
//! Fire-and-forget: the click only sends `niri::screenshot()`. The resulting
//! "Screenshot saved" toast is wired **once, globally** in `main.rs` (not
//! here) — a per-monitor subscription in this widget would fire one toast
//! per bar on a multi-monitor setup for a single capture.
//!
//! Deliberately no Open/Copy action buttons on that toast: the #220 triage
//! flagged that a self-posted toast has no local-action-dispatch path today
//! (`notifications::invoke_action` only broadcasts `ActionInvoked` on the bus
//! for an external app to catch; nothing in trollshell subscribes to its
//! own). Wiring that is a follow-up, not this slice.

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
