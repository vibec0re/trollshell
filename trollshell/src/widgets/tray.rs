use hytte::bus::OwnState;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::tray::{self, MenuEntry, MenuItem, TrayItem};

use crate::components::cast;
use crate::components::diff::{DiffOp, plan_diff};
use crate::widgets::contention::{self, Subject};
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

/// The words this widget's contended states are rendered with (#747).
const SUBJECT: Subject = Subject {
    headline: "The system tray is not receiving items",
    bus_name: "org.kde.StatusNotifierWatcher",
    rival: "another status-notifier host (a second bar, plasmashell, …)",
};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-tray");

    // #747: tray apps look `org.kde.StatusNotifierWatcher` up once and
    // register with whoever answers, so losing that name to a second bar means
    // no item ever reaches us and the tray is simply empty — which looks
    // exactly like "no app has a tray icon". This glyph is the only thing in
    // the group that can tell the two apart; its tooltip carries the reason.
    //
    // Appended before any item button and never touched by `update_tray`,
    // which only ever removes keys it owns. Step 3 of the diff reorders the
    // item buttons ahead of it, so it settles at the trailing edge of the
    // group — beside the tray rather than inside it, which is where a
    // meta-complaint about the tray belongs.
    let contended = gtk::Image::from_icon_name(contention::WARN_ICON);
    contended.set_pixel_size(crate::scale::scale(16));
    contended.set_visible(false);
    container.append(&contended);
    bind(tray::ownership(), &contended, |img, state: OwnState| {
        if let Some(msg) = contention::notice(&state, &SUBJECT) {
            img.set_tooltip_text(Some(&msg));
            img.set_visible(true);
        } else {
            img.set_tooltip_text(None);
            img.set_visible(false);
        }
    });

    // The map persists across signal emits on the GTK main thread.
    // Rc is fine here — bind() drives the closure via spawn_local.
    let button_map: Rc<RefCell<ButtonMap>> = Rc::new(RefCell::new(HashMap::new()));

    let monitor = monitor.clone();
    bind(tray::items(), &container, move |container, items| {
        update_tray(container, &button_map, &items, &monitor);
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

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{ButtonMap, update_tray};
    use hytte::gtk::{self, prelude::*};
    use hytte::prelude::*;
    use hytte::services::tray::{ItemStatus, TrayItem};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    /// The live map `update_tray` diffs against, exactly as `widget()` builds
    /// it: `ButtonMap` (this file's own private alias, keyed by `String`) is
    /// the *inner* map — this wraps it in the same `Rc<RefCell<…>>` `widget()`
    /// uses.
    type SharedMap = Rc<RefCell<ButtonMap>>;

    /// A `TrayItem` distinguished only by `key` — the diff keys on it, and no
    /// other field matters to either test below. Every field is `pub` (see
    /// `hytte_services::tray::TrayItem`), so a struct literal needs no
    /// constructor of its own, exactly like `workspaces.rs`'s local `ws()`
    /// builds a `niri::Workspace` directly.
    fn item(key: &str) -> TrayItem {
        TrayItem {
            key: key.to_owned(),
            bus_name: String::new(),
            object_path: String::new(),
            title: String::new(),
            icon_name: String::new(),
            status: ItemStatus::Passive,
            icon_pixmap: None,
            tooltip_title: String::new(),
            tooltip_description: String::new(),
            menu_path: None,
            item_is_menu: false,
        }
    }

    /// Sorted key set of the shared cell, i.e. what the last write-back left.
    fn keys(map: &SharedMap) -> Vec<String> {
        let mut k: Vec<String> = map.borrow().keys().cloned().collect();
        k.sort();
        k
    }

    /// One connected output — the one parameter `update_tray` needs that
    /// `workspaces.rs`'s/`window_list.rs`'s equivalents don't take at all.
    ///
    /// `hytte_ui::monitor::Monitor::new` is `pub(crate)` to `hytte-ui`, so
    /// nothing outside that crate can build a `Monitor` directly — a fake or
    /// mock one is not on the table. The only public source is
    /// [`App::monitors`]/[`App::monitors_changed`], which means a real
    /// `adw::Application` actually has to activate. Built once per process
    /// and cached in a thread-local: `#[gtk::test]` (`gtk4-macros`'
    /// `test_synced`) funnels *every* `#[gtk::test]` in this binary onto one
    /// dedicated, shared worker thread, so caching here is safe and avoids
    /// standing up a second `adw::Application` for the second test.
    fn test_monitor() -> Monitor {
        thread_local! {
            static CACHED: RefCell<Option<Monitor>> = const { RefCell::new(None) };
        }
        if let Some(m) = CACHED.with(|c| c.borrow().clone()) {
            return m;
        }
        let captured: Rc<RefCell<Option<Monitor>>> = Rc::new(RefCell::new(None));
        let captured_for_body = Rc::clone(&captured);
        App::new("mov.vibec0re.trollshell.test.tray-reentrancy")
            .run(move |app| {
                *captured_for_body.borrow_mut() = app.monitors().into_iter().next();
                app.quit();
            })
            .expect("App::run");
        let monitor = captured.borrow_mut().take().expect(
            "no output under this display server — not expected under xvfb-run \
             (modal_reentrancy.rs verified exactly one), but this helper is about \
             getting a Monitor for the reentrancy tests below, not about environment \
             monitor discovery",
        );
        CACHED.with(|c| *c.borrow_mut() = Some(monitor.clone()));
        monitor
    }

    /// A fresh strip seeded with `initial`, as the first signal emission would.
    fn strip(monitor: &Monitor, initial: &[TrayItem]) -> (gtk::Box, SharedMap) {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let map: SharedMap = Rc::new(RefCell::new(HashMap::new()));
        update_tray(&container, &map, initial, monitor);
        (container, map)
    }

    /// Step 1 of the diff: `container.remove()` drops the last strong ref to
    /// an obsolete button, so `GtkWidget::destroy` is emitted
    /// **synchronously** from dispose, inside the removal loop — the same
    /// measured fact `workspaces.rs`'s and `components::reactive_list`'s
    /// tripwires rest on.
    ///
    /// This test makes that handler re-enter `update_tray` on the same cell,
    /// which is exactly the shape this function's own #643 comment claims to
    /// be safe against. Against the pre-#673
    /// `let mut map = button_map.borrow_mut();` the inner call hits a live
    /// `RefMut` and panics with `BorrowMutError` from inside a glib
    /// callback — which **aborts the process** rather than failing one test
    /// (#663's SIGABRT, the failure mode #674 exists for). With `take()` the
    /// cell is free for the whole diff, so the inner call simply finds an
    /// empty map.
    ///
    /// The guarantee under test is "no `BorrowMutError` abort", *not* that
    /// re-entrant updates compose: the outer call's closing write-back
    /// deliberately clobbers whatever the inner one stored, and the inner
    /// call's freshly-created button is left orphaned in the container. That
    /// is fine — `bind` does not recurse into an in-call source, so
    /// production never re-enters here; the borrow discipline is defence in
    /// depth against a *new* synchronous emission being introduced into the
    /// loop.
    #[gtk::test]
    fn update_tray_tolerates_a_reentrant_update_from_a_removed_button_destroy() {
        let monitor = test_monitor();
        let (container, map) = strip(&monitor, &[item("a"), item("b")]);
        assert_eq!(
            keys(&map),
            ["a".to_owned(), "b".to_owned()],
            "both buttons tracked after the first pass"
        );

        // True only while the outer `update_tray` is on the stack, so the
        // handler can record whether it ran inside the diff or was deferred.
        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        // Arm button "b"'s destroy to re-enter the diff on the same cell.
        let btn_b = map.borrow()["b"].0.clone();
        {
            let container = container.clone();
            let map = Rc::clone(&map);
            let monitor = monitor.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            btn_b.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                update_tray(&container, &map, &[item("a")], &monitor);
            });
        }
        // Drop our clone before the removing pass: while it lives the button
        // has a second strong ref, `container.remove()` won't dispose it, and
        // `destroy` never fires — the test would pass vacuously.
        drop(btn_b);

        in_outer.set(true);
        update_tray(&container, &map, &[item("a")], &monitor);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed button's `destroy` must fire synchronously inside `update_tray`; if \
             GTK ever defers it, or the button outlives the removal loop, this test proves \
             nothing about the borrow discipline"
        );
        assert_eq!(
            keys(&map),
            ["a".to_owned()],
            "the outer call's write-back must still land: re-entry may not leave the cell \
             holding the inner diff's map or an empty one"
        );
    }

    /// Step 2 of the diff, the *reuse* path: `apply_item_button_visuals`
    /// always builds a brand-new `gtk::Image` and calls `GtkButton::set_child`
    /// on it, which emits `notify::child` synchronously — the button's
    /// `child` property genuinely changes on every call (a fresh `gtk::Image`
    /// object each time), so unlike `workspaces.rs`'s `notify::label` hook
    /// this doesn't depend on any particular `TrayItem` field actually
    /// differing between passes. The pre-#673 `RefMut` stayed live across
    /// that call too, so this covers a second of the four emission points
    /// the function's own #643 comment names — a surviving button, no
    /// destroy involved.
    ///
    /// Same falsification as above: with `borrow_mut()` held, the re-entrant
    /// call aborts on `BorrowMutError`.
    #[gtk::test]
    fn update_tray_does_not_hold_the_button_map_across_apply_item_button_visuals() {
        let monitor = test_monitor();
        let (container, map) = strip(&monitor, &[item("a")]);

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let btn = map.borrow()["a"].0.clone();
        {
            let container = container.clone();
            let map = Rc::clone(&map);
            let monitor = monitor.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            btn.connect_child_notify(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                update_tray(&container, &map, &[item("a")], &monitor);
            });
        }
        drop(btn);

        in_outer.set(true);
        update_tray(&container, &map, &[item("a")], &monitor);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "`set_child` must emit `notify::child` synchronously inside the reuse arm; if it \
             stops doing so this test no longer exercises the borrow at all"
        );
        assert_eq!(
            keys(&map),
            ["a".to_owned()],
            "the reused button must still be tracked after a re-entrant pass"
        );
    }
}
