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

    bind(signal, &container, move |container, windows| {
        update_windows(container, &button_map, &windows);
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

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::update_windows;
    use hytte::gtk::{self, prelude::*};
    use hytte::services::niri::{self, Window};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    /// The pill map `update_windows` diffs against, exactly as `widget()`
    /// builds it. No `App`, no registry, no niri socket: every piece of state
    /// this function touches arrives through its own parameters, which is why
    /// this site needs no test-only seam at all (#674, same finding as
    /// `widgets/workspaces.rs`'s #814).
    type ButtonMap = Rc<RefCell<HashMap<u64, gtk::Button>>>;

    /// A `Window` on a single workspace. Only `id` and `title` vary — the
    /// diff keys on `id`, and `window_label_text` renders `title` into the
    /// pill label, which is the emission the second test hangs off. The rest
    /// of the fields are unread by `update_windows`/`apply_window_visuals`/
    /// `create_window_button`.
    fn win(id: u64, title: &str) -> Window {
        Window {
            id,
            title: Some(title.to_owned()),
            app_id: None,
            pid: None,
            workspace_id: None,
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: niri::WindowLayout {
                pos_in_scrolling_layout: None,
                tile_size: (0.0, 0.0),
                window_size: (0, 0),
                tile_pos_in_workspace_view: None,
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    /// Sorted key set of the shared cell, i.e. what the last write-back left.
    fn keys(map: &ButtonMap) -> Vec<u64> {
        let mut k: Vec<u64> = map.borrow().keys().copied().collect();
        k.sort_unstable();
        k
    }

    /// A fresh strip seeded with `initial`, as the first signal emission would.
    fn strip(initial: &[Window]) -> (gtk::Box, ButtonMap) {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let map: ButtonMap = Rc::new(RefCell::new(HashMap::new()));
        update_windows(&container, &map, initial);
        (container, map)
    }

    /// Step 1 of the diff: `container.remove()` drops the last strong ref to
    /// an obsolete pill, so `GtkWidget::destroy` is emitted **synchronously**
    /// from dispose, inside the removal loop — the same measured fact
    /// `components::reactive_list`'s tripwire and `workspaces.rs`'s #814
    /// regression test both rest on.
    ///
    /// This test makes that handler re-enter `update_windows` on the same
    /// cell, which is exactly the shape this function's own comment claims to
    /// be safe against. Against the pre-#673 `let mut map =
    /// button_map.borrow_mut();` (with the closing write-back dropped) the
    /// inner call hits a live `RefMut` and panics with `BorrowMutError` from
    /// inside a glib callback — which **aborts the test binary** rather than
    /// failing one test (#663's SIGABRT, the failure mode #674 exists for).
    /// With `take()` the cell is free for the whole diff, so the inner call
    /// simply finds an empty map.
    ///
    /// The guarantee under test is "no `BorrowMutError` abort", *not* that
    /// re-entrant updates compose: the outer call's closing write-back
    /// deliberately clobbers whatever the inner one stored, and the inner
    /// call's freshly-created pill is left orphaned in the container. That is
    /// fine — `bind` does not recurse into an in-call source, so production
    /// never re-enters here; the borrow discipline is defence in depth
    /// against a *new* synchronous emission being introduced into the loop.
    #[gtk::test]
    fn update_windows_tolerates_a_reentrant_update_from_a_removed_pill_destroy() {
        let (container, map) = strip(&[win(1, "one"), win(2, "two")]);
        assert_eq!(
            keys(&map),
            [1, 2],
            "both pills tracked after the first pass"
        );

        // True only while the outer `update_windows` is on the stack, so the
        // handler can record whether it ran inside the diff or was deferred.
        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        // Arm pill 2's destroy to re-enter the diff on the same cell.
        let btn2 = map.borrow()[&2].clone();
        {
            let container = container.clone();
            let map = Rc::clone(&map);
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            btn2.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                update_windows(&container, &map, &[win(1, "one")]);
            });
        }
        // Drop our clone before the removing pass: while it lives the button
        // has a second strong ref, `container.remove()` won't dispose it, and
        // `destroy` never fires — the test would pass vacuously.
        drop(btn2);

        in_outer.set(true);
        update_windows(&container, &map, &[win(1, "one")]);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed pill's `destroy` must fire synchronously inside `update_windows`; if \
             GTK ever defers it, or the pill outlives the removal loop, this test proves nothing \
             about the borrow discipline"
        );
        assert_eq!(
            keys(&map),
            [1],
            "the outer call's write-back must still land: re-entry may not leave the cell holding \
             the inner diff's map or an empty one"
        );
    }

    /// Step 2 of the diff, the *reuse* path: unlike `workspaces.rs`'s
    /// `apply_workspace_visuals` (which calls `GtkButton::set_label`
    /// directly), `apply_window_visuals` renders the title into a separate
    /// child `GtkLabel` (`btn.child()`, see the CenterBox-overlap comment on
    /// `create_window_button`) and calls `set_label` on *that*. Same
    /// underlying property setter, same synchronous `notify::label` emission
    /// (queued under `gtk_label_set_label`'s freeze/thaw pair, not deferred
    /// to an idle) — just on the child widget instead of the button itself.
    /// The pre-#673 `RefMut` stayed live across that call too, so this covers
    /// a second of the four emission points the function's comment names — a
    /// surviving pill, no destroy involved.
    ///
    /// Same falsification as above: with `borrow_mut()` held, the re-entrant
    /// call aborts on `BorrowMutError`.
    #[gtk::test]
    fn update_windows_does_not_hold_the_button_map_across_apply_window_visuals() {
        let (container, map) = strip(&[win(1, "one")]);

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let btn = map.borrow()[&1].clone();
        let label_widget = btn
            .child()
            .and_downcast::<gtk::Label>()
            .expect("create_window_button always sets a Label child");
        {
            let container = container.clone();
            let map = Rc::clone(&map);
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            label_widget.connect_label_notify(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                update_windows(&container, &map, &[win(1, "still one")]);
            });
        }
        drop(btn);
        drop(label_widget);

        // Same id (so the pill is reused, not rebuilt), different title (so
        // the label actually changes and `notify::label` fires).
        in_outer.set(true);
        update_windows(&container, &map, &[win(1, "renamed")]);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "`set_label` on the pill's child Label must emit `notify::label` synchronously \
             inside the reuse arm; if it stops doing so this test no longer exercises the borrow \
             at all"
        );
        assert_eq!(
            keys(&map),
            [1],
            "the reused pill must still be tracked after a re-entrant pass"
        );
    }
}
