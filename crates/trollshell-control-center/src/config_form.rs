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
        /// What its items mean, by index.
        options: &'static [&'static str],
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
                let row = combo_row(title, options, None);
                let reset = reset_button(&row);
                (
                    vec![row.clone().upcast()],
                    Control::Combo {
                        row: row.clone(),
                        options,
                    },
                    Note::Subtitle(row.upcast()),
                    Some(reset),
                )
            }
            Kind::Color { options } => {
                let combo = combo_row(title, options, Some(CUSTOM_COLOUR));
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
                (
                    vec![row.clone().upcast()],
                    Control::Collection(row.clone()),
                    Note::Subtitle(row),
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

/// `workspace` → `workspaces`, and `1 workspace` stays singular.
fn plural(noun: &str, count: usize) -> String {
    if count == 1 {
        noun.to_owned()
    } else if noun.ends_with('s') {
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
