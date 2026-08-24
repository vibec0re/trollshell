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
    let owned_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));
    let owned_overflow_track: Rc<RefCell<Option<adw::ActionRow>>> = Rc::new(RefCell::new(None));
    let other_expander = adw::ExpanderRow::builder().title("Other users").build();
    let other_rows_track: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::new(RefCell::new(Vec::new()));

    let group_for_bind = conn_group.clone();
    let owned_for_bind = owned_track.clone();
    let overflow_for_bind = owned_overflow_track.clone();
    let other_for_bind = other_expander.clone();
    let other_rows_for_bind = other_rows_track.clone();
    bind(netconn::connections(), &conn_group, move |_g, mut conns| {
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
            group_for_bind.remove(&r);
        }
        for r in other_rows_for_bind.take() {
            other_for_bind.remove(&r);
        }
        if let Some(prev) = overflow_for_bind.take() {
            group_for_bind.remove(&prev);
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
                group_for_bind.add(&row);
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
            group_for_bind.add(&hint);
            *overflow_for_bind.borrow_mut() = Some(hint);
        }

        if total_other > other_count {
            other_for_bind.set_subtitle(&format!("{other_count} of {total_other} sockets"));
        } else {
            other_for_bind.set_subtitle(&format!("{other_count} sockets"));
        }
        other_for_bind.set_visible(total_other > 0);
    });
    conn_group.add(&other_expander);

    // Wrap the connections list in a ScrolledWindow so that when many
    // sockets are open the panel scrolls instead of growing past the
    // screen height and clipping (#84).
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);
    scrolled.set_vscrollbar_policy(gtk::PolicyType::Automatic);
    scrolled.set_propagate_natural_height(true);
    // Design-baseline px, routed through `scale()` so the cap tracks font size
    // / text-scaling the same way `stats.rs`'s scroll wrapper does (#708).
    scrolled.set_max_content_height(crate::scale::scale(480));
    scrolled.set_child(Some(&conn_group));
    column.append(&scrolled);

    finish_page(&column)
}
