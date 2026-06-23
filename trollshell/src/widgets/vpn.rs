//! VPN bar chip — visible only when at least one VPN tunnel is up.
//!
//! Click opens `Page::Vpn` for the chip's monitor. All state comes from
//! `hytte::services::vpn::is_active()`; the widget itself does no
//! polling, no parsing, no process spawning.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::vpn;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator("ts-vpn", crate::modal::Page::Vpn, monitor);

    let icon = gtk::Image::from_icon_name("network-vpn-symbolic");
    btn.set_child(Some(&icon));

    bind_visible(vpn::is_active(), &btn);

    btn.upcast()
}
