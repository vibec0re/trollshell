//! The **Plugins** tab (#348, live runtime overlay #423, adaptive drill-down
//! #887) — one entry per `trollshell-plugin-<id>` systemd **user** unit,
//! round-tripped over the shell's `Control` endpoint (`ListPlugins` /
//! `ListPluginStates` / `StartPlugin` / `StopPlugin` / `SetPluginEnabled`).
//!
//! Lives in its own module for the reason [`crate::places_tab`] does: a tab
//! with its own state struct, its own poll timer and its own D-Bus surface is a
//! module, not four hundred lines in the middle of the app shell.
//!
//! # The shape (#887)
//!
//! ```text
//! AdwBreakpointBin                      ← owns the one breakpoint
//!   AdwNavigationSplitView              ← `collapsed` is what the breakpoint sets
//!     sidebar: AdwNavigationPage "Plugins"
//!       AdwToolbarView[AdwHeaderBar, GtkScrolledWindow[GtkListBox]]
//!     content: AdwNavigationPage "<plugin id>"
//!       AdwToolbarView[AdwHeaderBar, GtkStack["empty" | "plugin"]]
//! ```
//!
//! Wide, the split view shows both panes and selecting a row retargets the
//! detail pane. Narrow, the breakpoint collapses it: the list is the only page,
//! **activating** a row pushes the detail page, and the content header's back
//! button (which `AdwHeaderBar` grows by itself inside a navigation stack) pops
//! it. One widget tree serves both — there is no per-mode rebuild, which is
//! also why a resize across the threshold cannot lose state.
//!
//! # There is exactly one detail pane
//!
//! The content page's widgets are built once and *retargeted* at whichever
//! plugin is selected ([`refresh_detail`]), rather than a fresh page being
//! built per selection. That is what makes the 2 s poll ([`PLUGIN_POLL_INTERVAL`])
//! invisible: a refresh whose plugin set is unchanged updates row subtitles,
//! badges and the detail's own rows **in place**, so neither the list selection
//! nor a pushed navigation page is disturbed. Only a genuine membership change
//! ([`same_plugin_set`]) tears the rows down, and even then the selection is
//! restored by id if the plugin survived. If it did not, any pushed page pops
//! back to the list and the sidebar settles on the first remaining plugin —
//! never on a page for a unit that no longer exists. With no plugins at all,
//! [`clear_selection`] drops to the detail pane's empty state.
//!
//! A **failed** poll is not a membership change and must not be read as one: it
//! parks the selection ([`ParkedSelection`]) while the "unavailable"
//! placeholder is up, and the next good poll puts both the selection and a
//! pushed page back. Without that, one 3 s timeout on a 2 s cadence would
//! quietly move the user to whichever plugin happens to be first.
//!
//! # The switch holds its own truth for a while (#944)
//!
//! [`refresh_detail`] drives the detail switch from the last poll's
//! `ActiveState` on every tick — right up until the user has just asked for
//! the opposite. `connect_switch` records that ask as a [`PendingToggle`]
//! the instant the switch flips, and [`resolve_pending`] keeps
//! `refresh_detail` from driving the switch off a stale snapshot while it's
//! live: showing the wanted state instead until a poll agrees or
//! [`PENDING_TOGGLE_TIMEOUT`] admits the transition never happened. The
//! `syncing` guard (already needed so a fetched state doesn't loop back into
//! a Start/Stop call) is what stops `refresh_detail`'s own `set_active` from
//! recording a bogus intent of its own.
//!
//! # Polls are ordered, not serialised (#983)
//!
//! The 2 s tick and [`refresh_plugins_soon`]'s two extra polls overlap freely,
//! and each is two sequential `Control` calls with a 3 s timeout apiece — so
//! completions can and do arrive out of order. Every poll therefore carries a
//! [`PollGenerations`] stamp and [`on_poll_result_with_declared`] drops any
//! result older than the newest already applied. That is a *different* door
//! from the
//! [`PendingToggle`] latch above: the latch protects a toggle the poll hasn't
//! caught up with, whereas this protects the view from a poll the poll itself
//! has already superseded — the latch is legitimately spent by then, so it is
//! not there to help.
//!
//! # Transitions-only logging (#1017)
//!
//! [`on_poll_result_with_declared`]'s `Err` arm used to log `"ListPlugins failed"`
//! unconditionally — one line every [`PLUGIN_POLL_INTERVAL`] for the whole
//! time the shell is down, the one poller `#1002`/`#1015` (the connection
//! banner, the revision footer, and the AI Keys tab) left unconverted. It now
//! remembers the last *applied* poll's failure state
//! ([`PluginsState::last_failing`]) and logs only on a down→up or up→down
//! edge (`crate::log_transition`, `crate::LogTransition` — lifted out of
//! `ai_keys_tab`'s original private copy so this reuses the actual helper
//! rather than a third hand-copy), matching `main.rs`'s `ShellProbeUi` and
//! `ai_keys_tab`'s own guard. A stale, out-of-order completion (#983) is
//! still dropped **before** this runs — [`PollGenerations::accept`] gates the
//! whole of [`on_poll_result_with_declared`], transition logging included, so
//! a superseded result cannot flip `last_failing` on its way out.
//!
//! # `AdwBreakpointBin`, on contract (#856)
//!
//! #856 recorded what using that widget *off* contract costs: it warns once per
//! allocation, forever, whenever a child's minimum width exceeds the bin's
//! width (`adw-breakpoint-bin.c`'s condition is exactly `min_width > width`,
//! and nothing else — alignment cannot influence it). So nothing here pins a
//! child's minimum. The sidebar is sized with the split view's own
//! `min-sidebar-width` / `max-sidebar-width`, the list rides a
//! `GtkScrolledWindow` with an automatic horizontal policy so it can shrink
//! below its natural width, and the bin's own floor
//! ([`BIN_MIN_WIDTH_PX`]) is asserted in `gtk_tests` to be at or above the split
//! view's minimum in **both** configurations. libadwaita also documents that
//! adding a breakpoint strips the bin's minimum size in both directions, so the
//! floor is set on both axes.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use adw::prelude::*;
use gtk::glib;
use hytte_bus::RetryPolicy;

use crate::{
    CONTROL_IFACE, CONTROL_NAME, CONTROL_PATH, LogTransition, log_transition, spawn_on_runtime,
};

// ── Plugins tab (#348) · runtime overlay (#423) · drill-down (#887) ──────────

/// The poll cadence for the Plugins tab's live runtime overlay (#423). The tab
/// re-reads `ListPlugins` + `ListPluginStates` on this interval and refreshes
/// each row's connected/rendering badge **in place** (a changed plugin set
/// triggers a rebuild instead), so the badges track the host without the user
/// reopening the tab.
///
/// `pub(crate)`: since #989 the window's connection banner and revision footer
/// re-probe on **this** cadence (`crate::SHELL_PROBE_INTERVAL`) rather than a
/// number of their own, because they answer the same question about the same
/// endpoint and them disagreeing was the defect.
pub(crate) const PLUGIN_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long a user-initiated toggle holds the switch against a poll that
/// hasn't caught up yet (#944), before "truth wins" regardless.
///
/// `systemd-run` starts a transient unit in well under this — the 1.2 s
/// settle re-poll in [`refresh_plugins_soon`] already catches the common
/// case — so this is purely a backstop for a start/stop that hangs or
/// genuinely fails. At that point a switch frozen on what the user asked for
/// is a worse lie than showing the real (stuck) state, so 10 s is short
/// enough that a real failure surfaces quickly and long enough that no normal
/// transition ever hits it.
const PENDING_TOGGLE_TIMEOUT: Duration = Duration::from_secs(10);

/// The width at or below which the split view collapses to one pane at a time
/// (#887).
///
/// Derived from this app's own numbers rather than picked round. The window
/// opens at 760 × 560 (`crate::build_window`), and the sidebar is clamped to
/// [`SIDEBAR_MIN_PX`] … [`SIDEBAR_MAX_PX`]: at 760 the list gets ~240 px and
/// the detail ~520. Squeeze the window and the detail pane is what shrinks —
/// an `AdwPreferencesGroup` row with a title, a subtitle and a suffix stops
/// being readable somewhere around 300 px, and the sidebar cannot give up more
/// than its 220 px floor. 220 + 300 = 520, so **520 px is the last width at
/// which two panes are still worth having**; below it, one pane at a time is
/// strictly better. Well clear of the 760 px default, so the app never opens
/// collapsed.
const COLLAPSE_WIDTH_PX: f64 = 520.0;

/// The sidebar's floor. Plugin ids are short (`clock`, `departures`,
/// `claude-bridge`), but the row also carries a subtitle and the status cell,
/// so much under this and the status column starts ellipsizing.
const SIDEBAR_MIN_PX: f64 = 220.0;

/// The sidebar's ceiling — past this the list is just whitespace and the detail
/// pane is the one paying for it.
const SIDEBAR_MAX_PX: f64 = 300.0;

/// The share of a wide split the sidebar asks for, clamped by the two constants
/// above. 0.32 puts the list at ~243 px in the default 760 px window, which is
/// inside the clamp rather than pinned against it.
const SIDEBAR_FRACTION: f64 = 0.32;

/// The bin's own minimum, on both axes.
///
/// libadwaita documents that *"adding a breakpoint to `AdwBreakpointBin` will
/// result in it having no minimum size"* and that `width-request` /
/// `height-request` must therefore be set to the smallest size the bin is meant
/// to support — omitting the height half is not silent, it warns on every
/// allocation. This is the smallest width one pane is usable at, comfortably
/// below [`COLLAPSE_WIDTH_PX`] so the collapsed configuration is reachable, and
/// at or above the split view's own minimum in both configurations (asserted by
/// `the_bin_is_never_narrower_than_its_child_needs`, the #856 contract).
const BIN_MIN_WIDTH_PX: i32 = 360;

/// The bin's vertical floor — see [`BIN_MIN_WIDTH_PX`] for why both axes.
const BIN_MIN_HEIGHT_PX: i32 = 240;

/// One connected plugin's host-side runtime state, as `ListPluginStates` (#423)
/// reports it. Absent for a plugin id the host doesn't list — i.e. one with no
/// live host connection (started but never registered, or stopped).
#[derive(Clone)]
struct PluginRuntime {
    /// Whether the plugin has parked at least one render frame (its card/chip is
    /// live), vs. connected-but-not-yet-drawing.
    rendering: bool,
    /// The mount region it registered for (wire name), or `""` if unknown.
    mount: String,
    /// This plugin's declared `HYTTE_PLUGIN_MOUNT` override, read straight out
    /// of `plugins.json` (#1161) — `None` when the file declares no override
    /// for this id, in which case [`mount`](Self::mount) alone is the whole
    /// story.
    ///
    /// **`Some` is the interesting bit, not `Some` + disagreement.** The SDK
    /// applies `HYTTE_PLUGIN_MOUNT` before `Register`, so an override that
    /// *works* makes this equal [`mount`](Self::mount) — which means
    /// comparing the two can only ever detect the **failure** case, while
    /// #1161 asked to make an override *in force* visible. So
    /// [`mount_display`] keys the "set by nix" note off `is_some()` and uses
    /// the disagreement only to add "(not applied)" (#1260 review F1/F2).
    declared_mount: Option<String>,
    /// Seconds since the host last saw a frame (or its `Register`).
    last_seen_secs: u64,
    /// Effects the host's containment guards dropped (#435 rate cap / #436
    /// capability enforcement) over the connection's life.
    violations: u32,
}

/// Everything the UI knows about one plugin at the last poll, cached by id.
///
/// The detail pane needs this when the *selection* changes rather than the
/// data: a click between two polls has to render immediately from what the last
/// poll returned, not blank for up to [`PLUGIN_POLL_INTERVAL`] or fire an
/// extra round trip.
#[derive(Clone)]
struct PluginSnapshot {
    /// The unit's systemd `ActiveState`.
    active_state: String,
    /// Whether the unit is enabled (starts at login).
    enabled: bool,
    /// The host-side runtime state, or `None` for no live connection.
    rt: Option<PluginRuntime>,
}

/// One built sidebar row's live widgets, kept keyed by id so the periodic
/// refresh can update it **in place** (no rebuild → no flicker, and no lost
/// selection) while the plugin set is unchanged.
#[derive(Clone)]
struct PluginRow {
    row: adw::ActionRow,
    /// The prefix runtime badge (#423): a recoloured symbolic icon.
    badge: gtk::Image,
    /// The status column (#887): the compact word [`status_cell`] picks.
    status: gtk::Label,
}

/// One plugin's config form, mounted in the detail pane (#888 P1).
///
/// Which family a plugin owns is decided by the **binary** its unit runs, so
/// the mounted form is keyed by the plugin id it was built for and rebuilt
/// only when the selection moves to a plugin with a different one. Dropping
/// it stops the form's own re-read poll.
struct MountedForm {
    /// The plugin id this form was built for.
    plugin: String,
    /// The form. Its groups are in [`PluginDetail::plugin_page`] until this
    /// is dropped.
    form: crate::config_form::Form,
}

/// The detail pane's live widgets. Built once and retargeted at the selected
/// plugin — see the module docs on why there is exactly one of these.
#[derive(Clone)]
struct PluginDetail {
    /// The content `AdwNavigationPage`; its title is the selected plugin's id,
    /// which is also what the collapsed push shows in the header.
    page: adw::NavigationPage,
    /// `"empty"` (nothing selected) ⇄ `"plugin"` (the controls) ⇄ `"shell"`
    /// (the shell-owned config families, #888 P1).
    stack: gtk::Stack,
    /// The plugin page itself, so a *Configuration* group can be added to and
    /// removed from it as the selection moves (#888 P1).
    plugin_page: adw::PreferencesPage,
    /// The selected plugin's config form, when its binary owns a family.
    /// `None` for a plugin with no config file of its own — which is most of
    /// them.
    config: Rc<RefCell<Option<MountedForm>>>,
    /// The relocated on/off control: start+enable, or stop+disable.
    switch: adw::SwitchRow,
    /// The unit's own state — [`plugin_subtitle`]'s wording, unchanged.
    unit_row: adw::ActionRow,
    /// The host's live view — [`runtime_overlay`]'s status line in full.
    conn_row: adw::ActionRow,
    /// [`conn_row`](Self::conn_row)'s prefix badge.
    conn_badge: gtk::Image,
}

/// A selection set aside while the shell is unreachable, so a transient failure
/// doesn't retarget the user's drill-down (#943 review).
///
/// `list_plugins` runs with `RetryPolicy::Never` and a 3 s timeout on a 2 s
/// cadence, so **one** slow reply is enough to take the `Err` arm and show the
/// "Unavailable" placeholder. That placeholder has to drop the selection — the
/// rows it belonged to are gone — but the *id* is cheap to keep, and the next
/// good poll almost always brings the same plugin set back. Without this, the
/// rebuild after the placeholder sees no previous selection, takes the
/// first-load arm and silently moves the user to the first plugin in the list.
///
/// Carries any live [`PendingToggle`] for the same reason (#945 review,
/// finding 3): an "unavailable" placeholder is exactly the kind of transient
/// failure the selection itself is parked against, and dropping the intent
/// along with the rows would re-expose the very bounce #944 fixed the moment
/// the poll recovers — a stale poll inside [`PENDING_TOGGLE_TIMEOUT`] would
/// snap the just-restored switch back to whatever it read before the outage.
struct ParkedSelection {
    /// The plugin that was selected when the poll failed.
    id: String,
    /// Whether its detail page was pushed (i.e. the tab was collapsed and the
    /// user had drilled in) at that moment, so the restore can put the
    /// navigation back where it was and not merely the highlight.
    pushed: bool,
    /// The intent that was pending for [`id`](Self::id), if any, moved here
    /// wholesale — including its original `since` — so the timeout keeps
    /// counting from when the user actually toggled, not from the restore.
    pending: Option<PendingToggle>,
}

/// A user-initiated toggle the poll hasn't confirmed yet (#944).
///
/// `refresh_detail` drives the switch straight off the last poll's
/// `ActiveState` on every tick, which is right *until* the user has just
/// asked for the opposite: a poll that lands mid-transition (still reporting
/// the pre-toggle state) would otherwise snap the switch back until the next
/// tick catches up. Recorded by `connect_switch` the instant the switch
/// flips, and consulted (and cleared) by [`resolve_pending`] on every
/// `refresh_detail` for the plugin it names.
struct PendingToggle {
    /// The plugin this intent is about. A `refresh_detail` for any other
    /// plugin drops it outright — see [`resolve_pending`].
    plugin_id: String,
    /// The state the user asked for: `true` = start+enable, `false` =
    /// stop+disable.
    wanted: bool,
    /// When the toggle happened, for [`PENDING_TOGGLE_TIMEOUT`].
    since: Instant,
}

/// What the sidebar list is currently showing, so a poll only rebuilds on a
/// real transition (list ⇄ empty ⇄ unavailable) and otherwise updates in place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginsView {
    Uninit,
    List,
    Empty,
    Unavailable,
}

/// Monotonic ordering over the tab's **overlapping** `Control` polls (#983).
///
/// Nothing serialises the polls. [`build_page`]'s timer fires every
/// [`PLUGIN_POLL_INTERVAL`], [`refresh_plugins_soon`] adds two more after a
/// toggle, and each one is a [`list_plugins_and_states`] round trip — two
/// sequential calls with a 3 s timeout each, so 0–6 s wide — spawned onto the
/// shared runtime and fired-and-forgotten by [`spawn_on_runtime`]. One slow
/// reply is therefore enough to make completions arrive out of order, and
/// [`apply_plugins`] rewrites [`PluginsState::snapshot`] wholesale: a stale
/// `inactive` landing after a newer `active` re-bounces the detail switch
/// (**after** #944/#945's latch was legitimately cleared by the newer poll, so
/// that mechanism cannot help — this is a different door) and regresses every
/// row's status column with it.
///
/// The cure is ordering, not exclusion: each spawn takes an [`issue`]d
/// generation, and its completion [`accept`]s it only if no newer generation
/// has already been applied. An older result is dropped whole — including its
/// `Err`, which would otherwise replace a live list with the "Unavailable"
/// placeholder. Deliberately *not* an in-flight guard: skipping ticks while a
/// 3 s poll drags would also stop [`refresh_plugins_soon`]'s settle re-poll
/// from ever landing, and the newest answer is the one worth having.
///
/// [`issue`]: PollGenerations::issue
/// [`accept`]: PollGenerations::accept
#[derive(Default)]
struct PollGenerations {
    /// The generation handed to the most recently spawned poll.
    issued: Cell<u64>,
    /// The newest generation whose result has been applied to the tab. `0`
    /// until the first completion, which is below every issued generation.
    applied: Cell<u64>,
}

impl PollGenerations {
    /// Stamp a freshly spawned poll with the next generation.
    ///
    /// `saturating_add` only to keep the arithmetic total: at one poll per
    /// [`PLUGIN_POLL_INTERVAL`] the counter needs ~10¹² years to reach
    /// `u64::MAX`, so the saturating branch is unreachable rather than a
    /// behaviour worth designing around.
    fn issue(&self) -> u64 {
        let next = self.issued.get().saturating_add(1);
        self.issued.set(next);
        next
    }

    /// Claim `generation` as the newest applied, or refuse it (`false`)
    /// because a newer poll's result already landed.
    ///
    /// Strictly greater: a generation is issued once, so an equal value can
    /// only be the same poll's completion running twice, which is not a thing
    /// [`spawn_on_runtime`] does.
    fn accept(&self, generation: u64) -> bool {
        if generation <= self.applied.get() {
            return false;
        }
        self.applied.set(generation);
        true
    }
}

/// Shared, mutable state threaded through the Plugins tab's refresh path so the
/// build, the detail pane's handlers and the poll timer all drive the same
/// widgets.
#[derive(Clone)]
struct PluginsState {
    /// The split view, for `show-content` (the collapsed push/pop).
    split: adw::NavigationSplitView,
    /// The sidebar list.
    list: gtk::ListBox,
    /// The pinned **Shell** entry above it (#888 P1): the config families the
    /// shell itself owns (`core-leds`, `workspaces`), which have no plugin to
    /// hang off.
    ///
    /// A second `GtkListBox` rather than a row in [`list`](Self::list): that
    /// list is torn down and rebuilt on every membership change and replaced
    /// wholesale by a placeholder when the shell is unreachable, and the one
    /// thing this entry must do is keep working while the shell is down —
    /// it edits files, exactly as the Places tab does. Selection is
    /// coordinated by hand (each list deselects the other), which is what
    /// [`shell_selected`](Self::shell_selected) is for.
    shell_list: gtk::ListBox,
    /// Whether the **Shell** entry is what the detail pane is showing.
    ///
    /// Not folded into [`selected`](Self::selected): that field is a *plugin
    /// id*, every path that reads it means "which plugin", and widening it to
    /// an enum would touch every one of them for a page that is not a plugin
    /// at all.
    shell_selected: Rc<Cell<bool>>,
    /// The shell-owned families' forms, built once with the tab.
    ///
    /// Held for their lifetime, not their widgets': dropping a
    /// `config_form::Form` is what stops its re-read poll, and these live as
    /// long as the tab does.
    shell_forms: Rc<Vec<crate::config_form::Form>>,
    /// The one detail pane.
    detail: PluginDetail,
    /// Every child currently in [`list`](Self::list) (plugin rows or a single
    /// placeholder), for teardown before a rebuild.
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    /// The plugin rows keyed by id, for the in-place update.
    by_id: Rc<RefCell<HashMap<String, PluginRow>>>,
    /// The last poll's data per id, for a selection change between polls.
    snapshot: Rc<RefCell<HashMap<String, PluginSnapshot>>>,
    /// The selected plugin's id, or `None` for the empty state.
    selected: Rc<RefCell<Option<String>>>,
    /// The selection held over an "unavailable" placeholder — see
    /// [`ParkedSelection`]. `None` whenever the tab is showing real rows.
    parked: Rc<RefCell<Option<ParkedSelection>>>,
    /// What's currently shown, gating rebuild vs. in-place update.
    view: Rc<Cell<PluginsView>>,
    /// Ordering over the overlapping polls (#983) — see [`PollGenerations`].
    polls: Rc<PollGenerations>,
    /// A user toggle the poll hasn't confirmed yet (#944) — see
    /// [`PendingToggle`]. `None` whenever the shown plugin's switch is free to
    /// follow the snapshot.
    pending: Rc<RefCell<Option<PendingToggle>>>,
    /// Guard so programmatically setting the detail switch from a fetched state
    /// doesn't loop back into a Start/Stop call (mirrors the Places tab's).
    syncing: Rc<Cell<bool>>,
    /// Guard so *programmatic* list selection — restoring it after a rebuild,
    /// or the `row-selected(None)` that removing rows emits — doesn't run the
    /// user-driven selection path and, with it, disturb navigation.
    selecting: Rc<Cell<bool>>,
    /// Whether the most recently *applied* poll's outcome was a failure —
    /// `None` before the first completion. Drives transitions-only logging
    /// (#1017, mirrors `ai_keys_tab`'s field of the same name/shape): a run
    /// of identical outcomes logs once, not once per poll — see
    /// [`on_poll_result_with_declared`].
    last_failing: Rc<Cell<Option<bool>>>,
    /// `plugins.json`'s declared `HYTTE_PLUGIN_MOUNT` overrides (#1161),
    /// parsed at most once per change to the file — see [`DeclaredMounts`]
    /// for why the 2 s poll must not re-read it (#1260 review F7).
    declared: Rc<RefCell<DeclaredMounts>>,
    /// The process environment [`plugins_json_candidates`] resolves
    /// [`search_path`](Self::search_path) from, read once in [`build_tab`]
    /// and reused for the tab's whole life.
    ///
    /// `hytte_config::xdg::Env::from_process()` is cheap — it only reads four
    /// environment variables — but [`search_path`](Self::search_path)'s
    /// `OnceCell` only masks the **result** of consulting it: until #1286,
    /// `refresh_plugins` still called `Env::from_process()` fresh on every
    /// [`PLUGIN_POLL_INTERVAL`] tick and passed the new value in, immediately
    /// discarded once `search_path` was already resolved (#1279 review N3).
    /// Harmless while `Env::from_process()` does nothing but read four
    /// variables, but "the process env is read at most once" was a
    /// convention at the call site rather than a fact about the type — if
    /// `Env::from_process()` ever grows a warning of its own (it is the one
    /// function in `hytte_config::xdg` that touches the environment), the
    /// tick would refire it forever on a misconfigured box, the same failure
    /// mode [`search_path`](Self::search_path) exists to close. Holding the
    /// resolved `Env` here instead makes "read once" the type: nothing left
    /// on the tick's path can call `Env::from_process()` again.
    env: Rc<hytte_config::xdg::Env>,
    /// The `plugins.json` search path (#1260 review F8), resolved from
    /// [`env`](Self::env) the first time [`refresh_plugins`] runs and held
    /// for the rest of the tab's life.
    ///
    /// [`plugins_json_candidates`]'s `env.config_home()`/`env.config_dirs()`
    /// calls `tracing::warn!` on a relative `$XDG_CONFIG_HOME`/
    /// `$XDG_CONFIG_DIRS` (`hytte_config::xdg`'s `is_absolute`, #985), and
    /// re-resolving the search path on every [`PLUGIN_POLL_INTERVAL`] tick
    /// re-fires that warning every tick, for the window's whole life, on a
    /// misconfigured box (#1270, inherited nit from the #1260 review). The
    /// environment a running process sees cannot change out from under it, so
    /// the search path it implies cannot either — a `OnceCell` makes
    /// "resolved (and, if bad, warned about) once" the type, not a convention
    /// a future edit could quietly break by moving the resolution back inside
    /// the tick.
    search_path: Rc<OnceCell<Vec<PathBuf>>>,
}

/// [`PluginsState`] with its widget handles held **weakly** — what the
/// tab's own handlers capture, and the reason they don't leak the tab (#943
/// review).
///
/// GTK owns a signal handler for as long as it owns the widget it is connected
/// to, so a handler that captures a strong `PluginsState` closes a cycle:
/// `list` → its `row-selected` handler → `PluginsState` → `list`. Nothing
/// breaks it, and the whole tab tree — both panes, every row — outlives the
/// window it was built for. That is the same defect
/// `crates/hytte-reactive/src/bind.rs` avoids by holding its widget through a
/// [`glib::WeakRef`] and handing it back to the closure (`nix/lint-bind-pins.py`
/// exists to keep call sites honest about it); this is that convention applied
/// to a state struct rather than a single widget.
///
/// The `Rc` cells are cloned strongly on purpose: none of them refers to an
/// **ancestor** of a widget with a handler on it (`by_id` and `rows` hold the
/// list's own children, and a GTK child does not hold a reference to its
/// parent), so they close no cycle, and holding them strongly is what lets a
/// handler that fires during teardown still see coherent bookkeeping.
#[derive(Clone)]
struct WeakPluginsState {
    split: glib::WeakRef<adw::NavigationSplitView>,
    list: glib::WeakRef<gtk::ListBox>,
    shell_list: glib::WeakRef<gtk::ListBox>,
    shell_selected: Rc<Cell<bool>>,
    shell_forms: Rc<Vec<crate::config_form::Form>>,
    detail: WeakPluginDetail,
    rows: Rc<RefCell<Vec<gtk::Widget>>>,
    by_id: Rc<RefCell<HashMap<String, PluginRow>>>,
    snapshot: Rc<RefCell<HashMap<String, PluginSnapshot>>>,
    selected: Rc<RefCell<Option<String>>>,
    parked: Rc<RefCell<Option<ParkedSelection>>>,
    view: Rc<Cell<PluginsView>>,
    polls: Rc<PollGenerations>,
    pending: Rc<RefCell<Option<PendingToggle>>>,
    syncing: Rc<Cell<bool>>,
    selecting: Rc<Cell<bool>>,
    last_failing: Rc<Cell<Option<bool>>>,
    declared: Rc<RefCell<DeclaredMounts>>,
    env: Rc<hytte_config::xdg::Env>,
    search_path: Rc<OnceCell<Vec<PathBuf>>>,
}

/// [`PluginDetail`]'s widgets, weakly — see [`WeakPluginsState`].
#[derive(Clone)]
struct WeakPluginDetail {
    page: glib::WeakRef<adw::NavigationPage>,
    stack: glib::WeakRef<gtk::Stack>,
    plugin_page: glib::WeakRef<adw::PreferencesPage>,
    /// Strongly, like the other `Rc` cells: a mounted form holds
    /// `AdwPreferencesGroup`s that are *children* of `plugin_page`, and a
    /// child does not hold its parent — so this closes no cycle, and holding
    /// it strongly is what lets a handler firing during teardown still see
    /// coherent bookkeeping.
    config: Rc<RefCell<Option<MountedForm>>>,
    switch: glib::WeakRef<adw::SwitchRow>,
    unit_row: glib::WeakRef<adw::ActionRow>,
    conn_row: glib::WeakRef<adw::ActionRow>,
    conn_badge: glib::WeakRef<gtk::Image>,
}

impl PluginsState {
    /// The handler-side view of this state.
    fn downgrade(&self) -> WeakPluginsState {
        WeakPluginsState {
            split: self.split.downgrade(),
            list: self.list.downgrade(),
            shell_list: self.shell_list.downgrade(),
            shell_selected: self.shell_selected.clone(),
            shell_forms: self.shell_forms.clone(),
            detail: WeakPluginDetail {
                page: self.detail.page.downgrade(),
                stack: self.detail.stack.downgrade(),
                plugin_page: self.detail.plugin_page.downgrade(),
                config: self.detail.config.clone(),
                switch: self.detail.switch.downgrade(),
                unit_row: self.detail.unit_row.downgrade(),
                conn_row: self.detail.conn_row.downgrade(),
                conn_badge: self.detail.conn_badge.downgrade(),
            },
            rows: self.rows.clone(),
            by_id: self.by_id.clone(),
            snapshot: self.snapshot.clone(),
            selected: self.selected.clone(),
            parked: self.parked.clone(),
            view: self.view.clone(),
            polls: self.polls.clone(),
            pending: self.pending.clone(),
            syncing: self.syncing.clone(),
            selecting: self.selecting.clone(),
            last_failing: self.last_failing.clone(),
            declared: self.declared.clone(),
            env: self.env.clone(),
            search_path: self.search_path.clone(),
        }
    }
}

impl WeakPluginsState {
    /// Rebuild the strong state for the duration of one callback, or `None`
    /// once the tab has been dropped — in which case there is nothing to
    /// update and the handler returns. All-or-nothing on purpose: the widgets
    /// live and die as one tree, so a partial upgrade would mean a torn tab,
    /// not a case worth handling.
    fn upgrade(&self) -> Option<PluginsState> {
        Some(PluginsState {
            split: self.split.upgrade()?,
            list: self.list.upgrade()?,
            shell_list: self.shell_list.upgrade()?,
            shell_selected: self.shell_selected.clone(),
            shell_forms: self.shell_forms.clone(),
            detail: PluginDetail {
                page: self.detail.page.upgrade()?,
                stack: self.detail.stack.upgrade()?,
                plugin_page: self.detail.plugin_page.upgrade()?,
                config: self.detail.config.clone(),
                switch: self.detail.switch.upgrade()?,
                unit_row: self.detail.unit_row.upgrade()?,
                conn_row: self.detail.conn_row.upgrade()?,
                conn_badge: self.detail.conn_badge.upgrade()?,
            },
            rows: self.rows.clone(),
            by_id: self.by_id.clone(),
            snapshot: self.snapshot.clone(),
            selected: self.selected.clone(),
            parked: self.parked.clone(),
            view: self.view.clone(),
            polls: self.polls.clone(),
            pending: self.pending.clone(),
            syncing: self.syncing.clone(),
            selecting: self.selecting.clone(),
            last_failing: self.last_failing.clone(),
            declared: self.declared.clone(),
            env: self.env.clone(),
            search_path: self.search_path.clone(),
        })
    }
}

/// Build the real **Plugins** tab: an adaptive drill-down over the
/// `trollshell-plugin-<id>` systemd **user** units (#887).
///
/// The sidebar lists every unit with its status; the detail pane carries the
/// controls — a switch that starts+enables or stops+disables the unit, so the
/// choice both applies now and persists across logins — plus the unit's state
/// and the host's live view of the plugin's socket connection (#423): connected
/// and rendering, connected but not yet drawing, or the diagnostic case a unit
/// list alone cannot show, active-but-never-connected. When the shell isn't
/// running the list call fails and the sidebar shows an "unavailable" row — no
/// panic.
///
/// Returns the tab's root widget and the poll `SourceId`; the caller ties the
/// latter to the window so the timer dies with it (#542) rather than polling
/// `Control` forever after the window closes.
pub(crate) fn build_page() -> (adw::BreakpointBin, glib::SourceId) {
    let (bin, state) = build_tab();

    refresh_plugins(&state);

    // Live overlay (#423): poll on an interval so the badges track reality
    // without the user reopening the tab. `refresh_plugins` updates in place
    // while the plugin set is unchanged, so a steady set never flickers *and*
    // never disturbs the selection or a pushed detail page (#887).
    let poll = {
        let state = state.clone();
        glib::timeout_add_local(PLUGIN_POLL_INTERVAL, move || {
            refresh_plugins(&state);
            glib::ControlFlow::Continue
        })
    };
    (bin, poll)
}

/// The widget tree and its state, with no `Control` traffic and no timer.
///
/// Split out of [`build_page`] so the GTK tests can drive the layout and the
/// refresh path with fabricated data instead of a live shell — which is the
/// only way to test either, since a test process has no session bus to answer
/// `ListPlugins`.
fn build_tab() -> (adw::BreakpointBin, PluginsState) {
    // ── Sidebar: the plugin list ────────────────────────────────────────────
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    // libadwaita's own sidebar list styling — the same class GNOME apps put on
    // the list inside an `AdwNavigationSplitView` sidebar.
    list.add_css_class("navigation-sidebar");

    // ── Sidebar: the pinned Shell entry (#888 P1) ───────────────────────────
    //
    // Above the plugin list and outside it, so `clear_rows`' teardown and the
    // "shell unavailable" placeholder — both of which replace the plugin
    // list's every row — cannot take it with them. Editing `core-leds.toml`
    // is the Places tab's argument exactly: the file is the state store and
    // the shell is a client of it, so the editor has to keep working while
    // the shell is down.
    let shell_list = gtk::ListBox::new();
    shell_list.set_selection_mode(gtk::SelectionMode::Single);
    shell_list.add_css_class("navigation-sidebar");
    shell_list.append(
        &adw::ActionRow::builder()
            .title("Shell")
            .subtitle("Settings the shell itself owns")
            .activatable(true)
            .build(),
    );

    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&shell_list);
    sidebar_box.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar_box.append(&list);

    // `Automatic`, not `Never`: a `Never` horizontal policy makes the scrolled
    // window's minimum width its child's, which would push the split view's
    // minimum past the bin's floor and buy the #856 warning on every collapsed
    // allocation. `Automatic` lets the list shrink and scroll instead.
    let list_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&sidebar_box)
        .build();

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&tab_header_bar());
    sidebar_toolbar.set_content(Some(&list_scroller));
    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Plugins");

    // ── Content: the one detail pane ────────────────────────────────────────
    let env = Rc::new(hytte_config::xdg::Env::from_process());
    let (detail, shell_forms) = build_detail(&env);

    // ── The split view + the breakpoint that collapses it ───────────────────
    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&sidebar_page));
    split.set_content(Some(&detail.page));
    split.set_min_sidebar_width(SIDEBAR_MIN_PX);
    split.set_max_sidebar_width(SIDEBAR_MAX_PX);
    split.set_sidebar_width_fraction(SIDEBAR_FRACTION);

    let bin = adw::BreakpointBin::new();
    // Both axes — see `BIN_MIN_WIDTH_PX`.
    bin.set_size_request(BIN_MIN_WIDTH_PX, BIN_MIN_HEIGHT_PX);
    bin.set_child(Some(&split));

    // A property setter, which is what `AdwBreakpoint` is for: it restores the
    // previous value itself when the condition stops applying, so there is no
    // apply/unapply handler to keep in sync.
    let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        COLLAPSE_WIDTH_PX,
        adw::LengthUnit::Px,
    ));
    breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
    bin.add_breakpoint(breakpoint);

    let state = PluginsState {
        split,
        list,
        shell_list,
        shell_selected: Rc::new(Cell::new(false)),
        shell_forms: Rc::new(shell_forms),
        detail,
        rows: Rc::new(RefCell::new(Vec::new())),
        by_id: Rc::new(RefCell::new(HashMap::new())),
        snapshot: Rc::new(RefCell::new(HashMap::new())),
        selected: Rc::new(RefCell::new(None)),
        parked: Rc::new(RefCell::new(None)),
        view: Rc::new(Cell::new(PluginsView::Uninit)),
        polls: Rc::new(PollGenerations::default()),
        pending: Rc::new(RefCell::new(None)),
        syncing: Rc::new(Cell::new(false)),
        selecting: Rc::new(Cell::new(false)),
        last_failing: Rc::new(Cell::new(None)),
        declared: Rc::new(RefCell::new(DeclaredMounts::default())),
        env,
        search_path: Rc::new(OnceCell::new()),
    };

    connect_selection(&state);
    connect_shell_entry(&state);
    connect_switch(&state);
    show_empty_detail(&state);

    (bin, state)
}

/// A header bar for one of the tab's two `AdwToolbarView`s, with the window
/// controls off.
///
/// `AdwHeaderBar` defaults `show-start-title-buttons` /
/// `show-end-title-buttons` to `true`, which is right for a header bar that is
/// the window's own. These two are not: the tab is mounted *inside*
/// `crate::build_window`'s `AdwApplicationWindow`, which already has its header
/// bar (with the view switcher) at the top. Left on the defaults, the sidebar
/// and content header bars each draw a second, live `GtkWindowControls` cluster
/// — hit-testable close/minimise/maximise buttons that appear and disappear as
/// the user switches tabs, since Places and AI Keys have no header bar of their
/// own. `the_tab_draws_no_window_controls` pins it.
///
/// `pub(crate)`: [`crate::places_tab`]'s pushed detail page (#944) reuses this
/// rather than building its own `adw::HeaderBar::new()` — same reasoning,
/// same fix, one place to change it.
pub(crate) fn tab_header_bar() -> adw::HeaderBar {
    adw::HeaderBar::builder()
        .show_start_title_buttons(false)
        .show_end_title_buttons(false)
        .build()
}

/// Build the detail pane once: an empty state, the per-plugin controls and the
/// shell-owned config page, in a stack under a header bar that grows a back
/// button when collapsed.
///
/// Returns the shell families' forms beside it: their groups are in the
/// `"shell"` page, but a form's *handle* is what owns its re-read poll, so the
/// tab holds them for its own lifetime.
fn build_detail(env: &Rc<hytte_config::xdg::Env>) -> (PluginDetail, Vec<crate::config_form::Form>) {
    // The empty state carries what the old `AdwPreferencesGroup` description
    // said, because it is the only place that blurb still has. It is *not*
    // where a fresh tab lands: `apply_plugins` settles the sidebar on the
    // first plugin on the first load, so with any plugin installed this page
    // is reachable only in the two no-rows-to-drill-into states — no plugins
    // installed, or the shell unreachable (`set_placeholder` → both go through
    // `clear_selection`) — and when the selected plugin vanishes from the
    // snapshot. That is exactly when a plugins-are-units explainer is worth
    // reading, so it stays.
    // No `<id>` in the text: `AdwStatusPage`'s description is a plain label,
    // so an escaped `&lt;id&gt;` would render literally — the ellipsis says
    // the same thing with no markup exposure either way.
    let empty = adw::StatusPage::builder()
        .icon_name("application-x-addon-symbolic")
        .title("No plugin selected")
        .description(
            "Widget plugins run as trollshell-plugin-… systemd user units. Pick one to \
             start or stop it, and to see the host's live view of it.",
        )
        .build();

    let switch = adw::SwitchRow::builder()
        .title("Running")
        .subtitle("Start and enable the unit, or stop and disable it")
        .build();
    let controls = adw::PreferencesGroup::new();
    controls.add(&switch);

    let unit_row = adw::ActionRow::builder().title("Unit").build();
    let conn_row = adw::ActionRow::builder().title("Host connection").build();
    let conn_badge = gtk::Image::new();
    conn_badge.set_valign(gtk::Align::Center);
    conn_row.add_prefix(&conn_badge);
    let status_group = adw::PreferencesGroup::builder()
        .title("Status")
        .description("The unit's own state, and the host's live view of the plugin's connection.")
        .build();
    status_group.add(&unit_row);
    status_group.add(&conn_row);

    let plugin_page = adw::PreferencesPage::new();
    plugin_page.add(&controls);
    plugin_page.add(&status_group);

    // The shell-owned families (#888 P1). Built once, with the tab: unlike a
    // plugin's, this page's content does not depend on a selection.
    let shell_page = adw::PreferencesPage::new();
    let shell_forms: Vec<crate::config_form::Form> = crate::config_form::shell_families()
        .into_iter()
        .map(|ops| {
            let form = crate::config_form::build(ops, env);
            for group in form.groups() {
                shell_page.add(group);
            }
            form
        })
        .collect();

    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&plugin_page, Some("plugin"));
    stack.add_named(&shell_page, Some("shell"));

    let toolbar = adw::ToolbarView::new();
    // No explicit back button: inside a collapsed `AdwNavigationSplitView` the
    // header bar is in a navigation stack with the sidebar beneath it, and
    // `AdwHeaderBar` grows the back button itself.
    toolbar.add_top_bar(&tab_header_bar());
    toolbar.set_content(Some(&stack));

    let page = adw::NavigationPage::new(&toolbar, "Plugin");
    (
        PluginDetail {
            page,
            stack,
            plugin_page,
            config: Rc::new(RefCell::new(None)),
            switch,
            unit_row,
            conn_row,
            conn_badge,
        },
        shell_forms,
    )
}

/// Wire the pinned **Shell** entry (#888 P1).
///
/// Selecting it clears the plugin selection and shows the shell page;
/// selecting a plugin clears this one ([`connect_selection`] does the mirror
/// image). Both sides run under the tab's existing `selecting` guard, so
/// deselecting one list does not drive the other's user-selection path.
fn connect_shell_entry(state: &PluginsState) {
    let weak = state.downgrade();
    state.shell_list.connect_row_selected(move |_, row| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.selecting.get() || row.is_none() {
            return;
        }
        state.shell_selected.set(true);
        // Whatever plugin was shown is not shown any more, so no plugin's
        // intent is "for" the detail pane — the rule `clear_selection` and
        // `refresh_detail` both apply (#944).
        state.pending.borrow_mut().take();
        // And a selection parked behind an "unavailable" placeholder (#943)
        // is retired here rather than left to be restored: it exists so a
        // transient failure does not move the user, and this *is* the user
        // moving. Without this, the next good poll would put the plugin page
        // back over the page they just opened.
        state.parked.borrow_mut().take();
        *state.selected.borrow_mut() = None;
        state.selecting.set(true);
        state.list.select_row(None::<&gtk::ListBoxRow>);
        state.selecting.set(false);
        refresh_shell_forms(&state);
        show_shell_detail(&state);
    });

    let weak = state.downgrade();
    state.shell_list.connect_row_activated(move |_, _| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        // Collapsed, this *is* the push; uncollapsed the split view already
        // satisfies it.
        state.split.set_show_content(true);
    });
}

/// Show the shell-owned config page and title the detail pane for it.
fn show_shell_detail(state: &PluginsState) {
    state.detail.page.set_title("Shell");
    state.detail.stack.set_visible_child_name("shell");
}

/// Re-read every shell family now, so opening the page shows what the files
/// say rather than what they said up to one poll tick ago.
fn refresh_shell_forms(state: &PluginsState) {
    for form in state.shell_forms.iter() {
        form.refresh_from_disk();
    }
}

/// Drop the **Shell** selection, because a plugin row was picked instead.
fn clear_shell_selection(state: &PluginsState) {
    if !state.shell_selected.replace(false) {
        return;
    }
    state.selecting.set(true);
    state.shell_list.select_row(None::<&gtk::ListBoxRow>);
    state.selecting.set(false);
}

/// Wire the two halves of drill-down: `row-selected` retargets the detail pane
/// (which is all a wide layout needs, and is also what keyboard arrows drive),
/// `row-activated` additionally shows the content — a push, when collapsed.
///
/// Both closures capture the **weak** state ([`WeakPluginsState`]) and upgrade
/// per callback: these handlers are owned by the very list they would otherwise
/// hold, and the strong version of that is a cycle the tab never escapes.
fn connect_selection(state: &PluginsState) {
    {
        let weak = state.downgrade();
        state.list.connect_row_selected(move |_, row| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            // A programmatic selection: restoring one after a rebuild, or the
            // `None` that removing the selected row emits. Neither is the user
            // navigating, so neither may touch the detail or the stack.
            if state.selecting.get() {
                return;
            }
            let id = row.and_then(|row| id_for_row(&state, row));
            // A plugin row is what the pane shows now, not the Shell entry
            // (#888 P1). `row == None` is the user ctrl-clicking the
            // selection away, which is not a reason to hand the pane back to
            // a Shell page they left.
            if id.is_some() {
                clear_shell_selection(&state);
            }
            *state.selected.borrow_mut() = id;
            refresh_detail(&state);
        });
    }
    {
        let weak = state.downgrade();
        state.list.connect_row_activated(move |_, row| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let Some(id) = id_for_row(&state, row) else {
                return;
            };
            clear_shell_selection(&state);
            *state.selected.borrow_mut() = Some(id);
            refresh_detail(&state);
            // Collapsed, this *is* the push. Uncollapsed it is a no-op the
            // split view already satisfies.
            state.split.set_show_content(true);
        });
    }
}

/// Wire the relocated on/off control. One switch serves every plugin, so it
/// reads the selected id at fire time rather than capturing one.
///
/// Weakly, for [`connect_selection`]'s reason: the switch is inside the tab the
/// state holds. The strong clone the async completion takes is a *bounded*
/// hold (one `Control` round trip plus the 1.2 s settle poll), not an
/// ownership edge — and it wants the tab alive to refresh it.
fn connect_switch(state: &PluginsState) {
    let weak = state.downgrade();
    state.detail.switch.connect_active_notify(move |sw| {
        let Some(state) = weak.upgrade() else {
            return;
        };
        if state.syncing.get() {
            return;
        }
        let selected = state.selected.borrow().clone();
        let Some(id) = selected else {
            return;
        };
        let want_on = sw.is_active();
        // Record the intent before the round trip even starts (#944): a poll
        // that lands before `StartPlugin`/`StopPlugin` has taken effect must
        // not read as the truth until either a poll agrees or the timeout
        // gives up. `syncing` above is what keeps this arm from firing at all
        // for `refresh_detail`'s own programmatic `set_active`, so every
        // intent recorded here really did come from the user.
        //
        // `since` is captured here, not re-read from `state.pending` in the
        // completion below, because the switch can be flipped again before
        // this round trip lands — `on_toggle_result` needs to tell "this
        // call's own intent" from "a newer one" apart, and a timestamp taken
        // at record time is what makes that comparison exact.
        let since = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: id.clone(),
            wanted: want_on,
            since,
        });
        spawn_on_runtime(set_plugin_state(id, want_on), move |res| {
            on_toggle_result(&state, since, res);
        });
    });
}

/// The completion half of the round trip [`connect_switch`] starts (#945
/// review, finding 1): a failed `StartPlugin`/`StopPlugin` already knows —
/// at the call's own `RetryPolicy::Never` timeout, well inside
/// [`PENDING_TOGGLE_TIMEOUT`] — that the transition it recorded an intent for
/// never happened, so it must clear that intent rather than let
/// [`resolve_pending`] keep answering `wanted` for the full 10s window on a
/// switch that is never coming back on its own.
///
/// Guarded on identity: `since` is the timestamp *this* call's intent was
/// recorded with, captured by `connect_switch` before the round trip started.
/// If the user flipped the switch again while this call was in flight,
/// `state.pending` now holds a newer intent with a different `since` — a
/// later toggle the user asked for after this one — and this completion must
/// not clobber it.
///
/// Split out of the `connect_switch` closure so it can be driven directly in
/// tests with a fabricated `res`, with no session bus needed — see
/// `gtk_tests`' `a_failed_toggle_clears_its_own_intent` and
/// `a_failed_toggles_completion_does_not_clobber_a_newer_intent`.
///
/// #945 re-check: the intent isn't always in `state.pending` by the time this
/// runs. `set_placeholder` moves it wholesale into `state.parked`'s
/// [`ParkedSelection::pending`] the moment a poll failure parks the selection
/// (`:1134`), and a correlated outage — `ListPlugins` and `StartPlugin` hit
/// the same `Control` endpoint, so one dead shell fails both — typically parks
/// it before this completion arrives. The identity guard above then finds
/// `state.pending == None`, clears nothing, and the definitively failed
/// intent would otherwise be restored with the selection on the next good
/// poll (the switch lies again until [`PENDING_TOGGLE_TIMEOUT`]). So this also
/// checks the park for the same `since` and drops the intent there — leaving
/// the parked selection itself alone, same as `set_placeholder`'s own
/// `take()`. The two homes are mutually exclusive (an intent lives in exactly
/// one), so at most one of the two clears ever fires.
fn on_toggle_result(state: &PluginsState, since: Instant, res: Result<(), hytte_bus::BusError>) {
    if let Err(err) = res {
        tracing::info!(%err, "plugin start/stop failed");
        let still_this_intent = state
            .pending
            .borrow()
            .as_ref()
            .is_some_and(|intent| intent.since == since);
        if still_this_intent {
            state.pending.borrow_mut().take();
        }
        let still_parked_intent = state
            .parked
            .borrow()
            .as_ref()
            .and_then(|parked| parked.pending.as_ref())
            .is_some_and(|intent| intent.since == since);
        if still_parked_intent && let Some(parked) = state.parked.borrow_mut().as_mut() {
            parked.pending = None;
        }
    }
    // Either way: an immediate re-poll snaps the switch to the truth as soon
    // as the shell has it, on success or on failure.
    refresh_plugins_soon(state);
}

/// Which plugin a sidebar row belongs to.
///
/// By widget identity rather than by index: the row order and the `by_id` map
/// are maintained by different code paths, and a lookup that silently returns
/// the *wrong* plugin on a desync is worse than one that returns `None`. The
/// map holds a handful of entries, so the scan is free. Returns `None` for the
/// placeholder row (no plugins / shell unavailable), which is exactly right —
/// there is nothing to drill into.
fn id_for_row(state: &PluginsState, row: &gtk::ListBoxRow) -> Option<String> {
    let map = state.by_id.borrow();
    let found = map
        .iter()
        .find(|(_, prow)| prow.row.upcast_ref::<gtk::ListBoxRow>() == row)
        .map(|(id, _)| id.clone());
    drop(map);
    found
}

/// Re-read the unit list (`ListPlugins`) plus the runtime overlay
/// (`ListPluginStates`) over `Control` and reflect them into the tab —
/// updating rows and the detail pane in place while the plugin set is
/// unchanged, rebuilding the rows on any structural change, and showing a
/// single placeholder when there are no plugins (informational) or the shell is
/// unreachable ("unavailable").
///
/// The spawn is stamped with a [`PollGenerations::issue`]d generation that
/// [`on_poll_result_with_declared`] compares against the newest already
/// applied, so a slow poll completing after a faster later one is dropped
/// rather than rewriting the tab with its stale answer (#983).
///
/// The declared-mount map comes from [`PluginsState::declared`] here, and is
/// threaded through to [`on_poll_result_with_declared`] rather than read from
/// inside it, so the many existing `on_poll_result` tests stay hermetic
/// (#1161) — none of them touch the real filesystem, and none of them should
/// start to by accident.
///
/// What this tick costs is one [`probe_candidates`] stat, not a read and a
/// `serde_json` parse: the parse happens on the first tick and then only when
/// the file's stamp changes (#1260 review F7 — see [`DeclaredMounts`] for why
/// a stat-shaped stamp is enough for a nix-rendered store symlink). The search
/// path itself — which candidate paths to stat — is resolved from
/// [`PluginsState::env`] through [`PluginsState::search_path`] once, not per
/// tick (#1270 — see those fields' docs for why re-resolving the search path,
/// or rebuilding the `Env` it is resolved from, every tick is the wrong
/// default).
fn refresh_plugins(state: &PluginsState) {
    let generation = state.polls.issue();
    let candidates = resolved_search_path(&state.search_path, &state.env);
    let declared = {
        let probe = probe_candidates(candidates);
        state
            .declared
            .borrow_mut()
            .get(probe, read_declared_mounts_at)
    };
    let state = state.clone();
    spawn_on_runtime(list_plugins_and_states(), move |res| {
        on_poll_result_with_declared(&state, generation, res, &declared);
    });
}

/// One [`list_plugins_and_states`] completion, applied to the tab — or
/// dropped, if a newer poll already landed (#983).
///
/// Split out of [`refresh_plugins`]' closure so the ordering guard is
/// reachable from a test: a `gtk_test` can drive two completions in the wrong
/// order without a session bus to answer `ListPlugins`, which is the only way
/// to reproduce the inversion deterministically.
///
/// The generation gate covers **every** arm, not just the success one: an
/// `Err` from a poll that timed out at t+3 s is exactly as stale as an `Ok`
/// from it, and letting it through would replace a live list with the
/// "Unavailable" placeholder — parking the selection and blanking the
/// snapshot — a second after a newer poll proved the shell is answering fine.
///
/// Logs on transitions only (#1017, `crate::log_transition` — see the module
/// doc's "Transitions-only logging" section): a run of identical outcomes
/// (`Err` or `Ok`) writes one journal line, not one per
/// [`PLUGIN_POLL_INTERVAL`] tick. The transition check runs *after* the
/// generation gate above, so a stale, out-of-order completion (#983) cannot
/// flip [`PluginsState::last_failing`] on its way out — it never reaches this
/// point.
///
/// A thin wrapper over [`on_poll_result_with_declared`] with an empty
/// declared-mount map — i.e. "no plugin has a declared override" — so every
/// existing test call keeps its exact meaning (none of these fixtures declare
/// a `mount`, so this is also the right answer, not just a stub). The one
/// production call site with a real map is [`refresh_plugins`].
///
/// `cfg`-gated to match its only callers (`gtk_tests`, which needs a real
/// display) rather than plain `#[cfg(test)]`: the hermetic `mod tests` below
/// never calls this, so a bare `#[cfg(test)]` would make it dead code under a
/// `cargo test` that doesn't enable `system-tests`.
#[cfg(all(test, feature = "system-tests"))]
fn on_poll_result(state: &PluginsState, generation: u64, res: PollResult) {
    on_poll_result_with_declared(state, generation, res, &HashMap::new());
}

/// `on_poll_result`'s real logic, parameterised by the plugin-id →
/// declared-mount map
/// [`read_declared_mounts_at`] reads out of `plugins.json` (#1161). Split out so
/// that real filesystem read is injectable rather than baked into the
/// function every existing poll-ordering test already drives — see
/// `tests-must-not-touch-real-xdg` in the project's own house rules for why
/// that separation matters here.
fn on_poll_result_with_declared(
    state: &PluginsState,
    generation: u64,
    res: PollResult,
    declared: &HashMap<String, String>,
) {
    if !state.polls.accept(generation) {
        tracing::debug!(
            generation,
            applied = state.polls.applied.get(),
            "dropping an out-of-order plugin poll"
        );
        return;
    }
    let is_err = res.is_err();
    let previous = state.last_failing.replace(Some(is_err));
    match log_transition(previous, is_err) {
        LogTransition::Failed => {
            if let Err(err) = &res {
                tracing::info!(%err, "ListPlugins failed");
            }
        }
        LogTransition::Recovered => tracing::info!("ListPlugins recovered"),
        LogTransition::None => {}
    }
    match res {
        Ok((units, states)) if !units.is_empty() => {
            apply_plugins(state, &units, &runtime_states(states, declared));
        }
        Ok(_) => set_placeholder(
            state,
            PluginsView::Empty,
            "No plugins installed",
            "Install a trollshell-plugin unit to manage it here.",
        ),
        Err(_) => {
            set_placeholder(
                state,
                PluginsView::Unavailable,
                "Unavailable",
                "Is trollshell running?",
            );
        }
    }
}

/// Zip a `ListPluginStates` reply with the declared-mount map
/// [`read_declared_mounts_at`] read into `PluginRuntime`s, keyed by id (#1161).
///
/// Split out of [`on_poll_result_with_declared`] purely so the
/// declared-mount attachment is unit-testable on its own: it takes no
/// [`PluginsState`] (a live GTK widget tree `build_tab` builds, unavailable
/// to the hermetic `mod tests` below), so a test can drive it directly rather
/// than only through `gtk_tests`.
fn runtime_states(
    states: PollStates,
    declared: &HashMap<String, String>,
) -> HashMap<String, PluginRuntime> {
    states
        .into_iter()
        .map(|(id, rendering, mount, last_seen_secs, violations)| {
            let declared_mount = declared.get(&id).cloned();
            (
                id,
                PluginRuntime {
                    rendering,
                    mount,
                    declared_mount,
                    last_seen_secs,
                    violations,
                },
            )
        })
        .collect()
}

/// Every place a `plugins.json` could be, **most important first** — the same
/// first-existing-wins search `trollshell::plugin_launcher::candidate_paths`
/// walks: `$XDG_CONFIG_HOME` (else `$HOME/.config`) first, then each
/// `$XDG_CONFIG_DIRS` entry (else `/etc/xdg`).
///
/// That second half is not decoration: a home-manager install renders
/// `~/.config/trollshell/plugins.json` and a NixOS one renders
/// `/etc/xdg/trollshell/plugins.json`, so the `config_dirs` leg is the only
/// one a NixOS user ever hits (#1260 review F8 — it had no test).
///
/// Rebuilt here over [`hytte_config::xdg::Env`] rather than imported —
/// `trollshell` is a binary crate, not a library another crate can link,
/// which is the #640 argument for `hytte-config` existing as a GTK-free leaf
/// in the first place. Taking the `Env` as a parameter rather than reading
/// the process environment inside is what lets a test drive both legs from a
/// `tempfile` dir without touching the real XDG config (the project's own
/// `tests-must-not-touch-real-xdg` rule).
fn plugins_json_candidates(env: &hytte_config::xdg::Env) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = env.config_home() {
        candidates.push(home.join("trollshell").join("plugins.json"));
    }
    candidates.extend(
        env.config_dirs()
            .into_iter()
            .map(|dir| dir.join("trollshell").join("plugins.json")),
    );
    candidates
}

/// [`plugins_json_candidates`], resolved through `cache` at most once — the
/// production half of [`PluginsState::search_path`] (#1270).
///
/// `env.config_home()`/`env.config_dirs()` `tracing::warn!` on a relative
/// `$XDG_CONFIG_HOME`/`$XDG_CONFIG_DIRS` (`hytte_config::xdg::is_absolute`,
/// #985); calling [`plugins_json_candidates`] fresh on every
/// [`refresh_plugins`] tick re-fired that warning every tick, forever, on a
/// misconfigured box (#1260 review, inherited nit 2). `env` is only consulted
/// the first time `cache` is empty — an `OnceCell` on the GTK main thread
/// needs no synchronisation, and the four variables it reads cannot change
/// for a process already running, so "resolve once" costs nothing a later
/// tick would have wanted.
///
/// Taking `cache` as a parameter (rather than reading
/// [`PluginsState::search_path`] directly) is what lets a test drive this
/// with its own throwaway `OnceCell` and a hand-built [`Env`](hytte_config::xdg::Env)
/// — including a relative one — without touching the real process
/// environment (`tests-must-not-touch-real-xdg`).
fn resolved_search_path<'a>(
    cache: &'a OnceCell<Vec<PathBuf>>,
    env: &hytte_config::xdg::Env,
) -> &'a [PathBuf] {
    cache.get_or_init(|| plugins_json_candidates(env))
}

/// The `plugins.json` the search path settled on, plus the cheap identity
/// [`DeclaredMounts`] keys its cached parse on (#1260 review F7).
///
/// Everything here comes from one `stat` plus a `realpath`-style walk (a
/// handful of `readlink`s — one per path component per symlink hop crossed)
/// per candidate — no read, no parse — which is the whole point: the tab's
/// 2 s poll runs this on the GTK main thread and must not do the blocking
/// read it used to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PluginsJson {
    /// The file the search found, i.e. the first candidate that exists.
    path: PathBuf,
    /// `canonicalize(path)` (`realpath(3)`, resolving **every** hop), or,
    /// on the rare path `realpath(3)` refuses, `None`.
    ///
    /// This is the discriminator that matters in practice, but only if it
    /// resolves the whole chain: both platform modules render `plugins.json`
    /// as a symlink into the nix store, and every store path carries the
    /// content hash in its own name and the frozen mtime `1970-01-01
    /// 00:00:01` — measured and documented in
    /// `hytte_config::subsystem::watch`'s module doc, which is why *that*
    /// poller stamps `(mtime, content hash)` rather than `(mtime, len)`. A
    /// hash would mean reading the file every tick, the cost this cache
    /// exists to avoid; the fully-resolved target carries the content hash
    /// in its own name, so it is exactly as sharp and costs one `realpath`.
    ///
    /// This was `read_link` — a single `readlink(2)`, one hop — through
    /// #1260, and that hop is where home-manager's
    /// `~/.config/… -> /nix/store/<hash>-home-manager-files/…` lands, but
    /// NixOS's `environment.etc` renders `/etc/xdg/… -> /etc/static/…`, a
    /// **rebuild-invariant** first hop with no hash at all (`/etc/static`
    /// itself is the second hop, `-> /nix/store/<hash>-etc/etc`). So on
    /// NixOS `read_link` returned the same answer across a content-changing
    /// rebuild and the whole stamp silently degenerated to `len` alone
    /// (#1270, #1260 review N1) — bounded by the nine wire names' pairwise-
    /// distinct lengths, so a single-plugin `mount` edit still moved `len`,
    /// but a length-preserving edit (two plugins' mounts swapped, say) did
    /// not. `canonicalize` resolves every hop instead of one, at a handful
    /// of syscalls instead of one — still nothing next to reading and
    /// parsing the file.
    link: Option<PathBuf>,
    /// `(mtime, len)` — [`hytte_config::subsystem::watch`]'s original stamp,
    /// and what actually discriminates a **hand-written** `plugins.json`
    /// (which has no link target and a real mtime).
    modified: Option<SystemTime>,
    len: u64,
}

/// Probe the search path: the first existing `plugins.json` and its stamp, or
/// `None` when no file exists anywhere on it.
///
/// A thin wrapper over [`probe_candidates`] for the tests below, which
/// exercise both halves — the environment-to-paths resolution and the
/// filesystem probe — together. [`refresh_plugins`] calls the two
/// separately, since only the first needs to be cached (#1270).
#[cfg(test)]
fn probe_plugins_json(env: &hytte_config::xdg::Env) -> Option<PluginsJson> {
    probe_candidates(&plugins_json_candidates(env))
}

/// The filesystem half of `probe_plugins_json`: stat every candidate path
/// in order and return the first that exists, with its stamp.
///
/// Split out of what used to be `probe_plugins_json` itself so
/// [`refresh_plugins`] can probe a **cached** candidate list
/// ([`PluginsState::search_path`]) without re-resolving it — and re-risking
/// the relative-`$XDG_*`-path warning `plugins_json_candidates` can trigger —
/// on every tick (#1270).
fn probe_candidates(candidates: &[PathBuf]) -> Option<PluginsJson> {
    candidates.iter().find_map(|path| {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        Some(PluginsJson {
            link: std::fs::canonicalize(path).ok(),
            modified: meta.modified().ok(),
            len: meta.len(),
            path: path.clone(),
        })
    })
}

/// The declared `HYTTE_PLUGIN_MOUNT` overrides in `plugins.json`, parsed **at
/// most once per change to the file** (#1260 review F7).
///
/// Before this, [`refresh_plugins`] did a full read + `serde_json` parse on
/// the GTK main thread on every 2 s tick, for the window's whole life and
/// regardless of which tab was showing — for a file that is a nix-rendered
/// store symlink and changes only on a rebuild. The cache keeps the parse and
/// leaves only the `probe_plugins_json` stat behind, which is the shape
/// `hytte_config::subsystem::watch` already polls config layers with.
///
/// Caching "there is no file" is deliberate too: that is the common case on a
/// box whose plugins were installed by hand, and it should not re-walk the
/// search path's every candidate any more often than a hit does.
#[derive(Default)]
struct DeclaredMounts {
    /// What the last `probe_plugins_json` found — see [`Seen`].
    seen: Seen,
    /// The last parse. Handed out by `Rc` so a poll's completion closure can
    /// hold it without re-cloning the map.
    map: Rc<HashMap<String, String>>,
}

/// The state [`DeclaredMounts`] compares tick to tick.
///
/// Three states, not `Option<Option<…>>`: [`Unprobed`](Self::Unprobed) — the
/// cache has never run, so the first tick must read — is a genuinely
/// different thing from [`Missing`](Self::Missing), "the search path has no
/// `plugins.json`", which is a cacheable answer like any other.
#[derive(Default, PartialEq, Eq)]
enum Seen {
    #[default]
    Unprobed,
    Missing,
    Found(PluginsJson),
}

impl DeclaredMounts {
    /// The cached map, re-reading through `read` **only** when `probe`
    /// differs from the one the cache last parsed.
    ///
    /// `read` is a parameter rather than a hard-wired call so a test can
    /// count how often the blocking half actually runs — which is the
    /// property this type exists for, and the one a stamp comparison that
    /// stopped working would break silently.
    fn get<F>(&mut self, probe: Option<PluginsJson>, read: F) -> Rc<HashMap<String, String>>
    where
        F: FnOnce(&Path) -> HashMap<String, String>,
    {
        let seen = probe.map_or(Seen::Missing, Seen::Found);
        if self.seen != seen {
            self.map = Rc::new(match &seen {
                Seen::Found(found) => read(&found.path),
                Seen::Unprobed | Seen::Missing => HashMap::new(),
            });
            self.seen = seen;
        }
        Rc::clone(&self.map)
    }
}

/// Read every plugin's declared `HYTTE_PLUGIN_MOUNT` out of the
/// `plugins.json` at `path` (#1161) — the same file `nix/hm-module.nix` /
/// `nix/nixos-module.nix` render.
///
/// Best-effort, the same failure mode [`hytte_config::places::load_places`]
/// gives this tab's Places sibling: an unreadable or unparsable file yields
/// an empty map rather than an error — the row simply shows no override note,
/// same as an unreachable shell showing no runtime overlay.
///
/// **Two id namespaces meet in the map this returns** (#1260 review F9, and
/// inherited from #423 rather than new here): its keys are `plugins.json`
/// attribute names — i.e. `programs.trollshell.plugins.<id>`, a nix option
/// name — while the ids they are looked up by in [`runtime_states`] come from
/// `ListPluginStates`, i.e. the **manifest** id the plugin registered with.
/// Nothing enforces that the two agree; they coincide by convention, and
/// `apply_plugins`' own `rt.get(id)` has zipped the same two namespaces since
/// #423. A plugin whose manifest id differs from its nix attribute name
/// simply shows no override note.
fn read_declared_mounts_at(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|text| declared_mounts_from_json(&text))
        .unwrap_or_default()
}

/// The pure half of [`read_declared_mounts_at`]: `{"plugins": {"<id>": {"env":
/// {"HYTTE_PLUGIN_MOUNT": "<name>"}}}}` → `{id: name}`, dropping any entry
/// that is missing the key or shaped unexpectedly rather than erroring — the
/// same tolerance the rest of this best-effort read has. Split out so a test
/// can drive it without touching the filesystem.
fn declared_mounts_from_json(text: &str) -> HashMap<String, String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return HashMap::new();
    };
    let Some(plugins) = value.get("plugins").and_then(serde_json::Value::as_object) else {
        return HashMap::new();
    };
    plugins
        .iter()
        .filter_map(|(id, spec)| {
            let mount = spec.get("env")?.get("HYTTE_PLUGIN_MOUNT")?.as_str()?;
            Some((id.clone(), mount.to_owned()))
        })
        .collect()
}

/// Apply a non-empty unit list + runtime overlay: update the existing rows in
/// place when the plugin set already matches (no flicker, no lost selection),
/// else rebuild them and restore the selection by id.
fn apply_plugins(
    state: &PluginsState,
    units: &[(String, String, bool)],
    rt: &HashMap<String, PluginRuntime>,
) {
    // Cache first: everything below, and every selection change until the next
    // poll, renders out of this.
    {
        let mut snap = state.snapshot.borrow_mut();
        snap.clear();
        for (id, active_state, enabled) in units {
            snap.insert(
                id.clone(),
                PluginSnapshot {
                    active_state: active_state.clone(),
                    enabled: *enabled,
                    rt: rt.get(id).cloned(),
                },
            );
        }
    }

    let known: Vec<String> = state.by_id.borrow().keys().cloned().collect();
    let listed: Vec<String> = units.iter().map(|(id, ..)| id.clone()).collect();
    let same_set = state.view.get() == PluginsView::List && same_plugin_set(&known, &listed);

    if same_set {
        for (id, active_state, enabled) in units {
            // Clone the row handle out and let the borrow end at this `let`:
            // `update_plugin_row` drives GTK setters, and any path from a
            // synchronous handler back into `by_id` while it is borrowed panics
            // with a `BorrowMutError` from inside a glib callback, which aborts
            // the process rather than failing gracefully (#643).
            let prow = state.by_id.borrow().get(id).cloned();
            if let Some(prow) = prow {
                update_plugin_row(&prow, active_state, *enabled, rt.get(id));
            }
        }
        refresh_detail(state);
        return;
    }

    // Structural change (or first load): rebuild the rows, then put the
    // selection back on the same plugin if it survived.
    //
    // `parked` is the selection an "unavailable" placeholder set aside (#943
    // review); `take` it either way, because once real rows are back it has
    // either been restored or been proven gone. A live `selected` wins over it
    // — that path never lost its page, so it has nothing to restore.
    let parked = state.parked.take();
    let previously = state.selected.borrow().clone();
    let (wanted, restore_push, restore_pending) = match (previously, parked) {
        (Some(id), _) => (Some(id), false, None),
        (None, Some(parked)) => {
            let ParkedSelection {
                id,
                pushed,
                pending,
            } = parked;
            // Belt-and-braces identity check (#945 review, finding 3): a
            // parked intent is always recorded for the same plugin it's
            // parked alongside (see `ParkedSelection`'s doc), but restoring
            // it under any other id would silently steer the wrong switch.
            let pending = pending.filter(|intent| intent.plugin_id == id);
            (Some(id), pushed, pending)
        }
        (None, None) => (None, false, None),
    };
    clear_rows(state);
    for (id, active_state, enabled) in units {
        let prow = build_plugin_row(id, active_state, *enabled, rt.get(id));
        state.list.append(&prow.row);
        state.rows.borrow_mut().push(prow.row.clone().upcast());
        // The semicolon rule: `insert` returns the displaced entry, and as a
        // bare statement that `Option<PluginRow>` is a temporary dropped
        // *before* the `RefMut` (temporaries drop in reverse creation order),
        // i.e. it would drop GTK widgets while `by_id` is borrowed. Binding it
        // moves the drop past the borrow. `clear_rows` ran just above, so the
        // displaced value is `None` today.
        let displaced = state.by_id.borrow_mut().insert(id.clone(), prow);
        drop(displaced);
    }
    state.view.set(PluginsView::List);

    match wanted {
        // The plugin survived: put the selection back exactly where it was,
        // silently, so a pushed detail page stays pushed.
        Some(id) if state.by_id.borrow().contains_key(&id) => {
            select_silently(state, &id);
            // Restoring across a placeholder: the page was popped when the
            // rows went away, so put it back — the user drilled in, and a
            // failed poll is not them navigating out.
            if restore_push {
                state.split.set_show_content(true);
            }
            // And the intent that was live when the placeholder parked the
            // selection, if any — with its original `since` untouched, so the
            // timeout keeps counting from the real toggle rather than from
            // this restore (#945 review, finding 3).
            if let Some(pending) = restore_pending {
                *state.pending.borrow_mut() = Some(pending);
            }
        }
        // Either the selected plugin is gone, or this is the first load.
        //
        // Pop first: the user asked to see *that* plugin, and leaving a pushed
        // page open — or silently swapping a different plugin in underneath it
        // — would both be lies. Then settle the sidebar on the first remaining
        // plugin, so a wide layout shows a detail pane rather than an empty one
        // beside a full list.
        //
        // That second half is entirely this code's doing. `GtkListBox` in
        // `Single` mode does **not** auto-select an appended row — that is
        // `Browse` mode — so without the explicit `select_silently` below a
        // rebuild would leave the list with nothing selected beside a full set
        // of rows. It is the *silent* variant because `apply_plugins` owns the
        // selection here: it sets `selected` itself and calls `refresh_detail`
        // once at the end, so letting the `row-selected` handler run would
        // re-enter that path mid-rebuild and, worse, make a poll look like the
        // user navigating.
        // The **Shell** entry is a selection too (#888 P1), and it survives
        // every membership change: it is not one of these units. Settling the
        // sidebar on the first plugin here would drag the operator off a page
        // they are editing, two seconds after a `systemctl --user` elsewhere
        // added a unit.
        _ if state.shell_selected.get() => {}
        _ => {
            state.split.set_show_content(false);
            if let Some((first, ..)) = units.first() {
                select_silently(state, first);
            }
        }
    }
    refresh_detail(state);
}

/// Whether the sidebar already holds exactly the freshly-listed plugin ids.
///
/// Set equality, order-insensitive: `known` comes from a map's keys so it is
/// duplicate-free, and `ListPlugins` yields one entry per unit, so comparing
/// lengths and testing membership one way is enough. Order-insensitive on
/// purpose — a reorder alone must not tear the rows down, because a teardown
/// costs the selection and, when collapsed, the pushed page. This predicate is
/// the entire difference between a poll the user never notices and a list that
/// flickers back to the top every two seconds.
fn same_plugin_set(known: &[String], listed: &[String]) -> bool {
    known.len() == listed.len() && listed.iter().all(|id| known.contains(id))
}

/// Select `id`'s row without running the user-driven selection path — used to
/// restore a selection across a rebuild, where the detail pane is refreshed by
/// the caller and the navigation state must not move.
fn select_silently(state: &PluginsState, id: &str) {
    let row = state.by_id.borrow().get(id).map(|prow| prow.row.clone());
    let Some(row) = row else {
        return;
    };
    *state.selected.borrow_mut() = Some(id.to_owned());
    state.selecting.set(true);
    state
        .list
        .select_row(Some(row.upcast_ref::<gtk::ListBoxRow>()));
    state.selecting.set(false);
}

/// Drop the selection entirely: no row selected, the empty state in the detail
/// pane, and — the part that matters when collapsed — pop back to the list.
///
/// This is the *no plugins at all* fallback (the placeholder views, and a
/// snapshot that has lost the selected id). When plugins remain,
/// [`apply_plugins`] settles the sidebar on one of them instead of blanking the
/// pane.
fn clear_selection(state: &PluginsState) {
    *state.selected.borrow_mut() = None;
    // Nothing is shown any more, so no plugin's intent is "for" the detail
    // pane — same reasoning as a plain plugin switch (#944).
    state.pending.borrow_mut().take();
    state.selecting.set(true);
    state.list.select_row(None::<&gtk::ListBoxRow>);
    state.selecting.set(false);
    // The **Shell** entry is not a plugin, and losing every plugin — or the
    // shell going unreachable, which is the case this runs in most often — is
    // no reason to navigate away from a page that edits files and works with
    // the shell down (#888 P1). `show_empty_detail` keeps it up for the same
    // reason; only the plugin half of the pane is cleared here.
    if state.shell_selected.get() {
        return;
    }
    state.split.set_show_content(false);
    show_empty_detail(state);
}

/// Decide what [`refresh_detail`] should show on the switch for the shown
/// plugin `id`, resolving `pending` along the way (#944).
///
/// Pure with respect to the widget: this only ever reads/clears `pending` and
/// returns the boolean to display; `refresh_detail` is the one place that
/// actually drives `switch.set_active`. Behaviour:
///
/// * No pending intent (or one that belongs to a *different* plugin than
///   `id` — i.e. the shown plugin changed): show the truth (`running`), and
///   drop a stale intent for another plugin outright.
/// * A pending intent for `id` that the poll now agrees with, or that has
///   outlived [`PENDING_TOGGLE_TIMEOUT`]: clear it and show the truth — this
///   is the "truth wins" path, whether by confirmation or by timeout.
/// * A pending intent for `id` that the poll still contradicts, within the
///   timeout: keep it pending and show what the user asked for instead of the
///   stale snapshot.
fn resolve_pending(pending: &RefCell<Option<PendingToggle>>, id: &str, running: bool) -> bool {
    // Bound rather than matched on directly: a `RefMut` created in a match's
    // scrutinee lives for the whole match (all arms), and the `else` arm
    // below needs its own `borrow_mut()` — the same "semicolon rule" this
    // file's other `RefCell` juggling already documents (`clear_rows`,
    // `apply_plugins`).
    let taken = pending.borrow_mut().take();
    match taken {
        Some(intent) if intent.plugin_id == id => {
            if running == intent.wanted || intent.since.elapsed() >= PENDING_TOGGLE_TIMEOUT {
                running
            } else {
                let wanted = intent.wanted;
                *pending.borrow_mut() = Some(intent);
                wanted
            }
        }
        // Either nothing was pending, or it was pending for a plugin that
        // isn't shown any more — already taken above either way, so there is
        // nothing left to put back.
        _ => running,
    }
}

/// Show the detail pane's empty state and reset its title — unless the
/// pinned **Shell** entry is what the pane is showing, in which case there is
/// nothing empty about it (#888 P1).
///
/// The guard lives here rather than at each of the three call sites
/// ([`clear_selection`], [`refresh_detail`]'s no-selection arm, and
/// [`build_tab`]'s initial state) so a fourth cannot forget it.
fn show_empty_detail(state: &PluginsState) {
    if state.shell_selected.get() {
        show_shell_detail(state);
        return;
    }
    state.detail.stack.set_visible_child_name("empty");
    state.detail.page.set_title("Plugin");
}

/// Retarget the one detail pane at the selected plugin, from the last poll's
/// snapshot. A selection whose plugin has vanished falls back to
/// [`clear_selection`] rather than showing stale rows.
fn refresh_detail(state: &PluginsState) {
    let selected = state.selected.borrow().clone();
    let Some(id) = selected else {
        // Nothing shown ⇒ no plugin's intent is "for" the detail pane — the
        // same rule `clear_selection` applies (#944), reached here too
        // because deselecting (a ctrl-click in `Single` mode) drives
        // `connect_row_selected(None)` straight into this arm without going
        // through `clear_selection` (#945 review, finding 2). Left unhandled,
        // the intent would silently steer whichever plugin gets selected
        // next.
        state.pending.borrow_mut().take();
        show_empty_detail(state);
        return;
    };
    let snap = state.snapshot.borrow().get(&id).cloned();
    let Some(snap) = snap else {
        clear_selection(state);
        return;
    };

    state.detail.page.set_title(&id);
    state.detail.stack.set_visible_child_name("plugin");
    refresh_config(state, &id);

    state
        .detail
        .unit_row
        .set_subtitle(&plugin_subtitle(&snap.active_state, snap.enabled));

    let (icon, css, status) = runtime_overlay(&snap.active_state, snap.rt.as_ref());
    let connection = if status.is_empty() {
        "Not connected".to_owned()
    } else {
        status
    };
    state.detail.conn_row.set_subtitle(&connection);
    apply_badge(&state.detail.conn_badge, icon, css, &connection);

    // #944: while a user toggle is pending for *this* plugin and the poll
    // hasn't caught up (or timed out), show what the user asked for instead
    // of bouncing back to the stale `ActiveState`. Removing this line and
    // using `is_running(&snap.active_state)` directly is the mutation that
    // must fail `a_pending_toggle_holds_the_switch_against_a_stale_poll`.
    let show_running = resolve_pending(&state.pending, &id, is_running(&snap.active_state));

    // Under `syncing`, so reflecting the resolved state doesn't fire a
    // Start/Stop back at the shell.
    state.syncing.set(true);
    state.detail.switch.set_active(show_running);
    state.syncing.set(false);
}

/// Mount (or leave alone, or tear down) the selected plugin's *Configuration*
/// group — the schema-derived form for the config family its **binary** owns
/// (#888 P1).
///
/// Keyed by plugin id and rebuilt only when the selection moves, so the 2 s
/// poll — which calls [`refresh_detail`] on every tick — costs nothing here,
/// and a form the operator is typing into is not rebuilt underneath them.
fn refresh_config(state: &PluginsState, id: &str) {
    let mounted_for = { state.detail.config.borrow().as_ref().map(|m| m.plugin.clone()) };
    if mounted_for.as_deref() == Some(id) {
        return;
    }
    // Take the old one out of the cell *before* removing its groups: a
    // `PreferencesPage::remove` drives GTK, which can emit synchronously into
    // a handler that re-enters this cell, and a `BorrowMutError` inside a glib
    // callback aborts the process (#643).
    let previous = state.detail.config.take();
    if let Some(previous) = previous {
        for group in previous.form.groups() {
            state.detail.plugin_page.remove(group);
        }
        // Explicit, and load-bearing: dropping the handle is what stops the
        // form's re-read poll.
        drop(previous);
    }

    let Some(ops) = family_for_plugin(state, id) else {
        return;
    };
    let form = crate::config_form::build(ops, &state.env);
    for group in form.groups() {
        state.detail.plugin_page.add(group);
    }
    let displaced = state.detail.config.replace(Some(MountedForm {
        plugin: id.to_owned(),
        form,
    }));
    drop(displaced);
}

/// Which config family the plugin listed as `id` owns, if any.
///
/// From the **binary** its unit runs, not from its id: `stats` and `stats-bar`
/// are two launches of one `hytte-plugin-stats` reading one `stats.toml`
/// (`docs/plugin-env.md`), so a rule over ids would either miss the second or
/// guess. `plugins.json` carries each entry's `exec`, and
/// [`manifest_id_of_exec`] is `nix/module-common.nix`'s own `inferManifestId`
/// — the function that decides what the plugin calls itself in the first
/// place.
///
/// Falls back to the id when there is no `plugins.json` entry (a
/// hand-installed static unit, the legacy launch path `plugin_launcher.rs`
/// still supports), which is right for the conventional case and reaches no
/// family at all otherwise. Read on a **selection change**, not on the poll —
/// see [`refresh_config`].
fn family_for_plugin(state: &PluginsState, id: &str) -> Option<crate::config_form::FamilyOps> {
    let candidates = resolved_search_path(&state.search_path, &state.env);
    let declared = probe_candidates(candidates)
        .and_then(|found| manifest_id_at(&found.path, id));
    crate::config_form::family(declared.as_deref().unwrap_or(id))
}

/// The manifest id `plugins.json` at `path` implies for the plugin `id`.
fn manifest_id_at(path: &Path, id: &str) -> Option<String> {
    manifest_id_from_json(&std::fs::read_to_string(path).ok()?, id)
}

/// [`manifest_id_at`]'s pure half: `{"plugins": {"<id>": {"exec": "…"}}}` →
/// the manifest id of that binary. Split out so a test can drive it without
/// touching the filesystem, exactly as [`declared_mounts_from_json`] is.
fn manifest_id_from_json(text: &str, id: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let exec = value.get("plugins")?.get(id)?.get("exec")?.as_str()?;
    Some(manifest_id_of_exec(exec))
}

/// `…/bin/hytte-plugin-stats` → `stats`; `…/bin/hytte-claude-bridge` →
/// `claude-bridge`.
///
/// A transcription of `nix/module-common.nix`'s `inferManifestId`, which is
/// what both platform modules use to decide whether an entry needs an explicit
/// `HYTTE_PLUGIN_ID` — so this agrees with the launcher by construction rather
/// than by coincidence.
fn manifest_id_of_exec(exec: &str) -> String {
    let binary = exec.rsplit('/').next().unwrap_or(exec);
    binary
        .strip_prefix("hytte-plugin-")
        .or_else(|| binary.strip_prefix("hytte-"))
        .unwrap_or(binary)
        .to_owned()
}

/// Remove every currently-added child (plugin rows or a placeholder) from the
/// list and forget the keyed rows, before a rebuild.
fn clear_rows(state: &PluginsState) {
    // Removing the selected row emits `row-selected(None)`; that is bookkeeping,
    // not the user deselecting, so it must not run the selection path.
    state.selecting.set(true);
    // `take()`, not `borrow_mut().drain(..)`: the chained `RefMut` would stay
    // live across every `list.remove()`, which can emit synchronously into a
    // handler that re-enters these cells — a `BorrowMutError` inside a glib
    // callback aborts the process (#643).
    for row in state.rows.take() {
        state.list.remove(&row);
    }
    // Same reason for `by_id`: `clear()` drops each `PluginRow`'s widgets
    // *inside* the borrow, whereas `take()`'s borrow is over before the
    // returned map (and so its widgets) drops.
    drop(state.by_id.take());
    state.selecting.set(false);
}

/// Show a single informational/placeholder row (no plugins, or shell
/// unavailable), rebuilding only on a *transition* into `view` so a steady poll
/// doesn't flicker it. There is nothing to drill into, so the row is neither
/// activatable nor selectable and the detail pane drops to its empty state.
///
/// Entering [`PluginsView::Unavailable`] *parks* the selection first (see
/// [`ParkedSelection`]): an unreachable shell says nothing about which plugins
/// exist, so the id is kept for [`apply_plugins`] to restore. Entering
/// [`PluginsView::Empty`] is the opposite — the shell answered, and it answered
/// "no plugins" — so any parked selection is dropped there.
fn set_placeholder(state: &PluginsState, view: PluginsView, title: &str, subtitle: &str) {
    if state.view.get() == view {
        return;
    }
    // Before `clear_selection` below wipes both of them. A repeated failure
    // re-enters with the same `view` and returns above, so the first
    // failure's park is never overwritten with the `None` it left behind.
    let park = if view == PluginsView::Unavailable {
        let selected = state.selected.borrow().clone();
        selected.map(|id| ParkedSelection {
            pushed: state.split.shows_content(),
            // Take, not clone: an intent is "for" exactly one home at a time
            // (the live pane, or the park), same as `clear_selection`'s own
            // `take()`. `clear_selection` below sees `None` and is a no-op on
            // `pending`.
            pending: state.pending.borrow_mut().take(),
            id,
        })
    } else {
        None
    };
    *state.parked.borrow_mut() = park;
    clear_rows(state);
    state.snapshot.borrow_mut().clear();
    clear_selection(state);
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .activatable(false)
        .selectable(false)
        .build();
    state.list.append(&row);
    state.rows.borrow_mut().push(row.upcast());
    state.view.set(view);
}

/// Build one plugin's sidebar row: the id, [`plugin_subtitle`]'s unit line, the
/// prefix runtime badge (#423) and the status column (#887).
///
/// No per-row handler — drill-down is the list's `row-selected` /
/// `row-activated`, so a row is a display of one plugin and nothing else. The
/// on/off control it used to carry now lives on the detail page, which is also
/// what frees the row to be activatable in the first place: a `AdwSwitchRow`
/// spends its activation toggling its own switch.
fn build_plugin_row(
    id: &str,
    active_state: &str,
    enabled: bool,
    rt: Option<&PluginRuntime>,
) -> PluginRow {
    let row = adw::ActionRow::builder()
        .title(id)
        .activatable(true)
        .build();

    let badge = gtk::Image::new();
    badge.set_valign(gtk::Align::Center);
    row.add_prefix(&badge);

    // The status column. `dim-label` + `caption` only — no new colours; the
    // badge beside it is where colour lives, and it is the same three classes
    // #423 already used.
    let status = gtk::Label::builder()
        .valign(gtk::Align::Center)
        .xalign(1.0)
        .build();
    status.add_css_class("dim-label");
    status.add_css_class("caption");
    row.add_suffix(&status);

    let prow = PluginRow { row, badge, status };
    update_plugin_row(&prow, active_state, enabled, rt);
    prow
}

/// Reflect a unit's state + runtime overlay into an existing sidebar row: the
/// subtitle, the prefix badge and the status column.
fn update_plugin_row(
    prow: &PluginRow,
    active_state: &str,
    enabled: bool,
    rt: Option<&PluginRuntime>,
) {
    let (icon, css, status) = runtime_overlay(active_state, rt);
    prow.row
        .set_subtitle(&plugin_subtitle(active_state, enabled));
    // The badge's tooltip carries the long runtime line the row no longer
    // spells out; the detail page shows it in full.
    apply_badge(&prow.badge, icon, css, &status);
    prow.status.set_text(status_cell(active_state, rt));
}

/// Set (or hide) a prefix runtime badge: a recolored symbolic icon whose
/// tooltip echoes the runtime status line. An empty `icon` hides it (an inactive
/// unit with no connection has nothing to overlay).
fn apply_badge(badge: &gtk::Image, icon: &str, css: &str, tooltip: &str) {
    for class in ["success", "accent", "warning"] {
        badge.remove_css_class(class);
    }
    if icon.is_empty() {
        badge.set_visible(false);
        return;
    }
    badge.set_icon_name(Some(icon));
    if !css.is_empty() {
        badge.add_css_class(css);
    }
    badge.set_tooltip_text(Some(tooltip));
    badge.set_visible(true);
}

/// Re-read now and again after systemd settles the transition, so a just-toggled
/// plugin catches up without waiting for the next poll tick.
fn refresh_plugins_soon(state: &PluginsState) {
    refresh_plugins(state);
    let state = state.clone();
    glib::timeout_add_local_once(Duration::from_millis(1200), move || {
        refresh_plugins(&state);
    });
}

/// The sidebar's status column (#887): one word for what this plugin is doing,
/// at a glance, down the right-hand edge of the list.
///
/// It answers a different question from [`plugin_subtitle`], which reports the
/// *unit* (systemd's `ActiveState` plus whether it starts at login). This
/// reports the *plugin*: whether the host has a live connection to it and
/// whether that connection is drawing. A running unit whose plugin never dialed
/// the socket is the case the two disagree on, and the one worth spotting from
/// the list — hence "Not connected" rather than a second "Running".
///
/// A unit that is still *coming up* (`activating` / `reloading`) is not that
/// case: it has not had a chance to dial the socket yet, so reporting it with
/// the same word as a plugin that crashed after start would flag a
/// disagreement that does not exist. It gets "Starting…" instead — the same
/// word [`plugin_subtitle`] uses for the unit, because here the two genuinely
/// do agree.
///
/// Deliberately `&'static str`: a column is only a column if the values are
/// short and drawn from a closed set. The full sentence — mount region, dropped
/// effects, how long since the last frame — is the detail page's job.
fn status_cell(active_state: &str, rt: Option<&PluginRuntime>) -> &'static str {
    match rt {
        Some(rt) if rt.rendering => "Rendering",
        Some(_) => "Connected",
        None => match active_state {
            // Still coming up — no connection yet is expected, not a
            // disagreement. Before the `is_running` arm, which would otherwise
            // swallow both of these.
            "activating" | "reloading" => "Starting…",
            _ if is_running(active_state) => "Not connected",
            "failed" => "Failed",
            "deactivating" => "Stopping…",
            "inactive" => "Stopped",
            // A state systemd grew after this was written: say so rather than
            // claim one of the five above.
            _ => "Unknown",
        },
    }
}

/// Whether a systemd `ActiveState` means the plugin is currently running — the
/// switch's on-state. `activating` / `reloading` count as on (it's coming up).
fn is_running(active_state: &str) -> bool {
    matches!(active_state, "active" | "activating" | "reloading")
}

/// The base row subtitle: a human status from the unit's `ActiveState` plus its
/// persisted enabled/disabled state. The runtime overlay (#423) appends to this.
fn plugin_subtitle(active_state: &str, enabled: bool) -> String {
    let status = match active_state {
        "active" => "Running",
        "activating" | "reloading" => "Starting…",
        "deactivating" => "Stopping…",
        "failed" => "Failed",
        "inactive" => "Stopped",
        other => other,
    };
    let persist = if enabled { "enabled" } else { "disabled" };
    format!("{status} · {persist}")
}

/// The connected/rendering overlay for a plugin row (#423), merging the systemd
/// `active_state` with the host's live runtime state (`None` = no live host
/// connection). Returns `(icon, css_class, status)`: `icon` is a symbolic name
/// for the prefix badge (`""` hides it), `css_class` recolors it
/// (`"success"`/`"accent"`/`"warning"`/`""`), and `status` is the human-readable
/// runtime line appended to the subtitle. Pure → unit-tested.
fn runtime_overlay(
    active_state: &str,
    rt: Option<&PluginRuntime>,
) -> (&'static str, &'static str, String) {
    match rt {
        // Connected and drawing: the healthy case.
        Some(rt) if rt.rendering => (
            "emblem-ok-symbolic",
            "success",
            format!(
                "Connected · rendering in {}{}{}",
                mount_display(&rt.mount, rt.declared_mount.as_deref()),
                violations_suffix(rt.violations),
                seen_suffix(rt.last_seen_secs),
            ),
        ),
        // Connected but hasn't rendered yet (coming up, or a silent plugin).
        Some(rt) => (
            "content-loading-symbolic",
            "accent",
            format!(
                "Connected · not yet rendering{}{}",
                violations_suffix(rt.violations),
                seen_suffix(rt.last_seen_secs),
            ),
        ),
        // Unit is active but no host connection — crashed after start / never
        // registered. This is the diagnostic case the unit list can't show.
        None if is_running(active_state) => (
            "dialog-warning-symbolic",
            "warning",
            "Active but not connected".to_owned(),
        ),
        // Inactive and unconnected: nothing to overlay.
        None => ("", "", String::new()),
    }
}

/// A human label for one of the nine wire names
/// `hytte_plugin_proto::manifest::Mount::wire_name` carries, or a stand-in
/// when the host didn't report one. An unrecognised, non-empty wire name (a
/// future tenth mount this build predates) falls back to the wire name
/// itself — never "unknown" — the same forward-compat call the wire's own
/// `Mount::from_wire_name` makes: showing *something* beats hiding a value
/// that genuinely exists (#1161).
fn mount_or_unknown(mount: &str) -> &str {
    match mount {
        "" => "an unknown region",
        "SidebarLead" => "Sidebar (top)",
        "SidebarTop" => "Sidebar (middle)",
        "SidebarBottom" => "Sidebar (bottom)",
        "SidebarRightLead" => "Sidebar, right (top)",
        "SidebarRightTop" => "Sidebar, right (middle)",
        "SidebarRightBottom" => "Sidebar, right (bottom)",
        "BarLeft" => "Bar (left)",
        "BarCenter" => "Bar (center)",
        "BarRight" => "Bar (right)",
        other => other,
    }
}

/// The mount line for display (#1161). Pure → unit-tested.
///
/// `mount` is the **effective**, host-registered mount and is always what
/// goes first, because the line it feeds reads `"rendering in …"` and that
/// is a claim about where the card actually is. `declared` is what
/// `plugins.json` asked for, and it only ever *annotates*:
///
/// | `declared`         | line                                          |
/// |--------------------|-----------------------------------------------|
/// | `None`             | `Sidebar (middle)`                            |
/// | `Some(== mount)`   | `Sidebar, right (middle) · set by nix`        |
/// | `Some(!= mount)`   | `Sidebar (middle) · nix asked for Sidebar, right (middle) (not applied)` |
///
/// The middle row is the case #1161 asked to make visible ("so an operator
/// can see an override is in force") and it is also the **common** one: the
/// SDK applies `HYTTE_PLUGIN_MOUNT` before `Register` (`hytte_plugin::run`),
/// so an override that works makes the two values equal. Keying the note off
/// the *disagreement* instead — what this function did until #1260's review
/// F1 — therefore said nothing at all whenever the feature worked, and spoke
/// up only for a plugin binary that ignores the variable.
///
/// The bottom row is that failure case, and the reason the effective mount
/// leads rather than the declared one (#1260 review F2): the declared value
/// is precisely the one that did **not** take, so putting it in the
/// "rendering in" slot asserted the card was somewhere it is not.
fn mount_display(mount: &str, declared: Option<&str>) -> String {
    let effective = mount_or_unknown(mount);
    match declared {
        // The override was declared and did not take: say where it *is*, first.
        Some(declared) if declared != mount => {
            format!(
                "{effective} · nix asked for {} (not applied)",
                mount_or_unknown(declared)
            )
        }
        // The override took — this is the case #1161 wants visible.
        Some(_) => format!("{effective} · set by nix"),
        None => effective.to_owned(),
    }
}

/// A " · N dropped" suffix when the plugin has tripped the containment guards
/// (#435/#436), else empty.
fn violations_suffix(violations: u32) -> String {
    if violations == 0 {
        String::new()
    } else {
        format!(" · {violations} dropped")
    }
}

/// A " · seen …" suffix for a nontrivial gap since the host last saw a frame,
/// humanized to s/m/h; empty for a fresh (<5s) plugin so an actively-drawing
/// card's subtitle stays tidy.
fn seen_suffix(secs: u64) -> String {
    match secs {
        0..=4 => String::new(),
        5..=59 => format!(" · seen {secs}s ago"),
        60..=3599 => format!(" · seen {}m ago", secs / 60),
        _ => format!(" · seen {}h ago", secs / 3600),
    }
}

// ── Plugins tab Control calls (#348) ─────────────────────────────────────────

/// `ListPlugins`' reply: `(id, active_state, enabled)` per plugin user unit.
type PollUnits = Vec<(String, String, bool)>;

/// `ListPluginStates`' reply (#423): `(id, rendering, mount, last_seen_secs,
/// violations)` per plugin with a live host connection.
type PollStates = Vec<(String, bool, String, u64, u32)>;

/// What one [`list_plugins_and_states`] round trip hands
/// [`on_poll_result_with_declared`].
type PollResult = Result<(PollUnits, PollStates), hytte_bus::BusError>;

/// `ListPlugins` → `[(id, active_state, enabled)]` for each plugin user unit.
async fn list_plugins() -> Result<PollUnits, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListPlugins")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<(String, String, bool)>>()
        .await
}

/// `ListPluginStates` → `[(id, rendering, mount, last_seen_secs, violations)]`
/// for each plugin with a live host connection (#423). The runtime overlay the
/// Plugins tab draws on top of the unit list.
async fn list_plugin_states() -> Result<PollStates, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListPluginStates")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<(String, bool, String, u64, u32)>>()
        .await
}

/// Fetch the unit list and the runtime overlay for the Plugins tab in one shot
/// (#423). The overlay is **best-effort**: a `ListPluginStates` error (e.g. an
/// older shell that predates it) degrades to no overlay rather than blanking the
/// unit list, so the tab still works against a shell without the method.
async fn list_plugins_and_states() -> PollResult {
    let units = list_plugins().await?;
    let states = list_plugin_states().await.unwrap_or_default();
    Ok((units, states))
}

/// Apply an on/off toggle for plugin `id`: `on` → start + enable, `off` → stop +
/// disable, so the change both takes effect now and persists across logins. Two
/// `Control` calls; the first error short-circuits.
async fn set_plugin_state(id: String, on: bool) -> Result<(), hytte_bus::BusError> {
    let start_stop = if on { "StartPlugin" } else { "StopPlugin" };
    plugin_id_call(start_stop, &id).await?;
    set_plugin_enabled(&id, on).await
}

/// One `StartPlugin`/`StopPlugin` call carrying a plugin id, returning `()`. A
/// slightly longer timeout — the shell drives a systemd job to apply it.
async fn plugin_id_call(method: &str, id: &str) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method(method)
        .args((id.to_owned(),))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// `SetPluginEnabled(id, enabled)`: persist the plugin's auto-start state.
async fn set_plugin_enabled(id: &str, enabled: bool) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetPluginEnabled")
        .args((id.to_owned(), enabled))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use std::cell::OnceCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    use super::{
        DeclaredMounts, PluginRuntime, PluginsJson, PollGenerations, PollStates,
        declared_mounts_from_json, is_running, mount_display, mount_or_unknown, plugin_subtitle,
        probe_candidates, probe_plugins_json, read_declared_mounts_at, resolved_search_path,
        runtime_overlay, runtime_states, same_plugin_set, seen_suffix, status_cell,
        violations_suffix,
    };

    // ── Poll ordering (#983) ────────────────────────────────────────────────
    //
    // The widget-level consequences live in `gtk_tests`; these pin the
    // ordering rule itself, hermetically (no display, no bus).

    /// The defect's shape in one assertion: two polls are outstanding, the
    /// newer one completes first, and the older one is then refused.
    #[test]
    fn a_generation_older_than_the_newest_applied_is_refused() {
        let polls = PollGenerations::default();
        let slow = polls.issue();
        let fresh = polls.issue();
        assert!(fresh > slow, "generations must be monotonic");
        assert!(polls.accept(fresh), "the newest result applies");
        assert!(!polls.accept(slow), "an older result is dropped");
    }

    /// The common case, and the mutation an ordering test alone would miss: a
    /// gate that refused everything would also "fix" the defect. Every
    /// in-order completion must apply.
    #[test]
    fn every_in_order_completion_is_accepted() {
        let polls = PollGenerations::default();
        for _ in 0..5 {
            let generation = polls.issue();
            assert!(
                polls.accept(generation),
                "a poll that completes before the next one is issued must always apply"
            );
        }
    }

    /// The first completion of a fresh tab must apply: `applied` starts at
    /// `0`, below every issued generation.
    #[test]
    fn the_first_poll_of_a_fresh_tab_is_accepted() {
        let polls = PollGenerations::default();
        let first = polls.issue();
        assert!(first > 0, "a generation must be above the applied floor");
        assert!(polls.accept(first));
    }

    /// Strictly greater, not "greater or equal": re-running one completion is
    /// not something `spawn_on_runtime` does, and treating it as fresh would
    /// let a duplicated stale delivery through.
    #[test]
    fn the_newest_generation_is_not_accepted_twice() {
        let polls = PollGenerations::default();
        let only = polls.issue();
        assert!(polls.accept(only));
        assert!(!polls.accept(only), "the same generation must apply once");
    }

    /// A connected plugin's runtime state, for the overlay tests. No declared
    /// override — see [`rt_with_declared`] for that half.
    fn rt(rendering: bool, mount: &str, last_seen_secs: u64, violations: u32) -> PluginRuntime {
        PluginRuntime {
            rendering,
            mount: mount.to_owned(),
            declared_mount: None,
            last_seen_secs,
            violations,
        }
    }

    /// `["a", "b"]` as the owned ids the diff works on.
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    /// [`rt`] plus a declared-mount override, for the #1161 override tests.
    fn rt_with_declared(
        rendering: bool,
        mount: &str,
        declared: &str,
        last_seen_secs: u64,
        violations: u32,
    ) -> PluginRuntime {
        PluginRuntime {
            declared_mount: Some(declared.to_owned()),
            ..rt(rendering, mount, last_seen_secs, violations)
        }
    }

    #[test]
    fn running_states_map_the_switch() {
        assert!(is_running("active"));
        assert!(is_running("activating"));
        assert!(is_running("reloading"));
        assert!(!is_running("inactive"));
        assert!(!is_running("failed"));
        assert!(!is_running("deactivating"));
    }

    #[test]
    fn subtitle_combines_status_and_persistence() {
        assert_eq!(plugin_subtitle("active", true), "Running · enabled");
        assert_eq!(plugin_subtitle("failed", false), "Failed · disabled");
        assert_eq!(plugin_subtitle("inactive", true), "Stopped · enabled");
        assert_eq!(plugin_subtitle("activating", false), "Starting… · disabled");
    }

    #[test]
    fn subtitle_passes_through_unknown_active_state() {
        assert_eq!(
            plugin_subtitle("maintenance", true),
            "maintenance · enabled"
        );
    }

    // ── Runtime overlay (#423) ───────────────────────────────────────────────

    #[test]
    fn overlay_reports_connected_and_rendering() {
        let (icon, css, status) = runtime_overlay("active", Some(&rt(true, "SidebarTop", 1, 0)));
        assert_eq!(icon, "emblem-ok-symbolic");
        assert_eq!(css, "success");
        assert_eq!(status, "Connected · rendering in Sidebar (middle)");
    }

    /// The case #1161 asked for, in the surface the note actually reaches:
    /// an override that **took**. The host reports the overridden mount (the
    /// SDK applied `HYTTE_PLUGIN_MOUNT` before `Register`), and the row says
    /// so — "an operator can see an override is in force".
    ///
    /// Red before #1260's review F1: the old `mount_display` keyed its note
    /// off `declared != mount`, so this — the working, common case — showed
    /// nothing at all.
    #[test]
    fn overlay_says_an_override_that_took_is_in_force() {
        let (_, _, status) = runtime_overlay(
            "active",
            Some(&rt_with_declared(
                true,
                "SidebarRightTop",
                "SidebarRightTop",
                1,
                0,
            )),
        );
        assert_eq!(
            status,
            "Connected · rendering in Sidebar, right (middle) · set by nix"
        );
    }

    /// The failure case: a declared mount the plugin ignored. The row must
    /// keep the **effective** mount in the "rendering in" slot — it is the
    /// place the card actually is — and name the declared one as the thing
    /// that did not take (#1260 review F2; the old wording put the
    /// non-effective value first and so asserted the card was on the other
    /// sidebar).
    #[test]
    fn overlay_notes_a_declared_mount_that_did_not_take() {
        let (_, _, status) = runtime_overlay(
            "active",
            Some(&rt_with_declared(
                true,
                "SidebarTop",
                "SidebarRightTop",
                1,
                0,
            )),
        );
        assert_eq!(
            status,
            "Connected · rendering in Sidebar (middle) \
             · nix asked for Sidebar, right (middle) (not applied)"
        );
    }

    #[test]
    fn overlay_reports_connected_not_yet_rendering() {
        let (icon, css, status) = runtime_overlay("activating", Some(&rt(false, "", 0, 0)));
        assert_eq!(icon, "content-loading-symbolic");
        assert_eq!(css, "accent");
        assert_eq!(status, "Connected · not yet rendering");
    }

    #[test]
    fn overlay_flags_active_but_not_connected() {
        // The diagnostic case: the unit is up, but nothing dialed the socket.
        let (icon, css, status) = runtime_overlay("active", None);
        assert_eq!(icon, "dialog-warning-symbolic");
        assert_eq!(css, "warning");
        assert_eq!(status, "Active but not connected");
    }

    #[test]
    fn overlay_blank_for_inactive_and_unconnected() {
        let (icon, css, status) = runtime_overlay("inactive", None);
        assert_eq!(icon, "");
        assert_eq!(css, "");
        assert!(status.is_empty());
    }

    #[test]
    fn overlay_surfaces_violations_and_age() {
        let (_, _, status) = runtime_overlay("active", Some(&rt(true, "BarCenter", 90, 3)));
        assert_eq!(
            status,
            "Connected · rendering in Bar (center) · 3 dropped · seen 1m ago"
        );
    }

    /// One assertion per wire name (#1161) — every entry in
    /// `hytte_plugin_proto::manifest::Mount::ALL` must have a human label
    /// here, and an unrecognised (future) name must still show *something*
    /// rather than "unknown".
    #[test]
    fn mount_or_unknown_covers_the_nine_wire_names() {
        assert_eq!(mount_or_unknown("SidebarLead"), "Sidebar (top)");
        assert_eq!(mount_or_unknown("SidebarTop"), "Sidebar (middle)");
        assert_eq!(mount_or_unknown("SidebarBottom"), "Sidebar (bottom)");
        assert_eq!(mount_or_unknown("SidebarRightLead"), "Sidebar, right (top)");
        assert_eq!(
            mount_or_unknown("SidebarRightTop"),
            "Sidebar, right (middle)"
        );
        assert_eq!(
            mount_or_unknown("SidebarRightBottom"),
            "Sidebar, right (bottom)"
        );
        assert_eq!(mount_or_unknown("BarLeft"), "Bar (left)");
        assert_eq!(mount_or_unknown("BarCenter"), "Bar (center)");
        assert_eq!(mount_or_unknown("BarRight"), "Bar (right)");
    }

    /// The **wire** half of the same claim, and the half nine string
    /// literals structurally cannot make (#1260 review F4): every entry of
    /// `Mount::ALL` must reach a human label, so a rename on the wire — or a
    /// tenth variant — reds here instead of leaking the raw wire string onto
    /// the screen through `mount_or_unknown`'s `other => other` arm, which is
    /// the one thing #1161 said not to do.
    ///
    /// `hytte-plugin-proto` is a **dev**-dependency for exactly this: its
    /// rlib is already linked into this binary through `hytte-plugin-agents`,
    /// so naming it costs no new `Cargo.lock` package and nothing at
    /// runtime — but the shipped binary has no use for `Mount`, only this
    /// assertion does.
    #[test]
    fn every_wire_mount_has_a_human_label() {
        for mount in hytte_plugin_proto::manifest::Mount::ALL {
            let wire = mount.wire_name();
            assert_ne!(
                mount_or_unknown(wire),
                wire,
                "{wire} has no human label in mount_or_unknown"
            );
        }
    }

    #[test]
    fn mount_falls_back_when_unknown() {
        assert_eq!(mount_or_unknown(""), "an unknown region");
        // A tenth mount this build predates: shown as-is, never hidden.
        assert_eq!(mount_or_unknown("SidebarLeftLeft"), "SidebarLeftLeft");
    }

    #[test]
    fn mount_display_is_just_the_effective_mount_with_no_declared_value() {
        assert_eq!(mount_display("SidebarTop", None), "Sidebar (middle)");
    }

    /// An override that took is the **common** case (the SDK applies
    /// `HYTTE_PLUGIN_MOUNT` before `Register`, so the host reports it back)
    /// and it is the one #1161 asked to make visible. Keying the note off
    /// `declared != mount` — what this did before #1260's review F1 — made
    /// this exact case say nothing.
    #[test]
    fn mount_display_says_an_override_in_force_is_set_by_nix() {
        assert_eq!(
            mount_display("SidebarRightTop", Some("SidebarRightTop")),
            "Sidebar, right (middle) · set by nix"
        );
    }

    /// The effective mount leads even when the declared one disagrees: the
    /// line it feeds says "rendering in", and the declared value is precisely
    /// the one that did not take (#1260 review F2).
    #[test]
    fn mount_display_keeps_the_effective_mount_first_when_the_override_failed() {
        assert_eq!(
            mount_display("SidebarTop", Some("SidebarRightTop")),
            "Sidebar (middle) · nix asked for Sidebar, right (middle) (not applied)"
        );
    }

    // ── The declared-mount reader (#1161) ────────────────────────────────────

    #[test]
    fn declared_mounts_from_json_reads_the_env_key() {
        let text = r#"{
            "version": 1,
            "plugins": {
                "agents": { "exec": "x", "env": { "HYTTE_PLUGIN_MOUNT": "SidebarRightTop" }, "secrets": [], "enabled": true }
            }
        }"#;
        let got = declared_mounts_from_json(text);
        assert_eq!(
            got.get("agents").map(String::as_str),
            Some("SidebarRightTop")
        );
    }

    #[test]
    fn declared_mounts_from_json_ignores_a_plugin_without_the_key() {
        let text = r#"{
            "version": 1,
            "plugins": {
                "pet": { "exec": "x", "env": {}, "secrets": [], "enabled": true }
            }
        }"#;
        assert!(declared_mounts_from_json(text).is_empty());
    }

    #[test]
    fn declared_mounts_from_json_is_empty_for_garbage() {
        assert!(declared_mounts_from_json("not json at all").is_empty());
        assert!(declared_mounts_from_json("{}").is_empty());
        assert!(declared_mounts_from_json(r#"{"plugins": "not an object"}"#).is_empty());
    }

    // ── The declared-mount map reaches `PluginRuntime` (#1161) ───────────────
    //
    // `runtime_states` is the wiring `on_poll_result_with_declared` runs on
    // every successful poll, split out precisely so this is testable without
    // a `PluginsState` (a live GTK widget tree `build_tab` builds — this
    // module is the hermetic half, see the module doc's "Tests must not
    // touch the real XDG config" precedent for why the filesystem read
    // itself is injected rather than called from in here too).

    #[test]
    fn runtime_states_attaches_the_declared_mount() {
        let mut declared = HashMap::new();
        declared.insert("agents".to_owned(), "SidebarRightTop".to_owned());
        let states: PollStates = vec![("agents".to_owned(), true, "SidebarTop".to_owned(), 1, 0)];
        let rt = runtime_states(states, &declared);
        let agents = rt.get("agents").expect("an entry for agents");
        assert_eq!(agents.mount, "SidebarTop");
        assert_eq!(agents.declared_mount.as_deref(), Some("SidebarRightTop"));
    }

    #[test]
    fn runtime_states_leaves_declared_mount_none_when_undeclared() {
        let states: PollStates = vec![("clock".to_owned(), true, "BarCenter".to_owned(), 1, 0)];
        let rt = runtime_states(states, &HashMap::new());
        assert_eq!(
            rt.get("clock").expect("an entry for clock").declared_mount,
            None
        );
    }

    // ── The declared-mount cache (#1260 review F7) ───────────────────────────
    //
    // The property: the 2 s poll costs one `stat`, and the blocking read +
    // `serde_json` parse runs only when `plugins.json` actually changed.
    // Drive the cache with a counting `read` — nothing else can tell a cache
    // that works from one that re-reads every time, since both return the
    // right map.

    /// A stamp for a file that is not there to be stat'd — the cache never
    /// dereferences the path, only compares the stamp.
    fn stamp(path: &str, len: u64) -> PluginsJson {
        PluginsJson {
            path: PathBuf::from(path),
            link: None,
            modified: None,
            len,
        }
    }

    fn one_override() -> HashMap<String, String> {
        let mut map = HashMap::new();
        map.insert("agents".to_owned(), "SidebarRightTop".to_owned());
        map
    }

    #[test]
    fn the_declared_mount_file_is_parsed_once_while_its_stamp_holds() {
        let mut cache = DeclaredMounts::default();
        let mut reads = 0;
        for _ in 0..5 {
            let map = cache.get(Some(stamp("/x/plugins.json", 42)), |_| {
                reads += 1;
                one_override()
            });
            assert_eq!(
                map.get("agents").map(String::as_str),
                Some("SidebarRightTop"),
                "every tick must still see the overrides"
            );
        }
        assert_eq!(
            reads, 1,
            "five polls over an unchanged file must parse it once, not {reads} times"
        );
    }

    #[test]
    fn a_changed_stamp_reparses() {
        let mut cache = DeclaredMounts::default();
        let mut reads = 0;
        cache.get(Some(stamp("/x/plugins.json", 42)), |_| {
            reads += 1;
            HashMap::new()
        });
        // Same path, different content: a rebuild.
        let map = cache.get(Some(stamp("/x/plugins.json", 43)), |_| {
            reads += 1;
            one_override()
        });
        assert_eq!(reads, 2, "a changed stamp must re-read");
        assert_eq!(
            map.get("agents").map(String::as_str),
            Some("SidebarRightTop")
        );
    }

    #[test]
    fn no_plugins_json_is_cached_too_and_never_read() {
        let mut cache = DeclaredMounts::default();
        for _ in 0..3 {
            let map = cache.get(None, |_| panic!("there is no file to read"));
            assert!(map.is_empty());
        }
    }

    /// The other direction of the same door: a file appearing where there was
    /// none must be picked up, not swallowed by the cached "no file".
    #[test]
    fn a_file_appearing_later_is_picked_up() {
        let mut cache = DeclaredMounts::default();
        assert!(
            cache
                .get(None, |_| panic!("there is no file yet"))
                .is_empty()
        );
        let map = cache.get(Some(stamp("/x/plugins.json", 42)), |_| one_override());
        assert_eq!(
            map.get("agents").map(String::as_str),
            Some("SidebarRightTop")
        );
    }

    // ── The XDG search path (#1260 review F8) ────────────────────────────────
    //
    // `probe_plugins_json`'s `$XDG_CONFIG_HOME` → `$XDG_CONFIG_DIRS` walk is
    // exactly what differs between a home-manager install
    // (`~/.config/trollshell/plugins.json`) and a NixOS one
    // (`/etc/xdg/trollshell/plugins.json`), and the `dirs` leg — the only one
    // a NixOS user ever hits — had no test at all. `Env`'s four fields are
    // `pub`, so these drive two `tempfile` dirs and never touch the real XDG
    // config (`tests-must-not-touch-real-xdg`).

    /// An `Env` naming `home` as the overlay and `dirs` as the single base.
    fn env_of(home: &Path, dirs: &Path) -> hytte_config::xdg::Env {
        hytte_config::xdg::Env {
            home: None,
            config_home: Some(home.to_string_lossy().into_owned()),
            config_dirs: Some(dirs.to_string_lossy().into_owned()),
            state_home: None,
        }
    }

    /// Write a `plugins.json` declaring one override under `base`.
    fn write_plugins_json(base: &Path, mount: &str) -> PathBuf {
        let dir = base.join("trollshell");
        std::fs::create_dir_all(&dir).expect("create the trollshell config dir");
        let path = dir.join("plugins.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"version":1,"plugins":{{"agents":{{"exec":"x","env":{{"HYTTE_PLUGIN_MOUNT":"{mount}"}},"secrets":[],"enabled":true}}}}}}"#
            ),
        )
        .expect("write plugins.json");
        path
    }

    #[test]
    fn the_search_path_finds_a_dirs_only_plugins_json() {
        let home = tempfile::tempdir().expect("a temp XDG_CONFIG_HOME");
        let dirs = tempfile::tempdir().expect("a temp XDG_CONFIG_DIRS entry");
        // The NixOS shape: nothing in the overlay, the file in the base.
        let written = write_plugins_json(dirs.path(), "SidebarRightBottom");
        let env = env_of(home.path(), dirs.path());

        let found = probe_plugins_json(&env).expect("the dirs entry must be found");
        assert_eq!(found.path, written);
        assert_eq!(
            read_declared_mounts_at(&found.path)
                .get("agents")
                .map(String::as_str),
            Some("SidebarRightBottom"),
            "a NixOS install's override must reach the tab"
        );
    }

    #[test]
    fn the_overlay_wins_over_the_dirs_entry() {
        let home = tempfile::tempdir().expect("a temp XDG_CONFIG_HOME");
        let dirs = tempfile::tempdir().expect("a temp XDG_CONFIG_DIRS entry");
        let overlay = write_plugins_json(home.path(), "BarRight");
        write_plugins_json(dirs.path(), "SidebarRightBottom");
        let env = env_of(home.path(), dirs.path());

        let found = probe_plugins_json(&env).expect("the overlay must be found");
        assert_eq!(found.path, overlay, "first existing wins, overlay first");
        assert_eq!(
            read_declared_mounts_at(&found.path)
                .get("agents")
                .map(String::as_str),
            Some("BarRight")
        );
    }

    #[test]
    fn no_plugins_json_anywhere_probes_to_none() {
        let home = tempfile::tempdir().expect("a temp XDG_CONFIG_HOME");
        let dirs = tempfile::tempdir().expect("a temp XDG_CONFIG_DIRS entry");
        assert!(probe_plugins_json(&env_of(home.path(), dirs.path())).is_none());
    }

    // ── The stamp's `link` half must resolve every hop (#1270, #1260 review N1) ─

    /// Force `path`'s mtime to a fixed instant, the way
    /// `crates/hytte-config/src/places.rs`'s `config_watcher_reloads_only_on_changed_content`
    /// does — deterministic, no filesystem-granularity flakiness, and (the
    /// point here) identical across two otherwise-different files.
    fn force_mtime(path: &Path, secs: u64) {
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for mtime")
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
            .expect("force the mtime");
    }

    /// **Falsification target for #1270 item 1.** `plugins.json`'s candidate
    /// is itself a symlink to a symlink (`a -> b -> target`) — one hop
    /// deeper than either platform module actually renders, chosen because
    /// it is the shape that makes the difference between `read_link` (one
    /// hop: `readlink(a)` answers `b`, full stop, however many times `b`'s
    /// own target changes) and `canonicalize` (every hop) impossible to
    /// miss. `target1`/`target2` are forced to the same length AND the same
    /// mtime, so `(mtime, len)` alone cannot move — only a `link` that
    /// resolves all the way through sees `b` re-pointed from one to the
    /// other.
    ///
    /// **Falsified two ways, both pasted in the PR body**: reverting
    /// `probe_candidates`'s `canonicalize` back to `read_link` reds this
    /// (the immediate target is always `b`, never `target1`/`target2`
    /// directly); setting `link: None` outright reds it too — which is
    /// exactly the #1260 review's own measurement (all 180 pre-existing
    /// tests stay green with the field deleted, because none of them builds
    /// a real symlink chain).
    #[test]
    fn the_stamp_moves_when_a_symlink_chains_final_target_changes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let target1 = dir.path().join("target1");
        let target2 = dir.path().join("target2");
        // Same length (5 bytes each), different content.
        std::fs::write(&target1, "AAAAA").expect("write target1");
        std::fs::write(&target2, "BBBBB").expect("write target2");
        force_mtime(&target1, 1);
        force_mtime(&target2, 1);

        let b = dir.path().join("b");
        let a = dir.path().join("plugins.json");
        std::os::unix::fs::symlink(&target1, &b).expect("symlink b -> target1");
        std::os::unix::fs::symlink(&b, &a).expect("symlink a -> b");

        let candidates = vec![a.clone()];
        let stamp1 = probe_candidates(&candidates).expect("the chain resolves to a file");
        assert_eq!(stamp1.len, 5, "both targets are 5 bytes");
        assert_eq!(
            stamp1.modified,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1)),
            "both targets share the forced mtime"
        );
        assert_eq!(
            stamp1.link,
            target1.canonicalize().ok(),
            "the resolved link must be the chain's final target"
        );

        // Re-point the INTERMEDIATE hop only — `a` itself is never touched,
        // so `read_link(&a)` would answer `b` before this and after it.
        std::fs::remove_file(&b).expect("remove b");
        std::os::unix::fs::symlink(&target2, &b).expect("symlink b -> target2");

        let stamp2 = probe_candidates(&candidates).expect("the chain still resolves");
        assert_eq!(stamp2.len, stamp1.len, "length is unchanged by design");
        assert_eq!(
            stamp2.modified, stamp1.modified,
            "mtime is unchanged by design"
        );
        assert_ne!(
            stamp1, stamp2,
            "canonicalize must resolve through BOTH hops and see the final \
             target move, even though (mtime, len) alone cannot"
        );
        assert_eq!(
            stamp2.link,
            target2.canonicalize().ok(),
            "the resolved link must follow the re-point to the new final target"
        );
    }

    // ── The search path is resolved once, not per tick (#1270) ───────────────

    /// **Item 3 pin.** No tracing-capture harness exists in this crate
    /// (`grep -rn "fn capture(" crates/trollshell-control-center` finds
    /// nothing — the nearest is `hytte_config::test_support`, reachable only
    /// as a dev-dependency this crate doesn't take), so this pins the
    /// MECHANISM that keeps [`refresh_plugins`] from re-triggering
    /// `hytte_config::xdg`'s relative-path warning every tick, rather than
    /// capturing the warning itself: [`resolved_search_path`] is the exact
    /// function `refresh_plugins` calls, parameterised over `env` so a test
    /// can drive it with a hand-built (here: relative) one instead of the
    /// real process environment.
    ///
    /// The second call passes a **different** `Env` — one that would yield a
    /// real, non-empty candidate list if it were actually consulted — so a
    /// regression **in [`resolved_search_path`] itself** (recomputing on
    /// every call instead of consulting `cache`) would show up as
    /// `second != first` here.
    ///
    /// This test drives `resolved_search_path` directly against its own
    /// throwaway `OnceCell`, so it does **not** reach [`refresh_plugins`]'s
    /// call site — reverting that call site to
    /// `plugins_json_candidates(&Env::from_process())` (the exact pre-#1270
    /// per-tick shape) leaves this test green, because the regression is in
    /// which cache `refresh_plugins` consults, not in
    /// `resolved_search_path`'s own logic. That production wiring is what
    /// `gtk_tests::the_tick_resolves_the_search_path_through_the_tabs_own_latch`
    /// pins instead, by calling `refresh_plugins` itself and reading
    /// [`PluginsState::search_path`] back out.
    #[test]
    fn the_search_path_is_resolved_once_even_across_a_changed_env() {
        let cache: OnceCell<Vec<PathBuf>> = OnceCell::new();
        let relative = hytte_config::xdg::Env {
            home: None,
            config_home: Some("relative/path".to_owned()),
            config_dirs: None,
            state_home: None,
        };

        let first = resolved_search_path(&cache, &relative).to_vec();
        assert_eq!(
            first,
            vec![PathBuf::from("/etc/xdg/trollshell/plugins.json")],
            "a relative XDG_CONFIG_HOME with no $HOME contributes no home \
             leg, leaving only XDG_CONFIG_DIRS's unset-default /etc/xdg \
             (this is also where the relative-path warning fires)"
        );

        // A second, different `Env` — an ABSOLUTE `config_home` that would
        // add a real, DIFFERENT candidate ahead of `/etc/xdg/…` if
        // `resolved_search_path` actually re-resolved against it.
        let absolute = hytte_config::xdg::Env {
            config_home: Some("/should/not/be/seen".to_owned()),
            ..relative
        };
        let second = resolved_search_path(&cache, &absolute).to_vec();

        assert_eq!(
            second, first,
            "a second tick must reuse the cached search path, not re-resolve \
             it against a newer Env — which is exactly what would bring the \
             per-tick relative-path warning back"
        );
    }

    /// An unreadable (here: nonexistent) file is best-effort, the same as an
    /// unparsable one — no override note, never a panic.
    #[test]
    fn reading_a_missing_file_yields_an_empty_map() {
        assert!(read_declared_mounts_at(Path::new("/nonexistent/plugins.json")).is_empty());
    }

    #[test]
    fn violations_suffix_only_when_nonzero() {
        assert_eq!(violations_suffix(0), "");
        assert_eq!(violations_suffix(1), " · 1 dropped");
        assert_eq!(violations_suffix(7), " · 7 dropped");
    }

    #[test]
    fn seen_suffix_humanizes_the_gap() {
        assert_eq!(seen_suffix(2), "");
        assert_eq!(seen_suffix(30), " · seen 30s ago");
        assert_eq!(seen_suffix(600), " · seen 10m ago");
        assert_eq!(seen_suffix(7200), " · seen 2h ago");
    }

    // ── The status column (#887) ─────────────────────────────────────────────

    #[test]
    fn status_cell_reports_the_live_connection_first() {
        assert_eq!(
            status_cell("active", Some(&rt(true, "BarCenter", 0, 0))),
            "Rendering"
        );
        assert_eq!(
            status_cell("active", Some(&rt(false, "", 0, 0))),
            "Connected"
        );
    }

    /// The case the column exists for: the unit says one thing and the host
    /// says another. A `Running · enabled` subtitle beside a `Not connected`
    /// status is the whole diagnostic.
    ///
    /// A unit that is merely *starting* is not that case (#943 review): it has
    /// not had a chance to connect, so it must not wear the same word as a
    /// plugin that crashed after start.
    #[test]
    fn status_cell_separates_a_running_unit_from_a_live_plugin() {
        assert_eq!(status_cell("active", None), "Not connected");
        assert_eq!(plugin_subtitle("active", true), "Running · enabled");
        assert_eq!(status_cell("activating", None), "Starting…");
        assert_eq!(status_cell("reloading", None), "Starting…");
        // …and the two columns agree on a starting unit, which is the point.
        assert_eq!(plugin_subtitle("activating", true), "Starting… · enabled");
    }

    #[test]
    fn status_cell_covers_the_stopped_states() {
        assert_eq!(status_cell("failed", None), "Failed");
        assert_eq!(status_cell("deactivating", None), "Stopping…");
        assert_eq!(status_cell("inactive", None), "Stopped");
    }

    /// A state systemd grew later must not be laundered into "Stopped" — the
    /// column would then be quietly wrong rather than visibly ignorant.
    #[test]
    fn status_cell_admits_an_unknown_state() {
        assert_eq!(status_cell("maintenance", None), "Unknown");
    }

    // ── The row diff (#887) ──────────────────────────────────────────────────

    #[test]
    fn the_same_plugin_set_is_recognised() {
        assert!(same_plugin_set(
            &ids(&["clock", "departures"]),
            &ids(&["clock", "departures"])
        ));
    }

    /// Order-insensitive on purpose: a reorder that rebuilt the list would cost
    /// the selection and, when collapsed, the pushed page.
    #[test]
    fn a_reorder_alone_is_not_a_change() {
        assert!(same_plugin_set(
            &ids(&["clock", "departures"]),
            &ids(&["departures", "clock"])
        ));
    }

    #[test]
    fn membership_changes_are_changes() {
        assert!(!same_plugin_set(&ids(&["clock"]), &ids(&["clock", "pet"])));
        assert!(!same_plugin_set(&ids(&["clock", "pet"]), &ids(&["clock"])));
        // Same size, different member — the case a length check alone misses.
        assert!(!same_plugin_set(&ids(&["clock"]), &ids(&["pet"])));
    }

    #[test]
    fn the_first_load_is_a_change() {
        assert!(!same_plugin_set(&[], &ids(&["clock"])));
        assert!(same_plugin_set(&[], &[]));
    }
}

/// The layout half of #887, which is geometry and navigation state and so needs
/// a real display (`xvfb-run`) — hence the `system-tests` gate, mirroring
/// `trollshell/src/widgets/mpris.rs`'s `gtk_tests`.
///
/// These drive [`build_tab`] and [`apply_plugins`] directly with fabricated
/// unit lists: a test process has no session bus to answer `ListPlugins`, and
/// the layout does not care where the rows came from.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use adw::prelude::*;
    use gtk::glib;

    use super::{
        BIN_MIN_WIDTH_PX, COLLAPSE_WIDTH_PX, PENDING_TOGGLE_TIMEOUT, PendingToggle, PluginRuntime,
        PluginsState, PollResult, apply_plugins, build_tab, on_poll_result, on_toggle_result,
        refresh_detail,
    };
    use crate::test_support::captured_logs;

    /// Run the GTK main loop until it has nothing left to dispatch, so a queued
    /// resize/allocation actually happens.
    fn pump() {
        while glib::MainContext::default().iteration(false) {}
    }

    /// Feed the tab a unit list, as a `ListPlugins` reply would. The first id is
    /// connected and rendering; the rest are active but unconnected, which keeps
    /// both status-column branches on screen.
    fn apply(state: &PluginsState, plugin_ids: &[&str]) {
        let units: Vec<(String, String, bool)> = plugin_ids
            .iter()
            .map(|id| ((*id).to_owned(), "active".to_owned(), true))
            .collect();
        let mut rt = HashMap::new();
        if let Some(first) = plugin_ids.first() {
            rt.insert(
                (*first).to_owned(),
                PluginRuntime {
                    rendering: true,
                    mount: "BarCenter".to_owned(),
                    declared_mount: None,
                    last_seen_secs: 1,
                    violations: 0,
                },
            );
        }
        apply_plugins(state, &units, &rt);
        pump();
    }

    /// Feed the tab a unit list with an explicit `ActiveState` and no runtime
    /// overlay (#944) — for the pending-toggle tests, which care about the
    /// state string a stale poll reports rather than the connected/rendering
    /// badge [`apply`] fixes at `"active"`.
    fn apply_state(state: &PluginsState, plugin_ids: &[&str], active_state: &str) {
        let units: Vec<(String, String, bool)> = plugin_ids
            .iter()
            .map(|id| ((*id).to_owned(), active_state.to_owned(), true))
            .collect();
        apply_plugins(state, &units, &HashMap::new());
        pump();
    }

    /// Put the tab in a window `width` px wide and let GTK allocate it. The
    /// window is returned so the caller can keep it alive — a destroyed window
    /// unmaps the tree, and every assertion here is about a mapped tree.
    fn present(bin: &adw::BreakpointBin, width: i32) -> gtk::Window {
        let window = gtk::Window::new();
        window.set_child(Some(bin));
        window.set_default_size(width, 400);
        window.present();
        pump();
        window
    }

    /// Tear a presented window down without leaving the bin parented to a
    /// destroyed widget.
    fn dismiss(window: &gtk::Window) {
        window.set_child(None::<&gtk::Widget>);
        window.destroy();
        pump();
    }

    /// One failed poll: what [`super::refresh_plugins`]' `Err` arm does when
    /// `ListPlugins` times out or the shell isn't there. Same call, same
    /// strings — the tab cannot tell this apart from the real thing, which is
    /// the point.
    fn poll_failed(state: &PluginsState) {
        super::set_placeholder(
            state,
            super::PluginsView::Unavailable,
            "Unavailable",
            "Is trollshell running?",
        );
        pump();
    }

    /// One `list_plugins_and_states` reply as [`super::on_poll_result`]
    /// receives it (#983): every listed plugin in `active_state`, and no
    /// runtime overlay — the poll-ordering tests care about the `ActiveState`
    /// a completion carries, not about the connected/rendering badge.
    ///
    /// The `Ok` wrapper is the point, not an oversight: this and [`poll_err`]
    /// are the two arms of the same [`PollResult`], and a test reads better
    /// naming the outcome than spelling `Ok(…)` at each of its call sites.
    #[allow(clippy::unnecessary_wraps, reason = "the Ok arm of a PollResult pair")]
    fn poll_ok(plugin_ids: &[&str], active_state: &str) -> PollResult {
        let units = plugin_ids
            .iter()
            .map(|id| ((*id).to_owned(), active_state.to_owned(), true))
            .collect();
        Ok((units, Vec::new()))
    }

    /// A failed poll's completion — what a `ListPlugins` timeout hands
    /// [`super::on_poll_result`].
    fn poll_err() -> PollResult {
        Err(hytte_bus::BusError::Permanent {
            reason: "timed out".to_owned(),
            dbus_name: None,
        })
    }

    /// The status column's current word for `id`'s sidebar row (#887) — the
    /// row half of "the view must not regress", beside the detail switch.
    fn status_text(state: &PluginsState, id: &str) -> String {
        let rows = state.by_id.borrow();
        let text = rows
            .get(id)
            .unwrap_or_else(|| panic!("no row for {id}"))
            .status
            .text()
            .to_string();
        drop(rows);
        text
    }

    /// Every `GtkWindowControls` under `root`, at any depth.
    ///
    /// Walked by hand (`first_child`/`next_sibling`) rather than by any public
    /// "find a descendant" API, because there isn't one: header-bar internals
    /// are private widgetry, and the question — *does this subtree draw window
    /// buttons?* — is about what GTK built underneath, not about anything the
    /// tab named.
    fn window_controls_under(root: &gtk::Widget) -> Vec<gtk::WindowControls> {
        let mut found = Vec::new();
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Ok(controls) = widget.clone().downcast::<gtk::WindowControls>() {
                found.push(controls);
            }
            found.extend(window_controls_under(&widget));
            child = widget.next_sibling();
        }
        found
    }

    /// What a click on a row does: select it, then activate it.
    /// `ListBox::select_row` emits `row-selected`, and the click gesture emits
    /// `row-activated` after it — the two halves the tab wires separately, one
    /// for retargeting the detail pane and one for the collapsed push.
    fn click(state: &PluginsState, id: &str) {
        let rows = state.by_id.borrow();
        let row = rows
            .get(id)
            .unwrap_or_else(|| panic!("no row for {id}"))
            .row
            .clone();
        drop(rows);
        let row = row.upcast::<gtk::ListBoxRow>();
        state.list.select_row(Some(&row));
        state.list.emit_by_name::<()>("row-activated", &[&row]);
        pump();
    }

    /// Wide: both panes, side by side, inside the bin.
    ///
    /// Bounds *and* `pick`, deliberately: `is_visible()` is orthogonal to
    /// clipping, so a pane can be visible and drawn nowhere near where the test
    /// thinks it is. `compute_bounds` says where it actually is; `pick` says
    /// what a user's pointer would actually hit there.
    #[gtk::test]
    fn a_wide_allocation_shows_both_panes() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 640);

        let width = f64::from(bin.width());
        assert!(
            width > COLLAPSE_WIDTH_PX,
            "the harness' virtual display gave the bin only {width}px, at or below the \
             {COLLAPSE_WIDTH_PX}px threshold — this case cannot test the wide layout"
        );
        assert!(
            !state.split.is_collapsed(),
            "at {width}px the split view must stay expanded"
        );

        let sidebar = state.split.sidebar().expect("a sidebar page");
        let content = state.split.content().expect("a content page");
        let height = f64::from(bin.height());

        for (name, page) in [("sidebar", &sidebar), ("content", &content)] {
            let bounds = page
                .compute_bounds(&bin)
                .unwrap_or_else(|| panic!("{name} has no bounds relative to the bin"));
            assert!(
                bounds.width() > 0.0 && bounds.height() > 0.0,
                "the {name} pane is allocated {}×{}",
                bounds.width(),
                bounds.height()
            );
            assert!(
                bounds.x() >= -0.5 && f64::from(bounds.x() + bounds.width()) <= width + 0.5,
                "the {name} pane spans {}…{} outside the bin's 0…{width}",
                bounds.x(),
                bounds.x() + bounds.width()
            );
        }

        let left = bin
            .pick(width * 0.15, height * 0.6, gtk::PickFlags::DEFAULT)
            .expect("something to pick on the left");
        assert!(
            left.is_ancestor(&sidebar),
            "the left edge must be the plugin list, not {}",
            left.type_()
        );
        let right = bin
            .pick(width * 0.85, height * 0.6, gtk::PickFlags::DEFAULT)
            .expect("something to pick on the right");
        assert!(
            right.is_ancestor(&content),
            "the right edge must be the detail pane, not {}",
            right.type_()
        );

        dismiss(&window);
    }

    /// Narrow: the list is the whole tab, and activating a row pushes the
    /// detail page titled with the plugin's id.
    ///
    /// Falsified by deleting `bin.add_breakpoint(breakpoint)` in `build_tab`:
    /// the split view then never collapses and this fails at the
    /// `is_collapsed` assertion, with the content pane still on screen beside
    /// a 420px list.
    #[gtk::test]
    fn a_narrow_allocation_collapses_to_the_list() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);

        let width = f64::from(bin.width());
        assert!(
            width <= COLLAPSE_WIDTH_PX,
            "the bin was allocated {width}px, above the {COLLAPSE_WIDTH_PX}px threshold — \
             this case cannot test the narrow layout"
        );
        assert!(
            state.split.is_collapsed(),
            "at {width}px the split view must collapse"
        );
        assert!(
            !state.split.shows_content(),
            "collapsed, the tab opens on the list, not on a detail page"
        );

        let sidebar = state.split.sidebar().expect("a sidebar page");
        let content = state.split.content().expect("a content page");
        assert!(sidebar.is_mapped(), "the list must be on screen");
        assert!(
            !content.is_mapped(),
            "collapsed and un-pushed, the detail pane must not be on screen"
        );

        // Whatever is under the pointer in the middle of the tab is the list.
        let hit = bin
            .pick(
                width * 0.5,
                f64::from(bin.height()) * 0.5,
                gtk::PickFlags::DEFAULT,
            )
            .expect("something to pick");
        assert!(
            hit.is_ancestor(&sidebar),
            "collapsed, the whole tab is the list; picked {} instead",
            hit.type_()
        );

        // The drill-down itself.
        click(&state, "departures");
        assert!(
            state.split.shows_content(),
            "activating a row must push its detail page"
        );
        assert_eq!(
            content.title(),
            "departures",
            "the pushed page is titled with the plugin's id"
        );
        assert!(content.is_mapped(), "the pushed page must be on screen");

        dismiss(&window);
    }

    /// A poll that returns the same plugins must be invisible: same selection,
    /// same pushed page, and the very same row widgets (a rebuild would replace
    /// them and take the selection with it).
    #[gtk::test]
    fn a_refresh_with_the_same_set_keeps_the_navigation_state() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);
        click(&state, "departures");

        let before = state
            .by_id
            .borrow()
            .get("departures")
            .map(|prow| prow.row.clone())
            .expect("a row for departures");

        apply(&state, &["clock", "departures"]);

        let after = state
            .by_id
            .borrow()
            .get("departures")
            .map(|prow| prow.row.clone())
            .expect("a row for departures");
        assert_eq!(before, after, "an unchanged set must not rebuild the rows");
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("departures"),
            "the selection must survive a refresh"
        );
        assert_eq!(
            state.list.selected_row().as_ref(),
            Some(after.upcast_ref::<gtk::ListBoxRow>()),
            "the selected row must still be highlighted"
        );
        assert!(
            state.split.shows_content(),
            "the pushed detail page must survive a refresh"
        );
        assert_eq!(
            state.split.content().expect("a content page").title(),
            "departures"
        );

        dismiss(&window);
    }

    /// The awkward one: the selected plugin is uninstalled between polls. The
    /// pushed page must pop rather than keep showing a unit that no longer
    /// exists, and the sidebar must settle on a plugin that does.
    #[gtk::test]
    fn a_refresh_that_drops_the_selection_pops_back() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);
        click(&state, "departures");
        assert!(state.split.shows_content());

        apply(&state, &["clock"]);

        assert!(
            !state.split.shows_content(),
            "the pushed page must pop when its plugin disappears"
        );
        assert_ne!(
            state.selected.borrow().as_deref(),
            Some("departures"),
            "a plugin that is gone cannot stay selected"
        );
        assert_ne!(
            state.split.content().expect("a content page").title(),
            "departures",
            "the detail pane must not still be titled for a plugin that is gone"
        );
        // …and it lands on the one plugin that is left, rather than a blank
        // pane beside a full list.
        assert_eq!(state.selected.borrow().as_deref(), Some("clock"));
        assert_eq!(
            state.detail.stack.visible_child_name().as_deref(),
            Some("plugin")
        );

        dismiss(&window);
    }

    /// The genuinely empty case, which is where the empty state does show: a
    /// poll that reports no plugins at all clears the selection and blanks the
    /// pane, and does not leave a row behind that could be drilled into.
    #[gtk::test]
    fn losing_every_plugin_blanks_the_detail_pane() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock"]);
        let window = present(&bin, 420);
        click(&state, "clock");
        assert!(state.split.shows_content());

        super::set_placeholder(
            &state,
            super::PluginsView::Empty,
            "No plugins installed",
            "Install a trollshell-plugin unit to manage it here.",
        );
        pump();

        assert!(!state.split.shows_content(), "the pushed page must pop");
        assert_eq!(state.selected.borrow().as_deref(), None);
        assert_eq!(
            state.detail.stack.visible_child_name().as_deref(),
            Some("empty")
        );
        assert!(state.list.selected_row().is_none());

        dismiss(&window);
    }

    /// A surviving selection is restored across a genuine rebuild — the other
    /// half of the diff: the set changed, so the rows *are* torn down, and the
    /// selection has to be put back by id.
    #[gtk::test]
    fn a_rebuild_restores_a_surviving_selection_without_navigating() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);
        click(&state, "departures");

        // A third plugin appears: a real membership change, so the rows are
        // rebuilt.
        apply(&state, &["clock", "departures", "pet"]);

        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("departures"),
            "a surviving plugin keeps the selection across a rebuild"
        );
        assert!(
            state.list.selected_row().is_some(),
            "the restored selection must be highlighted"
        );
        assert!(
            state.split.shows_content(),
            "restoring a selection must not pop the page the user pushed"
        );
        // The other half of the rebuild: the plugin that *appeared* got a row.
        // Without this the test passes under a `same_plugin_set → true`
        // mutation, which skips the rebuild entirely — the selection survives
        // for the wrong reason and `pet` never reaches the list (#943 review).
        assert!(
            state.by_id.borrow().contains_key("pet"),
            "a plugin that appeared must get a row"
        );

        dismiss(&window);
    }

    /// A poll that *fails* is not a poll that says the plugin set changed.
    ///
    /// `list_plugins` runs `RetryPolicy::Never` with a 3 s timeout on a 2 s
    /// cadence, so one slow reply shows the "unavailable" placeholder. That
    /// placeholder legitimately drops the selection — its rows are gone — but
    /// the next good poll must put the user back where they were, page and
    /// all, instead of taking the first-load arm and silently selecting the
    /// first plugin in the list (#943 review).
    ///
    /// Falsified by deleting the `parked` restore in `apply_plugins` (the
    /// `(None, Some(parked))` arm): the selection comes back as `clock` and
    /// the page stays popped.
    #[gtk::test]
    fn a_transient_poll_failure_keeps_the_selection() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);
        click(&state, "departures");
        assert!(state.split.shows_content());

        poll_failed(&state);
        // The placeholder itself is unchanged: no rows, no selection, popped.
        assert_eq!(state.selected.borrow().as_deref(), None);
        assert!(!state.split.shows_content());

        // …and the very next good poll returns the same plugins.
        apply(&state, &["clock", "departures"]);

        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("departures"),
            "a failed poll must not retarget the selection"
        );
        assert!(
            state.split.shows_content(),
            "the page the user pushed must come back with it"
        );
        assert_eq!(
            state.split.content().expect("a content page").title(),
            "departures"
        );
        let row = state
            .by_id
            .borrow()
            .get("departures")
            .map(|prow| prow.row.clone())
            .expect("a row for departures");
        assert_eq!(
            state.list.selected_row().as_ref(),
            Some(row.upcast_ref::<gtk::ListBoxRow>()),
            "the restored selection must be highlighted too"
        );
        assert!(
            state.parked.borrow().is_none(),
            "a restored selection must not stay parked"
        );

        dismiss(&window);
    }

    /// The other side of the same coin: the failure was real *and* the plugin
    /// really did go away while the shell was down. Then the parked selection
    /// is stale, and the pre-existing pop-then-settle-on-the-first semantics
    /// apply unchanged.
    #[gtk::test]
    fn a_failure_that_hid_a_removed_plugin_still_pops_back() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 420);
        click(&state, "departures");

        poll_failed(&state);
        apply(&state, &["clock"]);

        assert!(
            !state.split.shows_content(),
            "a plugin that is genuinely gone must not have its page restored"
        );
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("clock"),
            "the sidebar settles on the surviving plugin"
        );
        assert_ne!(
            state.split.content().expect("a content page").title(),
            "departures"
        );
        assert!(
            state.parked.borrow().is_none(),
            "the park is spent either way"
        );

        dismiss(&window);
    }

    /// The tab must not draw a second set of window buttons.
    ///
    /// Mounted the way `crate::build_window` mounts it — inside an
    /// `AdwApplicationWindow` at its real 760 × 560 default, under a
    /// `AdwToolbarView` whose top bar is the app's own header bar with the view
    /// switcher — because that context is the entire bug: two `AdwHeaderBar`s
    /// on libadwaita's defaults draw their own `GtkWindowControls` inside a
    /// window that already has a header bar of its own.
    ///
    /// The positive half matters as much as the negative one: the app's header
    /// bar *does* have mapped controls here, so a walk that simply found
    /// nothing would fail rather than pass vacuously.
    ///
    /// Falsified by dropping `show_start_title_buttons(false)` /
    /// `show_end_title_buttons(false)` from `tab_header_bar`.
    #[gtk::test]
    fn the_tab_draws_no_window_controls() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);

        // `crate::build_window`'s shape, minus the tabs the tab under test
        // doesn't need.
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &bin,
            Some("plugins"),
            "Plugins",
            "application-x-addon-symbolic",
        );
        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&stack));
        let window = adw::ApplicationWindow::builder()
            .title("trollshell Control Center")
            .default_width(760)
            .default_height(560)
            .content(&toolbar)
            .build();
        window.present();
        pump();

        let app_controls: Vec<gtk::WindowControls> = window_controls_under(header.upcast_ref())
            .into_iter()
            .filter(gtk::prelude::WidgetExt::is_mapped)
            .collect();
        assert!(
            !app_controls.is_empty(),
            "the window's own header bar must keep its controls — without them this \
             test cannot tell 'no controls in the tab' from 'no controls anywhere'"
        );

        let stray: Vec<gtk::WindowControls> = window_controls_under(bin.upcast_ref())
            .into_iter()
            .filter(gtk::prelude::WidgetExt::is_mapped)
            .collect();
        assert!(
            stray.is_empty(),
            "the tab draws {} mapped GtkWindowControls of its own (first is {}px wide) — \
             a second close/minimise/maximise cluster inside the window",
            stray.len(),
            stray.first().map_or(0, gtk::prelude::WidgetExt::width)
        );

        // The one start-side button the content header *should* grow when
        // collapsed and pushed: the navigation back button, which is
        // `show-back-button`'s business and not the title buttons'.
        state.split.set_collapsed(true);
        pump();
        click(&state, "departures");
        let back = has_mapped_back_button(&state.split.content().expect("a content page"));
        assert!(
            back,
            "turning the title buttons off must not cost the collapsed back button"
        );

        window.set_content(None::<&gtk::Widget>);
        window.destroy();
        pump();
    }

    /// Whether `page`'s subtree has a mapped `go-previous-symbolic` button —
    /// what `AdwHeaderBar` grows inside a navigation stack.
    fn has_mapped_back_button(page: &adw::NavigationPage) -> bool {
        fn walk(root: &gtk::Widget) -> bool {
            let mut child = root.first_child();
            while let Some(widget) = child {
                if let Ok(button) = widget.clone().downcast::<gtk::Button>()
                    && button.icon_name().as_deref() == Some("go-previous-symbolic")
                    && button.is_mapped()
                {
                    return true;
                }
                if walk(&widget) {
                    return true;
                }
                child = widget.next_sibling();
            }
            false
        }
        walk(page.upcast_ref())
    }

    /// Dropping the tab must free it.
    ///
    /// The tab's own handlers are owned by widgets *inside* the tab, so a
    /// handler that captured a strong `PluginsState` would close a cycle
    /// (`list` → handler → state → `list`) that nothing breaks: every
    /// control-center window would leave its whole Plugins tree behind, and
    /// `crate::build_window`'s close handler — which only stops the timers —
    /// cannot help. Hence the weak captures, and hence this test: drop every
    /// strong handle, pump the main loop, and the `ListBox` must be gone.
    ///
    /// Falsified by capturing `state.clone()` in `connect_selection` /
    /// `connect_switch` instead of `state.downgrade()`: the weak refs still
    /// upgrade afterwards.
    #[gtk::test]
    fn dropping_the_tab_frees_the_widget_tree() {
        adw::init().expect("libadwaita init");
        let (list, switch, split) = {
            let (bin, state) = build_tab();
            apply(&state, &["clock", "departures"]);
            let window = present(&bin, 640);
            // Through the handlers, so the closures have actually run.
            click(&state, "departures");

            let weak = (
                state.list.downgrade(),
                state.detail.switch.downgrade(),
                state.split.downgrade(),
            );
            dismiss(&window);
            weak
            // `bin` and `state` — the only strong handles a caller holds —
            // drop here.
        };
        pump();

        assert!(
            list.upgrade().is_none(),
            "the plugin list outlived the tab: a handler is holding the state that holds it"
        );
        assert!(
            switch.upgrade().is_none(),
            "the detail switch outlived the tab"
        );
        assert!(
            split.upgrade().is_none(),
            "the split view outlived the tab — the whole tree with it"
        );
    }

    /// The #856 contract, asserted rather than hoped for.
    ///
    /// `AdwBreakpointBin` warns once per allocation — forever — when its
    /// child's minimum width exceeds the bin's width; the condition is exactly
    /// `min_width > width`. Adding a breakpoint strips the bin's own minimum,
    /// so the bin can be allocated as little as its `width-request`. Both
    /// configurations therefore have a floor to clear:
    ///
    /// * collapsed, the bin can be as narrow as `BIN_MIN_WIDTH_PX`;
    /// * expanded, it is never narrower than `COLLAPSE_WIDTH_PX + 1` — below
    ///   that the breakpoint has already collapsed it.
    ///
    /// Measured on libadwaita 1.9.3: 410 px expanded (against a 521 px floor)
    /// and 190 px collapsed (against 360 px). Both have room, and the numbers
    /// say where it went — the expanded figure is the sidebar's 220 px minimum
    /// plus the detail pane's ~190 px.
    ///
    /// #856 also notes the warning never shows up in tests (the bin blocks
    /// warnings around first allocation and the breakpoint transition), so
    /// grepping stderr proves nothing. Measuring does.
    #[gtk::test]
    fn the_bin_is_never_narrower_than_its_child_needs() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures", "claude-bridge"]);
        let window = present(&bin, 640);

        state.split.set_collapsed(false);
        pump();
        let expanded = state.split.measure(gtk::Orientation::Horizontal, -1).0;
        assert!(
            f64::from(expanded) <= COLLAPSE_WIDTH_PX + 1.0,
            "expanded, the split view needs {expanded}px but the breakpoint lets it be \
             allocated {}px — AdwBreakpointBin would warn on every allocation (#856)",
            COLLAPSE_WIDTH_PX + 1.0
        );

        state.split.set_collapsed(true);
        pump();
        let collapsed = state.split.measure(gtk::Orientation::Horizontal, -1).0;
        assert!(
            collapsed <= BIN_MIN_WIDTH_PX,
            "collapsed, the split view needs {collapsed}px but the bin's floor is \
             {BIN_MIN_WIDTH_PX}px — AdwBreakpointBin would warn on every allocation (#856)"
        );

        dismiss(&window);
    }

    /// The status column reaches the row, and says what the plugin is doing
    /// rather than repeating what the unit is doing.
    #[gtk::test]
    fn the_rows_carry_the_status_column() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply(&state, &["clock", "departures"]);
        let window = present(&bin, 640);

        let rows = state.by_id.borrow();
        let clock = rows.get("clock").expect("a row for clock");
        let departures = rows.get("departures").expect("a row for departures");
        assert_eq!(clock.status.text(), "Rendering");
        assert_eq!(departures.status.text(), "Not connected");
        // Both units are `active · enabled`; only the column separates them.
        assert_eq!(clock.row.subtitle().as_deref(), Some("Running · enabled"));
        assert_eq!(
            departures.row.subtitle().as_deref(),
            Some("Running · enabled")
        );
        drop(rows);

        dismiss(&window);
    }

    // ── The switch's pending toggle (#944) ───────────────────────────────────
    //
    // These construct `PendingToggle` directly rather than flipping
    // `state.detail.switch` and letting `connect_switch` record it: that
    // handler also fires a real `Control` call, and — same reason `apply`
    // fabricates poll replies instead of calling `ListPlugins` — this test
    // process has no session bus to answer it. Writing `state.pending`
    // straight is the recording half of `connect_switch` with the D-Bus round
    // trip removed; `refresh_detail`'s consumption of it is exercised for
    // real through `apply_state`.

    /// The case #944 was filed for: the user turns a plugin on, but the very
    /// next poll still reports the pre-toggle `ActiveState` (systemd hasn't
    /// caught up). The switch must hold the user's answer, not the stale one.
    ///
    /// Falsified by deleting the `resolve_pending` line in `refresh_detail`
    /// (using `is_running(&snap.active_state)` for `show_running` directly):
    /// then the stale "inactive" poll snaps the switch back off.
    #[gtk::test]
    fn a_pending_toggle_holds_the_switch_against_a_stale_poll() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);
        assert!(!state.detail.switch.is_active(), "clock starts stopped");

        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now(),
        });

        // A stale poll: systemd hasn't caught up yet, still "inactive".
        apply_state(&state, &["clock"], "inactive");

        assert!(
            state.detail.switch.is_active(),
            "a stale poll must not bounce the switch back to the snapshot's ActiveState"
        );
        assert!(
            state.pending.borrow().is_some(),
            "the intent is still unresolved and must stay pending"
        );

        dismiss(&window);
    }

    /// Once a poll actually agrees with what the user asked for, the intent is
    /// spent — the switch should read that as confirmation, not merely as one
    /// more poll to ignore.
    #[gtk::test]
    fn a_confirming_poll_clears_the_pending_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);

        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now(),
        });
        apply_state(&state, &["clock"], "inactive");
        assert!(
            state.pending.borrow().is_some(),
            "sanity: still pending before the confirming poll"
        );

        // The unit has caught up.
        apply_state(&state, &["clock"], "active");

        assert!(
            state.detail.switch.is_active(),
            "the switch must stay on once the poll agrees"
        );
        assert!(
            state.pending.borrow().is_none(),
            "a poll that matches the wanted state must clear the intent"
        );

        dismiss(&window);
    }

    /// The backstop: a transition that never actually happens (crashed unit,
    /// hung `systemd-run`) must not freeze the switch on the user's wish
    /// forever. Past `PENDING_TOGGLE_TIMEOUT`, truth wins even though the poll
    /// never agreed.
    ///
    /// The intent is backdated rather than slept for — the injected clock
    /// this timeout needs to be testable without a real 10 s wait.
    #[gtk::test]
    fn a_timed_out_intent_lets_a_stale_poll_through() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);

        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now()
                .checked_sub(PENDING_TOGGLE_TIMEOUT + Duration::from_millis(50))
                // `Instant` is `CLOCK_MONOTONIC`, anchored to boot, not to
                // process start — the precondition is "the machine has been
                // up for over 10s", trivially true anywhere this test runs.
                .expect("machine has been up for over 10s"),
        });

        // Still stale — the unit never actually started — but the intent has
        // expired, so the real state wins.
        apply_state(&state, &["clock"], "inactive");

        assert!(
            !state.detail.switch.is_active(),
            "an expired intent must stop overriding the real state"
        );
        assert!(
            state.pending.borrow().is_none(),
            "a timed-out intent must be cleared, not merely ignored once"
        );

        dismiss(&window);
    }

    /// Selecting a different plugin must drop the intent rather than let it
    /// silently steer a switch it was never about.
    #[gtk::test]
    fn switching_the_shown_plugin_drops_the_pending_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock", "departures"], "inactive");
        let window = present(&bin, 640);
        click(&state, "clock");

        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now(),
        });

        click(&state, "departures");

        assert_eq!(state.selected.borrow().as_deref(), Some("departures"));
        assert!(
            state.pending.borrow().is_none(),
            "selecting a different plugin must drop the other one's pending intent"
        );
        assert!(
            !state.detail.switch.is_active(),
            "the newly shown plugin's switch must read its own real state, unclouded by the \
             dropped intent"
        );

        dismiss(&window);
    }

    // ── #945 review fixes ────────────────────────────────────────────────────

    /// Finding 1: a failed `StartPlugin`/`StopPlugin` already knows — at its
    /// own `RetryPolicy::Never` timeout, well inside `PENDING_TOGGLE_TIMEOUT`
    /// — that the transition it recorded an intent for never happened, and
    /// must clear that intent rather than leave `resolve_pending` answering
    /// `wanted` for the full 10s window.
    ///
    /// Drives `on_toggle_result` directly (the completion half of
    /// `connect_switch`, with the D-Bus round trip removed) rather than a real
    /// `Control` call, for the reason the module doc above gives.
    ///
    /// Falsified by removing the `state.pending.borrow_mut().take()` call from
    /// `on_toggle_result`'s `Err` arm: the intent survives, and the next
    /// (correctly stale) poll still shows the user's wish instead of the
    /// truth.
    #[gtk::test]
    fn a_failed_toggle_clears_its_own_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);

        let since = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since,
        });

        on_toggle_result(
            &state,
            since,
            Err(hytte_bus::BusError::Permanent {
                reason: "no such unit".to_owned(),
                dbus_name: None,
            }),
        );

        assert!(
            state.pending.borrow().is_none(),
            "a failed toggle must clear the intent it recorded"
        );

        // The next poll — still reporting the pre-toggle state, since the
        // toggle never actually happened — must now be read as the truth
        // rather than bounced off a lingering intent.
        apply_state(&state, &["clock"], "inactive");
        assert!(
            !state.detail.switch.is_active(),
            "with the intent cleared, the switch must show the real (unchanged) state"
        );

        dismiss(&window);
    }

    /// Finding 1's identity guard: a second toggle recorded while the first
    /// call is still in flight must survive that first call's (failed)
    /// completion — the two are different intents (different `since`), and a
    /// completion only owns the one it started with.
    ///
    /// Falsified by dropping the `since` comparison in `on_toggle_result` (an
    /// unconditional `state.pending.borrow_mut().take()` on `Err`): intent B
    /// would be clobbered by intent A's late, irrelevant failure.
    #[gtk::test]
    fn a_failed_toggles_completion_does_not_clobber_a_newer_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);

        // Intent A: the user's first click.
        let since_a = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: since_a,
        });

        // Intent B: the user flips the switch again before A's round trip
        // returns — a later `since`, replacing A in `state.pending` exactly
        // as a real second flip would (`connect_switch` always overwrites).
        let since_b = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: false,
            since: since_b,
        });

        // A's call finally completes — and fails.
        on_toggle_result(
            &state,
            since_a,
            Err(hytte_bus::BusError::Permanent {
                reason: "timed out".to_owned(),
                dbus_name: None,
            }),
        );

        let pending = state.pending.borrow();
        let intent = pending
            .as_ref()
            .expect("intent B must survive intent A's completion");
        assert_eq!(
            intent.since, since_b,
            "the surviving intent must be B, not wiped by A's stale completion"
        );
        assert!(
            !intent.wanted,
            "the surviving intent must still be B's wish"
        );
        drop(pending);

        dismiss(&window);
    }

    /// Finding 2: deselecting must drop the intent too — the same "nothing
    /// shown ⇒ no intent belongs to the pane" rule `clear_selection` applies.
    /// Reached here because a ctrl-click deselect (`Single`-mode `GtkListBox`
    /// allows it) drives `connect_row_selected(None)` straight into
    /// `refresh_detail`'s early-return arm, outside `clear_selection`.
    ///
    /// Falsified by removing the `state.pending.borrow_mut().take()` call
    /// from that arm.
    #[gtk::test]
    fn deselecting_drops_the_pending_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);
        click(&state, "clock");

        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now(),
        });

        // The ctrl-click deselect path: `connect_selection`'s `row-selected`
        // handler just sets `selected` and calls `refresh_detail` — no
        // `clear_selection` in between.
        *state.selected.borrow_mut() = None;
        refresh_detail(&state);

        assert!(
            state.pending.borrow().is_none(),
            "deselecting must drop the pending intent along with the selection"
        );

        dismiss(&window);
    }

    /// Finding 3: an "unavailable" placeholder must park a live intent
    /// alongside the selection it already parks, not drop it — a poll failure
    /// inside `PENDING_TOGGLE_TIMEOUT` must not re-expose the very bounce
    /// #944 fixed the moment the selection is restored. The restored intent
    /// must also keep counting from its original `since`, not the restore.
    ///
    /// Falsified by not carrying `pending` through `ParkedSelection` (or by
    /// restamping `since` on restore).
    #[gtk::test]
    fn an_unavailable_poll_parks_the_intent_with_the_selection() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);
        click(&state, "clock");

        let since = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since,
        });

        // One failed poll: the shell went briefly unreachable.
        poll_failed(&state);
        assert!(
            state.selected.borrow().is_none(),
            "sanity: the placeholder drops the live selection"
        );

        // The next good poll brings the same plugin back.
        apply_state(&state, &["clock"], "inactive");

        assert_eq!(state.selected.borrow().as_deref(), Some("clock"));
        assert!(
            state.detail.switch.is_active(),
            "the restored selection must still show the user's wanted state, not the poll \
             that never agreed with it"
        );
        let restored = state.pending.borrow();
        let intent = restored
            .as_ref()
            .expect("the intent must be restored alongside the selection");
        assert_eq!(intent.plugin_id, "clock");
        assert_eq!(
            intent.since, since,
            "the restored intent must keep counting from its original `since`, not reset the \
             timeout"
        );
        drop(restored);

        dismiss(&window);
    }

    /// #945 re-check's new finding: a poll failure can park the live intent
    /// (`set_placeholder` moves it into `ParkedSelection::pending`, proven by
    /// [`an_unavailable_poll_parks_the_intent_with_the_selection`] above)
    /// *before* the toggle's own completion arrives — the common case, since
    /// `ListPlugins` and `StartPlugin` hit the same `Control` endpoint and one
    /// dead shell fails both. `on_toggle_result`'s identity guard on
    /// `state.pending` alone finds nothing to clear in that ordering, so the
    /// definitively failed intent would otherwise be restored with the
    /// selection on the next good poll — the switch lying again until
    /// `PENDING_TOGGLE_TIMEOUT`.
    ///
    /// Falsified by removing the park-inspection clause from
    /// `on_toggle_result`'s `Err` arm: `pending` stays `Some` in the park,
    /// gets restored, and the switch shows the wanted (active) state instead
    /// of the poll's truth (inactive).
    #[gtk::test]
    fn a_failed_toggle_also_clears_an_already_parked_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);
        click(&state, "clock");

        let since = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since,
        });

        // The poll fails first and parks the live intent along with the
        // selection — `state.pending` is now `None`.
        poll_failed(&state);
        assert!(
            state.pending.borrow().is_none(),
            "sanity: the placeholder moves the intent into the park"
        );

        // The toggle's own `StartPlugin` completion lands after the park,
        // and it failed too — the same dead shell.
        on_toggle_result(
            &state,
            since,
            Err(hytte_bus::BusError::Permanent {
                reason: "no such unit".to_owned(),
                dbus_name: None,
            }),
        );

        // The next good poll restores the selection. Applied directly
        // (skipping `apply_state`'s trailing `pump()`) rather than a real
        // `refresh_plugins_soon`-driven poll: `on_toggle_result` above just
        // fired one of its own (a real, doomed `spawn_on_runtime` call, since
        // there is no live `Control` endpoint in this test), and pumping the
        // loop here would risk that background attempt's `Err` completing
        // mid-assertion and re-parking the very selection this test is
        // checking — a timing hazard orthogonal to the fix under test.
        let units = vec![("clock".to_owned(), "inactive".to_owned(), true)];
        apply_plugins(&state, &units, &HashMap::new());
        assert_eq!(state.selected.borrow().as_deref(), Some("clock"));
        assert!(
            state.pending.borrow().is_none(),
            "the failed intent must not be restored alongside the selection"
        );
        assert!(
            !state.detail.switch.is_active(),
            "with the intent dropped, the switch must show the poll's truth (inactive), not \
             the wanted state"
        );

        dismiss(&window);
    }

    /// Mirror of [`a_failed_toggles_completion_does_not_clobber_a_newer_intent`]
    /// for the parked home: a parked intent recorded under a *different*
    /// `since` than the completion names must survive that completion — the
    /// same identity guard, applied to `state.parked`'s carried intent rather
    /// than `state.pending`.
    ///
    /// Falsified by dropping the `since` comparison in the park-inspection
    /// clause (an unconditional clear of `parked.pending` on `Err`): B would
    /// be clobbered by A's late, irrelevant failure.
    #[gtk::test]
    fn a_failed_toggles_completion_does_not_clobber_a_newer_parked_intent() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();
        apply_state(&state, &["clock"], "inactive");
        let window = present(&bin, 640);
        click(&state, "clock");

        // Intent A: the user's first click, then a poll failure parks it.
        let since_a = Instant::now();
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: since_a,
        });
        poll_failed(&state);

        // Intent B replaces A in the park directly, standing in for a second
        // toggle cycle while parked — what matters here is only that the
        // park ends up holding a `since` different from A's.
        let since_b = Instant::now();
        {
            let mut parked = state.parked.borrow_mut();
            let slot = parked
                .as_mut()
                .expect("sanity: the placeholder must have parked a selection");
            slot.pending = Some(PendingToggle {
                plugin_id: "clock".to_owned(),
                wanted: false,
                since: since_b,
            });
        }

        // A's call finally completes — and fails. It must not touch B.
        on_toggle_result(
            &state,
            since_a,
            Err(hytte_bus::BusError::Permanent {
                reason: "timed out".to_owned(),
                dbus_name: None,
            }),
        );

        let parked = state.parked.borrow();
        let intent = parked
            .as_ref()
            .and_then(|p| p.pending.as_ref())
            .expect("intent B must survive intent A's stale completion");
        assert_eq!(
            intent.since, since_b,
            "the surviving parked intent must be B, not wiped by A's stale completion"
        );
        assert!(
            !intent.wanted,
            "the surviving parked intent must still be B's wish"
        );
        drop(parked);

        dismiss(&window);
    }

    // ── Poll ordering (#983) ────────────────────────────────────────────────

    /// The base case: a completion older than the newest already applied is
    /// dropped whole, so neither the detail switch nor the sidebar rows
    /// regress to what it read.
    ///
    /// Driven through [`super::on_poll_result`] rather than
    /// [`super::apply_plugins`] because the generation is exactly what
    /// distinguishes the two completions — the payloads are both perfectly
    /// valid `ListPlugins` replies, and either one is right at the moment its
    /// call was made.
    ///
    /// Falsified by deleting the `state.polls.accept(generation)` guard in
    /// `on_poll_result`: the stale `inactive` then lands and this fails at the
    /// switch assertion, with the status column back on `Stopped`.
    #[gtk::test]
    fn a_stale_poll_result_cannot_regress_the_switch_or_the_rows() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();

        let seed = state.polls.issue();
        on_poll_result(&state, seed, poll_ok(&["clock"], "inactive"));
        pump();
        let window = present(&bin, 640);
        assert!(!state.detail.switch.is_active(), "clock starts stopped");

        // The slow poll is spawned first and completes last — the whole shape
        // of the defect.
        let slow = state.polls.issue();
        let fresh = state.polls.issue();
        on_poll_result(&state, fresh, poll_ok(&["clock"], "active"));
        pump();
        assert!(
            state.detail.switch.is_active(),
            "sanity: the newer poll must be applied normally"
        );
        assert_eq!(status_text(&state, "clock"), "Not connected");

        on_poll_result(&state, slow, poll_ok(&["clock"], "inactive"));
        pump();

        assert!(
            state.detail.switch.is_active(),
            "an out-of-order completion must not bounce the switch back to its stale ActiveState"
        );
        assert_eq!(
            status_text(&state, "clock"),
            "Not connected",
            "…nor regress the sidebar row it also rewrites"
        );
        let snapshot = state.snapshot.borrow();
        assert_eq!(
            snapshot.get("clock").map(|snap| snap.active_state.as_str()),
            Some("active"),
            "the cache every selection change renders from must hold the newest answer"
        );
        drop(snapshot);

        dismiss(&window);
    }

    /// #983's reported sequence, end to end: the user flips the switch on, a
    /// later poll confirms the transition and **legitimately** spends the
    /// #944/#945 latch, and only then does the poll that was spawned before
    /// the flip complete — carrying the pre-toggle truth.
    ///
    /// This is the case the latch cannot cover, which is why it needed its own
    /// mechanism: by the time the stale result lands, `pending` is `None`
    /// because a poll genuinely agreed with the user, so `resolve_pending` has
    /// nothing left to hold the switch with.
    ///
    /// Falsified by deleting the `accept` guard: the switch flips off at the
    /// last assertion, exactly as the issue describes.
    #[gtk::test]
    fn a_late_stale_poll_cannot_rebounce_the_switch_after_the_latch_cleared() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();

        let seed = state.polls.issue();
        on_poll_result(&state, seed, poll_ok(&["clock"], "inactive"));
        pump();
        let window = present(&bin, 640);

        // t=0 — the user flips the switch on; `refresh_plugins_soon` spawns a
        // poll (P0) whose `ListPluginStates` half will drag.
        *state.pending.borrow_mut() = Some(PendingToggle {
            plugin_id: "clock".to_owned(),
            wanted: true,
            since: Instant::now(),
        });
        let p0 = state.polls.issue();

        // t=1.4 — P1 comes back `activating`: `is_running` agrees with the
        // wanted state, so `resolve_pending` retires the intent.
        let p1 = state.polls.issue();
        on_poll_result(&state, p1, poll_ok(&["clock"], "activating"));
        pump();
        assert!(
            state.pending.borrow().is_none(),
            "sanity: a confirming poll must spend the latch — this test is about what happens \
             *after* that"
        );
        assert!(state.detail.switch.is_active());

        // t=2.1 — P2, `active`.
        let p2 = state.polls.issue();
        on_poll_result(&state, p2, poll_ok(&["clock"], "active"));
        pump();
        assert!(state.detail.switch.is_active());

        // t=2.4 — P0 finally completes, three seconds stale.
        on_poll_result(&state, p0, poll_ok(&["clock"], "inactive"));
        pump();

        assert!(
            state.detail.switch.is_active(),
            "the switch must not re-bounce off a poll that predates the toggle"
        );
        assert_eq!(
            status_text(&state, "clock"),
            "Not connected",
            "and the row must not regress to Stopped with it"
        );

        dismiss(&window);
    }

    /// The guard covers the `Err` arm too: a poll that times out at t+3 s is
    /// exactly as stale as one that answers, and letting it through would tear
    /// a live list down to the "Unavailable" placeholder — parking the
    /// selection and blanking the snapshot — a second after a newer poll
    /// proved the shell is answering.
    ///
    /// Falsified by deleting the `accept` guard: the rows go to one
    /// placeholder and this fails at the row-count assertion.
    #[gtk::test]
    fn a_stale_failure_cannot_tear_down_a_live_list() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();

        let seed = state.polls.issue();
        on_poll_result(&state, seed, poll_ok(&["clock", "departures"], "active"));
        pump();
        let window = present(&bin, 640);
        click(&state, "departures");

        let slow = state.polls.issue();
        let fresh = state.polls.issue();
        on_poll_result(&state, fresh, poll_ok(&["clock", "departures"], "active"));
        pump();

        // The slow poll's 3 s timeout finally fires.
        on_poll_result(&state, slow, poll_err());
        pump();

        assert_eq!(
            state.by_id.borrow().len(),
            2,
            "a stale failure must not replace a freshly-confirmed list with the placeholder"
        );
        assert!(state.parked.borrow().is_none(), "…and so must park nothing");
        assert_eq!(
            state.selected.borrow().as_deref(),
            Some("departures"),
            "the user's selection must survive it"
        );
        assert!(
            !state.snapshot.borrow().is_empty(),
            "the snapshot must not be blanked by a superseded failure"
        );

        dismiss(&window);
    }

    /// The other half of the guard, and the mutation that would otherwise pass
    /// silently: it must drop **only** stale results. A run of in-order
    /// completions — the overwhelmingly common case — has to apply every one
    /// of them, or the tab simply stops updating.
    ///
    /// Falsified by making `accept` return `false` unconditionally: the switch
    /// never follows the second poll and this fails immediately.
    #[gtk::test]
    fn in_order_polls_all_apply() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();

        let seed = state.polls.issue();
        on_poll_result(&state, seed, poll_ok(&["clock"], "inactive"));
        pump();
        let window = present(&bin, 640);

        for (active_state, running) in [
            ("active", true),
            ("inactive", false),
            ("active", true),
            ("failed", false),
        ] {
            let generation = state.polls.issue();
            on_poll_result(&state, generation, poll_ok(&["clock"], active_state));
            pump();
            assert_eq!(
                state.detail.switch.is_active(),
                running,
                "an in-order poll reporting {active_state} must be applied"
            );
        }

        dismiss(&window);
    }

    /// The generation is stamped **at spawn**, not at completion — which is
    /// the whole of the ordering guarantee. Taking it inside the completion
    /// closure would make every result the newest one, turning the #983 gate
    /// into a production no-op while the rest of the suite stays green,
    /// because every other test issues its generations by hand and never goes
    /// through [`super::refresh_plugins`].
    ///
    /// That blind spot is the same one the #989 side closed deliberately with
    /// `install_shell_probe` + its tick counter: the five mutations in the PR
    /// body all land inside `on_poll_result` / `accept` / `apply`, and none
    /// targets the *caller*. Supplied by the adversarial review of `32bf073`,
    /// which confirmed it red under exactly that mutation and green here.
    ///
    /// No bus needed: `refresh_plugins` returns as soon as it has spawned, so
    /// `issued` must already have advanced by the time it does.
    #[gtk::test]
    fn a_poll_is_stamped_when_it_is_spawned() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        let before = state.polls.issued.get();
        super::refresh_plugins(&state);
        super::refresh_plugins(&state);

        assert_eq!(
            state.polls.issued.get(),
            before + 2,
            "both polls must take their generation at spawn; a generation taken at \
             completion makes every result the newest and the #983 gate a no-op"
        );
    }

    /// `the_search_path_is_resolved_once_even_across_a_changed_env` drives
    /// `resolved_search_path` with a throwaway `OnceCell`, so it is blind to
    /// the production call site: reverting `refresh_plugins` to
    /// `plugins_json_candidates(&Env::from_process())` — the exact pre-#1270
    /// per-tick shape item 3 exists to delete — leaves all 182 tests green,
    /// `PluginsState::search_path` dead but still constructed (so no
    /// dead-code warning either).
    #[gtk::test]
    fn the_tick_resolves_the_search_path_through_the_tabs_own_latch() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        assert!(
            state.search_path.get().is_none(),
            "nothing resolves the search path before the first tick"
        );
        super::refresh_plugins(&state);
        let first = state
            .search_path
            .get()
            .cloned()
            .expect("refresh_plugins must resolve THROUGH PluginsState::search_path");
        super::refresh_plugins(&state);
        assert_eq!(
            state.search_path.get(),
            Some(&first),
            "and a second tick must reuse it, never re-resolve"
        );
    }

    /// #1286 item 2: even with [`PluginsState::search_path`]'s `OnceCell`
    /// masking the *result*, `refresh_plugins` was still building a fresh
    /// `hytte_config::xdg::Env::from_process()` on every tick and passing it
    /// in, discarded the moment `search_path` was already resolved (#1279
    /// review N3). `PluginsState::env` is meant to make that read-once too.
    ///
    /// Replaces the tab's `env` with one naming a directory the real process
    /// environment does not, then drives `refresh_plugins` through it: if the
    /// tick ever falls back to `Env::from_process()` — the real process
    /// environment — instead of reading [`PluginsState::env`], the resolved
    /// search path lands on the real environment's candidates instead of this
    /// fixture's, and the assertion below fails.
    ///
    /// **Falsify**: change `refresh_plugins` back to
    /// `resolved_search_path(&state.search_path, &hytte_config::xdg::Env::from_process())`
    /// → this test reds (below and in the module comment).
    #[gtk::test]
    fn the_tick_reads_the_tabs_own_env_not_a_fresh_one() {
        adw::init().expect("libadwaita init");
        let (_bin, built) = build_tab();
        let fixture_env = hytte_config::xdg::Env {
            home: None,
            config_home: Some("/should/not/be/seen/config-home".to_owned()),
            config_dirs: Some("/should/not/be/seen/config-dirs".to_owned()),
            state_home: None,
        };
        let state = PluginsState {
            env: Rc::new(fixture_env),
            ..built
        };

        assert!(
            state.search_path.get().is_none(),
            "the candidate list is resolved on the first tick, never at build"
        );
        super::refresh_plugins(&state);
        let resolved = state
            .search_path
            .get()
            .cloned()
            .expect("refresh_plugins must resolve the search path");
        assert_eq!(
            resolved,
            vec![
                PathBuf::from("/should/not/be/seen/config-home/trollshell/plugins.json"),
                PathBuf::from("/should/not/be/seen/config-dirs/trollshell/plugins.json"),
            ],
            "refresh_plugins must resolve THROUGH PluginsState::env, not a \
             fresh Env::from_process() — got the real process environment's \
             candidates instead of the fixture's"
        );
    }

    /// The other side of the `Err` gate: a failure that *is* the newest poll
    /// must still tear the list down to the "Unavailable" placeholder.
    ///
    /// Every other test that asserts that placeholder goes through
    /// [`poll_failed`], which calls `set_placeholder` **directly** and so
    /// bypasses `on_poll_result` entirely — leaving the `Err` arm pinned only
    /// in its *refusing* direction ([`a_stale_failure_cannot_tear_down_a_live_list`]).
    /// No-op that arm and the whole suite stays green while the tab silently
    /// loses its shell-is-down state: a dead shell would leave the last good
    /// list frozen on screen forever. That is half of this module's own
    /// "the gate covers every arm" claim, in the direction that matters to the
    /// user. Supplied by the adversarial review of `32bf073`.
    #[gtk::test]
    fn a_fresh_failure_still_shows_the_unavailable_placeholder() {
        adw::init().expect("libadwaita init");
        let (bin, state) = build_tab();

        let seed = state.polls.issue();
        on_poll_result(&state, seed, poll_ok(&["clock", "departures"], "active"));
        pump();
        let window = present(&bin, 640);
        click(&state, "departures");

        let newest = state.polls.issue();
        on_poll_result(&state, newest, poll_err());
        pump();

        assert!(
            state.by_id.borrow().is_empty(),
            "the newest poll failing must replace the live list with the placeholder"
        );
        assert_eq!(
            state.parked.borrow().as_ref().map(|park| park.id.as_str()),
            Some("departures"),
            "…and park the selection for the next good poll to restore"
        );

        dismiss(&window);
    }

    // ── Transitions-only logging (#1017) ────────────────────────────────────
    //
    // Driven through `on_poll_result` with a real `tracing` subscriber, so
    // these pin the *journal*, not just the state `log_transition`'s own
    // hermetic tests in `main.rs` already cover — a mutation that computes
    // the right `LogTransition` but forgets to act on it (or acts on the
    // wrong arm) is invisible to those.

    /// Run `body` with an `INFO` subscriber installed for this thread, and
    /// return the lines it emitted mentioning `"ListPlugins"`.
    ///
    /// Wraps `crate::test_support::captured_logs` — the capture plumbing
    /// itself (was this file's own `CapturedLog`, byte-identical to
    /// `main.rs`'s and `ai_keys_tab`'s copies) is shared since the #1017
    /// review (LOW 3); this file's own addition is the `"ListPlugins"`
    /// filter, so the pure-`log_transition` tests in `main.rs` and this
    /// file's own poll-ordering tests never show up as noise.
    ///
    /// No callsite warm-up needed (the #1017 review, LOW 4, corrected an
    /// earlier version of this doc comment that claimed one was): `#[gtk::test]`
    /// expands to `gtk::test_synced`, which serialises every test body in
    /// this binary onto one `glib::ThreadPool::exclusive(1)` thread — so the
    /// sibling `poll_err()`/`poll_ok(…)` calls this comment used to worry
    /// about racing are never concurrent with this test, only prior on the
    /// same thread — and `tracing_core::callsite::register_dispatch` rebuilds
    /// every already-registered callsite's `Interest` against the new
    /// `Dispatch` each time `captured_logs` installs one, so a callsite any
    /// earlier test poisoned is repaired before `body` runs anyway (see PR
    /// #1032's review, and `shader_map::tests::counting_events` in
    /// `trollshell/src/plugins/shader_map.rs` for the citations). Verified: a
    /// 100-run campaign of this crate's `system-tests` binary under
    /// `xvfb-run`, no warm-up, 0 failures.
    fn captured_transition_logs(body: impl FnOnce()) -> Vec<String> {
        captured_logs(body)
            .into_iter()
            .filter(|line| line.contains("ListPlugins"))
            .collect()
    }

    /// The defect's shape, and #1017's whole point: N consecutive failing
    /// polls must write **one** `"ListPlugins failed"` line, not N.
    ///
    /// Falsified by deleting the `log_transition`/`last_failing` guard in
    /// `on_poll_result` (reverting its `Err` arm to an unconditional
    /// `tracing::info!`): every failing poll then logs, and this fails with
    /// `left: 5, right: 1`.
    #[gtk::test]
    fn n_failing_polls_emit_exactly_one_failed_line() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        let lines = captured_transition_logs(|| {
            for _ in 0..5 {
                let generation = state.polls.issue();
                on_poll_result(&state, generation, poll_err());
            }
        });

        assert_eq!(
            lines.len(),
            1,
            "five failing polls in a row must write one journal line, not five: {lines:?}"
        );
        assert!(
            lines[0].contains("ListPlugins failed"),
            "the one line must be the failure, not something else: {lines:?}"
        );
    }

    /// down → up → down: two `"ListPlugins failed"` lines (one per down
    /// edge) and one `"ListPlugins recovered"` line (the single up edge),
    /// with every repeated poll in between staying silent.
    ///
    /// Falsified the same way as the test above, and separately by a
    /// mutation that folds `LogTransition::Recovered` into `::None` in
    /// `log_transition` (the recovered count drops to 0) or that logs
    /// `"ListPlugins failed"` unconditionally on every `Err` regardless of
    /// `previous` (the failed count rises to 4).
    #[gtk::test]
    fn down_up_down_logs_two_failures_and_one_recovery() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        let lines = captured_transition_logs(|| {
            // Down (the very first poll ever): 1 failed line.
            on_poll_result(&state, state.polls.issue(), poll_err());
            // Still down: silence.
            on_poll_result(&state, state.polls.issue(), poll_err());
            // Up: 1 recovered line.
            on_poll_result(&state, state.polls.issue(), poll_ok(&["clock"], "active"));
            // Still up: silence.
            on_poll_result(&state, state.polls.issue(), poll_ok(&["clock"], "active"));
            // Down again: 1 more failed line.
            on_poll_result(&state, state.polls.issue(), poll_err());
            // Still down: silence.
            on_poll_result(&state, state.polls.issue(), poll_err());
        });

        let failed = lines
            .iter()
            .filter(|line| line.contains("ListPlugins failed"))
            .count();
        let recovered = lines
            .iter()
            .filter(|line| line.contains("ListPlugins recovered"))
            .count();
        assert_eq!(failed, 2, "one line per down edge: {lines:?}");
        assert_eq!(recovered, 1, "one line for the single up edge: {lines:?}");
        assert_eq!(lines.len(), 3, "…and nothing else: {lines:?}");
    }

    /// The ordinary case — the control-center opened while the shell is up —
    /// must write nothing at all. Neither test above starts from a success,
    /// so a mutation that only fires on the first-ever *successful* poll (an
    /// extra `LogTransition::None if previous.is_none()` arm emitting
    /// "`ListPlugins` recovered") survives the whole suite. Supplied by the
    /// adversarial review of `232a8a2` (#1035, MED 2).
    #[gtk::test]
    fn a_first_poll_that_succeeds_is_silent() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        let lines = captured_transition_logs(|| {
            on_poll_result(&state, state.polls.issue(), poll_ok(&["clock"], "active"));
            on_poll_result(&state, state.polls.issue(), poll_ok(&["clock"], "active"));
        });

        assert!(
            lines.is_empty(),
            "a shell that is up when the window opens must write no line at all: {lines:?}"
        );
    }

    /// A stale, out-of-order completion (#983) must not reach the transition
    /// guard — the claim `on_poll_result`'s own doc and the module doc both
    /// make, which no test above exercises: hoisting the transition block
    /// above `PollGenerations::accept` leaves the suite green while a
    /// superseded poll writes a `ListPlugins failed` line the newest poll has
    /// already disproved. Supplied by the adversarial review of `232a8a2`
    /// (#1035, MED 1).
    #[gtk::test]
    fn a_stale_failure_writes_no_journal_line() {
        adw::init().expect("libadwaita init");
        let (_bin, state) = build_tab();

        let stale = state.polls.issue();
        let newest = state.polls.issue();

        let lines = captured_transition_logs(|| {
            on_poll_result(&state, newest, poll_ok(&["clock"], "active"));
            on_poll_result(&state, stale, poll_err());
        });

        assert!(
            lines.is_empty(),
            "a superseded poll must be dropped before the transition guard: {lines:?}"
        );
    }
}
