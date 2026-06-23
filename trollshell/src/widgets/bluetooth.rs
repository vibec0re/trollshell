use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::bluetooth;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn =
        crate::components::chip::indicator("ts-bluetooth", crate::modal::Page::Bluetooth, monitor);

    let icon = gtk::Image::new();
    btn.set_child(Some(&icon));

    // Combine adapter + devices to pick the correct icon and visibility.
    let combined = map_ref! {
        let adapter = bluetooth::adapter(),
        let devs = bluetooth::devices() => {
            let any_connected = devs.iter().any(|d| d.connected);
            (adapter.clone(), any_connected)
        }
    };

    bind(
        combined,
        &btn,
        |w, (adapter, any_connected)| match &adapter {
            None => {
                w.set_visible(false);
            }
            Some(a) => {
                w.set_visible(true);
                let img = w
                    .child()
                    .and_downcast::<gtk::Image>()
                    .expect("button child is an Image");
                let icon_name = if a.powered && any_connected {
                    "bluetooth-active-symbolic"
                } else if a.powered {
                    "bluetooth-symbolic"
                } else {
                    "bluetooth-disabled-symbolic"
                };
                img.set_icon_name(Some(icon_name));
            }
        },
    );

    let _ = icon; // moved into button child above; keep reference for bind
    btn.upcast()
}
