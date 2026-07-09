use std::cell::RefCell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

use crate::components::chip::wire_scroll;
use crate::components::focus::{FocusTarget, yield_to_niri_focus};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-workspaces");

    let connector = monitor.connector();
    let connector_for_filter = connector.clone();
    let signal = niri::workspaces().map(move |all| {
        let mut filtered: Vec<_> = all
            .into_iter()
            .filter(|ws| ws.output == connector_for_filter)
            .collect();
        // Niri's WorkspacesChanged event doesn't guarantee sort order;
        // sort by idx so the buttons always go left-to-right 1..N.
        filtered.sort_by_key(|ws| ws.idx);
        filtered
    });

    // Latest filtered+sorted list for this monitor, shared with the scroll
    // handler below so it can find prev/next without re-deriving the
    // per-monitor filter from a fresh signal read.
    let current_workspaces: Rc<RefCell<Vec<niri::Workspace>>> = Rc::new(RefCell::new(Vec::new()));

    let container_for_signal = container.clone();
    let current_workspaces_for_bind = Rc::clone(&current_workspaces);
    bind(signal, &container, move |_, workspaces| {
        current_workspaces_for_bind
            .borrow_mut()
            .clone_from(&workspaces);

        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for ws in workspaces {
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
                // Same on-demand-keyboard tie-break as the window pills — the
                // bar would otherwise keep keyboard focus after the switch.
                // A workspace switch has no single target window id, so we
                // re-arm when the focused window simply changes (or on the
                // safety-net timeout).
                yield_to_niri_focus(btn, FocusTarget::WorkspaceSwitch);
            });
            container_for_signal.append(&btn);
        }
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
