use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

use crate::components::focus::yield_to_niri_focus;

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

    let container_for_signal = container.clone();
    bind(signal, &container, move |_, workspaces| {
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
                yield_to_niri_focus(btn);
            });
            container_for_signal.append(&btn);
        }
    });

    container.upcast()
}
