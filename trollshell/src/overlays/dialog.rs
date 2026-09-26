//! Layer-shell **plugin dialog** overlay (#1010): a centered surface on the
//! clicked card's output (else the focused one, #1413) holding one plugin's own
//! page.
//!
//! Raised by the effect broker ([`crate::plugins`]) when a **sidebar-mounted**
//! plugin emits `Effect::OpenPage(Page::PluginSelf)`. A bar chip's page keeps
//! opening the drawer beneath the bar, unchanged — see
//! `plugins::effects::page_surface`, which is where that decision is written
//! down. The plugin does not change a line to appear here, and the wire
//! vocabulary does not change: the routing is host-side, on the mount the host
//! already knew.
//!
//! Raised by Mara live-verifying #963, twice: the agents card sits bottom-left
//! in the sidebar and its rows opened a panel top-right in the drawer — *"its
//! very weird the panel opens in the top right after clicking bottom left"*.
//!
//! # Two shipped shapes, nothing new invented
//!
//! * The **window** is the drawer's (`modal.rs`'s `install`): one fullscreen
//!   layer surface whose `gtk::Overlay` *main* child is a transparent
//!   click-catcher (`.ts-modal-catcher`) and whose *overlay* child is a centered
//!   positioner carrying the card. That is how the drawer already gets "a click
//!   outside dismisses" with **no dimming** — the catcher paints nothing. Its
//!   cost is the drawer's cost today: while the dialog is up, pointer events on
//!   that output go to the catcher rather than through it.
//!
//!   The one difference from the drawer is [`Layer::Overlay`] +
//!   [`KeyboardMode::Exclusive`] instead of `Layer::Top` + `OnDemand` (the
//!   secret prompt's and the consent card's choice), so `Esc` always lands and a
//!   plugin `Entry` inside the page gets the keys.
//! * The **body** is the drawer's plugin child — [`plugins::plugin_dialog_slot`]
//!   builds the very tree `plugins::plugin_panel_slot` does, over the dialog's
//!   **own** selection (`dialog_panel_id`) rather than the drawer's. Same
//!   reconciler, same event routing to the plugin's live connection, same #903
//!   destroy story.
//!
//! # What "modal" means here
//!
//! Keyboard-exclusive while it is up, and any outside click closes it. That is
//! all. There is **no backdrop and no dimming** — Annika on #1010, 2026-09-10:
//! *"Let's keep it simple 4 now <3 no dimming <3"*. Other windows stay visible;
//! they are not clickable *through* the catcher while the dialog is up, exactly
//! as with the drawer. When the surface unmaps, niri hands keyboard focus back
//! to the previous toplevel, the way it does after the secret prompt closes.
//!
//! The sidebar stays up underneath (Annika's *"default 🤷‍♀️"* on §8, 2026-09-17):
//! the catcher covers it like everything else, and the first outside click
//! closes the dialog only.
//!
//! # Priority: the shell's own prompts outrank a plugin's page
//!
//! This is the **first** `Layer::Overlay` + `KeyboardMode::Exclusive` surface in
//! the tree that a *plugin* can raise, so the layer inventory is worth stating
//! once — it is derived nowhere else (#1361 review, HIGH-1):
//!
//! | surface | layer | keyboard | raised by |
//! | --- | --- | --- | --- |
//! | `overlays::frame` | Overlay | none | the shell |
//! | `overlays::prompt` (Wi-Fi/VPN secret) | Overlay | **Exclusive** | the shell |
//! | `overlays::consent` (#487 card) | Overlay | **Exclusive** | the shell |
//! | **this** | Overlay | **Exclusive** | `Effect::OpenPage(Page::PluginSelf)` |
//! | `modal` (drawer), `sidebar`, `notifications`, `osd` | Top | OnDemand/none | the shell |
//!
//! Within one layer wlroots/niri stack by **surface creation order**, so a
//! dialog raised after a consent card sits above it — anchored to all four
//! edges, with a real full-surface input region — and both swallows every press
//! aimed at the card and takes the keyboard. A plugin holding `Consent` +
//! `OpenPage` can emit both in one render frame (`EFFECT_BURST`), which would
//! let it make the card *asking about that very plugin* impossible to answer.
//!
//! Two halves close it, and both are enforced here rather than left to call
//! order:
//!
//! * `may_raise` — an `OpenPage(PluginSelf)` arriving while a consent card or
//!   a secret prompt is up is **refused** (dropped with one `debug!`, never
//!   queued; `OpenPage` is one-way, so the plugin is told nothing either way).
//! * [`yield_to_shell_prompt`] — a card or prompt going up takes any live dialog
//!   down first, so the opposite arrival order cannot leave one underneath.
//!
//! **Why the layer stayed `Layer::Overlay`.** `Layer::Top` would give the same
//! guarantee structurally (this surface would then sit below both shell
//! prompts), and it was considered. It was not taken because: the spec fixes
//! `Overlay` + `Exclusive` as §2's whole argument for the surface — `Esc` always
//! lands and a plugin `Entry` gets the keys — and on `Top` that becomes a
//! property of how the compositor treats exclusive keyboard interactivity
//! *below* the overlay layer, which **nothing in CI can check** (no compositor)
//! and which would also re-order this surface against the toasts and the OSD,
//! which nobody asked for. The two rules above are enforceable and testable
//! here; a layer change would trade a pinned rule for an unpinned assumption.
//!
//! The chrome is deliberately **not** the consent card's (#1361 review, HIGH-1,
//! second half): `.ts-dialog-card` had been byte-for-byte `.ts-consent-root` —
//! same surface colour, radius, border and shadow, centered on the same output —
//! so a plugin page could pass for a shell prompt. It now carries its own
//! treatment (a popover-toned surface, a different radius and shadow, and a
//! ruled header whose plugin id is monospaced), which no shell prompt uses.
//!
//! # One window, on the clicked card's output
//!
//! Not one per monitor: [`install`] keeps a connector → [`Monitor`] map and
//! [`open_on_focused`] builds the single window on the output it is handed, the
//! `consent.rs` shape (#499/#517). Since #1413 the broker hands it the output
//! the sidebar card was **clicked** on when a recent click is behind the page
//! (`plugins::effects::page_output`), and niri's focused output only when none
//! is — a card clicked on monitor B opens its page on B even while niri's focus,
//! as the shell last heard of it, is still on A. The card is centred either
//! way; the output is all a click changes. Opening a second plugin's page while
//! one is up swaps the selection in place; opening on a *different* output
//! rebuilds the one window there. Never two windows.
//!
//! Keyboard: niri hands keyboard focus only to layer surfaces on its **active**
//! output, so the `Exclusive` grab — and with it `Esc` — lands once niri's focus
//! is on the dialog's output. A click on a layer surface moves niri's focus to
//! that output, so this is normally immediate; the close button and an outside
//! click dismiss regardless.
//!
//! CSS hooks (`ts-`-prefixed, matching the prompt/consent overlays' shape):
//! - window root: `.ts-dialog` (transparent, like `.ts-prompt`)
//! - click-catcher: `.ts-modal-catcher` (the drawer's own transparent rule)
//! - card: `.ts-dialog-card`
//! - header row: `.ts-dialog-header`, its plugin-id label `.ts-dialog-title`
//!
//! The plugin's tree is reached through the existing `.ts-plugin-panel`
//! **descendant** rules, so plugin styling does not fork between drawer and
//! dialog.

use std::cell::RefCell;
use std::collections::BTreeMap;

use hytte::adw;
use hytte::gtk::{self, gdk, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::niri;
use hytte::ui::{Anchor, Layer, LayerShell, layer_window};
use hytte_plugin_proto::Mount;

use plan::Step;
pub(crate) use plan::may_raise;

use crate::overlays::sidebar::SIDEBAR_WIDTH;
use crate::plugins;
use crate::scale::scale;

// ── Tunables ──────────────────────────────────────────────────────────────────

/// The card's `AdwClamp` cap, in CSS px before [`scale`] — roughly twice the
/// sidebar card width a page opened from a sidebar card was drawn beside.
///
/// A cap rather than a fixed width: it also ends the "a long URL widens the
/// surface" class for good (#967), because the clamp stops the page's natural
/// width from reaching the surface at all.
const DIALOG_MAX_WIDTH: i32 = SIDEBAR_WIDTH * 2;

/// Height budget for the card's scroller, in CSS px before [`scale`].
///
/// Deliberately a constant rather than the drawer's measured
/// `BarGeometry::available_card_height`: this surface hangs off no bar and
/// reserves nothing, so there is no geometry to derive it from — and a centered
/// card that grows to the full height of a tall output is the shape #967 asked
/// the sidebar not to have either. Chosen a little above the drawer's
/// `MIN_CARD_HEIGHT` floor (240) so an ordinary page does not scroll at all.
const DIALOG_MAX_HEIGHT: i32 = 560;

/// The layer-shell namespace this surface announces itself to the compositor as
/// — a niri rule can match it, like `hytte-prompt` / `hytte-consent`.
const DIALOG_NAMESPACE: &str = "hytte-dialog";

// ── The decision half ─────────────────────────────────────────────────────────

/// What one [`open_on_focused`] call *decides*, with nothing performed.
///
/// A module of its own, and a deliberately **hermetic** one: it imports the wire
/// `Mount` and nothing else from this file. That is the point — `close`,
/// `show`, the thread-locals and GTK are all out of scope here, so the sequence
/// below cannot be changed by reaching past it for a side effect. The review's
/// own falsification for HIGH-2 (*"route the rebuild arm through `close()`"*) is
/// therefore not expressible in this module without first adding an import,
/// which is exactly the edit a reviewer would see.
///
/// It exists because [`open_on_focused`] cannot be driven whole — building the
/// surface needs a Wayland compositor speaking `zwlr_layer_shell_v1` — while
/// everything it *decides* can be, on the `broker_open_page_with` shape this PR
/// already uses for the routing rule.
mod plan {
    use hytte_plugin_proto::Mount;

    /// One unit of work [`open_on_focused_with`] asks its caller to perform.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Step {
        /// Retitle the live card in place (the same-output swap).
        Retitle,
        /// Take the previous window down **without** publishing the dismissal.
        TakeWindow,
        /// Build and present the surface on this connector.
        Show(String),
        /// Publish the selection: `Some(mount)` on open, `None` on dismissal.
        Publish(Option<Mount>),
    }

    /// Whether a plugin page may be raised at all right now.
    ///
    /// The shell's own keyboard-exclusive overlays outrank a plugin-raised one:
    /// a dialog is never raised over the #487 consent card or the Wi-Fi/VPN
    /// secret prompt, both `Layer::Overlay` + `KeyboardMode::Exclusive` and both
    /// stacked *below* a surface created after them. A plugin holding both
    /// `Consent` and `OpenPage` could otherwise cover — and out-focus — the card
    /// asking about itself (#1361 review, HIGH-1).
    pub(crate) const fn may_raise(consent_up: bool, prompt_up: bool) -> bool {
        !consent_up && !prompt_up
    }

    /// [`super::open_on_focused`]'s body, with the resolve, the live connector
    /// and the effects injected.
    ///
    /// `shell_grab` is [`may_raise`]'s answer, taken by the caller so this stays
    /// pure; `resolve` is the connector lookup; `live` is the connector the
    /// dialog is currently up on, if any.
    pub(super) fn open_on_focused_with(
        preferred: Option<&str>,
        mount: Mount,
        shell_grab: bool,
        resolve: impl FnOnce(Option<&str>) -> Option<String>,
        live: Option<&str>,
        mut act: impl FnMut(Step),
    ) {
        if shell_grab {
            // Refused, not queued: `OpenPage` is one-way, the plugin is told
            // nothing either way, and a queued page would pop up the instant the
            // human answered the card — which is the same ambush one frame later.
            return;
        }
        let Some(connector) = resolve(preferred) else {
            return;
        };
        if live == Some(connector.as_str()) {
            // Already up on this output: the window, its catcher and its panel
            // slot all stay; only the header text and the selection change.
            act(Step::Retitle);
            act(Step::Publish(Some(mount)));
            return;
        }
        if live.is_some() {
            // A different output. The old window goes down **without** the
            // dismissal being published: a rebuild must republish the selection
            // exactly once, or every connected plugin sees a `SlotVisible`
            // false→true edge for a page that never left the screen, and a
            // sidebar plugin with no sidebar open parks and unparks its poller
            // on exactly that edge.
            act(Step::TakeWindow);
        }
        act(Step::Show(connector));
        act(Step::Publish(Some(mount)));
    }

    #[cfg(test)]
    mod tests {
        use super::{Step, may_raise, open_on_focused_with};
        use hytte_plugin_proto::Mount;

        /// Collect the steps one call decides on.
        fn steps(
            preferred: Option<&str>,
            shell_grab: bool,
            resolvable: Option<&str>,
            live: Option<&str>,
        ) -> Vec<Step> {
            let resolvable = resolvable.map(str::to_owned);
            let mut steps = Vec::new();
            open_on_focused_with(
                preferred,
                Mount::SidebarBottom,
                shell_grab,
                |_| resolvable,
                live,
                |s| steps.push(s),
            );
            steps
        }

        /// HIGH-1: a plugin's page is never raised over the shell's own
        /// keyboard-exclusive surfaces.
        #[test]
        fn a_plugin_page_never_covers_the_shells_own_keyboard_grab() {
            assert!(may_raise(false, false));
            assert!(
                !may_raise(true, false),
                "the consent card may be asking about this very plugin"
            );
            assert!(
                !may_raise(false, true),
                "a Wi-Fi/VPN secret is being typed into a surface this would out-focus"
            );
            assert!(!may_raise(true, true));
        }

        /// …and the refusal is a refusal, not a deferral: with a card up the
        /// call performs **nothing**, where the identical call without one shows
        /// the page. The control half is what makes this a test of the gate
        /// rather than of the `resolve` stub.
        ///
        /// This is the same-frame burst case (`EFFECT_BURST = 8`): a plugin
        /// holding `Consent` + `OpenPage` can emit `RequestConsent` and
        /// `OpenPage(PluginSelf)` on one render frame, and `route_render` pushes
        /// them to the broker in order — so by the time the page is brokered the
        /// card is already up, which is exactly `shell_grab = true` here.
        ///
        /// **Falsification:** drop the `if shell_grab` early return → the first
        /// assertion reds with the control's step list.
        #[test]
        fn a_page_raised_while_the_shell_is_asking_is_refused_not_queued() {
            assert_eq!(
                steps(Some("DP-1"), true, Some("DP-1"), None),
                Vec::new(),
                "a page must not be raised — or queued — over a consent card or a secret prompt",
            );
            assert_eq!(
                steps(Some("DP-1"), false, Some("DP-1"), None),
                vec![
                    Step::Show("DP-1".to_owned()),
                    Step::Publish(Some(Mount::SidebarBottom)),
                ],
                "control: the same call with nothing up shows the page",
            );
        }

        /// **The #1010 commit-2 rule.** Re-opening on a *different* output
        /// rebuilds the one window; it must republish the selection **once** and
        /// never publish the dismissal on the way, or every connected plugin
        /// sees a `SlotVisible` false→true edge for a page that never left the
        /// screen — and a sidebar plugin with no sidebar open parks and unparks
        /// its poller on exactly that edge.
        ///
        /// **Falsification:** emit `Step::Publish(None)` beside the
        /// `Step::TakeWindow` (what routing the arm through `close()` means in
        /// step terms) → this reds. Routing it through `close()` *literally* is
        /// not expressible here: this module imports no actor — see its docs.
        #[test]
        fn a_rebuild_on_another_output_never_publishes_a_dismissal() {
            assert_eq!(
                steps(Some("DP-2"), false, Some("DP-2"), Some("DP-1")),
                vec![
                    Step::TakeWindow,
                    Step::Show("DP-2".to_owned()),
                    Step::Publish(Some(Mount::SidebarBottom)),
                ],
            );
        }

        /// The same output swaps in place: no window work at all, so no scope
        /// churn and no second surface.
        #[test]
        fn a_second_open_on_the_same_output_swaps_in_place() {
            assert_eq!(
                steps(Some("DP-1"), false, Some("DP-1"), Some("DP-1")),
                vec![Step::Retitle, Step::Publish(Some(Mount::SidebarBottom))],
            );
        }

        /// No monitor mounted → nowhere to show it, and nothing is published (a
        /// stale selection must not be lowered by a call that showed nothing).
        #[test]
        fn nowhere_to_show_it_publishes_nothing() {
            assert_eq!(steps(None, false, None, None), Vec::new());
        }
    }
}

// ── Thread-local state ────────────────────────────────────────────────────────

/// The live dialog: its window, the connector it was built on so a second open
/// on the *same* output can swap the selection instead of rebuilding, and the
/// focused-output subscription that takes it down when the user's attention
/// moves to another screen.
struct LiveDialog {
    window: gtk::Window,
    connector: String,
    /// The `niri::focused_output()` subscription for this window (#1361 review,
    /// MEDIUM-3). Aborted by [`LiveDialog::dismantle`], so a dialog never leaves
    /// a subscription behind and a stale one can never act on a later window.
    focus_sub: glib::JoinHandle<()>,
}

impl LiveDialog {
    /// Take this dialog's surface down: stop watching the focused output, then
    /// close the window. Does **not** publish anything — every caller decides
    /// that for itself, which is what keeps a rebuild from publishing a
    /// dismissal it did not mean.
    fn dismantle(self) {
        self.focus_sub.abort();
        // Closing destroys the panel slot inside, whose own `connect_destroy`
        // aborts its render subscription and releases the panel scope it was
        // showing — refcounted across children (#921), so a drawer showing the
        // same plugin keeps its renderer instances.
        self.window.close();
    }
}

thread_local! {
    /// The single live dialog window, if any. One globally — not one per
    /// monitor — like `consent.rs`'s prompt.
    static DIALOG: RefCell<Option<LiveDialog>> = const { RefCell::new(None) };

    /// Mounted monitors keyed by `Monitor.connector()`, so [`open_on_focused`]
    /// can build on niri's focused output. Re-keyed on each hot-plug via
    /// [`close_all`] + [`install`], exactly as `consent.rs` does.
    ///
    /// A `BTreeMap` rather than a `HashMap` (#1361 review, LOW) so the
    /// "focused output unknown" fallback — the first entry — is the same output
    /// every time instead of whichever one the hash order happened to yield;
    /// two consecutive opens picking *different* screens would rebuild the
    /// window for nothing.
    static MONITORS: RefCell<BTreeMap<String, Monitor>> = const { RefCell::new(BTreeMap::new()) };
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Register `monitor` as a candidate output for plugin dialogs. Called per
/// monitor from `main.rs`'s `monitors_changed` loop, like
/// [`crate::overlays::consent::install`]; the focused-output *tracker* lives in
/// the shared [`crate::components::focused_output`] cache, so this only
/// maintains the connector → [`Monitor`] map [`open_on_focused`] resolves
/// against.
pub fn install(monitor: &Monitor) {
    let Some(connector) = monitor.connector() else {
        tracing::warn!(
            description = ?monitor.description(),
            "dialog::install: monitor has no connector name; skipping"
        );
        return;
    };
    // Tail-expression `insert` + an outer `drop`, the #643 shape the other four
    // `install` sites use: the displaced `Monitor`'s drop is a `GdkMonitor`
    // refcount decrement, which emits nothing, but the shape is uniform so
    // nobody has to re-derive which of them were "the real ones".
    drop(MONITORS.with(|m| m.borrow_mut().insert(connector, monitor.clone())));
}

/// Close any live dialog and forget the mounted monitors before a hot-plug
/// rebuild, mirroring `consent::close_all`. The re-install re-keys cleanly.
///
/// **A hot-plug destroys an open page**, and not only an unplug: `main.rs` tears
/// every per-monitor surface down on each `monitors_changed`, so plugging a
/// second monitor in takes the page the user was reading with it. That matches
/// what `modal::close_all` does to the drawer, and it is deliberate — but it is
/// visible, so it says so in the journal rather than happening silently
/// (#1361 review, MEDIUM-3). The dismissal is published exactly once, by
/// [`close`], which is what keeps the plugin's `SlotVisible` from being left
/// pinned by a surface that no longer exists.
pub fn close_all() {
    if DIALOG.with(|d| d.borrow().is_some()) {
        tracing::debug!("plugin dialog closed: the monitor set changed (hot-plug rebuild)");
    }
    close();
    MONITORS.with(|m| m.borrow_mut().clear());
}

/// Open `plugin_id`'s own page in the dialog on the `preferred` output, falling
/// back to any mounted one. Called from the plugin effect broker when a
/// **sidebar-mounted** plugin emits `Effect::OpenPage(Page::PluginSelf)`, with
/// `preferred` the output the card was clicked on when a recent click is behind
/// the page, else niri's focused output (#1413; the name predates that).
/// GTK-main-thread only (the broker runs there).
///
/// `mount` is the producing plugin's mount, carried through for the
/// slot-visibility contribution (#1010 §4) — a page that is up must not let its
/// plugin park the poller feeding it.
///
/// Opening while a dialog is already up on the same output **swaps the
/// selection** (same plugin: a no-op re-publish); on a different output the one
/// window is rebuilt there. There is never a second window.
///
/// No-op with one `debug!` line when no monitor is mounted at all — there is
/// nowhere to show it, and the plugin is told nothing either way (`OpenPage` is
/// a one-way effect).
pub fn open_on_focused(preferred: Option<&str>, plugin_id: &str, mount: Mount) {
    // The two refusals log here rather than in `plan`, which stays pure.
    let shell_grab = !may_raise(
        crate::overlays::consent::is_up(),
        crate::overlays::prompt::is_up(),
    );
    if shell_grab {
        tracing::debug!(
            plugin = %plugin_id,
            "plugin dialog refused: the shell is asking the user something (#1361 HIGH-1)",
        );
        return;
    }
    if resolve_connector(preferred).is_none() {
        tracing::debug!(plugin = %plugin_id, "plugin dialog: no monitor to show on");
        return;
    }

    plan::open_on_focused_with(
        preferred,
        mount,
        shell_grab,
        resolve_connector,
        live_connector().as_deref(),
        |step| perform(step, plugin_id),
    );
}

/// Carry out one [`Step`] the plan decided on. The only place in this module
/// that both decides nothing and touches everything.
fn perform(step: Step, plugin_id: &str) {
    match step {
        Step::Retitle => set_title(plugin_id),
        Step::TakeWindow => {
            if let Some(live) = take_live() {
                live.dismantle();
            }
        }
        Step::Show(connector) => {
            if let Some(monitor) = MONITORS.with(|m| m.borrow().get(&connector).cloned()) {
                show(&monitor, &connector, plugin_id);
            }
        }
        Step::Publish(Some(mount)) => publish_selection(plugin_id, mount),
        Step::Publish(None) => publish_dismissal(),
    }
}

/// Dismiss the dialog: take the window down, clear the dialog's selection and
/// drop its slot-visibility contribution.
///
/// The single close path — `Esc`, the close button, a click on the catcher and
/// the `dialog-close` `GAction` all land here, so there is one place that
/// decides what dismissal *does* (the `consent.rs` `resolve` argument). A no-op
/// when nothing is up, which is what makes the `GAction` safe to bind blind.
///
/// Clearing **only** the dialog's own selection is the §2.1 contract: the
/// drawer's `active_panel_id` is never touched here, so a drawer showing another
/// plugin's page on another monitor keeps showing it.
pub fn close() {
    // Bind-then-act (#631): a GTK call made inside an `if let` on a `RefCell`
    // scrutinee would hold the borrow across it.
    let Some(live) = take_live() else {
        return;
    };
    live.dismantle();
    publish_dismissal();
}

/// Take the dialog down because the **shell** needs the screen and the keyboard:
/// a #487 consent card or a Wi-Fi/VPN secret prompt is about to be raised
/// (#1361 review, HIGH-1).
///
/// The other half of `may_raise`, covering the opposite arrival order — a
/// dialog already up when a card goes up would otherwise sit *above* it (same
/// layer, later surface) and swallow every press aimed at it. A no-op when no
/// dialog is up, so both call sites can call it unconditionally.
///
/// `raised_by` names the surface taking over, for the journal line: the page
/// vanishing under a prompt is a visible thing happening to the user, and this
/// is the only record of why.
pub fn yield_to_shell_prompt(raised_by: &str) {
    let Some(live) = take_live() else {
        return;
    };
    tracing::debug!(
        raised_by,
        "plugin dialog closed: the shell is raising its own keyboard-exclusive prompt",
    );
    live.dismantle();
    publish_dismissal();
}

/// Take the dialog down because the plugin whose page it shows **disconnected**
/// (#1361 review, LOW).
///
/// Without this the page blanks (`render_active_panel`'s `None` arm) but
/// `dialog_panel_id` and the visibility contributor stay set, so every plugin in
/// that sidebar family goes on being told `SlotVisible = true` until a human
/// dismisses an empty card. Called from the scope releaser's departure sweep.
///
/// Publishes the dismissal **unconditionally once the selection matches**, not
/// only when a window is up: the selection is the thing being cleared, and a
/// surface that has somehow already gone must not leave it pinned.
pub(crate) fn close_for_departed(plugin_id: &str) {
    // The departure sweep can be driven without a live host — `pump_tests`
    // drives the scope releaser directly — and the accessor below `.expect()`s
    // the handles. Same guard, same reason, as
    // `pump::request_preem_repaint_all_when_live`.
    if !crate::plugins::host_is_live() {
        return;
    }
    if crate::plugins::dialog_panel().as_deref() != Some(plugin_id) {
        return;
    }
    tracing::debug!(
        plugin = %plugin_id,
        "plugin dialog closed: the plugin whose page it showed disconnected",
    );
    if let Some(live) = take_live() {
        live.dismantle();
    }
    publish_dismissal();
}

/// Take the live dialog out of its slot, releasing the borrow before the caller
/// touches GTK (#631). The single taker — every take-down path goes through it,
/// so "at most one window" is a property of this module rather than of its
/// callers.
fn take_live() -> Option<LiveDialog> {
    DIALOG.with(|d| d.borrow_mut().take())
}

/// The connector the dialog is currently up on, if any.
fn live_connector() -> Option<String> {
    DIALOG.with(|d| d.borrow().as_ref().map(|live| live.connector.clone()))
}

/// Clear the dialog's selection and drop its slot-visibility contribution —
/// **one** visibility edge, whatever took the window down.
///
/// Clearing only the dialog's own selection is the §2.1 contract: the drawer's
/// `active_panel_id` is never touched here, so a drawer showing another plugin's
/// page on another monitor keeps showing it.
fn publish_dismissal() {
    crate::plugins::set_dialog_panel(None);
    crate::plugins::set_dialog_visibility(None);
}

/// Whether a dialog is currently up. Read by this module's own tests and by
/// `commands.rs`'s `dialog-close` test; nothing shipped reads it — the shell's
/// own surfaces coordinate through the selection handle, not through this.
#[cfg(all(test, feature = "system-tests"))]
pub(crate) fn is_open() -> bool {
    DIALOG.with(|d| d.borrow().is_some())
}

/// Put a live dialog on `connector` showing `plugin_id`, with the plugin host's
/// handles installed — the state every take-down path acts on.
///
/// Everything except the layer surface itself is real: the window is a plain
/// `gtk::Window` (nothing in this tree can build a layer one in a test, see
/// [`gtk_tests`]), but the selection and the visibility contributor are
/// published through the shipped setters, so a take-down that forgets one of
/// them is visible to the caller. Resets the slot-visibility edge counter, so a
/// test can assert the dismissal costs exactly one.
///
/// `pub(crate)` so `commands.rs`'s `dialog-close` test can seed the same state
/// rather than grow a second, differently-wrong copy of it.
#[cfg(all(test, feature = "system-tests"))]
pub(crate) fn seed_for_test(connector: &str, plugin_id: &str) -> gtk::Window {
    hytte::reactive::registry::reset_for_tests();
    crate::plugins::install_test_handles();

    let window = gtk::Window::new();
    window.set_child(Some(&build_root(plugin_id, &gtk::Label::new(Some("page")))));
    DIALOG.with(|d| {
        *d.borrow_mut() = Some(LiveDialog {
            window: window.clone(),
            connector: connector.to_owned(),
            // A real `JoinHandle` with nothing behind it: the production one
            // watches `niri::focused_output()`, which needs the niri service.
            focus_sub: glib::MainContext::default().spawn_local(std::future::ready(())),
        });
    });
    crate::plugins::set_dialog_panel(Some(plugin_id));
    crate::plugins::set_dialog_visibility(Some(Mount::SidebarBottom));
    crate::plugins::reset_visibility_edges();
    window
}

// ── Internals ─────────────────────────────────────────────────────────────────

/// The focused output's connector, or the first mounted one —
/// `modal::open_on_focused`'s fallback rule, so an unknown or absent focused
/// output still shows the page somewhere rather than silently dropping the
/// click. Deterministic since the map became a `BTreeMap`.
fn resolve_connector(preferred: Option<&str>) -> Option<String> {
    MONITORS.with(|m| {
        let monitors = m.borrow();
        preferred
            .filter(|key| monitors.contains_key(*key))
            .map(str::to_owned)
            .or_else(|| monitors.keys().next().cloned())
    })
}

/// Publish the dialog's selection into the plugin host: which plugin's panel the
/// dialog child renders, and the mount whose slot-visibility aggregate this
/// dialog holds up while it is on screen.
fn publish_selection(plugin_id: &str, mount: Mount) {
    crate::plugins::set_dialog_panel(Some(plugin_id));
    crate::plugins::set_dialog_visibility(Some(mount));
}

/// Build and present the one dialog window on `monitor`, showing `plugin_id`'s
/// page. The caller has already taken any previous window down.
fn show(monitor: &Monitor, connector: &str, plugin_id: &str) {
    let window = layer_window(monitor)
        .layer(Layer::Overlay)
        // All four edges, so the inner `Overlay` fills the screen and the
        // catcher really does catch every outside click — the drawer's
        // fullscreen-surface shape (`modal::build_drawer_window`).
        .anchor(Anchor::Top)
        .anchor(Anchor::Bottom)
        .anchor(Anchor::Left)
        .anchor(Anchor::Right)
        .exclusive(false)
        .keyboard_mode(KeyboardMode::Exclusive)
        .namespace(DIALOG_NAMESPACE)
        .build();
    window.add_css_class("ts-dialog");
    // Ignore other layer surfaces' exclusive zones so the card centers on the
    // true screen, not on whatever the bar left over (the drawer's `-1`).
    window.set_exclusive_zone(-1);

    window.set_child(Some(&build_root(plugin_id, &plugins::plugin_dialog_slot())));
    wire_escape(&window);

    window.set_visible(true);
    window.present();

    DIALOG.with(|d| {
        *d.borrow_mut() = Some(LiveDialog {
            window,
            connector: connector.to_owned(),
            focus_sub: watch_focused_output(connector),
        });
    });
}

/// Take the dialog down when niri's focused output moves off the screen it was
/// built on (#1361 review, MEDIUM-3).
///
/// A layer surface with `KeyboardMode::Exclusive` holds the keyboard for **its
/// own** output, so once the focus moves the card keeps the grab on a screen the
/// user has left: `Esc` no longer reaches it, and the only dismissals left are a
/// click on the old output's catcher and the `dialog-close` bind. Rather than
/// move the surface — which would mean rebuilding it, losing the page's preem
/// animation state, and doing so on every glance at another screen — the page is
/// dismissed, which is what a click anywhere outside it already does.
///
/// Subscribed per window and aborted with it, so there is at most one of these
/// alive. The connector is re-checked inside, so an abort that loses a race can
/// still not act on a *later* window.
///
/// What closes it is a **move**, decided by [`FocusWatch`], not merely a
/// focused output that differs (#1413). Since the dialog opens on the output the
/// card was *clicked* on, it can be built while niri's focused output — as the
/// shell last heard of it — is still another one: niri moves its focus to a
/// clicked output, but the shell hears of that over IPC, and the plugin round
/// trip that produced the page can beat it. Under the old "any other named
/// output" rule the subscription's first value, that stale focus, took the page
/// down the instant it went up.
///
/// The watch is **seeded** with the focus the shell knew when the window was
/// built — [`crate::components::focused_output::current`], the cache the broker
/// read to place the page — so a move that lands between `show` and the
/// subscription's first value is a move, not a baseline (#1416 review, L3).
fn watch_focused_output(connector: &str) -> glib::JoinHandle<()> {
    let built_on = connector.to_owned();
    let mut watch = FocusWatch::new(connector, crate::components::focused_output::current());
    glib::MainContext::default().spawn_local(niri::focused_output().for_each(move |focused| {
        if watch.observe(focused) && live_connector().as_deref() == Some(&*built_on) {
            tracing::debug!(
                output = %built_on,
                "plugin dialog closed: the focused output moved off the screen it was built on",
            );
            close();
        }
        std::future::ready(())
    }))
}

/// The whole fold [`watch_focused_output`] runs over `niri::focused_output()`:
/// the output the dialog was built on and the last focus seen, fed one value at
/// a time (#1416 review, MEDIUM 1). A value of its own, rather than two
/// variables captured by the subscription closure, so the fold — not only the
/// one-step [`focus_moved_off`] rule — is under test: comparing a value against
/// *itself* (updating `last` before the comparison) never closes anything, and
/// only a sequence can see that.
struct FocusWatch {
    /// The connector the dialog was built on.
    built_on: String,
    /// The focused output seen last — seeded with the one the shell knew when
    /// the dialog was built; `None` when niri reported no focused workspace.
    last: Option<String>,
}

impl FocusWatch {
    /// A watch for a dialog built on `built_on` while the shell believed niri's
    /// focus was on `known_at_build`.
    fn new(built_on: &str, known_at_build: Option<String>) -> Self {
        Self {
            built_on: built_on.to_owned(),
            last: known_at_build,
        }
    }

    /// Feed the next focused-output value; whether it takes the dialog down.
    fn observe(&mut self, focused: Option<String>) -> bool {
        let moved_off = focus_moved_off(&self.built_on, self.last.as_deref(), focused.as_deref());
        self.last = focused;
        moved_off
    }
}

/// Whether niri's focused output, now `focused`, has just **moved off** the
/// output the dialog was built on (`built_on`), given the value before it
/// (`last`).
///
/// Two conditions, both needed:
///
/// * **A named output other than `built_on`.** `None` (niri startup, no focused
///   workspace) is not "somewhere else", and focus arriving *on* the dialog's
///   own screen is the opposite of leaving it.
/// * **A change from `last`.** Since #1413 the dialog can be built while the
///   focus is on another output — the card was clicked on a screen niri's
///   focus had not yet reached as far as the shell knew (or never will, on a
///   compositor that does not move focus on a click) — and that focus is not a
///   move. `niri::focused_output` also re-emits the same output on every
///   workspace change, and a repeat is not a move either.
///
/// So a dialog built on `B` under focus on `A` stays up while the focus stays
/// on `A` or follows the click to `B`, and goes down the moment it moves from
/// wherever it was to any screen other than `B`. For a dialog built where the
/// shell believed the focus was (`last` seeded with `built_on`) the answer is
/// the pre-#1413 rule's on every sequence: until the first value naming
/// another output, every value is `built_on` or `None`, so that first one is
/// always a change from `last` and closes it.
fn focus_moved_off(built_on: &str, last: Option<&str>, focused: Option<&str>) -> bool {
    focused.is_some_and(|out| out != built_on && Some(out) != last)
}

/// Which key presses dismiss the dialog: `Escape`, and nothing else.
///
/// A named predicate rather than a comparison inline in the handler so the rule
/// is pinnable on its own — the handler below is reachable from a test (see
/// `gtk_tests`), but only by emitting the signal, and a predicate that reads
/// as "`Esc` closes, every other key reaches the plugin's page" is worth being
/// able to state.
fn dismisses(key: gdk::Key) -> bool {
    key == gdk::Key::Escape
}

/// `Esc` dismisses, wherever the focus sits inside the surface — the prompt
/// overlay's controller, on the window so a plugin `Entry` holding focus does
/// not swallow it.
///
/// Every other key `Proceed`s, so typing into a plugin's `Entry` inside the page
/// works exactly as it does in the drawer.
fn wire_escape(window: &gtk::Window) {
    let key_ctrl = gtk::EventControllerKey::new();
    key_ctrl.connect_key_pressed(|_, key, _, _| {
        if dismisses(key) {
            close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(key_ctrl);
}

/// The dialog's whole widget tree: a `gtk::Overlay` whose main child is the
/// transparent click-catcher and whose overlay child is the centered card
/// (header + `body`).
///
/// Split out of [`show`] — and taking `body` as a parameter — for
/// `plugins::region::build_panel_child`'s reason: [`plugins::plugin_dialog_slot`]
/// `.expect()`s a registered `PluginHandles` out of the thread-local registry,
/// which a `#[gtk::test]` has no booted `App` to provide, and [`show`] itself
/// needs a live [`Monitor`] and a Wayland compositor to build a layer surface
/// against. Everything this function assembles is the part a test can drive, and
/// it is the part with a shape to get wrong.
fn build_root(plugin_id: &str, body: &impl IsA<gtk::Widget>) -> gtk::Overlay {
    // Transparent click-catching background: any press dismisses. The drawer's
    // class, so it inherits the one transparent rule rather than a second copy
    // of it.
    let catcher = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    catcher.add_css_class("ts-modal-catcher");
    catcher.set_hexpand(true);
    catcher.set_vexpand(true);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(0);
    gesture.connect_pressed(|_, _, _, _| close());
    catcher.add_controller(gesture);

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&catcher));
    overlay.add_overlay(&build_card(plugin_id, body));
    overlay
}

/// The card itself: header row over the page, clamped in width and bounded in
/// height, centered on the surface.
fn build_card(plugin_id: &str, body: &impl IsA<gtk::Widget>) -> gtk::Widget {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    column.add_css_class("ts-dialog-card");
    column.append(&build_header(plugin_id));

    // The #967 shape: the page scrolls inside a bounded card instead of growing
    // the surface. `hscrollbar_policy = Never` propagates the child's minimum
    // width through (the clamp caps the *natural*), so a page with a wide
    // minimum is still readable rather than silently clipped.
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_width(true)
        .propagate_natural_height(true)
        .max_content_height(scale(DIALOG_MAX_HEIGHT))
        .child(body)
        .build();
    column.append(&scroller);

    let clamp = adw::Clamp::builder()
        .maximum_size(scale(DIALOG_MAX_WIDTH))
        .tightening_threshold(scale(DIALOG_MAX_WIDTH))
        .child(&column)
        .build();
    // Centered on the fullscreen surface — the whole point of the overlay
    // (`Align::Center` on both axes, where the drawer aligns to the bar's
    // corner). `Align::Center` also stops the card from expanding to fill.
    clamp.set_halign(gtk::Align::Center);
    clamp.set_valign(gtk::Align::Center);
    clamp.upcast()
}

/// The header: the plugin **id**, dim, and a close button.
///
/// The id and not a display name, because there is no display name to use: the
/// manifest carries none and `SlotRender` does not either, and adding one is a
/// proto change this cut rules out (§9). The plugin's own panel tree carries its
/// title. Annika took the default on §8 (2026-09-17): *"default 🤷‍♀️"* — the
/// plugin id, dim.
fn build_header(plugin_id: &str) -> gtk::Box {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    header.add_css_class("ts-dialog-header");

    let title = gtk::Label::new(Some(plugin_id));
    title.add_css_class("ts-dialog-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    header.append(&title);

    let close_btn = gtk::Button::from_icon_name("window-close-symbolic");
    close_btn.add_css_class("flat");
    close_btn.add_css_class("circular");
    close_btn.set_tooltip_text(Some("Close"));
    close_btn.connect_clicked(|_| close());
    header.append(&close_btn);

    header
}

/// Retitle the live dialog's header in place, for the swap-the-selection path.
///
/// Walks to the label rather than stashing a widget handle in the thread-local:
/// the alternative is a second strong clone of a widget the window already owns,
/// living for as long as the dialog does — the pin shape `nix/lint-bind-pins.py`
/// exists to catch. The walk is a fixed path over a tree this file builds three
/// functions up, and it is inert (no retitle, one `debug!`) if any hop fails.
fn set_title(plugin_id: &str) {
    let label = DIALOG.with(|d| {
        d.borrow()
            .as_ref()
            .and_then(|live| live.window.child())
            .and_then(|root| root.downcast::<gtk::Overlay>().ok())
            .and_then(|overlay| overlay.last_child())
            .and_then(|clamp| clamp.downcast::<adw::Clamp>().ok())
            .and_then(|clamp| clamp.child())
            .and_then(|column| column.first_child())
            .and_then(|header| header.first_child())
            .and_then(|title| title.downcast::<gtk::Label>().ok())
    });
    if let Some(label) = label {
        label.set_text(plugin_id);
    } else {
        tracing::debug!(
            plugin = %plugin_id,
            "plugin dialog: header label not found; title left as it was"
        );
    }
}

// ── Hermetic tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{FocusWatch, focus_moved_off};

    /// [`focus_moved_off`]'s truth table (#1361 MEDIUM-3, re-scoped by #1413):
    /// the dialog goes down when niri's focus **moves** to another screen, and
    /// not merely because the focus sat on another screen when it went up — the
    /// case a click on a card on `B` with the focus still on `A` produces. The
    /// dialog is on `B` in every row; `last` is the value before (the seed, for
    /// the first one).
    ///
    /// **Falsification:** restore the pre-#1413 rule
    /// (`focused.is_some_and(|out| out != built_on)`, ignoring `last`) → the
    /// "focus still on A" row reds, which is the dialog closing the instant it
    /// opens on the clicked monitor; drop the
    /// `out != built_on` term → "focus follows the click to B" reds.
    #[test]
    fn the_dialog_closes_when_the_focus_moves_off_its_screen_not_when_it_starts_elsewhere() {
        let rows = [
            (Some("B"), Some("A"), true, "focus moved from B to A"),
            (Some("B"), Some("B"), false, "a repeat of B"),
            (Some("B"), None, false, "no focus is not somewhere else"),
            (
                Some("A"),
                Some("A"),
                false,
                "built on B for a click there, focus still on A: a repeat of A",
            ),
            (
                Some("A"),
                Some("B"),
                false,
                "focus follows the click to B",
            ),
            (
                Some("A"),
                Some("C"),
                true,
                "focus moved from A to a third screen",
            ),
            (None, Some("A"), true, "focus appeared on another screen"),
            (
                None,
                Some("B"),
                false,
                "focus appeared on the dialog's screen",
            ),
            (None, None, false, "still no focus"),
        ];
        for (last, focused, closes, case) in rows {
            assert_eq!(focus_moved_off("B", last, focused), closes, "{case}");
        }
    }

    /// The **fold**: [`FocusWatch`] fed a whole sequence, seeded with the focus
    /// the shell knew when the dialog was built (#1416 review, MEDIUM 1 and
    /// L3). The one-step table above cannot see how `last` is carried from
    /// value to value; this can.
    ///
    /// **Falsification:** update `last` before comparing
    /// (`self.last.clone_from(&focused);` first in `observe`, the review's
    /// clippy-clean W1c) → every value is compared with itself, nothing ever
    /// closes, and every sequence that expects a `true` reds; treat the first
    /// value as the baseline instead of seeding (this PR's first cut) → the two
    /// L3 sequences red; seed with `None` instead of the focus known at build →
    /// the second sequence reds at its first `A`; never update `last` → the
    /// second reds at its final `A`, a move from `B` compared with the seed.
    #[test]
    fn the_watcher_closes_on_a_move_seen_across_values() {
        let seq = |seed: Option<&str>, values: &[Option<&str>]| -> Vec<bool> {
            let mut watch = FocusWatch::new("B", seed.map(str::to_owned));
            values
                .iter()
                .map(|value| watch.observe(value.map(str::to_owned)))
                .collect()
        };
        // #1361 MEDIUM-3: built on the focused output, then the focus leaves.
        assert_eq!(seq(Some("B"), &[Some("B"), Some("A")]), [false, true]);
        // #1413: built on B for a click there while the shell still believed
        // the focus was on A; it follows the click to B, then leaves.
        assert_eq!(
            seq(Some("A"), &[Some("A"), Some("B"), Some("B"), Some("A")]),
            [false, false, false, true],
        );
        // L3: a move that lands between building the window and the
        // subscription's first value is a move, not the baseline.
        assert_eq!(seq(Some("B"), &[Some("A")]), [true]);
        assert_eq!(seq(Some("A"), &[Some("C")]), [true]);
        // No focus known at build: focus appearing elsewhere closes it, as the
        // pre-#1413 rule did.
        assert_eq!(seq(None, &[None, Some("A")]), [false, true]);
    }

    /// …and the real watcher **is** seeded with the focus the shell knew at
    /// build time. A source scan, on this module's own
    /// `the_shell_prompts_yield_the_dialog_before_they_raise` precedent:
    /// `watch_focused_output` subscribes to `niri::focused_output()`, which needs
    /// the niri service, inside a window only a Wayland compositor can build,
    /// so no test reaches the call; the sequence test above pins what a seed
    /// does, and this pins that the one shipped call passes it.
    ///
    /// **Falsification:** seed with `None`
    /// (`FocusWatch::new(connector, None)`) → this reds; the sequence test
    /// alone stays green under that mutation (measured).
    #[test]
    fn the_real_watcher_is_seeded_with_the_focus_known_at_build() {
        let src = include_str!("dialog.rs");
        let body: String = src
            .split("fn watch_focused_output(")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .expect("dialog.rs defines watch_focused_output")
            .split_whitespace()
            .collect();
        assert!(
            body.contains("FocusWatch::new(connector,crate::components::focused_output::current())"),
            "watch_focused_output must seed its FocusWatch with the focused output the shell \
             knew when the window was built (#1416 review, L3)",
        );
    }

    /// HIGH-1's second half is only a rule if the two shell surfaces actually
    /// call it. A source scan, on `consent.rs`'s own
    /// `request_never_names_a_decision_of_its_own` precedent: the *behaviour* of
    /// [`super::yield_to_shell_prompt`] is pinned in `gtk_tests`, but
    /// `consent::request` and `prompt::show_prompt` are not reachable from any
    /// test — both need a live `Monitor` and a compositor to build their own
    /// layer surface — so this is what stops the rule from living in a function
    /// nobody calls.
    ///
    /// **Falsification:** delete either call → this reds, naming the file.
    #[test]
    fn the_shell_prompts_yield_the_dialog_before_they_raise() {
        for (file, src) in [
            ("consent.rs", include_str!("consent.rs")),
            ("prompt.rs", include_str!("prompt.rs")),
        ] {
            assert!(
                src.contains("dialog::yield_to_shell_prompt("),
                "{file} must take any plugin dialog down before raising its own \
                 keyboard-exclusive surface (#1361 HIGH-1)",
            );
        }
    }
}

// ── GTK integration tests (need a display → gated to `system-tests`) ─────────

/// The dialog's **shape**, driven through the same builders `show` mounts.
///
/// What is *not* here, and why: the layer-shell window itself. Building one
/// calls `gtk_layer_init_for_window`, which requires a Wayland compositor
/// speaking `zwlr_layer_shell_v1` — under `xvfb` (what CI has) it is a hard
/// error, not a degraded window. Nothing anywhere in this tree constructs a
/// layer surface in a test for that reason. So `show`'s four window properties
/// (`Layer::Overlay`, `KeyboardMode::Exclusive`, the namespace, the four
/// anchors) are **live-verify** items — `docs/live-verify.md`'s #1010 section
/// names them — and everything below the window is pinned here.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{
        DIALOG, build_root, close, close_all, close_for_departed, is_open, seed_for_test as seed,
        set_title, wire_escape, yield_to_shell_prompt,
    };
    use hytte::adw::{self, prelude::*};
    use hytte::gtk::glib::translate::IntoGlib;
    use hytte::gtk::{self, gdk};
    use hytte::reactive::registry;

    /// A stand-in for the plugin panel slot: the real one `.expect()`s a
    /// registered `PluginHandles`, which a `#[gtk::test]` has no booted `App`
    /// to provide (the `build_panel_child` split's argument, one level up).
    fn body() -> gtk::Label {
        gtk::Label::new(Some("page"))
    }

    /// Assert the dialog is fully down: no window, no selection, and exactly
    /// `edges` slot-visibility edge(s) published since [`seed`].
    fn assert_dismissed(edges: u32) {
        assert!(!is_open(), "the window must be taken down");
        assert_eq!(
            crate::plugins::dialog_panel(),
            None,
            "the dialog's own selection must be cleared"
        );
        assert_eq!(
            crate::plugins::visibility_edges(),
            edges,
            "a dismissal publishes exactly one slot-visibility edge — a `watch` \
             receiver cannot see a flap, so it is counted where it is emitted",
        );
        registry::reset_for_tests();
    }

    /// The drawer's window shape, reproduced: the `gtk::Overlay`'s **main**
    /// child is the transparent catcher and its **overlay** child is the card.
    /// That ordering is the whole no-dimming mechanism — GTK paints overlay
    /// children above the main child, so the card is on top of a catcher that
    /// paints nothing, and no cross-surface restacking is asked of niri.
    ///
    /// **Falsification:** swap the two (`set_child` the card,
    /// `add_overlay` the catcher) → the catcher would paint over the page; the
    /// class assertions below go red.
    #[gtk::test]
    fn the_catcher_is_the_main_child_and_the_card_the_overlay_child() {
        adw::init().expect("libadwaita init");
        let root = build_root("demo", &body());

        let main = root.child().expect("an Overlay main child");
        assert!(
            main.has_css_class("ts-modal-catcher"),
            "the main child must be the transparent click-catcher"
        );

        let card = root.last_child().expect("an overlay child");
        assert!(
            card.downcast_ref::<adw::Clamp>().is_some(),
            "the overlay child must be the clamped card"
        );
    }

    /// The header is the plugin **id**, dim, plus a close button — §8's default,
    /// and the reason the id is not a display name is that there is none on the
    /// wire (§9).
    #[gtk::test]
    fn the_header_carries_the_plugin_id_and_a_close_button() {
        adw::init().expect("libadwaita init");
        let root = build_root("hytte-plugin-agents", &body());

        let header = header_of(&root);
        let title = header
            .first_child()
            .and_then(|w| w.downcast::<gtk::Label>().ok())
            .expect("a title label");
        assert_eq!(title.text(), "hytte-plugin-agents");
        assert!(title.has_css_class("ts-dialog-title"));

        let close_btn = header
            .last_child()
            .and_then(|w| w.downcast::<gtk::Button>().ok())
            .expect("a close button");
        assert!(
            close_btn.has_css_class("flat"),
            "the close button is the flat/circular header shape"
        );
    }

    /// Opening plugin Y while X is shown **swaps the content**: the header
    /// retitles in place and no second window is built.
    ///
    /// Drives [`set_title`] against a hand-built `LiveDialog` — the swap path
    /// `open_on_focused` takes when the focused output already has the dialog —
    /// because `open_on_focused` itself needs a layer surface.
    ///
    /// **Falsification:** make `set_title` a no-op → the second assertion reds.
    #[gtk::test]
    fn opening_a_second_plugin_retitles_in_place() {
        adw::init().expect("libadwaita init");
        let window = seed("DP-1", "first");

        set_title("second");

        let root = window
            .child()
            .and_then(|w| w.downcast::<gtk::Overlay>().ok())
            .expect("the dialog root");
        let title = header_of(&root)
            .first_child()
            .and_then(|w| w.downcast::<gtk::Label>().ok())
            .expect("a title label");
        assert_eq!(
            title.text(),
            "second",
            "a second open on the same output must retitle the live card, not build a window"
        );

        // Through the real close path, which `seed`'s installed handles make
        // reachable — and which leaves the thread-locals clean for the next test
        // on this thread.
        close();
        assert_dismissed(1);
        window.destroy();
    }

    /// [`close`] over an empty slot touches nothing and — crucially — does not
    /// reach the plugin registry, which is what makes the `dialog-close`
    /// `GAction` safe to bind blind and what keeps a stray `Esc` from panicking
    /// a shell whose dialog is already down.
    ///
    /// **Falsification:** drop the `was_open` guard in `close` so it always
    /// publishes → this panics on the unregistered `PluginHandles`.
    #[gtk::test]
    fn closing_with_nothing_up_is_inert() {
        adw::init().expect("libadwaita init");
        DIALOG.with(|d| *d.borrow_mut() = None);
        close();
        assert!(!is_open());
    }

    /// `Esc` dismisses and **stops** the press; every other key proceeds into
    /// the page, so a plugin `Entry` inside it still receives what is typed.
    ///
    /// Drives the shipped closure, not a copy of it: [`wire_escape`] is wired
    /// onto a plain `gtk::Window` and the controller's own `key-pressed` signal
    /// is emitted, which is the only way to reach a key handler without a
    /// compositor delivering a real event. `close()` runs for real on the
    /// `Escape` arm and is inert with nothing up
    /// (`closing_with_nothing_up_is_inert`), so no registry is needed.
    ///
    /// **Falsification:** make `dismisses` always `false` (Escape a no-op), or
    /// drop the `add_controller` call → the first assertion reds.
    #[gtk::test]
    fn escape_dismisses_and_every_other_key_reaches_the_page() {
        adw::init().expect("libadwaita init");
        DIALOG.with(|d| *d.borrow_mut() = None);

        let window = gtk::Window::new();
        wire_escape(&window);

        let controller = window
            .observe_controllers()
            .into_iter()
            .flatten()
            .find_map(|c| c.downcast::<gtk::EventControllerKey>().ok())
            .expect("wire_escape must leave a key controller on the window");

        assert!(
            press(&controller, gdk::Key::Escape),
            "Escape must dismiss the dialog and stop the press"
        );
        assert!(
            !press(&controller, gdk::Key::Return),
            "every other key must proceed into the plugin's page"
        );

        window.destroy();
    }

    /// HIGH-1's second half: a shell prompt going up takes a live dialog **down**
    /// first, so the card is never built underneath one. Publishes exactly one
    /// dismissal edge, like every other take-down.
    ///
    /// The wiring — that `consent::request` and `prompt::show_prompt` actually
    /// call this — is pinned by
    /// `the_shell_prompts_yield_the_dialog_before_they_raise` below.
    ///
    /// **Falsification:** make `yield_to_shell_prompt` a no-op → `is_open()`
    /// stays true and this reds.
    #[gtk::test]
    fn a_shell_prompt_takes_the_dialog_down_before_it_raises() {
        adw::init().expect("libadwaita init");
        let window = seed("DP-1", "demo");

        yield_to_shell_prompt("consent card");

        assert_dismissed(1);
        window.destroy();
    }

    /// MEDIUM-3: a hot-plug rebuild destroys an open page — deliberate, matching
    /// `modal::close_all` — and must publish exactly one dismissal edge, so the
    /// plugin's `SlotVisible` is not left pinned by a surface that is gone.
    ///
    /// **Falsification:** drop `close()` from `close_all` → `is_open()` stays
    /// true; drop the `publish_dismissal()` from `close` → the selection stays
    /// set and the edge count is 0.
    #[gtk::test]
    fn a_hot_plug_takes_the_page_down_and_publishes_one_edge() {
        adw::init().expect("libadwaita init");
        let window = seed("DP-1", "demo");

        close_all();

        assert_dismissed(1);
        window.destroy();
    }

    /// LOW: the plugin whose page is up **disconnects**. Without this the page
    /// blanks but the selection and the visibility contributor stay set, so
    /// every plugin in that sidebar family keeps being told `SlotVisible = true`
    /// until a human dismisses an empty card.
    ///
    /// The second half is the guard that makes it safe to call for *every*
    /// departing plugin: a different plugin leaving must not touch the dialog.
    ///
    /// **Falsification:** drop the `dialog_panel() != Some(plugin_id)` early
    /// return → the second assertion reds (an unrelated departure dismisses the
    /// page).
    #[gtk::test]
    fn a_departed_plugin_takes_its_own_page_down_and_only_its_own() {
        adw::init().expect("libadwaita init");

        let window = seed("DP-1", "showing");
        close_for_departed("someone-else");
        assert!(
            is_open(),
            "another plugin leaving must not dismiss this page"
        );
        assert_eq!(crate::plugins::dialog_panel().as_deref(), Some("showing"));
        assert_eq!(crate::plugins::visibility_edges(), 0);

        close_for_departed("showing");
        assert_dismissed(1);
        window.destroy();
    }

    /// Emit one `key-pressed` on `controller`, returning whether the handler
    /// claimed it (`Propagation::Stop`).
    fn press(controller: &gtk::EventControllerKey, key: gdk::Key) -> bool {
        controller.emit_by_name::<bool>(
            "key-pressed",
            &[&key.into_glib(), &0u32, &gdk::ModifierType::empty()],
        )
    }

    /// The header box inside a built root.
    fn header_of(root: &gtk::Overlay) -> gtk::Widget {
        root.last_child()
            .and_then(|w| w.downcast::<adw::Clamp>().ok())
            .and_then(|clamp| clamp.child())
            .and_then(|column| column.first_child())
            .expect("header row")
    }
}
