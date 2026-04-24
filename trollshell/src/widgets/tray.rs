use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::tray::{self, MenuEntry, MenuItem, TrayItem};

pub fn widget() -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-tray");

    let container_for_signal = container.clone();
    bind(tray::items(), &container, move |_, items| {
        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for item in items {
            let btn = build_item_button(&item);
            container_for_signal.append(&btn);
        }
    });

    container.upcast()
}

fn build_item_button(item: &TrayItem) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-tray-item");

    let icon = gtk::Image::new();

    if !item.icon_name.is_empty() {
        // Prefer named icon from the icon theme.
        icon.set_icon_name(Some(&item.icon_name));
    } else if let Some((w, h, ref bytes)) = item.icon_pixmap {
        // Build a MemoryTexture from the ARGB32 pixmap data.
        // SNI uses ARGB32 in network byte order (big-endian), which maps to
        // gdk::MemoryFormat::A8r8g8b8.
        #[allow(clippy::cast_sign_loss)]
        let stride = (w as usize) * 4;
        let gbytes = glib::Bytes::from(bytes.as_slice());
        let texture = gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::A8r8g8b8, &gbytes, stride);
        icon.set_paintable(Some(&texture));
    } else {
        // Generic fallback.
        icon.set_icon_name(Some("application-x-executable-symbolic"));
    }

    btn.set_child(Some(&icon));

    // Rich tooltip: prefer tooltip fields, fall back to title.
    let tooltip_markup = if !item.tooltip_title.is_empty() {
        if item.tooltip_description.is_empty() {
            glib::markup_escape_text(&item.tooltip_title).to_string()
        } else {
            format!(
                "<b>{}</b>\n{}",
                glib::markup_escape_text(&item.tooltip_title),
                glib::markup_escape_text(&item.tooltip_description)
            )
        }
    } else if !item.title.is_empty() {
        glib::markup_escape_text(&item.title).to_string()
    } else {
        String::new()
    };

    if !tooltip_markup.is_empty() {
        btn.set_tooltip_markup(Some(&tooltip_markup));
    }

    // Primary click → Activate.
    let bus = item.bus_name.clone();
    let path = item.object_path.clone();
    btn.connect_clicked(move |_| tray::activate(&bus, &path));

    // Secondary click → open DBusMenu if available.
    if let Some(menu_path) = item.menu_path.clone() {
        let bus_name = item.bus_name.clone();
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        let btn_weak = btn.downgrade();
        gesture.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let bus_name = bus_name.clone();
            let menu_path = menu_path.clone();
            let btn_weak = btn_weak.clone();
            glib::MainContext::default().spawn_local(async move {
                let Some(btn) = btn_weak.upgrade() else {
                    return;
                };
                let menu = tray::fetch_menu(&bus_name, &menu_path).await;
                let items = match menu {
                    Some(m) => m.items,
                    None => return,
                };
                if items.is_empty() {
                    return;
                }
                let popover = build_menu_popover(&bus_name, &menu_path, &items);
                popover.set_parent(&btn);
                popover.popup();
            });
        });
        btn.add_controller(gesture);
    }

    btn
}

/// Build a `gtk::Popover` containing a vertical box of menu items.
fn build_menu_popover(
    bus_name: &str,
    menu_path: &str,
    items: &[MenuEntry],
) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("ts-tray-menu");

    let vbox = gtk::Box::new(gtk::Orientation::Vertical, 0);
    popover.set_child(Some(&vbox));

    for entry in items {
        match entry {
            MenuEntry::Separator => {
                let sep = gtk::Separator::new(gtk::Orientation::Horizontal);
                vbox.append(&sep);
            }
            MenuEntry::Item(item) => {
                let widget = build_menu_item_widget(bus_name, menu_path, item, &popover);
                vbox.append(&widget);
            }
        }
    }

    // Close the popover when it loses focus.
    popover.connect_closed(|p| {
        p.unparent();
    });

    popover
}

/// Build the widget for a single `MenuItem`.
fn build_menu_item_widget(
    bus_name: &str,
    menu_path: &str,
    item: &MenuItem,
    parent_popover: &gtk::Popover,
) -> gtk::Widget {
    // Build display label with toggle prefix.
    let label_text = if item.toggle_type == tray::ToggleType::None {
        item.label.clone()
    } else {
        let prefix = if item.toggle_state == 1 { "✓ " } else { "  " };
        format!("{prefix}{}", item.label)
    };

    if let Some(ref sub_items) = item.submenu {
        // Submenu button — clicking opens a child popover.
        let display = format!("{label_text} ▸");
        let btn = gtk::Button::with_label(&display);
        btn.set_sensitive(item.enabled);

        let sub_items_cloned = sub_items.clone();
        let bus_name = bus_name.to_string();
        let menu_path = menu_path.to_string();
        let btn_weak = btn.downgrade();
        btn.connect_clicked(move |_| {
            let Some(b) = btn_weak.upgrade() else { return };
            let sub_popover =
                build_menu_popover(&bus_name, &menu_path, &sub_items_cloned);
            sub_popover.set_parent(&b);
            sub_popover.popup();
        });

        btn.upcast()
    } else {
        // Standard leaf item.
        let btn = gtk::Button::with_label(&label_text);
        btn.set_sensitive(item.enabled);

        if !item.icon_name.is_empty() {
            let icon = gtk::Image::from_icon_name(&item.icon_name);
            btn.set_child(Some(&icon));
        }

        let bus_name = bus_name.to_string();
        let menu_path = menu_path.to_string();
        let item_id = item.id;
        let popover_weak = parent_popover.downgrade();
        btn.connect_clicked(move |_| {
            tray::menu_event(&bus_name, &menu_path, item_id);
            if let Some(p) = popover_weak.upgrade() {
                p.popdown();
            }
        });

        btn.upcast()
    }
}
