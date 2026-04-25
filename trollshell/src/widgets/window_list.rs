use hytte::futures_signals::map_ref;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri::{self, Window};

/// Per-monitor list of windows on the monitor's currently-active workspace.
/// Each window is a button labeled with its title (falling back to app id).
/// Clicking focuses that window.
pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    container.add_css_class("ts-windows");

    let connector = monitor.connector();
    let signal = active_workspace_windows(connector);

    let container_for_signal = container.clone();
    bind(signal, &container, move |_, windows| {
        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for win in windows {
            let label_text = win
                .title
                .clone()
                .or_else(|| win.app_id.clone())
                .unwrap_or_else(|| format!("window {}", win.id));
            // Bounded label — ellipsize at render, tooltip has full title.
            // Prevents the left cluster from pushing the right cluster off
            // the monitor when window titles or counts get large.
            let label_widget = gtk::Label::new(Some(&label_text));
            label_widget.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label_widget.set_max_width_chars(18);
            let btn = gtk::Button::new();
            btn.set_child(Some(&label_widget));
            btn.set_tooltip_text(Some(&label_text));
            if win.is_focused {
                btn.add_css_class("focused");
            }
            let id = win.id;
            btn.connect_clicked(move |_| niri::focus_window(id));
            container_for_signal.append(&btn);
        }
    });

    container.upcast()
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

