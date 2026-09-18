//! Editing a `Map` or a `List` — the sub-pages a collection row pushes
//! (#888 P2, #1373).
//!
//! P1 drew a [`Kind::Map`] or [`Kind::List`] leaf as a read-only summary row
//! and said so on it. This is the other half: the row is activatable and
//! pushes an `AdwNavigationPage`, and what is on that page depends on the
//! kind.
//!
//! | leaf | page | the write unit |
//! | --- | --- | --- |
//! | [`Kind::Map`] (`display`) | one row per entry, plus **Add** | one leaf, per sub-key |
//! | a map entry (`display.argus`) | the **generic form itself**, over the map's fields at the prefix `display.argus.` | one leaf |
//! | [`Kind::List`] of scalars (`order`) | one `AdwEntryRow` per item, plus a trailing blank one | the **whole array** |
//! | [`Kind::List`] of records (`apps`) | one row per element, plus **Add** | the **whole array** |
//! | one record (`apps[0]`) | the generic form again, over the element's fields | the **whole array** |
//!
//! # Why an entry page is the same `Form`
//!
//! Because [`Raw`] already keys everything by full dotted path.
//! `Raw::origins` carries `display.argus.label` as readily as `style`, and
//! `Raw::is_locked` walks *up* — so `_locked = ["display.argus"]` greys the
//! entry and `_locked = ["display"]` greys the map, with no code here. A
//! second, hand-written editor for entries would have had to re-derive
//! provenance, locks, the refusal-on-the-row, the draft guard and the
//! debounce; instead [`Subject::Leaves`] carries a prefix and the existing
//! form draws it. The only thing the schema structurally cannot do is *name*
//! that path, which is exactly what `save_leaf_to_locked_unchecked` is for
//! and why it exists.
//!
//! # Why a record page is not
//!
//! An array index is not a TOML path: nothing addresses `apps[0].id`, and
//! rule 3 of the layering replaces an array whole anyway. So a record page is
//! the same *rows* over a different [`Subject`] — [`Subject::Element`] — whose
//! save reads the array, patches one key of one element and writes the array
//! back. Its locks and its provenance are the **array's**, because that is
//! the granularity the layering has to offer.
//!
//! # What drives these pages
//!
//! The parent form's own 2 s poll, through [`refresh_open`] — not a timer per
//! page (#1373 item 7). Every open page answers "is my subject still there";
//! the first that says no is popped along with everything under it, which is
//! the `#963`/`#1246` "refresh what is on screen, park the rest" rule applied
//! to a drill-down. A page whose subject is still there re-renders only the
//! parts that moved, so a half-typed entry row survives a poll exactly as it
//! does on the family page.
//!
//! # Where they are pushed
//!
//! The nearest `AdwNavigationView` **above the row**, found at activation
//! time rather than passed in. That keeps the form mountable anywhere — the
//! Plugins tab's detail pane today, another tab tomorrow — and means a host
//! with no navigation view degrades to a logged no-op instead of a panic.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::glib;
use hytte_config::schema::{Field, Kind};
use hytte_config::subsystem::{ConfigError, Origin, Raw};
use hytte_config::{toml, toml_edit};

use super::{
    Control, Form, FormInner, Spec, Subject, build_form, humanise, leaf_of, parse_scalar, plural,
    provenance,
};

/// What the operator is told a collection page cannot be edited *for*.
const SET_IN_NIX: &str = "Set in nix. A base layer pins this, so nothing saved here would survive the next load — \
     edit the file the rows name, or the option that renders it.";

// ── One open sub-page ────────────────────────────────────────────────────────

/// A page this form pushed, and how the poll keeps it honest.
pub(super) struct SubPage {
    /// The pushed page — what a `popped` signal is matched against, and what
    /// [`pop_all`] navigates away from.
    pub(super) page: adw::NavigationPage,
    /// Re-render from a fresh read. **`false` means the subject is gone** and
    /// this page (with everything pushed on top of it) must come down.
    refresh: Box<dyn Fn(&Raw) -> bool>,
    /// Run when this page leaves the stack, however it leaves it — the
    /// operator pressing Back, the poll popping it, or the form being
    /// unmounted underneath it.
    ///
    /// One caller today: a record page sweeping up an element that carries
    /// nothing (#1383 review, MEDIUM 1). It **must** defer its work to an
    /// idle tick — `drop` runs from inside the `popped` handler, which holds
    /// [`FormInner::open`] — and it must tolerate the form already being
    /// gone.
    on_pop: Option<Box<dyn FnOnce()>>,
    /// A sub-form's handle, when this page has one — held so its widgets and
    /// its row handlers live exactly as long as the page does.
    _form: Option<Form>,
}

impl Drop for SubPage {
    fn drop(&mut self) {
        if let Some(on_pop) = self.on_pop.take() {
            on_pop();
        }
    }
}

/// Re-render every open sub-page from `raw`, popping the first whose subject
/// has gone and everything above it.
pub(super) fn refresh_open(inner: &FormInner, raw: &Raw) {
    // The borrow is dropped before anything is popped: `pop_from` takes the
    // same cell, and a `refresh` closure can reach widgets whose handlers
    // re-enter it (#643's shape).
    let mut gone = None;
    {
        let open = inner.open.borrow();
        for (index, sub) in open.iter().enumerate() {
            if !(sub.refresh)(raw) {
                gone = Some(index);
                break;
            }
        }
    }
    if let Some(index) = gone {
        pop_from(inner, index);
    }
}

/// Take down the sub-page at `index` and everything pushed on top of it.
fn pop_from(inner: &FormInner, index: usize) {
    let doomed: Vec<SubPage> = {
        let mut open = inner.open.borrow_mut();
        if index >= open.len() {
            return;
        }
        open.split_off(index)
    };
    let Some(first) = doomed.first() else {
        return;
    };
    let nav = inner.nav.borrow().as_ref().map(|(nav, _)| nav.clone());
    let Some(nav) = nav else {
        return;
    };
    // `pop_to_page` on the page *below* the first doomed one, rather than a
    // loop of `pop()`: a pop is animated, so the stack a loop reads back
    // mid-transition is not the stack it thinks it is.
    if let Some(previous) = nav.previous_page(&first.page) {
        nav.pop_to_page(&previous);
    }
    drop(doomed);
}

/// Take every open sub-page down — [`FormInner::drop`]'s call, when the form
/// itself is going away under pages that are still on screen.
pub(super) fn pop_all(nav: &adw::NavigationView, open: &mut Vec<SubPage>) {
    if let Some(first) = open.first()
        && let Some(previous) = nav.previous_page(&first.page)
    {
        nav.pop_to_page(&previous);
    }
    open.clear();
}

// ── Opening one ──────────────────────────────────────────────────────────────

/// A collection row was activated: push the page its [`Kind`] calls for.
pub(super) fn activate(weak: &Weak<FormInner>, index: usize) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    let Some(row) = inner.rows.get(index) else {
        return;
    };
    let Control::Collection(action) = &row.control else {
        return;
    };
    let Some(nav) = nav_above(action) else {
        tracing::warn!(
            family = inner.ops.family.name,
            key = row.key.as_str(),
            "a collection row was activated with no AdwNavigationView above it — nothing to \
             push its page onto"
        );
        return;
    };
    let key = row.key.clone();
    let page = match row.field.kind {
        Kind::Map(fields) => map_page(&inner, row.field, key, fields),
        Kind::List(Kind::Map(fields)) => records_page(&inner, row.field, key, fields),
        Kind::List(element) => list_page(&inner, row.field, key, element),
        // Unreachable: only a collection row carries a `Control::Collection`.
        _ => return,
    };
    push(&inner, &nav, page);
}

/// The nearest `AdwNavigationView` above `widget`, if this form is mounted in
/// one at all.
fn nav_above(widget: &impl IsA<gtk::Widget>) -> Option<adw::NavigationView> {
    widget
        .as_ref()
        .ancestor(adw::NavigationView::static_type())
        .and_downcast()
}

/// Push `page` onto `nav`, remember it, and — the first time — start watching
/// for the operator pressing Back.
fn push(inner: &Rc<FormInner>, nav: &adw::NavigationView, page: SubPage) {
    if inner.nav.borrow().is_none() {
        let weak = Rc::downgrade(inner);
        let handler = nav.connect_popped(move |_, popped| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            // Everything from the popped page up is off the stack. A page
            // this form already forgot (because `pop_from` split it off
            // before navigating) is simply not found, which is the no-op it
            // should be.
            //
            // Split off and dropped **outside** the borrow: a `SubPage`'s
            // `on_pop` can reach back into this same cell, and `truncate`
            // under a `borrow_mut` would be a `BorrowMutError` rather than a
            // missed sweep.
            let doomed: Vec<SubPage> = {
                let mut open = inner.open.borrow_mut();
                match open.iter().position(|sub| sub.page == *popped) {
                    Some(index) => open.split_off(index),
                    None => return,
                }
            };
            drop(doomed);
        });
        *inner.nav.borrow_mut() = Some((nav.clone(), handler));
    }
    nav.push(&page.page);
    inner.open.borrow_mut().push(page);
}

// ── A Map: one row per entry, plus Add ───────────────────────────────────────

/// One `[display.<name>]` page: the entries, and a way to add one.
struct MapPage {
    form: Weak<FormInner>,
    /// The map's own dotted path.
    key: String,
    /// One entry's fields.
    fields: &'static [Field],
    /// The map's own `Field`, for the noun its rows are counted in.
    field: &'static Field,
    group: adw::PreferencesGroup,
    /// Every row currently in [`Self::group`], for teardown before a rebuild
    /// — `places_tab::rebuild`'s shape, and never borrowed across a
    /// `remove()` (#643).
    rows: RefCell<Vec<gtk::Widget>>,
    /// What the map last said, so a poll that moved some *other* key does not
    /// tear this list down under a half-typed name.
    seen: RefCell<Option<toml::Value>>,
    /// The banner a refused write is reported in — a page-level surface,
    /// because the thing refused is the page's own (an add, a delete, a whole
    /// array), not any one row's.
    banner: adw::Banner,
}

fn map_page(
    inner: &Rc<FormInner>,
    field: &'static Field,
    key: String,
    fields: &'static [Field],
) -> SubPage {
    let title = humanise(leaf_of(&key));
    let group = adw::PreferencesGroup::builder()
        .title(title.clone())
        .description(field.doc)
        .build();
    let banner = adw::Banner::new("");
    let page = adw::PreferencesPage::new();
    page.add(&group);

    let map = Rc::new(MapPage {
        form: Rc::downgrade(inner),
        key,
        fields,
        field,
        group,
        rows: RefCell::new(Vec::new()),
        seen: RefCell::new(None),
        banner: banner.clone(),
    });
    map.rebuild(&inner.raw.borrow());

    let navigation = wrap(&title, &page, &banner, None);
    let refresh = {
        let map = Rc::clone(&map);
        Box::new(move |raw: &Raw| {
            let value = raw.value(&map.key).cloned();
            if *map.seen.borrow() != value {
                map.rebuild(raw);
            }
            // A map field always exists — it is a schema `Field` — so this
            // page's subject cannot go away underneath it.
            true
        }) as Box<dyn Fn(&Raw) -> bool>
    };
    SubPage {
        page: navigation,
        refresh,
        on_pop: None,
        _form: None,
    }
}

impl MapPage {
    /// Draw the entries `raw` holds, then the Add row when there is somewhere
    /// to add to.
    fn rebuild(self: &Rc<Self>, raw: &Raw) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let value = raw.value(&self.key).cloned();
        let locked = raw.is_locked(&self.key);
        self.banner.set_revealed(locked);
        if locked {
            self.banner.set_title(SET_IN_NIX);
        }

        let names: Vec<String> = value
            .as_ref()
            .and_then(toml::Value::as_table)
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default();

        let mut rows = Vec::with_capacity(names.len() + 1);
        for name in &names {
            let path = format!("{}.{name}", self.key);
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(name))
                .subtitle(glib::markup_escape_text(&entry_summary(
                    raw,
                    &path,
                    self.fields,
                )))
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                // Weakly (#1384 item 2, the #224 `WeakRef` contract): `page`
                // owns `self.group` owns `row` owns this closure, so a strong
                // capture here closes a cycle no external drop ever breaks —
                // `nix/lint-bind-pins.py` cannot see it, because the widget
                // this closure is connected to (`row`) is not the object
                // captured (`page`).
                let page = Rc::downgrade(self);
                let name = name.clone();
                row.connect_activated(move |_| {
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    let name = name.clone();
                    glib::idle_add_local_once(move || page.open(&name));
                });
            }
            self.group.add(&row);
            rows.push(row.upcast::<gtk::Widget>());
        }

        if !locked {
            let add = adw::EntryRow::builder()
                .title(format!("Add {}", article(leaf_of(&self.key))))
                .show_apply_button(true)
                .build();
            add.set_tooltip_text(Some(
                "A name with no dots in it. The entry appears in the file once you give it a \
                 value.",
            ));
            // Weakly, for `row.connect_activated`'s reason just above.
            let page = Rc::downgrade(self);
            let taken = names.clone();
            add.connect_apply(move |entry| {
                let Some(page) = page.upgrade() else {
                    return;
                };
                let name = entry.text().trim().to_owned();
                if let Err(why) = check_name(&name, &page.key, &taken) {
                    page.say(why);
                    return;
                }
                entry.set_text("");
                page.banner.set_revealed(false);
                // Deferred for `places_tab::add`'s reason: opening the entry
                // rebuilds this very group, and `entry` is a row in it,
                // mid-`apply`.
                let page = Rc::clone(&page);
                glib::idle_add_local_once(move || page.open(&name));
            });
            self.group.add(&add);
            rows.push(add.upcast::<gtk::Widget>());
        }

        *self.rows.borrow_mut() = rows;
        *self.seen.borrow_mut() = value;
        if names.is_empty() {
            self.group.set_description(Some(&format!(
                "{} No {} yet.",
                self.field.doc,
                plural(leaf_of(&self.key), 0)
            )));
        } else {
            self.group.set_description(Some(self.field.doc));
        }
    }

    /// Push one entry's page.
    fn open(self: &Rc<Self>, name: &str) {
        let Some(inner) = self.form.upgrade() else {
            return;
        };
        let Some(nav) = nav_above(&self.group) else {
            return;
        };
        let page = entry_page(&inner, &self.key, name, self.fields);
        push(&inner, &nav, page);
    }

    /// Say why something was refused, on this page.
    fn say(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }
}

/// What one entry's row says under its name: which of its keys are set, and
/// where the entry's values come from.
fn entry_summary(raw: &Raw, path: &str, fields: &[Field]) -> String {
    let set: Vec<&str> = fields
        .iter()
        .filter(|field| raw.value(&format!("{path}.{}", field.path)).is_some())
        .map(|field| field.path)
        .collect();
    let what = if set.is_empty() {
        "nothing set".to_owned()
    } else {
        set.join(", ")
    };
    format!("{what} — {}", provenance_of(raw, path, fields))
}

/// The provenance line for a whole entry: *Yours* only when at least one of
/// its leaves is, *Set in nix* when it or an ancestor is locked, and the base
/// layer's name otherwise — because an entry no layer but a base states is
/// one this form must not offer to delete (#1373 item 5).
fn provenance_of(raw: &Raw, path: &str, fields: &[Field]) -> String {
    let locked = raw.is_locked(path);
    let origins: Vec<&Origin> = fields
        .iter()
        .filter_map(|field| raw.origin(&format!("{path}.{}", field.path)))
        .collect();
    let mine = origins
        .iter()
        .any(|origin| matches!(origin, Origin::Overlay));
    let origin = if mine {
        Some(&Origin::Overlay)
    } else {
        origins.first().copied()
    };
    provenance(locked, origin, origins.is_empty())
}

/// Whether any layer states a value under the entry at `path` — which is what
/// "this entry exists" means when the map's keys are the operator's.
fn present_in(raw: &Raw, path: &str, fields: &[Field]) -> bool {
    fields
        .iter()
        .any(|field| raw.value(&format!("{path}.{}", field.path)).is_some())
}

/// Whether this form may offer to delete the entry at `path`.
///
/// Only when it is unlocked **and** at least one of its leaves is the
/// operator's own. An entry every layer below states is not this form's to
/// remove: dropping a base-layer entry needs an `_unset` marker, and §5 says
/// the form never writes one — that spelling is for scripts and nix.
fn deletable(raw: &Raw, path: &str, fields: &[Field]) -> bool {
    !raw.is_locked(path)
        && fields.iter().any(|field| {
            matches!(
                raw.origin(&format!("{path}.{}", field.path)),
                Some(Origin::Overlay)
            )
        })
}

/// A name a TOML path can carry as one segment (#1373 item 2, the rule
/// `nix/module-common.nix` asserts for `agents.display`).
///
/// A dot is refused because `_locked` spells a locked leaf as a **dotted**
/// path and `merge::collect_locks` splits it with no escaping, so
/// `display."a.b".icon` renders a lock indistinguishable from the nested
/// segments `a → b → icon` — the nix module refuses it at eval for exactly
/// this reason, and an editor that let one in would reintroduce it below nix.
///
/// `taken` is the names the map already holds. An existing name is **refused**
/// rather than silently opening that entry (#1383 review, LOW 3): *Add* and
/// *pick one of the rows above it* are two different things to have asked
/// for, and a form that answers the first by doing the second leaves the
/// operator editing a row they thought they had just created. `places_tab`
/// sidesteps the question by generating a unique name (`unused_name`); here
/// the name is the operator's to type, so the answer has to be said out loud.
pub(super) fn check_name(name: &str, key: &str, taken: &[String]) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("Give it a name first.");
    }
    if taken.iter().any(|had| had == name) {
        return Err("That name already exists — open its row above to edit it.");
    }
    if name.contains('.') {
        return Err(
            "A name cannot contain a dot: locks are written as dotted paths, so a dotted name \
             would pin the wrong key.",
        );
    }
    if name.starts_with('_') {
        return Err("Names starting with an underscore are reserved (_locked, _unset).");
    }
    // Defensive, and free: `key` is a schema path, so this can only fire if a
    // schema ever grew a quoted segment.
    if key.is_empty() {
        return Err("This collection has no path to add to.");
    }
    Ok(())
}

/// `entry` → `an entry`, `workspace` → `a workspace` — over the **singular**,
/// since what an Add row adds is one of them.
fn article(noun: &str) -> String {
    let noun = singular(noun);
    let vowel = noun.starts_with(['a', 'e', 'i', 'o', 'u']);
    format!("{} {noun}", if vowel { "an" } else { "a" })
}

/// [`super::plural`]'s inverse — `items` → `item`, `apps` → `app`, `boxes` →
/// `box`, and `order` → `order`.
///
/// Two rules deep and English-only, for `plural`'s own stated reason: the
/// nouns are config **key names**, and the ones in the tree are `order`,
/// `apps`, `workspace`, `display` and `entry`. A key these rules get wrong
/// costs a label a letter, not a save. It exists at all because a list's key
/// is plural by convention (`apps`, `order`) while a map's is singular
/// (`display`, `workspace`), and an Add row adds **one** of them — the naive
/// version says *Add an apps*.
///
/// The `-ies` rule `plural` has is deliberately **not** inverted: undoing it
/// means putting a `y` back (`entries` → `entry`), which this cannot do while
/// it borrows, and no key in the tree needs it. Such a noun is left alone,
/// which costs a label a letter.
fn singular(noun: &str) -> &str {
    if let Some(stem) = noun.strip_suffix("es")
        && (stem.ends_with(['s', 'x', 'z']) || stem.ends_with("ch") || stem.ends_with("sh"))
    {
        return stem;
    }
    match noun.strip_suffix('s') {
        Some(stem) if !stem.is_empty() && !stem.ends_with('s') => stem,
        _ => noun,
    }
}

/// `array` re-emitted with each record's keys in `order` first and the rest
/// after, in the order they had.
///
/// Every element, not only the one an edit touched: the array comes out of
/// the **merged** read, whose `toml::Table` is ordered by key rather than by
/// the bytes any layer wrote, so re-emitting it as-is would alphabetise
/// records the operator never opened. `order` is the schema's, which is what
/// `DEFAULT_TOML` documents and what the page renders — so the file reads the
/// way the page does. A key the schema does not know keeps its place at the
/// end rather than being dropped, which is the sibling writer's own promise.
pub(super) fn in_field_order(array: &toml_edit::Array, order: &[&str]) -> toml_edit::Array {
    let mut out = toml_edit::Array::new();
    for value in array {
        let Some(record) = value.as_inline_table() else {
            out.push_formatted(value.clone());
            continue;
        };
        let mut sorted = toml_edit::InlineTable::new();
        for key in order {
            if let Some(held) = record.get(key) {
                sorted.insert(*key, held.clone());
            }
        }
        for (key, held) in record {
            if !sorted.contains_key(key) {
                sorted.insert(key, held.clone());
            }
        }
        out.push(sorted);
    }
    out
}

// ── One map entry: the generic form, at a prefix ─────────────────────────────

fn entry_page(
    inner: &Rc<FormInner>,
    map_key: &str,
    name: &str,
    fields: &'static [Field],
) -> SubPage {
    let path = format!("{map_key}.{name}");
    let banner = adw::Banner::new("");
    let form = build_form(
        inner.ops,
        &Rc::clone(&inner.env),
        Spec {
            subject: Subject::Leaves {
                prefix: format!("{path}."),
            },
            fields,
            single_group: Some((
                name.to_owned(),
                format!(
                    "One {} of {}. Each row saves on its own, and the entry appears in \
                     {}.toml with the first value you give it.",
                    leaf_of(map_key),
                    humanise(leaf_of(map_key)),
                    inner.ops.family.name
                ),
            )),
            polls: false,
        },
    );

    let page = adw::PreferencesPage::new();
    for group in form.groups() {
        page.add(group);
    }

    let locked = inner.raw.borrow().is_locked(&path);
    let delete = (!locked).then(|| delete_button(inner, &path, name, fields, &banner));
    let navigation = wrap(name, &page, &banner, delete.as_ref());

    // A brand-new entry does not exist in any layer yet, so "it is not there"
    // cannot mean "it went away" until it has been there once. `existed` is
    // what tells the two apart — without it, opening Add's page would pop it
    // again on the next tick, before the operator had typed anything. Seeded
    // from the read this page was built against rather than left to the first
    // refresh, because an entry deleted *before* that first refresh would
    // otherwise never look like it had gone.
    let existed = Cell::new(present_in(&inner.raw.borrow(), &path, fields));
    let delete = delete.map(|button| button.downgrade());
    // The sub-form re-reads through its own handle, driven from here so one
    // poll serves the whole drill-down (#1373 item 7).
    let form_refresh = form.refresher();
    let refresh = Box::new(move |raw: &Raw| {
        let present = present_in(raw, &path, fields);
        if present {
            existed.set(true);
        } else if existed.get() {
            return false;
        }
        let locked = raw.is_locked(&path);
        banner.set_revealed(locked);
        if locked {
            banner.set_title(SET_IN_NIX);
        }
        if let Some(button) = delete.as_ref().and_then(glib::WeakRef::upgrade) {
            button.set_visible(deletable(raw, &path, fields));
        }
        form_refresh();
        true
    }) as Box<dyn Fn(&Raw) -> bool>;

    SubPage {
        page: navigation,
        refresh,
        on_pop: None,
        _form: Some(form),
    }
}

/// The destructive action in an entry page's header.
fn delete_button(
    inner: &Rc<FormInner>,
    path: &str,
    name: &str,
    fields: &'static [Field],
    banner: &adw::Banner,
) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Delete this entry")
        .visible(deletable(&inner.raw.borrow(), path, fields))
        .build();
    button.add_css_class("flat");
    button.add_css_class("destructive-action");
    let weak = Rc::downgrade(inner);
    let (path, name, banner) = (path.to_owned(), name.to_owned(), banner.clone());
    button.connect_clicked(move |anchor| {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        confirm_delete(&inner, anchor, &path, &name, fields, &banner);
    });
    button
}

/// Confirm, then remove the whole entry — `places_tab::confirm_delete`'s
/// shape, down to `AdwMessageDialog` rather than `AdwAlertDialog` (libadwaita
/// is pinned to `v1_4` here, and `AlertDialog` is 1.5).
fn confirm_delete(
    inner: &Rc<FormInner>,
    anchor: &gtk::Button,
    path: &str,
    name: &str,
    fields: &'static [Field],
    banner: &adw::Banner,
) {
    // A **mixed** entry — some leaves the operator's, some a base layer's —
    // loses only the operator's, because that is the only file this app
    // writes. Saying so is the whole point of the sentence: the row will
    // still be there afterwards, showing the base's values, and an operator
    // who was not told that would read it as a delete that failed.
    let raw = inner.raw.borrow();
    let inherited: Vec<&str> = fields
        .iter()
        .filter(|field| {
            matches!(
                raw.origin(&format!("{path}.{}", field.path)),
                Some(Origin::Default | Origin::Base(_))
            )
        })
        .map(|field| field.path)
        .collect();
    drop(raw);

    let mut body = format!(
        "Your own {} lines for it are removed from {}.toml, comments around them included. A \
         list inside it goes whole — arrays replace rather than merge, so if a layer below \
         states one, that layer's list is what comes back.",
        name, inner.ops.family.name
    );
    if !inherited.is_empty() {
        use std::fmt::Write as _;
        // Infallible into a `String`; the `Result` is the trait's, not this
        // formatter's.
        let _ = write!(
            body,
            "\n\nA layer below still sets {}, so the entry stays — with those values instead \
             of yours. Removing it there means editing that file, or the nix option that \
             renders it.",
            inherited.join(", ")
        );
    }

    let dialog = adw::MessageDialog::new(
        anchor.root().and_downcast::<gtk::Window>().as_ref(),
        Some(&format!("Delete \u{201c}{name}\u{201d}?")),
        Some(&body),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let weak = Rc::downgrade(inner);
    let (path, banner) = (path.to_owned(), banner.clone());
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        match remove_entry(&inner, &path) {
            // The page comes down on the next refresh, which the reload the
            // write just triggered has already run — the entry is gone, so
            // `existed && !present` pops it. Nothing to do here.
            Ok(()) => {}
            Err(err) => {
                banner.set_title(&err.to_string());
                banner.set_revealed(true);
            }
        }
    });
    dialog.present();
}

/// [`hytte_config::subsystem::remove_entry_to_locked`] for this form, then a
/// forced re-read so every open page sees it in the same beat.
pub(super) fn remove_entry(inner: &Rc<FormInner>, path: &str) -> Result<(), ConfigError> {
    let Some(overlay) = inner.overlay.as_ref() else {
        return Err(inner.not_a_leaf(
            path,
            "nowhere to write: neither $XDG_CONFIG_HOME nor $HOME is set",
        ));
    };
    let locked = inner.raw.borrow().locked.clone();
    let result = (inner.ops.remove_entry)(overlay, path, &locked);
    if result.is_ok() {
        inner.reload(true);
    } else {
        tracing::warn!(
            family = inner.ops.family.name,
            key = path,
            "an entry could not be removed"
        );
    }
    result
}

// ── A List of scalars: one entry row per item, the array written whole ───────

/// One `order = [...]` page.
struct ListPage {
    form: Weak<FormInner>,
    /// The array's own dotted path.
    key: String,
    /// What one item may be.
    element: &'static Kind,
    field: &'static Field,
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    seen: RefCell<Option<toml::Value>>,
    banner: adw::Banner,
}

fn list_page(
    inner: &Rc<FormInner>,
    field: &'static Field,
    key: String,
    element: &'static Kind,
) -> SubPage {
    let title = humanise(leaf_of(&key));
    let group = adw::PreferencesGroup::builder()
        .title(title.clone())
        .description(format!(
            "{} Arrays replace rather than merge, so this list is saved whole: your own file \
             states all of it, or none of it. Changing one item therefore copies the items a \
             layer below states into your own file too — reset the row to hand the whole list \
             back.",
            field.doc
        ))
        .build();
    let banner = adw::Banner::new("");
    let page = adw::PreferencesPage::new();
    page.add(&group);

    let list = Rc::new(ListPage {
        form: Rc::downgrade(inner),
        key,
        element,
        field,
        group,
        rows: RefCell::new(Vec::new()),
        seen: RefCell::new(None),
        banner: banner.clone(),
    });
    list.rebuild(&inner.raw.borrow());

    let navigation = wrap(&title, &page, &banner, None);
    let refresh = Box::new(move |raw: &Raw| {
        let value = raw.value(&list.key).cloned();
        if *list.seen.borrow() != value {
            list.rebuild(raw);
        }
        true
    }) as Box<dyn Fn(&Raw) -> bool>;
    SubPage {
        page: navigation,
        refresh,
        on_pop: None,
        _form: None,
    }
}

impl ListPage {
    fn rebuild(self: &Rc<Self>, raw: &Raw) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let value = raw.value(&self.key).cloned();
        let locked = raw.is_locked(&self.key);
        self.banner.set_revealed(locked);
        if locked {
            self.banner.set_title(SET_IN_NIX);
        }

        let items = items_of(value.as_ref());
        let noun = leaf_of(&self.key);
        let mut rows = Vec::with_capacity(items.len() + 1);
        for (slot, item) in items.iter().enumerate() {
            let row = adw::EntryRow::builder()
                .title(format!("{} {}", humanise(singular(noun)), slot + 1))
                .text(item)
                .show_apply_button(true)
                .sensitive(!locked)
                .build();
            {
                // Weakly (#1384 item 2): `page` owns `self.group` owns `row`
                // owns this closure — the same cycle `MapPage` closes without
                // this, see its own row handler's comment.
                let page = Rc::downgrade(self);
                row.connect_apply(move |entry| {
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    // A blank edit means *remove*, the `places_tab::list_group`
                    // rule — a list of blanks is not a thing this file can
                    // mean, and the alternative is a row nobody can get rid of.
                    let text = entry.text().trim().to_owned();
                    page.write(|items| {
                        if slot >= items.len() {
                        } else if text.is_empty() {
                            items.remove(slot);
                        } else {
                            items[slot].clone_from(&text);
                        }
                    });
                });
            }
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .tooltip_text("Remove")
                .valign(gtk::Align::Center)
                .sensitive(!locked)
                .build();
            remove.add_css_class("flat");
            {
                // Weakly, for the row handler's reason just above.
                let page = Rc::downgrade(self);
                remove.connect_clicked(move |_| {
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    page.write(|items| {
                        if slot < items.len() {
                            items.remove(slot);
                        }
                    });
                });
            }
            row.add_suffix(&remove);
            self.group.add(&row);
            rows.push(row.upcast::<gtk::Widget>());
        }

        if !locked {
            let add = adw::EntryRow::builder()
                .title(format!("Add {}", article(noun)))
                .show_apply_button(true)
                .build();
            // Weakly, for the row handler's reason above.
            let page = Rc::downgrade(self);
            add.connect_apply(move |entry| {
                let Some(page) = page.upgrade() else {
                    return;
                };
                let text = entry.text().trim().to_owned();
                if text.is_empty() {
                    return;
                }
                entry.set_text("");
                page.write(|items| items.push(text.clone()));
            });
            self.group.add(&add);
            rows.push(add.upcast::<gtk::Widget>());
        }

        *self.rows.borrow_mut() = rows;
        *self.seen.borrow_mut() = value;
        let _ = self.field;
    }

    /// Apply `edit` to the items and write the **whole array** back.
    fn write(self: &Rc<Self>, edit: impl FnOnce(&mut Vec<String>)) {
        let Some(inner) = self.form.upgrade() else {
            return;
        };
        let mut items = items_of(inner.raw.borrow().value(&self.key));
        edit(&mut items);

        let Some(overlay) = inner.overlay.as_ref() else {
            self.say("nowhere to write: neither $XDG_CONFIG_HOME nor $HOME is set");
            return;
        };
        let locked = inner.raw.borrow().locked.clone();
        // An empty list is a **removal**, not `[]`: the row's own reset means
        // "fall back to the layer below", and an empty array here would state
        // "no items" at the top of the precedence order, which is a different
        // thing and one rule 3 makes permanent.
        let result = if items.is_empty() {
            inner.write_array(overlay, &self.key, None, &locked)
        } else {
            match typed_array(&inner, &self.key, self.element, &items) {
                Err(err) => Err(err),
                Ok(array) => inner.write_array(overlay, &self.key, Some(array), &locked),
            }
        };
        match result {
            Ok(()) => {
                self.banner.set_revealed(false);
                inner.reload(true);
            }
            Err(err) => self.say(&err.to_string()),
        }
    }

    fn say(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }
}

/// Every element of `value` as text, or an empty list.
fn items_of(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map_or_else(|| item.to_string(), std::borrow::ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Every item's text, parsed through the list's own element [`Kind`] and
/// checked against it — the typed array [`FormInner::write_array`] gets,
/// rather than the string every element used to be pushed as (#1384 item 1).
///
/// `items_of` stringifies every element for a row's own text — the one
/// representation that works for a `Text`/`Choice`/`Color` row and every
/// other kind's display alike — so this is the one place that has to undo it
/// before the array reaches the writer. Before this, a `Kind::List(Kind::Int)`
/// or `List(Bool)` field was rewritten as strings on the very first edit: a
/// string is never what `Int`/`Bool`'s own [`Kind::accepts`] takes, so
/// `check_items`' successor here — [`FormInner::check`] — refused every
/// following save, **including a bare remove**, which re-emits the whole
/// array the same way.
///
/// Per element rather than `Kind::List(..).accepts(array)` once built, so the
/// refusal names *which* item (`key[slot]`, [`Subject::key_of`]'s own
/// spelling for an array element) and *what an item may be*, not what an
/// array may be — and so an item that will not even parse (`"abc"` against
/// `Kind::Int`) is refused with the same wording as one that parses but is
/// out of range, rather than silently becoming a string.
fn typed_array(
    inner: &Rc<FormInner>,
    key: &str,
    element: &'static Kind,
    items: &[String],
) -> Result<toml_edit::Array, ConfigError> {
    let mut array = toml_edit::Array::new();
    for (slot, text) in items.iter().enumerate() {
        let at = format!("{key}[{slot}]");
        let value = parse_scalar(*element, text).ok_or_else(|| ConfigError::Rejected {
            subsystem: inner.ops.family.name.to_owned(),
            key: at.clone(),
            found: text.clone(),
            expected: element.expected(),
        })?;
        inner.check(&at, *element, &value)?;
        array.push(value);
    }
    Ok(array)
}

// ── A List of records: rows in, a whole array out ────────────────────────────

/// One `apps = [{ id = … }, …]` page.
struct RecordsPage {
    form: Weak<FormInner>,
    key: String,
    fields: &'static [Field],
    field: &'static Field,
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<gtk::Widget>>,
    seen: RefCell<Option<toml::Value>>,
    banner: adw::Banner,
}

fn records_page(
    inner: &Rc<FormInner>,
    field: &'static Field,
    key: String,
    fields: &'static [Field],
) -> SubPage {
    let title = humanise(leaf_of(&key));
    let group = adw::PreferencesGroup::builder()
        .title(title.clone())
        .description(format!(
            "{} In order. Arrays replace rather than merge, so an edit here saves the whole \
             list.",
            field.doc
        ))
        .build();
    let banner = adw::Banner::new("");
    let page = adw::PreferencesPage::new();
    page.add(&group);

    let records = Rc::new(RecordsPage {
        form: Rc::downgrade(inner),
        key,
        fields,
        field,
        group,
        rows: RefCell::new(Vec::new()),
        seen: RefCell::new(None),
        banner: banner.clone(),
    });
    records.rebuild(&inner.raw.borrow());

    let navigation = wrap(&title, &page, &banner, None);
    let refresh = Box::new(move |raw: &Raw| {
        let value = raw.value(&records.key).cloned();
        if *records.seen.borrow() != value {
            records.rebuild(raw);
        }
        true
    }) as Box<dyn Fn(&Raw) -> bool>;
    SubPage {
        page: navigation,
        refresh,
        on_pop: None,
        _form: None,
    }
}

impl RecordsPage {
    fn rebuild(self: &Rc<Self>, raw: &Raw) {
        for row in self.rows.take() {
            self.group.remove(&row);
        }
        let value = raw.value(&self.key).cloned();
        let locked = raw.is_locked(&self.key);
        self.banner.set_revealed(locked);
        if locked {
            self.banner.set_title(SET_IN_NIX);
        }

        let count = value
            .as_ref()
            .and_then(toml::Value::as_array)
            .map_or(0, Vec::len);
        let noun = leaf_of(&self.key);
        let mut rows = Vec::with_capacity(count + 1);
        for index in 0..count {
            let title = record_title(raw, &self.key, index, self.fields)
                .unwrap_or_else(|| format!("{} {}", humanise(singular(noun)), index + 1));
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&title))
                .subtitle(glib::markup_escape_text(&record_summary(
                    raw,
                    &self.key,
                    index,
                    self.fields,
                )))
                .activatable(true)
                .build();
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                // Weakly (#1384 item 2): `page` owns `self.group` owns `row`
                // owns this closure — the same cycle `MapPage`'s own row
                // handler closes.
                let page = Rc::downgrade(self);
                row.connect_activated(move |_| {
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    glib::idle_add_local_once(move || page.open(index));
                });
            }
            if !locked {
                let remove = gtk::Button::builder()
                    .icon_name("list-remove-symbolic")
                    .tooltip_text("Remove")
                    .valign(gtk::Align::Center)
                    .build();
                remove.add_css_class("flat");
                // Weakly, for the row handler's reason above.
                let page = Rc::downgrade(self);
                remove.connect_clicked(move |_| {
                    let Some(page) = page.upgrade() else {
                        return;
                    };
                    page.write(|array| {
                        if index < array.len() {
                            array.remove(index);
                        }
                    });
                });
                row.add_suffix(&remove);
            }
            self.group.add(&row);
            rows.push(row.upcast::<gtk::Widget>());
        }

        if !locked {
            let add = adw::ActionRow::builder()
                .title(format!("Add {}", article(noun)))
                .subtitle("Appended to the end of the list once you name it")
                .activatable(true)
                .build();
            add.add_prefix(&gtk::Image::from_icon_name("list-add-symbolic"));
            // Weakly, for the row handler's reason above.
            let page = Rc::downgrade(self);
            add.connect_activated(move |_| {
                let Some(page) = page.upgrade() else {
                    return;
                };
                // `add` saves, and a save rebuilds this group — which would
                // mean removing *this row* from inside its own emission.
                glib::idle_add_local_once(move || page.add());
            });
            self.group.add(&add);
            rows.push(add.upcast::<gtk::Widget>());
        }

        *self.rows.borrow_mut() = rows;
        *self.seen.borrow_mut() = value;
        let _ = self.field;
    }

    /// Open a page for a record **one past the end**, so the first thing the
    /// operator does is name it — and **write nothing** on the way (#1383
    /// review, MEDIUM 1).
    ///
    /// This used to append an empty inline table and save it. That is the
    /// `places_tab::add` shape, and it is wrong here for a reason that is
    /// specific to what reads the file: the shell's `parse_apps` returns
    /// `Err` for an element with no `id`, and `parse_stack` then drops the
    /// stack's **whole** apps list — so pressing Add blanked a working
    /// stack's apps until an id was typed, and going Back without typing left
    /// the poison record in the file for good. A record comes into being with
    /// its name, which is the rule a map entry already followed one level up
    /// ([`MapPage`]'s Add writes nothing either).
    fn add(self: &Rc<Self>) {
        let at = self
            .form
            .upgrade()
            .map(|inner| {
                inner
                    .raw
                    .borrow()
                    .value(&self.key)
                    .and_then(toml::Value::as_array)
                    .map_or(0, Vec::len)
            })
            .unwrap_or_default();
        self.open(at);
    }

    /// Push one record's page.
    fn open(self: &Rc<Self>, index: usize) {
        let Some(inner) = self.form.upgrade() else {
            return;
        };
        let Some(nav) = nav_above(&self.group) else {
            return;
        };
        let page = record_page(&inner, &self.key, index, self.fields);
        push(&inner, &nav, page);
    }

    /// Apply `edit` to the array and write it back whole. `true` if it stuck.
    fn write(self: &Rc<Self>, edit: impl FnOnce(&mut toml_edit::Array)) -> bool {
        let Some(inner) = self.form.upgrade() else {
            return false;
        };
        let mut array = inner.array_at(&self.key);
        edit(&mut array);
        // Schema order, for [`in_field_order`]'s reason: the array came out of
        // the merged read, which is ordered by key rather than by any layer's
        // bytes.
        let order: Vec<&str> = self.fields.iter().map(|field| field.path).collect();
        let array = in_field_order(&array, &order);

        let Some(overlay) = inner.overlay.as_ref() else {
            self.say("nowhere to write: neither $XDG_CONFIG_HOME nor $HOME is set");
            return false;
        };
        let locked = inner.raw.borrow().locked.clone();
        let result = if array.is_empty() {
            inner.write_array(overlay, &self.key, None, &locked)
        } else {
            inner.write_array(overlay, &self.key, Some(array), &locked)
        };
        match result {
            Ok(()) => {
                self.banner.set_revealed(false);
                inner.reload(true);
                true
            }
            Err(err) => {
                self.say(&err.to_string());
                false
            }
        }
    }

    fn say(&self, message: &str) {
        self.banner.set_title(message);
        self.banner.set_revealed(true);
    }
}

/// A record's row title: the first [`Kind::Text`] field it actually carries
/// (`apps`' `id`), which is the only name a record has.
fn record_title(raw: &Raw, key: &str, index: usize, fields: &[Field]) -> Option<String> {
    let element = super::element_of(raw, key, index)?;
    fields
        .iter()
        .find(|field| matches!(field.kind, Kind::Text { .. }))
        .and_then(|field| element.get(field.path))
        .and_then(toml::Value::as_str)
        .map(std::borrow::ToOwned::to_owned)
}

/// A record's row subtitle: its other keys, spelled `key = value`.
fn record_summary(raw: &Raw, key: &str, index: usize, fields: &[Field]) -> String {
    let Some(element) = super::element_of(raw, key, index) else {
        return "not a table".to_owned();
    };
    let said: Vec<String> = fields
        .iter()
        .filter_map(|field| {
            element
                .get(field.path)
                .map(|value| format!("{} = {}", field.path, super::spell_value(value)))
        })
        .collect();
    if said.is_empty() {
        "nothing set".to_owned()
    } else {
        said.join(", ")
    }
}

fn record_page(
    inner: &Rc<FormInner>,
    key: &str,
    index: usize,
    fields: &'static [Field],
) -> SubPage {
    let banner = adw::Banner::new("");
    let title = record_title(&inner.raw.borrow(), key, index, fields)
        .unwrap_or_else(|| format!("{} {}", humanise(singular(leaf_of(key))), index + 1));
    let form = build_form(
        inner.ops,
        &Rc::clone(&inner.env),
        Spec {
            subject: Subject::Element {
                array: key.to_owned(),
                index,
                key_field: naming_field(fields),
            },
            fields,
            single_group: Some((
                title.clone(),
                format!(
                    "Entry {} of {}. Arrays replace rather than merge, so changing a row here \
                     saves the whole list — including anything a layer below states, which is \
                     copied into your own file the first time you save.",
                    index + 1,
                    humanise(leaf_of(key))
                ),
            )),
            polls: false,
        },
    );

    let page = adw::PreferencesPage::new();
    for group in form.groups() {
        page.add(group);
    }
    let navigation = wrap(&title, &page, &banner, None);

    let key = key.to_owned();
    let form_refresh = form.refresher();
    // A record the operator asked to **add** does not exist yet — `index` is
    // one past the end until its name is written — so "it is not there"
    // cannot mean "it went away" until it has been there once (#1383 review,
    // MEDIUM 1). Seeded from the read this page was built against, the entry
    // page's own shape.
    let existed = Cell::new(super::element_of(&inner.raw.borrow(), &key, index).is_some());
    let refresh = {
        let key = key.clone();
        Box::new(move |raw: &Raw| {
            // The element is this page's subject, so an array that shrank past
            // it — a sibling deleted, a hand edit, a nix rebuild — pops the
            // page rather than leaving a form writing into an index that moved.
            if super::element_of(raw, &key, index).is_some() {
                existed.set(true);
            } else if existed.get() {
                return false;
            }
            let locked = raw.is_locked(&key);
            banner.set_revealed(locked);
            if locked {
                banner.set_title(SET_IN_NIX);
            }
            form_refresh();
            true
        }) as Box<dyn Fn(&Raw) -> bool>
    };

    // A record carrying nothing at all is what `parse_apps` chokes on, taking
    // the stack's whole apps list with it — so one that is on screen and gets
    // left behind is swept up rather than kept (#1383 review, MEDIUM 1's
    // second half). Only a wholly empty element: anything the operator or a
    // layer actually states is theirs, and the list's Remove is how a record
    // with content goes. Deferred to an idle tick because this runs from
    // `SubPage::drop`, which fires from inside `open`'s own borrow on the
    // `popped` path.
    let weak = Rc::downgrade(inner);
    let on_pop = Box::new(move || {
        glib::idle_add_local_once(move || prune_empty_record(&weak, &key, index));
    }) as Box<dyn FnOnce()>;

    SubPage {
        page: navigation,
        refresh,
        on_pop: Some(on_pop),
        _form: Some(form),
    }
}

/// The field that **names** a record — the first [`Kind::Text`] of its own
/// fields, which is what [`record_title`] titles the row by.
fn naming_field(fields: &'static [Field]) -> Option<&'static str> {
    fields
        .iter()
        .find(|field| matches!(field.kind, Kind::Text { .. }))
        .map(|field| field.path)
}

/// Remove element `index` of the array at `key` when it carries nothing at
/// all — see [`record_page`]'s `on_pop`.
fn prune_empty_record(weak: &Weak<FormInner>, key: &str, index: usize) {
    let Some(inner) = weak.upgrade() else {
        return;
    };
    let empty =
        super::element_of(&inner.raw.borrow(), key, index).is_some_and(toml::Table::is_empty);
    if !empty {
        return;
    }
    let Some(overlay) = inner.overlay.as_ref() else {
        return;
    };
    let mut array = inner.array_at(key);
    if index >= array.len() {
        return;
    }
    array.remove(index);
    let locked = inner.raw.borrow().locked.clone();
    let array = (!array.is_empty()).then_some(array);
    if let Err(err) = inner.write_array(overlay, key, array, &locked) {
        tracing::warn!(
            family = inner.ops.family.name,
            key,
            %err,
            "an empty record could not be swept up"
        );
        return;
    }
    inner.reload(true);
}

// ── Furniture ────────────────────────────────────────────────────────────────

/// A sub-page's chrome: the tab's own header bar (no window controls — #944),
/// an optional destructive action in it, and the banner refusals land in.
fn wrap(
    title: &str,
    page: &adw::PreferencesPage,
    banner: &adw::Banner,
    action: Option<&gtk::Button>,
) -> adw::NavigationPage {
    let header = crate::plugins_tab::tab_header_bar();
    if let Some(action) = action {
        header.pack_end(action);
    }
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(banner);
    toolbar.set_content(Some(page));
    adw::NavigationPage::new(&toolbar, title)
}
