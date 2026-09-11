//! Workspaces drawer page — phases 1 and 2 of the workspace-stacks epic
//! (#1071 §6), designed on discussion #1063.
//!
//! One column per monitor, side by side; in each column, a card per saved stack
//! and a card per *unnamed but populated* niri workspace; on each card, the
//! name, a strip of app icons, and — for a saved stack — one Start/Stop button.
//!
//! ## Where the cards come from
//!
//! Three sources, joined in [`model`], and the join is the substance of phase 2:
//!
//! * **`workspaces.toml`** ([`crate::config::workspaces`]) decides which cards
//!   exist and in what order. A saved stack is a card whether or not it is on a
//!   screen — that is what "Inactive cards keep their place" means (§3.6).
//! * **niri** decides which of them are Active, which of a stack's app icons are
//!   lit, and which unnamed workspaces get an **ephemeral** card. §3.7 settles
//!   that an unnamed workspace is a card like any other, with Edit → Save
//!   creating its entry — so there is deliberately **no `+` button**.
//! * **systemd** ([`crate::workspace_stacks::slices_up`]) is the other half of
//!   the Active derivation (§3.3): a stack whose windows are all gone but whose
//!   units linger is still Active, and still has something for Stop to do.
//!
//! ## Where the columns come from
//!
//! From niri, not from GDK. Every `Workspace` carries the `output` it lives on,
//! and niri keeps at least one workspace per connected output, so the connected
//! set comes out of the *same* snapshot the cards are built from. Joining a
//! second monitor source would only introduce a window where the two disagree;
//! `App::monitors()` is not reachable from a panel anyway, since
//! `modal::build_page` hands a page no `&Monitor`.
//!
//! Column order is lexical by connector, which is the order
//! `hytte::services::displays::outputs()` already sorts outputs into, so the
//! Workspaces page and the Displays page list the same screens the same way.
//! One trailing column may follow them: the stacks whose recorded monitor is not
//! connected (§5). Phase 3 (§3.6) gives cards a saved order *inside* a column
//! via `MoveWorkspaceToIndex`; the column order itself stays a property of the
//! connector set.
//!
//! ## What is still phases 3–4
//!
//! No autostart, no column-order restore, no card drag between columns, and no
//! edit sub-page: phase 2's Edit is the ephemeral card's inline name field, and
//! Save takes the **name only** (§3.7) — the apps come from the workspace as it
//! is right now and everything else is defaulted. The full form, the
//! desktop-entry picker and `Exec` field-code stripping are phase 4.
//!
//! ## Testability seam
//!
//! [`panel_workspaces`] only supplies the signals; [`build_panel`] takes them
//! generically and [`model`] is a pure function of one snapshot. Every accessor
//! it wraps `.expect()`s a registered `Registry`, so this split is what lets the
//! page be driven from a bare `#[gtk::test]` with plain `Mutable`s — the same
//! seam `widgets::workspaces::bind_workspace_pills` and
//! `panels::bluetooth::bind_device_groups` carve for the same reason.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, pango, prelude::*};
use hytte::prelude::*;
use hytte::services::niri::{self, Window, Workspace};
use hytte::services::systemd;

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::layout::{
    DRAWER_MAX_WIDTH_WIDE, finish_page_clamped, page_box, toggle_class,
};
use crate::config::workspaces::{
    self as config_workspaces, Stack, StackApp as SavedApp, Workspaces,
};
use crate::workspace_stacks::{self, StackState, state_of};

/// CSS class on an app icon whose app has at least one window open on the
/// card's workspace.
const APP_RUNNING_CLASS: &str = "ts-ws-app-running";

/// The complement of [`APP_RUNNING_CLASS`] — a saved app of the stack with no
/// window open.
const APP_IDLE_CLASS: &str = "ts-ws-app-idle";

/// CSS class on a card that is not on a screen (#1071 §5).
const CARD_INACTIVE_CLASS: &str = "ts-ws-card-inactive";

/// Design-baseline height cap for one monitor's card list, in CSS px, before
/// [`crate::scale::scale`]. Sized like `panels::connections`' list scroller —
/// tall enough that a realistic stack count never scrolls, short enough that the
/// drawer can lay the columns out rather than being told to be as tall as the
/// tallest one.
const COLUMN_MAX_HEIGHT: i32 = 480;

/// Shown in a monitor's column when that monitor has no cards at all.
const EMPTY_COLUMN_HINT: &str = "No workspaces on this screen";

/// Shown instead of the columns when niri has reported no outputs yet — the
/// first moments after a shell start, or a lost IPC socket.
const NO_OUTPUTS_HINT: &str = "Waiting for niri\u{2026}";

/// The name an unsaved workspace shows in place of one (#1071 §3.7).
const EPHEMERAL_NAME: &str = "Unsaved workspace";

/// Under an Inactive card, in Annika's own words from the epic.
const INACTIVE_HINT: &str = "Not on a screen";

/// Tooltip on the trailing column's heading.
const OFFLINE_COLUMN_HINT: &str =
    "These stacks name a screen that is not connected. Starting one puts it on the focused screen.";

/// The Save field's placeholder.
const SAVE_PLACEHOLDER: &str = "Name this workspace\u{2026}";

/// Shown on the Save field when the typed name cannot be a workspace name.
const NAME_HINT: &str = "Lowercase letters, digits and single dashes — no leading, \
                         trailing or doubled dash, at most 32 characters.";

/// The rule, plus — when there is one — the name the typed one would become.
///
/// §3.1 asks the Save entry to "offer the sanitised form". `normalize` only
/// folds case, deliberately, so the suggestion is computed separately and
/// *offered* rather than applied: a silent rewrite would hand the user a stack
/// under a name they did not type, while `niri msg action focus-workspace` still
/// answers to the one they did.
fn name_hint(typed: &str) -> String {
    match sanitise(typed) {
        Some(suggestion) => format!("{NAME_HINT}\n\nTry \u{201c}{suggestion}\u{201d}."),
        None => NAME_HINT.to_owned(),
    }
}

/// The typed text as the nearest usable workspace name, or `None` when there is
/// nothing left to suggest.
///
/// Lowercase, runs of anything-but-`[a-z0-9]` collapsed to one dash, trimmed of
/// leading and trailing dashes, clipped to the length the validator allows —
/// which is exactly the shape [`systemd::is_valid_workspace_name`] accepts, so
/// the suggestion is always one the field will take.
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
    // 32 is the validator's cap; clipping can leave a trailing dash, so trim
    // again rather than assume.
    let clipped = trimmed.get(..trimmed.len().min(32)).unwrap_or(trimmed);
    let candidate = clipped.trim_matches('-');
    (!candidate.is_empty() && candidate != typed).then(|| candidate.to_owned())
}
/// One app in a card's stack row.
///
/// Identified by its `app_id` (what niri reports per window, and what #1071
/// §3.2 makes the stack's stored identity) rather than by window, so N windows
/// of one app are one icon.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StackApp {
    /// The Wayland app-id, resolved to an icon and a display name at render
    /// time. Compositor-supplied, so never rendered as markup.
    app_id: String,
    /// At least one window of this app is open on the card's workspace. A saved
    /// stack whose app is not running renders dim (#1071 §5).
    running: bool,
}

/// What a card is, and therefore what buttons it gets (#1071 §3.7/§5).
#[derive(Clone, Debug, Eq, PartialEq)]
enum Kind {
    /// A stack in `workspaces.toml`. Start/Stop, by state.
    Saved(StackState),
    /// A niri workspace with windows on it and no name — the "save current"
    /// case, which §3.7 settles as a card like any other rather than a `+`
    /// button. Its only action is Edit → Save, which is what creates its entry.
    ///
    /// Carries the niri workspace id because Save has to **name that
    /// workspace**, not merely write a file: §3.7's *"the batch names the niri
    /// workspace immediately, so the saved workspace is the Active card"*.
    Ephemeral { workspace: u64 },
}

/// One card.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Card {
    /// The name shown, and — for a saved card — the stack's identity.
    /// Empty for an ephemeral card, which has none until it is saved.
    name: String,
    kind: Kind,
    /// The apps on the card's row. For a saved stack: its recorded apps, each
    /// lit or dim by whether a window of it is open. For an ephemeral card: the
    /// live windows, always lit.
    apps: Vec<StackApp>,
}

impl Card {
    /// Whether a Start/Stop button acts, and what it says.
    fn is_active(&self) -> bool {
        matches!(self.kind, Kind::Saved(StackState::Active))
    }
}

/// One column. Usually a monitor; the last may be the "not connected" one.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Column {
    /// The output's connector, e.g. `"DP-1"`, or [`OFFLINE_COLUMN`].
    connector: String,
    /// `true` for the trailing column holding stacks whose recorded monitor is
    /// not connected (#1071 §5/§7).
    offline: bool,
    cards: Vec<Card>,
}

/// Heading of the trailing column for stacks whose monitor is absent.
const OFFLINE_COLUMN: &str = "Not connected";

/// The page's whole model, from one snapshot. Pure — no GTK, no registry.
///
/// Three sources, and the join between them is the substance of phase 2:
///
/// * **the file** (`saved`) decides which cards exist and in what order; a
///   stack is a card whether or not it is on a screen, which is what "Inactive
///   cards keep their place" means (#1071 §3.6).
/// * **niri** decides which of them are Active, which apps are lit, and which
///   *unnamed* workspaces get an ephemeral card (§3.7).
/// * **systemd** (`slices_up`) is the second half of the Active derivation, and
///   `starting` outranks both while a Start is in flight (§3.3).
///
/// Column placement: a saved stack goes to its recorded monitor when that is
/// connected, to the workspace it is currently on when it is Active with none
/// recorded, and otherwise — a stack with no monitor that is not running — to
/// the focused output, because that is where a Start would put it. A stack
/// whose recorded monitor is *absent* goes to the trailing offline column
/// rather than somewhere misleading.
fn model(
    workspaces: &[Workspace],
    windows: &[Window],
    saved: &Workspaces,
    slices_up: &BTreeSet<String>,
    starting: &BTreeSet<String>,
) -> Vec<Column> {
    let connected: BTreeSet<&str> = workspaces
        .iter()
        .filter_map(|w| w.output.as_deref())
        .collect();
    if connected.is_empty() {
        return Vec::new();
    }
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .and_then(|w| w.output.as_deref());

    // `BTreeMap` rather than a sort afterwards: the connector ordering *is* the
    // column ordering, so putting it in the collection type means no later edit
    // can drop the sort without also losing the grouping.
    let mut by_output: BTreeMap<&str, Vec<Card>> = connected
        .iter()
        .map(|connector| ((*connector), Vec::new()))
        .collect();
    let mut offline: Vec<Card> = Vec::new();

    // Saved stacks first, in the file's order, so an Inactive one keeps its
    // place among the Active ones.
    for name in saved.names_in_order() {
        let Some(stack) = saved.stacks.get(&name) else {
            continue;
        };
        let live = named_workspace(workspaces, &name);
        let state = state_of(
            &name,
            workspaces,
            windows,
            slices_up.contains(&name),
            starting,
        );
        let open: BTreeSet<&str> = live
            .map(|w| open_app_ids(w.id, windows))
            .unwrap_or_default();
        let card = Card {
            name: name.clone(),
            kind: Kind::Saved(state),
            apps: stack
                .apps
                .iter()
                .map(|app| StackApp {
                    app_id: app.id.clone(),
                    running: open.contains(app.id.as_str()),
                })
                .collect(),
        };

        match stack.monitor.as_deref() {
            Some(monitor) if !connected.contains(monitor) => offline.push(card),
            Some(monitor) => by_output.entry(monitor).or_default().push(card),
            None => {
                let here = live
                    .and_then(|w| w.output.as_deref())
                    .or(focused)
                    .unwrap_or_else(|| connected.iter().next().copied().unwrap_or_default());
                by_output.entry(here).or_default().push(card);
            }
        }
    }

    // Then the ephemeral ones — an unnamed workspace with windows on it. An
    // unnamed *empty* workspace is niri's trailing spare, not a card; a named
    // workspace that no stack knows about is the user's own and is left alone.
    let mut ephemeral: Vec<&Workspace> = workspaces
        .iter()
        .filter(|w| w.name.is_none())
        .filter(|w| windows.iter().any(|win| win.workspace_id == Some(w.id)))
        .collect();
    ephemeral.sort_by_key(|w| w.idx);
    for workspace in ephemeral {
        let Some(output) = workspace.output.as_deref() else {
            continue;
        };
        by_output.entry(output).or_default().push(Card {
            name: String::new(),
            kind: Kind::Ephemeral {
                workspace: workspace.id,
            },
            // `ordered_app_ids`, **not** `open_app_ids`: this list is what a
            // Save records, and #1071 §3.4 step 3 makes the stack's order be
            // niri's column order. Collecting through a `BTreeSet` here (as this
            // did) silently sorted it lexicographically, which is the input
            // phase 3's `MoveColumnToIndex` would then have restored wrongly.
            apps: ordered_app_ids(workspace.id, windows)
                .into_iter()
                .map(|app_id| StackApp {
                    app_id: app_id.to_owned(),
                    running: true,
                })
                .collect(),
        });
    }

    let mut columns: Vec<Column> = by_output
        .into_iter()
        .map(|(connector, cards)| Column {
            connector: connector.to_owned(),
            offline: false,
            cards,
        })
        .collect();
    if !offline.is_empty() {
        columns.push(Column {
            connector: OFFLINE_COLUMN.to_owned(),
            offline: true,
            cards: offline,
        });
    }
    columns
}

/// The workspace carrying `name`, matched the way niri matches it — case
/// insensitively. Mirrors `workspace_stacks`' own lookup for the same reason.
fn named_workspace<'w>(workspaces: &'w [Workspace], name: &str) -> Option<&'w Workspace> {
    workspaces.iter().find(|w| {
        w.name
            .as_ref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// **Membership only**: which app-ids have a window on `workspace_id`.
///
/// A set, deliberately — its one caller asks "is this saved app running?", which
/// is a lookup and not an ordering. Anything that needs the order calls
/// [`ordered_app_ids`] directly; routing an ordered list through here is how the
/// ephemeral card's column order got silently sorted alphabetically.
fn open_app_ids(workspace_id: u64, windows: &[Window]) -> BTreeSet<&str> {
    ordered_app_ids(workspace_id, windows).into_iter().collect()
}

/// As [`open_app_ids`], but keeping niri's column order — what an ephemeral
/// card's row shows, and what a Save records as the stack order (#1071 §3.4
/// step 3 makes that order niri's column order, so recording it in column order
/// is what makes a Start reproduce what was saved).
fn ordered_app_ids(workspace_id: u64, windows: &[Window]) -> Vec<&str> {
    let mut on_workspace: Vec<&Window> = windows
        .iter()
        .filter(|w| w.workspace_id == Some(workspace_id))
        .collect();
    // `pos_in_scrolling_layout` is `(column, tile)`, 1-based, and `None` for a
    // floating window — those sort last, then by id so the order is total and
    // does not wobble between two otherwise-equal windows.
    on_workspace.sort_by_key(|w| {
        (
            w.layout
                .pos_in_scrolling_layout
                .unwrap_or((usize::MAX, usize::MAX)),
            w.id,
        )
    });

    let mut seen: HashSet<&str> = HashSet::new();
    on_workspace
        .into_iter()
        .filter_map(|w| {
            let app_id = w.app_id.as_deref()?;
            seen.insert(app_id).then_some(app_id)
        })
        .collect()
}

pub fn panel_workspaces() -> gtk::Widget {
    build_panel(
        niri::workspaces(),
        niri::windows(),
        config_workspaces::signal(),
        workspace_stacks::slices_up(),
        workspace_stacks::starting(),
    )
}

/// [`panel_workspaces`] with every source injected, so a test can drive the page
/// without a registered `Registry`.
fn build_panel<W, N, S, U, T>(
    workspaces: W,
    windows: N,
    saved: S,
    slices_up: U,
    starting: T,
) -> gtk::Widget
where
    W: Signal<Item = Vec<Workspace>> + 'static,
    N: Signal<Item = Vec<Window>> + 'static,
    S: Signal<Item = Workspaces> + 'static,
    U: Signal<Item = BTreeSet<String>> + 'static,
    T: Signal<Item = BTreeSet<String>> + 'static,
{
    let columns_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    columns_box.add_css_class("ts-ws-columns");
    columns_box.set_homogeneous(true);
    columns_box.set_valign(gtk::Align::Fill);
    columns_box.set_vexpand(true);

    let combined = map_ref! {
        let workspaces = workspaces,
        let windows = windows,
        let saved = saved,
        let slices_up = slices_up,
        let starting = starting =>
            model(workspaces, windows, saved, slices_up, starting)
    }
    // `Window` does not derive `PartialEq` (only `Workspace` does), so the
    // *inputs* cannot be deduped — but the model can, and it is what the
    // rebuild costs. Without this every window-title change on any workspace
    // tears down and rebuilds every card on every monitor. It also matters more
    // in phase 2 than in phase 1: a rebuild now discards a half-typed name in an
    // ephemeral card's Save field, and the slice poll ticks every three seconds.
    .dedupe_cloned();

    bind_columns(&columns_box, combined);

    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.add_css_class("ts-ws-page");
    column.append(&columns_box);

    // One column per monitor inside the 680px default clamp would squeeze a
    // two-screen setup to ~330px a column — the same measurement that put the
    // Stats multicolumn page on the wide clamp (#508).
    finish_page_clamped(&column, DRAWER_MAX_WIDTH_WIDE)
}

/// Rebuild the per-monitor columns from `model` into `columns_box`.
///
/// Split out of [`build_panel`] so this `bind` call site can be driven with a
/// synthetic signal in tests, the same way `panels::bluetooth`'s groups are
/// (#772). The apply closure takes its container from `bind` rather than
/// capturing a strong clone — #224's `WeakRef` contract, which `nix`'s
/// `bind-pins` check (#831) enforces at the source level.
fn bind_columns<S>(columns_box: &gtk::Box, model: S)
where
    S: Signal<Item = Vec<Column>> + 'static,
{
    // One cache for the whole page: an app on two workspaces costs one
    // `AppInfo::all()` scan, not one per card. Lives as long as the binding.
    let meta_cache: MetaCache = Rc::new(RefCell::new(HashMap::new()));
    bind(model, columns_box, move |columns_box, columns| {
        while let Some(child) = columns_box.first_child() {
            columns_box.remove(&child);
        }
        if columns.is_empty() {
            columns_box.append(&hint(NO_OUTPUTS_HINT));
            return;
        }
        for column in &columns {
            columns_box.append(&build_column(column, &meta_cache));
        }
    });
}

fn build_column(column: &Column, meta_cache: &MetaCache) -> gtk::Widget {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 8);
    outer.add_css_class("ts-ws-column");
    outer.set_hexpand(true);
    outer.set_valign(gtk::Align::Start);

    // A plain `gtk::Label` has `use-markup` off, so a connector (or, below, a
    // workspace name or an app id — all three compositor-supplied) cannot
    // inject Pango markup the way an `AdwPreferencesGroup` title would
    // (#30/#753).
    let title = gtk::Label::new(Some(&column.connector));
    title.add_css_class("ts-ws-column-title");
    title.set_xalign(0.0);
    title.set_ellipsize(pango::EllipsizeMode::End);
    if column.offline {
        title.add_css_class("ts-ws-column-offline");
        title.set_tooltip_text(Some(OFFLINE_COLUMN_HINT));
    }
    outer.append(&title);

    let cards = gtk::Box::new(gtk::Orientation::Vertical, 8);
    cards.set_valign(gtk::Align::Start);
    if column.cards.is_empty() {
        cards.append(&hint(EMPTY_COLUMN_HINT));
    } else {
        for card in &column.cards {
            cards.append(&build_card(card, meta_cache));
        }
    }

    // The card list scrolls (phase 1 review, LOW-2). In phase 1 the column held
    // one card per *named* niri workspace, so overflow was academic; §3.7 makes
    // every workspace a card and §5 puts a button on each, so the cards are both
    // taller and far more numerous — and the drawer *clips* what does not fit
    // (`modal.rs`: "the fullscreen drawer surface clips whatever still doesn't
    // fit"), which would make a card unreachable with no affordance that it
    // exists. `Never` horizontally so a long workspace name ellipsizes the way
    // it already did rather than growing a second scrollbar.
    //
    // `max_content_height` is what makes any of that happen. With
    // `propagate_natural_height` and no cap, the scroller requests its whole
    // content height and the drawer grants it — so the scrollbar never appears
    // and the clipping is exactly as it was. The first cut of this fix had no
    // cap and passed only because its test supplied a 220 px window, which the
    // drawer never does. `scale()`d and `connections.rs`-shaped: this is an
    // inside-card list scroller, so the cap is meant to grow with the font,
    // unlike the Stats viewport's real-pixel screen budget (#787).
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(crate::scale::scale(COLUMN_MAX_HEIGHT))
        .vexpand(true)
        .child(&cards)
        .build();
    scroller.add_css_class("ts-ws-scroller");
    outer.append(&scroller);

    outer.upcast()
}

fn build_card(card: &Card, meta_cache: &MetaCache) -> gtk::Widget {
    // `.ts-panel` is the shell's card surface (`components::layout::section`
    // paints the same one); `.ts-ws-card` is the hook for this page's own
    // spacing and for the greying of a whole Inactive card.
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.add_css_class("ts-panel");
    outer.add_css_class("ts-ws-card");
    toggle_class(&outer, CARD_INACTIVE_CLASS, !card.is_active());

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);

    let name = gtk::Label::new(Some(if card.name.is_empty() {
        EPHEMERAL_NAME
    } else {
        &card.name
    }));
    name.add_css_class("ts-ws-card-name");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(pango::EllipsizeMode::End);
    header.append(&name);

    match &card.kind {
        Kind::Saved(state) => header.append(&start_stop_button(&card.name, *state)),
        Kind::Ephemeral { .. } => {}
    }
    outer.append(&header);

    if let Kind::Saved(StackState::Inactive) = card.kind {
        let note = hint(INACTIVE_HINT);
        note.add_css_class("ts-ws-card-note");
        outer.append(&note);
    }

    let apps = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    apps.add_css_class("ts-ws-apps");
    for app in &card.apps {
        apps.append(&build_app_icon(app, meta_cache));
    }
    outer.append(&apps);

    if let Kind::Ephemeral { workspace } = card.kind {
        outer.append(&save_row(card, workspace));
    }

    outer.upcast()
}

/// The one button a saved card carries, by state (#1071 §5).
fn start_stop_button(name: &str, state: StackState) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("ts-ws-action");
    match state {
        StackState::Starting => {
            // A spinner rather than a label, and insensitive: a second click is
            // not a second Start, and `spawn_start` refuses one anyway — this is
            // so the user is not left wondering whether the first one took.
            let spinner = gtk::Spinner::new();
            spinner.start();
            button.set_child(Some(&spinner));
            button.set_tooltip_text(Some("Starting…"));
            button.set_sensitive(false);
        }
        StackState::Active => {
            button.set_icon_name("media-playback-stop-symbolic");
            button.set_tooltip_text(Some("Stop this workspace"));
            let name = name.to_owned();
            button.connect_clicked(move |_| workspace_stacks::spawn_stop(name.clone()));
        }
        StackState::Inactive => {
            button.set_icon_name("media-playback-start-symbolic");
            button.set_tooltip_text(Some("Start this workspace"));
            let name = name.to_owned();
            button.connect_clicked(move |_| start_by_name(&name));
        }
    }
    button
}

/// Start the stack `name`, reading it out of the file at click time.
///
/// The name came off a card the model built, so the stack is read back rather
/// than captured: the file is live-reloaded and the drawer can sit open across
/// an edit, so a captured `Stack` could launch a stale app list.
fn start_by_name(name: &str) {
    let saved = config_workspaces::current();
    let Some(stack) = saved.stacks.get(name).cloned() else {
        tracing::warn!(workspace = name, "no such stack in workspaces.toml");
        return;
    };
    workspace_stacks::spawn_start(name.to_owned(), stack, saved.stacks);
}

/// The ephemeral card's Edit → Save (#1071 §3.7).
///
/// Deliberately **not** a `+` button, and deliberately not the phase-4 edit
/// sub-page either: §3.7 settles that an unnamed workspace is a card like any
/// other and that Save is what creates its entry. Phase 2's Save takes the
/// **name only** — the apps come from what is on the workspace right now, and
/// everything else is defaulted, since the monitor is set by dragging (phase 3)
/// and the layout/autostart fields live in the edit form (phase 4).
///
/// # Two checks here, one on the runtime
///
/// The two the field can answer **now** stay here, because a red field beside
/// the cursor is a better correction surface than a toast: the name has to be a
/// usable workspace name, and it has to be one no *stack* already has. The third
/// — that no niri **workspace** already carries it — needs a socket round trip,
/// so it lives in [`workspace_stacks::save`] along with the write and the
/// `SetWorkspaceName` that makes the saved workspace *be* this one.
fn save_row(card: &Card, workspace: u64) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-ws-save");

    let entry = gtk::Entry::builder()
        .placeholder_text(SAVE_PLACEHOLDER)
        .hexpand(true)
        .max_length(32)
        .build();
    entry.add_css_class("ts-ws-save-entry");
    row.append(&entry);

    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.add_css_class("ts-ws-save-button");
    row.append(&save);

    // Clear the red as soon as the user starts correcting. Without this the
    // field stays red through every keystroke of the fix, since the class was
    // only ever removed on a *successful* commit.
    entry.connect_changed(|entry| {
        entry.remove_css_class("error");
        entry.set_tooltip_text(None);
    });

    let apps = card.apps.clone();
    let commit = move |entry: &gtk::Entry| {
        let typed = entry.text().to_string();
        let reject = |entry: &gtk::Entry, why: &str| {
            entry.add_css_class("error");
            entry.set_tooltip_text(Some(why));
        };
        let Some(name) = systemd::normalize_workspace_name(&typed) else {
            // `normalize` deliberately refuses rather than rewrites anything but
            // case, so §3.1's "offers the sanitised form" is offered here — the
            // field keeps what was typed and the tooltip carries the suggestion,
            // which is a correction the user can accept or ignore rather than a
            // silent rewrite of a name they will later type at `niri msg`.
            reject(entry, &name_hint(&typed));
            return;
        };
        if config_workspaces::current().stacks.contains_key(&name) {
            reject(
                entry,
                &format!("A workspace called \u{201c}{name}\u{201d} already exists."),
            );
            return;
        }
        entry.remove_css_class("error");
        entry.set_tooltip_text(None);
        let stack = Stack {
            apps: apps
                .iter()
                .map(|app| SavedApp {
                    id: app.app_id.clone(),
                    exec: None,
                })
                .collect(),
            ..Stack::default()
        };
        // Off to the runtime: the remaining checks and the write are I/O, and
        // the `SetWorkspaceName` that turns this ephemeral card into the saved
        // one is a niri round trip. Nothing to redraw by hand — the file poll
        // and the niri event stream both republish, and the card comes back as
        // one Active saved card rather than two.
        workspace_stacks::spawn_save(workspace, name, stack);
    };

    // Enter in the field and the button do the same thing. `connect_activate`
    // takes the entry from GTK; the button's handler holds a weak reference, so
    // the row is not pinned by its own callback.
    entry.connect_activate({
        let commit = commit.clone();
        move |entry| commit(entry)
    });
    let weak = entry.downgrade();
    save.connect_clicked(move |_| {
        if let Some(entry) = weak.upgrade() {
            commit(&entry);
        }
    });

    row.upcast()
}

fn build_app_icon(app: &StackApp, meta_cache: &MetaCache) -> gtk::Image {
    // Resolve into locals first: an argument-position `borrow_mut()` lives for
    // the whole statement, so inlining this into the GTK calls below would hold
    // the `RefMut` across a call that can synchronously re-enter (#643/#663/
    // #832 — a `BorrowMutError` through a glib callback aborts the process).
    let meta = resolve_app_meta(&app.app_id, &mut meta_cache.borrow_mut());
    let (icon, tooltip) = meta.map_or_else(
        || (fallback_icon(), app.app_id.clone()),
        |m| (m.icon.unwrap_or_else(fallback_icon), m.display_name),
    );

    let img = gtk::Image::from_gicon(&icon);
    img.set_icon_size(gtk::IconSize::Normal);
    img.set_valign(gtk::Align::Center);
    // Plain text, not markup — see `build_column`.
    img.set_tooltip_text(Some(&tooltip));
    img.add_css_class("ts-ws-app");
    apply_running_class(&img, app.running);
    img
}

/// The glow/dim flip. A saved stack's app with no window open is this, called
/// with `false`.
fn apply_running_class(icon: &gtk::Image, running: bool) {
    toggle_class(icon, APP_RUNNING_CLASS, running);
    toggle_class(icon, APP_IDLE_CLASS, !running);
}

fn hint(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("ts-ws-empty");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label
}

/// The niri fixtures both test modules build snapshots from. A module of its
/// own rather than a `mod tests` helper because the pure model tests and the
/// GTK tests are separately `#[cfg]`-gated and each needs them.
#[cfg(test)]
mod fixtures {
    use crate::config::workspaces::{Stack, StackApp, Workspaces};
    use hytte::services::niri::{Window, WindowLayout, Workspace};
    use std::collections::BTreeMap;

    pub(super) const LEFT: &str = "DP-1";
    pub(super) const RIGHT: &str = "HDMI-A-1";

    /// A workspace on `output`, named iff `name` is `Some`. Not focused.
    pub(super) fn ws(id: u64, idx: u8, output: &str, name: Option<&str>) -> Workspace {
        Workspace {
            id,
            idx,
            name: name.map(str::to_owned),
            output: Some(output.to_owned()),
            is_urgent: false,
            is_active: false,
            is_focused: false,
            active_window_id: None,
        }
    }

    /// [`ws`], focused. Which output is focused decides where a stack with no
    /// recorded monitor shows, so it is a fixture knob rather than a constant.
    pub(super) fn ws_focused(id: u64, idx: u8, output: &str, name: Option<&str>) -> Workspace {
        Workspace {
            is_active: true,
            is_focused: true,
            ..ws(id, idx, output, name)
        }
    }

    /// A tiled window of `app_id` in column `column` of workspace `workspace`.
    pub(super) fn win(id: u64, workspace: u64, app_id: &str, column: usize) -> Window {
        Window {
            id,
            title: None,
            app_id: Some(app_id.to_owned()),
            pid: None,
            workspace_id: Some(workspace),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((column, 1)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    /// A saved stack: its apps, and optionally the monitor it records.
    pub(super) fn stack(monitor: Option<&str>, apps: &[&str]) -> Stack {
        Stack {
            monitor: monitor.map(str::to_owned),
            apps: apps
                .iter()
                .map(|id| StackApp {
                    id: (*id).to_owned(),
                    exec: None,
                })
                .collect(),
            ..Stack::default()
        }
    }

    /// A `workspaces.toml` with these stacks, in this order.
    pub(super) fn saved(stacks: &[(&str, Stack)]) -> Workspaces {
        Workspaces {
            order: stacks.iter().map(|(name, _)| (*name).to_owned()).collect(),
            stacks: stacks
                .iter()
                .map(|(name, stack)| ((*name).to_owned(), stack.clone()))
                .collect(),
        }
    }

    /// No stacks saved at all — the shipped state, and what an ephemeral-only
    /// page is built from.
    pub(super) fn no_stacks() -> Workspaces {
        Workspaces {
            order: Vec::new(),
            stacks: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod model_tests {
    use super::fixtures::{LEFT, RIGHT, no_stacks, saved, stack, win, ws, ws_focused};
    use super::{Card, Column, Kind, StackState, model};
    use crate::config::workspaces::Workspaces;
    use std::collections::BTreeSet;

    /// The model with nothing in flight and no slice up — the common case.
    fn built(
        workspaces: &[hytte::services::niri::Workspace],
        windows: &[hytte::services::niri::Window],
        file: &Workspaces,
    ) -> Vec<Column> {
        model(
            workspaces,
            windows,
            file,
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
    }

    fn connectors(columns: &[Column]) -> Vec<&str> {
        columns.iter().map(|c| c.connector.as_str()).collect()
    }

    fn card_names(column: &Column) -> Vec<&str> {
        column.cards.iter().map(|c| c.name.as_str()).collect()
    }

    fn app_ids(card: &Card) -> Vec<&str> {
        card.apps.iter().map(|a| a.app_id.as_str()).collect()
    }

    fn find<'c>(columns: &'c [Column], connector: &str) -> &'c Column {
        columns
            .iter()
            .find(|c| c.connector == connector)
            .unwrap_or_else(|| panic!("no column for {connector}"))
    }

    /// One column per connected output, ordered by connector — unchanged from
    /// phase 1, but now driven by the *connected set* rather than by which
    /// outputs happen to carry a named workspace, since a saved stack must have
    /// a column to sit in even when nothing of it is running.
    ///
    /// Falsified by collapsing the fold to a single column.
    #[test]
    fn one_column_per_output_in_connector_order() {
        let columns = built(
            &[ws(1, 1, RIGHT, None), ws(2, 1, LEFT, None)],
            &[],
            &no_stacks(),
        );
        assert_eq!(connectors(&columns), [LEFT, RIGHT]);
    }

    /// A saved stack is a card whether or not it is running — that is what
    /// "Inactive cards keep their place" means (#1071 §3.6) — and the file's
    /// order is the card order.
    #[test]
    fn every_saved_stack_is_a_card_in_the_files_order() {
        let file = saved(&[
            ("music", stack(Some(LEFT), &["mpv"])),
            ("chat", stack(Some(LEFT), &["firefox"])),
        ]);
        let columns = built(&[ws(1, 1, LEFT, None)], &[], &file);
        assert_eq!(
            card_names(find(&columns, LEFT)),
            ["music", "chat"],
            "the file's order, not alphabetical"
        );
        assert!(
            columns[0]
                .cards
                .iter()
                .all(|c| c.kind == Kind::Saved(StackState::Inactive)),
            "nothing is running"
        );
    }

    /// §7's state rows, on the page rather than on `state_of`: a stack with
    /// windows on its named workspace is Active, one with a slice up is Active,
    /// one in flight is Starting, and the rest are Inactive.
    #[test]
    fn a_cards_state_comes_from_the_windows_the_slice_and_the_in_flight_set() {
        let file = saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            ("dev", stack(Some(LEFT), &["Alacritty"])),
            ("music", stack(Some(LEFT), &["mpv"])),
            ("gone", stack(Some(LEFT), &["x"])),
        ]);
        let workspaces = [
            ws(1, 1, LEFT, Some("chat")),
            ws(2, 2, LEFT, Some("dev")),
            ws(3, 3, LEFT, Some("music")),
        ];
        let columns = model(
            &workspaces,
            &[win(9, 1, "firefox", 1)],
            &file,
            // `dev`'s units are up although its windows are gone.
            &BTreeSet::from(["dev".to_owned()]),
            &BTreeSet::from(["music".to_owned()]),
        );
        let kinds: Vec<&Kind> = find(&columns, LEFT).cards.iter().map(|c| &c.kind).collect();
        assert_eq!(
            kinds,
            [
                &Kind::Saved(StackState::Active),   // windows
                &Kind::Saved(StackState::Active),   // units
                &Kind::Saved(StackState::Starting), // in flight
                &Kind::Saved(StackState::Inactive), // no workspace at all
            ]
        );
    }

    /// A saved stack's row shows **its recorded apps**, lit or dim by whether a
    /// window of that app is open — which is the phase-1 glow field finally
    /// doing something (#1071 §5).
    ///
    /// Falsified by lighting every icon: `mpv` would read as running.
    #[test]
    fn a_saved_cards_apps_are_the_files_apps_lit_by_the_open_windows() {
        let file = saved(&[("chat", stack(Some(LEFT), &["firefox", "mpv"]))]);
        let columns = built(
            &[ws(1, 1, LEFT, Some("chat"))],
            &[win(9, 1, "firefox", 1)],
            &file,
        );
        let card = &find(&columns, LEFT).cards[0];
        assert_eq!(
            app_ids(card),
            ["firefox", "mpv"],
            "the file's list, in order"
        );
        assert_eq!(
            card.apps.iter().map(|a| a.running).collect::<Vec<_>>(),
            [true, false],
            "lit by what is actually open"
        );
    }

    /// §3.7: an unnamed workspace **with windows on it** is an ephemeral card,
    /// carrying its live apps in niri's column order.
    #[test]
    fn an_unnamed_workspace_with_windows_is_an_ephemeral_card() {
        let columns = built(
            &[ws(1, 1, LEFT, None)],
            &[win(9, 1, "mpv", 2), win(10, 1, "firefox", 1)],
            &no_stacks(),
        );
        let cards = &find(&columns, LEFT).cards;
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].kind, Kind::Ephemeral { workspace: 1 });
        assert_eq!(cards[0].name, "", "it has no name until it is saved");
        assert_eq!(
            app_ids(&cards[0]),
            ["firefox", "mpv"],
            "niri's column order, which is the order a Save records"
        );
    }

    /// …but an unnamed **empty** workspace is niri's trailing spare, not a
    /// card. Without this every screen would carry a permanent blank card, and
    /// the one a Start is about to adopt would be offered for saving.
    ///
    /// Falsified by dropping the has-windows filter.
    #[test]
    fn an_empty_unnamed_workspace_is_not_a_card() {
        let columns = built(&[ws(1, 1, LEFT, None)], &[], &no_stacks());
        assert!(find(&columns, LEFT).cards.is_empty());
    }

    /// A *named* workspace no stack knows about is the user's own — niri
    /// `set-workspace-name`, a keybind — and the page leaves it alone rather
    /// than offering to adopt it.
    #[test]
    fn a_workspace_named_by_hand_is_not_a_card() {
        let columns = built(
            &[ws(1, 1, LEFT, Some("scratch"))],
            &[win(9, 1, "firefox", 1)],
            &no_stacks(),
        );
        assert!(find(&columns, LEFT).cards.is_empty());
    }

    /// Placement: a stack's recorded monitor decides its column.
    #[test]
    fn a_stack_sits_in_the_column_of_the_monitor_it_records() {
        let file = saved(&[
            ("chat", stack(Some(RIGHT), &["firefox"])),
            ("dev", stack(Some(LEFT), &["Alacritty"])),
        ]);
        let columns = built(
            &[ws_focused(1, 1, LEFT, None), ws(2, 1, RIGHT, None)],
            &[],
            &file,
        );
        assert_eq!(card_names(find(&columns, LEFT)), ["dev"]);
        assert_eq!(card_names(find(&columns, RIGHT)), ["chat"]);
    }

    /// A stack with **no** recorded monitor follows the workspace it is on when
    /// it is Active, and the focused output otherwise — which is where a Start
    /// would put it, so the card is where the action would land.
    #[test]
    fn a_stack_with_no_monitor_follows_its_workspace_then_the_focus() {
        let file = saved(&[
            ("chat", stack(None, &["firefox"])),
            ("dev", stack(None, &["Alacritty"])),
        ]);
        let columns = built(
            &[
                ws_focused(1, 1, LEFT, None),
                // `chat` is live, on the *other* screen.
                ws(2, 1, RIGHT, Some("chat")),
            ],
            &[win(9, 2, "firefox", 1)],
            &file,
        );
        assert_eq!(
            card_names(find(&columns, RIGHT)),
            ["chat"],
            "an Active stack shows where it actually is"
        );
        assert_eq!(
            card_names(find(&columns, LEFT)),
            ["dev"],
            "an Inactive one shows where a Start would put it"
        );
    }

    /// §5/§7: a stack whose recorded monitor is **not connected** goes to a
    /// trailing greyed column of its own, rather than to a screen it does not
    /// name.
    ///
    /// Falsified by placing it on the focused output: `chat` would join `dev`
    /// on DP-1 and the offline column would never exist.
    #[test]
    fn a_stack_whose_monitor_is_absent_goes_to_the_trailing_column() {
        let file = saved(&[
            ("chat", stack(Some("DP-9"), &["firefox"])),
            ("dev", stack(Some(LEFT), &["Alacritty"])),
        ]);
        let columns = built(&[ws_focused(1, 1, LEFT, None)], &[], &file);
        assert_eq!(connectors(&columns), [LEFT, super::OFFLINE_COLUMN]);
        assert!(
            columns.last().expect("a trailing column").offline,
            "and it is flagged, so the view can grey it"
        );
        assert_eq!(card_names(&columns[1]), ["chat"]);
        assert_eq!(card_names(&columns[0]), ["dev"]);
    }

    /// Saved cards come before ephemeral ones in a column: the saved order is
    /// the user's, and an unnamed workspace has no place in it.
    #[test]
    fn ephemeral_cards_come_after_the_saved_ones() {
        let file = saved(&[("chat", stack(Some(LEFT), &["firefox"]))]);
        let columns = built(
            &[ws(1, 1, LEFT, Some("chat")), ws(2, 2, LEFT, None)],
            &[win(9, 2, "mpv", 1)],
            &file,
        );
        let cards = &find(&columns, LEFT).cards;
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].name, "chat");
        assert_eq!(cards[1].kind, Kind::Ephemeral { workspace: 2 });
    }

    /// **MEDIUM-7.** An ephemeral card's apps are in **niri's column order**,
    /// not alphabetical.
    ///
    /// This list is what a Save records, and #1071 §3.4 step 3 makes the stack's
    /// order *be* niri's column order — so phase 3's `MoveColumnToIndex` would
    /// restore whatever this gets wrong. Routing it through a `BTreeSet` (as it
    /// did) sorted it lexicographically with nothing to notice.
    ///
    /// The app ids are chosen so lexicographic and column order **disagree**:
    /// `mpv` sits at column 1 and `a-editor` at column 2, so an alphabetical
    /// answer is `[a-editor, mpv]` and the right one is `[mpv, a-editor]`.
    /// Two mutations red here and nowhere else:
    ///
    /// * **J:** delete `ordered_app_ids`' `sort_by_key(pos_in_scrolling_layout)`.
    /// * **K:** reverse the ordered list before it is used.
    #[test]
    fn an_ephemeral_cards_apps_follow_niris_columns_not_the_alphabet() {
        let columns = built(
            &[ws(1, 1, LEFT, None)],
            &[win(9, 1, "mpv", 1), win(10, 1, "a-editor", 2)],
            &no_stacks(),
        );
        assert_eq!(
            app_ids(&find(&columns, LEFT).cards[0]),
            ["mpv", "a-editor"],
            "column order, which is the order a Save records"
        );

        // The same two windows with their columns swapped must come out the
        // other way round — so the assertion above cannot be satisfied by any
        // fixed ordering, alphabetical or otherwise.
        let swapped = built(
            &[ws(1, 1, LEFT, None)],
            &[win(9, 1, "mpv", 2), win(10, 1, "a-editor", 1)],
            &no_stacks(),
        );
        assert_eq!(app_ids(&find(&swapped, LEFT).cards[0]), ["a-editor", "mpv"]);
    }

    /// Two windows of one app are one icon, and a window with no `app_id`
    /// contributes none — there is nothing to resolve a desktop entry from.
    #[test]
    fn an_ephemeral_cards_apps_are_deduped_and_skip_anonymous_windows() {
        let mut anonymous = win(1, 1, "x", 1);
        anonymous.app_id = None;
        let columns = built(
            &[ws(1, 1, LEFT, None)],
            &[
                anonymous,
                win(2, 1, "firefox", 2),
                win(3, 1, "firefox", 3),
                win(4, 1, "mpv", 4),
            ],
            &no_stacks(),
        );
        assert_eq!(app_ids(&find(&columns, LEFT).cards[0]), ["firefox", "mpv"]);
    }

    /// §3.1's "offer the sanitised form": the suggestion is always one the
    /// validator would accept, and there is none when nothing is left.
    #[test]
    fn the_save_field_suggests_a_name_the_validator_would_take() {
        for typed in [
            "Chat Room",
            "  dev/2  ",
            "my_stack",
            "--weird--",
            "Ünïcödé chat",
            &"x".repeat(80),
        ] {
            let suggestion = super::sanitise(typed)
                .unwrap_or_else(|| panic!("{typed:?} should still suggest something"));
            assert!(
                hytte::services::systemd::is_valid_workspace_name(&suggestion),
                "{typed:?} suggested {suggestion:?}, which the field would refuse"
            );
        }
        assert_eq!(
            super::sanitise("chat"),
            None,
            "already usable, nothing to say"
        );
        assert_eq!(super::sanitise("---"), None, "nothing survives");
        assert_eq!(super::sanitise(""), None);
        assert_eq!(super::sanitise("Chat Room").as_deref(), Some("chat-room"));
    }

    /// No outputs at all → no columns, so the page can say it is waiting for
    /// niri instead of rendering an empty frame.
    #[test]
    fn no_connected_outputs_produce_no_columns() {
        let mut orphan = ws(1, 1, LEFT, Some("chat"));
        orphan.output = None;
        assert!(built(&[orphan], &[], &no_stacks()).is_empty());
        assert!(built(&[], &[], &saved(&[("chat", stack(None, &["x"]))])).is_empty());
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::fixtures::{LEFT, RIGHT, no_stacks, saved, stack, win, ws};
    use super::{
        APP_IDLE_CLASS, APP_RUNNING_CLASS, CARD_INACTIVE_CLASS, Column, EMPTY_COLUMN_HINT,
        EPHEMERAL_NAME, NO_OUTPUTS_HINT, OFFLINE_COLUMN, bind_columns, build_panel,
    };
    use crate::config::workspaces::Workspaces;
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, graphene, prelude::*};
    use hytte::services::niri::{Window, Workspace};
    use std::collections::BTreeSet;

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Drive the GTK main loop until `done()` holds, or `ms` of wall clock has
    /// passed.
    ///
    /// Needed on top of [`pump`] for anything that only settles on a **frame**:
    /// appending a child queues an allocation on its parent, and the queue is
    /// drained by the frame clock rather than by an idle. Same helper, same
    /// reason, as `overlays::sidebar`'s scroll tests.
    fn pump_until(ms: u64, done: impl Fn() -> bool) {
        let expired = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = expired.clone();
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            flag.set(true);
        });
        while !expired.get() && !done() {
            gtk::glib::MainContext::default().iteration(true);
        }
    }

    /// Every descendant of `root` carrying `class`, depth-first in tree order.
    ///
    /// The page is `gtk::Box`es all the way down and GTK exposes no "find by
    /// class", so this is how a test reads the rendered structure back — the
    /// same move `reactive_list`'s `row_titles` makes for rows.
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

    fn columns(page: &gtk::Widget) -> Vec<gtk::Widget> {
        by_class(page, "ts-ws-column")
    }

    fn cards(scope: &gtk::Widget) -> Vec<gtk::Widget> {
        by_class(scope, "ts-ws-card")
    }

    fn icons(scope: &gtk::Widget) -> Vec<gtk::Widget> {
        by_class(scope, "ts-ws-app")
    }

    /// The text of the first `gtk::Label` under `scope` carrying `class`.
    fn label_text(scope: &gtk::Widget, class: &str) -> String {
        let label = by_class(scope, class)
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Label>().ok())
            .unwrap_or_else(|| panic!("no .{class} label under the scope"));
        label.text().to_string()
    }

    /// The page, plus every handle that drives it.
    struct Fixture {
        page: gtk::Widget,
        workspaces: Mutable<Vec<Workspace>>,
        windows: Mutable<Vec<Window>>,
        saved: Mutable<Workspaces>,
        slices_up: Mutable<BTreeSet<String>>,
        starting: Mutable<BTreeSet<String>>,
    }

    fn fixture() -> Fixture {
        adw::init().expect("libadwaita init");
        let workspaces: Mutable<Vec<Workspace>> = Mutable::new(Vec::new());
        let windows: Mutable<Vec<Window>> = Mutable::new(Vec::new());
        let saved: Mutable<Workspaces> = Mutable::new(no_stacks());
        let slices_up: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let starting: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let page = build_panel(
            workspaces.signal_cloned(),
            windows.signal_cloned(),
            saved.signal_cloned(),
            slices_up.signal_cloned(),
            starting.signal_cloned(),
        );
        pump();
        Fixture {
            page,
            workspaces,
            windows,
            saved,
            slices_up,
            starting,
        }
    }

    /// Mount `page` in a real window so allocations exist and `pick()` answers.
    fn present(page: &gtk::Widget) -> gtk::Window {
        let window = gtk::Window::new();
        window.set_child(Some(page));
        window.set_default_size(1000, 600);
        window.present();
        pump();
        window
    }

    fn bounds_in(outer: &gtk::Widget, inner: &gtk::Widget, what: &str) -> graphene::Rect {
        inner
            .compute_bounds(outer)
            .unwrap_or_else(|| panic!("{what} has no allocation inside its container"))
    }

    /// `inner`'s rectangle, taken in `outer`'s coordinate space, lies inside
    /// `outer` — and a click at its centre actually lands on it.
    ///
    /// `is_visible()` is orthogonal to being on screen (#851/#838), which is
    /// exactly how a chip once shipped drawn 250px outside its clipping bin
    /// with both geometry tests green. Nothing here reads that flag.
    fn assert_inside_and_hittable(outer: &gtk::Widget, inner: &gtk::Widget, what: &str) {
        // A child appended after the window was presented has no allocation
        // until the next frame, so wait for one rather than reading a
        // guaranteed-empty rectangle. A genuinely zero-area widget still fails
        // the assertion below once the timeout expires.
        pump_until(2000, || {
            inner
                .compute_bounds(outer)
                .is_some_and(|b| b.width() > 0.0 && b.height() > 0.0)
        });
        let rect = bounds_in(outer, inner, what);
        assert!(
            rect.width() > 0.0 && rect.height() > 0.0,
            "{what} allocated a zero-area rectangle: {rect:?}"
        );
        // The container's own rectangle in its own coordinate space — no int
        // casts, and it is the same measurement `rect` is expressed in.
        let frame = bounds_in(outer, outer, "the container itself");
        assert!(
            rect.x() >= frame.x() - 0.5
                && rect.y() >= frame.y() - 0.5
                && rect.x() + rect.width() <= frame.x() + frame.width() + 0.5
                && rect.y() + rect.height() <= frame.y() + frame.height() + 0.5,
            "{what} is drawn outside its container: {rect:?} against {frame:?}"
        );
        let (cx, cy) = (
            f64::from(rect.x() + rect.width() / 2.0),
            f64::from(rect.y() + rect.height() / 2.0),
        );
        assert!(
            outer
                .pick(cx, cy, gtk::PickFlags::DEFAULT)
                .is_some_and(|w| w == *inner || w.is_ancestor(inner)),
            "a click at the centre of {what} does not reach it"
        );
    }

    /// §7: **one column per monitor**, side by side, in connector order — and
    /// each column is actually on screen inside the page, not merely `visible`.
    ///
    /// Falsified by collapsing `model`'s per-output fold to one column.
    #[gtk::test]
    fn one_column_per_monitor() {
        let f = fixture();
        f.workspaces.set(vec![
            ws(1, 1, RIGHT, Some("right-one")),
            ws(2, 1, LEFT, Some("left-one")),
        ]);
        pump();
        let window = present(&f.page);

        let cols = columns(&f.page);
        assert_eq!(cols.len(), 2, "one column per monitor niri reports");
        assert_eq!(label_text(&cols[0], "ts-ws-column-title"), LEFT);
        assert_eq!(label_text(&cols[1], "ts-ws-column-title"), RIGHT);

        let strip = cols[0]
            .parent()
            .expect("a column is parented into the columns row");
        for (i, col) in cols.iter().enumerate() {
            assert_inside_and_hittable(&strip, col, &format!("column {i}"));
        }
        // Side by side, not stacked: the second column starts at or after where
        // the first one ends.
        let first = bounds_in(&strip, &cols[0], "column 0");
        let second = bounds_in(&strip, &cols[1], "column 1");
        assert!(
            second.x() >= first.x() + first.width() - 0.5,
            "the monitors' columns overlap instead of sitting side by side: \
             {first:?} then {second:?}"
        );

        window.destroy();
    }

    /// §7: the cards come from **the file**, and an unnamed workspace is a card
    /// only when something is on it (#1071 §3.7).
    ///
    /// Falsified by dropping `model`'s has-windows filter on the ephemeral
    /// branch: `HDMI-A-1`'s empty spare then renders a card and both counts are
    /// wrong.
    #[gtk::test]
    fn a_saved_stack_is_a_card_and_an_empty_unnamed_workspace_is_not() {
        let f = fixture();
        f.saved.set(saved(&[("chat", stack(Some(LEFT), &["a"]))]));
        f.workspaces.set(vec![
            ws(1, 1, LEFT, Some("chat")),
            ws(2, 2, LEFT, None),
            ws(3, 1, RIGHT, None),
        ]);
        pump();
        let window = present(&f.page);

        let cols = columns(&f.page);
        assert_eq!(cols.len(), 2);

        let left_cards = cards(&cols[0]);
        assert_eq!(
            left_cards.len(),
            1,
            "the empty unnamed workspace on {LEFT} is niri's spare, not a card"
        );
        assert_eq!(label_text(&left_cards[0], "ts-ws-card-name"), "chat");
        assert_inside_and_hittable(&cols[0], &left_cards[0], "the `chat` card");

        assert!(
            cards(&cols[1]).is_empty(),
            "{RIGHT} has no saved stack and nothing running, so it has no cards"
        );
        assert_eq!(
            label_text(&cols[1], "ts-ws-empty"),
            EMPTY_COLUMN_HINT,
            "a column with no cards shows the hint"
        );

        window.destroy();
    }

    /// §7: **the glow follows the windows signal.** A saved stack's icon is lit
    /// when a window of that app is open on its workspace and dim when not, and
    /// the flip is live.
    ///
    /// Falsified by dropping `windows` from `build_panel`'s `map_ref!` (pass
    /// `&[]` to `model` instead): the icon never lights and the first assertion
    /// after the open fails.
    ///
    /// Classes and counts, not resolved icon names: what `resolve_app_meta`
    /// returns depends on which desktop files the harness happens to have
    /// installed. The identity of an app in the model is pinned by
    /// `model_tests` instead.
    #[gtk::test]
    fn a_saved_stacks_icons_light_and_dim_with_its_windows() {
        let f = fixture();
        f.saved.set(saved(&[(
            "dev",
            stack(Some(LEFT), &["com.example.Term", "com.example.Editor"]),
        )]));
        f.workspaces.set(vec![ws(7, 1, LEFT, Some("dev"))]);
        pump();
        let window = present(&f.page);

        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        let dark = icons(&card);
        assert_eq!(dark.len(), 2, "a saved stack shows its apps even when down");
        assert!(
            dark.iter().all(|i| i.has_css_class(APP_IDLE_CLASS)),
            "…dim, because none of them is running"
        );
        assert!(
            dark.iter().all(|i| !i.has_css_class(APP_RUNNING_CLASS)),
            "the running and idle classes are mutually exclusive, so the \
             greying is a flip rather than an addition"
        );

        f.windows.set(vec![win(1, 7, "com.example.Term", 1)]);
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        let after_open = icons(&card);
        assert_eq!(
            after_open.len(),
            2,
            "the row is the file's list, not the windows'"
        );
        assert!(
            after_open[0].has_css_class(APP_RUNNING_CLASS),
            "the app with a window open on the workspace glows"
        );
        assert!(
            after_open[1].has_css_class(APP_IDLE_CLASS),
            "…and the one without stays dim"
        );
        assert!(
            after_open[0]
                .tooltip_text()
                .is_some_and(|t| !t.trim().is_empty()),
            "each icon names its app, resolved or raw"
        );
        assert_inside_and_hittable(&card, &after_open[0], "the app icon");

        // A window on a *different* workspace does not light it.
        f.windows.set(vec![win(2, 99, "com.example.Editor", 1)]);
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        assert!(
            icons(&card).iter().all(|i| i.has_css_class(APP_IDLE_CLASS)),
            "another workspace's windows must not light this card"
        );

        window.destroy();
    }

    /// An ephemeral card shows the live windows, deduped, and carries no
    /// Start/Stop — its only action is the Save field (#1071 §3.7).
    #[gtk::test]
    fn an_ephemeral_card_shows_its_windows_and_offers_a_name_field() {
        let f = fixture();
        f.workspaces.set(vec![ws(7, 1, LEFT, None)]);
        f.windows.set(vec![
            win(1, 7, "com.example.Term", 1),
            win(2, 7, "com.example.Term", 2),
            win(3, 7, "com.example.Editor", 3),
        ]);
        pump();
        let window = present(&f.page);

        let card = cards(&f.page).first().cloned().expect("an ephemeral card");
        assert_eq!(
            label_text(&card, "ts-ws-card-name"),
            EPHEMERAL_NAME,
            "it has no name until it is saved"
        );
        assert_eq!(
            icons(&card).len(),
            2,
            "two windows of one app are one icon — the stack is a set of apps"
        );
        assert!(
            by_class(&card, "ts-ws-action").is_empty(),
            "there is nothing to Start: it is already running"
        );

        let entry = by_class(&card, "ts-ws-save-entry")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Entry>().ok())
            .expect("the Save field");
        assert_inside_and_hittable(&card, entry.upcast_ref(), "the Save field");
        let save = by_class(&card, "ts-ws-save-button")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("the Save button");
        assert!(save.is_sensitive());

        // A name that cannot be a slice is refused **in place** — no rewrite, no
        // silent accept — and the tooltip offers the sanitised form §3.1 asks
        // for.
        //
        // This stops at the validation boundary deliberately, and not only
        // because the write needs a registry: past it, a *valid* name would
        // reach `save_stack` and write the developer's own
        // `~/.config/trollshell/workspaces.toml`. Keep every name this test
        // types unusable. The write's round trip is `config::workspaces`' own
        // `a_save_adds_its_stack_and_invents_no_order`, against a tempdir.
        entry.set_text("Chat Room");
        save.emit_clicked();
        pump();
        assert!(
            entry.has_css_class("error"),
            "an unusable name marks the field rather than writing the file"
        );
        assert_eq!(
            entry.text(),
            "Chat Room",
            "what was typed is kept — the suggestion is offered, not applied"
        );
        assert!(
            entry
                .tooltip_text()
                .is_some_and(|t| t.contains("chat-room")),
            "…and the sanitised form is what is offered: {:?}",
            entry.tooltip_text()
        );

        // The red clears as soon as the correction starts, rather than staying
        // through every keystroke of the fix.
        entry.set_text("Chat Roo");
        pump();
        assert!(
            !entry.has_css_class("error"),
            "the error class is not sticky"
        );

        window.destroy();
    }

    /// §7: **Inactive is greyed, and `Starting` disables the button.**
    ///
    /// The three states in one page, so the button's identity is pinned by what
    /// it *is* rather than by what it is not: Start when Inactive, Stop when
    /// Active, and an insensitive spinner while a Start is in flight.
    ///
    /// Falsified by dropping `starting` from `model`'s inputs: `music`'s button
    /// stays sensitive and the state count is wrong.
    #[gtk::test]
    fn the_three_card_states_render_as_three_different_buttons() {
        let f = fixture();
        f.saved.set(saved(&[
            ("chat", stack(Some(LEFT), &["a"])),
            ("dev", stack(Some(LEFT), &["b"])),
            ("music", stack(Some(LEFT), &["c"])),
        ]));
        f.workspaces.set(vec![
            ws(1, 1, LEFT, Some("chat")),
            ws(2, 2, LEFT, Some("music")),
        ]);
        f.windows.set(vec![win(9, 1, "a", 1)]);
        f.starting.set(BTreeSet::from(["music".to_owned()]));
        pump();
        let window = present(&f.page);

        let all = cards(&f.page);
        assert_eq!(all.len(), 3, "every saved stack is a card, running or not");

        let button = |card: &gtk::Widget| {
            by_class(card, "ts-ws-action")
                .into_iter()
                .find_map(|w| w.downcast::<gtk::Button>().ok())
                .expect("a card's Start/Stop button")
        };

        // chat — Active (a window on its workspace).
        assert!(!all[0].has_css_class(CARD_INACTIVE_CLASS));
        let stop = button(&all[0]);
        assert_eq!(
            stop.icon_name().as_deref(),
            Some("media-playback-stop-symbolic")
        );
        assert!(stop.is_sensitive());
        assert_inside_and_hittable(&all[0], stop.upcast_ref(), "the Stop button");

        // dev — Inactive: greyed, says so, and offers Start.
        assert!(
            all[1].has_css_class(CARD_INACTIVE_CLASS),
            "an Inactive card is greyed"
        );
        assert_eq!(label_text(&all[1], "ts-ws-card-note"), "Not on a screen");
        let start = button(&all[1]);
        assert_eq!(
            start.icon_name().as_deref(),
            Some("media-playback-start-symbolic")
        );
        assert!(start.is_sensitive());

        // music — Starting: a spinner, insensitive, and no icon to click.
        let spinning = button(&all[2]);
        assert!(
            !spinning.is_sensitive(),
            "a Start in flight disables the button, so a second click is not a \
             second Start"
        );
        assert_eq!(spinning.icon_name(), None, "a spinner, not an icon");

        // …and clearing the in-flight set hands the button back. `music`'s
        // workspace exists but is empty, so it falls to Inactive.
        f.starting.set(BTreeSet::new());
        pump();
        let cleared = button(&cards(&f.page)[2]);
        assert!(cleared.is_sensitive());
        assert_eq!(
            cleared.icon_name().as_deref(),
            Some("media-playback-start-symbolic")
        );

        // §7's "named + units → Active" row, reaching the *view*: `music` has
        // no windows at all, so only the slice poll can make it Active — and
        // when it does, the button becomes Stop.
        //
        // Falsified by dropping `slices_up` from `model`'s inputs: the card
        // stays on Start and the greying never lifts.
        f.slices_up.set(BTreeSet::from(["music".to_owned()]));
        pump();
        let cards_now = cards(&f.page);
        assert!(
            !cards_now[2].has_css_class(CARD_INACTIVE_CLASS),
            "a stack whose units are up is Active even with every window closed"
        );
        assert_eq!(
            button(&cards_now[2]).icon_name().as_deref(),
            Some("media-playback-stop-symbolic"),
            "…and it offers Stop, because there is still something to stop"
        );

        window.destroy();
    }

    /// §7: a stack whose recorded monitor is not connected gets its own
    /// trailing column rather than landing on a screen it does not name.
    #[gtk::test]
    fn an_absent_monitors_stacks_get_the_trailing_column() {
        let f = fixture();
        f.saved.set(saved(&[
            ("gone", stack(Some("DP-9"), &["a"])),
            ("here", stack(Some(LEFT), &["b"])),
        ]));
        f.workspaces.set(vec![ws(1, 1, LEFT, None)]);
        pump();
        let window = present(&f.page);

        let cols = columns(&f.page);
        assert_eq!(cols.len(), 2);
        assert_eq!(label_text(&cols[0], "ts-ws-column-title"), LEFT);
        assert_eq!(
            label_text(&cols[1], "ts-ws-column-title"),
            OFFLINE_COLUMN,
            "the trailing column names itself"
        );
        assert_eq!(cards(&cols[0]).len(), 1);
        assert_eq!(cards(&cols[1]).len(), 1);
        assert_eq!(label_text(&cards(&cols[1])[0], "ts-ws-card-name"), "gone");

        // …and it goes away when the screen comes back.
        f.workspaces
            .set(vec![ws(1, 1, LEFT, None), ws(2, 1, "DP-9", None)]);
        pump();
        assert_eq!(
            columns(&f.page).len(),
            2,
            "two real screens, no offline column"
        );
        assert!(
            !by_class(&f.page, "ts-ws-column-offline")
                .iter()
                .any(|_| true),
            "nothing is flagged offline any more"
        );

        window.destroy();
    }

    /// The card list scrolls (phase 1 review, LOW-2) — **at the height the
    /// drawer actually gives it**, which is the part the first cut got wrong.
    ///
    /// The drawer surface is as tall as the screen and imposes no height on the
    /// page; it simply *clips* whatever does not fit (`modal.rs`). So a
    /// `ScrolledWindow` with `propagate_natural_height` and no
    /// `max_content_height` asks for its whole content height, gets it, never
    /// shows a scrollbar, and is clipped exactly as before. The first version of
    /// this test supplied a 220 px window — a constraint the drawer never
    /// does — and so passed against a fix that did nothing.
    ///
    /// This one gives the window **more room than the cap** and asserts the cap
    /// binds anyway. Falsified two ways: remove `max_content_height` (the
    /// scroller grows to fit and `upper == page_size`), or append the cards
    /// straight into the column (no `GtkScrolledWindow` ancestor at all).
    #[gtk::test]
    fn a_column_of_many_cards_scrolls_at_the_drawers_own_height() {
        let f = fixture();
        let many: Vec<(&str, _)> = [
            "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q",
            "r", "s", "t",
        ]
        .iter()
        .map(|name| (*name, stack(Some(LEFT), &["x"])))
        .collect();
        f.saved.set(saved(&many));
        f.workspaces.set(vec![ws(1, 1, LEFT, None)]);
        pump();

        let window = gtk::Window::new();
        window.set_child(Some(&f.page));
        // Taller than `COLUMN_MAX_HEIGHT`, and taller than twenty cards need —
        // i.e. the drawer's own situation, where nothing external constrains the
        // page. The cap has to be what produces the scroll.
        window.set_default_size(900, 1400);
        window.present();
        pump();

        let all = cards(&f.page);
        assert_eq!(all.len(), 20, "every stack is still a card");

        let scroller = by_class(&f.page, "ts-ws-scroller")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::ScrolledWindow>().ok())
            .expect("the card list is inside a ScrolledWindow");
        assert!(
            all[0].is_ancestor(&scroller),
            "the cards are inside the scroller, not beside it"
        );
        assert!(
            scroller.max_content_height() > 0,
            "the scroller is capped, or it just grows to fit and never scrolls"
        );

        // The adjustment is what makes the overflow reachable: more content
        // than page means a scrollbar with somewhere to go.
        let adjustment = scroller.vadjustment();
        pump_until(2000, || adjustment.upper() > adjustment.page_size());
        assert!(
            adjustment.upper() > adjustment.page_size(),
            "twenty cards in a window with room to spare must still overflow a \
             scrollable area, because the cap binds: upper {} page {} cap {}",
            adjustment.upper(),
            adjustment.page_size(),
            scroller.max_content_height(),
        );
        assert!(
            scroller.height() <= scroller.max_content_height() + 1,
            "…and the scroller itself stays within its cap rather than growing \
             to its content: {} against {}",
            scroller.height(),
            scroller.max_content_height(),
        );

        window.destroy();
    }

    /// Before niri answers there are no columns at all; the page names the wait
    /// rather than rendering an empty strip.
    #[gtk::test]
    fn the_page_waits_for_niri_instead_of_rendering_nothing() {
        let f = fixture();
        assert!(columns(&f.page).is_empty());
        assert_eq!(label_text(&f.page, "ts-ws-empty"), NO_OUTPUTS_HINT);
    }

    /// §7: the page mounts into a drawer-shaped `gtk::Stack` as a named child
    /// and becomes visible **without disturbing the pages already in it**.
    ///
    /// Drives a stand-in stack rather than `modal::build_pages_stack`, which
    /// reaches into the plugin service registry and every page's services. What
    /// is under test is the mount contract `modal::ensure_page` relies on:
    /// `child_by_name` is its "already built?" key, so a page that failed to
    /// take its own name would either never build or evict a sibling.
    /// `Page::Workspaces ⇄ "workspaces"` is pinned in `modal.rs`'s own tests.
    #[gtk::test]
    fn the_page_mounts_as_a_named_stack_child_without_evicting_its_siblings() {
        let f = fixture();
        let stack = gtk::Stack::new();
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);

        let media = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let plugin_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
        stack.add_named(&media, Some("media"));
        stack.add_named(&plugin_slot, Some("__plugin"));
        stack.add_named(&f.page, Some("workspaces"));

        for name in ["media", "__plugin", "workspaces"] {
            assert!(
                stack.child_by_name(name).is_some(),
                "adding the Workspaces page dropped the {name:?} child"
            );
        }

        stack.set_visible_child_name("workspaces");
        pump();
        assert_eq!(stack.visible_child_name().as_deref(), Some("workspaces"));

        // Switching away and back leaves every child in place — the drawer
        // swaps pages far more often than it builds them.
        stack.set_visible_child_name("media");
        pump();
        assert_eq!(stack.visible_child_name().as_deref(), Some("media"));
        assert!(
            f.page.parent().as_ref() == Some(stack.upcast_ref::<gtk::Widget>()),
            "a page that is not the visible child is still a child of the stack"
        );

        stack.set_visible_child_name("workspaces");
        pump();
        assert_eq!(stack.visible_child_name().as_deref(), Some("workspaces"));
        assert!(
            stack.child_by_name("media").as_ref() == Some(media.upcast_ref::<gtk::Widget>()),
            "showing the Workspaces page replaced the `media` child instead of \
             covering it"
        );
    }

    /// #224/#761/#831: the columns binding must not pin its container.
    ///
    /// Falsified by capturing a strong clone of `columns_box` in
    /// `bind_columns`'s apply closure instead of taking `bind`'s own argument —
    /// which is also what `nix/lint-bind-pins.py` rejects at the source level.
    #[gtk::test]
    fn the_columns_binding_does_not_pin_its_container() {
        adw::init().expect("libadwaita init");
        let columns_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let weak = columns_box.downgrade();
        let model: Mutable<Vec<Column>> = Mutable::new(Vec::new());
        bind_columns(&columns_box, model.signal_cloned());
        pump();

        drop(columns_box);

        assert!(
            weak.upgrade().is_none(),
            "bind_columns must not pin its container: a strong clone captured by \
             the apply closure (rather than taking the closure's own argument \
             from `bind`) would keep this alive for the life of the binding, \
             defeating #224's WeakRef contract"
        );
    }
}
