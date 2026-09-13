//! Layout primitives every drawer panel uses.
//!
//! `page_box` and `finish_page` wrap the outer column with the standard
//! drawer styling and an `AdwClamp` width cap. `page_grid` is the
//! two-column grid container. `section` (renamed from `panel` to avoid
//! clashing with the `panels/` module name) is the titled card you
//! attach into a grid cell. `boxed_list` builds a selection-free
//! `gtk::ListBox` with the Adwaita `boxed-list` style. `toggle_class`
//! adds or removes a single CSS class based on a boolean.

use hytte::adw;
use hytte::gtk::{self, prelude::*};

use crate::scale::scale;

/// Drawer's max content width, in CSS px (`AdwClamp.maximum_size` below, and
/// the per-trigger margin clamp in `modal.rs` that keeps the card from
/// falling off-screen left) — single source so the two never drift apart.
pub(crate) const DRAWER_MAX_WIDTH: i32 = 680;

/// Wider drawer max content width for the side-by-side-columns pages. Two
/// columns inside the global `DRAWER_MAX_WIDTH` (680) would each squeeze to
/// ~330px — the opposite of #508's "the panel got smaller" complaint — so a page
/// that lays out in columns opts into this via [`finish_page_clamped`].
/// `modal.rs`'s centering clamp (`main_margin_for_center`) uses this as its
/// upper bound too, so a card up to this wide still centers correctly under its
/// trigger chip.
///
/// Two pages use it, for the same measurement: the multicolumn Stats page
/// (#508, two history graphs) and the Workspaces page (#1071, one column per
/// monitor). Every other page stays on `finish_page`/`DRAWER_MAX_WIDTH`.
pub(crate) const DRAWER_MAX_WIDTH_WIDE: i32 = 1080;

/// Horizontal spacing, in CSS px before [`crate::scale::scale`], between the
/// Workspaces page's per-monitor columns. `panels::workspaces`' columns box is
/// built with it and [`workspaces_page_width`] budgets one gutter for it, so
/// the two cannot drift apart.
pub(crate) const WORKSPACES_COLUMN_SPACING: i32 = 12;

/// Design width, in CSS px before [`crate::scale::scale`], of **one**
/// Workspaces column.
///
/// Why 360: it is what a column got on a three-monitor box before #1219 —
/// `DRAWER_MAX_WIDTH_WIDE` (1080) over three columns — so a one- or two-column
/// page sized from it renders columns of exactly the width the wide page has
/// always rendered, rather than a new guess. The floor under it is #508's
/// measurement: two columns inside `DRAWER_MAX_WIDTH` (680) squeeze to ~330 px
/// each, which is the squeeze that put the multicolumn pages on the wide clamp
/// in the first place, so 360 sits just above the line rather than at it.
///
/// It governs the **two**-column width only. Three columns take
/// [`WORKSPACES_THREE_COLUMN_WIDTH`] on Annika's word, which is narrower than
/// three of these — see that constant.
pub(crate) const WORKSPACES_COLUMN_WIDTH: i32 = 360;

/// The three-column Workspaces page width, in CSS px before
/// [`crate::scale::scale`] — **Annika's number**, #1219, 2026-09-13: *"IDK 960
/// also sufficient for 3."*
///
/// It is deliberately **not** `3 × WORKSPACES_COLUMN_WIDTH + 2 × gutter` (1092,
/// over the drawer's centering bound anyway) nor `DRAWER_MAX_WIDTH_WIDE` (1080):
/// 960 less the two 12-px gutters leaves **312 px** a column, under the 360 the
/// two-column arm is built from and a little under the ~330 px #508 measured as
/// the squeeze line. She looked at it and judged it sufficient, so this is a
/// decision recorded rather than a measurement — the reason the constant exists
/// at all instead of the arm being spelled inline is so the deviation has one
/// place to be read and one place to be changed.
///
/// Four or more columns keep [`DRAWER_MAX_WIDTH_WIDE`]: past three, the page is
/// already dividing the widest drawer the centering clamp supports, and taking
/// width *away* there would only make the squeeze worse.
pub(crate) const WORKSPACES_THREE_COLUMN_WIDTH: i32 = 960;

/// The width the Workspaces **Edit** sub-page measures, in CSS px before
/// [`crate::scale::scale`] — always, whatever the page behind it is showing
/// (#1220).
///
/// #1108 made the Edit form measure exactly what the page it replaces measures,
/// so the drawer would not jump on ✎. #1219 then made that page's width follow
/// its column count, which on a one-screen box is 680 — and a 680-px form is
/// what Annika filed #1220 about: her screenshot is **418 px** of form with an
/// apps column around 150 px, *"Apps too narrow to edit comfortable … how tf is
/// one supposed to use this"*.
///
/// So the form no longer follows the page. It takes this width on every route,
/// which leaves its app list ~700 px next to the 240-px fields column at every
/// screen count. The cost is that ✎ from a 680- or 732-px page widens the
/// drawer, i.e. #1108's no-jump contract is relaxed on purpose — Annika, #1219,
/// 2026-09-13: *"Slight jump in edit form is ok."*
///
/// Same number as [`WORKSPACES_THREE_COLUMN_WIDTH`] and not derived from it: the
/// two are independent decisions that happen to have landed on the same figure
/// (hers for three columns; ours for "the apps column needs ~700 px"), and tying
/// them together would make a future change to either silently move the other.
pub(crate) const EDIT_FORM_WIDTH: i32 = 960;

/// The width the Workspaces page (and its Edit sub-page) should measure, in CSS
/// px before [`crate::scale::scale`], for a page rendering `columns` columns
/// (#1219).
///
/// The Workspaces columns box is **homogeneous**, so the page width is divided
/// equally however many columns there are: before #1219 the page was pinned to
/// `DRAWER_MAX_WIDTH_WIDE` unconditionally (#1108's minimum size request), which
/// on a single-monitor box drew one 1080-px column around card content that only
/// needs ~340 px. This is that floor made per-column:
///
/// * **0 or 1** columns → [`DRAWER_MAX_WIDTH`] (680), the ordinary drawer width
///   every other page uses. 0 is the no-outputs hint, which wants no more.
/// * **2** columns → two [`WORKSPACES_COLUMN_WIDTH`] columns plus the one
///   [`WORKSPACES_COLUMN_SPACING`] gutter between them (732).
/// * **3** columns → [`WORKSPACES_THREE_COLUMN_WIDTH`] (960) — Annika's number
///   on #1219, and 312 px a column rather than the 360 the two-column arm
///   assumes. See that constant for why the deviation is deliberate.
/// * **4 or more** → [`DRAWER_MAX_WIDTH_WIDE`] (1080), the width the page had
///   at every count before #1219 and the widest the drawer's centering clamp in
///   `modal.rs` supports. Past three the columns share it and get narrower,
///   exactly as they did before.
///
/// The Edit sub-page does **not** go through this any more — it takes
/// [`EDIT_FORM_WIDTH`] whatever the page behind it measures (#1220).
///
/// `columns` is the count of columns the page actually **renders** — one per
/// connected output plus at most one trailing "not connected" column (#1071 §5)
/// — not the length of `hytte::services::displays::outputs()`. The column set is
/// derived from niri's workspace snapshot, not from that service (see
/// `panels::workspaces`' module doc), so the two counts can legitimately
/// disagree.
///
/// `const fn` so `panels::workspaces` can seed its published-width cell with
/// `workspaces_page_width(0)` in a `const` block rather than repeating 680.
pub(crate) const fn workspaces_page_width(columns: usize) -> i32 {
    match columns {
        0 | 1 => DRAWER_MAX_WIDTH,
        2 => 2 * WORKSPACES_COLUMN_WIDTH + WORKSPACES_COLUMN_SPACING,
        3 => WORKSPACES_THREE_COLUMN_WIDTH,
        _ => DRAWER_MAX_WIDTH_WIDE,
    }
}

/// Point an already-built [`adw::Clamp`] — one [`finish_page_clamped`] returned
/// — at `width` (CSS px at the design baseline), as both a ceiling **and** a
/// floor, so the page measures exactly that wide.
///
/// All three properties move together on purpose:
///
/// * `maximum_size`/`tightening_threshold` stay equal for the reason
///   [`finish_page`] documents (a threshold below the maximum makes the clamp
///   over-request, #134), and
/// * `size_request` is the **minimum** #1108 added so the page fills its cap
///   instead of shrinking to its content's natural width.
///
/// Setting the ceiling without the floor leaves the page narrow; setting the
/// floor without the ceiling lets a wide child push past it. A page whose width
/// changes at runtime (today: Workspaces, on output hot-plug — #1219) has to
/// move all three, which is why this exists rather than each call site doing two
/// of the three.
pub(crate) fn set_page_width(clamp: &adw::Clamp, width: i32) {
    let cap = scale(width);
    clamp.set_maximum_size(cap);
    clamp.set_tightening_threshold(cap);
    clamp.set_size_request(cap, -1);
}

/// Floor an already-built page clamp at **its own ceiling**, so the page fills
/// the cap it is already clamped to instead of shrinking to its content's
/// natural width (#1108).
///
/// The width is not a parameter on purpose. The clamp's `maximum_size` *is* the
/// width the page decided on — for the Workspaces page,
/// `panels::workspaces`' binding sets all three properties from
/// [`workspaces_page_width`] on every model revision — so reading it back here
/// means the number lives in exactly one place and the on-show re-push can never
/// be a revision behind the page it is flooring (#1225 review, MEDIUM 3: the
/// published-number cell this replaces was still at its seed on the very first
/// show, because `bind` applies a turn later than the synchronous
/// build → show → present call stack).
///
/// `tightening_threshold` is re-pushed alongside it for the reason
/// [`finish_page`] documents — a threshold below the maximum makes the clamp
/// over-request (#134) — so a clamp that reaches this function with the two out
/// of step leaves it consistent.
pub(crate) fn fill_page_to_its_cap(clamp: &adw::Clamp) {
    let cap = clamp.maximum_size();
    clamp.set_tightening_threshold(cap);
    clamp.set_size_request(cap, -1);
}

pub(crate) fn page_box() -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
    b.add_css_class("ts-modal-page");
    b
}

/// Wrap a finished page widget (Box or Grid) in an `AdwClamp` so a child
/// reporting a pathological natural width (e.g. an `AdwActionRow` subtitle
/// that ends up holding a long single-line list) can't push the
/// layer-shell modal surface to full-screen width. Belt-and-suspenders
/// against the same class of bug — individual rows should still constrain
/// themselves (multi-line subtitles, `subtitle_lines(0)`, etc.) but this
/// catches the ones that don't.
pub(crate) fn finish_page(content: &impl IsA<gtk::Widget>) -> gtk::Widget {
    // `maximum_size` caps the child allocation; `tightening_threshold` is
    // intentionally set equal to it so `AdwClamp` never over-requests beyond
    // the cap. When threshold < maximum_size, the clamp's natural-width
    // request is `threshold + 3×(maximum_size − threshold)`, which balloons
    // the surface past the card and creates wide lilac side-margins on panels
    // whose content natural width crosses the threshold (network, vpn — #134).
    // With threshold == maximum_size that formula collapses to exactly
    // `maximum_size` and the overshoot disappears. Both values are scaled with
    // the font so the cap grows consistently with the rest of the shell (#114).
    let cap = scale(DRAWER_MAX_WIDTH);
    let clamp = adw::Clamp::builder()
        .maximum_size(cap)
        .tightening_threshold(cap)
        .child(content)
        .build();
    clamp.upcast()
}

/// [`finish_page`] with a caller-chosen max width instead of the global
/// `DRAWER_MAX_WIDTH`. Added for the Stats multicolumn layout (#508), which
/// wants a wider clamp ([`DRAWER_MAX_WIDTH_WIDE`]) than the other pages so its
/// two columns each get a usable width. Same `threshold == maximum_size`
/// no-overshoot trick and font-scaling as [`finish_page`] — see that function's
/// comment for why the two clamp values are equal.
pub(crate) fn finish_page_clamped(content: &impl IsA<gtk::Widget>, max_width: i32) -> gtk::Widget {
    let cap = scale(max_width);
    let clamp = adw::Clamp::builder()
        .maximum_size(cap)
        .tightening_threshold(cap)
        .child(content)
        .build();
    clamp.upcast()
}

/// Two-column (or more) grid for rich modal pages. Sections attach via
/// `grid.attach(&section, col, row, 1, 1)`. Columns are homogeneous (equal
/// width) — the right default for most pages, where sections are meant to
/// line up evenly. A page that wants asymmetric columns instead should use
/// [`page_grid_non_homogeneous`] rather than flipping the flag after
/// construction, which — pre-#708 — was how `panel_media` opted out: a
/// homogeneous grid propagates one child's minimum natural width to every
/// column (`#702`'s blown-out CPU card mirroring its width onto Memory was
/// exactly this), so a page that legitimately wants unequal columns needs
/// that stated at construction, not patched on after the fact.
pub(crate) fn page_grid() -> gtk::Grid {
    page_grid_with_homogeneous(true)
}

/// [`page_grid`] with column homogeneity turned off — for pages whose
/// columns are deliberately asymmetric (e.g. `panel_media`'s art column vs.
/// its info column). See [`page_grid`]'s doc comment for why this is a
/// separate constructor rather than a post-construction
/// `set_column_homogeneous(false)` call.
pub(crate) fn page_grid_non_homogeneous() -> gtk::Grid {
    page_grid_with_homogeneous(false)
}

fn page_grid_with_homogeneous(column_homogeneous: bool) -> gtk::Grid {
    let g = gtk::Grid::new();
    g.add_css_class("ts-modal-page");
    g.add_css_class("ts-page-grid");
    g.set_row_spacing(12);
    g.set_column_spacing(12);
    g.set_column_homogeneous(column_homogeneous);
    g
}

/// Selection-free `gtk::ListBox` styled with Adwaita's `boxed-list` look.
/// Use `list.append(&row)` to populate it.
pub(crate) fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

/// Add `class` to `widget` when `on` is `true`; remove it otherwise.
pub(crate) fn toggle_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// Card-style section with a title header. Caller appends content by
/// calling `outer.append(&child)` on the returned Box.
///
/// Renamed from `panel` to avoid the module-name clash with `panels/`.
pub(crate) fn section(title: &str) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 4);
    outer.add_css_class("ts-panel");
    outer.set_hexpand(true);
    outer.set_vexpand(true);
    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("ts-panel-title");
    title_label.set_xalign(0.0);
    outer.append(&title_label);
    outer
}

#[cfg(test)]
mod tests {
    use super::{
        DRAWER_MAX_WIDTH, DRAWER_MAX_WIDTH_WIDE, EDIT_FORM_WIDTH, WORKSPACES_COLUMN_SPACING,
        WORKSPACES_COLUMN_WIDTH, workspaces_page_width,
    };

    /// #1219: the Workspaces page's width follows how many columns it renders,
    /// instead of the fixed `DRAWER_MAX_WIDTH_WIDE` floor #1108 pushed on every
    /// show — which drew a single 1080-px column on a one-monitor box.
    ///
    /// The widths are pinned as **literals**, not re-derived from the constants
    /// above: derived expectations cannot see a changed constant (they change
    /// with it), and these four numbers are the design — 680 for one screen, 732
    /// for two, **960 for three** (Annika, #1219, 2026-09-13: *"IDK 960 also
    /// sufficient for 3."*) and 1080 from four.
    ///
    /// **The mutation**: any arm collapsed back to `DRAWER_MAX_WIDTH_WIDE` — the
    /// three-column one especially, which is where this PR's first round shipped
    /// 1080 against her 960 — or the 2-column arm rounded to `DRAWER_MAX_WIDTH`,
    /// reds this.
    #[test]
    fn the_workspaces_page_width_follows_the_column_count() {
        assert_eq!(
            workspaces_page_width(0),
            680,
            "no outputs: the hint needs no more than an ordinary drawer"
        );
        assert_eq!(
            workspaces_page_width(1),
            680,
            "one column: the ordinary drawer width"
        );
        assert_eq!(
            workspaces_page_width(2),
            732,
            "two columns of 360 plus the 12-px gutter between them"
        );
        assert_eq!(
            workspaces_page_width(3),
            960,
            "three columns: Annika's number on #1219, not the 1080 this page had before"
        );
        assert_eq!(
            workspaces_page_width(4),
            1080,
            "four or more share the wide page, as every count did before #1219"
        );
    }

    /// The three-column width is a **judgement call, not a measurement**, so the
    /// consequence it accepts is pinned rather than left implicit: 960 less the
    /// two gutters is 312 px a column — under `WORKSPACES_COLUMN_WIDTH` (360),
    /// which is what the two-column arm is built from, and a little under the
    /// ~330 px #508 measured as the squeeze line.
    ///
    /// Annika looked at the alternative (1080, what the page had before #1219)
    /// and answered *"IDK 960 also sufficient for 3."* This test exists so that
    /// judgement is re-read the day someone changes the constant: a future width
    /// that happens to clear 360 is fine, but silently sliding *further* below it
    /// is not.
    ///
    /// **The mutation**: `WORKSPACES_THREE_COLUMN_WIDTH` dropped toward 732 reds
    /// this on the lower bound, while the literal pin above catches the 1080
    /// direction.
    #[test]
    fn three_columns_take_annikas_960_and_the_narrower_column_it_implies() {
        let gutters = 2 * WORKSPACES_COLUMN_SPACING;
        let share = (workspaces_page_width(3) - gutters) / 3;
        assert_eq!(
            share, 312,
            "three columns inside Annika's 960 come out at 312 px each"
        );
        assert!(
            share < WORKSPACES_COLUMN_WIDTH,
            "the whole point of pinning this: 312 is *below* the {WORKSPACES_COLUMN_WIDTH} px \
             design column, accepted on #1219 rather than measured"
        );
        assert!(
            share >= 300,
            "…but not arbitrarily below it — {share} px a column is past what she looked at"
        );
    }

    /// #1220: the Edit form is **not** sized from the page behind it any more.
    ///
    /// #1108 tied the two together so the drawer would not jump on ✎; #1219 then
    /// made the page as narrow as 680 on a one-screen box, and 680 minus the
    /// 240-px fields column is the form Annika filed #1220 about. So the form
    /// takes its own width, wide enough that the app list gets ~700 px at every
    /// screen count, and the jump is accepted (*"Slight jump in edit form is
    /// ok."*, #1219).
    ///
    /// **The mutation**: `EDIT_FORM_WIDTH` set back to `DRAWER_MAX_WIDTH` — or to
    /// anything that leaves the list under 600 px — reds this.
    #[test]
    fn the_edit_form_is_wide_enough_for_its_app_list_at_every_page_width() {
        assert_eq!(EDIT_FORM_WIDTH, 960, "the Edit form's own width (#1220)");
        // 240 for the fields column, 12 for the grid's gutter — what is left is
        // the app list, and "too narrow to edit comfortable" is the bug.
        let apps = EDIT_FORM_WIDTH - 240 - WORKSPACES_COLUMN_SPACING;
        assert!(
            apps >= 700,
            "the app list gets {apps} px beside the fields column; #1220 was filed at ~150"
        );
        for columns in 0..=3usize {
            assert!(
                EDIT_FORM_WIDTH >= workspaces_page_width(columns),
                "up to three columns ✎ only ever *widens* the drawer — {columns} columns"
            );
        }
        // Four screens or more is the one case where ✎ narrows it (1080 → 960).
        // That is still inside "slight jump in edit form is ok" and is the price
        // of the form having one width; pinned so the direction is a decision
        // rather than a surprise. `workspaces_page_width(4)` is
        // `DRAWER_MAX_WIDTH_WIDE`, so this doubles as the form still fitting the
        // drawer's centering bound.
        assert!(
            EDIT_FORM_WIDTH < workspaces_page_width(4),
            "at four columns the page ({}) is wider than the form, so ✎ narrows the \
             drawer there — and the form stays inside the centering bound \
             ({DRAWER_MAX_WIDTH_WIDE})",
            workspaces_page_width(4)
        );
    }

    /// The page can never ask for less than an ordinary drawer nor more than the
    /// widest the drawer supports, and it never *shrinks* as columns are added.
    ///
    /// `DRAWER_MAX_WIDTH_WIDE` is the upper bound `modal::main_margin_for_center`
    /// clamps a card's extent to, so a width above it would stop centering under
    /// its trigger; `DRAWER_MAX_WIDTH` is what every other page gets, and a
    /// Workspaces page narrower than that would be a new, smaller drawer nobody
    /// asked for (#508's original complaint, in reverse).
    ///
    /// **The mutation**: an arm returning e.g. `2 * WORKSPACES_COLUMN_WIDTH` for
    /// three columns (1104, over the centering bound) reds this while leaving the
    /// literal pin above green for 0, 1 and 2.
    #[test]
    fn every_page_width_stays_inside_the_drawers_own_bounds() {
        for columns in 0..12usize {
            let width = workspaces_page_width(columns);
            assert!(
                width >= DRAWER_MAX_WIDTH,
                "{columns} columns asked for {width}, narrower than an ordinary drawer ({DRAWER_MAX_WIDTH})"
            );
            assert!(
                width <= DRAWER_MAX_WIDTH_WIDE,
                "{columns} columns asked for {width}, wider than the drawer's centering bound ({DRAWER_MAX_WIDTH_WIDE})"
            );
            assert!(
                width <= workspaces_page_width(columns + 1),
                "adding a column made the page narrower: {columns} -> {width}"
            );
        }
    }

    /// At one and two columns, a column still clears #508's squeeze line.
    ///
    /// That measurement is the reason the multicolumn pages opted into
    /// `DRAWER_MAX_WIDTH_WIDE` at all: two columns inside `DRAWER_MAX_WIDTH` come
    /// out at ~330 px each, which read as "the panel got smaller". So the one- and
    /// two-column widths have to leave each column at least
    /// `WORKSPACES_COLUMN_WIDTH` (360) after the gutters are taken out — which is
    /// what makes 732 rather than 680 the two-column answer.
    ///
    /// **Three columns are deliberately excluded**: Annika's 960 leaves 312 px
    /// there, under this line on purpose, and
    /// [`three_columns_take_annikas_960_and_the_narrower_column_it_implies`] is
    /// where that trade is written down. Widening this loop to cover it would be
    /// asserting the opposite of the decision on #1219.
    ///
    /// **The mutation**: the 2-column arm returning `DRAWER_MAX_WIDTH` (680, i.e.
    /// 334 px a column) reds this.
    #[test]
    fn a_column_below_the_wide_width_clears_the_508_squeeze_line() {
        for columns in 1..=2usize {
            let count = i32::try_from(columns).expect("a handful of columns fits an i32");
            let gutters = (count - 1) * WORKSPACES_COLUMN_SPACING;
            let share = (workspaces_page_width(columns) - gutters) / count;
            assert!(
                share >= WORKSPACES_COLUMN_WIDTH,
                "{columns} columns leave {share} px each, under the {WORKSPACES_COLUMN_WIDTH} px \
                 a column is designed for (#508 measured ~330 px as the squeeze)"
            );
        }
    }
}
