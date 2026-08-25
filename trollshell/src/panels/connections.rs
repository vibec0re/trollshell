//! Drawer drill-down listing per-process active sockets.
//!
//! Surfaces every entry of `hytte::services::netconn::connections()`. Own-user
//! sockets (PID present) appear at top sorted by program name; other-user
//! sockets (where ss can't see PID) collapse into a single "Other users"
//! expander at the bottom. Each bucket is capped at `CONN_BUCKET_CAP` rows;
//! truncation is surfaced via a "(+N more)" hint row in the own bucket and a
//! "{shown} of {total} sockets" subtitle on the other-users expander.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk;
use hytte::prelude::*;
use hytte::services::netconn;

use crate::components::connection_row::{CONN_BUCKET_CAP, build_connection_row};
use crate::components::layout::{finish_page, page_box};

pub fn panel_connections() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.set_spacing(16);

    let conn_group = adw::PreferencesGroup::builder()
        .title("Active connections")
        .build();
    bind(
        netconn::connections().map(|cs| {
            let total = cs.len();
            let with_pid = cs.iter().filter(|c| c.pid.is_some()).count();
            format!("{total} sockets, {with_pid} with PID")
        }),
        &conn_group,
        |g, txt| g.set_description(Some(&txt)),
    );

    // Top-level rows: own-user sockets sorted by program. Other users
    // (where ss can't see PID) collapse into a single expander at the
    // bottom so they don't dominate.
    let other_expander = adw::ExpanderRow::builder().title("Other users").build();
    bind_connections_group(&conn_group, &other_expander, netconn::connections());
    conn_group.add(&other_expander);

    // Wrap the connections list in a ScrolledWindow so that when many
    // sockets are open the panel scrolls instead of growing past the
    // screen height and clipping (#84).
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    // Design-baseline px, routed through `scale()` so the cap tracks font size
    // / text-scaling (#708) — this is an inside-card list scroller, so it's
    // legitimately meant to grow with the font. That's *not* true of the Stats
    // drawer's viewport cap: #787 deliberately pulled that one off `scale()`
    // because it's a live screen-space budget measured off the monitor, and
    // scaling an already-real-pixel number again would double-count the font
    // factor. Pinned alongside `network/wifi.rs`'s and `notifications.rs`'s
    // siblings by `scale::tests::three_scroll_heights_scale_with_factor`.
    scrolled.set_max_content_height(crate::scale::scale(480));
    scrolled.set_child(Some(&conn_group));
    column.append(&scrolled);

    finish_page(&column)
}

/// Drain-rebuild the own-user rows into `conn_group` and the other-users
/// rows into `other_expander`'s own row list, from `signal`. Split out of
/// [`panel_connections`] so this `bind` call site's `WeakRef` contract (#772)
/// can be driven with a synthetic signal in tests, the same way
/// `reactive_list` is (#761/#771).
///
/// `other_expander` is a second widget the closure also writes to — not the
/// `bind` target — so it stays a captured clone; see #772's note on
/// `other_for_bind` for why that clone is a separate design question.
///
/// The apply closure is a one-line call to [`rebuild_connections`]; see there
/// for the borrow discipline and for why the rebuild is a free function.
fn bind_connections_group<S>(
    conn_group: &adw::PreferencesGroup,
    other_expander: &adw::ExpanderRow,
    signal: S,
) where
    S: Signal<Item = Vec<netconn::Connection>> + 'static,
{
    let owned_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let owned_overflow_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let other_rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let owned_for_bind = owned_track;
    let overflow_for_bind = owned_overflow_track;
    let other_for_bind = other_expander.clone();
    let other_rows_for_bind = other_rows_track;
    bind(signal, conn_group, move |conn_group, conns| {
        rebuild_connections(
            conn_group,
            &other_for_bind,
            &owned_for_bind,
            &other_rows_for_bind,
            &overflow_for_bind,
            conns,
        );
    });
}

/// One rebuild pass: sort `conns`, tear down whatever the previous pass left
/// in the three tracking cells, and rebuild both buckets plus the overflow
/// hint row.
///
/// A free function taking its three cells explicitly, mirroring
/// `widgets/tasks.rs`'s `wire_tasks_bind`/`rebuild_list` pair — the loop body
/// used to sit inline inside [`bind_connections_group`]'s apply closure, where
/// nothing could call it. Extracting it changes no behaviour (the closure is
/// now a single forwarding call) and is what lets the colocated `#[gtk::test]`s
/// below drive the cells directly: re-entering through the `Mutable` the #772
/// test uses would be *deferred* to the next main-context iteration, since
/// `bind` polls its signal from an async task, so it could never reproduce the
/// synchronous re-entry this borrow discipline exists for (#674).
fn rebuild_connections(
    conn_group: &adw::PreferencesGroup,
    other_expander: &adw::ExpanderRow,
    owned_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
    other_rows_track: &Rc<RefCell<Vec<adw::ActionRow>>>,
    overflow_track: &Rc<RefCell<Option<adw::ActionRow>>>,
    mut conns: Vec<netconn::Connection>,
) {
    conns.sort_by(|a, b| match (a.pid.is_some(), b.pid.is_some()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (true, true) => a
            .program
            .as_deref()
            .unwrap_or("")
            .cmp(b.program.as_deref().unwrap_or("")),
        (false, false) => a.local.to_string().cmp(&b.local.to_string()),
    });

    let total_owned = conns.iter().filter(|c| c.pid.is_some()).count();
    let total_other = conns.len() - total_owned;

    // Take each cell instead of holding a `RefMut` across the rebuild. The
    // named bindings this replaces stayed live all the way down past every
    // `remove()`/`add()`/`add_row()` below — same hazard as a chained
    // `borrow_mut().drain(..)`, just spelled with a `let`. A synchronous
    // emission re-entering one of these cells would panic, and a panic
    // unwinding through a glib callback aborts the process (#643). The rows
    // are rebuilt into owned `Vec`s and stored back once, at the end.
    for r in owned_track.take() {
        conn_group.remove(&r);
    }
    for r in other_rows_track.take() {
        other_expander.remove(&r);
    }
    if let Some(prev) = overflow_track.take() {
        conn_group.remove(&prev);
    }

    let mut owned: Vec<adw::ActionRow> = Vec::new();
    let mut others: Vec<adw::ActionRow> = Vec::new();
    let mut owned_count = 0usize;
    let mut other_count = 0usize;
    for c in &conns {
        if c.pid.is_some() {
            if owned_count >= CONN_BUCKET_CAP {
                continue;
            }
            let row = build_connection_row(c);
            conn_group.add(&row);
            owned.push(row);
            owned_count += 1;
        } else {
            if other_count >= CONN_BUCKET_CAP {
                continue;
            }
            let row = build_connection_row(c);
            other_expander.add_row(&row);
            others.push(row);
            other_count += 1;
        }
    }
    // Safe to write back through a `RefMut`: `take()` above left each cell
    // holding an empty `Vec`, so the value dropped inside the borrow has no
    // drop glue to run.
    *owned_track.borrow_mut() = owned;
    *other_rows_track.borrow_mut() = others;

    if total_owned > owned_count {
        let hint = adw::ActionRow::builder()
            .title(format!("(+{} more)", total_owned - owned_count))
            .activatable(false)
            .selectable(false)
            .build();
        hint.set_subtitle("Top sockets shown.");
        conn_group.add(&hint);
        *overflow_track.borrow_mut() = Some(hint);
    }

    if total_other > other_count {
        other_expander.set_subtitle(&format!("{other_count} of {total_other} sockets"));
    } else {
        other_expander.set_subtitle(&format!("{other_count} sockets"));
    }
    other_expander.set_visible(total_other > 0);
}

/// #772 regression coverage: the hand-rolled `bind` call site behind
/// [`bind_connections_group`] must hold its `conn_group` container only
/// weakly, exactly like `reactive_list`'s own #761/#771 regression test.
/// (`other_expander` is a *second* widget the closure also writes to, not
/// the `bind` target — out of scope for #772, see the note on
/// `bind_connections_group` above.)
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{bind_connections_group, netconn};
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// `bind_connections_group` must not keep its `conn_group` container
    /// alive by itself, per the #224 `WeakRef` contract at
    /// `hytte-reactive/src/bind.rs:16-22`. Falsified by reintroducing the
    /// `group_for_bind` strong clone the apply closure used to capture.
    #[gtk::test]
    fn connections_group_binding_does_not_pin_conn_group() {
        adw::init().expect("libadwaita init");
        let conn_group = adw::PreferencesGroup::new();
        let weak = conn_group.downgrade();
        let other_expander = adw::ExpanderRow::builder().title("Other users").build();
        let conns: Mutable<Vec<netconn::Connection>> = Mutable::new(Vec::new());
        bind_connections_group(&conn_group, &other_expander, conns.signal_cloned());
        pump();

        drop(conn_group);

        assert!(
            weak.upgrade().is_none(),
            "bind_connections_group must not pin its conn_group: a strong clone captured by \
             the apply closure (rather than taking the closure's own `conn_group` argument from \
             `bind`) would keep this alive for the life of the binding, defeating #224's \
             WeakRef contract"
        );
    }
}

/// The `RefCell`-across-a-GTK-call abort class (#674) for this file.
///
/// ## Why there *is* a production change alongside these tests
///
/// Unlike `widgets/tasks.rs`'s `rebuild_list` or `widgets/workspaces.rs`'s
/// `update_workspaces`, this file's rebuild pass used to live **inline inside
/// [`bind_connections_group`]'s apply closure**, with its three cells created
/// as closure captures. Nothing could call it, and nothing could reach the
/// cells to observe them.
///
/// Driving it through the `Mutable` the #772 test above already uses does not
/// help: `bind` polls its signal from a `glib::MainContext` task, so a
/// `Mutable::set` issued from inside an apply wakes that task for the *next*
/// main-context iteration. The re-entry would be deferred, no borrow would
/// still be live, and the test would prove nothing about the hazard the
/// `take()`s exist for. So the loop body was hoisted into
/// [`rebuild_connections`] — a strictly mechanical extract: the body is
/// byte-identical to what the closure held, modulo the capture names becoming
/// parameter names, and the closure is now a single forwarding call.
///
/// ## Why the probe is `destroy`
///
/// PR #817 (`overlays/notifications.rs`) found a removed widget's `destroy`
/// can be silently deferred when something outside the cell under test still
/// holds it — its toast cards kept a focusable dismiss button alive past
/// `vbox.remove()`, so `destroy` never fired *inside* the call and the probe
/// had to become `unmap`.
///
/// That trap does not apply here, and it was checked rather than assumed: the
/// `adw::PreferencesGroup` and `adw::ExpanderRow` these tests build are never
/// added to any window, so there is no `GtkRoot` and no focus-widget chain to
/// retain a removed row past `remove()` — `destroy` fires off pure
/// refcounting, as it does for `widgets/tasks.rs`'s rows. Each test asserts
/// that rather than trusting it: the `fired_inside == Some(true)` check is an
/// anti-vacuity guard, and it is the only reason a probe that never re-enters
/// would be caught instead of shipping a decorative green.
///
/// Needs a real display server (GTK widgets have to be constructible), hence
/// the `system-tests` gate, like the rest of this bug class.
#[cfg(all(test, feature = "system-tests"))]
mod reentrancy_tests {
    use std::cell::{Cell, RefCell};
    use std::net::SocketAddr;
    use std::rc::Rc;

    use hytte::adw::{self, prelude::*};
    use hytte::gtk;
    use hytte::services::netconn::{ConnState, Connection, Proto};

    use super::{CONN_BUCKET_CAP, rebuild_connections};

    type Rows = Rc<RefCell<Vec<adw::ActionRow>>>;
    type OptRow = Rc<RefCell<Option<adw::ActionRow>>>;

    /// One socket. Only `pid` decides the bucket, and only `program` feeds the
    /// own-user sort — every other field just renders into a subtitle nothing
    /// here inspects.
    fn conn(port: u16, pid: Option<u32>) -> Connection {
        Connection {
            proto: Proto::Tcp,
            local: SocketAddr::from(([127, 0, 0, 1], port)),
            remote: None,
            state: ConnState::Established,
            pid,
            program: pid.map(|p| format!("prog{p:03}")),
        }
    }

    /// `count` own-user sockets (PID present → the `conn_group` bucket).
    fn owned_conns(count: u16) -> Vec<Connection> {
        (1..=count)
            .map(|n| conn(9000 + n, Some(u32::from(n))))
            .collect()
    }

    /// `count` other-user sockets (no PID → the `other_expander` bucket).
    fn other_conns(count: u16) -> Vec<Connection> {
        (1..=count).map(|n| conn(8000 + n, None)).collect()
    }

    /// The two containers and three cells `bind_connections_group` builds,
    /// exactly as it builds them. No registry, no `App`, no `ss` poller: every
    /// piece of state `rebuild_connections` touches now arrives through its
    /// own parameters.
    fn fresh() -> (adw::PreferencesGroup, adw::ExpanderRow, Rows, Rows, OptRow) {
        adw::init().expect("libadwaita init");
        (
            adw::PreferencesGroup::new(),
            adw::ExpanderRow::builder().title("Other users").build(),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(RefCell::new(None)),
        )
    }

    /// Cell 1, the own-user row list: `conn_group.remove(&r)` drops the
    /// group's reference and the loop-owned `r` binding drops its own at the
    /// end of that iteration — the last strong ref, so `GtkWidget::destroy`
    /// fires **synchronously** from dispose.
    ///
    /// The handler re-enters `rebuild_connections` on the same three cells.
    /// Against a pre-#643 `let mut owned = owned_track.borrow_mut();` (closing
    /// write-back folded into that guard) the inner call hits a live `RefMut`
    /// and aborts the binary with `BorrowMutError` rather than failing one
    /// test — #663's SIGABRT, the failure mode #674 exists for. With `take()`
    /// the cell is free for the whole call, so the inner call finds an empty
    /// `Vec`.
    #[gtk::test]
    fn rebuild_connections_tolerates_a_reentrant_rebuild_from_a_removed_owned_row_destroy() {
        let (group, expander, owned, others, overflow) = fresh();
        let seed = owned_conns(2);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
        assert_eq!(
            owned.borrow().len(),
            2,
            "both own-user sockets must be rendered after the seeding call"
        );

        // True only while the outer `rebuild_connections` is on the stack, so
        // the handler can record whether it ran inside the call or was
        // deferred.
        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let row2 = owned.borrow()[1].clone();
        {
            let group = group.clone();
            let expander = expander.clone();
            let owned = Rc::clone(&owned);
            let others = Rc::clone(&others);
            let overflow = Rc::clone(&overflow);
            let seed = seed.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            row2.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
            });
        }
        // Drop our clone before the removing pass: while it lives the row has
        // a second strong ref, `remove()` won't dispose it, and `destroy`
        // never fires — the test would pass vacuously.
        drop(row2);

        in_outer.set(true);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed own-user row's `destroy` must fire synchronously inside \
             `rebuild_connections`; if GTK ever defers it, or the row outlives the removal loop, \
             this test proves nothing about the borrow discipline"
        );
        assert_eq!(
            owned.borrow().len(),
            2,
            "the outer call's write-back must still land: re-entry may not leave the cell holding \
             the inner call's rows or an empty Vec"
        );
    }

    /// Cell 2, the other-users row list. Same shape as cell 1, but the rows
    /// live in the `adw::ExpanderRow`'s own list (`add_row`/`remove`) rather
    /// than in the `adw::PreferencesGroup` — a different containment story, so
    /// the synchronous-`destroy` fact is re-established here rather than
    /// carried over.
    ///
    /// Falsifiable independently: reverting only `other_rows_track`'s `take()`
    /// to a held `borrow_mut()` makes the abort name that cell and no other.
    #[gtk::test]
    fn rebuild_connections_tolerates_a_reentrant_rebuild_from_a_removed_other_row_destroy() {
        let (group, expander, owned, others, overflow) = fresh();
        let seed = other_conns(2);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
        assert_eq!(
            others.borrow().len(),
            2,
            "both other-user sockets must be rendered after the seeding call"
        );

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let row2 = others.borrow()[1].clone();
        {
            let group = group.clone();
            let expander = expander.clone();
            let owned = Rc::clone(&owned);
            let others = Rc::clone(&others);
            let overflow = Rc::clone(&overflow);
            let seed = seed.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            row2.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
            });
        }
        drop(row2);

        in_outer.set(true);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the removed other-user row's `destroy` must fire synchronously inside \
             `rebuild_connections`; `adw::ExpanderRow` holds its rows in an internal list box, so \
             if that keeps one alive past the removal loop this test proves nothing"
        );
        assert_eq!(
            others.borrow().len(),
            2,
            "the outer call's write-back must still land for the other-users bucket too"
        );
    }

    /// Cell 3, the overflow hint row: one more own-user socket than
    /// `CONN_BUCKET_CAP` collapses the tail into a "(+N more)" row, torn down
    /// and rebuilt on every call whose own-user bucket still overflows.
    ///
    /// The hint is removed *after* both row lists have already been `take()`n
    /// and released, so a revert of only `overflow_track` names that cell and
    /// no other — the two lists are free by the time the handler runs.
    #[gtk::test]
    fn rebuild_connections_tolerates_a_reentrant_rebuild_from_a_removed_overflow_destroy() {
        let (group, expander, owned, others, overflow) = fresh();
        let cap = u16::try_from(CONN_BUCKET_CAP).expect("CONN_BUCKET_CAP fits in u16");
        let seed = owned_conns(cap + 1);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
        assert!(
            overflow.borrow().is_some(),
            "one socket past the bucket cap must collapse into an overflow hint row after the \
             seeding call"
        );

        let in_outer = Rc::new(Cell::new(false));
        let fired_inside = Rc::new(Cell::new(None::<bool>));

        let hint = overflow.borrow().clone().expect("just asserted Some above");
        {
            let group = group.clone();
            let expander = expander.clone();
            let owned = Rc::clone(&owned);
            let others = Rc::clone(&others);
            let overflow = Rc::clone(&overflow);
            let seed = seed.clone();
            let in_outer = Rc::clone(&in_outer);
            let fired_inside = Rc::clone(&fired_inside);
            let armed = Cell::new(true);
            hint.connect_destroy(move |_| {
                if !armed.replace(false) {
                    return;
                }
                fired_inside.set(Some(in_outer.get()));
                rebuild_connections(&group, &expander, &owned, &others, &overflow, seed.clone());
            });
        }
        drop(hint);

        in_outer.set(true);
        rebuild_connections(&group, &expander, &owned, &others, &overflow, seed);
        in_outer.set(false);

        assert_eq!(
            fired_inside.get(),
            Some(true),
            "the retired overflow hint's `destroy` must fire synchronously inside \
             `rebuild_connections`; if it does not, this test proves nothing about the borrow \
             discipline"
        );
        assert!(
            overflow.borrow().is_some(),
            "the outer call's write-back must still land: re-entry may not leave the cell empty \
             or holding the inner call's hint row"
        );
    }
}
