//! Workspaces drawer page — phases 1 to 3 of the workspace-stacks epic
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
//! The *set* of columns comes from niri, not from GDK. Every `Workspace`
//! carries the `output` it lives on, and niri keeps at least one workspace per
//! connected output, so the connected set comes out of the *same* snapshot the
//! cards are built from. Joining a second monitor source would only introduce
//! a window where the two disagree; `App::monitors()` is not reachable from a
//! panel anyway, since `modal::build_page` hands a page no `&Monitor`.
//!
//! Column **order**, since #1110, is the outputs' logical position — `(x, y)`
//! as niri reports it, left-to-right then top-to-bottom, connector only the
//! tie-break for two outputs at the same point. That position is not on
//! `Workspace`, so [`model`] takes a second snapshot,
//! `hytte::services::displays::outputs()`, purely for `Output::x`/`::y`; a
//! connector the position snapshot hasn't (yet) caught up with falls back to
//! `(0, 0)`, which degrades to the old connector-lexical order rather than
//! scrambling the columns. Before #1110 the order was lexical by connector,
//! which is what `by_output`'s `BTreeMap` gave for free and happened to also
//! be `displays::outputs()`'s own sort — coincidence, not a rule, and wrong on
//! any layout whose connector names don't read left to right (#1110).
//! One trailing column may follow them: the stacks whose recorded monitor is not
//! connected (§5). Cards *inside* a column follow the file's `order` (§3.6),
//! which `workspace_stacks::order_index` also hands to niri as the started
//! workspace's index; the column order itself stays a property of the
//! connected outputs, not of the cards in it.
//!
//! ## Dragging a card (§5/§3.6)
//!
//! A card's screen is set by **dragging it into another monitor's column** —
//! there is no monitor field anywhere in the UI, by design (Annika, on the epic
//! thread) — and its place in the order by **dragging it onto another card**.
//! A saved card carries a [`card_drag_source`] and a [`card_drop_target`]; every
//! *connected* monitor's column carries a [`monitor_drop_target`]. One
//! [`drop_plan`] decides both halves, so a drop on a card in another column
//! records the screen **and** the position. Phase 3 shipped the screen half;
//! the order half is phase 4, which is where §5 puts it.
//!
//! ## Edit (§5/§3.7, phase 4)
//!
//! Every card — saved or ephemeral — carries an Edit button that opens
//! [`crate::panels::workspace_edit`] in the same drawer, content replaced.
//! Phase 2 put an inline name field and a Save button on the ephemeral card
//! instead; #1109 retired it ("nightmare to render"), so there is nothing
//! inline on any card and the form is the only editor.
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
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, gdk, glib, pango, prelude::*};
use hytte::prelude::*;
use hytte::services::displays::{self, Output as DisplayOutput};
use hytte::services::niri::{self, Window, Workspace};

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::layout::{
    DRAWER_MAX_WIDTH_WIDE, finish_page_clamped, page_box, toggle_class,
};
use crate::config::workspaces::{self as config_workspaces, Layout, Workspaces};
use crate::panels::workspace_edit;
use crate::workspace_stacks::{self, StackState, state_of};

/// CSS class on an app icon whose app has at least one window open on the
/// card's workspace.
const APP_RUNNING_CLASS: &str = "ts-ws-app-running";

/// The complement of [`APP_RUNNING_CLASS`] — a saved app of the stack with no
/// window open.
const APP_IDLE_CLASS: &str = "ts-ws-app-idle";

/// CSS class on a card that is not on a screen (#1071 §5).
const CARD_INACTIVE_CLASS: &str = "ts-ws-card-inactive";

/// CSS class on a card the pointer is dragging another card over — the
/// in-column reorder's landing marker (#1071 §3.6, phase 4).
const CARD_DROP_CLASS: &str = "ts-ws-card-drop";

/// CSS class on a monitor column the pointer is dragging a card over (#1071 §5).
const COLUMN_DROP_CLASS: &str = "ts-ws-column-drop";

/// CSS class on a card while it is being dragged.
const CARD_DRAGGING_CLASS: &str = "ts-ws-card-dragging";

/// Tooltip on a saved card, since a drag is the only affordance for either
/// thing it can change: the screen a stack lives on (§5 has no monitor field, by
/// design) and where it sits among its neighbours (§3.6).
const DRAG_HINT: &str = "Drag this card onto another card to reorder it, or into another screen's \
                         column to move it there.";

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

/// One entry in a card's stack row — one per **window**, not one per app-id
/// (#1133): a workspace with two Alacritty windows is two entries, each with
/// its own icon, because #1071 §3.2's stored identity is an *ordered list of
/// apps* and the same `app_id` may appear in it as often as it has windows.
#[derive(Clone, Debug, Eq, PartialEq)]
struct StackApp {
    /// The Wayland app-id, resolved to an icon and a display name at render
    /// time. Compositor-supplied, so never rendered as markup.
    app_id: String,
    /// This entry's window is open on the card's workspace. For a saved stack,
    /// "this entry's window" is positional: the n-th entry of an `app_id`
    /// glows when the n-th open window of that `app_id` exists — see
    /// [`open_app_counts`]. A dim icon renders dim (#1071 §5).
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
    Ephemeral {
        workspace: u64,
        /// The connector the workspace is on — §3.7's *"record the monitor"*.
        /// Taken from the workspace rather than from the column so a Save writes
        /// the screen niri says it is on, not the one the page drew it in.
        output: String,
        /// Its windows, in niri's column order: `(app_id, pid)`.
        ///
        /// The pid is what §3.7's *"an `app_id` with no entry becomes an app
        /// with `exec` = the process's command line"* needs, and it has to come
        /// from **this** snapshot: by the time the Edit form opens, the window
        /// may be gone, and a `/proc` read against a recycled pid would prefill
        /// somebody else's command line.
        windows: Vec<(String, Option<i32>)>,
    },
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
    /// The niri workspace currently carrying this stack's name, if any.
    ///
    /// What tells a drag between monitor columns whether there is anything to
    /// *move* as well as to record (#1071 §5): an Active stack's workspace goes
    /// with it, an Inactive one's next Start just reads the file. `None` on an
    /// ephemeral card, which has no stack to rewrite and is not draggable.
    live: Option<u64>,
    /// The screen `workspaces.toml` records for this stack — **not** the column
    /// the card is drawn in, which for a stack with no recorded monitor is
    /// wherever it happens to be running or focused.
    ///
    /// What [`drop_plan`] compares against to refuse a card put back where it
    /// already was. `None` on an ephemeral card and on a saved stack that
    /// records no screen.
    monitor: Option<String>,
}

impl Card {
    /// Whether a Start/Stop button acts, and what it says.
    fn is_active(&self) -> bool {
        matches!(self.kind, Kind::Saved(StackState::Active))
    }

    /// The niri workspace a drag to another monitor should move as well as
    /// record (#1071 §5) — `Some` **only while Active**.
    ///
    /// [`Card::live`] is set whenever a workspace carries the stack's name, and
    /// that is deliberately weaker than Active: a stack whose windows the user
    /// closed by hand still has its lingering empty workspace (niri does not
    /// remove one immediately) right up until the next Start releases the name.
    /// Moving *that* across screens would shuffle the user's workspace indices
    /// on both monitors to relocate an empty placeholder — so the file is the
    /// whole change for an Inactive stack, and its next Start reads it.
    fn workspace_to_move(&self) -> Option<u64> {
        self.is_active().then_some(self.live).flatten()
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

impl Column {
    /// The connector a drop landing in this column may record — `None` for the
    /// trailing "not connected" one (review HIGH 1).
    ///
    /// A method rather than an inline `(!offline).then_some(…)` at the one call
    /// site so the choice is falsifiable: `drop_plan` being right about `None`
    /// is no help if the thing that decides `None` is untested, and the first
    /// cut of this fix passed its own `drop_plan` test with the wiring still
    /// handing `OFFLINE_COLUMN` through.
    fn drop_connector(&self) -> Option<&str> {
        (!self.offline).then_some(self.connector.as_str())
    }
}

/// Heading of the trailing column for stacks whose monitor is absent.
const OFFLINE_COLUMN: &str = "Not connected";

/// The whole page: the columns, plus the card order the page is showing.
///
/// The order is carried alongside rather than read back off the columns because
/// the columns have **lost** it: they are grouped by monitor, and `order` is one
/// flat array across every screen (#1071 §4). Concatenating the columns would
/// yield a per-monitor order, which is exactly the "global rewrite" a drop must
/// not perform — dragging a card inside DP-1's column would silently reshuffle
/// HDMI-A-1's cards in the file.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct PageModel {
    columns: Vec<Column>,
    /// Every saved stack's name, in the order the file puts them — which is
    /// what a reorder rewrites.
    order: Vec<String>,
}

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
///
/// Column **order** (#1110): by the outputs' logical position (`outputs`,
/// `(x, y)`), connector only the tie-break; a connector `outputs` has no entry
/// for falls back to `(0, 0)`. The offline column always trails, regardless of
/// position.
fn model(
    workspaces: &[Workspace],
    windows: &[Window],
    saved: &Workspaces,
    slices_up: &BTreeSet<String>,
    starting: &BTreeSet<String>,
    outputs: &[DisplayOutput],
) -> PageModel {
    let connected: BTreeSet<&str> = workspaces
        .iter()
        .filter_map(|w| w.output.as_deref())
        .collect();
    if connected.is_empty() {
        return PageModel::default();
    }
    let focused = workspaces
        .iter()
        .find(|w| w.is_focused)
        .and_then(|w| w.output.as_deref());

    // Connector -> logical position, for the final column sort below. A
    // connector `connected` names but this snapshot hasn't (yet) reported
    // falls back to `(0, 0)` at the sort site rather than here, so a missing
    // entry reads the same as an explicit `(0, 0)` output — both degrade to
    // the connector tie-break.
    let positions: BTreeMap<&str, (i32, i32)> = outputs
        .iter()
        .map(|o| (o.name.as_str(), (o.x, o.y)))
        .collect();

    // `BTreeMap` for cheap dedup while grouping cards by output as they're
    // built — **not** for its order, which used to be the column order but
    // isn't any more (#1110): that's decided afterwards, by `positions`.
    let mut by_output: BTreeMap<&str, Vec<Card>> = connected
        .iter()
        .map(|connector| ((*connector), Vec::new()))
        .collect();
    let mut offline: Vec<Card> = Vec::new();

    // Saved stacks first, in the file's order, so an Inactive one keeps its
    // place among the Active ones.
    let order = saved.names_in_order();
    for name in order.clone() {
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
        let open_counts: BTreeMap<&str, usize> = live
            .map(|w| open_app_counts(w.id, windows))
            .unwrap_or_default();
        // Per-instance glow (#1133): the n-th entry of an `app_id` in the
        // file's list lights up when the n-th open window of that `app_id`
        // exists, so `seen` counts each id's ordinal as the map runs in stack
        // order — not membership, which would light every entry of a
        // half-open pair.
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        let card = Card {
            name: name.clone(),
            kind: Kind::Saved(state),
            apps: stack
                .apps
                .iter()
                .map(|app| {
                    let ordinal = seen.entry(app.id.as_str()).or_insert(0);
                    *ordinal += 1;
                    let open = open_counts.get(app.id.as_str()).copied().unwrap_or(0);
                    StackApp {
                        app_id: app.id.clone(),
                        running: *ordinal <= open,
                    }
                })
                .collect(),
            live: live.map(|w| w.id),
            monitor: stack.monitor.clone(),
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

    for (output, card) in ephemeral_cards(workspaces, windows) {
        by_output.entry(output).or_default().push(card);
    }

    let columns = order_columns(by_output, offline, &positions);
    PageModel { columns, order }
}

/// Turn the per-connector groupings into the final, ordered column list
/// (#1110). Split out of [`model`] purely to keep that function under
/// clippy's line count — the sort itself needs nothing `model` doesn't
/// already have in hand.
///
/// Position order, connector the tie-break; a connector `positions` has no
/// entry for falls back to `(0, 0)`. The offline column, if any, always
/// trails, regardless of position.
fn order_columns(
    by_output: BTreeMap<&str, Vec<Card>>,
    offline: Vec<Card>,
    positions: &BTreeMap<&str, (i32, i32)>,
) -> Vec<Column> {
    let mut columns: Vec<Column> = by_output
        .into_iter()
        .map(|(connector, cards)| Column {
            connector: connector.to_owned(),
            offline: false,
            cards,
        })
        .collect();
    columns.sort_by(|a, b| {
        let pos_a = positions
            .get(a.connector.as_str())
            .copied()
            .unwrap_or((0, 0));
        let pos_b = positions
            .get(b.connector.as_str())
            .copied()
            .unwrap_or((0, 0));
        (pos_a, &a.connector).cmp(&(pos_b, &b.connector))
    });
    if !offline.is_empty() {
        columns.push(Column {
            connector: OFFLINE_COLUMN.to_owned(),
            offline: true,
            cards: offline,
        });
    }
    columns
}

/// The ephemeral cards — an unnamed workspace with windows on it — paired with
/// the connector each belongs in (#1071 §3.7).
///
/// An unnamed *empty* workspace is niri's trailing spare, not a card; a named
/// workspace that no stack knows about is the user's own and is left alone.
fn ephemeral_cards<'w>(workspaces: &'w [Workspace], windows: &[Window]) -> Vec<(&'w str, Card)> {
    let mut unnamed: Vec<&Workspace> = workspaces
        .iter()
        .filter(|w| w.name.is_none())
        .filter(|w| windows.iter().any(|win| win.workspace_id == Some(w.id)))
        .collect();
    unnamed.sort_by_key(|w| w.idx);
    unnamed
        .into_iter()
        .filter_map(|workspace| {
            let output = workspace.output.as_deref()?;
            let on_workspace = ordered_windows(workspace.id, windows);
            Some((
                output,
                Card {
                    name: String::new(),
                    kind: Kind::Ephemeral {
                        workspace: workspace.id,
                        output: output.to_owned(),
                        windows: on_workspace
                            .iter()
                            .map(|(app_id, pid)| ((*app_id).to_owned(), *pid))
                            .collect(),
                    },
                    // In **niri's column order**, not a set: this list is what a
                    // Save records, and #1071 §3.4 step 3 makes the stack's
                    // order be the column order. Collecting through a
                    // `BTreeSet` here (as this once did) silently sorted it
                    // lexicographically, which is the input
                    // `workspace_stacks::column_order_batch` would then have
                    // restored wrongly.
                    apps: on_workspace
                        .iter()
                        .map(|(app_id, _)| StackApp {
                            app_id: (*app_id).to_owned(),
                            running: true,
                        })
                        .collect(),
                    // Deliberately `None`: an ephemeral card has no entry in the
                    // file, so there is nothing for a drag to rewrite. Its
                    // workspace id lives on `Kind::Ephemeral`, where Save uses
                    // it.
                    live: None,
                    monitor: None,
                },
            ))
        })
        .collect()
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

/// How many open windows of each app-id are on `workspace_id`, for a saved
/// card's per-instance glow (#1133).
///
/// A count, deliberately: the n-th entry of an `app_id` in the file's list
/// lights up when the n-th open window of that `app_id` exists, and that only
/// needs *how many* are open, not *which* window backs which entry — that
/// question is `column_order_batch`'s, at Start time, over a live niri, not
/// the drawer's at render time.
fn open_app_counts(workspace_id: u64, windows: &[Window]) -> BTreeMap<&str, usize> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for app_id in ordered_app_ids(workspace_id, windows) {
        *counts.entry(app_id).or_insert(0) += 1;
    }
    counts
}

/// The workspace's windows, resolved to app-ids in **niri's column order** —
/// what an ephemeral card's row shows, and what a Save records as the stack
/// order (#1071 §3.4 step 3 makes that order niri's column order, so recording
/// it in column order is what makes a Start reproduce what was saved).
///
/// **Not deduped** (#1133): two windows of one app-id are two entries, because
/// the stack itself now records one entry per window — a workspace with two
/// Alacritty windows saves, and shows, two Alacritty entries.
fn ordered_app_ids(workspace_id: u64, windows: &[Window]) -> Vec<&str> {
    ordered_windows(workspace_id, windows)
        .into_iter()
        .map(|(app_id, _)| app_id)
        .collect()
}

/// [`ordered_app_ids`], keeping each app's **pid** alongside its id.
///
/// The pid is §3.7's other half: an `app_id` that resolves to no desktop entry
/// is saved with `exec` = the running process's command line, which needs a pid
/// to read. Taken from the same snapshot as everything else on the card, because
/// a pid resolved later may have been recycled onto somebody else's process.
///
/// **Not deduped** (#1133): every window on the workspace contributes its own
/// entry, in column order — routing this through a dedup (as it once did) is
/// how two windows of one app collapsed into a single stack entry that could
/// only ever be Started or shown once.
fn ordered_windows(workspace_id: u64, windows: &[Window]) -> Vec<(&str, Option<i32>)> {
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

    on_workspace
        .into_iter()
        .filter_map(|w| w.app_id.as_deref().map(|app_id| (app_id, w.pid)))
        .collect()
}

pub fn panel_workspaces() -> gtk::Widget {
    build_panel(
        niri::workspaces(),
        niri::windows(),
        config_workspaces::signal(),
        workspace_stacks::slices_up(),
        workspace_stacks::starting(),
        displays::outputs(),
    )
}

/// [`panel_workspaces`] with every source injected, so a test can drive the page
/// without a registered `Registry`.
fn build_panel<W, N, S, U, T, O>(
    workspaces: W,
    windows: N,
    saved: S,
    slices_up: U,
    starting: T,
    outputs: O,
) -> gtk::Widget
where
    W: Signal<Item = Vec<Workspace>> + 'static,
    N: Signal<Item = Vec<Window>> + 'static,
    S: Signal<Item = Workspaces> + 'static,
    U: Signal<Item = BTreeSet<String>> + 'static,
    T: Signal<Item = BTreeSet<String>> + 'static,
    O: Signal<Item = Vec<DisplayOutput>> + 'static,
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
        let starting = starting,
        let outputs = outputs =>
            model(workspaces, windows, saved, slices_up, starting, outputs)
    }
    // `Window` does not derive `PartialEq` (only `Workspace` does), so the
    // *inputs* cannot be deduped — but the model can, and it is what the
    // rebuild costs. Without this every window-title change on any workspace
    // tears down and rebuilds every card on every monitor. It also matters more
    // in phase 2 than in phase 1: the slice poll ticks every three seconds and
    // a rebuild tears down every card on every monitor. (Since #1109 there is no
    // half-typed name on a card to lose — the Edit form holds its own draft and
    // is not rebound to any of this; see `panels::workspace_edit`'s module doc.)
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
    S: Signal<Item = PageModel> + 'static,
{
    // One cache for the whole page: an app on two workspaces costs one
    // `AppInfo::all()` scan, not one per card. Lives as long as the binding.
    let meta_cache: MetaCache = Rc::new(RefCell::new(HashMap::new()));
    bind(model, columns_box, move |columns_box, page| {
        while let Some(child) = columns_box.first_child() {
            columns_box.remove(&child);
        }
        if page.columns.is_empty() {
            columns_box.append(&hint(NO_OUTPUTS_HINT));
            return;
        }
        // A card dropped on a column or on another card may have come from
        // **any** column, so the facts a drop decides from have to be page-wide
        // rather than per column. Rebuilt with the model, so it is never more
        // than one revision old — the same currency every other handler on this
        // page has.
        let context = Rc::new(drop_context(&page));
        for column in &page.columns {
            columns_box.append(&build_column(column, &meta_cache, &context));
        }
    });
}

fn build_column(column: &Column, meta_cache: &MetaCache, context: &Rc<DropContext>) -> gtk::Widget {
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
        // The offline column's heading is not a connector (review HIGH 1) —
        // `build_card` asks `column` itself rather than being handed a
        // precomputed value, so there is exactly one place this is decided
        // (#1121 gap A: the fix round's own re-verification found the
        // original call site — `Some(column.connector.as_str())` here —
        // untested, because `Column::drop_connector`'s own test supplies the
        // `None` itself and never exercises this loop).
        for card in &column.cards {
            cards.append(&build_card(card, meta_cache, column, context));
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

    // A connected monitor's column takes cards dropped on it (#1071 §5). The
    // trailing "Not connected" column deliberately does not: it is not a
    // screen, so there is no connector to record and nowhere for niri to move a
    // workspace to.
    if !column.offline {
        outer.add_controller(monitor_drop_target(&column.connector, context));
    }

    outer.upcast()
}

/// The drop half of #1071 §5's drag between monitor columns.
///
/// Accepts a plain string — the stack's name, which is what
/// [`card_drag_source`] puts on the drag — rather than a custom `GType`: the
/// two ends are the same page in the same process, and a string keeps the drag
/// inspectable with the ordinary GTK tooling.
///
/// `gdk::DragAction::MOVE` rather than `COPY`, which is what it is: a stack
/// lives on one screen, and the drop moves it rather than duplicating it.
fn monitor_drop_target(connector: &str, context: &Rc<DropContext>) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
    // The highlight is on the *column*, so the user can see which screen the
    // card is about to land on while the pointer is still over the gap between
    // two cards.
    target.connect_enter(|target, _, _| {
        if let Some(widget) = target.widget() {
            widget.add_css_class(COLUMN_DROP_CLASS);
        }
        gdk::DragAction::MOVE
    });
    target.connect_leave(|target| {
        if let Some(widget) = target.widget() {
            widget.remove_css_class(COLUMN_DROP_CLASS);
        }
    });
    let connector = connector.to_owned();
    let context = Rc::clone(context);
    target.connect_drop(move |target, value, _, _| {
        if let Some(widget) = target.widget() {
            widget.remove_css_class(COLUMN_DROP_CLASS);
        }
        let Ok(name) = value.get::<String>() else {
            return false;
        };
        // Everything the drop decides is in `drop_plan`, and everything it
        // *does* is `perform_drop`. The handler holds no logic on purpose: a
        // GTK drop callback cannot be invoked from a test, so any branch left
        // in here is a branch nothing can falsify — which is exactly how the
        // same-screen guard and the Active-workspace hand-off both shipped
        // unfalsifiable in the first cut (#1106 review F5).
        //
        // `before: None` — a drop on the column's own background is about the
        // screen and says nothing about where among the cards it should sit.
        // This target is only ever attached to a *connected* column, so the
        // connector is always `Some` here.
        let Some(action) = drop_plan(&name, Some(&connector), None, &context) else {
            return false;
        };
        perform_drop(action);
        true
    });
    target
}

/// A saved card's drop target: the in-column reorder (#1071 §3.6/§5, phase 4).
///
/// Phase 3 deliberately shipped without this — its own PR body says *"Dragging a
/// card up or down within one column does nothing"* — because §5 puts the card
/// order's handles in this phase. Dropping card A on card B puts A where B is;
/// dropping it on a card in **another** column does that *and* moves it to that
/// screen, which is the one case where both halves of [`DropAction`] fire.
///
/// The target sits on the card rather than on a thin gap widget between cards
/// because the cards are the only things the user can aim at, and "drop it on
/// the card you want to be above" is the rule both halves of a list-reorder drag
/// can agree on.
fn card_drop_target(
    name: &str,
    connector: Option<&str>,
    context: &Rc<DropContext>,
) -> gtk::DropTarget {
    let target = gtk::DropTarget::new(glib::types::Type::STRING, gdk::DragAction::MOVE);
    target.connect_enter(|target, _, _| {
        if let Some(widget) = target.widget() {
            widget.add_css_class(CARD_DROP_CLASS);
        }
        gdk::DragAction::MOVE
    });
    target.connect_leave(|target| {
        if let Some(widget) = target.widget() {
            widget.remove_css_class(CARD_DROP_CLASS);
        }
    });
    let before = name.to_owned();
    let connector = connector.map(str::to_owned);
    let context = Rc::clone(context);
    target.connect_drop(move |target, value, _, _| {
        if let Some(widget) = target.widget() {
            widget.remove_css_class(CARD_DROP_CLASS);
        }
        let Ok(dragged) = value.get::<String>() else {
            return false;
        };
        let Some(action) = drop_plan(&dragged, connector.as_deref(), Some(&before), &context)
        else {
            return false;
        };
        perform_drop(action);
        true
    });
    target
}

/// One card as a drop can see it: what the file records, and what niri has.
///
/// Built from the model rather than read back at drop time, and — unlike
/// [`start_by_name`], which re-reads the file because a card carries no app
/// list — that is sound here: the model is rebuilt on the config signal itself,
/// so `monitor` is never staler than one revision of the very file a drop is
/// about to rewrite.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Droppable {
    /// The screen `workspaces.toml` records for this stack, if any.
    monitor: Option<String>,
    /// The niri workspace to move with it — `Some` only while Active.
    workspace: Option<u64>,
}

/// What a drop should do (#1071 §5). Pure.
///
/// Two independent halves, because a drop can do either, both or neither:
/// dropping a card on another screen's column changes its `monitor`, dropping it
/// on a card changes the `order`, and dropping it on a card in *another* column
/// does both. Both `None` is not a `DropAction` at all — see [`drop_plan`].
#[derive(Clone, Debug, Eq, PartialEq)]
struct DropAction {
    name: String,
    /// The screen to record, when the drop changes it.
    monitor: Option<String>,
    /// The live workspace to move with it — only ever set alongside `monitor`,
    /// and only while the stack is Active (phase 3's `workspace_to_move`).
    workspace: Option<u64>,
    /// The page-wide card order to write, when the drop changes it (#1071 §3.6).
    order: Option<Vec<String>>,
}

/// Everything a drop decides from: the cards, and the order they are in.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DropContext {
    /// Page-wide rather than per column: a card dropped on a column may have
    /// come from **any** column. Only saved cards appear — an ephemeral one has
    /// no entry in the file for a drop to rewrite.
    cards: BTreeMap<String, Droppable>,
    /// Every saved stack in the order the file puts them, across every screen.
    order: Vec<String>,
}

/// The drop-relevant facts about the whole page.
fn drop_context(page: &PageModel) -> DropContext {
    DropContext {
        cards: page
            .columns
            .iter()
            .flat_map(|c| &c.cards)
            .filter(|card| matches!(card.kind, Kind::Saved(_)))
            .map(|card| {
                (
                    card.name.clone(),
                    Droppable {
                        monitor: card.monitor.clone(),
                        workspace: card.workspace_to_move(),
                    },
                )
            })
            .collect(),
        order: page.order.clone(),
    }
}

/// `order` with `name` moved onto the slot `target` currently occupies
/// (#1071 §3.6).
///
/// **Remove, then insert at the target's index in the *original* list** — which
/// is the ordinary "move to index" of a list drag, and reads the way a user
/// expects in both directions: dragging a card **down** onto its neighbour puts
/// it after that neighbour, dragging it **up** onto one puts it before. (Taking
/// the target's index *after* the removal instead would make a one-place
/// downward drag a no-op, since the card is already immediately before its
/// successor.)
///
/// What it is **not** is "collect this column's names in the new order and
/// append the rest". `order` is one flat array across every screen (§4), so a
/// rebuild that grouped by column would reshuffle every *other* monitor's cards
/// as a side effect of dragging one card inside its own column. Removing one
/// element and reinserting it is the only rewrite that leaves every other name's
/// relative position exactly as it was, on every screen, by construction.
///
/// `None` when nothing would change — the name is not in the order, the target
/// is not, or the result equals the input — so an accidental two-pixel drag does
/// not rewrite `workspaces.toml`.
fn reorder_onto(order: &[String], name: &str, target: &str) -> Option<Vec<String>> {
    let from = order.iter().position(|n| n == name)?;
    let to = order.iter().position(|n| n == target)?;
    let mut out = order.to_vec();
    let moved = out.remove(from);
    // `to` indexes the original list, so after the removal it is at most
    // `out.len()` — which `Vec::insert` accepts as "append".
    out.insert(to.min(out.len()), moved);
    (out != order).then_some(out)
}

/// Decide a drop of the card `name` onto the column of `connector`, optionally
/// onto the card `before`.
///
/// `None` — nothing to do, which GTK reports back to the drag source as a
/// refused drop — whenever neither half would change anything:
///
/// * the name is not a saved card on this page (an ephemeral card is not
///   draggable, so this is a drag from somewhere else entirely);
/// * the stack **already records this screen** and the drop is not on a card, or
///   is on the card it already sits before. A card picked up and put back where
///   it was must not rewrite the config file.
///
/// Note what is *not* a reason to refuse the monitor half: a stack with no
/// `monitor` recorded, dropped on the column it is currently drawn in. That
/// column is where the focused output happens to be, not a recorded choice, so
/// the drop is the user saying "here, always" and it writes.
fn drop_plan(
    name: &str,
    connector: Option<&str>,
    before: Option<&str>,
    context: &DropContext,
) -> Option<DropAction> {
    let card = context.cards.get(name)?;
    // `connector` is `None` for the trailing "Not connected" column, and that is
    // the whole of review HIGH 1. That column's heading is the literal string
    // `OFFLINE_COLUMN`; passing it through as a connector wrote
    // `monitor = "Not connected"` into `workspaces.toml` — a value no output
    // will ever match, stranding the card in the greyed column until the file is
    // hand-edited — and, for an Active stack, sent niri a
    // `MoveWorkspaceToMonitor { output: "Not connected" }`.
    //
    // Phase 3 stated this invariant for the *column* target ("it is not a
    // screen, so there is no connector to record and nowhere for niri to move a
    // workspace to"); phase 4's card target routed around it. An `Option` is the
    // fix rather than a guard because it makes the offline case unrepresentable
    // at the type level — and it keeps the **reorder** half working there, which
    // is real: those cards are still ordered.
    let monitor = connector
        .filter(|connector| card.monitor.as_deref() != Some(*connector))
        .map(str::to_owned);
    // A card dropped on **itself** is not a reorder, and `reorder_onto` would
    // answer `None` for it anyway — but saying so here keeps the self-drop from
    // reading as an accident of the pure function.
    let order = before
        .filter(|before| *before != name)
        .and_then(|before| reorder_onto(&context.order, name, before));
    if monitor.is_none() && order.is_none() {
        return None;
    }
    Some(DropAction {
        name: name.to_owned(),
        // The live workspace rides the monitor half only: there is nothing for
        // niri to do about a card's position in a list.
        workspace: monitor.as_ref().and(card.workspace),
        monitor,
        order,
    })
}

/// Perform a decided drop (#1071 §5/§3.6).
///
/// The file first, in both halves, for the reason phase 3 states: a write that
/// failed (a read-only overlay, no `XDG_CONFIG_HOME`) must not leave the
/// workspace somewhere the file disagrees with, or the card snaps back on the
/// next poll having moved the user's windows for nothing.
fn perform_drop(action: DropAction) {
    if let Some(order) = action.order {
        workspace_stacks::spawn_set_order(order);
    }
    if let Some(monitor) = action.monitor {
        workspace_stacks::spawn_move_to_monitor(action.name, monitor, action.workspace);
    }
}

/// The drag half: a saved card carries its stack's name (#1071 §5).
///
/// Only saved cards get one. An ephemeral card has no entry in the file, so
/// there is no `monitor` for a drop to rewrite — it is the workspace niri has
/// already put somewhere, and moving *it* between screens is niri's own
/// business, not this page's.
///
/// In-column reordering is **not** this: #1071 §5 puts the card order's drag
/// handles in the Edit sub-page (phase 4), and this controller only ever
/// answers the question "which screen".
fn card_drag_source(name: &str) -> gtk::DragSource {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);
    let name = name.to_owned();
    // `prepare` is the *content* callback and nothing else. Dimming the card
    // rides `drag-begin`, which is the signal `drag-end` below is paired with —
    // today returning `Some` from prepare is what begins the drag, so the two
    // coincide, but a `prepare` that ever returns without a drag beginning
    // would leave the class on with no `drag-end` to take it off (#1106 review
    // INFO 10).
    source.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&name.to_value())));
    source.connect_drag_begin(|source, _| {
        // Dim the card while it is in flight, so the column it came from does
        // not look like it still holds it.
        if let Some(widget) = source.widget() {
            widget.add_css_class(CARD_DRAGGING_CLASS);
        }
    });
    source.connect_drag_end(|source, _, _| {
        if let Some(widget) = source.widget() {
            widget.remove_css_class(CARD_DRAGGING_CLASS);
        }
    });
    source
}

fn build_card(
    card: &Card,
    meta_cache: &MetaCache,
    column: &Column,
    context: &Rc<DropContext>,
) -> gtk::Widget {
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

    // `[start/stop] [edit]`, in Annika's own order (#1109). Every card gets the
    // Edit button — §3.7 settles that an unnamed workspace is a card like any
    // other and that **Edit → Save** is what creates its entry; only a saved one
    // has anything to start or stop.
    //
    // Phase 2 put an inline name field and a Save button *on* the ephemeral
    // card, because there was no form to open yet. #1109 retires it outright —
    // "nightmare to render", and nothing inline on any card — so there is no
    // fallback here, just the button.
    match &card.kind {
        Kind::Saved(state) => header.append(&start_stop_button(&card.name, *state)),
        Kind::Ephemeral { .. } => {}
    }
    header.append(&edit_button(card));
    outer.append(&header);

    if let Kind::Saved(StackState::Inactive) = card.kind {
        let note = hint(INACTIVE_HINT);
        note.add_css_class("ts-ws-card-note");
        outer.append(&note);
    }

    // #1119: the icons are double-size now (`build_app_icon`), so a busy
    // stack's apps no longer all fit in one row at a column's width. A
    // `gtk::FlowBox` wraps the overflow onto a second row instead of clipping
    // past the card's edge or shrinking to squeeze in, which is what a plain
    // `gtk::Box` would do. `max_children_per_line` is left generously high
    // rather than pinned to a small count, so wrapping is driven by the
    // column's actual width — the point of the feature — not by an arbitrary
    // per-row cap that would wrap a card that still had room.
    let apps = gtk::FlowBox::new();
    apps.add_css_class("ts-ws-apps");
    apps.set_selection_mode(gtk::SelectionMode::None);
    apps.set_row_spacing(6);
    apps.set_column_spacing(6);
    apps.set_min_children_per_line(1);
    apps.set_max_children_per_line(32);
    apps.set_halign(gtk::Align::Start);
    for app in &card.apps {
        apps.insert(&build_app_icon(app, meta_cache), -1);
    }
    outer.append(&apps);

    // A saved card can be dragged into another screen's column, and — since
    // phase 4 — onto another card to reorder it (#1071 §5/§3.6). There is no
    // monitor field anywhere in the UI; this *is* the monitor control, which is
    // Annika's call on the epic thread.
    if matches!(card.kind, Kind::Saved(_)) {
        outer.set_tooltip_text(Some(DRAG_HINT));
        outer.add_controller(card_drag_source(&card.name));
        outer.add_controller(card_drop_target(
            &card.name,
            column.drop_connector(),
            context,
        ));
    }

    outer.upcast()
}

/// The Edit button — the entry to the #1071 §5 sub-page, on every card.
///
/// Builds the whole [`workspace_edit::Draft`] here rather than passing a name,
/// because *this* is where the three sources have already been joined: the file
/// says what the stack is, niri says whether it is running and on what, and — for
/// an ephemeral card — the windows in front of the user are the apps. See
/// `panels::workspace_edit`'s module doc for why the form does not re-derive
/// any of it.
fn edit_button(card: &Card) -> gtk::Button {
    let button = gtk::Button::from_icon_name("document-edit-symbolic");
    button.add_css_class("flat");
    button.add_css_class("ts-ws-edit-open");
    button.set_tooltip_text(Some(match card.kind {
        Kind::Saved(_) => "Edit this workspace",
        Kind::Ephemeral { .. } => "Name and save this workspace",
    }));
    let card = card.clone();
    button.connect_clicked(move |_| {
        let Some(draft) = draft_for(&card) else {
            // The stack went out of the file between the model being built and
            // this click — the file is live-reloaded and the drawer can sit open
            // across a hand edit. Say so (review LOW 15): returning in silence
            // leaves the user pressing a dead button.
            workspace_stacks::report(
                &card.name,
                &format!(
                    "{} is no longer in workspaces.toml, so there is nothing to edit",
                    card.name
                ),
            );
            return;
        };
        // Publish first, switch second: the drawer child is already built and
        // bound, so a switch ahead of the draft would flash the "nothing is
        // being edited" hint.
        let key = draft.key();
        workspace_edit::open(draft);
        crate::modal::switch_to_workspace_edit(&key);
    });
    button
}

/// The draft the Edit sub-page opens with, for one card.
///
/// `None` for a saved card whose stack has vanished from the file between the
/// model being built and the button being pressed — the file is live-reloaded
/// and the drawer can sit open across a hand edit.
fn draft_for(card: &Card) -> Option<workspace_edit::Draft> {
    match &card.kind {
        Kind::Saved(state) => {
            // Read back rather than captured, the same call `start_by_name`
            // makes and for the same reason: a captured `Stack` could be a
            // revision behind the file the Save is about to rewrite.
            let saved = config_workspaces::current();
            let stack = saved.stacks.get(&card.name).cloned()?;
            Some(workspace_edit::Draft {
                previous: Some(card.name.clone()),
                name: card.name.clone(),
                apps: stack.apps,
                layout: stack.layout,
                autostart: stack.autostart,
                monitor: stack.monitor,
                workspace: card.live,
                // `Starting` blocks a rename too — see `rename_is_blocked`, and
                // review MEDIUM 2 for why that window is the one it matters in.
                active: workspace_edit::rename_is_blocked(*state),
                taken: other_names(&saved, Some(&card.name)),
            })
        }
        Kind::Ephemeral {
            workspace,
            output,
            windows,
        } => Some(ephemeral_draft(
            *workspace,
            output,
            other_names(&config_workspaces::current(), None),
            // §3.7's mapping: the workspace's windows in column order, each
            // resolved through its desktop entry, with the ones that have none
            // carrying the running command line for correction. The only impure
            // step — it reads `$XDG_DATA_DIRS` and `/proc` — which is why it is
            // taken here and the rest of the draft is built by the pure function
            // below.
            workspace_edit::ephemeral_apps_for(
                &windows
                    .iter()
                    .map(|(app_id, pid)| (app_id.clone(), pid.and_then(|p| u32::try_from(p).ok())))
                    .collect::<Vec<_>>(),
            ),
        )),
    }
}

/// Every saved stack name **except** `mine`, for the form's fast taken-name
/// check (review MEDIUM 8).
///
/// `mine` is excluded because re-saving a stack under its own name is the
/// ordinary case; `plan_save` also guards that, so this is belt and braces at
/// the seam where the set is built rather than where it is read.
fn other_names(saved: &Workspaces, mine: Option<&str>) -> BTreeSet<String> {
    saved
        .stacks
        .keys()
        .filter(|name| mine.is_none_or(|mine| !name.eq_ignore_ascii_case(mine)))
        .cloned()
        .collect()
}

/// The draft an **ephemeral** card's Edit opens with, given its apps (#1071
/// §3.7). Pure.
///
/// Split from [`draft_for`] so §3.7's *"record the monitor"* is falsifiable: the
/// rest of that arm reads `$XDG_DATA_DIRS` and `/proc`, which no test may do.
fn ephemeral_draft(
    workspace: u64,
    output: &str,
    taken: BTreeSet<String>,
    apps: Vec<crate::config::workspaces::StackApp>,
) -> workspace_edit::Draft {
    workspace_edit::Draft {
        previous: None,
        name: String::new(),
        apps,
        taken,
        layout: Layout::None,
        autostart: false,
        // §3.7's *"record the monitor"* — the screen **niri** says the workspace
        // is on, not the column the card happened to be drawn in. For a stack
        // with no recorded monitor those two differ whenever the focus is
        // elsewhere, and the file must record where the windows actually are.
        monitor: Some(output.to_owned()),
        workspace: Some(workspace),
        // An ephemeral card is by definition on screen, but `active` only gates
        // the rename refusal and an ephemeral card has no name to rename *from*.
        active: false,
    }
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
    workspace_stacks::spawn_start(name.to_owned(), stack, saved);
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
    // #1119: double-size (32 px) so a stack's apps read at a glance; the
    // `.ts-ws-apps` `FlowBox` above is what keeps the extra width from
    // overrunning the card.
    img.set_icon_size(gtk::IconSize::Large);
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
    use hytte::reactive::Pending;
    use hytte::services::displays::Output as DisplayOutput;
    use hytte::services::niri::{Window, WindowLayout, Workspace};
    use std::collections::BTreeMap;

    pub(super) const LEFT: &str = "DP-1";
    pub(super) const RIGHT: &str = "HDMI-A-1";

    /// A [`DisplayOutput`] at logical position `(x, y)` — #1110's column
    /// order. Only `name`/`x`/`y` matter to the model; the rest are neutral
    /// filler.
    pub(super) fn output_at(name: &str, x: i32, y: i32) -> DisplayOutput {
        DisplayOutput {
            name: name.to_owned(),
            make: String::new(),
            model: String::new(),
            mode: None,
            enabled: Pending::settled(true),
            scale: 1.0,
            transform: "normal".to_owned(),
            x,
            y,
        }
    }

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
    use super::fixtures::{LEFT, RIGHT, no_stacks, output_at, saved, stack, win, ws, ws_focused};
    use super::{
        Card, Column, DisplayOutput, DropAction, DropContext, Droppable, Kind, OFFLINE_COLUMN,
        PageModel, StackState, drop_context, drop_plan, model, reorder_onto,
    };
    use crate::config::workspaces::Workspaces;
    use std::collections::BTreeSet;

    /// The whole page model with nothing in flight and no slice up, and no
    /// output positions known unless given — a missing position falls back to
    /// `(0, 0)` for every connector, which degrades to the connector
    /// tie-break (#1110).
    fn page(
        workspaces: &[hytte::services::niri::Workspace],
        windows: &[hytte::services::niri::Window],
        file: &Workspaces,
        outputs: &[DisplayOutput],
    ) -> PageModel {
        model(
            workspaces,
            windows,
            file,
            &BTreeSet::new(),
            &BTreeSet::new(),
            outputs,
        )
    }

    /// [`page`]'s columns, with no output positions known — the common case
    /// for tests that don't care about column *order*.
    fn built(
        workspaces: &[hytte::services::niri::Workspace],
        windows: &[hytte::services::niri::Window],
        file: &Workspaces,
    ) -> Vec<Column> {
        built_with_outputs(workspaces, windows, file, &[])
    }

    /// [`built`], with output positions supplied — #1110's column order.
    fn built_with_outputs(
        workspaces: &[hytte::services::niri::Workspace],
        windows: &[hytte::services::niri::Window],
        file: &Workspaces,
        outputs: &[DisplayOutput],
    ) -> Vec<Column> {
        page(workspaces, windows, file, outputs).columns
    }

    /// A [`DropContext`] over these cards, in this page-wide order.
    fn context(cards: &[(&str, Droppable)], order: &[&str]) -> DropContext {
        DropContext {
            cards: cards
                .iter()
                .map(|(name, d)| ((*name).to_owned(), d.clone()))
                .collect(),
            order: order.iter().map(|n| (*n).to_owned()).collect(),
        }
    }

    fn names(order: &[String]) -> Vec<&str> {
        order.iter().map(String::as_str).collect()
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

    /// One column per connected output — unchanged from phase 1, but now
    /// driven by the *connected set* rather than by which outputs happen to
    /// carry a named workspace, since a saved stack must have a column to sit
    /// in even when nothing of it is running. No output positions are known
    /// here, so every connector ties at the `(0, 0)` fallback and the order
    /// degrades to the connector tie-break (`LEFT` = `"DP-1"` < `RIGHT` =
    /// `"HDMI-A-1"`) — see [`columns_order_by_output_position_not_connector`]
    /// for the case that actually exercises position (#1110).
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

    /// #1110: columns order by the outputs' logical position, not by
    /// connector name. `DP-3` sits leftmost (`x = 0`), then `DP-1`
    /// (`x = 1920`), then `DP-2` (`x = 3840`) — the reverse of connector
    /// order, which is exactly what makes this falsify a regression to
    /// lexical sorting.
    ///
    /// Falsified by sorting columns by connector instead of position.
    #[test]
    fn columns_order_by_output_position_not_connector() {
        let columns = built_with_outputs(
            &[
                ws(1, 1, "DP-1", None),
                ws(2, 1, "DP-2", None),
                ws(3, 1, "DP-3", None),
            ],
            &[],
            &no_stacks(),
            &[
                output_at("DP-1", 1920, 0),
                output_at("DP-2", 3840, 0),
                output_at("DP-3", 0, 0),
            ],
        );
        assert_eq!(connectors(&columns), ["DP-3", "DP-1", "DP-2"]);
    }

    /// #1110: two outputs at the same `x` order by `y`, connector only the
    /// second tie-break. `DP-9` (alphabetically last) sits above `DP-1`
    /// because its `y` is smaller — sorting by connector alone (ignoring `y`)
    /// would put `DP-1` first instead.
    ///
    /// Falsified by dropping `y` from the sort key.
    #[test]
    fn equal_x_orders_by_y_then_connector() {
        let columns = built_with_outputs(
            &[ws(1, 1, "DP-1", None), ws(2, 1, "DP-9", None)],
            &[],
            &no_stacks(),
            &[output_at("DP-1", 0, 1080), output_at("DP-9", 0, 0)],
        );
        assert_eq!(connectors(&columns), ["DP-9", "DP-1"]);
    }

    /// #1110: the offline column still trails every positioned column, even
    /// when a positioned connector would otherwise sort after it.
    #[test]
    fn offline_column_trails_a_positioned_set() {
        let file = saved(&[("chat", stack(Some("DP-9"), &["x"]))]);
        let columns = built_with_outputs(
            &[ws(1, 1, "DP-1", None)],
            &[],
            &file,
            &[output_at("DP-1", 3840, 0)],
        );
        assert_eq!(connectors(&columns), ["DP-1", super::OFFLINE_COLUMN]);
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
            &[],
        )
        .columns;
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

    /// #1133: a stack with **two** entries of the same app-id glows **per
    /// instance** — the first entry lights up with one window open, the
    /// second stays dim until a *second* window of that app-id exists.
    ///
    /// **The mutation**: glowing by set membership (`open.contains(app_id)`,
    /// the pre-#1133 rule) reds this — both entries would read `true` with
    /// only one Alacritty window open.
    #[test]
    fn a_saved_cards_second_instance_of_an_app_glows_only_with_a_second_window() {
        let file = saved(&[("term", stack(Some(LEFT), &["Alacritty", "Alacritty"]))]);

        let one_window = built(
            &[ws(1, 1, LEFT, Some("term"))],
            &[win(9, 1, "Alacritty", 1)],
            &file,
        );
        let card = &find(&one_window, LEFT).cards[0];
        assert_eq!(
            card.apps.iter().map(|a| a.running).collect::<Vec<_>>(),
            [true, false],
            "one window open: only the first entry glows"
        );

        let two_windows = built(
            &[ws(1, 1, LEFT, Some("term"))],
            &[win(9, 1, "Alacritty", 1), win(10, 1, "Alacritty", 2)],
            &file,
        );
        let card = &find(&two_windows, LEFT).cards[0];
        assert_eq!(
            card.apps.iter().map(|a| a.running).collect::<Vec<_>>(),
            [true, true],
            "two windows open: both entries glow"
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
        assert!(matches!(
            cards[0].kind,
            Kind::Ephemeral { workspace: 1, .. }
        ));
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
        assert!(matches!(
            cards[1].kind,
            Kind::Ephemeral { workspace: 2, .. }
        ));
    }

    /// **MEDIUM-7.** An ephemeral card's apps are in **niri's column order**,
    /// not alphabetical.
    ///
    /// This list is what a Save records, and #1071 §3.4 step 3 makes the stack's
    /// order *be* niri's column order — so `column_order_batch` would
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

    /// Two windows of one app are **two** entries (#1133 — an app can appear
    /// as many times as it has windows), and a window with no `app_id`
    /// contributes none — there is nothing to resolve a desktop entry from.
    ///
    /// **The mutation**: deduping `app_ids` (or `apps`) by `app_id` reds this —
    /// `firefox` would appear once instead of twice.
    #[test]
    fn an_ephemeral_cards_apps_are_one_per_window_and_skip_anonymous_windows() {
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
        assert_eq!(
            app_ids(&find(&columns, LEFT).cards[0]),
            ["firefox", "firefox", "mpv"]
        );
    }

    /// §3.7's two extra facts an ephemeral card has to carry so its Save can
    /// *be* a Save: the screen niri says it is on, and each window's pid.
    ///
    /// **The mutation**: dropping the pid (or taking it from a later snapshot)
    /// reds this — and, downstream, the unknown-`app_id` command line the Edit
    /// form prefills has nothing to read.
    #[test]
    fn an_ephemeral_card_carries_its_screen_and_its_windows_pids() {
        let mut firefox = win(9, 1, "firefox", 1);
        firefox.pid = Some(4242);
        let mut mpv = win(10, 1, "mpv", 2);
        mpv.pid = None;
        let columns = built(&[ws(1, 1, RIGHT, None)], &[firefox, mpv], &no_stacks());
        let Kind::Ephemeral {
            workspace,
            output,
            windows,
        } = &find(&columns, RIGHT).cards[0].kind
        else {
            panic!("an unnamed workspace with windows is an ephemeral card");
        };
        assert_eq!(*workspace, 1);
        assert_eq!(
            output, RIGHT,
            "§3.7 records the monitor niri reports, not the column it was drawn in"
        );
        assert_eq!(
            windows,
            &[("firefox".to_owned(), Some(4242)), ("mpv".to_owned(), None),],
            "the windows must arrive in column order, each with its own pid"
        );
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

    /// #1071 §5: a drag also moves the **live** workspace, but only while the
    /// stack is Active.
    ///
    /// **The mutation**: returning `card.live` unconditionally. An Inactive
    /// stack still has a niri workspace carrying its name — the lingering empty
    /// one niri has not cleaned up yet — so the drag would relocate that
    /// placeholder across screens, shuffling the workspace indices on both to
    /// move nothing the user can see.
    #[test]
    fn only_an_active_card_carries_a_workspace_to_move() {
        let workspaces = [ws_focused(1, 1, LEFT, Some("chat"))];
        let file = saved(&[("chat", stack(Some(LEFT), &["firefox"]))]);

        // Named, with a window: Active.
        let active = built(&workspaces, &[win(9, 1, "firefox", 1)], &file);
        let card = &find(&active, LEFT).cards[0];
        assert_eq!(card.kind, Kind::Saved(StackState::Active));
        assert_eq!(card.workspace_to_move(), Some(1));

        // Named, no window, no slice: Inactive — the lingering workspace.
        let inactive = built(&workspaces, &[], &file);
        let card = &find(&inactive, LEFT).cards[0];
        assert_eq!(card.kind, Kind::Saved(StackState::Inactive));
        assert_eq!(
            card.live,
            Some(1),
            "the lingering workspace is still there — so this is not vacuous"
        );
        assert_eq!(card.workspace_to_move(), None);
    }

    /// An ephemeral card has no entry in the file, so a drag has nothing to
    /// rewrite and it carries no workspace to move either.
    #[test]
    fn an_ephemeral_card_is_not_draggable() {
        let columns = built(
            &[ws_focused(1, 1, LEFT, None)],
            &[win(9, 1, "firefox", 1)],
            &no_stacks(),
        );
        let card = &find(&columns, LEFT).cards[0];
        assert!(matches!(card.kind, Kind::Ephemeral { workspace: 1, .. }));
        assert_eq!(card.live, None);
        assert_eq!(card.workspace_to_move(), None);
    }

    // ── #1071 §5, the drop path (review F5) ──────────────────────────────────
    //
    // The GTK handler is three lines and holds no decision, because a
    // `connect_drop` callback cannot be invoked from a test — so a branch left
    // inside it is a branch nothing can falsify, which is exactly how the
    // same-screen guard and the Active-workspace hand-off both shipped
    // unfalsifiable. Everything the drop decides now lives in `drop_plan`,
    // `reorder_onto` and `drop_context`, and all three are here.

    /// A card on `monitor`, with `workspace` if it is Active.
    fn droppable(monitor: Option<&str>, workspace: Option<u64>) -> Droppable {
        Droppable {
            monitor: monitor.map(str::to_owned),
            workspace,
        }
    }

    /// A card dropped on a screen it does not already record moves — and takes
    /// its live workspace with it when it has one.
    #[test]
    fn a_drop_on_another_screen_records_it_and_carries_an_active_workspace() {
        let ctx = context(
            &[
                ("chat", droppable(Some(LEFT), Some(7))),
                ("dev", droppable(Some(LEFT), None)),
            ],
            &["chat", "dev"],
        );

        assert_eq!(
            drop_plan("chat", Some(RIGHT), None, &ctx),
            Some(DropAction {
                name: "chat".to_owned(),
                monitor: Some(RIGHT.to_owned()),
                workspace: Some(7),
                order: None,
            }),
            "an Active stack's workspace follows it across screens"
        );
        assert_eq!(
            drop_plan("dev", Some(RIGHT), None, &ctx),
            Some(DropAction {
                name: "dev".to_owned(),
                monitor: Some(RIGHT.to_owned()),
                workspace: None,
                order: None,
            }),
            "an Inactive stack is the file change and nothing else"
        );
    }

    /// A card put back on the screen it already records changes nothing — so a
    /// two-pixel accidental drag does not rewrite `workspaces.toml`.
    #[test]
    fn a_drop_on_the_screen_it_already_records_does_nothing() {
        let ctx = context(&[("chat", droppable(Some(LEFT), Some(7)))], &["chat"]);
        assert_eq!(drop_plan("chat", Some(LEFT), None, &ctx), None);
        // …and neither does dropping it on **itself**, which is what a drag
        // that travelled two pixels lands on.
        assert_eq!(drop_plan("chat", Some(LEFT), Some("chat"), &ctx), None);
    }

    /// …but a stack that records **no** screen, dropped on the column it is
    /// merely drawn in, does write: that column is where the focused output
    /// happens to be, not a recorded choice, so the drop is the user saying
    /// "here, always".
    #[test]
    fn a_drop_pins_a_stack_that_recorded_no_screen() {
        let ctx = context(&[("chat", droppable(None, None))], &["chat"]);
        assert_eq!(
            drop_plan("chat", Some(LEFT), None, &ctx).and_then(|a| a.monitor),
            Some(LEFT.to_owned())
        );
    }

    /// A name that is not a saved card on this page is refused rather than
    /// guessed at.
    #[test]
    fn a_drop_of_something_that_is_not_a_card_is_refused() {
        assert_eq!(
            drop_plan("chat", Some(LEFT), None, &context(&[], &[])),
            None
        );
    }

    // ── #1071 §3.6, the in-column reorder (phase 4) ──────────────────────────

    /// §3.6: a drop on another card rewrites `order` — and **only** moves the
    /// dragged name, leaving every other card's relative position exactly as it
    /// was, on every screen.
    ///
    /// **The mutation** the brief names: a "global rewrite" that rebuilds the
    /// array as *this column's cards in their new order, then the rest*. With
    /// the two monitors' stacks interleaved below, that yields
    /// `[c, a, b, x, y]` instead of `[c, a, x, b, y]` — so this reds.
    #[test]
    fn a_reorder_moves_one_name_and_rewrites_no_other_monitors_stacks() {
        // a, b, c on DP-1; x, y on HDMI-A-1; interleaved in the file.
        let order = ["a", "x", "b", "y", "c"].map(str::to_owned);
        let moved = reorder_onto(&order, "c", "a").expect("the order changed");
        assert_eq!(
            names(&moved),
            ["c", "a", "x", "b", "y"],
            "the other screen's stacks were reshuffled"
        );
        // The other monitor's names keep both their relative order and their
        // neighbours among themselves.
        let others: Vec<&str> = names(&moved)
            .into_iter()
            .filter(|n| *n == "x" || *n == "y")
            .collect();
        assert_eq!(others, ["x", "y"]);
    }

    /// A one-place drag reads the way the pointer moved, in **both**
    /// directions: onto the card below puts it below, onto the card above puts
    /// it above.
    ///
    /// This is what fixes the index: taking the target's position *after* the
    /// removal instead would make the downward drag a no-op, because a card is
    /// already immediately before its own successor. Both assertions here failed
    /// on the first cut of `reorder_onto` for exactly that reason.
    #[test]
    fn a_one_place_drag_moves_in_the_direction_it_was_dragged() {
        let order = ["a", "b", "c"].map(str::to_owned);
        assert_eq!(
            names(&reorder_onto(&order, "a", "b").expect("changed")),
            ["b", "a", "c"],
            "dragging down onto the next card must land below it"
        );
        assert_eq!(
            names(&reorder_onto(&order, "c", "b").expect("changed")),
            ["a", "c", "b"],
            "dragging up onto the previous card must land above it"
        );
        // And the long drags, at both ends.
        assert_eq!(
            names(&reorder_onto(&order, "a", "c").expect("changed")),
            ["b", "c", "a"]
        );
        assert_eq!(
            names(&reorder_onto(&order, "c", "a").expect("changed")),
            ["c", "a", "b"]
        );
    }

    /// Nothing to do → no write. A name not in the order, a target not in it,
    /// and a drop that would reproduce the order are all `None`.
    #[test]
    fn a_reorder_that_changes_nothing_writes_nothing() {
        let order = ["a", "b", "c"].map(str::to_owned);
        assert_eq!(reorder_onto(&order, "a", "a"), None);
        assert_eq!(reorder_onto(&order, "zz", "a"), None);
        assert_eq!(reorder_onto(&order, "a", "zz"), None);
        assert!(reorder_onto(&[], "a", "b").is_none());
    }

    /// A drop on a card in **another** column does both halves at once: the
    /// screen and the position.
    #[test]
    fn a_drop_on_a_card_in_another_column_records_the_screen_and_the_order() {
        let ctx = context(
            &[
                ("chat", droppable(Some(LEFT), Some(7))),
                ("dev", droppable(Some(RIGHT), None)),
            ],
            &["chat", "dev"],
        );
        let action = drop_plan("chat", Some(RIGHT), Some("dev"), &ctx).expect("both halves");
        assert_eq!(action.monitor.as_deref(), Some(RIGHT));
        assert_eq!(action.workspace, Some(7));
        assert_eq!(names(&action.order.expect("reordered")), ["dev", "chat"]);
    }

    /// A drop **within** one column is the order alone: no `monitor`, and
    /// therefore no workspace for niri to move either.
    ///
    /// The dragged card is deliberately an **Active** one (review LOW 10 / the
    /// reviewer's surviving **M11**): with `droppable(Some(LEFT), None)` the
    /// card had no live workspace, so `workspace == None` held whether or not
    /// `drop_plan` gated the workspace on the monitor half — the assertion was
    /// vacuous and dropping the gate survived green.
    #[test]
    fn a_drop_within_one_column_rewrites_only_the_order() {
        let ctx = context(
            &[
                ("chat", droppable(Some(LEFT), Some(7))),
                ("dev", droppable(Some(LEFT), Some(9))),
            ],
            &["chat", "dev"],
        );
        let action = drop_plan("dev", Some(LEFT), Some("chat"), &ctx).expect("reordered");
        assert_eq!(
            action.monitor, None,
            "an in-column drop is not a screen change"
        );
        assert_eq!(
            action.workspace, None,
            "there is nothing for niri to do about a position in a list — and \
             this card really does have a live workspace (9) to have carried"
        );
        assert_eq!(names(&action.order.expect("reordered")), ["dev", "chat"]);
    }

    /// **Review HIGH 1**: the trailing "Not connected" column's heading is not a
    /// connector, so a drop onto a card there must record **no screen** — and
    /// must not ask niri to move a workspace to an output that does not exist.
    ///
    /// Before the fix `drop_plan` was handed the literal `OFFLINE_COLUMN`, and
    /// `card.monitor != Some("Not connected")` is true for every real stack, so
    /// the monitor half fired: `workspaces.toml` got `monitor = "Not connected"`
    /// (stranding the card in the greyed column until the file was hand-edited)
    /// and an Active stack got a bogus `MoveWorkspaceToMonitor`. Phase 3 stated
    /// the invariant for the *column* target; phase 4's card target routed
    /// around it.
    ///
    /// The reorder half still applies — those cards are still ordered.
    ///
    /// **The mutation**: taking `connector: &str` again, so the offline column
    /// passes its heading through, reds this on the first assertion.
    #[test]
    fn a_drop_onto_a_card_in_the_offline_column_records_no_screen() {
        let ctx = context(
            &[
                ("chat", droppable(Some("DP-9"), None)),
                ("dev", droppable(Some("DP-9"), Some(7))),
            ],
            &["chat", "dev"],
        );
        let action =
            drop_plan("chat", None, Some("dev"), &ctx).expect("the reorder half still applies");
        assert_eq!(
            action.monitor, None,
            "\"Not connected\" was written to the file as a connector"
        );
        assert_eq!(
            action.workspace, None,
            "niri was asked to move a workspace to an output that does not exist"
        );
        assert_eq!(names(&action.order.expect("reordered")), ["dev", "chat"]);

        // …and a drop there that would reorder nothing does nothing at all,
        // rather than falling through to a monitor rewrite.
        assert_eq!(drop_plan("chat", None, None, &ctx), None);
        assert_eq!(drop_plan("chat", None, Some("chat"), &ctx), None);
    }

    /// …and the column is what *decides* that `None` — the half `drop_plan`
    /// cannot see.
    ///
    /// The first cut of the HIGH 1 fix passed the `drop_plan` test above with
    /// the wiring still handing `OFFLINE_COLUMN` through, because that test
    /// supplies the `None` itself. This is the mutation-sensitive half.
    ///
    /// **The mutation**: `build_column` passing `Some(column.connector)`
    /// unconditionally reds this.
    #[test]
    fn only_a_connected_column_offers_a_connector_to_a_drop() {
        let columns = built(
            &[ws_focused(1, 1, LEFT, None)],
            &[],
            &saved(&[("gone", stack(Some("dp-9"), &["firefox"]))]),
        );
        let live = find(&columns, LEFT);
        let offline = find(&columns, OFFLINE_COLUMN);
        assert!(
            offline.offline,
            "the fixture must produce the trailing column"
        );

        assert_eq!(live.drop_connector(), Some(LEFT));
        assert_eq!(
            offline.drop_connector(),
            None,
            "the \"not connected\" heading was offered to a drop as a connector"
        );
    }

    /// §3.7: an ephemeral card's Edit opens on a draft that **records the
    /// screen** and carries the workspace Save has to name — and says nothing
    /// about a layout, an autostart or a previous name, because it has none.
    ///
    /// **The mutation**: dropping the `monitor` (or taking it from the focused
    /// output rather than the card's own) reds this — and on a machine whose
    /// focus is elsewhere, that is a Save that files the stack under the wrong
    /// screen.
    #[test]
    fn an_ephemeral_drafts_monitor_is_the_screen_its_workspace_is_on() {
        let apps = vec![crate::config::workspaces::StackApp {
            id: "weird-app".to_owned(),
            exec: Some("/home/me/bin/weird".to_owned()),
        }];
        let draft = super::ephemeral_draft(7, RIGHT, BTreeSet::new(), apps.clone());
        assert_eq!(
            draft.monitor.as_deref(),
            Some(RIGHT),
            "§3.7's 'record the monitor' was dropped"
        );
        assert_eq!(draft.workspace, Some(7), "Save has to name this workspace");
        assert_eq!(draft.apps, apps);
        assert_eq!(draft.previous, None, "an ephemeral Save is a creation");
        assert_eq!(draft.name, "", "it has no name until one is typed");
        assert_eq!(
            draft.layout,
            crate::config::workspaces::Layout::None,
            "§3.7: layout `none` unless known"
        );
        assert!(!draft.autostart);
        // Keyed by workspace id, which cannot collide with a stack name.
        assert_eq!(draft.key(), "#7");
    }

    /// The page-wide order a drop rewrites is the one the cards are drawn in —
    /// the file's order, across every screen, not a column's.
    #[test]
    fn the_drop_context_carries_the_files_order_across_every_screen() {
        let file = saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            ("dev", stack(Some(RIGHT), &["Alacritty"])),
            ("music", stack(Some(LEFT), &["mpv"])),
        ]);
        let ctx = drop_context(&page(
            &[ws_focused(1, 1, LEFT, None), ws(2, 1, RIGHT, None)],
            &[],
            &file,
            &[],
        ));
        assert_eq!(
            names(&ctx.order),
            ["chat", "dev", "music"],
            "the order was grouped by column instead of kept flat"
        );
    }

    /// The map the handler reads is built from the model: an Active card hands
    /// over its workspace, an Inactive one hands over `None`, and an ephemeral
    /// card is not in it at all.
    ///
    /// This is the other half of what shipped untested — `Card::workspace_to_move`
    /// was covered and the map that feeds the handler was not.
    #[test]
    fn droppable_cards_carries_the_recorded_screen_and_the_active_workspace() {
        let file = saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            ("dev", stack(Some(RIGHT), &["Alacritty"])),
        ]);
        let cards = drop_context(&page(
            &[
                // `chat` is live with a window → Active.
                ws_focused(1, 1, LEFT, Some("chat")),
                // `dev` is named but empty → Inactive, lingering.
                ws(2, 1, RIGHT, Some("dev")),
                // …and an unnamed, populated workspace → an ephemeral card.
                ws(3, 2, LEFT, None),
            ],
            &[win(9, 1, "firefox", 1), win(10, 3, "thunderbird", 1)],
            &file,
            &[],
        ))
        .cards;

        assert_eq!(
            cards.keys().collect::<Vec<_>>(),
            ["chat", "dev"],
            "saved cards only — an ephemeral card has no entry to rewrite"
        );
        assert_eq!(
            cards["chat"],
            Droppable {
                monitor: Some(LEFT.to_owned()),
                workspace: Some(1),
            }
        );
        assert_eq!(
            cards["dev"],
            Droppable {
                monitor: Some(RIGHT.to_owned()),
                workspace: None,
            },
            "Inactive: the lingering empty workspace is not moved"
        );
    }
}

#[cfg(all(test, feature = "system-tests"))]
pub(in crate::panels) mod tests {
    use super::fixtures::{LEFT, RIGHT, no_stacks, output_at, saved, stack, win, ws, ws_focused};
    use super::{
        APP_IDLE_CLASS, APP_RUNNING_CLASS, CARD_INACTIVE_CLASS, DisplayOutput, EMPTY_COLUMN_HINT,
        EPHEMERAL_NAME, NO_OUTPUTS_HINT, OFFLINE_COLUMN, PageModel, bind_columns, build_panel,
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

    /// The event controllers attached to `widget` itself.
    ///
    /// GTK4 has no "is this widget a drop target" flag — a target *is* a
    /// controller — so this is how #1071 §5's "which columns take a card" is
    /// read back. `observe_controllers` is the widget's own list, not its
    /// descendants', which is what makes the offline-column assertion mean
    /// something.
    fn controllers(widget: &gtk::Widget) -> Vec<gtk::EventController> {
        let list = widget.observe_controllers();
        (0..list.n_items())
            .filter_map(|i| list.item(i)?.downcast::<gtk::EventController>().ok())
            .collect()
    }

    fn has_drop_target(widget: &gtk::Widget) -> bool {
        controllers(widget)
            .iter()
            .any(ObjectExt::is::<gtk::DropTarget>)
    }

    fn has_drag_source(widget: &gtk::Widget) -> bool {
        controllers(widget)
            .iter()
            .any(ObjectExt::is::<gtk::DragSource>)
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
        outputs: Mutable<Vec<DisplayOutput>>,
    }

    fn fixture() -> Fixture {
        adw::init().expect("libadwaita init");
        let workspaces: Mutable<Vec<Workspace>> = Mutable::new(Vec::new());
        let windows: Mutable<Vec<Window>> = Mutable::new(Vec::new());
        let saved: Mutable<Workspaces> = Mutable::new(no_stacks());
        let slices_up: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let starting: Mutable<BTreeSet<String>> = Mutable::new(BTreeSet::new());
        let outputs: Mutable<Vec<DisplayOutput>> = Mutable::new(Vec::new());
        let page = build_panel(
            workspaces.signal_cloned(),
            windows.signal_cloned(),
            saved.signal_cloned(),
            slices_up.signal_cloned(),
            starting.signal_cloned(),
            outputs.signal_cloned(),
        );
        pump();
        Fixture {
            page,
            workspaces,
            windows,
            saved,
            slices_up,
            starting,
            outputs,
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
    ///
    /// Shared with `panels::workspace_edit`'s tests rather than copied there
    /// (review MEDIUM 5): one definition of the discipline, so the Edit page's
    /// geometry assertions cannot drift away from the card page's.
    pub(in crate::panels) fn assert_inside_and_hittable(
        outer: &gtk::Widget,
        inner: &gtk::Widget,
        what: &str,
    ) {
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

    /// §7: **one column per monitor**, side by side — and each column is
    /// actually on screen inside the page, not merely `visible`. No output
    /// positions are set here, so the columns fall back to the connector
    /// tie-break (`LEFT` = `"DP-1"` < `RIGHT` = `"HDMI-A-1"`); see
    /// [`columns_order_by_output_position`] for the case actually driven by
    /// position (#1110).
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

    /// #1110: rendered column order follows the outputs' logical position,
    /// not connector name. `RIGHT` (`"HDMI-A-1"`) is positioned at `x = 0`
    /// and `LEFT` (`"DP-1"`) at `x = 1920` — the reverse of connector order —
    /// so a regression to lexical sorting renders `LEFT` first instead.
    #[gtk::test]
    fn columns_order_by_output_position() {
        let f = fixture();
        f.workspaces.set(vec![
            ws(1, 1, RIGHT, Some("right-one")),
            ws(2, 1, LEFT, Some("left-one")),
        ]);
        f.outputs
            .set(vec![output_at(RIGHT, 0, 0), output_at(LEFT, 1920, 0)]);
        pump();
        let window = present(&f.page);

        let cols = columns(&f.page);
        assert_eq!(cols.len(), 2);
        assert_eq!(
            label_text(&cols[0], "ts-ws-column-title"),
            RIGHT,
            "RIGHT sits at x=0, so it renders first despite sorting after LEFT \
             by connector name"
        );
        assert_eq!(label_text(&cols[1], "ts-ws-column-title"), LEFT);

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

    /// An ephemeral card shows the live windows, one icon per window (#1133),
    /// carries no Start/Stop — it is already running — and offers **Edit** and
    /// nothing else (#1071 §3.7 / #1109).
    ///
    /// **#1109's own assertion**: nothing inline. Phase 2 put a `gtk::Entry` and
    /// a Save button on this card; Annika's ruling is that they go entirely, so
    /// this asserts there is **no entry anywhere on any card** rather than
    /// merely that the Edit button exists — a fallback left beside the button
    /// would satisfy the weaker claim.
    #[gtk::test]
    fn an_ephemeral_card_shows_its_windows_and_offers_only_edit() {
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
            3,
            "one icon per window (#1133) — two Term windows are two icons, \
             not one; the stack is a list of apps, one per window"
        );
        assert!(
            by_class(&card, "ts-ws-action").is_empty(),
            "there is nothing to Start: it is already running"
        );

        let edit = by_class(&card, "ts-ws-edit-open")
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Button>().ok())
            .expect("every card carries an Edit button (#1109)");
        assert!(edit.is_sensitive());
        assert_inside_and_hittable(&card, edit.upcast_ref(), "the Edit button");

        // #1109: "nightmare to render" — nothing inline on any card, and no
        // fallback beside the button either.
        let inline: Vec<gtk::Widget> = by_class(&f.page, "ts-ws-card")
            .into_iter()
            .flat_map(|card| {
                fn walk(widget: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
                    if widget.is::<gtk::Entry>() || widget.is::<gtk::Text>() {
                        out.push(widget.clone());
                    }
                    let mut child = widget.first_child();
                    while let Some(c) = child {
                        walk(&c, out);
                        child = c.next_sibling();
                    }
                }
                let mut out = Vec::new();
                walk(&card, &mut out);
                out
            })
            .collect();
        assert!(
            inline.is_empty(),
            "#1109: no card may carry an inline text field — found {}",
            inline.len()
        );

        window.destroy();
    }

    /// Every **saved** card carries an Edit button too, beside its Start/Stop —
    /// Annika's `[start/stop] [edit]` (#1109).
    #[gtk::test]
    fn every_saved_card_carries_an_edit_button_beside_its_action() {
        let f = fixture();
        f.workspaces.set(vec![ws(1, 1, LEFT, None)]);
        f.saved.set(saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            ("dev", stack(Some(LEFT), &["Alacritty"])),
        ]));
        pump();
        let window = present(&f.page);

        let all = cards(&f.page);
        assert_eq!(all.len(), 2);
        for card in &all {
            let edit = by_class(card, "ts-ws-edit-open")
                .into_iter()
                .find_map(|w| w.downcast::<gtk::Button>().ok())
                .expect("a saved card carries an Edit button");
            assert_inside_and_hittable(card, edit.upcast_ref(), "the Edit button");
            let action = by_class(card, "ts-ws-action")
                .into_iter()
                .next()
                .expect("…and still its Start/Stop");

            // Annika's own spelling is `[start/stop] [edit]` (#1109), and the
            // **order** is the part review LOW 14 found unasserted — both
            // existing is what the weaker version checked, which a swap would
            // have passed.
            assert!(
                edit.prev_sibling().as_ref() == Some(&action),
                "the Edit button must follow the Start/Stop one, not precede it"
            );
            let (left, right) = (
                bounds_in(card, &action, "the Start/Stop button"),
                bounds_in(card, edit.upcast_ref(), "the Edit button"),
            );
            assert!(
                left.x() < right.x(),
                "…and be drawn to its right: action at {left:?}, edit at {right:?}"
            );
        }

        window.destroy();
    }

    /// §3.6, phase 4: a saved card takes a drop as well as starting a drag, so a
    /// card can be dropped **onto** another card to reorder it.
    ///
    /// Phase 3 shipped the drag source and the column target; the card target is
    /// what this phase adds, and without it an in-column drag has nowhere to
    /// land. Falsified by not attaching `card_drop_target`.
    #[gtk::test]
    fn a_saved_card_takes_a_drop_so_it_can_be_reordered_onto() {
        let f = fixture();
        f.workspaces.set(vec![ws(1, 1, LEFT, None)]);
        f.saved.set(saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            ("dev", stack(Some(LEFT), &["Alacritty"])),
        ]));
        f.windows.set(vec![win(9, 1, "mpv", 1)]);
        pump();

        let saved_cards: Vec<gtk::Widget> = cards(&f.page)
            .into_iter()
            .filter(|c| !by_class(c, "ts-ws-action").is_empty())
            .collect();
        assert_eq!(saved_cards.len(), 2, "two saved cards");
        for card in &saved_cards {
            assert!(
                has_drop_target(card),
                "a saved card must take a drop, or it cannot be reordered onto"
            );
            assert!(has_drag_source(card), "…and still start one");
        }

        // The ephemeral card takes neither: it has no entry in the file, so
        // there is nothing to reorder and nothing to rewrite.
        let ephemeral = cards(&f.page)
            .into_iter()
            .find(|c| by_class(c, "ts-ws-action").is_empty())
            .expect("the unnamed workspace is a card too");
        assert!(!has_drop_target(&ephemeral));
        assert!(!has_drag_source(&ephemeral));
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
        let model: Mutable<PageModel> = Mutable::new(PageModel::default());
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

    /// Every connected monitor's column takes a drop; the "Not connected"
    /// column does not (#1071 §5).
    ///
    /// A drop target on the offline column would look like an affordance and
    /// then record a connector nobody has, so the very next model rebuild would
    /// put the card straight back where it came from.
    #[gtk::test]
    fn every_monitor_column_takes_a_drop_and_the_offline_one_does_not() {
        let f = fixture();
        f.workspaces
            .set(vec![ws(1, 1, LEFT, None), ws(2, 1, RIGHT, None)]);
        f.saved.set(saved(&[
            ("chat", stack(Some(LEFT), &["firefox"])),
            // Names a screen that is not here → the trailing offline column.
            ("gone", stack(Some("dp-9"), &["Alacritty"])),
        ]));
        pump();

        let all = columns(&f.page);
        assert_eq!(all.len(), 3, "two screens plus the offline column");
        for column in &all[..2] {
            assert!(
                has_drop_target(column),
                "a connected monitor's column takes a drop"
            );
        }
        assert_eq!(
            label_text(&all[2], "ts-ws-column-title"),
            OFFLINE_COLUMN,
            "the third really is the offline one"
        );
        assert!(
            !has_drop_target(&all[2]),
            "the offline column is not a screen and takes no drop"
        );
    }

    /// #1121 gap A: the offline column's own **cards** still take a drop —
    /// the reorder half applies there even though the monitor half does not
    /// (`build_column`'s comment on the loop above; `drop_plan` declines the
    /// monitor half on a `None` connector and keeps the reorder one).
    ///
    /// This is the wiring the re-verification of #1113 found untested: a
    /// mutation that has `build_card` reach for
    /// `Some(column.connector.as_str())` instead of asking `column` itself
    /// leaves every existing test green, because `Column::drop_connector`'s
    /// own test (`only_a_connected_column_offers_a_connector_to_a_drop`)
    /// supplies the `None` itself rather than exercising this call site, and
    /// `has_drop_target` cannot distinguish *which* connector a card's drop
    /// target closes over — only whether one is attached at all. The reshape
    /// (`build_card` takes `&Column` and calls `drop_connector()` itself, one
    /// call site) is what actually closes the gap; this test pins that the
    /// reorder half the reshape must not break stays working.
    #[gtk::test]
    fn a_saved_card_in_the_offline_column_still_takes_a_drop_for_reordering() {
        let f = fixture();
        // `model` builds no columns at all — not even the offline one —
        // unless niri reports at least one connected output.
        f.workspaces.set(vec![ws(1, 1, LEFT, None)]);
        f.saved
            .set(saved(&[("gone", stack(Some("dp-9"), &["Alacritty"]))]));
        pump();

        let all = columns(&f.page);
        let offline = all.last().expect("at least the offline column");
        assert_eq!(
            label_text(offline, "ts-ws-column-title"),
            OFFLINE_COLUMN,
            "the fixture must produce the trailing offline column"
        );
        let its_cards = cards(offline);
        assert_eq!(its_cards.len(), 1, "the one offline stack's card");
        assert!(
            has_drop_target(&its_cards[0]),
            "the offline column's card must still accept a drop for reordering"
        );
    }

    /// A saved card is draggable; an ephemeral one is not (#1071 §5 — there is
    /// no file entry for a drop to rewrite).
    #[gtk::test]
    fn only_saved_cards_carry_a_drag_source() {
        let f = fixture();
        f.workspaces
            .set(vec![ws(1, 1, LEFT, Some("chat")), ws(2, 2, LEFT, None)]);
        // The unnamed workspace needs a window to be a card at all.
        f.windows.set(vec![win(9, 2, "Alacritty", 1)]);
        f.saved
            .set(saved(&[("chat", stack(Some(LEFT), &["firefox"]))]));
        pump();

        let column = &columns(&f.page)[0];
        let cards = cards(column);
        assert_eq!(cards.len(), 2, "one saved, one ephemeral");
        assert!(
            has_drag_source(&cards[0]),
            "the saved card can be dragged to another screen"
        );
        assert!(
            !has_drag_source(&cards[1]),
            "the ephemeral card cannot: it has nothing in the file to rewrite"
        );
    }

    /// Cards inside a column follow the file's `order`, not the alphabet
    /// (#1071 §3.6) — and an Inactive one keeps its place among the Active
    /// ones.
    #[gtk::test]
    fn cards_in_a_column_follow_the_file_order() {
        let f = fixture();
        f.workspaces
            .set(vec![ws(1, 1, LEFT, Some("music")), ws(2, 2, LEFT, None)]);
        f.windows.set(vec![win(9, 1, "spotify", 1)]);
        // `order` is zoo, music, apt — not alphabetical — and `music` is the
        // only Active one, so an Inactive card has to hold its place on
        // either side of it.
        f.saved.set(saved(&[
            ("zoo", stack(Some(LEFT), &["firefox"])),
            ("music", stack(Some(LEFT), &["spotify"])),
            ("apt", stack(Some(LEFT), &["Alacritty"])),
        ]));
        pump();

        let column = &columns(&f.page)[0];
        let names: Vec<String> = cards(column)
            .iter()
            .map(|card| label_text(card, "ts-ws-card-name"))
            .collect();
        assert_eq!(
            names,
            ["zoo", "music", "apt"],
            "the file's order, with the Inactive cards keeping their places"
        );
    }

    /// #1119: the double-size icons wrap onto a second row instead of
    /// clipping past the card's edge or shrinking to squeeze in.
    ///
    /// 330 px is the width `build_column`'s own doc comment measures for a
    /// two-screen drawer column ("would squeeze a two-screen setup to ~330px
    /// a column") — comfortably narrower than twelve 32 px icons plus their
    /// spacing need in one row, so the wrap is not a near thing.
    ///
    /// **The mutation**: building `.ts-ws-apps` as a plain `gtk::Box` instead
    /// of a `gtk::FlowBox` reds this — the twelfth icon renders past the
    /// card's right edge instead of wrapping, which `assert_inside_and_hittable`
    /// catches, and every icon lands on one row, which the row-count assertion
    /// catches independently.
    #[gtk::test]
    fn a_crowded_stack_wraps_its_icons_onto_a_second_row() {
        let f = fixture();
        f.workspaces.set(vec![ws_focused(1, 1, LEFT, None)]);
        f.saved.set(saved(&[(
            "chat",
            stack(
                Some(LEFT),
                &["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"],
            ),
        )]));
        pump();

        let window = gtk::Window::new();
        window.set_child(Some(&f.page));
        window.set_default_size(330, 600);
        window.present();
        pump();

        let column = &columns(&f.page)[0];
        let card = &cards(column)[0];
        let apps = icons(card);
        assert_eq!(apps.len(), 12, "all twelve apps must be on the card");

        for (i, icon) in apps.iter().enumerate() {
            assert_inside_and_hittable(card, icon, &format!("icon {i}"));
        }

        let ys: Vec<f32> = apps
            .iter()
            .map(|icon| bounds_in(card, icon, "an app icon").y())
            .collect();
        let (min_y, max_y) = ys
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), &y| (lo.min(y), hi.max(y)));
        assert!(
            max_y - min_y > 1.0,
            "all twelve icons landed on one row (y {min_y}..{max_y}) — nothing wrapped"
        );
    }
}
