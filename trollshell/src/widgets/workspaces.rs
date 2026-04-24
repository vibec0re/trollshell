use gtk::prelude::*;
use hytte::prelude::*;
use hytte::services::niri;

pub fn widget() -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("trollshell-workspaces");

    let container_for_signal = container.clone();
    bind(
        niri::workspaces(),
        &container,
        move |_, workspaces| {
            // Drop existing children.
            while let Some(child) = container_for_signal.first_child() {
                container_for_signal.remove(&child);
            }
            for ws in workspaces {
                let btn = gtk::Button::with_label(&ws.id.to_string());
                if ws.is_focused {
                    btn.add_css_class("focused");
                }
                if ws.is_active {
                    btn.add_css_class("active");
                }
                container_for_signal.append(&btn);
            }
        },
    );

    container.upcast()
}
