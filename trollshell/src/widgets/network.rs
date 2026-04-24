use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::networkd::{self, Link, OperationalState};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-indicator");
    btn.add_css_class("ts-network");

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    bind(networkd::primary(), &icon, |w, primary| {
        w.set_icon_name(Some(icon_name(primary.as_ref())));
    });

    let monitor_for_click = monitor.clone();
    btn.connect_clicked(move |_| {
        crate::modal::toggle(&monitor_for_click, crate::modal::Page::Network);
    });
    btn.upcast()
}

fn icon_name(primary: Option<&Link>) -> &'static str {
    match primary.map(|l| l.operational) {
        Some(OperationalState::Routable) => "network-wired-symbolic",
        Some(OperationalState::Degraded | OperationalState::DegradedCarrier) => {
            "network-wired-acquiring-symbolic"
        }
        Some(_) => "network-wired-no-route-symbolic",
        None => "network-wired-disconnected-symbolic",
    }
}
