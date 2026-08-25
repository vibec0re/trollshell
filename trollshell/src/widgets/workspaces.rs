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
    // Take the map for the whole diff rather than holding a `RefMut` across it
    // (#643, spelling (2)): the binding this replaces stayed live past
    // `container.remove()`, `apply_workspace_visuals`, `container.append()` and
    // `insert_after()`, so any synchronous emission re-entering this cell would
    // panic — and a `BorrowMutError` unwinding through a glib callback aborts
    // the process rather than failing the update. Stored back at the end.
    let mut map = button_map.take();

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

    // The cell holds the empty `HashMap` `take()` left behind — which allocates
    // nothing until first insert — so this write-back drops nothing inside the
    // borrow (#643).
    *button_map.borrow_mut() = map;
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

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::update_workspaces;
    use hytte::gtk::{self, prelude::*};
    use hytte::services::niri;
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    /// The pill map `update_workspaces` diffs against, exactly as `widget()`
    /// builds it. No `App`, no registry, no niri socket: every piece of state
    /// this function touches arrives through its own parameters, which is why
    /// this site needs no test-only seam at all (#674).
    type ButtonMap = Rc<RefCell<HashMap<u64, gtk::Button>>>;

    /// A `Workspace` on this test's single output. Only `id` and `idx` vary —
    /// the diff keys on `id`, and `apply_workspace_visuals` renders `idx` into
    /// the pill label, which is the emission the second test hangs off.
    fn ws(id: u64, idx: u8) -> niri::Workspace {
        niri::Workspace {
            id,
            idx,
            name: None,
            output: Some("DP-1".to_owned()),
            is_urgent: false,
            is_active: false,
            is_focused: false,
            active_window_id: None,
        }
    }

    /// Sorted key set of the shared cell, i.e. what the last write-back left.
    fn keys(map: &ButtonMap) -> Vec<u64> {
        let mut k: Vec<u64> = map.borrow().keys().copied().collect();
        k.sort_unstable();
        k
    }

    /// A fresh strip seeded with `initial`, as the first signal emission would.
    fn strip(initial: &[niri::Workspace]) -> (gtk::Box, ButtonMap) {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let map: ButtonMap = Rc::new(RefCell::new(HashMap::new()));
        update_workspaces(&container, &map, initial);
        (container, map)
    }

    /// Step 1 of the diff: `container.remove()` drops the last strong ref to an
    /// obsolete pill, so `GtkWidget::destroy` is emitted **synchronously** from
    /// dispose, inside the removal loop — the same measured fact
    /// `components::reactive_list`'s tripwire rests on.
    ///
    /// This test makes that handler re-enter `update_workspaces` on the same
    /// cell, which is exactly the shape this function's own comment claims to
    /// be safe against. Against the pre-#673
    /// `let mut map = button_map.borrow_mut();` the inner call hits a live
    /// `RefMut` and panics with `BorrowMutError` from inside a glib callback —
    /// which **aborts the test binary** rather than failing one test (#663's
    /// SIGABRT, the failure mode #674 exists for). With `take()` the cell is
    /// free for the whole diff, so the inner call simply finds an empty map.
    ///
    /// The guarantee under test is "no `BorrowMutError` abort", *not* that
    /// re-entrant updates compose: the outer call's closing write-back
    /// deliberately clobbers whatever the inner one stored, and the inner
    /// call's freshly-created pill is left orphaned in the container. That is
    /// fine — `bind` does not recurse into an in-call source, so production
    /// never re-enters here; the borrow discipline is defence in depth against
    /// a *new* synchronous emission being introduced into the loop.
    #[gtk::test]
    fn update_workspaces_tolerates_a_reentrant_update_from_a_removed_pill_destroy() {
        let (container, map) = strip(&[ws(1, 1), ws(2, 2)]);
        assert_eq!(
            keys(&map),
            [1, 2],
            "both pills tracked after the first pass"
        );

        // True only while the outer `update_workspaces` is on the stack, so the
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
                update_workspaces(&container, &map, &[ws(1, 1)]);
            });
        }
        // Drop our clone before the removing pass: while it lives the button
        // has a second strong ref, `container.remove()` won't dispose it, and
        // `destroy` never fires — the test would pass vacuously.
        drop(btn2);

        in_outer.set(true);
        update_workspaces(&container, &map, &[ws(1, 1)]);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed pill's `destroy` must fire synchronously inside `update_workspaces`; if \
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

    /// Step 2 of the diff, the *reuse* path: `apply_workspace_visuals` calls
    /// `GtkButton::set_label`, which emits `notify::label` synchronously (it is
    /// queued under the freeze/thaw pair inside `gtk_button_set_label`, not
    /// deferred to an idle). The pre-#673 `RefMut` stayed live across that call
    /// too, so this covers a second of the four emission points the function's
    /// comment names — a surviving pill, no destroy involved.
    ///
    /// Same falsification as above: with `borrow_mut()` held, the re-entrant
    /// call aborts on `BorrowMutError`.
    #[gtk::test]
    fn update_workspaces_does_not_hold_the_button_map_across_apply_workspace_visuals() {
        let (container, map) = strip(&[ws(1, 1)]);

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let btn = map.borrow()[&1].clone();
        {
            let container = container.clone();
            let map = Rc::clone(&map);
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            btn.connect_label_notify(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                update_workspaces(&container, &map, &[ws(1, 9)]);
            });
        }
        drop(btn);

        // Same id (so the pill is reused, not rebuilt), different idx (so the
        // label actually changes and `notify::label` fires).
        in_outer.set(true);
        update_workspaces(&container, &map, &[ws(1, 2)]);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "`set_label` must emit `notify::label` synchronously inside the reuse arm; if it stops \
             doing so this test no longer exercises the borrow at all"
        );
        assert_eq!(
            keys(&map),
            [1],
            "the reused pill must still be tracked after a re-entrant pass"
        );
    }
}
