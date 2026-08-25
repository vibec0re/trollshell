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
    bind(signal, conn_group, move |conn_group, mut conns| {
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
        for r in owned_for_bind.take() {
            conn_group.remove(&r);
        }
        for r in other_rows_for_bind.take() {
            other_for_bind.remove(&r);
        }
        if let Some(prev) = overflow_for_bind.take() {
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
                other_for_bind.add_row(&row);
                others.push(row);
                other_count += 1;
            }
        }
        // Safe to write back through a `RefMut`: `take()` above left each cell
        // holding an empty `Vec`, so the value dropped inside the borrow has no
        // drop glue to run.
        *owned_for_bind.borrow_mut() = owned;
        *other_rows_for_bind.borrow_mut() = others;

        if total_owned > owned_count {
            let hint = adw::ActionRow::builder()
                .title(format!("(+{} more)", total_owned - owned_count))
                .activatable(false)
                .selectable(false)
                .build();
            hint.set_subtitle("Top sockets shown.");
            conn_group.add(&hint);
            *overflow_for_bind.borrow_mut() = Some(hint);
        }

        if total_other > other_count {
            other_for_bind.set_subtitle(&format!("{other_count} of {total_other} sockets"));
        } else {
            other_for_bind.set_subtitle(&format!("{other_count} sockets"));
        }
        other_for_bind.set_visible(total_other > 0);
    });
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
