use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

use crate::components::chip::wire_scroll;
use crate::components::diff::{DiffOp, plan_diff};
use crate::components::focus::{FocusTarget, yield_to_niri_focus};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-workspaces");

    let connector = monitor.connector();
    let connector_for_filter = connector.clone();
    let signal = niri::workspaces()
        .map(move |all| {
            let mut filtered: Vec<_> = all
                .into_iter()
                .filter(|ws| ws.output == connector_for_filter)
                .collect();
            // Niri's WorkspacesChanged event doesn't guarantee sort order;
            // sort by idx so the buttons always go left-to-right 1..N.
            filtered.sort_by_key(|ws| ws.idx);
            filtered
        })
        // `Workspace` (niri-ipc) derives `PartialEq`/`Eq`, unlike `Window` —
        // skip the apply entirely when this monitor's filtered+sorted list
        // is byte-for-byte unchanged (e.g. a WorkspacesChanged event that
        // only touched another output).
        .dedupe_cloned();

    // Latest filtered+sorted list for this monitor, shared with the scroll
    // handler below so it can find prev/next without re-deriving the
    // per-monitor filter from a fresh signal read.
    let current_workspaces: Rc<RefCell<Vec<niri::Workspace>>> = Rc::new(RefCell::new(Vec::new()));

    // Workspace id -> button, so repeat emits update pills in place instead
    // of tearing the whole strip down and rebuilding it (#229).
    let button_map: Rc<RefCell<HashMap<u64, gtk::Button>>> = Rc::new(RefCell::new(HashMap::new()));

    let container_for_signal = container.clone();
    let current_workspaces_for_bind = Rc::clone(&current_workspaces);
    bind(signal, &container, move |_, workspaces| {
        current_workspaces_for_bind
            .borrow_mut()
            .clone_from(&workspaces);

        update_workspaces(&container_for_signal, &button_map, &workspaces);
    });

    // Scroll over the pill strip to cycle the active workspace on this
    // monitor. Steps relative to the *active* workspace on this output —
    // not the globally-focused one, since focus can currently be on another
    // monitor while this strip still shows its own active pill — and clamps
    // at both ends rather than wrapping.
    let container_for_scroll = container.clone();
    let current_workspaces_for_scroll = Rc::clone(&current_workspaces);
    wire_scroll(&container, move |direction| {
        let list = current_workspaces_for_scroll.borrow();
        if list.is_empty() {
            return;
        }
        let Some(current_idx) = list.iter().position(|ws| ws.is_active) else {
            return;
        };
        let new_idx = if direction > 0.0 {
            (current_idx + 1).min(list.len() - 1)
        } else {
            current_idx.saturating_sub(1)
        };
        if new_idx == current_idx {
            return;
        }
        let id = list[new_idx].id;
        drop(list);
        niri::focus_workspace(id);
        yield_to_niri_focus(&container_for_scroll, FocusTarget::WorkspaceSwitch);
    });

    container.upcast()
}

/// Keyed-diff update of the workspace pill strip. Mirrors `widgets/tray.rs`'s
/// `plan_diff`/`DiffOp` shape (shared via `components::diff`, #198/#229): key
/// on `Workspace::id`, reuse buttons whose id survives (updating the label
/// and the focused/active classes in place), and only append/remove for
/// actual membership changes. The container-level scroll controller (#215)
/// wired in `widget()` above keeps working unchanged — an in-place update
/// never touches the container itself.
fn update_workspaces(
    container: &gtk::Box,
    button_map: &Rc<RefCell<HashMap<u64, gtk::Button>>>,
    workspaces: &[niri::Workspace],
) {
    let mut map = button_map.borrow_mut();

    let prev_keys: Vec<u64> = map.keys().copied().collect();
    let new_keys: Vec<u64> = workspaces.iter().map(|ws| ws.id).collect();
    let (ops, removed_keys) = plan_diff(&prev_keys, &new_keys);

    // ── 1. Remove obsolete pills ──────────────────────────────────────────
    for key in &removed_keys {
        if let Some(btn) = map.remove(key) {
            container.remove(&btn);
        }
    }

    // ── 2. Reuse stable pills; create new ones ────────────────────────────
    for (ws, &op) in workspaces.iter().zip(ops.iter()) {
        match op {
            DiffOp::Reuse => {
                if let Some(btn) = map.get(&ws.id) {
                    apply_workspace_visuals(btn, ws);
                }
            }
            DiffOp::Create => {
                let btn = create_workspace_button(ws);
                container.append(&btn); // position corrected in step 3
                map.insert(ws.id, btn);
            }
        }
    }

    // ── 3. Reorder to match service order ─────────────────────────────────
    // gtk_widget_insert_after() repositions an already-parented child
    // without destroying it. None as previous_sibling → place as first
    // child.
    let mut prev: Option<gtk::Button> = None;
    for ws in workspaces {
        if let Some(btn) = map.get(&ws.id) {
            btn.insert_after(container, prev.as_ref());
            prev = Some(btn.clone());
        }
    }
}

/// Re-apply the label and the focused/active classes to an existing pill.
fn apply_workspace_visuals(btn: &gtk::Button, ws: &niri::Workspace) {
    btn.set_label(&ws.idx.to_string());
    if ws.is_focused {
        btn.add_css_class("focused");
    } else {
        btn.remove_css_class("focused");
    }
    if ws.is_active {
        btn.add_css_class("active");
    } else {
        btn.remove_css_class("active");
    }
}

/// Build a workspace pill for `ws` and wire its click handler.
fn create_workspace_button(ws: &niri::Workspace) -> gtk::Button {
    let btn = gtk::Button::with_label(&ws.idx.to_string());
    if ws.is_focused {
        btn.add_css_class("focused");
    }
    if ws.is_active {
        btn.add_css_class("active");
    }
    let id = ws.id;
    btn.connect_clicked(move |btn| {
        niri::focus_workspace(id);
        // Same on-demand-keyboard tie-break as the window pills — the bar
        // would otherwise keep keyboard focus after the switch. A workspace
        // switch has no single target window id, so we re-arm when the
        // focused window simply changes (or on the safety-net timeout).
        yield_to_niri_focus(btn, FocusTarget::WorkspaceSwitch);
    });
    btn
}
