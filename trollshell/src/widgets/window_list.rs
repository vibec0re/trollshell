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
    let signal = combined_windows_signal(connector);

    let container_for_signal = container.clone();
    bind(signal, &container, move |_, windows| {
        while let Some(child) = container_for_signal.first_child() {
            container_for_signal.remove(&child);
        }
        for win in windows {
            let label = win
                .title
                .clone()
                .or_else(|| win.app_id.clone())
                .unwrap_or_else(|| format!("window {}", win.id));
            let trimmed = truncate(&label, 40);
            let btn = gtk::Button::with_label(&trimmed);
            btn.set_tooltip_text(Some(&label));
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
fn combined_windows_signal(
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max - 1).collect();
        format!("{cut}…")
    }
}
