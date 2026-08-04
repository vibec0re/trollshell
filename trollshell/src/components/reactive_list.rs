//! `reactive_list` — the one binder behind every "drain-and-rebuild a boxed
//! list from a `Vec<T>` signal" panel.
//!
//! `adw::PreferencesGroup` and `adw::ExpanderRow` expose no row-traversal API,
//! so each site that fed one from a reactive `Vec<T>` hand-rolled the same
//! bookkeeping: an `Rc<RefCell<Vec<Row>>>` tracking the rows it added (plus, at
//! some sites, an optional empty-state placeholder), then on each emission it
//! drained + `remove()`d every tracked row, and either re-added a placeholder
//! (empty) or built+added+re-tracked one row per item. The per-site copies
//! diverged in subtle ways (separate vs in-`Vec` placeholder, different
//! placeholder text), which made them a bug farm (#174).
//!
//! This module collapses that idiom into a single [`reactive_list`] call. The
//! container is abstracted by [`RowContainer`] so the same binder drives both a
//! `PreferencesGroup` (`add`/`remove`) and an `ExpanderRow` (`add_row`/`remove`).
//!
//! Empty-state model (single, explicit): when the emitted `Vec` is empty **and**
//! an `empty_placeholder` builder was supplied, one placeholder row is built and
//! added; otherwise the list is just empty. The placeholder is tracked and
//! removed before the next rebuild exactly like a data row.

use std::cell::RefCell;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::futures_signals::signal::Signal;
use hytte::gtk;
use hytte::prelude::*;

/// A container that holds a flat, rebuildable list of rows. Abstracts over the
/// two libadwaita widgets whose "add a row / remove a row" methods differ
/// (`PreferencesGroup` uses `add`/`remove`; `ExpanderRow` uses `add_row`/`remove`).
pub(crate) trait RowContainer: IsA<gtk::Widget> + Clone + 'static {
    /// Append `row` as a list child of this container.
    fn add_row(&self, row: &impl IsA<gtk::Widget>);
    /// Remove a previously-added `row` from this container.
    fn remove_row(&self, row: &impl IsA<gtk::Widget>);
}

impl RowContainer for adw::PreferencesGroup {
    fn add_row(&self, row: &impl IsA<gtk::Widget>) {
        self.add(row);
    }
    fn remove_row(&self, row: &impl IsA<gtk::Widget>) {
        self.remove(row);
    }
}

impl RowContainer for adw::ExpanderRow {
    fn add_row(&self, row: &impl IsA<gtk::Widget>) {
        adw::prelude::ExpanderRowExt::add_row(self, row);
    }
    fn remove_row(&self, row: &impl IsA<gtk::Widget>) {
        self.remove(row);
    }
}

/// Bind a `Vec<T>` signal to a boxed-list `container`, rebuilding its rows on
/// every emission.
///
/// On each emission the previously-added rows (and any placeholder) are removed,
/// then: if the `Vec` is empty and `empty_placeholder` is `Some`, the
/// placeholder row is built and added; otherwise one row per item is built via
/// `build_row` and added. The container is held only weakly, exactly like
/// [`hytte::prelude::bind`] (#224): the apply closure takes its container
/// argument from `bind` itself rather than capturing a strong clone, so
/// dropping the container's last strong ref frees it instead of pinning it
/// alive for the life of the binding (#761).
///
/// `R` is generic so both `adw::ActionRow` and `adw::ExpanderRow` row widgets
/// work; it must be a `GtkListBoxRow` descendant or libadwaita renders it below
/// the boxed list.
pub(crate) fn reactive_list<C, T, R, S>(
    container: &C,
    signal: S,
    build_row: impl Fn(&T) -> R + 'static,
    empty_placeholder: Option<impl Fn() -> R + 'static>,
) where
    C: RowContainer,
    T: 'static,
    R: IsA<gtk::Widget> + 'static,
    S: Signal<Item = Vec<T>> + 'static,
{
    // Tracks the rows we added (PreferencesGroup/ExpanderRow have no
    // row-traversal API). The placeholder, when shown, is tracked here too —
    // it's removed before the next rebuild exactly like a data row.
    let rows_track: Rc<RefCell<Vec<R>>> = Rc::new(RefCell::new(Vec::new()));
    bind(signal, container, move |container, items| {
        // `take()`, not `borrow_mut().drain(..)`: the `RefMut` of a chained
        // temporary lives for the whole `for`, so every `remove_row()` below
        // would run with `rows_track` mutably borrowed. `remove()` can emit
        // synchronously, and any handler re-entering this cell would panic —
        // and a panic unwinding through a glib callback aborts the process.
        // `take()` moves the vec out and ends the borrow before the first call.
        for row in rows_track.take() {
            container.remove_row(&row);
        }
        if items.is_empty() {
            if let Some(make_placeholder) = empty_placeholder.as_ref() {
                let placeholder = make_placeholder();
                container.add_row(&placeholder);
                rows_track.borrow_mut().push(placeholder);
            }
            return;
        }
        let mut new_rows = Vec::with_capacity(items.len());
        for item in &items {
            let row = build_row(item);
            container.add_row(&row);
            new_rows.push(row);
        }
        *rows_track.borrow_mut() = new_rows;
    });
}

// Coverage for the binder behind eight panels (#759, the first slice of #674's
// private / `pub(crate)` remainder — reachable only from a colocated `mod
// tests`, since `reactive_list` is `pub(crate)`).
//
// MEASURED FINDING: the "RefCell borrow held across a GTK call" hazard #643
// filed against this file is real in its *emission* half and unreachable in
// its *handler* half, so the `take()` guard above CANNOT be regression-tested.
// Recorded here so nobody re-derives it.
//
// The emission half is real — unlike the four `close_all` sites #758 measured,
// where `gtk::Window::destroy()` turned out to emit nothing at all.
// `remove_row()` unparents a row and the loop's own binding then drops the last
// reference, so that row's `destroy` fires *synchronously inside the `for`* —
// i.e. inside the `RefMut` the pre-#663 `borrow_mut().drain(..)` shape held.
// `rebuild_is_not_re_entered_by_a_synchronous_row_destroy` pins that ordering.
//
// What is missing is any path from that handler back to the cell:
//
//   1. `rows_track` is created inside `reactive_list` and captured *only* by
//      the `bind` apply closure. Neither `build_row` nor `empty_placeholder` is
//      ever handed it, and no row holds it — so the only possible second
//      borrower is a re-entrant call of that same closure.
//   2. That closure runs only from the `glib::MainContext::spawn_local` task
//      source's dispatch, and GLib will not dispatch a source already in-call
//      (nothing sets `G_SOURCE_CAN_RECURSE`). Measured: a destroy handler
//      firing inside the rebuild that re-emits the bound signal and then pumps
//      the main context dispatches exactly *one* source — an idle scheduled
//      right there as a control — and skips the freshly-woken bind task, which
//      runs only once the outer rebuild has returned.
//
// So reverting the loop to `for row in rows_track.borrow_mut().drain(..)` is
// unobservable: the scenario below produces a byte-identical event log either
// way — no `BorrowMutError`, no `SIGABRT`. Per #674's standard (a test that
// cannot be demonstrated failing against the un-fixed code is not worth
// adding), no reentrancy regression test ships for this file. The guard stays —
// it is free and it is the correct shape — and what ships instead is a tripwire
// on the two facts that make it unnecessary, plus the rebuild contract the
// borrow bookkeeping exists to serve.
//
// Leg 2 generalises to any other `bind`-closure-private tracking cell; leg 1
// does not. A site that hands its cell to a *row* callback (a `connect_clicked`
// capturing it, say) is re-enterable through that callback and none of this
// covers it — check leg 1 per site before reusing the argument.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::reactive_list;
    use hytte::adw::{self, prelude::*};
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    const PLACEHOLDER: &str = "Nothing here";

    thread_local! {
        /// Ordered event log: `build:<title>`, `destroy:<title>:begin|end`,
        /// `control-idle`.
        static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn log(event: String) {
        LOG.with(|l| l.borrow_mut().push(event));
    }

    fn take_log() -> Vec<String> {
        LOG.with(RefCell::take)
    }

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// `adw::PreferencesGroup` exposes no row traversal (the very gap this
    /// module exists to paper over), so read the rows back off the widget tree.
    fn row_titles(group: &adw::PreferencesGroup) -> Vec<String> {
        fn walk(widget: &gtk::Widget, out: &mut Vec<String>) {
            let mut child = widget.first_child();
            while let Some(c) = child {
                if let Ok(row) = c.clone().downcast::<adw::ActionRow>() {
                    out.push(row.title().to_string());
                } else {
                    walk(&c, out);
                }
                child = c.next_sibling();
            }
        }
        let mut out = Vec::new();
        walk(group.upcast_ref(), &mut out);
        out
    }

    fn index_of(events: &[String], needle: &str) -> usize {
        events
            .iter()
            .position(|e| e == needle)
            .unwrap_or_else(|| panic!("expected a {needle} event in {events:?}"))
    }

    /// The rebuild contract this module was extracted to centralise (#174):
    /// every row a previous emission added — the empty-state placeholder
    /// included — is removed before the next set is added, so the container
    /// never accumulates. This is what `rows_track`'s bookkeeping (and hence
    /// its borrow discipline) exists to serve.
    #[gtk::test]
    fn rebuild_replaces_every_row_and_tracks_the_empty_placeholder() {
        adw::init().expect("libadwaita init");
        let group = adw::PreferencesGroup::new();
        let items = Mutable::new(vec!["a".to_owned(), "b".to_owned()]);
        reactive_list(
            &group,
            items.signal_cloned(),
            |t: &String| adw::ActionRow::builder().title(t.as_str()).build(),
            Some(|| adw::ActionRow::builder().title(PLACEHOLDER).build()),
        );

        pump();
        assert_eq!(
            row_titles(&group),
            ["a", "b"],
            "the first emission builds one row per item"
        );

        items.set(vec!["c".to_owned()]);
        pump();
        assert_eq!(
            row_titles(&group),
            ["c"],
            "the previous rows are removed, not accumulated"
        );

        items.set(Vec::new());
        pump();
        assert_eq!(
            row_titles(&group),
            [PLACEHOLDER],
            "an empty Vec with a placeholder builder shows exactly the placeholder"
        );

        items.set(vec!["d".to_owned()]);
        pump();
        assert_eq!(
            row_titles(&group),
            ["d"],
            "the placeholder is tracked like a data row, so the next rebuild removes it"
        );
    }

    /// The two measured facts from the module comment above, as a tripwire.
    ///
    /// * A removed row's `destroy` fires **synchronously inside** the rebuild's
    ///   removal loop — #643's emission half is real for this file.
    /// * The rebuild is nonetheless **never re-entered**: that handler re-emits
    ///   the bound signal and pumps the main context, and the freshly-woken
    ///   bind task is skipped while its source is in-call. The control idle
    ///   scheduled in the same breath *does* run, which is what makes the
    ///   negative assertion mean something rather than just proving the main
    ///   context was idle.
    ///
    /// Together these are why the `take()` guard above cannot be falsified. If
    /// either flips, this fails and the guard becomes load-bearing.
    #[gtk::test]
    fn rebuild_is_not_re_entered_by_a_synchronous_row_destroy() {
        adw::init().expect("libadwaita init");
        drop(take_log());
        let group = adw::PreferencesGroup::new();
        let items = Mutable::new(vec!["a".to_owned()]);
        let items_for_row = items.clone();
        let armed = Rc::new(Cell::new(false));
        let armed_for_row = Rc::clone(&armed);
        reactive_list(
            &group,
            items.signal_cloned(),
            move |t: &String| {
                log(format!("build:{t}"));
                let row = adw::ActionRow::builder().title(t.as_str()).build();
                let title = t.clone();
                let items = items_for_row.clone();
                let armed = Rc::clone(&armed_for_row);
                row.connect_destroy(move |_| {
                    log(format!("destroy:{title}:begin"));
                    if armed.replace(false) {
                        // Re-emit from inside the removal loop, then give the
                        // main context every chance to run the woken bind task.
                        items.set(vec!["c".to_owned()]);
                        gtk::glib::idle_add_local_once(|| log("control-idle".to_owned()));
                        pump();
                    }
                    log(format!("destroy:{title}:end"));
                });
                row
            },
            None::<fn() -> adw::ActionRow>,
        );

        pump();
        armed.set(true);
        items.set(vec!["b".to_owned()]);
        pump();
        let events = take_log();

        let begin = index_of(&events, "destroy:a:begin");
        let end = index_of(&events, "destroy:a:end");
        let build_b = index_of(&events, "build:b");
        assert!(
            end < build_b,
            "the removed row's destroy must fire inside the removal loop, before the rebuild adds \
             the new rows — if it stops doing so this file's borrow discipline is moot: {events:?}"
        );
        assert!(
            events[begin..end].contains(&"control-idle".to_owned()),
            "the nested pump must actually dispatch sources, or the no-reentry assertion below \
             proves nothing: {events:?}"
        );
        assert!(
            !events[begin..end].iter().any(|e| e.starts_with("build:")),
            "the rebuild must not be re-entered from a handler running inside it; if GLib ever \
             recurses into an in-call source, the take() guard above becomes load-bearing and \
             wants a real regression test: {events:?}"
        );
        assert_eq!(
            row_titles(&group),
            ["c"],
            "the re-emission is applied after the outer rebuild returns, leaving the last value"
        );
    }

    /// #761 regression: `reactive_list` must not keep its container alive by
    /// itself, exactly like the #224 contract `bind` documents at
    /// `hytte-reactive/src/bind.rs:16-22`.
    ///
    /// This is the *contrast* to that module's own
    /// `dropping_bound_widget_frees_it_on_next_emission`, not a copy of it:
    /// that test drives a further `set()` to observe the free, because it is
    /// the upgrade-on-emission that drops `bind`'s last handle. Here no
    /// emission is needed — once the strong clone is gone, `GObject`
    /// refcounting frees the container synchronously with the `drop` below.
    #[gtk::test]
    fn dropping_the_container_frees_it_without_a_further_emission() {
        adw::init().expect("libadwaita init");
        let group = adw::PreferencesGroup::new();
        let weak = group.downgrade();
        let items = Mutable::new(vec!["a".to_owned()]);
        reactive_list(
            &group,
            items.signal_cloned(),
            |t: &String| adw::ActionRow::builder().title(t.as_str()).build(),
            None::<fn() -> adw::ActionRow>,
        );
        pump();

        drop(group);

        assert!(
            weak.upgrade().is_none(),
            "reactive_list must not pin its container: a strong clone captured by the apply \
             closure (rather than taking the closure's own `container` argument from `bind`) \
             would keep this alive for the life of the binding, defeating #224's WeakRef contract"
        );
    }
}
