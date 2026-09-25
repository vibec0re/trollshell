//! The Edit sub-page — #1071 §5, phase 4; laid out in two columns and given a
//! per-row launch-command override toggle by #1134.
//!
//! Same drawer, content replaced, in **two columns** (#1134 change 1): a
//! narrower left column for the name, the default layout and the autostart
//! switch, and a right column — most of the form's width — for the stack's
//! apps as a list, each with a drag handle and, behind a per-row "override the
//! launch command" toggle, an editable command (#1134 change 2; see
//! `resolved_command` for what the toggle pre-fills). **Add app** goes
//! through the desktop-entry picker. **No monitor field** — a card's screen is
//! set by dragging it between the page's columns (Annika, on the epic
//! thread), and phase 3 built that.
//!
//! ## How wide the form is
//!
//! `crate::components::layout::EDIT_FORM_WIDTH` (960), always — the same number
//! `modal::apply_workspace_edit_width_cap` floors the drawer slot at, so the two
//! halves of the sub-page agree.
//!
//! It used to be the width of the page behind it: #1108 tied them together so the
//! drawer would not jump the moment ✎ is pressed. #1219 then made that page as
//! narrow as 680 on a one-screen box, and 680 less the 240-px fields column is the
//! form Annika filed #1220 about — her screenshot is 418 px of form with an apps
//! column around 150. So the form takes its own width and the jump is accepted
//! (Annika, #1219, 2026-09-13: *"Slight jump in edit form is ok."*). The two
//! columns are therefore **always** side by side, as #1134 built them; the app
//! list gets ~700 px at every screen count.
//!
//! ## Why this is not a `Page` variant
//!
//! `Page` is `Copy` with unit variants only, passed by value at ~60 sites
//! (`modal.rs`), so a sub-page keyed by *which card* cannot be one. It rides the
//! shape the plugin panel already uses — [`crate::modal`]'s `Active::Plugin`
//! becoming `Active::WorkspaceEdit` — a drawer child keyed by a string, outside
//! the `Page` enum. §5 fixes exactly that, and phase 1's PR body promised it.
//!
//! ## Why the form takes a whole `Draft` rather than reading the world
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
//! So `open` publishes a `Draft` and the form is a pure function of it. The
//! consequence to know about: with drawers open on two monitors at once, both
//! show a form seeded from the same `Draft` and each holds its own edits — the
//! same shape the plugin panel's single global selection already has.
//!
//! ## What is pure here
//!
//! `plan_save`, `move_app` and `ephemeral_apps` are the three decisions,
//! and none of them touches GTK, the filesystem or niri. That is what makes
//! §7's phase-4 rows falsifiable: the validator's refusal, the drag's rewrite
//! and §3.7's "an `app_id` with no entry becomes an app with `exec` = the
//! process's command line" are each a function of a snapshot.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::{LocalBoxSignal, Mutable, Signal, SignalExt, always};
use hytte::gtk::{self, gdk, glib, pango, prelude::*};
use hytte::prelude::*;
use hytte::services::niri::{self, Window, Workspace};
use hytte::services::systemd;

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::app_picker::add_app_button;
use crate::components::desktop_entry::{self, Launchable};
use crate::components::layout::{
    EDIT_FORM_WIDTH, finish_page_clamped, page_box, page_grid_non_homogeneous,
};
use crate::config::workspaces::{Layout, Stack, StackApp};
use crate::workspace_stacks::{self, StackState};

/// A handle to "rebuild the app list", shared with the rows it builds.
///
/// The indirection is what lets a row's own **remove** button ask for the list
/// it is a child of to be rebuilt: a row cannot reach its parent's builder from
/// inside its own callback, and the builder cannot capture rows that do not
/// exist yet. The `Option` is filled in once, immediately after the closure is
/// made — it is only ever `None` for the length of that one statement.
///
/// Rebuilding rather than patching, because every row carries its **index** and
/// a removal renumbers everything after it. A patch would leave stale indices,
/// and a stale index removes the wrong app on the next press.
type Redraw = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// CSS class on one app row, and the hook a test counts rows by.
const APP_ROW_CLASS: &str = "ts-ws-edit-app";

/// CSS class on an app row's launch-command entry.
const EXEC_ENTRY_CLASS: &str = "ts-ws-edit-exec";

/// CSS class on the name field.
const NAME_ENTRY_CLASS: &str = "ts-ws-edit-name";

/// CSS class on the page root, so a test can tell an edit page from the card
/// page inside the same drawer stack.
const EDIT_PAGE_CLASS: &str = "ts-ws-edit";

/// CSS class on the two-column grid the form's fields sit in (#1134 change 1):
/// name/layout/autostart in a narrower left column, the app list in a right
/// column that claims most of the width.
const FORM_GRID_CLASS: &str = "ts-ws-edit-grid";

/// CSS class on a row's "override the launch command" toggle (#1134 change
/// 2). Off, a row shows only its icon and name; on, it reveals
/// [`EXEC_ENTRY_CLASS`] pre-filled with the resolved command.
const OVERRIDE_TOGGLE_CLASS: &str = "ts-ws-edit-override";

/// Design-baseline width, in CSS px before [`crate::scale::scale`], of the
/// left column holding name/layout/autostart — narrow on purpose, since #1134
/// change 1 gives the app list "most of the width".
const FIELD_COLUMN_WIDTH: i32 = 240;

/// Width cap, in characters, on the wrapping labels in the fields column —
/// [`RENAME_BLOCKED_HINT`] is the only one today (#1220).
///
/// A `gtk::Grid` brings every column up to its **natural** width before handing
/// what's left to the expanding one, and a wrapping `gtk::Label`'s natural width
/// is its whole *unwrapped* line. `RENAME_BLOCKED_HINT` is ~100 characters, so
/// with `wrap(true)` and no cap the fields column asked for ~600–700 px and ate
/// the width #1134 change 1 gave the app list — measured at the floored form,
/// 646 px of fields against 324 of list.
///
/// It is **not** why #1220 was filed: Annika's screenshot shows a **stopped**
/// stack, with no note on the form, and a cramped list anyway (that was the
/// missing width floor on the ✎ route — see
/// `modal::apply_workspace_edit_width_cap`). This cap is still right, and still
/// load-bearing once the floor exists: without it the note balloons the fields
/// column the moment you edit a running stack. Both cases are asserted in
/// `tests::the_app_list_keeps_most_of_the_width_stopped_or_running`.
///
/// 28 characters is about [`FIELD_COLUMN_WIDTH`] at the form's font, so the
/// column's natural width comes out at its own 240-px request rather than above
/// it.
///
/// Deliberately **one** mechanism rather than two: `set_natural_wrap_mode(Word)`
/// would also cap the natural width (at the longest word), but with both in place
/// deleting either one leaves the column narrow and the regression test green —
/// and the width it caps at would then be an accident of the sentence's longest
/// word rather than a width we chose to match the column.
const NOTE_MAX_WIDTH_CHARS: i32 = 28;

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

/// Design-baseline height cap for the whole form, in CSS px, before
/// [`crate::scale::scale`]. Sized like `panels::workspaces`' column scroller:
/// tall enough that a realistic stack never scrolls, short enough that the
/// drawer can lay the page out rather than being told to be as tall as its
/// content — which is what put Save and Cancel off the bottom (review MEDIUM 5).
const FORM_MAX_HEIGHT: i32 = 560;

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
    /// A rename is refused right now — see [`rename_is_blocked`], which is what
    /// fills this in, and [`plan_save`], which enforces it.
    pub active: bool,
    /// Every **other** stack name in the merged file when the form opened.
    ///
    /// Carried on the draft rather than read at Save time for the module doc's
    /// reason — the form does not reach for the world — and for a practical one:
    /// `config::workspaces::current()` needs a registered `Registry`, which a
    /// `#[gtk::test]` driving this page does not have.
    ///
    /// The authoritative check is still the writer's, against the merged layers.
    /// This is the *fast* one, so a taken name is refused beside the cursor
    /// instead of arriving as a toast after the drawer has gone (review
    /// MEDIUM 8).
    pub taken: BTreeSet<String>,
}

/// Whether a rename must be refused for a card in this state (review MEDIUM 2).
///
/// `Starting` counts, and it is the window in which it matters **most**: the
/// Edit button is not disabled while a Start is in flight (only start/stop is),
/// so the obvious gesture — press ▶, then ✎ — lands here. The apps are at that
/// moment being launched into `trollshell-ws-<old>.slice` under units named
/// after it, while the entry would become `<new>`: exactly the "unstoppable from
/// this page" state [`SaveError::RenameWhileActive`] exists to prevent, and the
/// grace window is up to ten seconds wide.
///
/// Pure, and separate from `draft_for`, so all three states can be asserted —
/// the mapping used to be an inline `== StackState::Active` that no test could
/// reach.
#[must_use]
pub(crate) fn rename_is_blocked(state: StackState) -> bool {
    match state {
        StackState::Active | StackState::Starting => true,
        StackState::Inactive => false,
    }
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
    InvalidName {
        typed: String,
        suggestion: Option<String>,
    },
    /// Another stack already has this name (review MEDIUM 8).
    ///
    /// Phase 2's inline field checked this beside the cursor and #1109 deleted
    /// the field; without it the refusal came from the writer, long after the
    /// drawer had gone back to the cards and dropped the draft. The writer still
    /// checks — against the merged layers, which is authoritative — but the user
    /// should not have to retype an app list to find out.
    NameTaken { name: String },
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
    // Only when the name is actually changing: re-saving a stack under its own
    // name is the ordinary case and must not trip over itself. Compared the way
    // niri and the validator compare names — case-insensitively — since `name`
    // is already folded and `taken` holds names from the file.
    if draft
        .previous
        .as_deref()
        .is_none_or(|previous| !previous.eq_ignore_ascii_case(&name))
        && draft
            .taken
            .iter()
            .any(|other| other.eq_ignore_ascii_case(&name))
    {
        return Err(SaveError::NameTaken { name });
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
        name_workspace: draft
            .previous
            .is_none()
            .then_some(draft.workspace)
            .flatten(),
        previous: draft.previous.clone(),
        name,
        stack,
    })
}

/// The typed text as the nearest usable workspace name, or `None` when there is
/// nothing left to suggest.
///
/// Not shared with anything else: #1109 retired the inline name field on an
/// ephemeral card that used to carry its own copy of this rule, so this is now
/// the only field it serves. If a second field ever needs it, that is the
/// moment to lift it into `components/`.
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
/// **Not deduped** (#1133): one entry per window, in column order — two
/// Alacritty windows are two `StackApp` entries, each independently editable
/// and each carrying its own window's command line, because #1071 §3.2's
/// stack is an ordered list of apps in which the same `app_id` may appear as
/// often as it has windows. (Before #1133 this deduped by app-id, first
/// occurrence wins, on the premise that the card's row showed one icon per
/// app-id; that premise is what #1133 changes — see
/// `panels::workspaces::StackApp`.)
#[must_use]
pub(crate) fn ephemeral_apps(
    windows: &[(String, Option<String>)],
    known: &BTreeSet<String>,
) -> Vec<StackApp> {
    windows
        .iter()
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

/// What an app row's "override the launch command" toggle asks, given the
/// app's id: the command to pre-fill the entry with (#1134 change 2).
///
/// Passed in rather than called from inside [`app_row`], so a `#[gtk::test]`
/// can drive the toggle without touching the machine's real `$XDG_DATA_DIRS`
/// — the same rule #1113's tests kept for every other `Exec` fact.
type Resolver = Rc<dyn Fn(&str) -> String>;

/// The command an override toggle reveals the **first** time it is switched
/// on for a row with no saved override — the same resolution
/// [`crate::workspace_stacks::app_start`] runs at Start time, so *"an override
/// starts from the real command"* rather than a blank field.
///
/// `entry` is `None` when the id names no installed desktop entry: there is
/// nothing to prefill, so the field opens empty and the user types their own
/// command. Pure, so this is the half of the resolver a test drives directly;
/// [`live_resolver`] is the impure half that actually reads a desktop file.
#[must_use]
pub(crate) fn resolved_command(entry: Option<&Launchable>) -> String {
    entry.map_or_else(String::new, |entry| {
        desktop_entry::strip_field_codes(&desktop_entry::exec_words(&entry.exec)).join(" ")
    })
}

/// [`resolved_command`] wired to the real desktop-entry lookup — what every
/// row outside a test resolves with.
fn live_resolver() -> Resolver {
    Rc::new(|id: &str| resolved_command(desktop_entry::launchable(id).as_ref()))
}

/// How an editor row derives its live [`RowVisual`], given the stack's name
/// (`None` for an ephemeral card) and — when the app is in the **saved**
/// stack — its apps there plus the app's own index among them.
///
/// Passed in rather than called from inside [`app_row`], for the same reason
/// [`Resolver`] is: [`live_row_visual`] reaches `niri::workspaces()` and
/// `niri::windows()`, which are `Registry` accessors that panic without one
/// registered — and this page's own `#[gtk::test]`s build a form with no
/// `Registry` at all (module doc, "The form must survive the refresh poll").
///
/// `saved` rather than the draft's own apps/index (#1312 review MED-4): the
/// draft can be reordered, added to or trimmed without a Save, and the row's
/// *launch* has to agree with what is actually in `workspaces.toml` — see
/// [`saved_index_of`].
type RowVisualSource =
    Rc<dyn Fn(Option<String>, Option<(Vec<StackApp>, usize)>) -> LocalRowVisualSignal>;

/// [`RowVisualSource`]'s return type, spelled once.
type LocalRowVisualSignal = LocalBoxSignal<'static, RowVisual>;

/// How `name` is currently on a workspace, and this app's window on it, given
/// already-resolved live signals — the part of [`RowVisualSource`] that does
/// not care whether those signals are the real `niri`/`workspace_stacks`
/// globals or scripted `Mutable`s (#1312 review LOW 7: split out so a test can
/// drive `build_app_icon`/`app_row` through this seam end to end, the same
/// split `panels::workspaces`' `build_panel`/`model` already uses).
fn row_visual_signal<W, N, U, T, A>(
    stack_name: Option<String>,
    saved: Option<(Vec<StackApp>, usize)>,
    workspaces: W,
    windows: N,
    slices_up: U,
    starting: T,
    app_starting: A,
) -> LocalRowVisualSignal
where
    W: Signal<Item = Vec<Workspace>> + 'static,
    N: Signal<Item = Vec<Window>> + 'static,
    U: Signal<Item = BTreeSet<String>> + 'static,
    T: Signal<Item = BTreeSet<String>> + 'static,
    A: Signal<Item = BTreeSet<String>> + 'static,
{
    let Some(name) = stack_name else {
        // An ephemeral card has no stack to start into at all — its apps came
        // *from* the workspace's own windows (`ephemeral_draft`), so it is
        // always Lit and never a click target (#1312 review LOW 6).
        return always(RowVisual::Lit).boxed_local();
    };
    let Some((saved_apps, saved_index)) = saved else {
        // Not (yet) in the saved file — a just-added, unsaved row has no unit
        // to file a start under (#1312 review MED-4).
        return always(RowVisual::Decoration).boxed_local();
    };
    map_ref! {
        let workspaces = workspaces,
        let windows = windows,
        let slices_up = slices_up,
        let starting = starting,
        let app_starting = app_starting => {
            let this_starting = app_starting.contains(&workspace_stacks::app_starting_key(&name, saved_index));
            if this_starting {
                RowVisual::Starting
            } else {
                let state = workspace_stacks::state_of(
                    &name,
                    workspaces,
                    windows,
                    slices_up.contains(&name),
                    starting,
                );
                let workspace = live_workspace_id(workspaces, &name);
                app_row_visual(state, &saved_apps, saved_index, workspace, windows)
            }
        }
    }
    .dedupe()
    .boxed_local()
}

/// [`RowVisualSource`]'s real implementation (#1312): [`row_visual_signal`]
/// fed the real `niri::workspaces()`/`niri::windows()` and
/// `workspace_stacks`' own `slices_up`/`starting`/`app_starting`.
fn live_row_visual() -> RowVisualSource {
    Rc::new(|stack_name, saved| {
        row_visual_signal(
            stack_name,
            saved,
            niri::workspaces(),
            niri::windows(),
            workspace_stacks::slices_up(),
            workspace_stacks::starting(),
            workspace_stacks::app_starting(),
        )
    })
}

/// How an editor row finds the app it is showing in the **saved** stack
/// (#1312 review MED-4), given the stack's name. `None` when the stack itself
/// has not been saved at all (an ephemeral card, or — defensively — a name
/// the file does not have).
///
/// Passed in rather than called from inside [`build_app_icon`], for the same
/// reason [`RowVisualSource`] is: the real one reaches
/// `config::workspaces::current()`, a `Registry` accessor.
type SavedStackSource = Rc<dyn Fn(&str) -> Option<Stack>>;

/// [`SavedStackSource`]'s real implementation.
fn live_saved_stack() -> SavedStackSource {
    Rc::new(|name: &str| {
        crate::config::workspaces::current()
            .stacks
            .get(name)
            .cloned()
    })
}

/// The impure lookups one editor row needs, bundled into one struct rather
/// than three trailing parameters (`app_row` and friends were at
/// `clippy::too_many_arguments` with them separate): the override toggle's
/// [`Resolver`], the row icon's [`RowVisualSource`], and its
/// [`SavedStackSource`]. `Clone` is cheap — every field is an `Rc`.
#[derive(Clone)]
struct RowSeams {
    resolve: Resolver,
    row_visual: RowVisualSource,
    saved_stack: SavedStackSource,
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
    build_slot_with(
        target,
        RowSeams {
            resolve: live_resolver(),
            row_visual: live_row_visual(),
            saved_stack: live_saved_stack(),
        },
    )
}

/// [`build_slot`] with every [`RowSeams`] seam injected, so a `#[gtk::test]`
/// can drive any of them without touching the machine's real
/// `$XDG_DATA_DIRS` or requiring a registered `Registry`.
fn build_slot_with<S>(target: S, seams: RowSeams) -> gtk::Widget
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
        if let Some(draft) = draft {
            root.append(&build_form(&draft, &seams));
        } else {
            let hint = gtk::Label::new(Some(NOTHING_HINT));
            hint.add_css_class("ts-ws-empty");
            hint.set_xalign(0.0);
            root.append(&hint);
        }
    });
    root.upcast()
}

/// The form for one card.
fn build_form(seed: &Draft, seams: &RowSeams) -> gtk::Widget {
    // The form's own state. Every control writes into it; Save reads it once.
    // An `Rc<RefCell<…>>` rather than a `Mutable` because nothing subscribes —
    // the app list is the only part that redraws, and it redraws because an
    // edit *asked* it to, not because a signal fired.
    let draft = Rc::new(RefCell::new(seed.clone()));

    let column = page_box();
    column.add_css_class("ts-popup-column");

    column.append(&build_header(seed));

    // #1134 change 1: name/layout/autostart in a narrower left column, the app
    // list in a right column with most of the width — a `gtk::Grid` (the same
    // asymmetric-column primitive `panels::media` uses for its art vs. info
    // split) rather than two plain `gtk::Box`es side by side, so a test can
    // assert the split by construction (`grid.child_at`) instead of by guessing
    // which sibling box is which.
    let grid = page_grid_non_homogeneous();
    grid.add_css_class(FORM_GRID_CLASS);

    // ── Left: name, layout, autostart ──────────────────────────────────────
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 4);
    fields.set_size_request(crate::scale::scale(FIELD_COLUMN_WIDTH), -1);
    // #1220, the half a natural-width cap alone does not fix: `hexpand`
    // **propagates up** — `gtk_widget_compute_expand` reports true for any widget
    // with an expanding descendant — and the name entry is `hexpand(true)` so it
    // fills the column. That made this box an expanding one, so `gtk::Grid` gave
    // its column a share of the leftover width on top of its natural size, and the
    // measured split inside the 1080-px page was 434 px of fields against 536 px of
    // app list. A 240-px *request* cannot prevent that; only saying so explicitly
    // can, because setting the flag by hand is what stops the propagation from
    // being consulted.
    fields.set_hexpand(false);

    let name = build_name_field(seed, &draft);
    fields.append(&labelled("Name", &name));
    if seed.active && seed.previous.is_some() {
        let note = gtk::Label::new(Some(RENAME_BLOCKED_HINT));
        note.add_css_class("ts-ws-empty");
        note.set_xalign(0.0);
        note.set_wrap(true);
        // #1220: without this the column's natural width is this whole sentence
        // on one line. See `NOTE_MAX_WIDTH_CHARS`.
        note.set_max_width_chars(NOTE_MAX_WIDTH_CHARS);
        fields.append(&note);
    }

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
    fields.append(&labelled("Default layout", &layout));

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
    fields.append(&labelled("Start at login", &autostart));

    grid.attach(&fields, 0, 0, 1, 1);

    // ── The app list, hexpand so it claims most of the width ──────────────
    let apps_column = gtk::Box::new(gtk::Orientation::Vertical, 6);
    apps_column.set_hexpand(true);

    let (apps, redraw) = build_app_list(&draft, seams);
    apps_column.append(&section_label("Apps"));
    apps_column.append(&apps);

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
    apps_column.append(&add_row);

    // Always right of the fields (#1134 change 1). #1220's first round stacked it
    // under them below the wide page width, which is what a form that follows the
    // page's width needs; this form has its own width instead
    // (`EDIT_FORM_WIDTH`, see the module doc), so the split always fits and the
    // stacked layout has nothing left to solve.
    grid.attach(&apps_column, 1, 0, 1, 1);

    column.append(&grid);

    // ── Save / Cancel ───────────────────────────────────────────────────────
    let (actions, refusal_label) = build_actions(&draft, &name);

    // **The body scrolls; the action row does not** (review MEDIUM 5).
    //
    // `finish_page_clamped` is an `adw::Clamp` — a *width* cap — and the drawer
    // surface is as tall as the screen and imposes no height on its child; it
    // simply clips whatever does not fit. Measured: this form is ~428 px with
    // one app and ~69 px per row after that, with `min == natural`, so GTK
    // cannot even squeeze it — Save and Cancel walked off the bottom at six apps
    // on a 768 px panel and ten on 1080p, with no keyboard route out.
    //
    // A scroller around the *whole* page would answer the clipping and still
    // leave Save ten rows down, reachable only by scrolling past the list you
    // were editing. So the scroller takes the body and the buttons sit under it,
    // always on screen whatever the app count — which is also just what a form
    // looks like.
    //
    // The cap is what makes the scroller scroll at all: with
    // `propagate_natural_height` and no `max_content_height` it requests its
    // whole content height and the drawer grants it, and the clipping is exactly
    // as it was (the same trap `panels::workspaces::build_column` documents).
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(crate::scale::scale(FORM_MAX_HEIGHT))
        .vexpand(true)
        .child(&column)
        .build();
    scroller.add_css_class("ts-ws-edit-scroller");

    let page = page_box();
    page.add_css_class("ts-popup-column");
    page.append(&scroller);
    page.append(&actions);
    page.append(&refusal_label);

    // The form's own width (#1220), the same constant
    // `modal::apply_workspace_edit_width_cap` floors the drawer slot at — so the
    // clamp inside the slot and the slot's minimum agree by construction rather
    // than by both reading a number that could move between them.
    //
    // #1108 had this follow the Workspaces page instead, so the drawer would not
    // jump on ✎. It no longer does: #1219 made that page as narrow as 680, and a
    // 680-px form is the ~150-px app list on #1220. The jump is the accepted cost
    // (Annika, #1219: *"Slight jump in edit form is ok."*) and it is settled per
    // selection, not per revision — the form is rebuilt when the draft lands
    // (`build_slot_with`'s binding), so a monitor hot-plugged while it is open
    // changes nothing here at all; the page behind it resizes live and this page
    // is re-laid out on the next open.
    finish_page_clamped(&page, EDIT_FORM_WIDTH)
}

/// Close the form when its own Save succeeds; show why when it does not.
///
/// Split out so the `bind` call site can be driven with a synthetic signal in a
/// `#[gtk::test]` — the same seam `build_slot` and `panels::workspaces`'
/// `bind_columns` carve, and for the same reason. The apply closure takes its
/// widget from `bind` rather than capturing a strong clone (#224's `WeakRef`
/// contract, which `nix`'s `bind-pins` check enforces at the source level).
fn bind_save_outcome<S>(
    save: &gtk::Button,
    outcomes: S,
    pending: &Rc<std::cell::Cell<Option<u64>>>,
    refusal_label: &gtk::Label,
) where
    S: Signal<Item = Option<workspace_stacks::SaveOutcome>> + 'static,
{
    let pending = Rc::clone(pending);
    let refusal_weak = refusal_label.downgrade();
    bind(outcomes, save, move |save, outcome| {
        let Some(outcome) = outcome else { return };
        // Not ours, or a replay of one that predates this form: ignore it. A
        // `Mutable`'s signal replays on subscribe, so without the ticket every
        // freshly-built form would immediately act on the previous Save.
        if pending.get() != Some(outcome.ticket) {
            return;
        }
        pending.set(None);
        save.set_sensitive(true);
        save.set_label("Save");
        match outcome.error {
            None => {
                close();
                crate::modal::switch_active(crate::modal::Page::Workspaces);
            }
            Some(error) => {
                if let Some(refusal) = refusal_weak.upgrade() {
                    refusal.set_text(&error);
                    refusal.set_visible(true);
                }
            }
        }
    });
}

/// The Save/Cancel row, and the label a refused Save writes under it.
///
/// Returns both because the caller pins them **outside** the body scroller (see
/// [`build_form`]): Save that is reachable only by scrolling past the app list
/// you were editing is barely better than Save that is clipped off the bottom.
fn build_actions(draft: &Rc<RefCell<Draft>>, name: &gtk::Entry) -> (gtk::Box, gtk::Label) {
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

    // The line a refused Save puts under the buttons, in the page rather than in
    // a toast (review MEDIUM 8). Hidden until there is something to say.
    let refusal_label = gtk::Label::new(None);
    refusal_label.add_css_class("ts-ws-edit-error");
    refusal_label.set_xalign(1.0);
    refusal_label.set_wrap(true);
    refusal_label.set_visible(false);

    // Which Save this form is waiting for. `None` = not saving; the binding
    // below ignores every outcome that is not this one, which is what stops a
    // replayed or someone else's result from closing the page.
    let pending: Rc<std::cell::Cell<Option<u64>>> = Rc::new(std::cell::Cell::new(None));

    {
        let draft = Rc::clone(draft);
        let name = name.downgrade();
        let save_weak = save.downgrade();
        let refusal_weak = refusal_label.downgrade();
        let pending = Rc::clone(&pending);
        save.connect_clicked(move |_| {
            if pending.get().is_some() {
                // Already in flight — a second click is not a second Save.
                return;
            }
            match plan_save(&draft.borrow()) {
                Ok(plan) => {
                    // **The form stays up.** It comes down in the outcome
                    // binding below, and only on success — a write that fails
                    // (a taken name the merged view knows about, no overlay
                    // path, a `SetWorkspaceName` that did not land) must not
                    // have already taken the user's whole draft with it.
                    let ticket = workspace_stacks::next_save_ticket();
                    pending.set(Some(ticket));
                    if let Some(save) = save_weak.upgrade() {
                        save.set_sensitive(false);
                        save.set_label("Saving\u{2026}");
                    }
                    if let Some(refusal) = refusal_weak.upgrade() {
                        refusal.set_visible(false);
                    }
                    commit(ticket, &plan);
                }
                Err(why) => {
                    // The correction surface is the field, beside the cursor —
                    // the same call phase 2's Save field made. A toast would put
                    // the rule somewhere the user is not looking.
                    let text = refusal(&why);
                    if let Some(name) = name.upgrade() {
                        name.add_css_class("error");
                        name.set_tooltip_text(Some(&text));
                        name.grab_focus();
                    }
                    if let Some(refusal) = refusal_weak.upgrade() {
                        refusal.set_text(&text);
                        refusal.set_visible(true);
                    }
                }
            }
        });
    }
    actions.append(&save);

    bind_save_outcome(
        &save,
        workspace_stacks::save_outcome(),
        &pending,
        &refusal_label,
    );
    (actions, refusal_label)
}

/// The form's title row: back to the cards, and what this form is for.
fn build_header(seed: &Draft) -> gtk::Widget {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let back = gtk::Button::from_icon_name("go-previous-symbolic");
    back.add_css_class("flat");
    back.set_tooltip_text(Some("Back to the workspaces"));
    // The back arrow is Cancel: leaving the form is leaving it, and a page with
    // two ways out that do different things is a page that loses edits.
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
    header.upcast()
}

/// The name field, writing into the draft as it is typed.
fn build_name_field(seed: &Draft, draft: &Rc<RefCell<Draft>>) -> gtk::Entry {
    let name = gtk::Entry::builder()
        .text(&seed.name)
        .placeholder_text("Workspace name\u{2026}")
        .max_length(32)
        .hexpand(true)
        .build();
    name.add_css_class(NAME_ENTRY_CLASS);
    let draft = Rc::clone(draft);
    name.connect_changed(move |entry| {
        // Clear the red as soon as the correction starts, rather than leaving it
        // through every keystroke of the fix (phase 2's LOW, kept).
        entry.remove_css_class("error");
        entry.set_tooltip_text(None);
        draft.borrow_mut().name = entry.text().to_string();
    });
    name
}

/// The app list, and the handle that rebuilds it.
///
/// Returns both because the rows' own buttons need the handle and the caller
/// needs the widget; see [`Redraw`] for why a rebuild rather than a patch.
fn build_app_list(draft: &Rc<RefCell<Draft>>, seams: &RowSeams) -> (gtk::ListBox, Redraw) {
    let apps = gtk::ListBox::new();
    apps.add_css_class("boxed-list");
    apps.add_css_class("ts-ws-edit-apps");
    apps.set_selection_mode(gtk::SelectionMode::None);

    let meta_cache: MetaCache = Rc::new(RefCell::new(HashMap::new()));
    let redraw: Redraw = Rc::new(RefCell::new(None));

    let rebuild: Rc<dyn Fn()> = {
        let draft = Rc::clone(draft);
        let redraw = Rc::clone(&redraw);
        let seams = seams.clone();
        // Weak, so the closure the list itself transitively holds does not pin
        // the list (#224's contract, stated by hand because this is not a
        // `bind`).
        let apps = apps.downgrade();
        Rc::new(move || {
            let Some(apps) = apps.upgrade() else {
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
                apps.append(&app_row(index, app, &meta_cache, &draft, &redraw, &seams));
            }
        })
    };
    *redraw.borrow_mut() = Some(Rc::clone(&rebuild));
    rebuild();
    (apps, redraw)
}

/// Run the stored rebuild closure, if the form is still alive.
fn fire(redraw: &Redraw) {
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
        SaveError::NameTaken { name } => {
            format!("A workspace called \u{201c}{name}\u{201d} already exists.")
        }
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
fn commit(ticket: u64, plan: &SavePlan) {
    match plan.name_workspace {
        Some(workspace) => {
            workspace_stacks::spawn_save(ticket, workspace, plan.name.clone(), plan.stack.clone());
        }
        None => workspace_stacks::spawn_save_edit(
            ticket,
            plan.previous.clone(),
            plan.name.clone(),
            plan.stack.clone(),
        ),
    }
}

// ── #1312: the editor row's icon starts a stopped app ───────────────────────

/// What an editor row's icon should look like right now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RowVisual {
    /// The app has a window on the stack's workspace, per [`app_row_visual`].
    /// Normal opacity, [`crate::panels::workspaces::APP_RUNNING_CLASS`] — a
    /// click does nothing.
    Lit,
    /// No window (yet), and the stack has a workspace to start it into.
    /// [`crate::panels::workspaces::APP_IDLE_CLASS`], tooltip "Start `<app>`" —
    /// a click starts it.
    Dim,
    /// Either this row's own click started it and
    /// [`workspace_stacks::run_app_start`] has not resolved yet, or the whole
    /// stack's own Start is in flight (#1312 review MED-2: it would race that
    /// Start for the very same `--unit=`). The same spinner/insensitive
    /// treatment `panels::workspaces`' `start_stop_button` uses for the whole
    /// card's `Starting`.
    Starting,
    /// Nothing to click: the stack has no workspace at all
    /// ([`StackState::Inactive`], review HIGH), or this app is not (yet) at
    /// this position in the **saved** stack — a just-added, unsaved row
    /// (review MED-4). Same idle look as [`Self::Dim`], no tooltip, and a
    /// click does nothing.
    Decoration,
}

/// Whether a click on an editor row's icon should start the app.
///
/// Only from [`RowVisual::Dim`]: [`RowVisual::Lit`] does nothing (the Triage's
/// "or focuses — not asked, so not built"); [`RowVisual::Starting`] means one
/// is already in flight, for this row or the whole stack; [`RowVisual::Decoration`]
/// means there is nothing to start into or nothing saved to start.
#[must_use]
fn row_click_starts(visual: RowVisual) -> bool {
    visual == RowVisual::Dim
}

/// Whether the app at `index` of `apps` has a window on `workspace` right now
/// (#1312's per-row presence check).
///
/// Built on [`workspace_stacks::missing_apps`] rather than a second presence
/// check: that function already answers "count-aware, in stack order" for
/// exactly this question (#1133), and reproducing its ordinal math here would
/// be a second copy that could drift. `missing_apps` marks the *trailing* `m`
/// occurrences of a repeated id missing — its ordinal only grows forward, so
/// once "missing" turns true for one occurrence of an id it stays true for
/// every later one — so this asks the same question in closed form: of the
/// occurrences of this app's id that come *after* `index`, are there fewer
/// than `m` of them? If so, `index` itself is inside that trailing group.
#[must_use]
fn app_is_running(
    apps: &[StackApp],
    index: usize,
    workspace: Option<u64>,
    windows: &[Window],
) -> bool {
    let Some(workspace) = workspace else {
        return false;
    };
    let Some(target) = apps.get(index) else {
        return false;
    };
    let stack = Stack {
        apps: apps.to_vec(),
        ..Stack::default()
    };
    let missing = workspace_stacks::missing_apps(&stack, workspace, windows);
    let missing_count = missing.iter().filter(|id| id.as_str() == target.id).count();
    if missing_count == 0 {
        return true;
    }
    let after = apps[index + 1..]
        .iter()
        .filter(|a| a.id == target.id)
        .count();
    after >= missing_count
}

/// An editor row's [`RowVisual`], from [`StackState`] and this app's window
/// presence (#1312) — the same two derivations (`state_of`, `missing_apps`)
/// the Workspaces page's card strips already use to light an app's icon.
///
/// `Inactive` has no workspace to start into at all (review HIGH): a click
/// there would have nowhere to put the launched window, so the row is
/// [`RowVisual::Decoration`], not a stopped-and-clickable [`RowVisual::Dim`].
/// `Starting` reads as [`RowVisual::Starting`] for every row, not merely dim
/// (review MED-2): `spawn_start` marks the whole stack in flight before niri
/// has even named its workspace, so there may be nothing yet to resolve a
/// window against, **and** a click there would race that Start for the exact
/// same `--unit=` [`workspace_stacks::app_launch`] would give this app. A row
/// settles to [`RowVisual::Lit`] or [`RowVisual::Dim`] the moment the state
/// reaches `Active`, on the very next signal tick, same as everything else
/// this page reads live.
#[must_use]
fn app_row_visual(
    state: StackState,
    apps: &[StackApp],
    index: usize,
    workspace: Option<u64>,
    windows: &[Window],
) -> RowVisual {
    match state {
        StackState::Inactive => RowVisual::Decoration,
        StackState::Starting => RowVisual::Starting,
        StackState::Active => {
            if app_is_running(apps, index, workspace, windows) {
                RowVisual::Lit
            } else {
                RowVisual::Dim
            }
        }
    }
}

/// Where the app at `index` of the **draft**'s `apps` actually is in the
/// **saved** stack's own app list (#1312 review MED-4) — `None` when it is
/// not there at all (a just-added, unsaved row).
///
/// Matched by desktop id and **ordinal among same-id draft rows**, not by
/// position: the draft can be reordered without a Save, so position alone
/// would file an app under a sibling's saved unit the moment the user drags a
/// row past one of a different id. Ordinal rather than the whole struct (id
/// *and* `exec`) so toggling an override on/off before saving does not, on
/// its own, make a row look unsaved — only the **set** of ids at or before
/// `index` changing does that.
///
/// Exact for the ordinary case (nothing has reordered *among* this id's own
/// duplicates) and the best available answer when it has — the same
/// trade-off [`app_is_running`]'s count-aware matching already makes.
#[must_use]
fn saved_index_of(saved_apps: &[StackApp], draft_apps: &[StackApp], index: usize) -> Option<usize> {
    let target_id = &draft_apps.get(index)?.id;
    let ordinal = draft_apps[..=index]
        .iter()
        .filter(|a| &a.id == target_id)
        .count();
    saved_apps
        .iter()
        .enumerate()
        .filter(|(_, a)| &a.id == target_id)
        .nth(ordinal - 1)
        .map(|(i, _)| i)
}

/// The workspace `name` is on right now, matched the way niri (and
/// `workspace_stacks::named`, private to that module) does: case
/// **insensitively**, or a workspace named `Chat` would hide from a stack
/// named `chat` while still occupying its name. A local copy rather than
/// widening that function's visibility for one read here.
fn live_workspace_id(workspaces: &[Workspace], name: &str) -> Option<u64> {
    workspaces
        .iter()
        .find(|w| {
            w.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .map(|w| w.id)
}

/// Bind an editor row's icon button to a live [`RowVisual`] signal — the
/// spinner/class/tooltip swap, mirroring the whole-card spinner
/// `panels::workspaces`' `start_stop_button` shows for `StackState::Starting`,
/// scoped to one row.
///
/// Returns the `Cell` the click handler reads to decide whether a click does
/// anything ([`row_click_starts`]) — [`bind()`]'s apply-loop is the only place
/// the latest value is known synchronously, so this is where it is stashed.
fn bind_row_icon<S>(
    button: &gtk::Button,
    image: &gtk::Image,
    display: &str,
    visual: S,
) -> Rc<Cell<RowVisual>>
where
    S: Signal<Item = RowVisual> + 'static,
{
    use crate::components::layout::toggle_class;
    use crate::panels::workspaces::{APP_IDLE_CLASS, APP_RUNNING_CLASS};

    let latest = Rc::new(Cell::new(RowVisual::Decoration));
    let image = image.clone();
    let display = display.to_owned();
    let store = Rc::clone(&latest);
    bind(visual, button, move |button, visual| {
        store.set(visual);
        // #1312 review LOW 8: the doc always claimed this; only the code
        // disagreed. `Starting`/`Decoration` are never a click target
        // (`row_click_starts`), so this is the affordance matching the fact
        // rather than a second gate.
        button.set_sensitive(matches!(visual, RowVisual::Lit | RowVisual::Dim));
        match visual {
            RowVisual::Starting => {
                let spinner = gtk::Spinner::new();
                spinner.start();
                button.set_child(Some(&spinner));
                button.set_tooltip_text(Some("Starting…"));
            }
            RowVisual::Lit => {
                button.set_child(Some(&image));
                toggle_class(&image, APP_RUNNING_CLASS, true);
                toggle_class(&image, APP_IDLE_CLASS, false);
                button.set_tooltip_text(None);
            }
            RowVisual::Dim => {
                button.set_child(Some(&image));
                toggle_class(&image, APP_RUNNING_CLASS, false);
                toggle_class(&image, APP_IDLE_CLASS, true);
                button.set_tooltip_text(Some(&format!("Start {display}")));
            }
            RowVisual::Decoration => {
                button.set_child(Some(&image));
                toggle_class(&image, APP_RUNNING_CLASS, false);
                toggle_class(&image, APP_IDLE_CLASS, true);
                button.set_tooltip_text(None);
            }
        }
    });
    latest
}

/// Wire an editor row's icon click to `on_start`, gated by [`row_click_starts`]
/// on the latest value [`bind_row_icon`] stashed.
fn wire_row_click(
    button: &gtk::Button,
    visual: &Rc<Cell<RowVisual>>,
    on_start: impl Fn() + 'static,
) {
    let visual = Rc::clone(visual);
    button.connect_clicked(move |_| {
        if row_click_starts(visual.get()) {
            on_start();
        }
    });
}

/// One editor row's icon button, fully wired (#1312): lit/dim/starting off
/// `row_visual`, a click on a dim one starting that one app.
///
/// Split out of [`app_row`] purely to keep that function under clippy's line
/// count — there is nothing here `app_row` couldn't inline.
fn build_app_icon(
    index: usize,
    image: &gtk::Image,
    display: &str,
    draft: &Rc<RefCell<Draft>>,
    seams: &RowSeams,
) -> gtk::Button {
    // The name and the draft's own apps come off the draft once, here at
    // row-build time: any edit that could invalidate them (add/remove/drag)
    // rebuilds every row from scratch anyway (see `Redraw`'s doc), so a
    // snapshot is valid for this row's whole life.
    let (stack_name, draft_apps) = {
        let seed = draft.borrow();
        (seed.previous.clone(), seed.apps.clone())
    };
    // #1312 review MED-4: resolved once, against the same snapshot, so the
    // visual and the click agree about which saved row (if any) this is.
    let saved: Option<(Vec<StackApp>, usize)> = stack_name.as_deref().and_then(|name| {
        let stack = (seams.saved_stack)(name)?;
        let saved_index = saved_index_of(&stack.apps, &draft_apps, index)?;
        Some((stack.apps, saved_index))
    });

    let icon_button = gtk::Button::new();
    icon_button.add_css_class("flat");
    icon_button.set_valign(gtk::Align::Center);
    icon_button.set_child(Some(image));

    let visual = (seams.row_visual)(stack_name.clone(), saved.clone());
    let visual_cell = bind_row_icon(&icon_button, image, display, visual);
    let draft = Rc::clone(draft);
    wire_row_click(&icon_button, &visual_cell, move || {
        let Some((name, (_, saved_index))) = stack_name.clone().zip(saved.clone()) else {
            return;
        };
        // The app itself still comes off the *live* draft (an override
        // toggled since this row was built should be respected); only the
        // unit it launches into is pinned to the saved index.
        if let Some(app) = draft.borrow().apps.get(index).cloned() {
            workspace_stacks::spawn_app_start(name, saved_index, app);
        }
    });
    icon_button
}

/// One app of the stack: icon, name, an optional launch-command override,
/// remove, drag handle (#1134 change 2).
fn app_row(
    index: usize,
    app: &StackApp,
    meta_cache: &MetaCache,
    draft: &Rc<RefCell<Draft>>,
    redraw: &Redraw,
    seams: &RowSeams,
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

    let icon_button = build_app_icon(index, &image, &display, draft, seams);
    body.append(&icon_button);

    let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
    labels.set_hexpand(true);

    // Plain `gtk::Label`: a display name and an app-id both come from outside
    // this shell, and `use-markup` is off on a plain label (#30/#753).
    let title = gtk::Label::new(Some(&display));
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::End);
    labels.append(&title);

    // #1134 change 2: the launch command is behind a per-row toggle now — a
    // row shows only its icon and name until an override is on. Whether the
    // entry exists at all (not merely whether it is visible) is what a test
    // reads back, which is why this is an `if`, not a `set_visible`: the
    // toggle's own handler rebuilds the row through [`fire`], so this branch
    // re-evaluates every time the row's override state actually changes.
    let has_override = app.exec.is_some();
    if has_override {
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
    }
    body.append(&labels);

    // The override toggle. Its initial state is `has_override`, read directly
    // off the app rather than tracked separately — the row is rebuilt (never
    // patched) on every add/remove/drag/toggle, so there is nowhere for a
    // second, disagreeing copy of this flag to live.
    let toggle = gtk::ToggleButton::new();
    toggle.set_icon_name("utilities-terminal-symbolic");
    toggle.add_css_class("flat");
    toggle.add_css_class(OVERRIDE_TOGGLE_CLASS);
    toggle.set_valign(gtk::Align::Center);
    toggle.set_tooltip_text(Some("Override the launch command"));
    toggle.set_active(has_override);
    {
        let draft = Rc::clone(draft);
        let redraw = Rc::clone(redraw);
        let resolve = Rc::clone(&seams.resolve);
        let id = app.id.clone();
        toggle.connect_toggled(move |button| {
            {
                let mut draft = draft.borrow_mut();
                if let Some(app) = draft.apps.get_mut(index) {
                    if button.is_active() {
                        // Only when there was nothing saved yet: re-entering an
                        // existing override must not clobber it with a fresh
                        // resolve — this only fires for a row going from no
                        // override to one.
                        if app.exec.is_none() {
                            app.exec = Some(resolve(&id));
                        }
                    } else {
                        // Turning the toggle off clears the override outright
                        // (rather than waiting for Save to drop it): the row's
                        // rendered state is `app.exec.is_some()`, so leaving a
                        // stale value here would spring the toggle back on at
                        // the very next rebuild.
                        app.exec = None;
                    }
                }
            }
            fire(&redraw);
        });
    }
    body.append(&toggle);

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
    source
        .connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&payload.to_value())));
    source
}

/// A row's drop target: dropping row `from` on row `index` moves it there.
fn row_drop_target(index: usize, draft: &Rc<RefCell<Draft>>, redraw: &Redraw) -> gtk::DropTarget {
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
        APP_ROW_CLASS, Draft, EXEC_ENTRY_CLASS, FIELD_COLUMN_WIDTH, FORM_GRID_CLASS,
        NAME_ENTRY_CLASS, NOTHING_HINT, OVERRIDE_TOGGLE_CLASS, RENAME_BLOCKED_HINT, Resolver,
        RowSeams, RowVisual, RowVisualSource, SavedStackSource, bind_row_icon, build_slot_with,
        live_resolver, row_visual_signal, wire_row_click,
    };
    use crate::components::layout::EDIT_FORM_WIDTH;
    use crate::config::workspaces::{Layout, Stack, StackApp};
    // One definition of the geometry discipline (and of how this tree is waited
    // on), shared with the card page's tests rather than copied (review MEDIUM 5).
    use crate::panels::workspaces::tests::{assert_inside_and_hittable, pump_until};
    use crate::panels::workspaces::{APP_IDLE_CLASS, APP_RUNNING_CLASS};
    use crate::scale::scale;
    use hytte::adw;
    use hytte::futures_signals::signal::{Mutable, SignalExt, always};
    use hytte::gtk::{self, prelude::*};
    use hytte::services::niri::{Window, WindowLayout, Workspace};
    use std::cell::Cell;
    use std::collections::BTreeSet;
    use std::rc::Rc;

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

    /// Every app row's override toggle, in row order.
    fn override_toggles(root: &gtk::Widget) -> Vec<gtk::ToggleButton> {
        by_class(root, OVERRIDE_TOGGLE_CLASS)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::ToggleButton>().ok())
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

    /// The event controllers attached to `widget` itself — how GTK4 answers
    /// "is this a drag source / a drop target", since both *are* controllers.
    fn controllers(widget: &gtk::Widget) -> Vec<gtk::EventController> {
        let list = widget.observe_controllers();
        (0..list.n_items())
            .filter_map(|i| list.item(i)?.downcast::<gtk::EventController>().ok())
            .collect()
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
            taken: BTreeSet::new(),
        }
    }

    /// [`saved_draft`] with the stack **stopped** — the state Annika's #1220
    /// screenshot is in, and the one the rename-blocked note is absent from
    /// (`build_form` gates it on `seed.active && seed.previous.is_some()`).
    fn stopped_draft() -> Draft {
        Draft {
            active: false,
            ..saved_draft()
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

    /// The two-column grid the form's fields and app list are attached into.
    fn form_grid(root: &gtk::Widget) -> gtk::Grid {
        by_class(root, FORM_GRID_CLASS)
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Grid>().ok())
            .expect("the form lays its fields out in a grid")
    }

    /// A [`RowVisualSource`] that always answers [`RowVisual::Dim`] and never
    /// touches niri's `Registry` accessors (#1312) — this page's forms are
    /// built with no `Registry` registered at all (module doc, "The form must
    /// survive the refresh poll"), and every test below that isn't
    /// specifically about the row icon just needs rows that build without
    /// panicking.
    fn dim_row_visual() -> RowVisualSource {
        Rc::new(|_, _| always(RowVisual::Dim).boxed_local())
    }

    /// A [`SavedStackSource`] that never has one (#1312) — paired with
    /// [`dim_row_visual`], which never consults it either, so every test
    /// below that isn't specifically about the row icon is unaffected by
    /// either seam.
    fn no_saved_stack() -> SavedStackSource {
        Rc::new(|_| None)
    }

    /// [`slot`]/[`slot_with_resolver`]'s shared body, every seam injected.
    fn slot_with(target: &Mutable<Option<Draft>>, seams: RowSeams) -> gtk::Widget {
        adw::init().expect("libadwaita init");
        let page = build_slot_with(target.signal_cloned(), seams);
        pump();
        page
    }

    /// The slot with a selection it can be driven from — the seam `edit_slot`
    /// wraps around the thread-local.
    ///
    /// Takes no width: since #1220 the form's own width is
    /// [`EDIT_FORM_WIDTH`] whatever the page behind it renders, so there is
    /// nothing to seed and no run-order dependence between these tests.
    fn slot(target: &Mutable<Option<Draft>>) -> gtk::Widget {
        slot_with(
            target,
            RowSeams {
                resolve: live_resolver(),
                row_visual: dim_row_visual(),
                saved_stack: no_saved_stack(),
            },
        )
    }

    /// [`slot`] with the override toggle's resolver injected too, so a test can
    /// drive it without touching the machine's real `$XDG_DATA_DIRS`.
    fn slot_with_resolver(target: &Mutable<Option<Draft>>, resolve: Resolver) -> gtk::Widget {
        slot_with(
            target,
            RowSeams {
                resolve,
                row_visual: dim_row_visual(),
                saved_stack: no_saved_stack(),
            },
        )
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

        // #1134 change 2: the launch command is behind a per-row toggle now —
        // a row with no saved override renders no entry at all, and one with a
        // saved override opens with its toggle already on.
        assert_eq!(
            exec_texts(&page),
            ["alacritty -e weechat"],
            "only the row with a saved override renders a launch-command entry"
        );
        let toggles = override_toggles(&page);
        assert_eq!(toggles.len(), 2, "every app row carries an override toggle");
        assert!(
            !toggles[0].is_active(),
            "firefox has no saved override, so its toggle opens off"
        );
        assert!(
            toggles[1].is_active(),
            "alacritty's saved override opens the toggle on"
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
        assert!(
            autostart.is_active(),
            "the switch opens on the stack's value"
        );

        // An Active stack says why its name cannot change (§5's rename refusal).
        assert!(
            label_texts(&page, "ts-ws-empty").contains(&RENAME_BLOCKED_HINT.to_owned()),
            "an Active stack must say why a rename is refused before it is tried"
        );
    }

    /// #1134 change 1: name/layout/autostart sit in a narrower left column,
    /// and the app list gets the right column with most of the width — at
    /// **every** page width, since #1220 gave the form a width of its own
    /// ([`EDIT_FORM_WIDTH`]) rather than following the page behind it.
    ///
    /// **The mutation**: appending every control into one plain vertical box
    /// again (no grid) reds this — the grid lookup itself fails. So does
    /// attaching the app list at `(0, 1)`, the stacked layout #1220's first round
    /// shipped and Annika's *"slight jump in edit form is ok"* made unnecessary.
    #[gtk::test]
    fn the_form_lays_out_in_two_columns_with_the_app_list_wide() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();

        let grid = form_grid(&page);

        let left = grid.child_at(0, 0).expect("the left (fields) column");
        let right = grid.child_at(1, 0).expect("the right (apps) column");
        assert!(
            grid.child_at(0, 1).is_none(),
            "the two columns sit side by side, so nothing is stacked under the fields"
        );

        assert!(
            right.hexpands(),
            "the app-list column must claim the extra width, not the field column"
        );
        assert!(
            name_field(&page).is_ancestor(&left),
            "the name field belongs in the left column"
        );
        assert!(
            !name_field(&page).is_ancestor(&right),
            "the name field must not be in the app-list column"
        );
        assert!(
            app_rows(&page)[0].is_ancestor(&right),
            "the app list belongs in the right column"
        );
        assert!(
            !app_rows(&page)[0].is_ancestor(&left),
            "the app list must not be in the field column"
        );
    }

    /// #1220: the fields column must stay at its own 240-px request instead of
    /// taking the width #1134 change 1 promised the app list — for a **stopped**
    /// draft and a **running** one, which look like the same bug and are not.
    ///
    /// * **Stopped** is what Annika actually filed. Her screenshot (#1220,
    ///   10:31:55Z) is 418 px wide with `Name / eb` running straight into
    ///   `Default layout` — no rename-blocked note anywhere — and the app list
    ///   cramped anyway. The cause was the missing width floor on the ✎ route:
    ///   the form sized itself to its own **minimum**, where the grid hands every
    ///   column its minimum and nothing is wrong with the columns at all.
    ///   `modal::switch_to_workspace_edit` applies that floor now
    ///   (`modal::gtk_tests::the_edit_button_route_applies_the_width_cap` pins the
    ///   route; this asserts what it buys — the list takes **≥ 60 %** of the form).
    /// * **Running** is the second bug, visible only once the floor exists, and it
    ///   took two mechanisms rather than one — which is why that half is falsified
    ///   by deleting either:
    ///   * the rename-blocked note was `wrap(true)` with no `max_width_chars`, so
    ///     its natural width was its whole unwrapped ~100-character line (measured
    ///     646 px), and a `gtk::Grid` hands every column its natural width before
    ///     anything else;
    ///   * `hexpand` propagates up from the name entry, so the fields box counted
    ///     as an expanding one and the grid gave its column a share of the leftover
    ///     width *on top of* that natural size (measured 434 px).
    ///     `fields.set_hexpand(false)` is what stops it.
    ///
    /// The floor here is `scale(EDIT_FORM_WIDTH)` — the exact number
    /// `modal::apply_workspace_edit_width_cap` pushes onto the real drawer slot,
    /// so this measures the geometry production has. Without a floor a plain
    /// `gtk::Window` sizes to its child's minimum and neither mechanism is
    /// observable at all, which is the trap the first round of this PR fell into:
    /// it floored the harness at a width the ✎ route never applied.
    ///
    /// **The mutations**: dropping `note.set_max_width_chars(NOTE_MAX_WIDTH_CHARS)`
    /// or `fields.set_hexpand(false)` reds the running half; `EDIT_FORM_WIDTH`
    /// dropped back to the ordinary drawer width reds both.
    #[gtk::test]
    fn the_app_list_keeps_most_of_the_width_stopped_or_running() {
        for (what, draft, note_expected) in [
            ("a stopped draft", stopped_draft(), false),
            ("a running draft", saved_draft(), true),
        ] {
            let target: Mutable<Option<Draft>> = Mutable::new(None);
            let page = slot(&target);
            target.set(Some(draft));
            pump();
            assert_eq!(
                label_texts(&page, "ts-ws-empty").contains(&RENAME_BLOCKED_HINT.to_owned()),
                note_expected,
                "{what}: the note is on the form exactly when the stack is running, \
                 and both halves of this test depend on which case they are in"
            );

            let floor = scale(EDIT_FORM_WIDTH);
            page.set_size_request(floor, -1);
            let window = gtk::Window::new();
            window.set_child(Some(&page));
            window.set_default_size(floor, 800);
            window.present();

            let grid = form_grid(&page);
            let fields = grid.child_at(0, 0).expect("the fields column");
            let apps = grid.child_at(1, 0).expect("the app-list column");
            pump_until(2000, || fields.width() > 0 && apps.width() > 0);

            let want = scale(FIELD_COLUMN_WIDTH);
            let slack = scale(48);
            assert!(
                fields.width() <= want + slack,
                "{what}: the fields column is {} px wide, past its own {want} px request \
                 (+{slack} slack for font width): a wrapping label in it is reporting an \
                 unwrapped natural width",
                fields.width()
            );
            // …and the consequence #1134 change 1 actually promised. Stated as a
            // ratio because it survives a different font: "most of the width" is
            // the claim, and with the note uncapped the list came out narrower
            // than the column beside it.
            assert!(
                apps.width() >= 2 * fields.width(),
                "{what}: the app list ({} px) does not get most of the width next to \
                 the fields column ({} px)",
                apps.width(),
                fields.width()
            );
            // The number Annika's complaint is about, against the form's own
            // width rather than against the other column: "Apps too narrow to
            // edit comfortable" was ~150 px of a 418-px form, i.e. 36 %.
            assert!(
                apps.width() * 100 >= floor * 60,
                "{what}: the app list is {} px of a {floor} px form — under 60 %, which \
                 is the shape #1220 was filed about",
                apps.width()
            );

            window.destroy();
        }
    }

    /// #1134 change 2: a row with no saved override renders no launch-command
    /// entry at all — not merely a hidden one — until its toggle is switched
    /// on.
    #[gtk::test]
    fn a_row_without_an_override_renders_no_entry() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(Draft {
            apps: vec![app("org.mozilla.firefox", None)],
            ..saved_draft()
        }));
        pump();

        assert_eq!(app_rows(&page).len(), 1);
        assert!(
            entries(&page, EXEC_ENTRY_CLASS).is_empty(),
            "a row with no saved override must render no launch-command entry"
        );
        let toggle = override_toggles(&page)
            .into_iter()
            .next()
            .expect("the row's override toggle");
        assert!(!toggle.is_active());
    }

    /// #1134 change 2: switching a row's override toggle on reveals an entry
    /// pre-filled with the **resolved** command — the same resolution
    /// `workspace_stacks::app_start` runs at Start time — not a blank field.
    ///
    /// Drives the toggle through an injected [`Resolver`] rather than the real
    /// desktop-entry lookup, so this never touches the machine's real
    /// `$XDG_DATA_DIRS` (the rule #1113's tests kept for every other `Exec`
    /// fact).
    ///
    /// **The mutation**: prefilling with an empty string instead of the
    /// resolver's answer reds this.
    #[gtk::test]
    fn toggling_an_override_on_prefills_the_resolved_command() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let resolve: Resolver = Rc::new(|id: &str| {
            assert_eq!(
                id, "org.mozilla.firefox",
                "resolved the wrong app's command"
            );
            "firefox --new-window".to_owned()
        });
        let page = slot_with_resolver(&target, resolve);
        target.set(Some(Draft {
            apps: vec![app("org.mozilla.firefox", None)],
            ..saved_draft()
        }));
        pump();

        let toggle = override_toggles(&page)
            .into_iter()
            .next()
            .expect("the row's override toggle");
        assert!(!toggle.is_active());

        toggle.set_active(true);
        pump();

        assert_eq!(
            exec_texts(&page),
            ["firefox --new-window"],
            "the revealed entry must start from the resolved command, not empty"
        );
    }

    /// #1134 change 2: turning a row's override toggle off clears it outright
    /// — the row's state is `app.exec.is_some()`, so a stale value would
    /// spring the toggle back on and the entry back up at the very next
    /// rebuild.
    ///
    /// **The mutation**: not clearing `app.exec` when the toggle turns off
    /// reds this — the toggle springs back on.
    #[gtk::test]
    fn toggling_an_override_off_clears_it() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();

        let before = override_toggles(&page);
        assert!(
            before[1].is_active(),
            "alacritty opens with its saved override on"
        );
        before[1].set_active(false);
        pump();

        assert!(
            entries(&page, EXEC_ENTRY_CLASS).is_empty(),
            "turning the override off must clear it, not just hide the entry"
        );
        let after = override_toggles(&page);
        assert!(
            !after[1].is_active(),
            "the toggle must not spring back on — the override is really gone"
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

    /// **Review MEDIUM 5**: Save and Cancel must stay reachable with a long app
    /// list in a drawer-sized window.
    ///
    /// `finish_page_clamped` is an `adw::Clamp` — a *width* cap — and the drawer
    /// surface is as tall as the screen and imposes no height on the page; it
    /// simply clips whatever does not fit. Measured before the fix: ~428 px with
    /// one app and ~69 px per row after that, with `min == natural`, so Save
    /// walked off the bottom at six apps on a 768 px panel and ten on 1080p,
    /// with no keyboard route out.
    ///
    /// Asserted with `assert_inside_and_hittable`'s discipline — geometry in the
    /// container's coordinate space plus a `pick()`, never `is_visible()`, which
    /// is how #851 shipped a chip drawn 250 px outside its clipping bin with two
    /// green tests.
    ///
    /// **The mutation**: deleting the `ScrolledWindow` reds this.
    #[gtk::test]
    fn save_stays_reachable_with_a_long_app_list_in_a_short_drawer() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(Draft {
            apps: (0..10)
                .map(|i| app(&format!("app-{i}"), Some(&format!("cmd-{i}"))))
                .collect(),
            ..saved_draft()
        }));
        pump();

        // A 768 px laptop panel, less the bar — the measurement's own case.
        let window = gtk::Window::new();
        window.set_child(Some(&page));
        window.set_default_size(640, 736);
        window.present();
        pump();

        assert_eq!(app_rows(&page).len(), 10);
        let save = by_class(&page, "ts-ws-edit-save")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("the Save button");
        let cancel = by_class(&page, "ts-ws-edit-cancel")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("the Cancel button");

        assert_inside_and_hittable(&page, save.upcast_ref(), "the Save button");
        assert_inside_and_hittable(&page, cancel.upcast_ref(), "the Cancel button");

        // …and the thing that makes that true is a capped scroller around the
        // **body**, not a lucky allocation: without the cap the scroller
        // requests its whole content height and the drawer grants it, and the
        // clipping is exactly as it was.
        let scroller = by_class(&page, "ts-ws-edit-scroller")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::ScrolledWindow>().ok())
            .expect("the form body is inside a ScrolledWindow");
        assert!(
            scroller.max_content_height() > 0,
            "the scroller is uncapped, so it just grows to fit and never scrolls"
        );
        // The buttons must be **outside** it, or they are reachable only by
        // scrolling past the very list you were editing.
        assert!(
            !save.is_ancestor(&scroller),
            "the Save button is inside the scroller"
        );
        assert!(
            app_rows(&page)[0].is_ancestor(&scroller),
            "…and the app list is not inside it, so nothing actually scrolls"
        );

        // #1121 gap B: a plain `gtk::Window` sizes itself to its child's
        // *minimum*, not its natural size, so the `assert_inside_and_hittable`
        // pair above never actually exercises the cap — this form's ten-app
        // minimum is small (every entry can shrink), so the 640×736 default
        // above is advisory and the window never clips the way the drawer (a
        // layer surface with no imposed height) does. The real guarantee the
        // cap gives is on the *request*: with it, the page must not ask for
        // more height than a 768 px panel, less the bar, can give it.
        //
        // **The mutation**: the scroller's cap raised to `100_000` (uncapped
        // in every way that matters here) reds this — the page's natural
        // height for ten apps measures ~1049 px uncapped, which is what put
        // Save and Cancel off the bottom in the first place.
        let (_, natural, _, _) = page.measure(gtk::Orientation::Vertical, 640);
        assert!(
            natural <= 736,
            "the page's natural height ({natural}) exceeds the drawer's real \
             736 px budget — the scroller cap is not doing its job"
        );

        window.destroy();
    }

    /// **Review MEDIUM 8**: a refused Save leaves the form up with the draft
    /// intact, and says why **in the page**.
    ///
    /// Before the fix, Save called `commit` (fire-and-forget onto the runtime),
    /// then `close()`, then switched the drawer back — so an ephemeral Save onto
    /// a name another stack already had came back as a toast long after the
    /// draft had been dropped, and the user retyped the name, the app list and
    /// every field from scratch.
    ///
    /// **The mutation**: closing the form on `Ok(plan)` again reds this.
    #[gtk::test]
    fn a_refused_save_keeps_the_form_and_its_draft() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(Draft {
            taken: ["music".to_owned()].into_iter().collect(),
            ..ephemeral_draft()
        }));
        pump();

        // Type a name another stack already has, and edit a launch command so
        // there is something to lose.
        name_field(&page).set_text("music");
        let exec = entries(&page, EXEC_ENTRY_CLASS)
            .into_iter()
            .next()
            .expect("a launch-command field");
        exec.set_text("weird --edited");
        pump();

        let save = by_class(&page, "ts-ws-edit-save")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("the Save button");
        save.emit_clicked();
        pump();

        // The form is still here, with everything the user typed.
        assert_eq!(app_rows(&page).len(), 1, "the form was torn down");
        assert_eq!(name_field(&page).text(), "music", "the name was lost");
        assert_eq!(
            entries(&page, EXEC_ENTRY_CLASS)
                .into_iter()
                .next()
                .expect("a launch-command field")
                .text(),
            "weird --edited",
            "the edited launch command was lost"
        );
        // …and it says why, in the page rather than only in a toast.
        assert!(
            name_field(&page).has_css_class("error"),
            "the field was not marked"
        );
        let shown = label_texts(&page, "ts-ws-edit-error");
        assert!(
            shown.iter().any(|t| t.contains("music")),
            "the refusal is not shown in the page: {shown:?}"
        );
        // Save is still usable — a refusal is not a dead end.
        assert!(save.is_sensitive());
    }

    /// …and the outcome binding is what closes it, on **its own** Save only.
    ///
    /// A `Mutable`'s signal replays on subscribe, so without the ticket every
    /// freshly-built form would immediately act on whatever the previous Save
    /// did. Driven through the injected signal rather than the global one.
    ///
    /// **The mutation**: dropping the ticket comparison reds the first half.
    #[gtk::test]
    fn a_form_ignores_a_save_outcome_that_is_not_its_own() {
        use crate::workspace_stacks::SaveOutcome;

        adw::init().expect("libadwaita init");
        let save = gtk::Button::with_label("Save");
        let refusal = gtk::Label::new(None);
        refusal.set_visible(false);
        let pending: Rc<std::cell::Cell<Option<u64>>> = Rc::new(std::cell::Cell::new(Some(7)));
        let outcomes: Mutable<Option<SaveOutcome>> = Mutable::new(None);
        super::bind_save_outcome(&save, outcomes.signal_cloned(), &pending, &refusal);
        pump();

        // Somebody else's Save, and a stale replay: neither is ours.
        outcomes.set(Some(SaveOutcome {
            ticket: 6,
            error: Some("not ours".to_owned()),
        }));
        pump();
        assert_eq!(
            pending.get(),
            Some(7),
            "an outcome that is not ours was taken"
        );
        assert!(!refusal.is_visible(), "…and it was shown to the user");

        // Ours, and failed: the reason is shown and the form stays waiting on
        // nothing further.
        outcomes.set(Some(SaveOutcome {
            ticket: 7,
            error: Some("the writer refused".to_owned()),
        }));
        pump();
        assert_eq!(pending.get(), None);
        assert!(refusal.is_visible());
        assert_eq!(refusal.text(), "the writer refused");
        assert!(save.is_sensitive(), "Save must be usable again");
    }

    /// #1121 gap C: the *other* half of review MEDIUM 8. The fast half —
    /// `plan_save`'s own refusal — is pinned by
    /// `a_refused_save_keeps_the_form_and_its_draft`; this is the async half, a
    /// failed [`workspace_stacks::SaveOutcome`] arriving through
    /// `bind_save_outcome` (a `NoOverlayPath`, or a `SetWorkspaceName` that did
    /// not land), which had no test of its own — closing the form there would
    /// throw the user's draft away over a write it never actually made.
    ///
    /// `close()` — what a regression on this path would also call — writes to
    /// the real thread-local `TARGET`, so a draft has to actually be in it for
    /// "still open" to mean anything.
    ///
    /// **The mutation**: `bind_save_outcome` calling `close()` on `Some(error)`
    /// too, instead of only on `None`, reds this.
    #[gtk::test]
    fn a_failed_save_outcome_leaves_the_form_open() {
        use crate::workspace_stacks::SaveOutcome;

        adw::init().expect("libadwaita init");
        super::open(ephemeral_draft());

        let save = gtk::Button::with_label("Save");
        let refusal = gtk::Label::new(None);
        refusal.set_visible(false);
        let pending: Rc<std::cell::Cell<Option<u64>>> = Rc::new(std::cell::Cell::new(Some(11)));
        let outcomes: Mutable<Option<SaveOutcome>> = Mutable::new(None);
        super::bind_save_outcome(&save, outcomes.signal_cloned(), &pending, &refusal);
        pump();

        outcomes.set(Some(SaveOutcome {
            ticket: 11,
            error: Some("no overlay path".to_owned()),
        }));
        pump();

        assert!(
            super::TARGET.with(|target| target.get_cloned()).is_some(),
            "a refused Save closed the form"
        );

        // `TARGET` is a thread-local shared by every `#[gtk::test]` in this
        // binary — they all run on the one GTK thread gtk4-macros creates —
        // so leave it as `close()` would, or the next test that opens the
        // form finds a stale draft still sitting in it.
        super::close();
    }

    /// **Cancel writes nothing** (#1071 §5) — observable after all.
    ///
    /// `EditContext` does not make this testable in a tempdir: the write
    /// destination is `xdg::overlay_path` inside `save_edit`, which takes no
    /// path parameter, and redirecting `$XDG_CONFIG_HOME` from a test needs
    /// `std::env::set_var`, `unsafe` in edition 2024 and this crate forbids
    /// it. It is testable anyway, because every write the form can start
    /// claims a ticket from `workspace_stacks::next_save_ticket` — a
    /// process-wide `AtomicU64` — before it spawns, so a Cancel that starts no
    /// write claims none (#1121, adopted from the #1113 re-verification).
    ///
    /// **The mutation**: giving the Cancel handler Save's `Ok(plan)` arm
    /// (i.e. having Cancel commit) reds this — on the assertion, not on an
    /// abort.
    #[gtk::test]
    fn cancel_starts_no_write() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(ephemeral_draft()));
        pump();
        name_field(&page).set_text("chat");
        pump();

        let before = crate::workspace_stacks::next_save_ticket();
        let cancel = by_class(&page, "ts-ws-edit-cancel")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("the Cancel button");
        cancel.emit_clicked();
        pump();
        let after = crate::workspace_stacks::next_save_ticket();

        assert_eq!(
            after - before,
            1,
            "Cancel claimed a save ticket, so it started a write"
        );
    }

    /// Every app row carries both halves of §5's drag: a source on the handle
    /// and a target on the row.
    #[gtk::test]
    fn every_app_row_can_be_dragged_and_dropped_on() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let page = slot(&target);
        target.set(Some(saved_draft()));
        pump();

        for row in app_rows(&page) {
            assert!(
                controllers(&row)
                    .iter()
                    .any(ObjectExt::is::<gtk::DropTarget>),
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

    /// #1312: the row icon's bound class follows a `RowVisual` signal live —
    /// [`APP_RUNNING_CLASS`] on `Lit`, gone on `Dim`/`Decoration`, and vice
    /// versa — plus `Starting`/`Decoration` actually going insensitive
    /// (review LOW 8).
    ///
    /// **The mutation**: swapping the two classes in [`bind_row_icon`]'s
    /// `match` (or dropping the `toggle_class` calls) reds this — the class
    /// present after a flip would be the wrong one, or both would linger.
    #[gtk::test]
    fn the_row_icon_class_follows_the_signal() {
        adw::init().expect("libadwaita init");
        let visual = Mutable::new(RowVisual::Dim);
        let button = gtk::Button::new();
        let image = gtk::Image::from_icon_name("application-x-executable-symbolic");
        let _cell = bind_row_icon(&button, &image, "Alacritty", visual.signal());
        pump();

        // #1312 review LOW 5: the classes live on the `gtk::Image` — the
        // card's own selectors, reused, would not match a `gtk::Button`.
        assert!(
            image.has_css_class(APP_IDLE_CLASS),
            "starts dim: no window yet"
        );
        assert!(!image.has_css_class(APP_RUNNING_CLASS));
        assert!(button.is_sensitive(), "dim is still a click target");

        visual.set(RowVisual::Lit);
        pump();
        assert!(
            image.has_css_class(APP_RUNNING_CLASS),
            "the window appeared — the icon must light"
        );
        assert!(
            !image.has_css_class(APP_IDLE_CLASS),
            "the idle class must not linger once lit"
        );
        assert!(button.is_sensitive(), "lit is still a click target (no-op)");

        visual.set(RowVisual::Starting);
        pump();
        assert!(
            !button.is_sensitive(),
            "#1312 review LOW 8: Starting must actually be insensitive, not \
             merely documented as such"
        );

        visual.set(RowVisual::Decoration);
        pump();
        assert!(
            image.has_css_class(APP_IDLE_CLASS),
            "decoration looks idle, like a stopped app"
        );
        assert!(
            !button.is_sensitive(),
            "decoration is not a click target — nothing to start into"
        );

        visual.set(RowVisual::Dim);
        pump();
        assert!(
            image.has_css_class(APP_IDLE_CLASS),
            "the window went away again — the icon must dim"
        );
        assert!(!image.has_css_class(APP_RUNNING_CLASS));
        assert!(button.is_sensitive());
    }

    /// #1312: a click on the row icon only calls the starter from
    /// [`RowVisual::Dim`] — the wiring `row_click_starts` gates, not merely
    /// the pure predicate in isolation.
    ///
    /// **The mutation**: dropping the `row_click_starts` guard in
    /// [`wire_row_click`] (calling `on_start` unconditionally) reds the
    /// `Lit`/`Starting` presses below.
    #[gtk::test]
    fn a_click_only_starts_from_a_dim_icon() {
        adw::init().expect("libadwaita init");
        let visual = Mutable::new(RowVisual::Dim);
        let button = gtk::Button::new();
        let image = gtk::Image::from_icon_name("application-x-executable-symbolic");
        let cell = bind_row_icon(&button, &image, "Alacritty", visual.signal());
        pump();

        let starts = Rc::new(Cell::new(0_u32));
        wire_row_click(&button, &cell, {
            let starts = Rc::clone(&starts);
            move || starts.set(starts.get() + 1)
        });

        button.emit_clicked();
        assert_eq!(starts.get(), 1, "a dim icon must start on click");

        visual.set(RowVisual::Lit);
        pump();
        button.emit_clicked();
        assert_eq!(starts.get(), 1, "a running app's click must emit nothing");

        visual.set(RowVisual::Starting);
        pump();
        button.emit_clicked();
        assert_eq!(
            starts.get(),
            1,
            "one already in flight for this row must not start a second time"
        );

        visual.set(RowVisual::Dim);
        pump();
        button.emit_clicked();
        assert_eq!(starts.get(), 2, "dim again once the signal says so");
    }

    /// A minimal niri workspace named `name` — this module's own fixture,
    /// since `workspace_stacks::tests`'s is private to that module.
    fn ws_named(id: u64, name: &str) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: Some(name.to_owned()),
            output: Some("DP-1".to_owned()),
            is_urgent: false,
            is_active: false,
            is_focused: false,
            active_window_id: None,
        }
    }

    /// [`ws_named`]'s window counterpart.
    fn win_on(id: u64, workspace: u64, app_id: &str) -> Window {
        Window {
            id,
            title: None,
            app_id: Some(app_id.to_owned()),
            pid: Some(1000 + i32::try_from(id).expect("small id")),
            workspace_id: Some(workspace),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    /// A [`RowVisualSource`] built on the real [`row_visual_signal`] (#1312
    /// review LOW 7) but fed scripted `Mutable`s instead of the real
    /// `niri`/`workspace_stacks` globals — so this drives `build_app_icon`/
    /// `app_row` through the actual production seam, not a constant.
    fn scripted_row_visual(
        workspaces: Mutable<Vec<Workspace>>,
        windows: Mutable<Vec<Window>>,
        slices_up: Mutable<BTreeSet<String>>,
        starting: Mutable<BTreeSet<String>>,
        app_starting: Mutable<BTreeSet<String>>,
    ) -> RowVisualSource {
        Rc::new(move |stack_name, saved| {
            row_visual_signal(
                stack_name,
                saved,
                workspaces.signal_cloned(),
                windows.signal_cloned(),
                slices_up.signal_cloned(),
                starting.signal_cloned(),
                app_starting.signal_cloned(),
            )
        })
    }

    /// #1312 review LOW 7: `build_app_icon`/`app_row` driven end to end
    /// through the **real** [`row_visual_signal`] and a real
    /// [`SavedStackSource`] — every other test here injects
    /// [`dim_row_visual`]/[`no_saved_stack`], which never exercises either
    /// seam's actual logic.
    ///
    /// **The mutation**: `saved_index_of` returning the *draft* index
    /// unconditionally (skipping the saved-stack lookup this test's
    /// `firefox`+`alacritty` pair depends on) reds this — `alacritty`'s row
    /// would look for a window at index 0, which only `firefox`'s window
    /// satisfies, and it would light on the wrong evidence or not at all.
    #[gtk::test]
    fn a_saved_apps_row_lights_through_the_real_seam_end_to_end() {
        let target: Mutable<Option<Draft>> = Mutable::new(None);
        let workspaces: Mutable<Vec<Workspace>> = Mutable::new(vec![ws_named(9, "chat")]);
        let windows: Mutable<Vec<Window>> = Mutable::new(Vec::new());
        let slices_up: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let starting: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let app_starting: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());

        let saved_apps = vec![
            crate::config::workspaces::StackApp {
                id: "firefox".to_owned(),
                exec: None,
            },
            crate::config::workspaces::StackApp {
                id: "alacritty".to_owned(),
                exec: None,
            },
        ];
        let saved_stack: SavedStackSource = {
            let saved_apps = saved_apps.clone();
            Rc::new(move |name: &str| {
                (name == "chat").then(|| Stack {
                    apps: saved_apps.clone(),
                    ..Stack::default()
                })
            })
        };
        let row_visual = scripted_row_visual(
            workspaces,
            windows.clone(),
            slices_up,
            starting,
            app_starting,
        );

        let page = slot_with(
            &target,
            RowSeams {
                resolve: live_resolver(),
                row_visual,
                saved_stack,
            },
        );

        let mut draft = saved_draft();
        draft.previous = Some("chat".to_owned());
        draft.workspace = Some(9);
        draft.apps = saved_apps;
        target.set(Some(draft));
        pump();

        let icons = || by_class(&page, APP_IDLE_CLASS);
        assert_eq!(
            icons().len(),
            2,
            "both rows start idle — niether app has a window yet"
        );

        // Firefox's window appears.
        windows.set(vec![win_on(50, 9, "firefox")]);
        pump();
        let lit = by_class(&page, APP_RUNNING_CLASS);
        assert_eq!(lit.len(), 1, "exactly one row lights: {lit:?}");
        assert_eq!(
            by_class(&page, APP_IDLE_CLASS).len(),
            1,
            "the other (alacritty) is still idle"
        );
    }
}

#[cfg(test)]
mod model_tests {
    use super::{
        Draft, RowVisual, SaveError, SavePlan, app_row_visual, ephemeral_apps, move_app, plan_save,
        rename_is_blocked, resolved_command, row_click_starts, saved_index_of,
    };
    use crate::components::desktop_entry::Launchable;
    use crate::config::workspaces::{Layout, Stack, StackApp};
    use crate::workspace_stacks::StackState;
    use hytte::services::niri::{Window, WindowLayout};
    use std::collections::BTreeSet;

    fn app(id: &str, exec: Option<&str>) -> StackApp {
        StackApp {
            id: id.to_owned(),
            exec: exec.map(str::to_owned),
        }
    }

    /// A minimal window on `workspace`, running `app_id` (#1312's fixtures).
    fn window(id: u64, workspace: u64, app_id: &str) -> Window {
        Window {
            id,
            title: None,
            app_id: Some(app_id.to_owned()),
            pid: Some(1000 + i32::try_from(id).expect("small id")),
            workspace_id: Some(workspace),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((1, 1)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
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

    /// **Review MEDIUM 2**: `Starting` blocks a rename too — and it is the
    /// window in which it matters most.
    ///
    /// Press ▶, then ✎ (which is *not* disabled while Starting; only start/stop
    /// is), rename, Save: the apps are at that moment being launched into
    /// `trollshell-ws-<old>.slice` under units named after it while the entry
    /// becomes `<new>`, and the grace window is up to ten seconds wide.
    ///
    /// The mapping used to be an inline `== StackState::Active` inside
    /// `draft_for`, which no test could reach.
    ///
    /// **The mutation**: `rename_is_blocked` answering `false` for `Starting`
    /// reds this.
    #[test]
    fn a_rename_is_refused_while_the_stack_is_starting_too() {
        for state in [StackState::Active, StackState::Starting] {
            let blocked = Draft {
                active: rename_is_blocked(state),
                ..draft("dev", Some("chat"))
            };
            assert_eq!(
                plan_save(&blocked).expect_err("refused"),
                SaveError::RenameWhileActive,
                "{state:?} must block a rename"
            );
        }
        // …and Inactive still does not.
        let free = Draft {
            active: rename_is_blocked(StackState::Inactive),
            ..draft("dev", Some("chat"))
        };
        assert!(
            plan_save(&free).is_ok(),
            "Inactive must still allow a rename"
        );
    }

    /// **Review MEDIUM 8**: a name another stack already has is refused *in the
    /// form*, not by the writer after the drawer has gone and taken the draft.
    ///
    /// Phase 2's inline field made this check beside the cursor; #1109 deleted
    /// the field, and nothing replaced the check.
    ///
    /// **The mutation**: deleting the `taken` check reds this.
    #[test]
    fn a_name_another_stack_already_has_is_refused() {
        let taken: BTreeSet<String> = ["music".to_owned(), "dev".to_owned()].into_iter().collect();

        // An ephemeral Save onto a taken name.
        let ephemeral = Draft {
            taken: taken.clone(),
            workspace: Some(7),
            ..draft("music", None)
        };
        assert_eq!(
            plan_save(&ephemeral).expect_err("refused"),
            SaveError::NameTaken {
                name: "music".to_owned()
            }
        );
        // Case-insensitively, the way niri and the validator compare names.
        let shouty = Draft {
            taken: taken.clone(),
            ..draft("MUSIC", None)
        };
        assert_eq!(
            plan_save(&shouty).expect_err("refused"),
            SaveError::NameTaken {
                name: "music".to_owned()
            }
        );

        // …and a rename onto one.
        let renaming = Draft {
            taken: taken.clone(),
            ..draft("dev", Some("chat"))
        };
        assert_eq!(
            plan_save(&renaming).expect_err("refused"),
            SaveError::NameTaken {
                name: "dev".to_owned()
            }
        );

        // But re-saving a stack under its **own** name is the ordinary case and
        // must not trip over itself — even though `taken` is non-empty.
        let unchanged = Draft {
            taken,
            ..draft("chat", Some("chat"))
        };
        assert!(
            plan_save(&unchanged).is_ok(),
            "saving a stack under its own name was refused"
        );
    }

    /// §3.7: an ephemeral card's Save carries the workspace to **name**; a saved
    /// card's does not, because niri has nothing to do for it.
    #[test]
    fn only_an_ephemeral_save_names_a_workspace() {
        let ephemeral = Draft {
            workspace: Some(7),
            ..draft("chat", None)
        };
        assert_eq!(
            plan_save(&ephemeral).expect("accepted").name_workspace,
            Some(7)
        );

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
            taken: BTreeSet::new(),
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

    /// Column order is preserved and two windows of one app are **two**
    /// entries (#1133), each carrying its own window's command line — the
    /// stack is a list of apps, one per window, not deduped by app-id.
    ///
    /// **The mutation**: reintroducing the pre-#1133 dedup (`seen.insert` as a
    /// filter) reds this — `ids` comes back `["b", "a"]` instead of
    /// `["b", "a", "b"]`, and `apps[2]`'s command line is lost entirely.
    #[test]
    fn an_ephemeral_saves_apps_are_one_entry_per_window_in_column_order() {
        let known = BTreeSet::new();
        let windows = vec![
            ("b".to_owned(), Some("b-cmd".to_owned())),
            ("a".to_owned(), Some("a-cmd".to_owned())),
            ("b".to_owned(), Some("b-cmd-2".to_owned())),
        ];
        let apps = ephemeral_apps(&windows, &known);
        assert_eq!(
            ids(&apps),
            ["b", "a", "b"],
            "one entry per window, column order preserved — not deduped"
        );
        assert_eq!(apps[0].exec.as_deref(), Some("b-cmd"));
        assert_eq!(
            apps[2].exec.as_deref(),
            Some("b-cmd-2"),
            "the second window of the same app-id keeps its own command line"
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

    /// #1134 change 2: the override toggle's prefill is the same resolution
    /// `workspace_stacks::app_start` runs at Start time — field codes stripped
    /// — not the raw `Exec=` line verbatim.
    ///
    /// **The mutation**: returning `entry.exec` unstripped reds this.
    #[test]
    fn resolved_command_strips_field_codes_like_start_does() {
        let entry = Launchable {
            exec: "firefox %u --new-window".to_owned(),
            dbus_activatable: false,
            try_exec_missing: false,
        };
        assert_eq!(
            resolved_command(Some(&entry)),
            "firefox --new-window",
            "the toggle must prefill what Start would actually run, not the raw Exec= line"
        );
    }

    /// An id with no installed desktop entry has nothing to prefill.
    #[test]
    fn resolved_command_is_empty_with_no_entry() {
        assert_eq!(resolved_command(None), String::new());
    }

    /// #1312: the editor row's icon lights only for `Active` with a matching
    /// window — all four of `state_of`'s states (`Active` split into its two
    /// sub-cases), and the review's HIGH/MED-2 fixes: `Inactive` is
    /// `Decoration` (nowhere to start into), `Starting` is `Starting` (would
    /// race the whole stack's own Start for the same unit), neither is `Dim`.
    ///
    /// **The mutation**: inverting the predicate (or answering `Dim` for
    /// `Inactive`/`Starting`) reds every case here.
    #[test]
    fn a_row_lights_only_when_active_and_its_app_has_a_window() {
        let apps = vec![app("Alacritty", None)];
        let here = [window(1, 9, "Alacritty")];

        assert_eq!(
            app_row_visual(StackState::Active, &apps, 0, Some(9), &here),
            RowVisual::Lit,
            "Active + a matching window → lit"
        );
        assert_eq!(
            app_row_visual(StackState::Active, &apps, 0, Some(9), &[]),
            RowVisual::Dim,
            "Active + no window → dim"
        );
        assert_eq!(
            app_row_visual(StackState::Inactive, &apps, 0, Some(9), &here),
            RowVisual::Decoration,
            "Inactive has no workspace to start into → decoration, even with a \
             matching window elsewhere"
        );
        assert_eq!(
            app_row_visual(StackState::Starting, &apps, 0, Some(9), &here),
            RowVisual::Starting,
            "the whole stack is starting → starting, not a click target"
        );
    }

    /// #1133 carried into the row: two entries of the same desktop id with
    /// only one window light the **first** row and dim the **second**, not
    /// both or neither.
    ///
    /// **The mutation**: checking membership (`windows.iter().any(id
    /// matches)`) instead of the count-aware rule reds this — both rows would
    /// read as lit.
    #[test]
    fn duplicate_app_ids_light_only_as_many_rows_as_there_are_windows() {
        let apps = vec![app("Alacritty", None), app("Alacritty", None)];
        let here = [window(1, 9, "Alacritty")];

        assert_eq!(
            app_row_visual(StackState::Active, &apps, 0, Some(9), &here),
            RowVisual::Lit,
            "the first entry has the one open window"
        );
        assert_eq!(
            app_row_visual(StackState::Active, &apps, 1, Some(9), &here),
            RowVisual::Dim,
            "the second entry has none left"
        );
    }

    /// #1312 review MED-2: a click during the whole stack's own `Starting`
    /// must not fire — it would race that Start for the same `--unit=`.
    ///
    /// **The mutation**: mapping `Starting` back to `Dim` in
    /// [`app_row_visual`] reds this.
    #[test]
    fn a_row_is_not_clickable_while_the_whole_stack_is_starting() {
        let apps = vec![app("Alacritty", None)];
        let here = [window(1, 9, "Alacritty")];
        let visual = app_row_visual(StackState::Starting, &apps, 0, Some(9), &here);
        assert_eq!(visual, RowVisual::Starting);
        assert!(!row_click_starts(visual));
    }

    /// #1312 review MED-4: a draft reordered but not (yet) saved still starts
    /// under the **saved** index, not its own current position.
    ///
    /// **The mutation**: using the draft's own `index` unchanged (skipping
    /// `saved_index_of`) reds this — it would resolve to 0 for both apps
    /// instead of disagreeing with their draft positions.
    #[test]
    fn a_reordered_unsaved_draft_still_resolves_the_saved_index() {
        let saved = vec![app("firefox", None), app("alacritty", None)];
        // The user dragged alacritty to the top and has not saved.
        let draft = vec![app("alacritty", None), app("firefox", None)];

        assert_eq!(
            saved_index_of(&saved, &draft, 0),
            Some(1),
            "alacritty, now at draft position 0, is saved at index 1"
        );
        assert_eq!(
            saved_index_of(&saved, &draft, 1),
            Some(0),
            "firefox, now at draft position 1, is saved at index 0"
        );
    }

    /// #1312 review MED-4: an app just added to the draft and never saved has
    /// no saved index — the row it would build is decoration, not a start
    /// under some other app's unit.
    #[test]
    fn a_newly_added_unsaved_app_has_no_saved_index() {
        let saved = vec![app("firefox", None)];
        let draft = vec![app("firefox", None), app("alacritty", None)];
        assert_eq!(saved_index_of(&saved, &draft, 1), None);
    }

    /// A duplicate id in the draft still resolves ordinally: the **second**
    /// draft occurrence of an id maps to the **second** saved occurrence, not
    /// back to the first.
    #[test]
    fn a_duplicate_id_resolves_by_ordinal_not_by_first_match() {
        let saved = vec![app("Alacritty", None), app("Alacritty", None)];
        let draft = saved.clone();
        assert_eq!(saved_index_of(&saved, &draft, 0), Some(0));
        assert_eq!(saved_index_of(&saved, &draft, 1), Some(1));
    }

    /// A click on the editor row's icon only starts anything from
    /// [`RowVisual::Dim`].
    ///
    /// **The mutation**: dropping the guard (answering `true` unconditionally)
    /// reds the `Lit`/`Starting` cases.
    #[test]
    fn a_click_only_starts_from_a_dim_icon() {
        assert!(
            row_click_starts(RowVisual::Dim),
            "a stopped app's icon must start on click"
        );
        assert!(
            !row_click_starts(RowVisual::Lit),
            "a running app's click must emit nothing"
        );
        assert!(
            !row_click_starts(RowVisual::Starting),
            "one already in flight for this row must not start a second time"
        );
    }
}
