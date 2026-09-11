//! Workspaces drawer page — phase 1 of the workspace-stacks epic (#1071 §6.1),
//! designed on discussion #1063.
//!
//! **Read-only.** One column per monitor, side by side; in each column, one
//! card per *named* niri workspace on that output; on each card, the name and a
//! strip of app icons for the windows currently open there, resolved through
//! [`crate::components::app_meta::resolve_app_meta`]. There is no file, no
//! `+` button, no start/stop, no edit — those are phases 2–4 (#1071 §6), and
//! every one of them lands on top of this page rather than beside it.
//!
//! ## Where the columns come from
//!
//! From niri, not from GDK. Every `Workspace` carries the `output` it lives on,
//! and niri keeps at least one workspace per connected output, so folding the
//! workspace list by `output` yields exactly one column per monitor — from the
//! *same* snapshot the cards are built from. Joining a second monitor source
//! would only introduce a window where the two disagree and a column has no
//! workspaces or a workspace has no column; `App::monitors()` is not reachable
//! from a panel anyway, since `modal::build_page` hands a page no `&Monitor`.
//!
//! Column order is lexical by connector, which is the order
//! `hytte::services::displays::outputs()` already sorts outputs into, so the
//! Workspaces page and the Displays page list the same screens the same way.
//! Phase 3 (#1071 §3.6) gives cards a saved order *inside* a column; the column
//! order itself stays a property of the connector set.
//!
//! A workspace whose `output` is `None` — niri's answer when no outputs are
//! connected at all — has no column to sit in and is dropped. Phase 2's
//! trailing "monitor not connected" column (#1071 §5) is about *saved* stacks
//! whose recorded monitor is absent, which is a different thing and needs the
//! file this phase deliberately does not have.
//!
//! ## The glow
//!
//! [`StackApp::running`] is the Active/Inactive bit the cards carry. In phase 1
//! every icon on a card comes from a window that is open right now, so it is
//! always `true` — but the class flip is wired anyway
//! ([`apply_running_class`]), because phase 2's greying of a stopped stack is
//! exactly this field going `false` and nothing else changing.
//!
//! ## Testability seam
//!
//! [`panel_workspaces`] only supplies the two niri signals; [`build_panel`]
//! takes them generically and [`model`] is a pure function of one
//! `(workspaces, windows)` snapshot. Both service accessors `.expect()` a
//! registered `Registry`, so this split is what lets the page be driven from a
//! bare `#[gtk::test]` with two `Mutable`s — the same seam
//! `widgets::workspaces::bind_workspace_pills` and
//! `panels::bluetooth::bind_device_groups` carve for the same reason.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use hytte::futures_signals::map_ref;
use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, pango, prelude::*};
use hytte::prelude::*;
use hytte::services::niri::{self, Window, Workspace};

use crate::components::app_meta::{MetaCache, fallback_icon, resolve_app_meta};
use crate::components::layout::{
    DRAWER_MAX_WIDTH_WIDE, finish_page_clamped, page_box, toggle_class,
};

/// CSS class on an app icon whose app has at least one window open on the
/// card's workspace. Phase 1 puts it on every icon; see the module doc.
const APP_RUNNING_CLASS: &str = "ts-ws-app-running";

/// The complement of [`APP_RUNNING_CLASS`] — a saved app of the stack with no
/// window open. Unreachable in phase 1 (nothing saves a stack yet), applied by
/// the same helper so phase 2 changes one `bool` rather than this file.
const APP_IDLE_CLASS: &str = "ts-ws-app-idle";

/// Shown in a monitor's column when that monitor has no *named* workspaces.
/// Without it the column reads as broken rather than empty, since an unnamed
/// workspace is not a card in phase 1 (#1071 §3.7 makes it one in phase 2,
/// through Edit → Save).
const EMPTY_COLUMN_HINT: &str = "No named workspaces on this screen";

/// Shown instead of the columns when niri has reported no outputs yet — the
/// first moments after a shell start, or a lost IPC socket.
const NO_OUTPUTS_HINT: &str = "Waiting for niri\u{2026}";

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
    /// At least one window of this app is open on the card's workspace.
    /// Always `true` in phase 1; see the module doc.
    running: bool,
}

/// One named niri workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Card {
    /// The workspace's niri name. A card exists only because this is `Some`.
    name: String,
    /// The apps with windows on the workspace, in niri's own column order.
    apps: Vec<StackApp>,
}

/// One monitor.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Column {
    /// The output's connector, e.g. `"DP-1"`.
    connector: String,
    /// The named workspaces on this output, in niri's `idx` order.
    cards: Vec<Card>,
}

/// The page's whole model, from one niri snapshot. Pure — no GTK, no registry.
///
/// * One [`Column`] per output niri reports a workspace on, ordered by
///   connector.
/// * One [`Card`] per **named** workspace on that output, ordered by `idx`
///   (niri's `WorkspacesChanged` does not guarantee list order — the same
///   reason `widgets::workspaces` sorts).
/// * One [`StackApp`] per distinct `app_id` among that workspace's windows,
///   ordered by `pos_in_scrolling_layout` — niri's column order, which #1071
///   §3.4 settles as the stack order phase 3 will restore.
///
/// A window with no `app_id` contributes no icon: there is nothing to resolve a
/// desktop entry from. Phase 2's "no desktop entry" greying (#1071 §4) is about
/// an `app_id` that names no entry, which is a different case and does render.
fn model(workspaces: &[Workspace], windows: &[Window]) -> Vec<Column> {
    // `BTreeMap` rather than a sort afterwards: the connector ordering *is* the
    // column ordering, so putting it in the collection type means no later edit
    // can drop the sort without also losing the grouping.
    let mut by_output: BTreeMap<&str, Vec<&Workspace>> = BTreeMap::new();
    for ws in workspaces {
        let Some(output) = ws.output.as_deref() else {
            continue;
        };
        by_output.entry(output).or_default().push(ws);
    }

    by_output
        .into_iter()
        .map(|(connector, mut on_output)| {
            on_output.sort_by_key(|ws| ws.idx);
            let cards = on_output
                .into_iter()
                .filter_map(|ws| {
                    let name = ws.name.clone()?;
                    Some(Card {
                        name,
                        apps: stack_apps(ws.id, windows),
                    })
                })
                .collect();
            Column {
                connector: connector.to_owned(),
                cards,
            }
        })
        .collect()
}

/// The distinct apps with a window on workspace `workspace_id`, in niri's
/// column order (leftmost first), first occurrence winning for a repeated app.
fn stack_apps(workspace_id: u64, windows: &[Window]) -> Vec<StackApp> {
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
            seen.insert(app_id).then(|| StackApp {
                app_id: app_id.to_owned(),
                running: true,
            })
        })
        .collect()
}

pub fn panel_workspaces() -> gtk::Widget {
    build_panel(niri::workspaces(), niri::windows())
}

/// [`panel_workspaces`] with the two niri signals injected, so a test can drive
/// the page without a registered `Registry`.
fn build_panel<W, N>(workspaces: W, windows: N) -> gtk::Widget
where
    W: Signal<Item = Vec<Workspace>> + 'static,
    N: Signal<Item = Vec<Window>> + 'static,
{
    let columns_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    columns_box.add_css_class("ts-ws-columns");
    columns_box.set_homogeneous(true);
    columns_box.set_valign(gtk::Align::Start);

    let combined = map_ref! {
        let workspaces = workspaces,
        let windows = windows => model(workspaces, windows)
    }
    // `Window` does not derive `PartialEq` (only `Workspace` does), so the
    // *inputs* cannot be deduped — but the model can, and it is what the
    // rebuild costs. Without this every window-title change on any workspace
    // tears down and rebuilds every card on every monitor.
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
    outer.append(&title);

    if column.cards.is_empty() {
        outer.append(&hint(EMPTY_COLUMN_HINT));
    } else {
        for card in &column.cards {
            outer.append(&build_card(card, meta_cache));
        }
    }

    outer.upcast()
}

fn build_card(card: &Card, meta_cache: &MetaCache) -> gtk::Widget {
    // `.ts-panel` is the shell's card surface (`components::layout::section`
    // paints the same one); `.ts-ws-card` is the hook for this page's own
    // spacing and for phase 2's greying of a whole Inactive card.
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    outer.add_css_class("ts-panel");
    outer.add_css_class("ts-ws-card");

    let name = gtk::Label::new(Some(&card.name));
    name.add_css_class("ts-ws-card-name");
    name.set_xalign(0.0);
    name.set_ellipsize(pango::EllipsizeMode::End);
    outer.append(&name);

    let apps = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    apps.add_css_class("ts-ws-apps");
    for app in &card.apps {
        apps.append(&build_app_icon(app, meta_cache));
    }
    outer.append(&apps);

    outer.upcast()
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

/// The glow/dim flip. Phase 2's Inactive card is this, called with `false`.
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
    use hytte::services::niri::{Window, WindowLayout, Workspace};

    pub(super) const LEFT: &str = "DP-1";
    pub(super) const RIGHT: &str = "HDMI-A-1";

    /// A workspace on `output`, named iff `name` is `Some`.
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
}

#[cfg(test)]
mod model_tests {
    use super::fixtures::{LEFT, RIGHT, win, ws};
    use super::{Card, Column, StackApp, model};

    fn connectors(columns: &[Column]) -> Vec<&str> {
        columns.iter().map(|c| c.connector.as_str()).collect()
    }

    fn card_names(column: &Column) -> Vec<&str> {
        column.cards.iter().map(|c| c.name.as_str()).collect()
    }

    fn app_ids(card: &Card) -> Vec<&str> {
        card.apps.iter().map(|a| a.app_id.as_str()).collect()
    }

    /// The per-monitor split: one column per output niri reports a workspace
    /// on, ordered by connector, each holding only its own output's cards.
    ///
    /// Falsified by collapsing the fold to a single column.
    #[test]
    fn one_column_per_output_in_connector_order() {
        let columns = model(
            &[
                // Deliberately not in connector order in the input: niri's
                // `WorkspacesChanged` does not promise one.
                ws(1, 1, RIGHT, Some("right-one")),
                ws(2, 1, LEFT, Some("left-one")),
                ws(3, 2, LEFT, Some("left-two")),
            ],
            &[],
        );

        assert_eq!(connectors(&columns), [LEFT, RIGHT]);
        assert_eq!(card_names(&columns[0]), ["left-one", "left-two"]);
        assert_eq!(card_names(&columns[1]), ["right-one"]);
    }

    /// Cards within a column follow niri's `idx`, whatever order the list
    /// arrived in.
    #[test]
    fn cards_follow_workspace_idx_not_list_order() {
        let columns = model(
            &[
                ws(1, 3, LEFT, Some("third")),
                ws(2, 1, LEFT, Some("first")),
                ws(3, 2, LEFT, Some("second")),
            ],
            &[],
        );

        assert_eq!(card_names(&columns[0]), ["first", "second", "third"]);
    }

    /// A NAMED workspace is a card; an unnamed one is not. That is the whole of
    /// what "saved" means in phase 1, so it is the load-bearing filter.
    ///
    /// Falsified by dropping `model`'s `ws.name.clone()?` guard: the unnamed
    /// workspaces then become (nameless) cards and the count is 3.
    #[test]
    fn a_named_workspace_is_a_card_and_an_unnamed_one_is_not() {
        let columns = model(
            &[
                ws(1, 1, LEFT, None),
                ws(2, 2, LEFT, Some("chat")),
                ws(3, 3, LEFT, None),
            ],
            &[],
        );

        assert_eq!(
            card_names(&columns[0]),
            ["chat"],
            "only the named workspace is a card — an unnamed one is ephemeral \
             until #1071 phase 2's Edit → Save gives it a name"
        );
        assert_eq!(
            connectors(&columns),
            [LEFT],
            "the unnamed workspaces still establish that the output exists, so \
             the column stays even though it holds one card"
        );
    }

    /// An output whose workspaces are all unnamed still gets its column — the
    /// column is the monitor, not the cards.
    #[test]
    fn an_output_with_no_named_workspaces_still_gets_an_empty_column() {
        let columns = model(&[ws(1, 1, LEFT, None), ws(2, 1, RIGHT, Some("chat"))], &[]);

        assert_eq!(connectors(&columns), [LEFT, RIGHT]);
        assert!(
            columns[0].cards.is_empty(),
            "the empty column is what carries the page's per-monitor hint"
        );
    }

    /// A card's apps are the windows on *its* workspace, in niri's column
    /// order, one icon per distinct app.
    #[test]
    fn apps_are_this_workspaces_windows_in_column_order_deduped() {
        let columns = model(
            &[ws(7, 1, LEFT, Some("dev")), ws(8, 2, LEFT, Some("chat"))],
            &[
                win(3, 7, "com.example.Editor", 2),
                win(1, 7, "com.example.Term", 1),
                // A second window of an app already in column 1 — one icon.
                win(2, 7, "com.example.Term", 1),
                // Another workspace's window must not leak into `dev`.
                win(4, 8, "com.example.Chat", 1),
            ],
        );

        assert_eq!(
            app_ids(&columns[0].cards[0]),
            ["com.example.Term", "com.example.Editor"],
            "leftmost niri column first, and two windows of one app are one app"
        );
        assert_eq!(app_ids(&columns[0].cards[1]), ["com.example.Chat"]);
    }

    /// Every icon phase 1 renders stands for a live window, so it glows. The
    /// field exists so phase 2 can set it `false`; this pins today's value so a
    /// regression that greys a running app is visible.
    #[test]
    fn every_app_of_a_live_workspace_is_running() {
        let columns = model(
            &[ws(7, 1, LEFT, Some("dev"))],
            &[win(1, 7, "com.example.Term", 1)],
        );

        assert_eq!(
            columns[0].cards[0].apps,
            [StackApp {
                app_id: "com.example.Term".to_owned(),
                running: true,
            }]
        );
    }

    /// A window with no `app_id` has nothing to resolve a desktop entry from,
    /// so it contributes no icon — and does not blank the card either.
    #[test]
    fn a_window_without_an_app_id_contributes_no_icon() {
        let mut anonymous = win(1, 7, "unused", 1);
        anonymous.app_id = None;
        let columns = model(
            &[ws(7, 1, LEFT, Some("dev"))],
            &[anonymous, win(2, 7, "com.example.Term", 2)],
        );

        assert_eq!(app_ids(&columns[0].cards[0]), ["com.example.Term"]);
    }

    /// No outputs connected: niri reports `output: None`. There is no column to
    /// put those workspaces in, and the page shows its waiting hint instead.
    #[test]
    fn workspaces_with_no_output_produce_no_columns() {
        let mut orphan = ws(1, 1, LEFT, Some("chat"));
        orphan.output = None;

        assert!(model(&[orphan], &[]).is_empty());
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::fixtures::{LEFT, RIGHT, win, ws};
    use super::{
        APP_IDLE_CLASS, APP_RUNNING_CLASS, Column, EMPTY_COLUMN_HINT, NO_OUTPUTS_HINT,
        bind_columns, build_panel,
    };
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, graphene, prelude::*};
    use hytte::services::niri::{Window, Workspace};

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
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
        by_class(scope, class)
            .into_iter()
            .find_map(|w| w.downcast::<gtk::Label>().ok())
            .map(|l| l.text().to_string())
            .unwrap_or_else(|| panic!("no .{class} label under the scope"))
    }

    /// The page, plus the two handles that drive it.
    struct Fixture {
        page: gtk::Widget,
        workspaces: Mutable<Vec<Workspace>>,
        windows: Mutable<Vec<Window>>,
    }

    fn fixture() -> Fixture {
        adw::init().expect("libadwaita init");
        let workspaces: Mutable<Vec<Workspace>> = Mutable::new(Vec::new());
        let windows: Mutable<Vec<Window>> = Mutable::new(Vec::new());
        let page = build_panel(workspaces.signal_cloned(), windows.signal_cloned());
        pump();
        Fixture {
            page,
            workspaces,
            windows,
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

    /// §7: a **named** workspace becomes a card, an unnamed one does not — and
    /// the empty column says so rather than rendering as broken.
    ///
    /// Falsified by dropping `model`'s `name.is_some()` filter: the unnamed
    /// workspaces then render nameless cards and both counts are wrong.
    #[gtk::test]
    fn a_named_workspace_becomes_a_card_and_an_unnamed_one_does_not() {
        let f = fixture();
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
            "the unnamed workspace on {LEFT} must not be a card in phase 1"
        );
        assert_eq!(label_text(&left_cards[0], "ts-ws-card-name"), "chat");
        assert_inside_and_hittable(&cols[0], &left_cards[0], "the `chat` card");

        assert!(
            cards(&cols[1]).is_empty(),
            "{RIGHT} has only unnamed workspaces, so it has no cards"
        );
        assert_eq!(
            label_text(&cols[1], "ts-ws-empty"),
            EMPTY_COLUMN_HINT,
            "a column with no named workspaces shows the hint"
        );

        window.destroy();
    }

    /// §7: **the glow follows the windows signal.** Opening a window on a named
    /// workspace adds its icon (glowing); closing it takes the icon away.
    ///
    /// Falsified by dropping `windows` from `build_panel`'s `map_ref!` (pass
    /// `&[]` to `model` instead): the card then never grows an icon and the
    /// first assertion after the open fails.
    ///
    /// Counts and classes, not resolved icon names: what `resolve_app_meta`
    /// returns depends on which desktop files the harness happens to have
    /// installed. The identity of an app in the model is pinned by
    /// `model_tests` instead.
    #[gtk::test]
    fn card_icons_follow_the_windows_signal() {
        let f = fixture();
        f.workspaces.set(vec![ws(7, 1, LEFT, Some("dev"))]);
        pump();
        let window = present(&f.page);

        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        assert!(
            icons(&card).is_empty(),
            "an empty workspace's card shows no app icons"
        );

        f.windows.set(vec![win(1, 7, "com.example.Term", 1)]);
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        let after_open = icons(&card);
        assert_eq!(after_open.len(), 1, "opening a window adds its app's icon");
        assert!(
            after_open[0].has_css_class(APP_RUNNING_CLASS),
            "an app with a window open on the workspace glows"
        );
        assert!(
            !after_open[0].has_css_class(APP_IDLE_CLASS),
            "the running and idle classes are mutually exclusive, so phase 2's \
             greying is a flip rather than an addition"
        );
        assert!(
            after_open[0]
                .tooltip_text()
                .is_some_and(|t| !t.trim().is_empty()),
            "each icon names its app, resolved or raw"
        );
        assert_inside_and_hittable(&card, &after_open[0], "the app icon");

        // A second window of a *different* app is a second icon…
        f.windows.set(vec![
            win(1, 7, "com.example.Term", 1),
            win(2, 7, "com.example.Editor", 2),
        ]);
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        assert_eq!(icons(&card).len(), 2);

        // …and a second window of the *same* app is not.
        f.windows.set(vec![
            win(1, 7, "com.example.Term", 1),
            win(3, 7, "com.example.Term", 2),
        ]);
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        assert_eq!(
            icons(&card).len(),
            1,
            "two windows of one app are one icon — the stack is a set of apps"
        );

        f.windows.set(Vec::new());
        pump();
        let card = cards(&f.page).first().cloned().expect("the `dev` card");
        assert!(
            icons(&card).is_empty(),
            "closing the last window takes the icon away again"
        );

        window.destroy();
    }

    /// A rename in niri is a live edit of the card, not a restart.
    #[gtk::test]
    fn renaming_a_workspace_renames_its_card() {
        let f = fixture();
        f.workspaces.set(vec![ws(7, 1, LEFT, Some("dev"))]);
        pump();
        assert_eq!(
            label_text(&cards(&f.page)[0], "ts-ws-card-name"),
            "dev",
            "the named workspace starts as a card called `dev`"
        );

        f.workspaces.set(vec![ws(7, 1, LEFT, Some("chat"))]);
        pump();
        assert_eq!(label_text(&cards(&f.page)[0], "ts-ws-card-name"), "chat");

        // …and unsetting the name retires the card without losing the column.
        f.workspaces.set(vec![ws(7, 1, LEFT, None)]);
        pump();
        assert!(cards(&f.page).is_empty());
        assert_eq!(columns(&f.page).len(), 1);
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
            f.page.parent().is_some_and(|p| p == stack.clone().upcast()),
            "a page that is not the visible child is still a child of the stack"
        );

        stack.set_visible_child_name("workspaces");
        pump();
        assert_eq!(stack.visible_child_name().as_deref(), Some("workspaces"));
        assert!(
            stack
                .child_by_name("media")
                .is_some_and(|w| w == media.clone().upcast()),
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
