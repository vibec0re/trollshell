use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::tray::{self, MenuEntry, MenuItem, TrayItem};

use crate::components::cast;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-tray");

    let container_for_signal = container.clone();
    let monitor = monitor.clone();
    bind(tray::items(), &container, move |_, items| {
        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for item in items {
            let btn = build_item_button(&item, &monitor);
            container_for_signal.append(&btn);
        }
    });

    container.upcast()
}

fn build_item_button(item: &TrayItem, monitor: &Monitor) -> gtk::Button {
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
        let stride = cast::i32_to_usize(w) * 4;
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

    let bus_name = item.bus_name.clone();
    let object_path = item.object_path.clone();
    let menu_path = item.menu_path.clone();
    let item_is_menu = item.item_is_menu;

    // Primary click. Per SNI spec, when `ItemIsMenu` is true the app has no
    // separate primary action — left-click should show the menu instead.
    {
        let bus_name = bus_name.clone();
        let object_path = object_path.clone();
        let menu_path = menu_path.clone();
        let btn_weak = btn.downgrade();
        let monitor = monitor.clone();
        btn.connect_clicked(move |_| {
            if item_is_menu {
                show_context_menu(
                    btn_weak.clone(),
                    bus_name.clone(),
                    object_path.clone(),
                    menu_path.clone(),
                    monitor.clone(),
                );
            } else {
                tray::activate(&bus_name, &object_path);
            }
        });
    }

    // Secondary click → DBusMenu popover if available, otherwise fall back to
    // the SNI's own `ContextMenu(x, y)` method (apps without a DBusMenu still
    // expect right-click to do *something*).
    {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        // Capture phase so we see the event before gtk::Button's built-in
        // click gesture has a chance to swallow it.
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        let btn_weak = btn.downgrade();
        let bus_name = bus_name.clone();
        let object_path = object_path.clone();
        let menu_path = menu_path.clone();
        let monitor = monitor.clone();
        gesture.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            show_context_menu(
                btn_weak.clone(),
                bus_name.clone(),
                object_path.clone(),
                menu_path.clone(),
                monitor.clone(),
            );
        });
        btn.add_controller(gesture);
    }

    btn
}

/// Display the context menu for a tray item. Prefers the `com.canonical.dbusmenu`
/// popover when the app exports one; falls back to calling `ContextMenu(x, y)`
/// on the `StatusNotifierItem` so the app can show its own menu.
fn show_context_menu(
    btn_weak: glib::WeakRef<gtk::Button>,
    bus_name: String,
    object_path: String,
    menu_path: Option<String>,
    monitor: Monitor,
) {
    glib::MainContext::default().spawn_local(async move {
        let Some(btn) = btn_weak.upgrade() else {
            return;
        };

        if let Some(mp) = menu_path {
            let menu = tray::fetch_menu(&bus_name, &mp).await;
            if let Some(m) = menu {
                if !m.items.is_empty() {
                    let popover = build_menu_popover(&bus_name, &mp, &m.items, &monitor);
                    popover.set_parent(&btn);
                    popover.popup();
                    return;
                }
                tracing::debug!(bus = %bus_name, path = %mp, "DBusMenu empty, falling back to ContextMenu");
            } else {
                tracing::debug!(bus = %bus_name, path = %mp, "DBusMenu fetch failed, falling back to ContextMenu");
            }
        }

        // No DBusMenu (or it was empty/failed) — let the app show its own menu.
        tray::context_menu(&bus_name, &object_path);
    });
}

/// Build a `gtk::Popover` containing a vertical box of menu items.
///
/// The popover gets a full-screen outside-click catcher
/// ([`hytte::ui::attach_dismiss_catcher`]) because, hosted on the bar's
/// layer-shell surface under niri, its autohide grab isn't routed and it
/// would otherwise never dismiss on outside-click (issue #9).
fn build_menu_popover(
    bus_name: &str,
    menu_path: &str,
    items: &[MenuEntry],
    monitor: &Monitor,
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
                let widget = build_menu_item_widget(bus_name, menu_path, item, &popover, monitor);
                vbox.append(&widget);
            }
        }
    }

    hytte::ui::attach_dismiss_catcher(&popover, monitor);

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
    monitor: &Monitor,
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
        let monitor = monitor.clone();
        btn.connect_clicked(move |_| {
            let Some(b) = btn_weak.upgrade() else { return };
            let sub_popover =
                build_menu_popover(&bus_name, &menu_path, &sub_items_cloned, &monitor);
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
