//! The Edit sub-page — #1071 §5, phase 4.
//!
//! Same drawer, content replaced: the name, the stack's apps as a list with an
//! editable launch command and a drag handle each, **Add app** through the
//! desktop-entry picker, the default layout, and the autostart switch. **No
//! monitor field** — a card's screen is set by dragging it between the page's
//! columns (Annika, on the epic thread), and phase 3 built that.
//!
//! ## Why this is not a `Page` variant
//!
//! `Page` is `Copy` with unit variants only, passed by value at ~60 sites
//! (`modal.rs`), so a sub-page keyed by *which card* cannot be one. It rides the
//! shape the plugin panel already uses — [`crate::modal`]'s `Active::Plugin`
//! becoming `Active::WorkspaceEdit` — a drawer child keyed by a string, outside
//! the `Page` enum. §5 fixes exactly that, and phase 1's PR body promised it.
//!
//! ## Why the form takes a whole [`Draft`] rather than reading the world
//!
//! Two reasons, one of them a bug this page would otherwise ship with.
//!
//! * **The page must survive the refresh poll.** `workspaces.toml` is
//!   live-reloaded and niri's event stream re-fires for the life of the session;
//!   a form rebound to either would have the user's half-typed name and
//!   half-edited app list thrown away every three seconds. Phase 2 hit the
//!   smaller version of this with the ephemeral card's Save field and answered
//!   it with `dedupe_cloned`; a whole form needs the stronger answer, which is
//!   to seed **once** per opening and hold its own state after that.
//! * **The card already knows.** The Workspaces page's `model` has joined the
//!   file, niri and systemd into exactly the facts this form edits. Re-deriving
//!   them here would be a second join that can disagree with the one the user is
//!   looking at.
//!
//! So [`open`] publishes a `Draft` and the form is a pure function of it. The
//! consequence to know about: with drawers open on two monitors at once, both
//! show a form seeded from the same `Draft` and each holds its own edits — the
//! same shape the plugin panel's single global selection already has.
//!
//! ## What is pure here
//!
//! [`plan_save`], [`move_app`] and [`ephemeral_apps`] are the three decisions,
//! and none of them touches GTK, the filesystem or niri. That is what makes
//! §7's phase-4 rows falsifiable: the validator's refusal, the drag's rewrite
//! and §3.7's "an `app_id` with no entry becomes an app with `exec` = the
//! process's command line" are each a function of a snapshot.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use hytte::futures_signals::signal::{Mutable, Signal, SignalExt};
use hytte::gtk::{self, gdk, glib, pango, prelude::*};
use hytte::prelude::*;
use hytte::services::systemd;

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::app_picker::add_app_button;
use crate::components::layout::{DRAWER_MAX_WIDTH_WIDE, finish_page_clamped, page_box};
use crate::config::workspaces::{Layout, Stack, StackApp};
use crate::workspace_stacks;

/// CSS class on one app row, and the hook a test counts rows by.
const APP_ROW_CLASS: &str = "ts-ws-edit-app";

/// CSS class on an app row's launch-command entry.
const EXEC_ENTRY_CLASS: &str = "ts-ws-edit-exec";

/// CSS class on the name field.
const NAME_ENTRY_CLASS: &str = "ts-ws-edit-name";

/// CSS class on the page root, so a test can tell an edit page from the card
/// page inside the same drawer stack.
const EDIT_PAGE_CLASS: &str = "ts-ws-edit";

/// The layouts the dropdown offers, in the order it offers them.
const LAYOUTS: [Layout; 4] = [Layout::None, Layout::Equal, Layout::Golden, Layout::Split];

/// Shown when the drawer is switched to this page with nothing to edit — after
/// a Save, or if something reaches the child directly.
const NOTHING_HINT: &str = "No workspace is being edited.";

/// Shown under an Active stack's name field, where a rename is refused.
const RENAME_BLOCKED_HINT: &str = "Stop this workspace before renaming it — its apps are running \
                                   in a systemd slice named after it.";

/// Placeholder on an app's launch command, which is optional.
const EXEC_PLACEHOLDER: &str = "Launch command (leave empty for the desktop entry's own)";

/// What the Edit sub-page is showing (#1071 §5).
///
/// Seeded by the card that opened it and then owned by the form; see the module
/// doc for why it is not re-derived.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Draft {
    /// The name this stack already has in `workspaces.toml`. `None` for an
    /// ephemeral card, which has none until Save creates its entry (#1071 §3.7)
    /// — and which is therefore also what tells a Save whether it is creating or
    /// replacing.
    pub previous: Option<String>,
    /// The name in the field. Starts as `previous` (or empty).
    pub name: String,
    /// In stack order, which **is** niri's column order (#1071 §3.4 step 3).
    pub apps: Vec<StackApp>,
    pub layout: Layout,
    pub autostart: bool,
    /// The screen the stack records. **Not editable here** — §5 has no monitor
    /// field — but carried through so a Save writes back what the drag set
    /// rather than dropping it.
    pub monitor: Option<String>,
    /// The niri workspace this card is. `Some` for an ephemeral card, where
    /// §3.7's Save has to *name* it, and for an Active saved stack.
    pub workspace: Option<u64>,
    /// The stack is running. A rename is refused while it is — see
    /// [`plan_save`].
    pub active: bool,
}

impl Draft {
    /// The key this draft is filed under in `Active::WorkspaceEdit`.
    ///
    /// A saved stack is keyed by its name. An ephemeral card has none, so it is
    /// keyed by its niri workspace id behind a `#`, which
    /// [`systemd::is_valid_workspace_name`] refuses — so the two keyspaces
    /// cannot collide however a workspace is named.
    pub(crate) fn key(&self) -> String {
        match (&self.previous, self.workspace) {
            (Some(name), _) => name.clone(),
            (None, Some(workspace)) => format!("#{workspace}"),
            (None, None) => String::new(),
        }
    }
}

/// Why a Save cannot go ahead (#1071 §5/§3.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SaveError {
    /// The typed name is not a usable workspace name. Carries the sanitised form
    /// to offer, when there is one — §3.1's *"refuses anything else and offers
    /// the sanitised form"*.
    InvalidName { typed: String, suggestion: Option<String> },
    /// The stack is Active and the name changed. Its apps are running in
    /// `trollshell-ws-<old>.slice` and its units are named after it, so a rename
    /// would leave Stop looking for a slice that no longer matches anything —
    /// the apps would be unstoppable from this page. Refusing is the honest
    /// answer; renaming the live workspace *and* every unit is not a rename, it
    /// is a restart.
    RenameWhileActive,
}

/// What a Save does (#1071 §5, §3.7).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SavePlan {
    /// The entry to replace, or `None` when this Save is creating one.
    pub previous: Option<String>,
    /// The name to file it under, normalised.
    pub name: String,
    pub stack: Stack,
    /// The niri workspace to **name** as part of this Save — §3.7's *"then name
    /// the niri workspace … so the saved card IS the Active card"*. `Some` only
    /// for an ephemeral card: a saved stack's workspace already carries its
    /// name, or has none to carry.
    pub name_workspace: Option<u64>,
}

/// Decide a Save. Pure (#1071 §5).
///
/// The name is normalised the way every other workspace name is — case folded,
/// because niri matches workspace names case-insensitively and a stack's name is
/// also a systemd slice name (§3.1). A name the validator refuses comes back as
/// [`SaveError::InvalidName`] carrying the sanitised form to *offer*: applying it
/// silently would hand the user a stack under a name they did not type, while
/// `niri msg action focus-workspace` still answers to the one they did.
///
/// An app whose launch command is blank or whitespace keeps `exec: None` rather
/// than `Some("")`, which is what the reader refuses at load (#1101's LOWs) and
/// what would otherwise be written straight back into the file.
pub(crate) fn plan_save(draft: &Draft) -> Result<SavePlan, SaveError> {
    let Some(name) = systemd::normalize_workspace_name(&draft.name) else {
        return Err(SaveError::InvalidName {
            typed: draft.name.clone(),
            suggestion: sanitise(&draft.name),
        });
    };
    let renaming = draft
        .previous
        .as_deref()
        .is_some_and(|previous| !previous.eq_ignore_ascii_case(&name));
    if renaming && draft.active {
        return Err(SaveError::RenameWhileActive);
    }
    let stack = Stack {
        monitor: draft.monitor.clone(),
        autostart: draft.autostart,
        layout: draft.layout,
        apps: draft
            .apps
            .iter()
            .map(|app| StackApp {
                id: app.id.clone(),
                exec: app
                    .exec
                    .as_deref()
                    .map(str::trim)
                    .filter(|exec| !exec.is_empty())
                    .map(str::to_owned),
            })
            .collect(),
    };
    Ok(SavePlan {
        // An ephemeral card's Save is a creation, and §3.7 routes it through
        // `workspace_stacks::save` so the file write and the `SetWorkspaceName`
        // are one transaction. A saved card's is a replacement.
        name_workspace: draft.previous.is_none().then_some(draft.workspace).flatten(),
        previous: draft.previous.clone(),
        name,
        stack,
    })
}

/// The typed text as the nearest usable workspace name, or `None` when there is
/// nothing left to suggest.
///
/// Duplicated from `panels::workspaces`' own `sanitise` on purpose: the two
/// fields it serves are the *same* rule stated at the *same* moment in two
/// different forms, and folding them together would put a helper in
/// `components/` whose only job is to be shared. If a third field ever needs it,
/// that is the moment.
fn sanitise(typed: &str) -> Option<String> {
    let mut out = String::new();
    for ch in typed.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    let clipped = trimmed.get(..trimmed.len().min(32)).unwrap_or(trimmed);
    let candidate = clipped.trim_matches('-');
    (!candidate.is_empty() && candidate != typed).then(|| candidate.to_owned())
}

/// `apps` with the entry at `from` moved to sit at `to` (#1071 §5's drag
/// handles, whose order **is** niri's column order — §3.4 step 3).
///
/// Remove-then-insert, which is what keeps every other app's relative order
/// exactly as it was: a rebuild that collected "the ones before" and "the ones
/// after" would reorder the rest whenever `from` and `to` straddle them.
///
/// Out-of-range indices, and `from == to`, return the list unchanged rather than
/// panicking: a drop's indices come from GTK, and a list that rebuilt between
/// the drag starting and the drop landing is an ordinary race, not a bug worth
/// aborting the shell over.
#[must_use]
pub(crate) fn move_app(apps: &[StackApp], from: usize, to: usize) -> Vec<StackApp> {
    let mut out = apps.to_vec();
    if from >= out.len() || to >= out.len() || from == to {
        return out;
    }
    let moved = out.remove(from);
    out.insert(to, moved);
    out
}

/// The apps an ephemeral card's Edit form opens with (#1071 §3.7).
///
/// `windows` is `(app_id, command line)` in **niri's column order**, one entry
/// per window on the workspace; `known` is the set of app-ids that resolve to an
/// installed desktop entry.
///
/// §3.7: *"map each `app_id` through `resolve_app_meta` — an `app_id` with no
/// entry becomes an app with `exec` = the process's command line, shown for
/// correction in Edit"*. So an id with an entry is stored bare (the entry's own
/// `Exec` is resolved at Start time, which is what §3.2 is for) and an id
/// without one carries the command line that is running *right now*, which is
/// the best available guess and is sitting in an editable field precisely
/// because it is a guess.
///
/// Deduped by app-id, first occurrence wins, so two windows of one app are one
/// entry in the stack — the card's row already shows them as one icon.
#[must_use]
pub(crate) fn ephemeral_apps(
    windows: &[(String, Option<String>)],
    known: &BTreeSet<String>,
) -> Vec<StackApp> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    windows
        .iter()
        .filter(|(app_id, _)| seen.insert(app_id.as_str()))
        .map(|(app_id, cmdline)| StackApp {
            id: app_id.clone(),
            exec: if known.contains(app_id) {
                None
            } else {
                cmdline
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_owned)
            },
        })
        .collect()
}

/// The process command line behind a window, for §3.7's unknown-`app_id` case.
///
/// `/proc/<pid>/cmdline` is NUL-separated; the arguments are joined with spaces,
/// which is a *lossy* rendering when one of them contains a space — and that is
/// fine here and nowhere else, because the result lands in an editable field
/// under a label that says it is a guess. Nothing launches it without the user
/// having looked at it.
fn cmdline_of(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let joined = raw
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// [`ephemeral_apps`] for a live workspace: resolve each window's entry and read
/// the command line of the ones with none.
///
/// Runs on the GTK main thread (it resolves desktop entries) and is the one
/// impure half of §3.7's mapping; the decision itself is [`ephemeral_apps`].
#[must_use]
pub(crate) fn ephemeral_apps_for(windows: &[(String, Option<u32>)]) -> Vec<StackApp> {
    let known: BTreeSet<String> = windows
        .iter()
        .map(|(app_id, _)| app_id.clone())
        .filter(|app_id| crate::components::desktop_entry::launchable(app_id).is_some())
        .collect();
    let resolved: Vec<(String, Option<String>)> = windows
        .iter()
        .map(|(app_id, pid)| (app_id.clone(), pid.and_then(cmdline_of)))
        .collect();
    ephemeral_apps(&resolved, &known)
}

// ── The published selection ──────────────────────────────────────────────────

thread_local! {
    /// Which card the Edit sub-page is showing, on this thread's drawers.
    ///
    /// Thread-local and not a service handle: it is the drawer's own transient
    /// UI state, it is only ever read and written on the GTK main thread, and
    /// nothing off that thread has any business in it — the same reasoning that
    /// keeps `modal`'s `PANELS` thread-local.
    static TARGET: Mutable<Option<Draft>> = Mutable::new(None);
}

/// Publish the card the Edit sub-page should show.
///
/// Called by the Workspaces page's Edit buttons, immediately before asking
/// `modal` to switch the drawers to this child.
pub(crate) fn open(draft: Draft) {
    TARGET.with(|target| target.set(Some(draft)));
}

/// Clear it — what Cancel and a finished Save do.
pub(crate) fn close() {
    TARGET.with(|target| target.set(None));
}

/// The drawer child. Added once per drawer, like the plugin slot.
pub fn edit_slot() -> gtk::Widget {
    build_slot(TARGET.with(|target| target.signal_cloned()))
}

/// [`edit_slot`] with its selection injected, so a `#[gtk::test]` can drive the
/// form without the thread-local.
fn build_slot<S>(target: S) -> gtk::Widget
where
    S: Signal<Item = Option<Draft>> + 'static,
{
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class(EDIT_PAGE_CLASS);
    root.set_vexpand(true);

    // `dedupe_cloned` is what makes the module doc's "survives the refresh poll"
    // true: the selection is re-published by nothing here, but a `Mutable`'s
    // signal replays on subscribe and any future caller of `open` with an equal
    // draft must not tear the form down under the user's cursor.
    bind(target.dedupe_cloned(), &root, move |root, draft| {
        while let Some(child) = root.first_child() {
            root.remove(&child);
        }
        match draft {
            Some(draft) => root.append(&build_form(&draft)),
            None => {
                let hint = gtk::Label::new(Some(NOTHING_HINT));
                hint.add_css_class("ts-ws-empty");
                hint.set_xalign(0.0);
                root.append(&hint);
            }
        }
    });
    root.upcast()
}

/// The form for one card.
fn build_form(seed: &Draft) -> gtk::Widget {
    // The form's own state. Every control writes into it; Save reads it once.
    // An `Rc<RefCell<…>>` rather than a `Mutable` because nothing subscribes —
    // the app list is the only part that redraws, and it redraws because an
    // edit *asked* it to, not because a signal fired.
    let draft = Rc::new(RefCell::new(seed.clone()));

    let column = page_box();
    column.add_css_class("ts-popup-column");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_tooltip_text(Some("Back to the workspaces"));
    back.connect_clicked(|_| cancel());
    header.append(&back);

    let title = gtk::Label::new(Some(if seed.previous.is_some() {
        "Edit workspace"
    } else {
        "Save this workspace"
    }));
    title.add_css_class("title-4");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    header.append(&title);
    column.append(&header);

    // ── Name ────────────────────────────────────────────────────────────────
    let name = gtk::Entry::builder()
        .text(&seed.name)
        .placeholder_text("Workspace name\u{2026}")
        .max_length(32)
        .hexpand(true)
        .build();
    name.add_css_class(NAME_ENTRY_CLASS);
    {
        let draft = Rc::clone(&draft);
        name.connect_changed(move |entry| {
            entry.remove_css_class("error");
            entry.set_tooltip_text(None);
            draft.borrow_mut().name = entry.text().to_string();
        });
    }
    column.append(&labelled("Name", &name));
    if seed.active && seed.previous.is_some() {
        let note = gtk::Label::new(Some(RENAME_BLOCKED_HINT));
        note.add_css_class("ts-ws-empty");
        note.set_xalign(0.0);
        note.set_wrap(true);
        column.append(&note);
    }

    // ── Apps ────────────────────────────────────────────────────────────────
    let apps = gtk::ListBox::new();
    apps.add_css_class("boxed-list");
    apps.add_css_class("ts-ws-edit-apps");
    apps.set_selection_mode(gtk::SelectionMode::None);

    let meta_cache: MetaCache = Rc::new(RefCell::new(HashMap::new()));
    // `Rc<dyn Fn()>` so a row's own buttons can ask for the list to be rebuilt
    // after they have changed the draft under it — a row cannot rebuild the list
    // it is a child of from inside its own callback without this indirection.
    let redraw: Rc<RefCell<Option<Rc<dyn Fn()>>>> = Rc::new(RefCell::new(None));
    {
        let draft = Rc::clone(&draft);
        let meta_cache = Rc::clone(&meta_cache);
        let apps_weak = apps.downgrade();
        let redraw_slot = Rc::clone(&redraw);
        let rebuild: Rc<dyn Fn()> = Rc::new(move || {
            let Some(apps) = apps_weak.upgrade() else {
                return;
            };
            while let Some(child) = apps.first_child() {
                apps.remove(&child);
            }
            let rows = draft.borrow().apps.clone();
            if rows.is_empty() {
                let empty = gtk::Label::new(Some("No apps yet — add one below."));
                empty.add_css_class("ts-ws-empty");
                empty.set_xalign(0.0);
                apps.append(&empty);
                return;
            }
            for (index, app) in rows.iter().enumerate() {
                apps.append(&app_row(
                    index,
                    app,
                    &meta_cache,
                    &draft,
                    &redraw_slot,
                ));
            }
        });
        *redraw.borrow_mut() = Some(Rc::clone(&rebuild));
        rebuild();
    }
    column.append(&section_label("Apps"));
    column.append(&apps);

    let add = add_app_button({
        let draft = Rc::clone(&draft);
        let redraw = Rc::clone(&redraw);
        move |id| {
            draft.borrow_mut().apps.push(StackApp {
                id: id.to_owned(),
                exec: None,
            });
            fire(&redraw);
        }
    });
    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    add_row.set_halign(gtk::Align::Start);
    add_row.append(&add);
    column.append(&add_row);

    // ── Layout ──────────────────────────────────────────────────────────────
    let layouts = gtk::StringList::new(&LAYOUTS.map(Layout::name));
    let layout = gtk::DropDown::builder().model(&layouts).build();
    layout.add_css_class("ts-ws-edit-layout");
    layout.set_selected(
        u32::try_from(
            LAYOUTS
                .iter()
                .position(|l| *l == seed.layout)
                .unwrap_or_default(),
        )
        .unwrap_or_default(),
    );
    {
        let draft = Rc::clone(&draft);
        layout.connect_selected_notify(move |dropdown| {
            let chosen = usize::try_from(dropdown.selected()).unwrap_or_default();
            draft.borrow_mut().layout = LAYOUTS.get(chosen).copied().unwrap_or_default();
        });
    }
    column.append(&labelled("Default layout", &layout));

    // ── Autostart ───────────────────────────────────────────────────────────
    let autostart = gtk::Switch::new();
    autostart.add_css_class("ts-ws-edit-autostart");
    autostart.set_active(seed.autostart);
    autostart.set_halign(gtk::Align::Start);
    {
        let draft = Rc::clone(&draft);
        autostart.connect_state_set(move |_, on| {
            draft.borrow_mut().autostart = on;
            glib::Propagation::Proceed
        });
    }
    column.append(&labelled("Start at login", &autostart));

    // ── Save / Cancel ───────────────────────────────────────────────────────
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.set_halign(gtk::Align::End);
    actions.set_margin_top(6);

    let cancel_button = gtk::Button::with_label("Cancel");
    cancel_button.add_css_class("ts-ws-edit-cancel");
    cancel_button.connect_clicked(|_| cancel());
    actions.append(&cancel_button);

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.add_css_class("ts-ws-edit-save");
    {
        let draft = Rc::clone(&draft);
        let name = name.downgrade();
        save.connect_clicked(move |_| {
            let plan = plan_save(&draft.borrow());
            match plan {
                Ok(plan) => {
                    commit(&plan);
                    close();
                    crate::modal::switch_active(crate::modal::Page::Workspaces);
                }
                Err(why) => {
                    // The correction surface is the field, beside the cursor —
                    // the same call phase 2's Save field makes. A toast would
                    // put the rule somewhere the user is not looking.
                    if let Some(name) = name.upgrade() {
                        name.add_css_class("error");
                        name.set_tooltip_text(Some(&refusal(&why)));
                        name.grab_focus();
                    }
                }
            }
        });
    }
    actions.append(&save);
    column.append(&actions);

    finish_page_clamped(&column, DRAWER_MAX_WIDTH_WIDE)
}

/// Run the stored rebuild closure, if the form is still alive.
fn fire(redraw: &Rc<RefCell<Option<Rc<dyn Fn()>>>>) {
    // Cloned out of the `RefCell` before it is called: a rebuild appends rows
    // whose own callbacks hold this same `Rc`, and a `Ref` held across that is
    // the re-entrant borrow that aborts the process (#643/#663/#832).
    let held = redraw.borrow().clone();
    if let Some(held) = held {
        held();
    }
}

/// Leave the form without writing anything (#1071 §5's **Cancel**).
///
/// Everything the form changed lives in its own `Draft`, which is dropped with
/// the page — there is no write to undo, which is what makes "Cancel reverts"
/// true by construction rather than by remembering to roll something back.
fn cancel() {
    close();
    crate::modal::switch_active(crate::modal::Page::Workspaces);
}

/// The message a refused Save puts on the name field.
fn refusal(why: &SaveError) -> String {
    match why {
        SaveError::InvalidName { suggestion, .. } => {
            let rule = "Lowercase letters, digits and single dashes — no leading, trailing or \
                        doubled dash, at most 32 characters.";
            match suggestion {
                Some(suggestion) => format!("{rule}\n\nTry \u{201c}{suggestion}\u{201d}."),
                None => rule.to_owned(),
            }
        }
        SaveError::RenameWhileActive => RENAME_BLOCKED_HINT.to_owned(),
    }
}

/// Perform a planned Save.
///
/// Two routes, because they are two different transactions:
///
/// * **an ephemeral card** goes through `workspace_stacks::save`, which verifies
///   the name is free, writes, and then `SetWorkspaceName`s the workspace in one
///   batch — §3.7's *"the batch names the niri workspace immediately, so the
///   saved workspace is the Active card"*. Writing the file alone would leave
///   two cards: the still-unnamed ephemeral one and a new Inactive one whose ▶
///   would launch a second copy of everything.
/// * **a saved card** is a file write and nothing else. Its workspace either
///   already carries the name or does not exist, and a rename — the only case
///   where niri would have something to do — is refused while Active.
fn commit(plan: &SavePlan) {
    match plan.name_workspace {
        Some(workspace) => {
            workspace_stacks::spawn_save(workspace, plan.name.clone(), plan.stack.clone())
        }
        None => workspace_stacks::spawn_save_edit(
            plan.previous.clone(),
            plan.name.clone(),
            plan.stack.clone(),
        ),
    }
}

/// One app of the stack: icon, name, launch command, remove, drag handle.
fn app_row(
    index: usize,
    app: &StackApp,
    meta_cache: &MetaCache,
    draft: &Rc<RefCell<Draft>>,
    redraw: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::new();
    row.add_css_class(APP_ROW_CLASS);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    body.set_margin_top(6);
    body.set_margin_bottom(6);
    body.set_margin_start(8);
    body.set_margin_end(8);

    // §5's drag handle. It is the drag *source*; the row is the drop target, so
    // the whole row is a landing area while only the handle starts a drag —
    // otherwise every attempt to select text in the launch-command field would
    // pick the row up instead.
    let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
    handle.add_css_class("ts-ws-edit-handle");
    handle.set_tooltip_text(Some(
        "Drag to reorder — this is the order the columns open in.",
    ));
    handle.add_controller(row_drag_source(index));
    body.append(&handle);

    // Resolve into a local first (#643/#663/#832 — an argument-position
    // `borrow_mut()` lives for the whole statement).
    let meta = resolve_app_meta(&app.id, &mut meta_cache.borrow_mut());
    let (icon, display) = meta.map_or_else(
        || (fallback_icon(), app.id.clone()),
        |m| (m.icon.unwrap_or_else(fallback_icon), m.display_name),
    );
    let image = gtk::Image::from_gicon(&icon);
    image.set_icon_size(gtk::IconSize::Normal);
    body.append(&image);

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);

    // Plain `gtk::Label`: a display name and an app-id both come from outside
    // this shell, and `use-markup` is off on a plain label (#30/#753).
    let title = gtk::Label::new(Some(&display));
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::End);
    labels.append(&title);

    let exec = gtk::Entry::builder()
        .text(app.exec.as_deref().unwrap_or_default())
        .placeholder_text(EXEC_PLACEHOLDER)
        .hexpand(true)
        .build();
    exec.add_css_class(EXEC_ENTRY_CLASS);
    {
        let draft = Rc::clone(draft);
        exec.connect_changed(move |entry| {
            let typed = entry.text().to_string();
            if let Some(app) = draft.borrow_mut().apps.get_mut(index) {
                app.exec = (!typed.trim().is_empty()).then_some(typed);
            }
        });
    }
    labels.append(&exec);
    body.append(&labels);

    let remove = gtk::Button::from_icon_name("list-remove-symbolic");
    remove.add_css_class("flat");
    remove.add_css_class("ts-ws-edit-remove");
    remove.set_valign(gtk::Align::Center);
    remove.set_tooltip_text(Some("Remove this app from the stack"));
    {
        let draft = Rc::clone(draft);
        let redraw = Rc::clone(redraw);
        remove.connect_clicked(move |_| {
            {
                let mut draft = draft.borrow_mut();
                if index < draft.apps.len() {
                    draft.apps.remove(index);
                }
            }
            fire(&redraw);
        });
    }
    body.append(&remove);

    row.set_child(Some(&body));
    row.add_controller(row_drop_target(index, draft, redraw));
    row
}

/// A row's drag source, carrying its index.
///
/// An index rather than the app-id: a stack may legitimately list one app twice
/// (two terminals), and an id would not say which of them was picked up.
fn row_drag_source(index: usize) -> gtk::DragSource {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);
    let payload = i64::try_from(index).unwrap_or(-1);
    source.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&payload.to_value())));
    source
}

/// A row's drop target: dropping row `from` on row `index` moves it there.
fn row_drop_target(
    index: usize,
    draft: &Rc<RefCell<Draft>>,
    redraw: &Rc<RefCell<Option<Rc<dyn Fn()>>>>,
) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(glib::types::Type::I64, gdk::DragAction::MOVE);
    let draft = Rc::clone(draft);
    let redraw = Rc::clone(redraw);
    target.connect_drop(move |_, value, _, _| {
        let Ok(from) = value.get::<i64>() else {
            return false;
        };
        let Ok(from) = usize::try_from(from) else {
            return false;
        };
        // Everything this decides is `move_app`, which is pure and tested; a
        // GTK drop callback cannot be invoked from a test, so any branch left
        // in here is a branch nothing can falsify (#1106 review F5).
        {
            let mut draft = draft.borrow_mut();
            let moved = move_app(&draft.apps, from, index);
            if moved == draft.apps {
                return false;
            }
            draft.apps = moved;
        }
        fire(&redraw);
        true
    });
    target
}

/// A label above a control, the shape the rest of the drawer's forms use.
fn labelled(text: &str, control: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 4);
    row.set_margin_top(6);
    row.append(&section_label(text));
    row.append(control);
    row.upcast()
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("dim-label");
    label.set_xalign(0.0);
    label
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::{
        APP_ROW_CLASS, Draft, EXEC_ENTRY_CLASS, NAME_ENTRY_CLASS, NOTHING_HINT,
        RENAME_BLOCKED_HINT, build_slot,
    };
    use crate::config::workspaces::{Layout, StackApp};
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, prelude::*};

    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    fn by_class(root: &impl IsA<gtk::Widget>, class: &str) -> Vec<gtk::Widget> {
        fn walk(widget: &gtk::Widget, class: &str, out: &mut Vec<gtk::Widget>) {
            if widget.has_css_class(class) {
                out.push(widget.clone());
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                walk(&c, class, out);
                child = c.next_sibling();
            }
        }
        let mut out = Vec::new();
        walk(root.upcast_ref(), class, &mut out);
        out
    }

    fn entries(root: &gtk::Widget, class: &str) -> Vec<gtk::Entry> {
        by_class(root, class)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Entry>().ok())
            .collect()
    }

    fn name_field(root: &gtk::Widget) -> gtk::Entry {
        entries(root, NAME_ENTRY_CLASS)
            .into_iter()
            .next()
            .expect("the form has a name field")
    }

    fn app_rows(root: &gtk::Widget) -> Vec<gtk::Widget> {
        by_class(root, APP_ROW_CLASS)
    }

    /// The launch command shown on each app row, in row order.
    fn exec_texts(root: &gtk::Widget) -> Vec<String> {
        entries(root, EXEC_ENTRY_CLASS)
            .into_iter()
            .map(|e| e.text().to_string())
            .collect()
    }

    fn label_texts(root: &gtk::Widget, class: &str) -> Vec<String> {
        by_class(root, class)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Label>().ok())
            .map(|l| l.text().to_string())
            .collect()
    }

    fn app(id: &str, exec: Option<&str>) -> StackApp {
        StackApp {
            id: id.to_owned(),
            exec: exec.map(str::to_owned),
        }
    }

    fn saved_draft() -> Draft {
        Draft {
            previous: Some("chat".to_owned()),
            name: "chat".to_owned(),
            apps: vec![
                app("org.mozilla.firefox", None),
                app("Alacritty", Some("alacritty -e weechat")),
            ],
            layout: Layout::Golden,
            autostart: true,
            monitor: Some("DP-1".to_owned()),
            workspace: Some(3),
            active: true,
        }
    }

    fn ephemeral_draft() -> Draft {
        Draft {
            previous: None,
            name: String::new(),
            apps: vec![app("weird-app", Some("/home/me/bin/weird --flag"))],
            monitor: Some("HDMI-A-1".to_owned()),
            workspace: Some(7),
            ..Draft::default()
        }
    }

    /// The slot with a selection it can be driven from — the seam `edit_slot`
    /// wraps around the thread-local.
    fn slot(target: &Mutable<Option<Draft>>) -> gtk::Widget {
        adw::init().expect("libadwaita init");
        let page = build_slot(target.signal_cloned());
        pump();
        page
    }

    /// §5: the sub-page opens **from a saved card**, showing every field of the
    /// stack — name, the apps with their launch commands, layout, autostart.
    #[gtk::test]
    fn the_edit_page_opens_from_a_saved_card() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        assert!(
            label_texts(&page, "ts-ws-empty").contains(&NOTHING_HINT.to_owned()),
            "with nothing selected the page says so"
        );

        target.set(Some(saved_draft()));
        pump();

        assert_eq!(name_field(&page).text(), "chat");
        assert_eq!(app_rows(&page).len(), 2, "one row per app of the stack");
        assert_eq!(
            exec_texts(&page),
            ["", "alacritty -e weechat"],
            "each row shows its own launch command, and an app with none shows none"
        );

        let layout = by_class(&page, "ts-ws-edit-layout")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::DropDown>().ok())
            .expect("the layout dropdown");
        assert_eq!(
            usize::try_from(layout.selected()).expect("a selection"),
            super::LAYOUTS
                .iter()
                .position(|l| *l == Layout::Golden)
                .expect("golden is offered"),
            "the dropdown opens on the stack's own layout"
        );

        let autostart = by_class(&page, "ts-ws-edit-autostart")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Switch>().ok())
            .expect("the autostart switch");
        assert!(autostart.is_active(), "the switch opens on the stack's value");

        // An Active stack says why its name cannot change (§5's rename refusal).
        assert!(
            label_texts(&page, "ts-ws-empty").contains(&RENAME_BLOCKED_HINT.to_owned()),
            "an Active stack must say why a rename is refused before it is tried"
        );
    }

    /// §3.7: the same page opens **from an ephemeral card**, with an empty name
    /// and the running command line of the app that has no desktop entry —
    /// *"shown for correction in Edit"*.
    #[gtk::test]
    fn the_edit_page_opens_from_an_ephemeral_card() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(ephemeral_draft()));
        pump();

        assert_eq!(
            name_field(&page).text(),
            "",
            "an ephemeral card has no name until Save gives it one"
        );
        assert_eq!(app_rows(&page).len(), 1);
        assert_eq!(
            exec_texts(&page),
            ["/home/me/bin/weird --flag"],
            "§3.7's command line must be in the field, editable"
        );
        // No rename refusal: there is no name to rename *from*.
        assert!(
            !label_texts(&page, "ts-ws-empty").contains(&RENAME_BLOCKED_HINT.to_owned()),
            "an ephemeral card was told it cannot be renamed"
        );
    }

    /// The page **survives the refresh poll** (the brief).
    ///
    /// `workspaces.toml` is live-reloaded and niri's event stream re-fires for
    /// the life of the session. The form must not be torn down under the user's
    /// cursor when an equal selection is republished — a half-typed name and a
    /// half-edited launch command have to still be there.
    ///
    /// **The mutation**: dropping `dedupe_cloned` from `build_slot` reds this —
    /// the re-set rebuilds the form and both edits are gone.
    #[gtk::test]
    fn the_edit_page_survives_a_refresh_that_changes_nothing() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();

        // The user starts typing.
        name_field(&page).set_text("chat-2");
        let exec = entries(&page, EXEC_ENTRY_CLASS)
            .into_iter()
            .next()
            .expect("a launch-command field");
        exec.set_text("firefox --private-window");
        pump();

        // …and the poll republishes the very same selection, twice.
        target.set(Some(saved_draft()));
        pump();
        target.set(Some(saved_draft()));
        pump();

        assert_eq!(
            name_field(&page).text(),
            "chat-2",
            "the refresh poll threw away a half-typed name"
        );
        assert_eq!(
            entries(&page, EXEC_ENTRY_CLASS)
                .into_iter()
                .next()
                .expect("a launch-command field")
                .text(),
            "firefox --private-window",
            "the refresh poll threw away a half-edited launch command"
        );
    }

    /// Selecting a **different** card does rebuild the form — the dedupe is
    /// about equal values, not about never rebuilding.
    ///
    /// Without this the test above would pass on a binding that ignored its
    /// signal entirely.
    #[gtk::test]
    fn selecting_another_card_rebuilds_the_form() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();
        assert_eq!(app_rows(&page).len(), 2);

        target.set(Some(ephemeral_draft()));
        pump();
        assert_eq!(name_field(&page).text(), "");
        assert_eq!(app_rows(&page).len(), 1);

        // …and clearing it returns to the empty state.
        target.set(None);
        pump();
        assert!(
            label_texts(&page, "ts-ws-empty").contains(&NOTHING_HINT.to_owned()),
            "clearing the selection left the form up"
        );
    }

    /// A row's **remove** button drops that app and only that app, and the list
    /// redraws — the indices the remaining rows carry have to be re-issued or
    /// the next remove takes the wrong one.
    #[gtk::test]
    fn removing_an_app_drops_that_row_and_renumbers_the_rest() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(Draft {
            apps: vec![
                app("a", Some("cmd-a")),
                app("b", Some("cmd-b")),
                app("c", Some("cmd-c")),
            ],
            ..saved_draft()
        }));
        pump();
        assert_eq!(exec_texts(&page), ["cmd-a", "cmd-b", "cmd-c"]);

        let remove = by_class(&page, "ts-ws-edit-remove")
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .nth(1)
            .expect("the second row's remove button");
        remove.emit_clicked();
        pump();
        assert_eq!(exec_texts(&page), ["cmd-a", "cmd-c"], "the wrong row went");

        // Remove the (now) second row: if the indices had not been re-issued
        // this would take `cmd-a`, or nothing at all.
        let remove = by_class(&page, "ts-ws-edit-remove")
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .nth(1)
            .expect("the second row's remove button");
        remove.emit_clicked();
        pump();
        assert_eq!(exec_texts(&page), ["cmd-a"]);
    }

    /// Every app row carries both halves of §5's drag: a source on the handle
    /// and a target on the row.
    #[gtk::test]
    fn every_app_row_can_be_dragged_and_dropped_on() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();

        fn controllers(widget: &gtk::Widget) -> Vec<gtk::EventController> {
            let list = widget.observe_controllers();
            (0..list.n_items())
                .filter_map(|i| list.item(i)?.downcast::<gtk::EventController>().ok())
                .collect()
        }

        for row in app_rows(&page) {
            assert!(
                controllers(&row).iter().any(ObjectExt::is::<gtk::DropTarget>),
                "an app row must take a drop, or the order cannot be changed"
            );
        }
        let handles = by_class(&page, "ts-ws-edit-handle");
        assert_eq!(handles.len(), 2, "one drag handle per app row");
        for handle in handles {
            assert!(
                controllers(&handle)
                    .iter()
                    .any(ObjectExt::is::<gtk::DragSource>),
                "the handle is what starts the drag — the row is not, or \
                 selecting text in the launch-command field would pick it up"
            );
        }
    }
}

#[cfg(test)]
mod model_tests {
    use super::{Draft, SaveError, SavePlan, ephemeral_apps, move_app, plan_save};
    use crate::config::workspaces::{Layout, Stack, StackApp};
    use std::collections::BTreeSet;

    fn app(id: &str, exec: Option<&str>) -> StackApp {
        StackApp {
            id: id.to_owned(),
            exec: exec.map(str::to_owned),
        }
    }

    fn draft(name: &str, previous: Option<&str>) -> Draft {
        Draft {
            previous: previous.map(str::to_owned),
            name: name.to_owned(),
            apps: vec![app("firefox", None)],
            ..Draft::default()
        }
    }

    fn ids(apps: &[StackApp]) -> Vec<&str> {
        apps.iter().map(|a| a.id.as_str()).collect()
    }

    /// #1071 §3.1, through §5's Name field: *"the Save entry refuses anything
    /// else and offers the sanitised form"*.
    ///
    /// **The mutation**: accepting a doubled dash reds this — `chat--dev` is a
    /// systemd unit systemd refuses (measured, #1101), so the stack's apps would
    /// launch into nothing.
    #[test]
    fn an_invalid_name_is_refused_with_the_sanitised_form_offered() {
        for (typed, suggestion) in [
            ("chat--dev", Some("chat-dev")),
            ("-chat", Some("chat")),
            ("chat-", Some("chat")),
            ("Chat Room!", Some("chat-room")),
            ("chat dev", Some("chat-dev")),
            ("", None),
            ("---", None),
        ] {
            let why = plan_save(&draft(typed, None)).expect_err("refused");
            assert_eq!(
                why,
                SaveError::InvalidName {
                    typed: typed.to_owned(),
                    suggestion: suggestion.map(str::to_owned),
                },
                "{typed:?} was not refused with the expected suggestion"
            );
        }
    }

    /// §3.1: uppercase is **folded**, not refused — niri matches workspace names
    /// case-insensitively, so `Chat` and `chat` are one name.
    #[test]
    fn an_uppercase_name_is_folded_rather_than_refused() {
        let plan = plan_save(&draft("Chat", None)).expect("accepted");
        assert_eq!(plan.name, "chat");
        // …and a "rename" that only changes case is not a rename, so it is not
        // refused even while Active.
        let active = Draft {
            active: true,
            ..draft("CHAT", Some("chat"))
        };
        assert_eq!(plan_save(&active).expect("accepted").name, "chat");
    }

    /// A rename while the stack is running is refused: its apps live in
    /// `trollshell-ws-<old>.slice` and its units are named after it, so renaming
    /// the entry alone would leave them unstoppable from this page.
    #[test]
    fn a_rename_is_refused_while_the_stack_is_active() {
        let active = Draft {
            active: true,
            ..draft("dev", Some("chat"))
        };
        assert_eq!(
            plan_save(&active).expect_err("refused"),
            SaveError::RenameWhileActive
        );
        // Inactive, the same rename goes through and says what it is replacing.
        let inactive = Draft {
            active: false,
            ..draft("dev", Some("chat"))
        };
        let plan = plan_save(&inactive).expect("accepted");
        assert_eq!(plan.previous.as_deref(), Some("chat"));
        assert_eq!(plan.name, "dev");
    }

    /// §3.7: an ephemeral card's Save carries the workspace to **name**; a saved
    /// card's does not, because niri has nothing to do for it.
    #[test]
    fn only_an_ephemeral_save_names_a_workspace() {
        let ephemeral = Draft {
            workspace: Some(7),
            ..draft("chat", None)
        };
        assert_eq!(plan_save(&ephemeral).expect("accepted").name_workspace, Some(7));

        // A saved, Active stack has a live workspace too — and it already
        // carries the name, so a Save must not re-name it.
        let saved = Draft {
            workspace: Some(7),
            active: true,
            ..draft("chat", Some("chat"))
        };
        assert_eq!(plan_save(&saved).expect("accepted").name_workspace, None);
    }

    /// The form's other fields reach the file, and a blank launch command is
    /// `None` rather than `Some("")` — which the reader refuses at load.
    #[test]
    fn the_form_becomes_the_stack_it_describes() {
        let full = Draft {
            previous: Some("chat".to_owned()),
            name: "chat".to_owned(),
            apps: vec![
                app("firefox", None),
                app("Alacritty", Some("alacritty -e weechat")),
                app("code", Some("   ")),
            ],
            layout: Layout::Golden,
            autostart: true,
            monitor: Some("DP-1".to_owned()),
            workspace: Some(3),
            active: true,
        };
        let SavePlan { stack, .. } = plan_save(&full).expect("accepted");
        assert_eq!(
            stack,
            Stack {
                monitor: Some("DP-1".to_owned()),
                autostart: true,
                layout: Layout::Golden,
                apps: vec![
                    app("firefox", None),
                    app("Alacritty", Some("alacritty -e weechat")),
                    app("code", None),
                ],
            },
            "a whitespace-only launch command was written as an empty string"
        );
    }

    /// §5's drag handles: the app order **is** niri's column order (§3.4 step 3),
    /// so a drop rewrites `apps` in the dropped order.
    ///
    /// **The mutation**: a rebuild that collects "before" and "after" rather than
    /// removing and reinserting reorders the untouched entries whenever the drag
    /// straddles them — the third case below reds it.
    #[test]
    fn a_drag_moves_one_app_and_leaves_every_other_in_place() {
        let apps = vec![
            app("a", None),
            app("b", None),
            app("c", None),
            app("d", None),
        ];
        // Down: the third to the front.
        assert_eq!(ids(&move_app(&apps, 2, 0)), ["c", "a", "b", "d"]);
        // Up: the first to the end.
        assert_eq!(ids(&move_app(&apps, 0, 3)), ["b", "c", "d", "a"]);
        // Straddling: everything between shifts by one, and nothing else moves.
        assert_eq!(ids(&move_app(&apps, 1, 2)), ["a", "c", "b", "d"]);
    }

    /// Out-of-range and no-op drops leave the list alone rather than panicking —
    /// a drop's indices come from GTK and the list can rebuild mid-drag.
    #[test]
    fn an_impossible_drag_changes_nothing() {
        let apps = vec![app("a", None), app("b", None)];
        assert_eq!(ids(&move_app(&apps, 0, 0)), ["a", "b"]);
        assert_eq!(ids(&move_app(&apps, 5, 0)), ["a", "b"]);
        assert_eq!(ids(&move_app(&apps, 0, 5)), ["a", "b"]);
        assert!(move_app(&[], 0, 0).is_empty());
    }

    /// §3.7: *"an `app_id` with no entry becomes an app with `exec` = the
    /// process's command line"* — and one **with** an entry does not, because
    /// §3.2 resolves its `Exec` at Start time.
    ///
    /// **The mutation**: giving every app the command line reds the first
    /// assertion; giving none of them one reds the second.
    #[test]
    fn an_ephemeral_saves_unknown_app_carries_its_command_line() {
        let known: BTreeSet<String> = ["org.mozilla.firefox".to_owned()].into_iter().collect();
        let windows = vec![
            (
                "org.mozilla.firefox".to_owned(),
                Some("/nix/store/x/bin/firefox".to_owned()),
            ),
            (
                "weird-app".to_owned(),
                Some("/home/me/bin/weird --flag".to_owned()),
            ),
        ];
        assert_eq!(
            ephemeral_apps(&windows, &known),
            vec![
                app("org.mozilla.firefox", None),
                app("weird-app", Some("/home/me/bin/weird --flag")),
            ]
        );
    }

    /// Column order is preserved and two windows of one app are one entry — the
    /// card's row already shows them as one icon, and the stack is a list of
    /// apps, not of windows.
    #[test]
    fn an_ephemeral_saves_apps_are_deduped_in_column_order() {
        let known = BTreeSet::new();
        let windows = vec![
            ("b".to_owned(), Some("b-cmd".to_owned())),
            ("a".to_owned(), Some("a-cmd".to_owned())),
            ("b".to_owned(), Some("b-cmd-2".to_owned())),
        ];
        let apps = ephemeral_apps(&windows, &known);
        assert_eq!(ids(&apps), ["b", "a"], "the column order was sorted away");
        assert_eq!(
            apps[0].exec.as_deref(),
            Some("b-cmd"),
            "the second window of an app overwrote the first's command line"
        );
    }

    /// An unknown app whose command line could not be read (the process is
    /// gone, `/proc` is not readable) gets no `exec` rather than an empty one.
    #[test]
    fn an_unreadable_command_line_leaves_the_app_bare() {
        let known = BTreeSet::new();
        let windows = vec![
            ("gone".to_owned(), None),
            ("blank".to_owned(), Some("   ".to_owned())),
        ];
        let apps = ephemeral_apps(&windows, &known);
        assert!(apps.iter().all(|a| a.exec.is_none()));
    }

    /// The `Active::WorkspaceEdit` key: a saved stack by name, an ephemeral card
    /// by workspace id behind a `#` — which `is_valid_workspace_name` refuses,
    /// so the two keyspaces cannot collide.
    #[test]
    fn the_edit_key_separates_a_saved_card_from_an_ephemeral_one() {
        assert_eq!(draft("chat", Some("chat")).key(), "chat");
        let ephemeral = Draft {
            workspace: Some(7),
            ..draft("", None)
        };
        assert_eq!(ephemeral.key(), "#7");
        assert!(
            !hytte::services::systemd::is_valid_workspace_name("#7"),
            "'#' must not be a legal workspace name, or the two keyspaces collide"
        );
    }
}
