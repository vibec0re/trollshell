use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri::{self, Window};

use crate::components::diff::{DiffOp, plan_diff};
use crate::components::focus::{FocusTarget, yield_to_niri_focus};

/// Per-monitor list of windows on the monitor's currently-active workspace.
/// Each window is a button labeled with its title (falling back to app id).
/// Clicking focuses that window.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-windows");
    let connector = monitor.connector();
    // No `.dedupe_cloned()` here: `Window` (niri-ipc) derives only `Clone`,
    // not `PartialEq` (unlike `Workspace` on the sibling widget), so
    // `Vec<Window>` isn't comparable — see the PR description for why a
    // projected comparison key wasn't added instead. The keyed apply below,
    // plus `GtkLabel::set_label`/`set_tooltip_text` being no-ops on unchanged
    // text, already keeps title churn on *other* outputs cheap.
    let signal = active_workspace_windows(connector);

    // Window id -> button, so retitles and focus moves update the existing
    // pill in place instead of tearing the whole strip down (#229).
    let button_map: Rc<RefCell<HashMap<u64, gtk::Button>>> = Rc::new(RefCell::new(HashMap::new()));

    let container_for_signal = container.clone();
    bind(signal, &container, move |_, windows| {
        update_windows(&container_for_signal, &button_map, &windows);
    });

    container.upcast()
}

/// Keyed-diff update of the window pill strip. Mirrors `widgets/tray.rs`'s
/// `plan_diff`/`DiffOp` shape (shared via `components::diff`, #198/#229): key
/// on `Window::id`, reuse buttons whose id survives (updating label/tooltip/
/// focus in place), and only append/remove for actual membership changes.
fn update_windows(
    container: &gtk::Box,
    button_map: &Rc<RefCell<HashMap<u64, gtk::Button>>>,
    windows: &[Window],
) {
    // Take the map for the whole diff rather than holding a `RefMut` across it
    // (#643, spelling (2)): the binding this replaces stayed live past
    // `container.remove()`, `apply_window_visuals`, `container.append()` and
    // `insert_after()`. Same shape as `widgets/workspaces.rs`'s and
    // `widgets/tray.rs`'s diff loops. Stored back at the end.
    let mut map = button_map.take();

    let prev_keys: Vec<u64> = map.keys().copied().collect();
    let new_keys: Vec<u64> = windows.iter().map(|w| w.id).collect();
    let (ops, removed_keys) = plan_diff(&prev_keys, &new_keys);

    // ── 1. Remove obsolete pills ──────────────────────────────────────────
    for key in &removed_keys {
        if let Some(btn) = map.remove(key) {
            container.remove(&btn);
        }
    }

    // ── 2. Reuse stable pills; create new ones ────────────────────────────
    for (win, &op) in windows.iter().zip(ops.iter()) {
        match op {
            DiffOp::Reuse => {
                if let Some(btn) = map.get(&win.id) {
                    apply_window_visuals(btn, win);
                }
            }
            DiffOp::Create => {
                let btn = create_window_button(win);
                container.append(&btn); // position corrected in step 3
                map.insert(win.id, btn);
            }
        }
    }

    // ── 3. Reorder to match service order ─────────────────────────────────
    // gtk_widget_insert_after() repositions an already-parented child
    // without destroying it. None as previous_sibling → place as first
    // child.
    let mut prev: Option<gtk::Button> = None;
    for win in windows {
        if let Some(btn) = map.get(&win.id) {
            btn.insert_after(container, prev.as_ref());
            prev = Some(btn.clone());
        }
    }

    // The cell holds the empty `HashMap` `take()` left behind, so this
    // write-back drops nothing inside the borrow (#643).
    *button_map.borrow_mut() = map;
}

/// Window's button label: title, falling back to app id, falling back to a
/// generic placeholder carrying the window id.
fn window_label_text(win: &Window) -> String {
    win.title
        .clone()
        .or_else(|| win.app_id.clone())
        .unwrap_or_else(|| format!("window {}", win.id))
}

/// Re-apply the label, tooltip, and the focused class to an existing pill.
fn apply_window_visuals(btn: &gtk::Button, win: &Window) {
    let label_text = window_label_text(win);
    if let Some(label_widget) = btn.child().and_downcast::<gtk::Label>() {
        label_widget.set_label(&label_text);
    }
    btn.set_tooltip_text(Some(&label_text));
    if win.is_focused {
        btn.add_css_class("focused");
    } else {
        btn.remove_css_class("focused");
    }
}

/// Build a window pill for `win` and wire its click handler.
fn create_window_button(win: &Window) -> gtk::Button {
    let label_text = window_label_text(win);
    // Bounded label — ellipsize at render, tooltip has the full title.
    // Cap is tight (20 chars) because GtkCenterBox doesn't prevent
    // the left cluster from growing into the centered MPRIS row;
    // wider titles caused visible overlap in the wild.
    let label_widget = gtk::Label::new(Some(&label_text));
    label_widget.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label_widget.set_max_width_chars(30);
    let btn = gtk::Button::new();
    btn.set_child(Some(&label_widget));
    btn.set_tooltip_text(Some(&label_text));
    if win.is_focused {
        btn.add_css_class("focused");
    }
    let id = win.id;
    btn.connect_clicked(move |btn| {
        niri::focus_window(id);
        // Without this the bar's on-demand keyboard grab swallows the
        // focus the click just requested — see `components::focus`.
        // Re-arms once niri confirms this exact window is focused.
        yield_to_niri_focus(btn, FocusTarget::Window(id));
    });
    btn
}

/// Combine workspaces + windows into a per-monitor active-workspace
/// window list. Re-evaluates whenever either upstream signal changes.
pub fn active_workspace_windows(
    connector: Option<String>,
) -> impl hytte::futures_signals::signal::Signal<Item = Vec<Window>> {
    let workspaces = niri::workspaces();
    let windows = niri::windows();
    map_ref! {
        let ws_list = workspaces,
        let win_list = windows =>
        match ws_list.iter().find(|ws| ws.output == connector && ws.is_active).map(|ws| ws.id) {
            Some(id) => {
                let mut filtered: Vec<Window> = win_list
                    .iter()
                    .filter(|w| w.workspace_id == Some(id))
                    .cloned()
                    .collect();
                // niri arranges tiled windows by (column, tile-in-column),
                // 1-based, leftmost column first. Floating windows have
                // pos_in_scrolling_layout = None; push them after tiled
                // ones, ordered by id for stability.
                filtered.sort_by_key(|w| {
                    (
                        w.layout.pos_in_scrolling_layout.is_none(),
                        w.layout.pos_in_scrolling_layout.unwrap_or((usize::MAX, usize::MAX)),
                        w.id,
                    )
                });
                filtered
            }
            None => Vec::new(),
        }
    }
}
