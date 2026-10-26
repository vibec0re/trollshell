use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;

use crate::components::chip::wire_scroll;
use crate::components::diff::{DiffOp, plan_diff};
use crate::components::focus::{FocusTarget, yield_to_niri_focus};

/// How long the strip steps from its **own** last scroll request rather than
/// from the active workspace niri reports.
///
/// Every notch used to step from the list's `is_active` pill, and that list
/// only moves once niri has answered the `focus_workspace` and its event has
/// come back round. A fast flick lands several notches before that — GDK
/// compresses a burst into one `scroll` with `|dy| > 1`, so they can even
/// arrive inside a single callback — and every one of them computed the same
/// "active + 1", so N notches moved one workspace. Inside this window a notch
/// steps from the workspace the strip last asked for instead: the shape of the
/// brightness chip's `pending` (#1495).
///
/// The hold rarely runs out in practice, because the binding clears the
/// pending request the moment niri reports it active ([`bind_workspace_pills`]).
/// It is the fallback for when niri never does — the workspace vanished, or a
/// keybind went elsewhere mid-burst — after which niri's active workspace is
/// the base again. A second is a long time next to an IPC round trip and
/// still shorter than a deliberate "scroll again".
const SCROLL_HOLD: Duration = Duration::from_secs(1);

/// The workspace this strip's last scroll notch asked niri to focus, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingFocus {
    id: u64,
    at: Instant,
}

/// The workspace one scroll notch should focus, or `None` to send nothing (the
/// notch would step past either end; the strip clamps rather than wraps).
///
/// `ids` is this output's workspaces in order, `active` the index of niri's
/// active one, `direction` [`wire_scroll`]'s `±1.0` (positive = scroll down =
/// the next workspace). The base is the strip's own `pending` request while it
/// is fresh and still in the list, else `active`. Compared against the *base*,
/// not `active`: with niri still on its way to workspace 3, a notch back to 2
/// must be sent even though 2 is what niri reports active.
fn scroll_step(
    ids: &[u64],
    active: Option<usize>,
    pending: Option<PendingFocus>,
    now: Instant,
    direction: f64,
) -> Option<u64> {
    let from_pending = pending
        .filter(|p| now.saturating_duration_since(p.at) < SCROLL_HOLD)
        .and_then(|p| ids.iter().position(|&id| id == p.id));
    let base = from_pending.or(active)?;
    let last = ids.len().checked_sub(1)?;
    let next = if direction > 0.0 {
        (base + 1).min(last)
    } else {
        base.saturating_sub(1)
    };
    if next == base {
        return None;
    }
    ids.get(next).copied()
}

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

    // The strip's own last scroll request, until niri reports it active
    // (`SCROLL_HOLD`). Shared with the binding, which is what clears it.
    let pending: Rc<Cell<Option<PendingFocus>>> = Rc::new(Cell::new(None));

    bind_workspace_pills(
        &container,
        signal,
        &current_workspaces,
        &button_map,
        &pending,
    );

    // Scroll over the pill strip to cycle the active workspace on this
    // monitor. Steps relative to the *active* workspace on this output —
    // not the globally-focused one, since focus can currently be on another
    // monitor while this strip still shows its own active pill — or to the
    // strip's own unanswered request (`scroll_step`), and clamps at both ends
    // rather than wrapping.
    let container_for_scroll = container.clone();
    let current_workspaces_for_scroll = Rc::clone(&current_workspaces);
    wire_scroll(&container, move |direction| {
        let (ids, active) = {
            let list = current_workspaces_for_scroll.borrow();
            let ids: Vec<u64> = list.iter().map(|ws| ws.id).collect();
            (ids, list.iter().position(|ws| ws.is_active))
        };
        let now = Instant::now();
        let Some(id) = scroll_step(&ids, active, pending.get(), now, direction) else {
            return;
        };
        pending.set(Some(PendingFocus { id, at: now }));
        niri::focus_workspace(id);
        yield_to_niri_focus(&container_for_scroll, FocusTarget::WorkspaceSwitch);
    });

    container.upcast()
}

/// Drive [`update_workspaces`] from `signal` into `container`, keyed against
/// `button_map`, and keep `current_workspaces` in step so the scroll handler
/// can find prev/next without re-deriving the per-monitor filter.
///
/// Also clears the scroll handler's `pending` request once niri reports that
/// workspace active — niri has caught up, so the next notch steps from what is
/// on screen again. Only *that* answer clears it: an emission for a workspace
/// an earlier notch of the same burst asked for is niri still on its way, and
/// dropping the request then would base the next notch on a workspace already
/// left behind, losing a step.
///
/// Split out of [`widget`] so this `bind` call site's `WeakRef` contract
/// (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven with a synthetic
/// signal in tests, the same extraction #772 made for its four sites. The
/// builder's own signal is a filter over `niri::workspaces()`, which
/// `.expect()`s without a registered `Registry` (#831).
fn bind_workspace_pills<S>(
    container: &gtk::Box,
    signal: S,
    current_workspaces: &Rc<RefCell<Vec<niri::Workspace>>>,
    button_map: &Rc<RefCell<HashMap<u64, gtk::Button>>>,
    pending: &Rc<Cell<Option<PendingFocus>>>,
) where
    S: Signal<Item = Vec<niri::Workspace>> + 'static,
{
    let current_workspaces_for_bind = Rc::clone(current_workspaces);
    let button_map = button_map.clone();
    let pending = Rc::clone(pending);
    bind(signal, container, move |container, workspaces| {
        current_workspaces_for_bind
            .borrow_mut()
            .clone_from(&workspaces);
        if let Some(asked) = pending.get()
            && workspaces
                .iter()
                .any(|ws| ws.is_active && ws.id == asked.id)
        {
            pending.set(None);
        }

        update_workspaces(container, &button_map, &workspaces);
    });
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
    use hytte::futures_signals::signal::Mutable;
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

    // ── #831: the `bind` call site's WeakRef contract ────────────────────────
    //
    // In the shape `panels/connections.rs` established for #772: the apply
    // closure must take the `&gtk::Box` `bind` hands it rather than a strong
    // clone captured from the enclosing scope, or the binding keeps the
    // container alive for its own lifetime and defeats #224's `WeakRef`
    // contract (`hytte-reactive/src/bind.rs:16-22`). `bind_workspace_pills` is
    // the seam that makes that reachable at all — `widget()`'s own signal
    // reads `niri::workspaces()`, which `.expect()`s without a registered
    // `Registry`.

    /// The scroll handler's shared list, exactly as `widget()` builds it.
    type WorkspaceList = Rc<RefCell<Vec<niri::Workspace>>>;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn workspace_pills_binding_applies_a_value() {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let current: WorkspaceList = Rc::new(RefCell::new(Vec::new()));
        let map: ButtonMap = Rc::new(RefCell::new(HashMap::new()));
        let workspaces: Mutable<Vec<niri::Workspace>> = Mutable::new(Vec::new());
        super::bind_workspace_pills(
            &container,
            workspaces.signal_cloned(),
            &current,
            &map,
            &Rc::new(Cell::new(None)),
        );
        pump();

        workspaces.set(vec![ws(1, 1)]);
        pump();

        assert_eq!(
            keys(&map),
            [1],
            "the emitted workspace must reach the strip as a tracked pill"
        );
        assert_eq!(
            current.borrow().len(),
            1,
            "the scroll handler's shared list must be updated by the same emission"
        );
        assert!(
            map.borrow().values().all(|btn| btn.parent().is_some()),
            "each tracked pill must actually be parented into the container — the map is \
             written by the same pass whether or not the container calls ran"
        );
    }

    /// Falsified by reintroducing the `container_for_signal` strong clone the
    /// apply closure used to capture: with it, `drop(container)` is not the
    /// last strong ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn workspace_pills_binding_does_not_pin_container() {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let weak = container.downgrade();
        let current: WorkspaceList = Rc::new(RefCell::new(Vec::new()));
        let map: ButtonMap = Rc::new(RefCell::new(HashMap::new()));
        let workspaces: Mutable<Vec<niri::Workspace>> = Mutable::new(vec![ws(1, 1)]);
        super::bind_workspace_pills(
            &container,
            workspaces.signal_cloned(),
            &current,
            &map,
            &Rc::new(Cell::new(None)),
        );
        pump();

        // The pills are children of the container; a GTK child holds no
        // reference to its parent, and the map is what would otherwise
        // outlive the drop below.
        drop(current);
        drop(map);
        drop(container);

        assert!(
            weak.upgrade().is_none(),
            "bind_workspace_pills must not pin its container: a strong clone captured by the \
             apply closure (rather than taking the closure's own `&gtk::Box` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's WeakRef \
             contract"
        );

        // The binding must release cleanly on the next emission, not panic on
        // a dead weak ref: `bind` upgrades, gets `None`, and breaks its loop.
        workspaces.set(vec![ws(2, 2)]);
        pump();
    }

    /// The scroll handler's unanswered request is cleared by niri's answer
    /// **to it**, and by nothing earlier: an emission activating a workspace
    /// an earlier notch of the burst asked for is niri on its way, not there.
    ///
    /// **Falsification:** clear `pending` on every emission → the first
    /// assertion reds; delete the clearing → the second does.
    #[gtk::test]
    fn the_binding_clears_a_scroll_request_once_niri_reports_it_active() {
        use super::PendingFocus;
        use std::time::Instant;

        let active = |id: u64, idx: u8| niri::Workspace {
            is_active: true,
            ..ws(id, idx)
        };
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        let current: WorkspaceList = Rc::new(RefCell::new(Vec::new()));
        let map: ButtonMap = Rc::new(RefCell::new(HashMap::new()));
        let pending = Rc::new(Cell::new(None));
        let workspaces: Mutable<Vec<niri::Workspace>> =
            Mutable::new(vec![active(1, 1), ws(2, 2), ws(3, 3)]);
        super::bind_workspace_pills(
            &container,
            workspaces.signal_cloned(),
            &current,
            &map,
            &pending,
        );
        pump();

        // Two notches down: the strip has asked for 3; niri answers the first.
        let asked = PendingFocus {
            id: 3,
            at: Instant::now(),
        };
        pending.set(Some(asked));
        workspaces.set(vec![ws(1, 1), active(2, 2), ws(3, 3)]);
        pump();
        assert_eq!(
            pending.get(),
            Some(asked),
            "niri reaching the first notch's workspace is not an answer to the second"
        );

        workspaces.set(vec![ws(1, 1), ws(2, 2), active(3, 3)]);
        pump();
        assert_eq!(pending.get(), None, "niri reached the request: cleared");
    }
}

/// [`scroll_step`]'s arithmetic — hermetic, no GTK.
#[cfg(test)]
mod scroll_tests {
    use super::{PendingFocus, SCROLL_HOLD, scroll_step};
    use std::time::{Duration, Instant};

    const DOWN: f64 = 1.0;
    const UP: f64 = -1.0;
    const IDS: [u64; 5] = [10, 20, 30, 40, 50];

    /// Feed `notches` through `scroll_step` the way the handler does — each
    /// request becomes the next notch's `pending` — with niri answering none of
    /// them, and return every id sent.
    fn burst(active: usize, notches: &[f64], start: Instant) -> Vec<u64> {
        let mut pending = None;
        let mut sent = Vec::new();
        for (i, &direction) in notches.iter().enumerate() {
            let now = start + Duration::from_millis(10 * u64::try_from(i).unwrap());
            if let Some(id) = scroll_step(&IDS, Some(active), pending, now, direction) {
                pending = Some(PendingFocus { id, at: now });
                sent.push(id);
            }
        }
        sent
    }

    /// A three-notch flick before niri answers moves three workspaces. Each
    /// notch used to step from the same `is_active` pill, so all three sent
    /// workspace 20.
    ///
    /// **Falsification:** ignore `pending` in `scroll_step` → `[20, 20, 20]`.
    #[test]
    fn a_fast_flick_moves_one_workspace_per_notch() {
        assert_eq!(burst(0, &[DOWN, DOWN, DOWN], Instant::now()), [20, 30, 40]);
    }

    /// Steps past the last workspace send nothing; the strip clamps.
    #[test]
    fn a_burst_clamps_at_the_end() {
        assert_eq!(burst(3, &[DOWN, DOWN, DOWN], Instant::now()), [50]);
        assert_eq!(burst(1, &[UP, UP, UP], Instant::now()), [10]);
    }

    /// A reversal inside a burst steps back from the request, not from the
    /// active workspace — so it is sent even when it lands on the workspace
    /// niri still reports active.
    #[test]
    fn a_reversal_steps_back_from_the_request() {
        assert_eq!(burst(0, &[DOWN, DOWN, UP], Instant::now()), [20, 30, 20]);
        assert_eq!(burst(0, &[DOWN, UP], Instant::now()), [20, 10]);
    }

    /// Past the hold, or once the requested workspace is gone, niri's active
    /// workspace is the base again.
    #[test]
    fn a_stale_or_vanished_request_falls_back_to_the_active_workspace() {
        let then = Instant::now();
        let stale = Some(PendingFocus { id: 40, at: then });
        assert_eq!(
            scroll_step(&IDS, Some(0), stale, then + SCROLL_HOLD, DOWN),
            Some(20)
        );
        let gone = Some(PendingFocus { id: 99, at: then });
        assert_eq!(scroll_step(&IDS, Some(0), gone, then, DOWN), Some(20));
    }

    /// No workspaces, or none active and nothing pending: nothing to step from.
    #[test]
    fn nothing_to_step_from_sends_nothing() {
        let now = Instant::now();
        assert_eq!(scroll_step(&[], None, None, now, DOWN), None);
        assert_eq!(scroll_step(&IDS, None, None, now, DOWN), None);
    }
}
