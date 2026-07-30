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
/// `build_row` and added. The container is cloned (cheap — GTK widgets are
/// reference-counted) and the bind future lives as long as the signal source,
/// matching [`hytte::prelude::bind`].
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
    let container_for_bind = container.clone();
    bind(signal, container, move |_, items| {
        // `take()`, not `borrow_mut().drain(..)`: the `RefMut` of a chained
        // temporary lives for the whole `for`, so every `remove_row()` below
        // would run with `rows_track` mutably borrowed. `remove()` can emit
        // synchronously, and any handler re-entering this cell would panic —
        // and a panic unwinding through a glib callback aborts the process.
        // `take()` moves the vec out and ends the borrow before the first call.
        for row in rows_track.take() {
            container_for_bind.remove_row(&row);
        }
        if items.is_empty() {
            if let Some(make_placeholder) = empty_placeholder.as_ref() {
                let placeholder = make_placeholder();
                container_for_bind.add_row(&placeholder);
                rows_track.borrow_mut().push(placeholder);
            }
            return;
        }
        let mut new_rows = Vec::with_capacity(items.len());
        for item in &items {
            let row = build_row(item);
            container_for_bind.add_row(&row);
            new_rows.push(row);
        }
        *rows_track.borrow_mut() = new_rows;
    });
}
