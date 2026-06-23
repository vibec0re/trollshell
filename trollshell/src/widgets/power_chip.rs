use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;

/// Bar chip → opens the power-menu drawer page (Lock / Logout / Suspend /
/// Reboot / Shutdown).
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-power", crate::modal::Page::PowerMenu, monitor);

    let icon = gtk::Image::from_icon_name("system-shutdown-symbolic");
    btn.set_child(Some(&icon));

    btn.upcast()
}
