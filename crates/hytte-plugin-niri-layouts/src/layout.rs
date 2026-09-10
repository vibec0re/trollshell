//! The three layouts, their column proportions, and the **pure planner**.
//!
//! Everything here is I/O-free: [`plan`] takes the three niri snapshots the
//! caller already fetched and returns the exact `(window id, proportion)` pairs
//! to send, so the whole column rule is unit-testable without a compositor, a
//! socket, or an env var. [`niri::apply`](crate::niri::apply) is the only thing
//! that talks to niri, and it does nothing this module has not decided.

use niri_ipc::{Window, Workspace};
use std::collections::BTreeMap;

/// The wide column's share under [`Layout::Golden`] — 1/φ, rounded to the three
/// digits the golden ratio is usually written with.
pub(crate) const GOLDEN_MAJOR: f64 = 0.618;

/// Every other column's share under [`Layout::Golden`] — 1 − 1/φ.
pub(crate) const GOLDEN_MINOR: f64 = 0.382;

/// Every column's share under [`Layout::Split`].
pub(crate) const SPLIT_SHARE: f64 = 0.5;

/// One of the three arrangements the chip and the CLI both offer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Layout {
    /// Every column the same width: `1/n` each (so `n = 1` is a full-width
    /// column).
    Equal,
    /// The leftmost column wide, every other one narrow — issue #1019's
    /// `[========] [====] ( .... ) [====]` sketch.
    Golden,
    /// Every column half the working area, whatever `n` is.
    Split,
}

impl Layout {
    /// The three, in the order the chip renders them and the CLI lists them.
    pub(crate) const ALL: [Self; 3] = [Self::Equal, Self::Golden, Self::Split];

    /// The token the CLI accepts and the button id embeds.
    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::Equal => "equal",
            Self::Golden => "golden",
            Self::Split => "split",
        }
    }

    /// Parse a CLI token / button-id suffix back into a layout.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.id() == id)
    }

    /// The Adwaita symbolic icon the chip's button wears. All three exist in
    /// adwaita-icon-theme 50 (the one the devShell puts on `XDG_DATA_DIRS`);
    /// an unresolvable name would render as `image-missing`, silently.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            // Four equal cells — "everything the same size".
            Self::Equal => "view-grid-symbolic",
            // A wide main area with one narrow panel on the right: the exact
            // silhouette of the golden reading this ships (see `proportions`).
            Self::Golden => "sidebar-show-right-symbolic",
            // Two equal halves.
            Self::Split => "view-dual-symbolic",
        }
    }

    /// The button's hover text. An icon carries no words at all, so this is the
    /// only thing that says what the glyph does — the load-bearing tooltip case
    /// the proto's `Node::Icon` documents.
    pub(crate) fn tooltip(self) -> &'static str {
        match self {
            Self::Equal => "Equal columns — every column the same width",
            Self::Golden => "Golden — first column 61.8 %, the rest 38.2 %",
            Self::Split => "Split — every column 50 %",
        }
    }

    /// The proportion for each of `columns` columns, left to right.
    ///
    /// **This is the one function to edit** if issue #1019's question 1 is
    /// answered "B" (the narrow columns *share* the remaining 38.2 % so
    /// everything stays on screen) rather than the "A" reading built here: swap
    /// the `Golden` arm's `GOLDEN_MINOR` for
    /// `GOLDEN_MINOR / (columns - 1) as f64` and nothing else moves — not the
    /// planner, not the chip, not the CLI.
    ///
    /// `columns == 0` yields no proportions (and [`plan`] therefore sends
    /// nothing); `columns == 1` yields one, which for `Equal` is a full-width
    /// `1.0`.
    // A column count past f64's exact-integer range would need 2^53 windows
    // open; the cast is exact for every n a compositor can produce.
    #[allow(clippy::cast_precision_loss)]
    pub(crate) fn proportions(self, columns: usize) -> Vec<f64> {
        if columns == 0 {
            return Vec::new();
        }
        match self {
            Self::Equal => vec![1.0 / columns as f64; columns],
            Self::Split => vec![SPLIT_SHARE; columns],
            Self::Golden => std::iter::once(GOLDEN_MAJOR)
                .chain(std::iter::repeat_n(GOLDEN_MINOR, columns - 1))
                .collect(),
        }
    }
}

/// The id of the workspace `layout` should act on: the **active** workspace of
/// the **focused output**.
///
/// With no focused output (niri reports `FocusedOutput: None` when nothing is
/// connected) this falls back to the globally focused workspace, which is the
/// only other defensible reading of "the current desktop"; with neither, there
/// is nothing to lay out.
fn target_workspace(workspaces: &[Workspace], focused_output: Option<&str>) -> Option<u64> {
    let found = match focused_output {
        Some(output) => workspaces
            .iter()
            .find(|w| w.is_active && w.output.as_deref() == Some(output)),
        None => workspaces.iter().find(|w| w.is_focused),
    };
    found.map(|w| w.id)
}

/// The `SetWindowWidth` requests `layout` implies, as `(window id, proportion)`
/// pairs in **left-to-right column order**.
///
/// The rules, all of which the tests pin:
///
/// - Only windows on the target workspace count ([`target_workspace`]).
/// - Floating windows are skipped (`is_floating`) — they are not in the
///   scrolling layout and have no column.
/// - So is any window with no `pos_in_scrolling_layout`, which is how a
///   fullscreen window reports itself.
/// - Windows are grouped by **column** (`pos_in_scrolling_layout.0`, 1-based).
///   niri widths are per *column*, so a stacked column of three windows is one
///   entry, addressed by its **first tile** (lowest tile index; ties broken by
///   the lower window id, so the plan never depends on niri's list order).
/// - `n` is therefore the number of columns, not the number of windows — issue
///   #1019's question 3, answered "columns".
/// - No columns → no requests at all.
pub(crate) fn plan(
    windows: &[Window],
    workspaces: &[Workspace],
    focused_output: Option<&str>,
    layout: Layout,
) -> Vec<(u64, f64)> {
    let Some(workspace) = target_workspace(workspaces, focused_output) else {
        return Vec::new();
    };

    // column index → (tile index, window id) of that column's first tile.
    let mut columns: BTreeMap<usize, (usize, u64)> = BTreeMap::new();
    for window in windows {
        if window.workspace_id != Some(workspace) || window.is_floating {
            continue;
        }
        let Some((column, tile)) = window.layout.pos_in_scrolling_layout else {
            continue;
        };
        let candidate = (tile, window.id);
        columns
            .entry(column)
            .and_modify(|first| {
                if candidate < *first {
                    *first = candidate;
                }
            })
            .or_insert(candidate);
    }

    let proportions = layout.proportions(columns.len());
    columns
        .into_values()
        .zip(proportions)
        .map(|((_, id), proportion)| (id, proportion))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{GOLDEN_MAJOR, GOLDEN_MINOR, Layout, plan};
    use niri_ipc::{Window, WindowLayout, Workspace};

    const OUTPUT: &str = "DP-1";
    const OTHER_OUTPUT: &str = "HDMI-A-1";

    fn workspace(id: u64, output: &str, is_active: bool) -> Workspace {
        Workspace {
            id,
            idx: 1,
            name: None,
            output: Some(output.to_owned()),
            is_urgent: false,
            is_active,
            is_focused: is_active && output == OUTPUT,
            active_window_id: None,
        }
    }

    /// A tiled window in column `column`, tile `tile` (both 1-based).
    fn tile(id: u64, workspace_id: u64, column: usize, tile: usize) -> Window {
        Window {
            id,
            title: None,
            app_id: None,
            pid: None,
            workspace_id: Some(workspace_id),
            is_focused: false,
            is_floating: false,
            is_urgent: false,
            layout: WindowLayout {
                pos_in_scrolling_layout: Some((column, tile)),
                tile_size: (100.0, 100.0),
                window_size: (100, 100),
                tile_pos_in_workspace_view: Some((0.0, 0.0)),
                window_offset_in_tile: (0.0, 0.0),
            },
            focus_timestamp: None,
        }
    }

    fn floating(id: u64, workspace_id: u64) -> Window {
        let mut w = tile(id, workspace_id, 1, 1);
        w.is_floating = true;
        w
    }

    /// A fullscreen window: niri leaves `pos_in_scrolling_layout` unset.
    fn fullscreen(id: u64, workspace_id: u64) -> Window {
        let mut w = tile(id, workspace_id, 1, 1);
        w.layout.pos_in_scrolling_layout = None;
        w
    }

    fn ids(plan: &[(u64, f64)]) -> Vec<u64> {
        plan.iter().map(|(id, _)| *id).collect()
    }

    fn shares(plan: &[(u64, f64)]) -> Vec<f64> {
        plan.iter().map(|(_, p)| *p).collect()
    }

    #[test]
    fn groups_windows_by_column_and_walks_left_to_right() {
        let ws = vec![workspace(1, OUTPUT, true)];
        // Deliberately out of order: the plan must follow the column index, not
        // niri's list order.
        let windows = vec![tile(30, 1, 3, 1), tile(10, 1, 1, 1), tile(20, 1, 2, 1)];

        let got = plan(&windows, &ws, Some(OUTPUT), Layout::Equal);

        assert_eq!(ids(&got), vec![10, 20, 30], "left-to-right by column index");
    }

    #[test]
    fn a_stacked_column_counts_once_and_is_addressed_by_its_first_tile() {
        let ws = vec![workspace(1, OUTPUT, true)];
        // Column 1 holds three stacked windows; column 2 holds one. n must be 2.
        let windows = vec![
            tile(12, 1, 1, 3),
            tile(11, 1, 1, 2),
            tile(10, 1, 1, 1),
            tile(20, 1, 2, 1),
        ];

        let got = plan(&windows, &ws, Some(OUTPUT), Layout::Equal);

        assert_eq!(
            ids(&got),
            vec![10, 20],
            "one request per column, addressed by the topmost tile"
        );
        assert_eq!(
            shares(&got),
            vec![0.5, 0.5],
            "n is the column count (2), not the window count (4)"
        );
    }

    #[test]
    fn ties_on_tile_index_break_on_the_lower_window_id() {
        let ws = vec![workspace(1, OUTPUT, true)];
        // Two windows claiming the same (column, tile) shouldn't happen, but the
        // plan must not depend on which one niri listed first.
        let forwards = vec![tile(77, 1, 1, 1), tile(11, 1, 1, 1)];
        let backwards = vec![tile(11, 1, 1, 1), tile(77, 1, 1, 1)];

        assert_eq!(
            ids(&plan(&forwards, &ws, Some(OUTPUT), Layout::Equal)),
            vec![11]
        );
        assert_eq!(
            ids(&plan(&backwards, &ws, Some(OUTPUT), Layout::Equal)),
            vec![11]
        );
    }

    #[test]
    fn floating_and_fullscreen_windows_are_skipped() {
        let ws = vec![workspace(1, OUTPUT, true)];
        let windows = vec![
            floating(90, 1),
            fullscreen(91, 1),
            tile(10, 1, 1, 1),
            tile(20, 1, 2, 1),
        ];

        let got = plan(&windows, &ws, Some(OUTPUT), Layout::Equal);

        assert_eq!(ids(&got), vec![10, 20], "only tiled, positioned windows");
        assert_eq!(
            shares(&got),
            vec![0.5, 0.5],
            "the skipped windows don't inflate n"
        );
    }

    #[test]
    fn windows_on_another_workspace_or_output_are_skipped() {
        // Workspace 1 is active on the focused output; 2 is the inactive one
        // beside it; 3 is active but on a different output.
        let ws = vec![
            workspace(1, OUTPUT, true),
            workspace(2, OUTPUT, false),
            workspace(3, OTHER_OUTPUT, true),
        ];
        let windows = vec![tile(10, 1, 1, 1), tile(20, 2, 1, 1), tile(30, 3, 1, 1)];

        let got = plan(&windows, &ws, Some(OUTPUT), Layout::Equal);

        assert_eq!(
            ids(&got),
            vec![10],
            "only the focused output's active workspace"
        );
        assert_eq!(shares(&got), vec![1.0], "n = 1 → full width");
    }

    #[test]
    fn no_focused_output_falls_back_to_the_focused_workspace() {
        let mut ws = vec![workspace(1, OUTPUT, true), workspace(3, OTHER_OUTPUT, true)];
        ws[1].is_focused = false;
        let windows = vec![tile(10, 1, 1, 1), tile(30, 3, 1, 1)];

        let got = plan(&windows, &ws, None, Layout::Equal);

        assert_eq!(ids(&got), vec![10]);
    }

    #[test]
    fn an_unknown_output_matches_no_workspace_and_plans_nothing() {
        let ws = vec![workspace(1, OUTPUT, true)];
        let windows = vec![tile(10, 1, 1, 1)];

        assert!(plan(&windows, &ws, Some("eDP-1"), Layout::Equal).is_empty());
    }

    #[test]
    fn no_tiled_columns_plans_nothing() {
        let ws = vec![workspace(1, OUTPUT, true)];

        assert!(
            plan(&[], &ws, Some(OUTPUT), Layout::Golden).is_empty(),
            "an empty workspace"
        );
        assert!(
            plan(&[floating(90, 1)], &ws, Some(OUTPUT), Layout::Golden).is_empty(),
            "a workspace holding only floaters"
        );
    }

    #[test]
    fn equal_splits_the_working_area_evenly() {
        assert_eq!(Layout::Equal.proportions(1), vec![1.0]);
        assert_eq!(Layout::Equal.proportions(2), vec![0.5, 0.5]);
        assert_eq!(Layout::Equal.proportions(4), vec![0.25, 0.25, 0.25, 0.25]);
    }

    #[test]
    fn split_is_half_for_every_column() {
        assert_eq!(Layout::Split.proportions(1), vec![0.5]);
        assert_eq!(Layout::Split.proportions(3), vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn golden_is_wide_first_then_narrow_rest() {
        // Reading A of #1019 question 1: the first column takes 61.8 % and every
        // other column takes 38.2 %, so the tail scrolls off to the right.
        assert_eq!(Layout::Golden.proportions(1), vec![GOLDEN_MAJOR]);
        assert_eq!(
            Layout::Golden.proportions(2),
            vec![GOLDEN_MAJOR, GOLDEN_MINOR]
        );
        assert_eq!(
            Layout::Golden.proportions(4),
            vec![GOLDEN_MAJOR, GOLDEN_MINOR, GOLDEN_MINOR, GOLDEN_MINOR],
            "every column after the first is the same narrow share (reading A)"
        );
    }

    #[test]
    fn zero_columns_yields_no_proportions_for_any_layout() {
        for layout in Layout::ALL {
            assert!(
                layout.proportions(0).is_empty(),
                "{} must plan nothing for an empty workspace",
                layout.id()
            );
        }
    }

    #[test]
    fn every_layout_yields_exactly_one_proportion_per_column() {
        for layout in Layout::ALL {
            for n in 1_usize..=6 {
                assert_eq!(layout.proportions(n).len(), n, "{} at n = {n}", layout.id());
            }
        }
    }

    #[test]
    fn ids_round_trip_through_from_id_and_nothing_else_parses() {
        for layout in Layout::ALL {
            assert_eq!(Layout::from_id(layout.id()), Some(layout));
        }
        assert_eq!(Layout::from_id("Equal"), None, "matching is case-sensitive");
        assert_eq!(Layout::from_id("golden-ratio"), None);
        assert_eq!(Layout::from_id(""), None);
    }

    #[test]
    fn every_layout_has_a_distinct_icon_and_a_non_empty_tooltip() {
        let mut icons: Vec<&str> = Layout::ALL.iter().map(|l| l.icon()).collect();
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), 3, "three glyphs, not one repeated");
        for layout in Layout::ALL {
            assert!(
                layout.icon().ends_with("-symbolic"),
                "{} wants a symbolic icon",
                layout.id()
            );
            // A blank or whitespace-only tooltip arms nothing at all host-side,
            // which would leave the glyph unexplained.
            assert!(
                !layout.tooltip().trim().is_empty(),
                "{} needs a legend",
                layout.id()
            );
        }
    }
}
