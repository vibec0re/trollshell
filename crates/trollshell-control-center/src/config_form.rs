//! The generic config form (#888 P1, spec §4–§5) — one row per config leaf,
//! generated from the family's [`Schema`] rather than hand-built per family.
//!
//! P0 (#1358, merged as `4f2fd20e`) put everything this needs in
//! `hytte-config`, which this app already links as a GTK-free leaf: a
//! [`Schema`] of [`Field`]s per family, a [`Raw`] layered read that stops
//! before the typed parse and carries the lock set plus per-leaf
//! [`Origin`], and [`subsystem::save_leaf_to_locked`], which writes **one**
//! leaf format-preserving and refuses four ways. What is here is the half
//! that draws.
//!
//! # One row per leaf, chosen by [`Kind`]
//!
//! | [`Kind`] | row |
//! | --- | --- |
//! | [`Kind::Bool`] | `AdwSwitchRow` |
//! | [`Kind::Int`] with an empty `also` | `AdwSpinRow::with_range` |
//! | [`Kind::Int`] with words | the same spin row, plus one suffix toggle per word |
//! | [`Kind::Choice`] | `AdwComboRow` over the options |
//! | [`Kind::Color`] | an `AdwComboRow` of the named colours (plus a *custom* item) and an `AdwEntryRow` for the `#rrggbb` literal, with a swatch |
//! | [`Kind::Text`] | `AdwEntryRow` with an apply button |
//! | [`Kind::List`] / [`Kind::Map`] | a read-only `AdwActionRow` summarising the value — editing a collection is #888 P2 |
//!
//! The row's **subtitle is provenance** (*Default* / *From `<layer file>`* /
//! *Yours* / *Set in nix*), its **tooltip** is the field's one-line `doc`
//! followed by the comment block `DEFAULT_TOML` carries directly above that
//! key, and it grows a per-row **reset** button that removes the operator's
//! own line (deliberately *not* an `_unset` marker — spec §5: a reset-to-
//! default is what a form user means). A locked leaf (#1227/#1331) is
//! insensitive and says *Set in nix*, which is the surface that design's
//! third point has had nowhere to land since #1331 shipped the API.
//!
//! # Three deviations from the sketch in §4, each for a reason
//!
//! 1. **[`build`] returns a [`Form`], not one `AdwPreferencesGroup`.** A
//!    family's fields are grouped by their own top-level table — `stats.toml`
//!    is `[sidebar]` and `[bar]`, which the issue asks for as two sub-groups
//!    — and an `AdwPreferencesGroup` cannot hold another: a non-`GtkListBoxRow`
//!    child of one renders *below* its list rather than inside it. So the
//!    form owns N groups and the caller adds each to its page.
//! 2. **No `locked` parameter and no `on_save` callback.** [`Raw`] already
//!    carries the lock set, and passing a second one invites the two
//!    disagreeing — which is exactly #1338's H2, one layer up. The save, the
//!    re-read and the provenance flip are the same five lines at every call
//!    site, so the form owns them; what a caller passes in is the
//!    [`xdg::Env`] the layers are resolved from, so a test never touches the
//!    real `~/.config/trollshell` (#1101).
//! 3. **The poll re-reads rather than stamping.** Places gates its re-read on
//!    a `(mtime, content-hash)` stamp; the equivalent for a `Subsystem`
//!    family lives behind `hytte-config`'s `watch` feature, which pulls
//!    `tokio` + `futures-signals` — a runtime a settings app deliberately
//!    does not grow (the crate's own dependency note). A hand-rolled
//!    `(mtime, len)` stamp is the one #1081 M5 measured as blind to exactly
//!    the layer that matters here: a nix-rendered base carries the frozen
//!    mtime `1970-01-01 00:00:01`, so a rebuild that adds a lock can move
//!    neither half of it. Re-reading three small TOML files twice a second,
//!    only while the page is up, is cheaper than being wrong about a greyed
//!    row.
//!
//! # What the poll compares
//!
//! **Values *and* locks and provenance**, not values alone (#1338 H2): a
//! `nixos-rebuild` that pins a key the overlay already sets moves no value
//! at all, and a form that dedups on values would leave the row editable
//! over a value the next load reverts. And a re-render never clobbers a
//! half-typed entry: each row remembers *what the file last said for its own
//! key* and pushes into its widget only when that moved, so a poll driven by
//! some other key's change leaves the draft alone (the #1338 draft-guard
//! lesson — compare against the file, not against the widget).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hytte_config::schema::{Family, Field, Kind, Schema};
use hytte_config::subsystem::{self, ConfigError, InvalidValue, Origin, Raw, Subsystem};
use hytte_config::{toml, toml_edit, xdg};

/// How long a change sits before it is written (spec §4).
///
/// A spin row fires `value-notify` per click of its `+`, and an operator
/// holding it down would otherwise write — and re-read, and re-render — one
/// file per step.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// How often a mounted form re-reads its layers. The Places tab's cadence,
/// and the Plugins tab's — the same 2 s an out-of-band `$EDITOR` save or a
/// `nixos-rebuild` is picked up in everywhere else in this app.
pub(crate) const CONFIG_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Words rendered upper-case in a row title rather than capitalised.
const ACRONYMS: &[&str] = &["cpu", "gpu", "id", "toml"];

// ── Which families this app can render, and how it reads and writes them ─────

/// A shell-owned family's two consts, as a type parameter.
///
/// The writer and the layered reader are generic over `S: Subsystem`, and the
/// `impl Subsystem` for `core-leds` and `workspaces` lives in the **shell**,
/// a binary this app cannot link (that is the whole reason
/// `hytte-config-families` exists). So this app brings its own `S` for those
/// two — and gets `NAME`/`DEFAULT_TOML` from [`Family`] rather than restating
/// them, so the shim and the shell cannot be reading two different files or
/// seeding two different defaults. `the_shell_shims_are_the_familys_own_two_consts`
/// pins that, and a marker type per family is what makes it expressible at
/// all: a `&'static str` cannot be a const generic parameter.
trait ShellFamily {
    /// The family this shim stands in for.
    const FAMILY: &'static Family;
}

/// `core-leds.toml`.
struct CoreLeds;

impl ShellFamily for CoreLeds {
    const FAMILY: &'static Family = &hytte_config_families::core_leds::FAMILY;
}

/// `workspaces.toml`.
struct Workspaces;

impl ShellFamily for Workspaces {
    const FAMILY: &'static Family = &hytte_config_families::workspaces::FAMILY;
}

/// The `S` [`subsystem::load_raw`] and [`subsystem::save_leaf_to_locked`] are
/// generic over, for a shell-owned family.
///
/// Deliberately inert. Neither entry point deserialises anything — that is
/// the point of a *raw* read and a *leaf* write — so this type's serde impl,
/// its `Resolved` and its `validate` exist only to satisfy the trait's
/// bounds, and the two members that carry meaning ([`Subsystem::NAME`] and
/// [`Subsystem::DEFAULT_TOML`]) are read straight off the family.
struct ShellSubsystem<F>(std::marker::PhantomData<F>);

impl<'de, F> serde::Deserialize<'de> for ShellSubsystem<F> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Nothing in this app deserialises a family; see the type's docs.
        serde::de::Deserialize::deserialize(deserializer).map(|serde::de::IgnoredAny| {
            Self(std::marker::PhantomData)
        })
    }
}

impl<F: ShellFamily> Subsystem for ShellSubsystem<F> {
    const NAME: &'static str = F::FAMILY.name;
    const DEFAULT_TOML: &'static str = F::FAMILY.default_toml;

    type Error = std::convert::Infallible;
    type Resolved = ();

    fn validate(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn parsed(&self) -> (Self::Resolved, Vec<InvalidValue>) {
        ((), Vec::new())
    }
}

/// One family as this app can act on it: its description, plus the two entry
/// points monomorphised over the `S` that family's file is read and written
/// through.
///
/// Function pointers rather than a trait object because the two halves are
/// free functions generic over `S` and nothing else varies — and because a
/// `FamilyOps` is then `Copy`, so a row handler can hold one without
/// ceremony.
#[derive(Clone, Copy)]
pub(crate) struct FamilyOps {
    /// The name, the schema and the documented default.
    pub(crate) family: &'static Family,
    /// Whether this family's scalar rows may be edited **in this version**.
    ///
    /// `false` for `agents` and `workspaces`, which spec §4 stages as
    /// read-only until P2 — not because a row could not be written (the
    /// writer is the same one), but because P1's job is to ship the editable
    /// path on two families and prove it. `workspaces` has no scalar leaf to
    /// edit anyway: both of its fields are collections.
    pub(crate) editable: bool,
    /// [`subsystem::load_raw`] for this family's `S`.
    load: fn(&[PathBuf]) -> Result<Raw, ConfigError>,
    /// [`subsystem::save_leaf_to_locked`] for this family's `S`.
    save: SaveLeaf,
}

/// [`subsystem::save_leaf_to_locked`] with its `S` already chosen.
type SaveLeaf = fn(
    &Path,
    &Schema,
    &str,
    Option<toml_edit::Value>,
    &BTreeSet<String>,
) -> Result<(), ConfigError>;

impl FamilyOps {
    /// The ops for a family read and written through `S`.
    fn of<S: Subsystem>(family: &'static Family, editable: bool) -> Self {
        Self {
            family,
            editable,
            load: subsystem::load_raw::<S>,
            save: subsystem::save_leaf_to_locked::<S>,
        }
    }
}

/// Every config family this app can render a form for.
///
/// Composed **here**, at the top of the dependency graph, which is the whole
/// shape of #888's erratum to §3: the two shell-owned families come from the
/// `hytte-config-families` leaf, and the two plugin-owned ones come from the
/// plugin crates this app links as libraries (`hytte-plugin-agents` since
/// #947 P4, `hytte-plugin-stats` since #1360 gave it a `[lib]` target). That
/// leaf cannot carry the plugin two — they link `hytte-config`, so it would
/// depend on crates that depend on it.
pub(crate) fn families() -> Vec<FamilyOps> {
    vec![
        FamilyOps::of::<ShellSubsystem<CoreLeds>>(&hytte_config_families::core_leds::FAMILY, true),
        FamilyOps::of::<ShellSubsystem<Workspaces>>(
            &hytte_config_families::workspaces::FAMILY,
            false,
        ),
        FamilyOps::of::<hytte_plugin_stats::config::StatsConfig>(
            &hytte_plugin_stats::config::FAMILY,
            true,
        ),
        FamilyOps::of::<hytte_plugin_agents::config::AgentsConfig>(
            &hytte_plugin_agents::config::FAMILY,
            false,
        ),
    ]
}

/// The family called `name`, or `None`.
pub(crate) fn family(name: &str) -> Option<FamilyOps> {
    families().into_iter().find(|ops| ops.family.name == name)
}

/// The **shell-owned** families, in the order the *Shell* entry renders them.
pub(crate) fn shell_families() -> Vec<FamilyOps> {
    hytte_config_families::FAMILIES
        .iter()
        .filter_map(|listed| family(listed.name))
        .collect()
}

// ── The form ─────────────────────────────────────────────────────────────────

/// A mounted form: the groups to add to a page, and everything that keeps
/// them in step with the file.
///
/// Dropping it stops the poll and cancels any pending debounced save — so a
/// caller tears a form down by dropping its handle after removing the groups
/// from its page, and nothing is left ticking against widgets nobody shows.
pub(crate) struct Form {
    inner: Rc<FormInner>,
}

/// [`Form`]'s shared half — what the row handlers hold **weakly**.
///
/// Weakly because the alternative is a cycle that outlives the window: a
/// handler is owned by the widget it is connected to, that widget is owned by
/// a group, and the groups are owned by this struct. The Plugins tab's
/// `WeakPluginsState` (#943) is the same fix on the same shape.
struct FormInner {
    /// The family, and how to read and write it.
    ops: FamilyOps,
    /// The search path, lowest precedence first. Resolved once: the process
    /// environment cannot change under a running process (the Plugins tab's
    /// `search_path` argument, #1270).
    layers: Vec<PathBuf>,
    /// `$XDG_CONFIG_HOME/trollshell/<family>.toml` — the one file this app
    /// may write. `None` when neither `$XDG_CONFIG_HOME` nor `$HOME` is set.
    overlay: Option<PathBuf>,
    /// One group per top-level table of the family, in the order
    /// `DEFAULT_TOML` documents them.
    groups: Vec<adw::PreferencesGroup>,
    /// A row that appears only when a layer could not be read or parsed,
    /// naming the file. Every control is insensitive while it is up: the
    /// merged view is not what the operator's files say, so offering to
    /// overwrite one of them would be a guess.
    banner: adw::ActionRow,
    /// One entry per [`Field`], in schema order.
    rows: Vec<Row>,
    /// What the layers last said — values, locks and per-leaf provenance from
    /// **one** read, which is what stops a row's value and its lock
    /// disagreeing (#1338 H2).
    raw: RefCell<Raw>,
    /// Guard so pushing a file value into a widget doesn't loop back into a
    /// save (the Places and Plugins tabs' `syncing`).
    syncing: Cell<bool>,
    /// The re-read timer.
    poll: Cell<Option<glib::SourceId>>,
}

impl Drop for FormInner {
    fn drop(&mut self) {
        if let Some(poll) = self.poll.take() {
            poll.remove();
        }
        for row in &self.rows {
            row.cancel_pending_save();
        }
    }
}

/// Build a form for `ops`, reading its layers out of `env`.
///
/// The groups are ready to add to an `AdwPreferencesPage`; the returned
/// handle owns the poll, so keep it for as long as the groups are shown.
pub(crate) fn build(ops: FamilyOps, env: &Rc<xdg::Env>) -> Form {
    let layers = env.config_layers(ops.family.name);
    let overlay = env.overlay_path(ops.family.name);

    let banner = adw::ActionRow::builder()
        .title("This file could not be read")
        .visible(false)
        .build();
    banner.add_css_class("error");

    let mut groups: Vec<adw::PreferencesGroup> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    let mut table_of_group: Option<Option<&str>> = None;
    for field in ops.family.schema.fields {
        let table = field.path.split_once('.').map(|(head, _)| head);
        if table_of_group != Some(table) {
            let group = adw::PreferencesGroup::builder()
                .title(group_title(ops.family, table))
                .description(group_description(ops, table))
                .build();
            if groups.is_empty() {
                group.add(&banner);
            }
            groups.push(group);
            table_of_group = Some(table);
        }
        let row = Row::build(field, ops);
        let group = groups
            .last()
            .expect("a group was pushed before the first field");
        for widget in &row.widgets {
            group.add(widget);
        }
        rows.push(row);
    }

    let inner = Rc::new(FormInner {
        ops,
        layers,
        overlay,
        groups,
        banner,
        rows,
        raw: RefCell::new(Raw::default()),
        syncing: Cell::new(false),
        poll: Cell::new(None),
    });

    connect_rows(&inner);
    inner.reload(true);

    let weak = Rc::downgrade(&inner);
    let poll = glib::timeout_add_local(CONFIG_POLL_INTERVAL, move || {
        let Some(inner) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        inner.reload(false);
        glib::ControlFlow::Continue
    });
    inner.poll.set(Some(poll));

    Form { inner }
}

impl Form {
    /// The groups to add to a page, in order.
    pub(crate) fn groups(&self) -> &[adw::PreferencesGroup] {
        &self.inner.groups
    }

    /// Re-read the layers now and apply anything that moved. Returns whether
    /// anything the form shows actually changed.
    ///
    /// The poll's own body, reachable by hand so the page is current the
    /// moment it is opened rather than up to one tick stale — and so a test
    /// can drive a refresh without waiting one out. The same split
    /// `places_tab::Editor::refresh_from_disk` has.
    pub(crate) fn refresh_from_disk(&self) -> bool {
        self.inner.reload(false)
    }
}

impl FormInner {
    /// Read every layer and push whatever moved into the rows.
    ///
    /// `force` is the first load: there is nothing to compare against yet, so
    /// every row is filled regardless.
    fn reload(&self, force: bool) -> bool {
        let next = match (self.ops.load)(&self.layers) {
            Ok(raw) => raw,
            Err(err) => {
                self.show_load_error(&err);
                return false;
            }
        };
        let unchanged = !force && same_view(&self.raw.borrow(), &next);
        if unchanged {
            return false;
        }
        self.banner.set_visible(false);
        self.apply(&next);
        *self.raw.borrow_mut() = next;
        true
    }

    /// Put `raw` on screen: every row's value (when *its own* key moved), its
    /// provenance and its sensitivity.
    fn apply(&self, raw: &Raw) {
        self.syncing.set(true);
        for row in &self.rows {
            row.apply(raw, self.ops.editable);
        }
        self.syncing.set(false);
    }

    /// A layer exists and could not be read or parsed: say which, and make
    /// every control insensitive until it can be.
    fn show_load_error(&self, err: &ConfigError) {
        tracing::warn!(family = self.ops.family.name, %err, "config layers could not be read");
        self.banner.set_subtitle(&glib::markup_escape_text(
            &err.to_string(),
        ));
        self.banner.set_visible(true);
        for row in &self.rows {
            row.set_sensitive(false);
        }
    }

    /// Write one leaf, then re-read so the row's provenance flips to *Yours*.
    ///
    /// Every refusal the writer has ([`ConfigError::Rejected`],
    /// [`ConfigError::Locked`], [`ConfigError::NotALeaf`], and the I/O ones)
    /// lands on the row it was for rather than in a toast: a form can have
    /// sixteen rows, and "which one did it refuse" is the first thing the
    /// operator needs.
    fn save(&self, index: usize, value: Option<toml_edit::Value>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        let Some(overlay) = self.overlay.as_ref() else {
            row.show_error(
                "nowhere to write: neither $XDG_CONFIG_HOME nor $HOME is set",
            );
            return;
        };
        let locked = self.raw.borrow().locked.clone();
        match (self.ops.save)(
            overlay,
            self.ops.family.schema,
            row.field.path,
            value,
            &locked,
        ) {
            Ok(()) => {
                row.clear_error();
                // The whole view, not just this row: a first-ever save seeds
                // the overlay from `DEFAULT_TOML`, which can move every
                // other row's provenance in the same write.
                self.reload(true);
            }
            Err(err) => {
                tracing::warn!(
                    family = self.ops.family.name,
                    key = row.field.path,
                    %err,
                    "a settings row's save was refused"
                );
                row.show_error(&err.to_string());
            }
        }
    }

    /// Queue a save for `index`, replacing any still-pending one for the same
    /// row (spec §4's 300 ms debounce).
    fn schedule_save(self: &Rc<Self>, index: usize, value: Option<toml_edit::Value>) {
        if self.syncing.get() {
            return;
        }
        let Some(row) = self.rows.get(index) else {
            return;
        };
        row.cancel_pending_save();
        let weak = Rc::downgrade(self);
        let pending = Rc::clone(&row.pending);
        let source = glib::timeout_add_local_once(SAVE_DEBOUNCE, move || {
            // Before anything else: this source has fired, so the id in the
            // cell is dead and removing it later would be the "programmer
            // error" `SourceId::remove` documents.
            pending.set(None);
            if let Some(inner) = weak.upgrade() {
                inner.save(index, value);
            }
        });
        row.pending.set(Some(source));
    }
}

/// Whether two layered reads say the same thing about **everything a row
/// renders** — values, locks and provenance (#1338 H2).
///
/// Not a `PartialEq` on [`Raw`]: that type is `#[non_exhaustive]` and carries
/// `sources`, which moves when a layer file appears or vanishes without
/// changing a single row.
fn same_view(a: &Raw, b: &Raw) -> bool {
    a.table == b.table && a.locked == b.locked && a.origins == b.origins
}

// ── One leaf's row ───────────────────────────────────────────────────────────

/// A leaf's widgets, plus the two pieces of state a refresh needs.
struct Row {
    /// The schema entry this row draws.
    field: &'static Field,
    /// Every widget this leaf contributes, in the order they are added to the
    /// group. One for all but [`Kind::Color`], which is a combo *and* an
    /// entry.
    widgets: Vec<adw::PreferencesRow>,
    /// The typed handles a value is pushed into and read back out of.
    control: Control,
    /// Where provenance and refusals are written — a subtitle where the row
    /// has one, a suffix label where it does not (`AdwEntryRow` is an
    /// `AdwPreferencesRow`, not an `AdwActionRow`, and has no subtitle).
    note: Note,
    /// Removes the operator's own line for this key. Absent on a read-only
    /// row, where there is nothing to remove.
    reset: Option<gtk::Button>,
    /// A debounced save that has not fired yet.
    pending: Rc<Cell<Option<glib::SourceId>>>,
    /// **What the file last said for this key**, which is what a re-render
    /// compares against — never the widget's own text, which is the
    /// operator's draft until they apply it (#1338).
    seen: RefCell<Option<toml::Value>>,
    /// Whether a refusal is currently shown in place of the provenance.
    failed: Cell<bool>,
}

/// The typed half of a [`Row`].
enum Control {
    /// [`Kind::Bool`].
    Switch(adw::SwitchRow),
    /// [`Kind::Int`]. `words` is one toggle per `also` spelling — empty for
    /// every `Int` in the tree but `core-leds`' `rows`.
    Spin {
        /// The number.
        row: adw::SpinRow,
        /// `(spelling, its toggle)`.
        words: Vec<(&'static str, gtk::ToggleButton)>,
    },
    /// [`Kind::Choice`].
    Combo {
        /// The combo.
        row: adw::ComboRow,
        /// The vocabulary, by index — and the first `options.len()` items of
        /// `model`.
        options: &'static [&'static str],
        /// The items themselves: the vocabulary, plus a transient item for a
        /// value the vocabulary does not have (see [`Row::push`]).
        model: gtk::StringList,
    },
    /// [`Kind::Color`] — the named options plus a literal.
    Colour {
        /// The named colours, and one trailing *custom* item.
        combo: adw::ComboRow,
        /// The `#rrggbb` literal.
        entry: adw::EntryRow,
        /// The literal, painted.
        swatch: gtk::DrawingArea,
        /// The named colours, by index.
        options: &'static [&'static str],
    },
    /// [`Kind::Text`].
    Text(adw::EntryRow),
    /// [`Kind::List`] / [`Kind::Map`] — a summary, read-only in v1.
    Collection(adw::ActionRow),
}

/// Where a row's provenance line goes.
enum Note {
    /// The row's own subtitle — `AdwActionRow` and its three subclasses.
    Subtitle(adw::ActionRow),
    /// A dim suffix label, for `AdwEntryRow`, which has no subtitle.
    Suffix(gtk::Label),
}

impl Note {
    /// Write the line.
    fn set(&self, text: &str) {
        match self {
            Self::Subtitle(row) => row.set_subtitle(&glib::markup_escape_text(text)),
            Self::Suffix(label) => label.set_text(text),
        }
    }

    /// Read it back — what a test asserts on, so it reads the widget rather
    /// than this module's own bookkeeping.
    #[cfg(test)]
    fn get(&self) -> String {
        match self {
            Self::Subtitle(row) => row.subtitle().map(|s| s.to_string()).unwrap_or_default(),
            Self::Suffix(label) => label.label().to_string(),
        }
    }

    /// Mark (or unmark) the line as a refusal.
    fn set_error(&self, failed: bool) {
        let widget: &gtk::Widget = match self {
            Self::Subtitle(row) => row.upcast_ref(),
            Self::Suffix(label) => label.upcast_ref(),
        };
        if failed {
            widget.add_css_class("error");
        } else {
            widget.remove_css_class("error");
        }
    }
}

impl Row {
    /// Build the widgets for one field. No handlers yet — those need the
    /// [`FormInner`] that will own this row (see [`connect_rows`]).
    fn build(field: &'static Field, ops: FamilyOps) -> Self {
        let title = humanise(leaf_of(field.path));
        let tooltip = tooltip_for(ops.family, field);
        let (widgets, control, note, reset) = widgets_for(field.kind, &title);

        for widget in &widgets {
            widget.set_tooltip_text(Some(&tooltip));
        }

        Self {
            field,
            widgets,
            control,
            note,
            reset,
            pending: Rc::new(Cell::new(None)),
            seen: RefCell::new(None),
            failed: Cell::new(false),
        }
    }
}

/// The widgets one [`Kind`] draws as, with no handlers on them yet.
///
/// Split out of [`Row::build`] because these arms are the substance of this
/// module and a `Row` is otherwise four fields of bookkeeping.
// Seven kinds; each arm is a handful of lines and splitting them further would
// scatter the one table the module doc's own is written from.
#[allow(clippy::too_many_lines)]
fn widgets_for(
    kind: Kind,
    title: &str,
) -> (Vec<adw::PreferencesRow>, Control, Note, Option<gtk::Button>) {
    {
        match kind {
            Kind::Bool => {
                let row = adw::SwitchRow::builder().title(title).build();
                let reset = reset_button(&row);
                (
                    vec![row.clone().upcast()],
                    Control::Switch(row.clone()),
                    Note::Subtitle(row.upcast()),
                    Some(reset),
                )
            }
            Kind::Int { min, max, also } => {
                // The bounds come from the parser (`Kind::Int`'s own doc), so
                // the widget cannot offer a value the loader would warn
                // about. The casts are the only way `AdwSpinRow` takes them.
                #[allow(clippy::cast_precision_loss)]
                let row = adw::SpinRow::with_range(min as f64, max as f64, 1.0);
                row.set_title(title);
                let words = also
                    .iter()
                    .map(|word| {
                        let toggle = gtk::ToggleButton::builder()
                            .label(*word)
                            .valign(gtk::Align::Center)
                            .tooltip_text(format!("Use the word \"{word}\" instead of a number"))
                            .build();
                        row.add_suffix(&toggle);
                        (*word, toggle)
                    })
                    .collect();
                let reset = reset_button(&row);
                (
                    vec![row.clone().upcast()],
                    Control::Spin {
                        row: row.clone(),
                        words,
                    },
                    Note::Subtitle(row.upcast()),
                    Some(reset),
                )
            }
            Kind::Choice { options } => {
                let (row, model) = combo_row(title, options, None);
                let reset = reset_button(&row);
                (
                    vec![row.clone().upcast()],
                    Control::Combo {
                        row: row.clone(),
                        options,
                        model,
                    },
                    Note::Subtitle(row.upcast()),
                    Some(reset),
                )
            }
            Kind::Color { options } => {
                let (combo, _) = combo_row(title, options, Some(CUSTOM_COLOUR));
                let entry = adw::EntryRow::builder()
                    .title(format!("{title} — #rrggbb"))
                    .show_apply_button(true)
                    .build();
                let swatch = swatch();
                entry.add_prefix(&swatch);
                let reset = reset_button(&combo);
                (
                    vec![combo.clone().upcast(), entry.clone().upcast()],
                    Control::Colour {
                        combo: combo.clone(),
                        entry,
                        swatch,
                        options,
                    },
                    Note::Subtitle(combo.upcast()),
                    Some(reset),
                )
            }
            Kind::Text { .. } => {
                let row = adw::EntryRow::builder()
                    .title(title)
                    .show_apply_button(true)
                    .build();
                let label = gtk::Label::builder().valign(gtk::Align::Center).build();
                label.add_css_class("dim-label");
                row.add_suffix(&label);
                let reset = reset_button_for_entry(&row);
                (
                    vec![row.clone().upcast()],
                    Control::Text(row),
                    Note::Suffix(label),
                    Some(reset),
                )
            }
            Kind::List(_) | Kind::Map(_) => {
                let row = adw::ActionRow::builder().title(title).build();
                row.set_activatable(false);
                // The **summary** is what this row's subtitle is for, so its
                // provenance goes in a suffix label — the same place an
                // `AdwEntryRow`'s does, and for the same reason: one subtitle,
                // two things to say.
                let label = gtk::Label::builder().valign(gtk::Align::Center).build();
                label.add_css_class("dim-label");
                row.add_suffix(&label);
                (
                    vec![row.clone().upcast()],
                    Control::Collection(row),
                    Note::Suffix(label),
                    None,
                )
            }
        }
    }
}

impl Row {
    /// Push `raw`'s answer for this key onto the screen.
    ///
    /// The **value** is pushed only when this key's own value moved; the
    /// provenance and the sensitivity are set every time, because a lock can
    /// appear over a value that did not change at all (#1338 H2).
    fn apply(&self, raw: &Raw, editable: bool) {
        let value = raw.value(self.field.path).cloned();
        let locked = raw.is_locked(self.field.path);
        let origin = raw.origin(self.field.path);

        let absent = value.is_none();
        let moved = { *self.seen.borrow() != value };
        if moved {
            self.push(value.as_ref());
            *self.seen.borrow_mut() = value;
        }

        self.clear_error();
        self.note.set(&provenance(locked, origin, absent));
        let writable = editable && !locked && !self.field.kind.is_collection();
        self.set_sensitive(writable);
        if let Some(reset) = &self.reset {
            reset.set_sensitive(writable && matches!(origin, Some(Origin::Overlay)));
        }
    }

    /// Fill the widgets from a merged value.
    fn push(&self, value: Option<&toml::Value>) {
        match &self.control {
            Control::Switch(row) => row.set_active(value.and_then(toml::Value::as_bool).unwrap_or(false)),
            Control::Spin { row, words } => {
                let word = value.and_then(toml::Value::as_str);
                for (spelling, toggle) in words {
                    toggle.set_active(word == Some(*spelling));
                }
                if let Some(n) = value.and_then(toml::Value::as_integer) {
                    #[allow(clippy::cast_precision_loss)]
                    row.set_value(n as f64);
                }
            }
            Control::Combo { row, options } => {
                row.set_selected(index_of(options, value.and_then(toml::Value::as_str)));
            }
            Control::Colour {
                combo,
                entry,
                swatch,
                options,
            } => {
                let text = value.and_then(toml::Value::as_str).unwrap_or_default();
                let named = index_of(options, Some(text));
                if named == gtk::INVALID_LIST_POSITION {
                    // A literal (or something no layer should have written):
                    // the *custom* item is the last one, right after the
                    // named options.
                    let custom = u32::try_from(options.len()).unwrap_or(0);
                    combo.set_selected(custom);
                    entry.set_text(text);
                } else {
                    combo.set_selected(named);
                    entry.set_text("");
                }
                swatch.queue_draw();
            }
            Control::Text(row) => {
                row.set_text(value.and_then(toml::Value::as_str).unwrap_or_default());
            }
            Control::Collection(row) => row.set_subtitle(&summarise(self.field, value)),
        }
    }

    /// The value this row would save right now, or `None` when it has nothing
    /// legal to say (a blank entry, a combo on no selection).
    fn value(&self) -> Option<toml_edit::Value> {
        match &self.control {
            Control::Switch(row) => Some(row.is_active().into()),
            Control::Spin { row, words } => {
                if let Some((spelling, _)) = words.iter().find(|(_, toggle)| toggle.is_active()) {
                    return Some((*spelling).into());
                }
                // The adjustment clamps to the schema's own bounds, so the
                // truncation is of a value that is already whole and in range.
                #[allow(clippy::cast_possible_truncation)]
                Some((row.value().round() as i64).into())
            }
            Control::Combo { row, options } => options
                .get(usize::try_from(row.selected()).unwrap_or(usize::MAX))
                .map(|option| (*option).into()),
            Control::Colour {
                combo,
                entry,
                options,
                ..
            } => match options.get(usize::try_from(combo.selected()).unwrap_or(usize::MAX)) {
                Some(named) => Some((*named).into()),
                // The *custom* item: whatever the entry holds, judged by the
                // writer rather than here — one validator, and it is the
                // loader's (`Kind::accepts`).
                None => Some(entry.text().as_str().into()),
            },
            Control::Text(row) => Some(row.text().as_str().into()),
            Control::Collection(_) => None,
        }
    }

    /// Show a refusal in place of the provenance line.
    fn show_error(&self, message: &str) {
        self.failed.set(true);
        self.note.set(message);
        self.note.set_error(true);
    }

    /// Put the provenance line back.
    fn clear_error(&self) {
        if self.failed.replace(false) {
            self.note.set_error(false);
        }
    }

    /// Grey (or ungrey) every widget this leaf owns.
    fn set_sensitive(&self, sensitive: bool) {
        for widget in &self.widgets {
            widget.set_sensitive(sensitive);
        }
    }

    /// Drop a debounced save that has not fired yet.
    fn cancel_pending_save(&self) {
        if let Some(source) = self.pending.take() {
            source.remove();
        }
    }
}

/// Wire every row's controls to [`FormInner::schedule_save`].
///
/// A second pass, after the `Rc` exists: each handler captures it **weakly**
/// (see [`FormInner`]) plus the row's index, and upgrades per callback.
fn connect_rows(inner: &Rc<FormInner>) {
    for (index, row) in inner.rows.iter().enumerate() {
        match &row.control {
            Control::Switch(switch) => {
                let weak = Rc::downgrade(inner);
                switch.connect_active_notify(move |_| save_from_row(&weak, index));
            }
            Control::Spin {
                row: spin, words, ..
            } => {
                {
                    let weak = Rc::downgrade(inner);
                    let words: Vec<gtk::ToggleButton> =
                        words.iter().map(|(_, toggle)| toggle.clone()).collect();
                    spin.connect_value_notify(move |_| {
                        // Touching the number *is* choosing a number: a word
                        // toggle that was on turns itself off rather than
                        // silently outranking the value the operator just
                        // dialled in.
                        let Some(form) = weak.upgrade() else {
                            return;
                        };
                        if form.syncing.get() {
                            return;
                        }
                        form.syncing.set(true);
                        for toggle in &words {
                            toggle.set_active(false);
                        }
                        form.syncing.set(false);
                        save_from_row(&weak, index);
                    });
                }
                for (_, toggle) in words {
                    let weak = Rc::downgrade(inner);
                    toggle.connect_toggled(move |_| save_from_row(&weak, index));
                }
            }
            Control::Combo { row: combo, .. } => {
                let weak = Rc::downgrade(inner);
                combo.connect_selected_notify(move |_| save_from_row(&weak, index));
            }
            Control::Colour {
                combo,
                entry,
                swatch,
                ..
            } => {
                {
                    let weak = Rc::downgrade(inner);
                    combo.connect_selected_notify(move |_| save_from_row(&weak, index));
                }
                {
                    let weak = Rc::downgrade(inner);
                    let swatch = swatch.clone();
                    let combo = combo.clone();
                    let options_len = match row.control {
                        Control::Colour { options, .. } => u32::try_from(options.len()).unwrap_or(0),
                        _ => 0,
                    };
                    entry.connect_apply(move |_| {
                        swatch.queue_draw();
                        // Applying a literal means the operator wants the
                        // literal — move the combo onto its *custom* item so
                        // the two halves of one leaf cannot contradict each
                        // other.
                        if let Some(form) = weak.upgrade() {
                            form.syncing.set(true);
                            combo.set_selected(options_len);
                            form.syncing.set(false);
                        }
                        save_from_row(&weak, index);
                    });
                }
                {
                    let swatch = swatch.clone();
                    entry.connect_changed(move |_| swatch.queue_draw());
                }
                let entry = entry.clone();
                swatch.set_draw_func(move |_, cr, width, height| paint_swatch(cr, width, height, &entry.text()));
            }
            Control::Text(entry) => {
                let weak = Rc::downgrade(inner);
                entry.connect_apply(move |_| save_from_row(&weak, index));
            }
            Control::Collection(_) => {}
        }

        if let Some(reset) = &row.reset {
            let weak = Rc::downgrade(inner);
            reset.connect_clicked(move |_| {
                if let Some(form) = weak.upgrade() {
                    // `None` removes the operator's line; it deliberately
                    // does not write an `_unset` marker (spec §5).
                    form.schedule_save(index, None);
                }
            });
        }
    }
}

/// Queue the save a control's own change implies.
fn save_from_row(weak: &Weak<FormInner>, index: usize) {
    let Some(form) = weak.upgrade() else {
        return;
    };
    let Some(row) = form.rows.get(index) else {
        return;
    };
    let value = row.value();
    form.schedule_save(index, value);
}

// ── Row furniture ────────────────────────────────────────────────────────────

/// What the trailing item of a [`Kind::Color`] combo is called.
const CUSTOM_COLOUR: &str = "custom (#rrggbb)";

/// A combo row over `options`, with `extra` appended when there is one.
fn combo_row(title: &str, options: &[&str], extra: Option<&str>) -> adw::ComboRow {
    let model = gtk::StringList::new(&[]);
    for option in options {
        model.append(option);
    }
    if let Some(extra) = extra {
        model.append(extra);
    }
    let row = adw::ComboRow::builder().title(title).build();
    row.set_model(Some(&model));
    row
}

/// The per-row *reset*: removes the operator's own line for this key, so the
/// value falls back to the layer below (spec §4).
fn reset_button(row: &impl IsA<adw::ActionRow>) -> gtk::Button {
    let button = reset_widget();
    row.as_ref().add_suffix(&button);
    button
}

/// [`reset_button`] for an `AdwEntryRow`, which is not an `AdwActionRow`.
fn reset_button_for_entry(row: &adw::EntryRow) -> gtk::Button {
    let button = reset_widget();
    row.add_suffix(&button);
    button
}

/// The button itself.
fn reset_widget() -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Remove your own value and fall back to the layer below")
        .build();
    button.add_css_class("flat");
    button
}

/// The colour literal, painted — a plain square, so an `#rrggbb` is judged by
/// eye rather than by reading six hex digits.
fn swatch() -> gtk::DrawingArea {
    gtk::DrawingArea::builder()
        .content_width(16)
        .content_height(16)
        .valign(gtk::Align::Center)
        .build()
}

/// Fill the swatch with `text`, when `text` is a colour at all.
fn paint_swatch(cr: &gtk::cairo::Context, width: i32, height: i32, text: &str) {
    let hex = if text.starts_with('#') {
        text.to_owned()
    } else {
        format!("#{text}")
    };
    let Ok(rgba) = gtk::gdk::RGBA::parse(&hex) else {
        return;
    };
    cr.set_source_rgba(
        f64::from(rgba.red()),
        f64::from(rgba.green()),
        f64::from(rgba.blue()),
        f64::from(rgba.alpha()),
    );
    cr.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    // A failed fill is a paint that did not happen, on a 16x16 decoration.
    // There is nothing to recover and nothing to tell the operator.
    let _ = cr.fill();
}

/// `options`' index for `value`, or [`gtk::INVALID_LIST_POSITION`] when the
/// file holds something the vocabulary does not have — which is a thing a
/// hand-edited or a stale base layer can legitimately do, and the row then
/// shows no selection rather than silently presenting the first option as if
/// it were the file's word.
fn index_of(options: &[&str], value: Option<&str>) -> u32 {
    value
        .and_then(|value| options.iter().position(|option| *option == value))
        .and_then(|index| u32::try_from(index).ok())
        .unwrap_or(gtk::INVALID_LIST_POSITION)
}

/// What a read-only collection row says it holds.
fn summarise(field: &Field, value: Option<&toml::Value>) -> String {
    let noun = leaf_of(field.path);
    let body = match value {
        Some(toml::Value::Array(items)) => {
            let names: Vec<String> = items.iter().map(spell_value).collect();
            format!("{} {} · {}", names.len(), plural(noun, names.len()), names.join(", "))
        }
        Some(toml::Value::Table(table)) => {
            let names: Vec<&str> = table.keys().map(String::as_str).collect();
            format!(
                "{} {} · {}",
                names.len(),
                plural(noun, names.len()),
                names.join(", ")
            )
        }
        Some(other) => spell_value(other),
        None => format!("no {}", plural(noun, 0)),
    };
    format!("{body} — editable in #888 P2")
}

/// A TOML scalar as one word.
fn spell_value(value: &toml::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), std::borrow::ToOwned::to_owned)
}

/// `workspace` → `workspaces`, `entry` → `entries`, and `1 workspace` stays
/// singular.
///
/// English, three rules deep, because the nouns are config **key names** and
/// the four families' are `workspace`, `display`, `order` and `entry`. A
/// fifth family's key that these rules get wrong costs a summary line a
/// letter, not a save.
fn plural(noun: &str, count: usize) -> String {
    if count == 1 {
        return noun.to_owned();
    }
    let consonant_y = noun
        .strip_suffix('y')
        .is_some_and(|stem| stem.ends_with(|c| !matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')));
    if consonant_y {
        format!("{}ies", &noun[..noun.len() - 1])
    } else if noun.ends_with(['s', 'x', 'z']) || noun.ends_with("ch") || noun.ends_with("sh") {
        format!("{noun}es")
    } else {
        format!("{noun}s")
    }
}

/// The row's subtitle: where the value the row is showing came from.
///
/// A **locked** key says so first and names the layer the kept value came
/// from, which is the file the operator has to go and edit (and the same one
/// `Loaded::lock_findings` names in the journal). `load_raw` attributes a
/// refused override to that base rather than to the overlay, precisely so
/// this line cannot say *Yours* over a value nix is holding.
fn provenance(locked: bool, origin: Option<&Origin>, absent: bool) -> String {
    let where_from = match origin {
        Some(Origin::Default) => "the built-in default".to_owned(),
        Some(Origin::Base(path)) => path.display().to_string(),
        Some(Origin::Overlay) => "your own config".to_owned(),
        None => String::new(),
    };
    if locked {
        return if where_from.is_empty() {
            "Set in nix".to_owned()
        } else {
            format!("Set in nix — {where_from}")
        };
    }
    match origin {
        Some(Origin::Default) => "Default".to_owned(),
        Some(Origin::Base(path)) => format!("From {}", path.display()),
        Some(Origin::Overlay) => "Yours".to_owned(),
        None if absent => "Not set by any layer".to_owned(),
        None => String::new(),
    }
}

/// The row's tooltip: the field's one sentence, then the paragraph
/// `DEFAULT_TOML` carries above that key — which is where the long
/// explanation lives (spec §2a), and the only place most of these keys are
/// documented at all.
fn tooltip_for(family: &Family, field: &Field) -> String {
    match comment_above(family.default_toml, field.path) {
        Some(block) if block != field.doc => format!("{}\n\n{block}", field.doc),
        _ => field.doc.to_owned(),
    }
}

/// A group's title: the table's name, or the family's for the root group.
fn group_title(family: &Family, table: Option<&str>) -> String {
    table.map_or_else(|| humanise(family.name), humanise)
}

/// A group's description: the table's own comment block (or the file
/// header's first paragraph for the root group), plus one sentence about
/// where a save lands — or why there is none.
fn group_description(ops: FamilyOps, table: Option<&str>) -> String {
    let documented = table.map_or_else(
        || first_paragraph(ops.family.default_toml),
        |table| comment_above(ops.family.default_toml, table),
    );
    let mut parts: Vec<String> = documented.into_iter().collect();
    if table.is_none() {
        parts.push(if ops.editable {
            format!(
                "Saved to {}.toml under your own config, which the shell re-reads within a few \
                 seconds — so this works whether or not trollshell is running, and hand edits to \
                 that file are preserved.",
                ops.family.name
            )
        } else {
            format!(
                "Read-only here: editing {} is #888 P2. Every row still says where its value \
                 comes from.",
                ops.family.name
            )
        });
    }
    parts.join("\n\n")
}

/// The contiguous comment block immediately above `path` in `default_toml`,
/// as prose.
///
/// `toml_edit`'s decor is what carries it — the prefix of the key (or of the
/// table header, for a `[table]` path) is every byte between the previous
/// item and this one, blank lines and `#` included. Only the **last**
/// contiguous run of comment lines is the key's own: a file header separated
/// by a blank line belongs to the file, not to the first key under it.
fn comment_above(default_toml: &str, path: &str) -> Option<String> {
    let doc = default_toml.parse::<toml_edit::DocumentMut>().ok()?;
    let mut table = doc.as_table();
    let mut segments = path.split('.').peekable();
    let prefix = loop {
        let segment = segments.next()?;
        if segments.peek().is_some() {
            table = table.get(segment)?.as_table()?;
            continue;
        }
        break match table.get(segment) {
            // A `[table]` header carries its comment on the table's own
            // decor; a leaf carries it on its key's.
            Some(toml_edit::Item::Table(nested)) => nested.decor().prefix().cloned(),
            Some(_) => table.key(segment)?.leaf_decor().prefix().cloned(),
            None => None,
        };
    }?;
    prose(prefix.as_str()?)
}

/// The first paragraph of a file's own header comment.
fn first_paragraph(default_toml: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    for line in default_toml.lines() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('#') {
            break;
        }
        let body = strip_hash(trimmed);
        if body.is_empty() {
            break;
        }
        lines.push(body);
    }
    join_prose(&lines)
}

/// The trailing run of comment lines in a decor prefix, as prose.
fn prose(prefix: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    for line in prefix.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            // A blank line ends the previous block; what follows is this
            // key's own.
            lines.clear();
            continue;
        }
        let Some(body) = trimmed.strip_prefix('#') else {
            lines.clear();
            continue;
        };
        lines.push(body.strip_prefix(' ').unwrap_or(body));
    }
    join_prose(&lines)
}

/// `#`-stripped body of one comment line.
fn strip_hash(line: &str) -> &str {
    let body = line.strip_prefix('#').unwrap_or(line);
    body.strip_prefix(' ').unwrap_or(body).trim_end()
}

/// Join comment lines, keeping an indented continuation (`#   vfd  …`,
/// which is a table of values) on its own line and wrapping the rest.
fn join_prose(lines: &[&str]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let mut out = String::new();
    for line in lines {
        let indented = line.starts_with(' ');
        if !out.is_empty() {
            out.push(if indented || out.ends_with(':') { '\n' } else { ' ' });
        }
        out.push_str(line.trim_end());
    }
    Some(out)
}

/// The last segment of a dotted path — what a row inside a table's own group
/// is called.
fn leaf_of(path: &str) -> &str {
    path.rsplit_once('.').map_or(path, |(_, leaf)| leaf)
}

/// `poll_seconds` → `Poll seconds`, `cpu` → `CPU`, `core-leds` → `Core leds`.
fn humanise(segment: &str) -> String {
    let words: Vec<String> = segment
        .split(['_', '-'])
        .map(|word| {
            if ACRONYMS.contains(&word) {
                word.to_uppercase()
            } else {
                word.to_owned()
            }
        })
        .collect();
    let joined = words.join(" ");
    let mut chars = joined.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}

// ── A fixture family, for the tests below ────────────────────────────────────

/// A family that exists only for this module's tests: **one leaf of every
/// [`Kind`]**.
///
/// The four real families between them have no editable [`Kind::Text`] in P1
/// (`agents`, which owns the only one, is staged read-only) and none has all
/// seven kinds at once, so a per-kind test written against real families would
/// be both incomplete and hostage to a schema change somewhere else. It rides
/// the very same [`ShellSubsystem`] the two shell families do, so the shim is
/// exercised rather than bypassed — and `the_fixture_family_verifies` holds it
/// to its own documented default with the same walker CI runs over the real
/// four.
#[cfg(test)]
mod fixture {
    use hytte_config::schema::{Family, Field, Kind, Schema};

    /// The fixture's own file: `<config>/trollshell/form-fixture.toml`.
    pub(super) const FAMILY: Family = Family {
        name: "form-fixture",
        schema: &SCHEMA,
        default_toml: DEFAULT_TOML,
    };

    /// One leaf of every kind.
    pub(super) const SCHEMA: Schema = Schema {
        family: "form-fixture",
        fields: &[
            Field {
                path: "flag",
                kind: Kind::Bool,
                doc: "A switch.",
            },
            Field {
                path: "count",
                kind: Kind::Int {
                    min: 0,
                    max: 10,
                    also: &[],
                },
                doc: "A plain spin row.",
            },
            Field {
                path: "rows",
                kind: Kind::Int {
                    min: 0,
                    max: 64,
                    also: &["rect"],
                },
                doc: "A spin row that also takes a word.",
            },
            Field {
                path: "style",
                kind: Kind::Choice {
                    options: &["vfd", "lcd", "oled"],
                },
                doc: "A combo.",
            },
            Field {
                path: "color",
                kind: Kind::Color {
                    options: &["heat", "style"],
                },
                doc: "A combo plus a literal.",
            },
            Field {
                path: "label",
                kind: Kind::Text { blank_ok: false },
                doc: "An entry.",
            },
            Field {
                path: "order",
                kind: Kind::List(&Kind::Text { blank_ok: false }),
                doc: "A read-only list.",
            },
            Field {
                path: "entry",
                kind: Kind::Map(ENTRY_FIELDS),
                doc: "A read-only table of entries.",
            },
        ],
    };

    /// One `[entry.<name>]`.
    const ENTRY_FIELDS: &[Field] = &[Field {
        path: "name",
        kind: Kind::Text { blank_ok: false },
        doc: "What it is called.",
    }];

    /// The fixture's documented default — commented like a real one, because
    /// the tooltip tests read comments back out of it.
    pub(super) const DEFAULT_TOML: &str = r#"# A fixture family for the config form's own tests.
#
# It states one leaf of every scalar kind, so a per-kind test has somewhere to
# write.

# A switch.
flag = true

# A plain spin row.
count = 3

# A spin row that also takes a word.
rows = 4

# A combo.
style = "vfd"

# A combo plus a literal.
color = "heat"

# An entry.
label = "fixture"

# The two collections are the operator's, so the default states neither and
# documents the shape instead — the same reason workspaces.toml is comments
# only:
#
#     order = ["one", "two"]
#
#     [entry.one]
#     name = "the first one"
"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture family, as a [`ShellFamily`] — so its form goes through
    /// the same [`ShellSubsystem`] `core-leds` does.
    struct Fixture;

    impl ShellFamily for Fixture {
        const FAMILY: &'static Family = &fixture::FAMILY;
    }

    /// The fixture's ops, editable.
    pub(super) fn fixture_ops() -> FamilyOps {
        FamilyOps::of::<ShellSubsystem<Fixture>>(&fixture::FAMILY, true)
    }

    #[test]
    fn the_fixture_family_verifies() {
        assert_eq!(
            fixture::FAMILY.verify(),
            Ok(()),
            "the fixture's schema and its documented default must agree"
        );
    }

    /// The shim is only safe because it cannot say anything the family does
    /// not: `NAME` decides which file is read and written, and `DEFAULT_TOML`
    /// is both the bottom merge layer and the seed a first-ever save writes.
    /// Both are read off [`Family`], so this reds only if someone restates
    /// either by hand.
    #[test]
    fn the_shell_shims_are_the_familys_own_two_consts() {
        assert_eq!(
            <ShellSubsystem<CoreLeds> as Subsystem>::NAME,
            hytte_config_families::core_leds::FAMILY.name
        );
        assert_eq!(
            <ShellSubsystem<CoreLeds> as Subsystem>::DEFAULT_TOML,
            hytte_config_families::core_leds::FAMILY.default_toml
        );
        assert_eq!(
            <ShellSubsystem<Workspaces> as Subsystem>::NAME,
            hytte_config_families::workspaces::FAMILY.name
        );
        assert_eq!(
            <ShellSubsystem<Workspaces> as Subsystem>::DEFAULT_TOML,
            hytte_config_families::workspaces::FAMILY.default_toml
        );
    }

    /// Every family this app offers a form for agrees with its own documented
    /// default — the sweep, so a fifth family gets this for free.
    #[test]
    fn every_family_this_app_renders_verifies() {
        for ops in families() {
            assert_eq!(
                ops.family.verify(),
                Ok(()),
                "{} does not match its documented default",
                ops.family.name
            );
        }
    }

    /// `save_leaf_to_locked` refuses outright when `schema.family` is not
    /// `S::NAME`, so a mismatched pair here would be a form whose every save
    /// answers `NotALeaf`.
    #[test]
    fn every_familys_schema_names_the_family_it_is_for() {
        for ops in families() {
            assert_eq!(
                ops.family.schema.family, ops.family.name,
                "{}'s schema names another family",
                ops.family.name
            );
        }
    }

    /// The four are the two shell-owned plus the two plugin-owned — #888's
    /// erratum to §3, asserted rather than assumed.
    #[test]
    fn the_four_families_are_composed_from_three_crates() {
        let names: Vec<&str> = families().iter().map(|ops| ops.family.name).collect();
        assert_eq!(names, ["core-leds", "workspaces", "stats", "agents"]);
        let shell: Vec<&str> = shell_families().iter().map(|ops| ops.family.name).collect();
        assert_eq!(shell, ["core-leds", "workspaces"]);
        assert!(family("stats").is_some());
        assert!(family("nothing-like-it").is_none());
    }

    /// Spec §4's staging: `stats` and `core-leds` are the two editable forms;
    /// `agents` and `workspaces` render read-only until P2.
    #[test]
    fn only_the_two_first_forms_are_editable() {
        let editable: Vec<&str> = families()
            .iter()
            .filter(|ops| ops.editable)
            .map(|ops| ops.family.name)
            .collect();
        assert_eq!(editable, ["core-leds", "stats"]);
    }

    #[test]
    fn a_row_title_is_its_leaf_segment_humanised() {
        assert_eq!(humanise(leaf_of("bar.poll_seconds")), "Poll seconds");
        assert_eq!(humanise(leaf_of("sidebar.cpu")), "CPU");
        assert_eq!(humanise(leaf_of("sidebar.gpu")), "GPU");
        assert_eq!(humanise(leaf_of("style")), "Style");
        assert_eq!(humanise("core-leds"), "Core leds");
    }

    /// The tooltip's second half is the block **immediately** above the key —
    /// not the file header, which a blank line separates from it and which
    /// belongs to the file.
    #[test]
    fn the_tooltip_takes_the_block_above_the_key_and_not_the_file_header() {
        let block = comment_above(hytte_config_families::core_leds::DEFAULT_TOML, "style")
            .expect("core-leds documents `style`");
        assert!(
            block.starts_with("The kit skin"),
            "expected the key's own block, got {block:?}"
        );
        assert!(
            !block.contains("This file is read live"),
            "the file header is not this key's comment: {block:?}"
        );
        assert!(
            block.contains("vfd"),
            "the indented value table is part of the block: {block:?}"
        );
    }

    /// A dotted path reaches a key inside a table, and a table's own header
    /// comment is read off the table rather than off a key.
    #[test]
    fn a_comment_is_found_for_a_nested_key_and_for_a_table_header() {
        let leaf = comment_above(hytte_plugin_stats::config::DEFAULT_TOML, "bar.per_core")
            .expect("stats documents `[bar] per_core`");
        assert!(
            leaf.contains("one cell per logical core"),
            "expected the nested key's own block, got {leaf:?}"
        );
        let table = comment_above(hytte_plugin_stats::config::DEFAULT_TOML, "sidebar")
            .expect("stats documents the `[sidebar]` header");
        assert!(
            table.contains("right-sidebar card"),
            "expected the table's own block, got {table:?}"
        );
        assert!(
            !table.contains("ONE file, TWO instances"),
            "the file header is not the first table's comment: {table:?}"
        );
    }

    /// A key the default does not state has no block, and the row falls back
    /// to the field's one sentence.
    #[test]
    fn a_field_with_no_documented_block_keeps_its_one_sentence() {
        assert_eq!(comment_above(fixture::DEFAULT_TOML, "order"), None);
        let field = fixture::SCHEMA
            .field("order")
            .expect("the fixture declares `order`");
        assert_eq!(tooltip_for(&fixture::FAMILY, field), "A read-only list.");
    }

    #[test]
    fn provenance_names_the_layer_a_value_came_from() {
        assert_eq!(provenance(false, Some(&Origin::Default), false), "Default");
        assert_eq!(provenance(false, Some(&Origin::Overlay), false), "Yours");
        assert_eq!(
            provenance(
                false,
                Some(&Origin::Base(PathBuf::from(
                    "/etc/xdg/trollshell/core-leds.toml"
                ))),
                false
            ),
            "From /etc/xdg/trollshell/core-leds.toml"
        );
    }

    /// The locked line says *set in nix* first — the #1331 design's third
    /// point — and names the base the kept value came from, which is the file
    /// to go and edit.
    #[test]
    fn a_locked_row_says_set_in_nix_and_names_the_base() {
        let line = provenance(
            true,
            Some(&Origin::Base(PathBuf::from(
                "/etc/xdg/trollshell/core-leds.toml"
            ))),
            false,
        );
        assert!(line.starts_with("Set in nix"), "{line}");
        assert!(line.contains("/etc/xdg/trollshell/core-leds.toml"), "{line}");
    }

    #[test]
    fn a_collection_row_counts_what_it_holds_and_says_it_is_read_only() {
        let field = fixture::SCHEMA
            .field("entry")
            .expect("the fixture declares `entry`");
        let mut table = toml::Table::new();
        table.insert("chat".to_owned(), toml::Value::Table(toml::Table::new()));
        table.insert("dev".to_owned(), toml::Value::Table(toml::Table::new()));
        let summary = summarise(field, Some(&toml::Value::Table(table)));
        assert!(summary.starts_with("2 entries · chat, dev"), "{summary}");
        assert!(summary.contains("#888 P2"), "{summary}");
        assert_eq!(plural("workspace", 2), "workspaces");
        assert_eq!(plural("display", 2), "displays");

        let order = fixture::SCHEMA
            .field("order")
            .expect("the fixture declares `order`");
        let array = toml::Value::Array(vec![toml::Value::String("one".to_owned())]);
        assert!(
            summarise(order, Some(&array)).starts_with("1 order · one"),
            "a single entry stays singular"
        );
        assert!(
            summarise(order, None).starts_with("no orders"),
            "an absent collection says so"
        );
    }
}

/// The form on a real display: one test per [`Kind`] that the row renders
/// what the layers say and that changing it writes **one leaf**, plus the
/// four rules of §4–§5 that only a running form can show (a locked row, a
/// reset, the poll, and a draft surviving it).
///
/// Every test here routes through a `TempDir` as `$XDG_CONFIG_HOME` /
/// `$XDG_CONFIG_DIRS` — never the real `~/.config/trollshell`, which a test
/// that saved through it would both pollute and then be perturbed by
/// (#1101). That is what [`FormInner::layers`] being resolved from an
/// injected [`xdg::Env`] buys, and it is the reason [`build`] takes one.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::time::Instant;

    use super::tests::fixture_ops;
    use super::*;

    /// A scratch config tree: a `$XDG_CONFIG_HOME` and one `$XDG_CONFIG_DIRS`
    /// entry, both inside one `TempDir`.
    struct Scratch {
        dir: tempfile::TempDir,
    }

    impl Scratch {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("a scratch config tree"),
            }
        }

        fn env(&self) -> Rc<xdg::Env> {
            Rc::new(xdg::Env {
                home: None,
                config_home: Some(self.dir.path().join("home").to_string_lossy().into_owned()),
                config_dirs: Some(self.dir.path().join("etc").to_string_lossy().into_owned()),
                state_home: None,
            })
        }

        /// The operator's own file for `family`.
        fn overlay(&self, family: &str) -> PathBuf {
            self.dir
                .path()
                .join("home")
                .join("trollshell")
                .join(format!("{family}.toml"))
        }

        /// The nix-written base layer for `family`.
        fn base(&self, family: &str) -> PathBuf {
            self.dir
                .path()
                .join("etc")
                .join("trollshell")
                .join(format!("{family}.toml"))
        }

        /// Write a layer, creating its directory.
        fn write(path: &Path, text: &str) {
            std::fs::create_dir_all(path.parent().expect("a layer has a directory"))
                .expect("the scratch directory is writable");
            std::fs::write(path, text).expect("the scratch layer is writable");
        }

        /// A layer's bytes, or `""` when it does not exist.
        fn read(path: &Path) -> String {
            std::fs::read_to_string(path).unwrap_or_default()
        }
    }

    /// Run the main loop until `done`, or fail after five seconds.
    ///
    /// Not a fixed sleep: the save is debounced by
    /// [`SAVE_DEBOUNCE`](super::SAVE_DEBOUNCE), so what a test waits for is
    /// the *effect*, and waiting for it by polling keeps the suite as fast as
    /// the debounce and no slower.
    fn settle_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while glib::MainContext::default().iteration(false) {}
            if done() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Drain whatever the main loop has pending, then let one debounce window
    /// pass, so "nothing was written" is a claim about a save that had its
    /// chance rather than one that had not fired yet.
    fn settle_nothing() {
        let deadline = Instant::now() + SAVE_DEBOUNCE + Duration::from_millis(250);
        while Instant::now() < deadline {
            while glib::MainContext::default().iteration(false) {}
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The `(line before, line after)` pairs by which two files differ, by
    /// position.
    ///
    /// This is what "**exactly one leaf**" is asserted with: a writer that
    /// replaced the whole table would move or drop comment lines and blank
    /// lines too, so the count would not be one. **Red if
    /// `save_leaf_to_locked` stops writing a single leaf** — the #1359
    /// falsification.
    fn changed_lines(before: &str, after: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut a = before.lines();
        let mut b = after.lines();
        loop {
            match (a.next(), b.next()) {
                (None, None) => return out,
                (left, right) => {
                    let left = left.unwrap_or("<missing>");
                    let right = right.unwrap_or("<missing>");
                    if left != right {
                        out.push((left.to_owned(), right.to_owned()));
                    }
                }
            }
        }
    }

    /// The form's row for `path`.
    fn row_of<'a>(form: &'a Form, path: &str) -> &'a Row {
        form.inner
            .rows
            .iter()
            .find(|row| row.field.path == path)
            .unwrap_or_else(|| panic!("the form has a row for {path}"))
    }

    /// That row's provenance line, read off the widget.
    fn note_of(form: &Form, path: &str) -> String {
        row_of(form, path).note.get()
    }

    /// Whether that row's widgets are editable.
    fn sensitive(form: &Form, path: &str) -> bool {
        row_of(form, path)
            .widgets
            .iter()
            .all(gtk::prelude::WidgetExt::is_sensitive)
    }

    /// The fixture form over a scratch tree, with `base` written as the nix
    /// layer when given.
    fn fixture_form(scratch: &Scratch, base: Option<&str>) -> Form {
        if let Some(base) = base {
            Scratch::write(&scratch.base("form-fixture"), base);
        }
        build(fixture_ops(), &scratch.env())
    }

    // ── One test per Kind: it renders, and a change writes one leaf ─────────

    #[gtk::test]
    fn a_bool_row_renders_the_file_and_writes_one_leaf() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        let Control::Switch(switch) = &row_of(&form, "flag").control else {
            panic!("a Bool is a switch row");
        };
        assert!(switch.is_active(), "the row renders the documented default");
        assert_eq!(note_of(&form, "flag"), "Default");

        switch.set_active(false);
        let overlay = scratch.overlay("form-fixture");
        settle_until("the switch to be saved", || {
            Scratch::read(&overlay).contains("flag = false")
        });

        // The overlay is seeded with the documented default and then **one**
        // leaf is set, so the file is that default with exactly one line
        // different — comments, blank lines and key order all intact.
        let written = Scratch::read(&overlay);
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &written),
            vec![("flag = true".to_owned(), "flag = false".to_owned())],
            "exactly one leaf moved:\n{written}"
        );
        assert!(written.contains("# A switch."), "the comments survive");
        assert_eq!(note_of(&form, "flag"), "Yours");
    }

    #[gtk::test]
    fn an_int_row_renders_the_file_and_writes_one_leaf() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        let Control::Spin { row, .. } = &row_of(&form, "count").control else {
            panic!("an Int is a spin row");
        };
        assert!((row.value() - 3.0).abs() < f64::EPSILON, "the documented default");
        assert_eq!(note_of(&form, "count"), "Default");

        row.set_value(7.0);
        let overlay = scratch.overlay("form-fixture");
        settle_until("the spin row to be saved", || {
            Scratch::read(&overlay).contains("count = 7")
        });
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![("count = 3".to_owned(), "count = 7".to_owned())],
            "exactly one leaf moved"
        );
        assert_eq!(note_of(&form, "count"), "Yours");
    }

    /// `core-leds`' `rows` takes `0..=64` **or** the word `"rect"`
    /// (`Kind::Int`'s `also`, #1360 HIGH 2). The word is one suffix toggle;
    /// touching the number turns it back off, so the two halves of one leaf
    /// cannot contradict each other.
    #[gtk::test]
    fn an_int_row_with_a_word_can_write_the_word_and_then_a_number() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("rows = \"rect\"\n"));
        let Control::Spin { row, words } = &row_of(&form, "rows").control else {
            panic!("an Int is a spin row");
        };
        let (_, rect) = words.first().expect("`rows` declares the word `rect`");
        assert!(rect.is_active(), "the file's word is what the row shows");

        let overlay = scratch.overlay("form-fixture");
        row.set_value(9.0);
        settle_until("the number to be saved", || {
            Scratch::read(&overlay).contains("rows = 9")
        });
        assert!(
            !rect.is_active(),
            "dialling a number is choosing a number, so the word turns itself off"
        );
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![("rows = 4".to_owned(), "rows = 9".to_owned())],
            "exactly one leaf moved"
        );

        rect.set_active(true);
        settle_until("the word to be saved", || {
            Scratch::read(&overlay).contains("rows = \"rect\"")
        });
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![("rows = 4".to_owned(), "rows = \"rect\"".to_owned())],
            "and the word replaces the number in place"
        );
    }

    #[gtk::test]
    fn a_choice_row_renders_the_file_and_writes_one_leaf() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("style = \"lcd\"\n"));
        let Control::Combo { row, options } = &row_of(&form, "style").control else {
            panic!("a Choice is a combo row");
        };
        assert_eq!(
            options[usize::try_from(row.selected()).expect("a selected index")],
            "lcd",
            "the row renders the base layer's word"
        );

        row.set_selected(2);
        let overlay = scratch.overlay("form-fixture");
        settle_until("the combo to be saved", || {
            Scratch::read(&overlay).contains("style = \"oled\"")
        });
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![(
                "style = \"vfd\"".to_owned(),
                "style = \"oled\"".to_owned()
            )],
            "exactly one leaf moved"
        );
        assert_eq!(note_of(&form, "style"), "Yours");
    }

    /// A value no layer's vocabulary has — a hand edit, or a base layer from a
    /// newer shell — shows **no** selection rather than the first option
    /// presented as if it were the file's word.
    #[gtk::test]
    fn a_choice_row_shows_no_selection_for_a_word_it_does_not_know() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("style = \"plasma\"\n"));
        let Control::Combo { row, .. } = &row_of(&form, "style").control else {
            panic!("a Choice is a combo row");
        };
        assert_eq!(row.selected(), gtk::INVALID_LIST_POSITION);
    }

    /// `Kind::Color` is two rows for one leaf: the named palette, and the
    /// `#rrggbb` literal beside a swatch.
    #[gtk::test]
    fn a_colour_row_writes_a_name_from_the_combo_and_a_literal_from_the_entry() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("color = \"#ff8800\"\n"));
        let Control::Colour {
            combo,
            entry,
            options,
            ..
        } = &row_of(&form, "color").control
        else {
            panic!("a Color is a combo plus an entry");
        };
        let custom = u32::try_from(options.len()).expect("a handful of options");
        assert_eq!(
            combo.selected(),
            custom,
            "a literal selects the trailing custom item"
        );
        assert_eq!(entry.text(), "#ff8800", "and the literal is in the entry");

        let overlay = scratch.overlay("form-fixture");
        combo.set_selected(1);
        settle_until("the named colour to be saved", || {
            Scratch::read(&overlay).contains("color = \"style\"")
        });
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![(
                "color = \"heat\"".to_owned(),
                "color = \"style\"".to_owned()
            )],
            "exactly one leaf moved"
        );

        entry.set_text("#102030");
        glib::prelude::ObjectExt::emit_by_name::<()>(entry, "apply", &[]);
        settle_until("the literal to be saved", || {
            Scratch::read(&overlay).contains("color = \"#102030\"")
        });
        assert_eq!(
            combo.selected(),
            custom,
            "applying a literal moves the combo onto its custom item"
        );
    }

    #[gtk::test]
    fn a_text_row_renders_the_file_and_writes_one_leaf_on_apply() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        let Control::Text(entry) = &row_of(&form, "label").control else {
            panic!("a Text is an entry row");
        };
        assert_eq!(entry.text(), "fixture");

        let overlay = scratch.overlay("form-fixture");
        entry.set_text("renamed");
        settle_nothing();
        assert_eq!(
            Scratch::read(&overlay),
            "",
            "typing is a draft: nothing is written until apply"
        );

        glib::prelude::ObjectExt::emit_by_name::<()>(entry, "apply", &[]);
        settle_until("the entry to be saved", || {
            Scratch::read(&overlay).contains("label = \"renamed\"")
        });
        assert_eq!(
            changed_lines(fixture::DEFAULT_TOML, &Scratch::read(&overlay)),
            vec![(
                "label = \"fixture\"".to_owned(),
                "label = \"renamed\"".to_owned()
            )],
            "exactly one leaf moved"
        );
    }

    /// A blank is not "unset" — the row's own **reset** is how that is spelled
    /// (#1360 LOW 8) — so the writer refuses it and the row says so instead of
    /// writing an invisible empty string.
    #[gtk::test]
    fn a_text_row_refuses_a_blank_on_the_row_rather_than_writing_one() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        let Control::Text(entry) = &row_of(&form, "label").control else {
            panic!("a Text is an entry row");
        };
        entry.set_text("   ");
        glib::prelude::ObjectExt::emit_by_name::<()>(entry, "apply", &[]);
        settle_until("the refusal to reach the row", || {
            note_of(&form, "label").contains("not blank")
        });
        assert_eq!(
            Scratch::read(&scratch.overlay("form-fixture")),
            "",
            "a refused save writes nothing at all"
        );
        assert!(row_of(&form, "label").failed.get());
    }

    /// `List` and `Map` are read-only in v1 (spec §1): a summary row, no
    /// controls, no reset.
    #[gtk::test]
    fn a_collection_row_is_a_read_only_summary() {
        let scratch = Scratch::new();
        let form = fixture_form(
            &scratch,
            Some("order = [\"one\", \"two\"]\n\n[entry.one]\nname = \"the first\"\n"),
        );
        let Control::Collection(row) = &row_of(&form, "order").control else {
            panic!("a List is an action row");
        };
        assert!(
            row.subtitle()
                .expect("the summary")
                .starts_with("2 orders · one, two"),
            "{:?}",
            row.subtitle()
        );
        assert!(!sensitive(&form, "order"), "a collection is read-only in v1");
        assert!(
            row_of(&form, "order").reset.is_none(),
            "and has nothing to reset"
        );
        let Control::Collection(entry) = &row_of(&form, "entry").control else {
            panic!("a Map is an action row");
        };
        assert!(
            entry
                .subtitle()
                .expect("the summary")
                .starts_with("1 entry · one"),
            "{:?}",
            entry.subtitle()
        );
    }

    // ── §5: locks, reset, the poll, and a draft that survives it ────────────

    /// The #1331 design's third point, finally on a row: a locked leaf is
    /// insensitive, says *Set in nix*, and a change made anyway is refused
    /// **before** a byte moves.
    ///
    /// **Red if the row builder stops reading the lock** — the #1359
    /// falsification.
    #[gtk::test]
    fn a_locked_row_is_insensitive_and_a_change_is_refused() {
        let scratch = Scratch::new();
        let form = fixture_form(
            &scratch,
            Some("_locked = [\"flag\"]\nflag = false\n"),
        );
        assert!(!sensitive(&form, "flag"), "a locked leaf is not editable");
        assert!(
            note_of(&form, "flag").starts_with("Set in nix"),
            "{}",
            note_of(&form, "flag")
        );
        assert!(
            !row_of(&form, "flag")
                .reset
                .as_ref()
                .expect("a scalar row has a reset")
                .is_sensitive(),
            "and there is nothing of yours to reset"
        );

        let Control::Switch(switch) = &row_of(&form, "flag").control else {
            panic!("a Bool is a switch row");
        };
        switch.set_active(true);
        settle_until("the refusal to reach the row", || {
            note_of(&form, "flag").contains("cannot be overridden")
        });
        assert_eq!(
            Scratch::read(&scratch.overlay("form-fixture")),
            "",
            "a locked key is refused before a byte moves"
        );
    }

    /// *Reset* removes the operator's line — and deliberately does **not**
    /// write an `_unset` marker (spec §5), so the value falls back to the
    /// layer below rather than being erased from it.
    #[gtk::test]
    fn reset_removes_the_leaf_and_falls_back_to_the_layer_below() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("count = 8\n"));
        let overlay = scratch.overlay("form-fixture");
        let Control::Spin { row, .. } = &row_of(&form, "count").control else {
            panic!("an Int is a spin row");
        };
        assert!((row.value() - 8.0).abs() < f64::EPSILON, "the base layer's value");

        row.set_value(2.0);
        settle_until("the spin row to be saved", || {
            Scratch::read(&overlay).contains("count = 2")
        });
        assert_eq!(note_of(&form, "count"), "Yours");

        row_of(&form, "count")
            .reset
            .as_ref()
            .expect("a scalar row has a reset")
            .emit_clicked();
        settle_until("the leaf to be removed", || {
            !Scratch::read(&overlay).contains("count = 2")
        });
        let written = Scratch::read(&overlay);
        assert!(
            !written.contains("count ="),
            "the key is gone, not set to the default: {written}"
        );
        assert!(
            !written.contains("_unset"),
            "and a reset is not an _unset marker: {written}"
        );
        assert!(
            (row.value() - 8.0).abs() < f64::EPSILON,
            "the row falls back to the base layer"
        );
        assert_eq!(
            note_of(&form, "count"),
            format!("From {}", scratch.base("form-fixture").display())
        );
    }

    /// A base layer that appears under a running form moves the row's
    /// provenance on the next poll.
    #[gtk::test]
    fn the_poll_flips_provenance_when_a_base_layer_changes_underneath() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        assert_eq!(note_of(&form, "count"), "Default");

        Scratch::write(&scratch.base("form-fixture"), "count = 5\n");
        assert!(form.refresh_from_disk(), "the layer moved");
        assert_eq!(
            note_of(&form, "count"),
            format!("From {}", scratch.base("form-fixture").display())
        );
        let Control::Spin { row, .. } = &row_of(&form, "count").control else {
            panic!("an Int is a spin row");
        };
        assert!((row.value() - 5.0).abs() < f64::EPSILON);
    }

    /// #1338's H2, on a row: a `nixos-rebuild` that pins a key the file
    /// already held **moves no value at all**, and a poll that compared
    /// values would leave the row editable over a value the next load
    /// reverts.
    ///
    /// **Red if the poll compares values only** — the #1359 falsification.
    #[gtk::test]
    fn the_poll_sees_a_lock_appear_over_a_value_that_did_not_move() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("flag = false\n"));
        assert!(sensitive(&form, "flag"), "not locked yet");

        // The same value, now pinned. Nothing in `table` differs.
        Scratch::write(
            &scratch.base("form-fixture"),
            "_locked = [\"flag\"]\nflag = false\n",
        );
        assert!(form.refresh_from_disk(), "the lock moved");
        assert!(!sensitive(&form, "flag"), "the row greys itself");
        assert!(
            note_of(&form, "flag").starts_with("Set in nix"),
            "{}",
            note_of(&form, "flag")
        );
    }

    /// The draft guard: a poll driven by **another** key's change must not
    /// overwrite what the operator is halfway through typing, because a row's
    /// text is their draft until they apply it.
    #[gtk::test]
    fn a_half_typed_entry_survives_a_poll_driven_by_another_key() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        let Control::Text(entry) = &row_of(&form, "label").control else {
            panic!("a Text is an entry row");
        };
        entry.set_text("half-typ");

        Scratch::write(&scratch.base("form-fixture"), "count = 5\n");
        assert!(form.refresh_from_disk(), "the other key moved");
        assert_eq!(entry.text(), "half-typ", "the draft is untouched");

        // …and the file's own word for that key still wins when it moves.
        Scratch::write(&scratch.base("form-fixture"), "label = \"from-nix\"\n");
        assert!(form.refresh_from_disk(), "this key moved");
        assert_eq!(entry.text(), "from-nix");
    }

    /// A layer that exists and cannot be parsed is said out loud, and nothing
    /// is offered for editing over a merged view that is not what the files
    /// say.
    #[gtk::test]
    fn an_unparsable_layer_is_named_and_the_form_goes_read_only() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, Some("this is not toml\n"));
        assert!(form.inner.banner.is_visible(), "the banner is up");
        assert!(
            form.inner
                .banner
                .subtitle()
                .expect("the banner says what happened")
                .contains("form-fixture.toml"),
            "naming the file: {:?}",
            form.inner.banner.subtitle()
        );
        assert!(!sensitive(&form, "flag"), "nothing is offered for editing");
    }

    // ── The real families, as the tab mounts them ───────────────────────────

    /// `stats.toml` is `[sidebar]` and `[bar]`, so its form is two groups of
    /// eight — the issue's own shape for it.
    #[gtk::test]
    fn the_stats_form_is_two_groups_of_eight_rows() {
        let scratch = Scratch::new();
        let ops = family("stats").expect("stats is one of the four");
        let form = build(ops, &scratch.env());
        let titles: Vec<String> = form
            .groups()
            .iter()
            .map(|group| group.title().to_string())
            .collect();
        assert_eq!(titles, ["Sidebar", "Bar"]);
        assert_eq!(form.inner.rows.len(), 16);
        assert!(
            form.groups()[0]
                .description()
                .expect("the table's own comment block")
                .contains("right-sidebar card"),
            "{:?}",
            form.groups()[0].description()
        );
        // Every row renders the documented default, with nothing on disk.
        assert_eq!(note_of(&form, "bar.cpu"), "Default");
        assert!(sensitive(&form, "bar.cpu"), "stats is editable in P1");
    }

    /// `core-leds` is the second form, and the one carrying the two kinds
    /// `stats` has not got.
    #[gtk::test]
    fn the_core_leds_form_is_one_group_with_a_choice_a_colour_and_a_word_taking_int() {
        let scratch = Scratch::new();
        let ops = family("core-leds").expect("core-leds is one of the four");
        let form = build(ops, &scratch.env());
        assert_eq!(form.groups().len(), 1);
        assert!(matches!(
            row_of(&form, "style").control,
            Control::Combo { .. }
        ));
        assert!(matches!(
            row_of(&form, "color").control,
            Control::Colour { .. }
        ));
        let Control::Spin { words, .. } = &row_of(&form, "rows").control else {
            panic!("`rows` is a spin row");
        };
        assert_eq!(words.len(), 1, "with the word `rect` beside the number");

        let overlay = scratch.overlay("core-leds");
        let Control::Combo { row, .. } = &row_of(&form, "style").control else {
            panic!("`style` is a combo row");
        };
        row.set_selected(2);
        settle_until("the skin to be saved", || {
            Scratch::read(&overlay).contains("style = \"oled\"")
        });
        assert_eq!(
            changed_lines(
                hytte_config_families::core_leds::DEFAULT_TOML,
                &Scratch::read(&overlay)
            ),
            vec![(
                "style = \"vfd\"".to_owned(),
                "style = \"oled\"".to_owned()
            )],
            "exactly one leaf moved in the real family's file too"
        );
    }

    /// The two families spec §4 stages as read-only render every row and
    /// offer none of them.
    #[gtk::test]
    fn the_read_only_families_render_but_do_not_offer_an_edit() {
        let scratch = Scratch::new();
        for name in ["agents", "workspaces"] {
            let ops = family(name).expect("one of the four");
            let form = build(ops, &scratch.env());
            assert!(!form.inner.rows.is_empty(), "{name} renders rows");
            for row in &form.inner.rows {
                assert!(
                    !row.widgets
                        .iter()
                        .any(gtk::prelude::WidgetExt::is_sensitive),
                    "{name}.{} is offered for editing in P1",
                    row.field.path
                );
            }
            assert!(
                form.groups()[0]
                    .description()
                    .expect("a description")
                    .contains("#888 P2"),
                "{name} says why it is read-only"
            );
        }
    }

    /// **Nothing the form wires up holds the form.**
    ///
    /// Every row handler and the poll capture a `Weak<FormInner>` and upgrade
    /// per callback ([`FormInner`]'s own doc, the Plugins tab's #943 lesson
    /// applied to a state struct) — so the handle a caller holds is the only
    /// strong reference, and dropping it is what stops the poll and frees the
    /// widgets. A single strong clone captured anywhere would make this count
    /// two and leak the form for the window's whole life.
    #[gtk::test]
    fn the_form_handle_is_the_only_strong_reference_to_it() {
        let scratch = Scratch::new();
        let form = fixture_form(&scratch, None);
        assert_eq!(
            Rc::strong_count(&form.inner),
            1,
            "a handler captured the form strongly"
        );
        // And `Drop` removes the live poll source, which `SourceId::remove`
        // documents as a programmer error to get wrong in either direction.
        drop(form);
        while glib::MainContext::default().iteration(false) {}
    }
}
