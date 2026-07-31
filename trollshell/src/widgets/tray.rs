use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::tray::{self, MenuEntry, MenuItem, TrayItem};

use crate::components::cast;
use crate::components::diff::{DiffOp, plan_diff};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Per-button live item data.  Click handlers capture this `Rc<RefCell<…>>`
/// and read it at activation time, so reused buttons always dispatch against
/// the most recent item state — even after many signal emits have updated the
/// cell since the button was first created.
type ItemCell = Rc<RefCell<TrayItem>>;

/// key → (button widget, live item cell).
type ButtonMap = HashMap<String, (gtk::Button, ItemCell)>;

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-tray");

    // The map persists across signal emits on the GTK main thread.
    // Rc is fine here — bind() drives the closure via spawn_local.
    let button_map: Rc<RefCell<ButtonMap>> = Rc::new(RefCell::new(HashMap::new()));

    let container_for_signal = container.clone();
    let monitor = monitor.clone();
    bind(tray::items(), &container, move |_, items| {
        update_tray(&container_for_signal, &button_map, &items, &monitor);
    });

    container.upcast()
}

/// Keyed-diff update of the tray container.
///
/// `tray::items()` fires on every `NewIcon`/`NewTitle`/`NewStatus`/`NewToolTip`
/// from any tray app.  This function avoids tearing down stable buttons on each
/// emit by keying on `TrayItem::key` (`"{bus_name}{object_path}"`):
///
/// 1. Remove buttons whose key is no longer present.
/// 2. For each item in service order: reuse the existing button (update cell +
///    visuals) or create a new one.
/// 3. Reorder the container children to match the service-published order.
///
/// The classification itself comes from the shared [`plan_diff`]/[`DiffOp`]
/// helper in `components::diff` (#198/#229/#578): this widget originated that
/// shape, `workspaces.rs`/`window_list.rs` generalised it out, and it now
/// keys on `String` here the same way they key on their own ids.
///
/// Click handlers are wired once at creation via [`create_item_button`].
/// They capture an `Rc<RefCell<TrayItem>>` and read it at activation time, so
/// they act on current data without stale captures.  Handlers are never
/// reconnected on reuse — the double-connect bug cannot occur here.
fn update_tray(
    container: &gtk::Box,
    button_map: &Rc<RefCell<ButtonMap>>,
    items: &[TrayItem],
    monitor: &Monitor,
) {
    // Take the map for the whole diff rather than holding a `RefMut` across it
    // (#643, spelling (2)): the binding this replaces stayed live past
    // `container.remove()`, `apply_item_button_visuals`, `create_item_button`,
    // `container.append()` and `insert_after()`. A synchronous emission
    // re-entering this cell would panic, and a `BorrowMutError` unwinding
    // through a glib callback aborts the process. Stored back at the end.
    let mut map = button_map.take();

    let prev_keys: Vec<String> = map.keys().cloned().collect();
    let new_keys: Vec<String> = items.iter().map(|i| i.key.clone()).collect();
    let (ops, removed_keys) = plan_diff(&prev_keys, &new_keys);

    // ── 1. Remove obsolete buttons ────────────────────────────────────────
    for key in &removed_keys {
        if let Some((btn, _)) = map.remove(key) {
            container.remove(&btn);
        }
    }

    // ── 2. Reuse stable buttons; create new ones ──────────────────────────
    for (item, &op) in items.iter().zip(ops.iter()) {
        match op {
            DiffOp::Reuse => {
                if let Some((btn, item_cell)) = map.get(&item.key) {
                    // Update live cell — handlers read this at click time.
                    *item_cell.borrow_mut() = item.clone();
                    // Re-apply visual properties (icon, tooltip).
                    apply_item_button_visuals(btn, item);
                }
            }
            DiffOp::Create => {
                let item_cell: ItemCell = Rc::new(RefCell::new(item.clone()));
                let btn = create_item_button(&item_cell, monitor);
                container.append(&btn); // position corrected in step 3
                map.insert(item.key.clone(), (btn, item_cell));
            }
        }
    }

    // ── 3. Reorder to match service order ─────────────────────────────────
    // gtk_widget_insert_after() repositions an already-parented child without
    // destroying it.  None as previous_sibling → place as first child.
    let mut prev: Option<gtk::Button> = None;
    for item in items {
        if let Some((btn, _)) = map.get(&item.key) {
            btn.insert_after(container, prev.as_ref());
            prev = Some(btn.clone());
        }
    }

    // The cell holds the empty `ButtonMap` `take()` left behind, so this
    // write-back drops nothing inside the borrow (#643).
    *button_map.borrow_mut() = map;
}

/// Create a tray button for `item_cell` and wire up click handlers.
///
/// Handlers are connected exactly once here and capture a clone of
/// `item_cell`.  They read the current item at activation time rather than
/// closing over snapshot values, so they remain correct across any number of
/// subsequent signal emits that update the cell.
///
/// Because handlers are never reconnected on reuse, there is no way for them
/// to stack up across emits — the double-connect bug cannot occur.
fn create_item_button(item_cell: &ItemCell, monitor: &Monitor) -> gtk::Button {
    let btn = gtk::Button::new();
    btn.add_css_class("ts-tray-item");

    // Clone the snapshot out rather than passing a live `Ref` (#643, spelling
    // (3)): `apply_item_button_visuals` is a run of GTK calls (icon build,
    // `set_child`, `set_tooltip_text`), and `update_tray`'s reuse arm holds the
    // `borrow_mut()` counterparty on this same cell.
    let item = item_cell.borrow().clone();
    apply_item_button_visuals(&btn, &item);

    // Primary click: Activate or show-menu depending on ItemIsMenu.
    {
        let item_cell = item_cell.clone();
        let btn_weak = btn.downgrade();
        let monitor = monitor.clone();
        btn.connect_clicked(move |_| {
            let item = item_cell.borrow();
            let item_is_menu = item.item_is_menu;
            let bus_name = item.bus_name.clone();
            let object_path = item.object_path.clone();
            let menu_path = item.menu_path.clone();
            drop(item); // release borrow before any async dispatch
            if item_is_menu {
                show_context_menu(
                    btn_weak.clone(),
                    bus_name,
                    object_path,
                    menu_path,
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
        let item_cell = item_cell.clone();
        let btn_weak = btn.downgrade();
        let monitor = monitor.clone();
        gesture.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let item = item_cell.borrow();
            let bus_name = item.bus_name.clone();
            let object_path = item.object_path.clone();
            let menu_path = item.menu_path.clone();
            drop(item);
            show_context_menu(
                btn_weak.clone(),
                bus_name,
                object_path,
                menu_path,
                monitor.clone(),
            );
        });
        btn.add_controller(gesture);
    }

    btn
}

/// Apply (or re-apply) the icon and tooltip from `item` to `btn`.
///
/// Safe to call on both freshly-created and reused buttons.  When reusing,
/// the old child widget is replaced and any previous tooltip is cleared if
/// the item no longer carries one.
///
/// # Icon fallback chain
///
/// 1. Named icon confirmed present in the current theme.
/// 2. Raw ARGB32 pixmap from `IconPixmap` (app-supplied bitmap).
/// 3. Named icon best-effort (theme may resolve it later).
/// 4. Generic symbolic fallback.
fn apply_item_button_visuals(btn: &gtk::Button, item: &TrayItem) {
    let icon = gtk::Image::new();

    // Robust fallback chain:
    //   1. Named icon if the current theme actually has it.
    //   2. Raw ARGB32 pixmap from IconPixmap (app-supplied bitmap).
    //   3. Named icon as best-effort (theme might resolve it at render time).
    //   4. Generic symbolic fallback.
    let icon_name_in_theme = !item.icon_name.is_empty()
        && gdk::Display::default()
            .is_some_and(|d| gtk::IconTheme::for_display(&d).has_icon(&item.icon_name));

    if icon_name_in_theme {
        icon.set_icon_name(Some(&item.icon_name));
    } else if let Some((w, h, ref bytes)) = item.icon_pixmap {
        // Build a MemoryTexture from the ARGB32 pixmap data.
        // SNI uses ARGB32 in network byte order (big-endian), which maps to
        // gdk::MemoryFormat::A8r8g8b8.
        let stride = cast::i32_to_usize(w) * 4;
        let gbytes = glib::Bytes::from(bytes.as_slice());
        let texture = gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::A8r8g8b8, &gbytes, stride);
        icon.set_paintable(Some(&texture));
    } else if !item.icon_name.is_empty() {
        // Best-effort: try the named icon even though the theme lookup did not
        // confirm it (no default display yet, or the icon may resolve later).
        icon.set_icon_name(Some(&item.icon_name));
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

    if tooltip_markup.is_empty() {
        // Clear any tooltip that was set on a previous emit.
        btn.set_tooltip_markup(None);
    } else {
        btn.set_tooltip_markup(Some(&tooltip_markup));
    }
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
                    // Nudge the popover to drop below the tray button (bar sits
                    // at the top edge). GTK falls back gracefully if the geometry
                    // doesn't fit.
                    popover.set_position(gtk::PositionType::Bottom);
                    popover.set_parent(&btn);
                    tracing::debug!(
                        tray_popover = "pre-popup",
                        bus = %bus_name,
                        path = %mp,
                        btn_has_root = btn.root().is_some(),
                        btn_width = btn.width(),
                        btn_height = btn.height(),
                    );
                    popover.popup();
                    tracing::debug!(
                        tray_popover = "post-popup",
                        bus = %bus_name,
                        path = %mp,
                        popover_visible = popover.is_visible(),
                        popover_has_parent = popover.parent().is_some(),
                    );
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
            // Same positioning nudge as the top-level menu popover.
            sub_popover.set_position(gtk::PositionType::Bottom);
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
