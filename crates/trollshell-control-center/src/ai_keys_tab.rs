//! The **AI Keys** tab (#392) — one password-entry row per known LLM
//! provider. Each row stores a key in the shell's keyring (`SetAiKey` over
//! `Control`) and shows whether a key is currently stored (`ListAiKeys`) — the
//! value itself is never read back. The apply button sets/updates the key
//! (wiping the entry after, so the plaintext isn't retained in the widget);
//! the trash button clears it. When the shell isn't running the calls fail and
//! the rows show "Unavailable".
//!
//! Lives in its own module for the reason [`crate::places_tab`] and
//! [`crate::plugins_tab`] do: a tab with its own state struct is a module, not
//! two hundred lines in the middle of the app shell.
//!
//! # Re-read on the shell probe's transition, not on a clock of its own (#1003)
//!
//! Before this the tab's `ListAiKeys` status was read exactly once, from
//! [`build_page`], at window construction — the same one-shot shape #989 fixed
//! for the connection banner and the revision footer. A control-center opened
//! before the shell (the ordinary order after login), or left open across a
//! shell restart, showed a stale "Unavailable"/"No key set" status until the
//! user happened to reopen the tab.
//!
//! The first fix for that (#1015 as originally shipped) gave this tab its own
//! 2 s timer, mirroring [`crate::plugins_tab`]'s. An adversarial review of that
//! PR caught what a poll-cadence framing hides: `ListAiKeys` reaches the
//! shell's `secrets::keyring()`, which **unlocks (and, on #879's no-collection
//! case, creates) the login keyring — a prompt-capable operation**. That
//! function's own doc is explicit: *"Only for paths a human is waiting on …
//! Do not reach for this from anything periodic; see `probe`."* A 2 s poller
//! is exactly the periodic caller that doc forbids: on a locked/absent
//! collection it would respawn an unlock prompt every tick, and because zbus
//! dispatches `Control`'s method calls **inline, one at a time**, a
//! `ListAiKeys` handler parked on that prompt for up to its 10 s timeout would
//! head-of-line-block `Ping`/`Revision`/`ListPlugins`/`GetPlace` behind it —
//! which is what made #1002's own connection banner flip to "not running" as
//! a side effect of this tab polling the keyring.
//!
//! The actual fix — and the issue's own first suggestion — is to give the
//! keyring no clock at all: [`build_page`] reacts to `main.rs`'s
//! `ShellProbeUi` telling it the shell's reachability *changed*
//! ([`on_shell_reachable_change`], wired in `main.rs`'s `build_window` via
//! `ShellProbeUi::set_reachable_listener`). A down→up edge is exactly when the
//! stored-key answer can have changed since this tab last knew it, so that
//! direction spawns a fresh [`refresh_ai_status`] call; an up→down edge needs
//! no `Control` round trip at all — the shell is already known unreachable,
//! so [`on_shell_reachable_change`] applies "Unavailable" straight away rather
//! than spending a call (and, on a genuinely wedged shell, another 10 s
//! prompt-timeout) confirming what is already known. Steady-state keyring
//! traffic from this tab is now zero.
//!
//! `build_page` does **not** also read once at build (a second-round review
//! finding, LOW 2): `reachable` starts `None` on a freshly built
//! `ShellProbeUi` and `set_reachable_listener` runs before the first
//! `poll()`, so the very first applied probe is *always* a transition and
//! *always* reaches this tab — an unconditional build-time read alongside
//! that would cost **two** `ListAiKeys` calls (two keyring unlocks) at every
//! window open where the shell is already up, where `origin/main` cost one.
//! Relying solely on the guaranteed first-probe delivery keeps every scenario
//! at exactly one read: already up at open → one, from the first probe; down
//! at open then started later → one, on the up edge. The tab shows its
//! built-in "…" placeholder (same idea as the banner's own
//! [`crate::CONNECTING_BANNER`]) for the brief window before that first probe
//! resolves.
//!
//! Apply and Clear keep their **immediate**, unconditional reads — a human
//! pressed a button and is waiting on the result — each spawn still carries a
//! [`PollGenerations`] stamp so a result older than the newest already applied
//! is dropped whole (the exact discipline [`crate::plugins_tab`]'s #983 fix
//! uses, now protecting Apply/Clear against a transition-triggered read rather
//! than against a removed timer's tick). [`apply_ai_status`] logs on
//! transitions only ([`log_transition`]), mirroring `ShellProbeUi::shown` in
//! `main.rs`.

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use hytte_bus::RetryPolicy;

use crate::{CONTROL_IFACE, CONTROL_NAME, CONTROL_PATH, spawn_on_runtime};

/// The LLM providers the AI Keys tab manages, `(slot, label, help)`. The `slot`
/// is the provider name the shell stores the key under and injects as
/// `<SLOT>_API_KEY` at plugin launch — for `openrouter` that's
/// `OPENROUTER_API_KEY`, exactly what the pet and caw plugins read. Add a row
/// here to surface a new provider.
const KNOWN_AI_PROVIDERS: &[(&str, &str, &str)] = &[(
    "openrouter",
    "OpenRouter",
    "Cloud LLM used by the pet and caw plugins. Create a key at openrouter.ai.",
)];

// ── Read ordering (#1003, mirrors plugins_tab's #983) ─────────────────────────

/// Monotonic ordering over the tab's overlapping `ListAiKeys` reads (#1003).
///
/// Apply, Clear and a shell-reachable transition ([`on_shell_reachable_change`])
/// can each spawn a `ListAiKeys` round trip, and nothing serialises them
/// against each other — a slow one spawned first can complete after a fast one
/// spawned later. Each spawn therefore takes an [`issue`]d generation, and its
/// completion [`accept`]s it only if no newer generation has already been
/// applied — the exact mechanism [`crate::plugins_tab`]'s `PollGenerations`
/// uses for the same reason (#983). Not duplicated by `use`: the two tabs'
/// shapes differ enough (this one has no #944-style latch to interact with)
/// that keeping them as separate, independently-falsifiable types is worth the
/// ~15 duplicated lines.
///
/// [`issue`]: PollGenerations::issue
/// [`accept`]: PollGenerations::accept
#[derive(Default)]
struct PollGenerations {
    /// The generation handed to the most recently spawned read.
    issued: Cell<u64>,
    /// The newest generation whose result has been applied to the tab. `0`
    /// until the first completion, which is below every issued generation.
    applied: Cell<u64>,
}

impl PollGenerations {
    /// Stamp a freshly spawned read with the next generation.
    fn issue(&self) -> u64 {
        let next = self.issued.get().saturating_add(1);
        self.issued.set(next);
        next
    }

    /// Claim `generation` as the newest applied, or refuse it (`false`)
    /// because a newer read's result already landed.
    ///
    /// Strictly greater: a generation is issued once, so an equal value can
    /// only be the same read's completion running twice, which is not a thing
    /// [`spawn_on_runtime`] does.
    fn accept(&self, generation: u64) -> bool {
        if generation <= self.applied.get() {
            return false;
        }
        self.applied.set(generation);
        true
    }
}

/// Shared, mutable state threaded through the AI Keys tab's build, its
/// Apply/Clear buttons, and [`on_shell_reachable_change`].
#[derive(Clone)]
struct AiKeysState {
    /// (slot, status label, clear button) per known provider, updated in
    /// place by every applied read rather than rebuilt — there is nothing to
    /// select or navigate in this tab, but keeping the shape matches
    /// `plugins_tab`'s in-place refresh anyway.
    rows: Rc<Vec<(String, gtk::Label, gtk::Button)>>,
    /// Ordering over the tab's overlapping reads (#1003) — see
    /// [`PollGenerations`].
    polls: Rc<PollGenerations>,
    /// Whether the most recently *applied* read's outcome was a failure —
    /// `None` before the first completion. Drives transitions-only logging
    /// (#1003, mirrors `main.rs`'s `ShellProbeUi::shown`): a run of identical
    /// outcomes logs once, not once per change.
    ///
    /// Coarser than `shown` by design (not an oversight — #1015's review, NIT
    /// 1): `shown` tracks the *rendered banner text*, so a different failure
    /// reason logs again; this tracks only success-vs-failure, so e.g. a
    /// `ServiceUnknown` followed by a `Timeout` stays quiet. Both outcomes
    /// render identically here ("Unavailable", clear button desensitised),
    /// so there is no second UI state a reason change would need to explain.
    last_failing: Rc<Cell<Option<bool>>>,
}

impl AiKeysState {
    fn new(rows: Vec<(String, gtk::Label, gtk::Button)>) -> Self {
        Self {
            rows: Rc::new(rows),
            polls: Rc::new(PollGenerations::default()),
            last_failing: Rc::new(Cell::new(None)),
        }
    }
}

/// Build the **AI Keys** tab.
///
/// Returns the tab's root widget and a closure the caller must register as
/// `main.rs`'s `ShellProbeUi::set_reachable_listener` (#1003) — unlike
/// [`crate::plugins_tab::build_page`] / [`crate::places_tab::build_page`] this
/// tab installs no timer of its own and so has no `SourceId` for the window to
/// drop on close; see the module doc for why.
///
/// The wiring at the call site (`main.rs`'s `build_window`) is, like
/// `install_shell_probe`'s own call site, reachable from no test — only the
/// closure's *behaviour* ([`on_shell_reachable_change`]) is pinned, in
/// `gtk_tests` below. Deleting the `set_reachable_listener` call in
/// `build_window` would silently return this tab to #1003's one-shot bug with
/// every test here still green, exactly the gap #1002 already carries for its
/// own `install_shell_probe` call site.
pub(crate) fn build_page() -> (adw::PreferencesPage, impl Fn(bool) + 'static) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("AI provider keys")
        .description(
            "API keys for the LLM-backed plugins, stored in your login keyring \
             (gnome-keyring/libsecret) — never on disk or in config. A key is \
             injected only into the plugins that declare it, and changing one \
             relaunches those plugins.",
        )
        .build();

    // (slot, entry, status label, clear button) per provider while building.
    let mut built = Vec::new();
    for (slot, label, help) in KNOWN_AI_PROVIDERS {
        let entry = adw::PasswordEntryRow::builder()
            .title(*label)
            .show_apply_button(true)
            .build();
        entry.set_tooltip_text(Some(help));

        let status_lbl = gtk::Label::new(Some("…"));
        status_lbl.add_css_class("dim-label");
        let clear_btn = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Clear the stored key")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        clear_btn.add_css_class("flat");
        entry.add_suffix(&status_lbl);
        entry.add_suffix(&clear_btn);

        group.add(&entry);
        built.push((*slot, entry, status_lbl, clear_btn));
    }
    page.add(&group);

    let state = AiKeysState::new(
        built
            .iter()
            .map(|(slot, _entry, lbl, btn)| ((*slot).to_owned(), lbl.clone(), btn.clone()))
            .collect(),
    );

    for (slot, entry, _lbl, clear_btn) in built {
        // Apply → SetAiKey, then wipe the entry (don't keep the plaintext) and
        // re-read the stored-key status.
        {
            let (slot, state) = (slot.to_owned(), state.clone());
            entry.connect_apply(move |e| {
                let value = e.text().to_string();
                if value.is_empty() {
                    return;
                }
                let (e, slot, state) = (e.clone(), slot.clone(), state.clone());
                spawn_on_runtime(set_ai_key(slot, value), move |res| {
                    if let Err(err) = res {
                        tracing::info!(%err, "SetAiKey failed");
                    }
                    e.set_text("");
                    refresh_ai_status(&state);
                });
            });
        }
        // Clear → ClearAiKey, then re-read the status.
        {
            let (slot, state) = (slot.to_owned(), state.clone());
            clear_btn.connect_clicked(move |_| {
                let (slot, state) = (slot.clone(), state.clone());
                spawn_on_runtime(clear_ai_key(slot), move |res| {
                    if let Err(err) = res {
                        tracing::info!(%err, "ClearAiKey failed");
                    }
                    refresh_ai_status(&state);
                });
            });
        }
    }

    // No build-time `refresh_ai_status` call here (#1003 LOW 2, second-round
    // review): `notify_shell_reachable_change` below is registered against a
    // freshly built `ShellProbeUi` whose `reachable` starts `None`, so the
    // very first probe it applies is unconditionally a transition and always
    // delivers one read — adding a second, build-time read here would cost
    // two `ListAiKeys` calls (two keyring unlocks) at every window open where
    // the shell is already up. See the module doc.
    let notify_shell_reachable_change = {
        let state = state.clone();
        move |reachable: bool| on_shell_reachable_change(&state, reachable)
    };
    (page, notify_shell_reachable_change)
}

/// Re-read which providers have a stored key (`ListAiKeys`) and reflect it
/// into each row's status label + clear-button sensitivity. Called by the
/// Apply/Clear buttons directly, and from [`on_shell_reachable_change`] when
/// the shell just became reachable — including the very first probe after
/// [`build_page`] returns, which is unconditionally a transition (see that
/// function's own comment).
fn refresh_ai_status(state: &AiKeysState) {
    let generation = state.polls.issue();
    let state = state.clone();
    spawn_on_runtime(list_ai_keys(), move |res| {
        on_ai_status_result(&state, generation, res);
    });
}

/// React to the shell probe's reachability changing (#1003) — the mechanism
/// that replaced this tab's original 2 s timer; see the module doc for why a
/// clock of its own was the wrong shape.
///
/// `reachable = true` (the shell just came up, or just answered for the first
/// time): the stored-key answer can have changed since this tab last knew it,
/// so spawn a genuine [`refresh_ai_status`] read.
///
/// `reachable = false` (the shell just went away): no `Control` round trip is
/// spent finding that out — the probe already established it, and asking again
/// would cost another call (and, on a wedged rather than absent shell, another
/// up-to-`OP_TIMEOUT`-second wait) to confirm what is already known. The
/// generation is still issued so this synthetic outcome can't later be
/// resurrected by a slower read that was in flight *before* the shell went
/// down (see `an_earlier_read_cannot_resurrect_a_status_a_later_clear_already_overwrote`
/// for the general shape of that race).
fn on_shell_reachable_change(state: &AiKeysState, reachable: bool) {
    if reachable {
        refresh_ai_status(state);
    } else {
        let generation = state.polls.issue();
        on_ai_status_result(state, generation, Err(shell_unreachable_error()));
    }
}

/// The `BusError` [`on_shell_reachable_change`] applies when the shell probe
/// has already established the endpoint is unreachable, without spending a
/// further `Control` call to rediscover it. Shaped like the real
/// `ServiceUnknown` a failed call against a not-running shell actually
/// produces (#959 confirmed that shape empirically), so [`apply_ai_status`]'s
/// logging and the rows it sets are indistinguishable from a genuine failed
/// read.
fn shell_unreachable_error() -> hytte_bus::BusError {
    hytte_bus::BusError::Permanent {
        reason: "the shell probe reported the control endpoint unreachable".to_owned(),
        dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
    }
}

/// Apply one `ListAiKeys` completion — unless `generation` is older than the
/// newest already applied, in which case it is dropped whole (#1003, mirrors
/// [`crate::plugins_tab::on_poll_result`]'s #983 guard).
fn on_ai_status_result(
    state: &AiKeysState,
    generation: u64,
    res: Result<Vec<String>, hytte_bus::BusError>,
) {
    if !state.polls.accept(generation) {
        tracing::debug!(
            generation,
            "dropping a stale ListAiKeys result — a newer poll already applied"
        );
        return;
    }
    apply_ai_status(state, res);
}

/// What (if anything) a poll outcome transitioning from `previous` (the last
/// *applied* poll's failure state — `None` before the first) to `is_err`
/// should log.
///
/// Pure so the transitions-only rule is unit-tested without a display server —
/// mirrors `main.rs`'s split of `should_probe_revision`/`format_banner_message`
/// out of `ShellProbeUi::apply`. [`apply_ai_status`]'s widget mutations are
/// pinned separately in `gtk_tests`, which is the only place the rows can be
/// asserted at all.
#[derive(Debug, PartialEq, Eq)]
enum LogTransition {
    /// Same outcome as last time (or the very first poll succeeded) — nothing
    /// to say.
    None,
    /// A run of successes (or the very first poll) just started failing.
    Failed,
    /// A run of failures just started succeeding again.
    Recovered,
}

fn log_transition(previous: Option<bool>, is_err: bool) -> LogTransition {
    if previous == Some(is_err) {
        return LogTransition::None;
    }
    if is_err {
        LogTransition::Failed
    } else if previous == Some(true) {
        LogTransition::Recovered
    } else {
        // The very first poll ever, and it succeeded: matches the pre-#1003
        // behaviour of never logging a bare success.
        LogTransition::None
    }
}

/// Reflect one `ListAiKeys` outcome into the rows, logging on transitions only
/// ([`log_transition`]) rather than on every applied read.
fn apply_ai_status(state: &AiKeysState, res: Result<Vec<String>, hytte_bus::BusError>) {
    let is_err = res.is_err();
    let previous = state.last_failing.replace(Some(is_err));
    match log_transition(previous, is_err) {
        LogTransition::Failed => {
            if let Err(err) = &res {
                tracing::info!(%err, "ListAiKeys failed");
            }
        }
        LogTransition::Recovered => tracing::info!("ListAiKeys recovered"),
        LogTransition::None => {}
    }
    match res {
        Ok(slots) => {
            let set: HashSet<String> = slots.into_iter().collect();
            for (slot, lbl, btn) in state.rows.iter() {
                let has = set.contains(slot);
                lbl.set_text(if has { "Key stored" } else { "No key set" });
                btn.set_sensitive(has);
            }
        }
        Err(_) => {
            for (_slot, lbl, btn) in state.rows.iter() {
                lbl.set_text("Unavailable");
                btn.set_sensitive(false);
            }
        }
    }
}

/// `ListAiKeys` → the provider slots that currently have a stored key. Values
/// are never returned.
async fn list_ai_keys() -> Result<Vec<String>, hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ListAiKeys")
        .timeout(Duration::from_secs(3))
        .retry(RetryPolicy::Never)
        .send::<Vec<String>>()
        .await
}

/// `SetAiKey(slot, value)`: store `value` as the key for `slot` in the shell's
/// keyring (which then relaunches the plugins that use it).
async fn set_ai_key(slot: String, value: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("SetAiKey")
        .args((slot, value))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

/// `ClearAiKey(slot)`: delete the stored key for `slot`.
async fn clear_ai_key(slot: String) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call(hytte_bus::BusKind::Session, CONTROL_NAME)
        .at_path(CONTROL_PATH)
        .iface(CONTROL_IFACE)
        .method("ClearAiKey")
        .args((slot,))
        .timeout(Duration::from_secs(5))
        .retry(RetryPolicy::Never)
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{LogTransition, PollGenerations, log_transition};

    // ── Poll ordering (#1003) ────────────────────────────────────────────────

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

    // ── Transitions-only logging (#1003) ─────────────────────────────────────

    #[test]
    fn a_fresh_tab_failing_for_the_first_time_is_logged() {
        assert_eq!(log_transition(None, true), LogTransition::Failed);
    }

    #[test]
    fn a_fresh_tab_succeeding_for_the_first_time_is_quiet() {
        // Matches the pre-#1003 behaviour: a bare success was never logged.
        assert_eq!(log_transition(None, false), LogTransition::None);
    }

    #[test]
    fn a_repeated_failure_is_not_logged_again() {
        assert_eq!(log_transition(Some(true), true), LogTransition::None);
    }

    #[test]
    fn a_repeated_success_is_not_logged_again() {
        assert_eq!(log_transition(Some(false), false), LogTransition::None);
    }

    #[test]
    fn recovering_from_a_failure_is_logged() {
        assert_eq!(log_transition(Some(true), false), LogTransition::Recovered);
    }

    #[test]
    fn regressing_after_a_success_is_logged() {
        assert_eq!(log_transition(Some(false), true), LogTransition::Failed);
    }
}

/// The AI Keys tab's widget-level behaviour (#1003).
///
/// Gated on `system-tests` for the same reason `main.rs`'s `gtk_tests` module
/// is: `gtk::Label`/`gtk::Button` are real widgets, so "the row reflects the
/// newest read, not a stale one" and "the rows follow the shell both up and
/// down" cannot be asserted without a display server. The pure decision logic
/// ([`PollGenerations`], [`log_transition`]) is already pinned hermetically by
/// `tests` above.
#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use adw::prelude::*;

    use super::{AiKeysState, on_ai_status_result, on_shell_reachable_change};

    /// Build a fabricated tab state for the given slots — no `build_page`, no
    /// `Control` traffic, just the widgets [`super::apply_ai_status`] writes
    /// into.
    fn test_state(slots: &[&str]) -> AiKeysState {
        let rows = slots
            .iter()
            .map(|slot| {
                (
                    (*slot).to_owned(),
                    gtk::Label::new(None),
                    gtk::Button::new(),
                )
            })
            .collect();
        AiKeysState::new(rows)
    }

    fn row_text(state: &AiKeysState, slot: &str) -> String {
        state
            .rows
            .iter()
            .find(|(s, _, _)| s == slot)
            .unwrap_or_else(|| panic!("no such row: {slot}"))
            .1
            .text()
            .to_string()
    }

    /// Whether `slot`'s clear button is sensitive — desensitised exactly when
    /// the row shows "Unavailable" or "No key set" (#1015 review, MEDIUM 1).
    fn row_sensitive(state: &AiKeysState, slot: &str) -> bool {
        state
            .rows
            .iter()
            .find(|(s, _, _)| s == slot)
            .unwrap_or_else(|| panic!("no such row: {slot}"))
            .2
            .is_sensitive()
    }

    /// Issue a fresh generation and apply `res` through it in one call — the
    /// shape every one of `refresh_ai_status`'s real callers (Apply, Clear,
    /// [`on_shell_reachable_change`]) produces, without a `Control` round trip.
    fn applied(state: &AiKeysState, res: Result<Vec<String>, hytte_bus::BusError>) {
        let generation = state.polls.issue();
        on_ai_status_result(state, generation, res);
    }

    fn down() -> Result<Vec<String>, hytte_bus::BusError> {
        Err(hytte_bus::BusError::Permanent {
            reason: "The name mov.vibec0re.trollshell.Control was not provided by any \
                     .service files"
                .to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
        })
    }

    /// The base case: a completion older than the newest already applied is
    /// dropped whole, so the row does not regress to what it read.
    ///
    /// Driven through [`on_ai_status_result`] rather than
    /// `super::apply_ai_status` because the generation is exactly what
    /// distinguishes the two completions.
    ///
    /// Falsified by deleting the `state.polls.accept(generation)` guard in
    /// `on_ai_status_result`: the stale "No key set" then lands and the last
    /// assertion fails.
    #[gtk::test]
    fn a_stale_ai_keys_poll_cannot_regress_the_rows() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        let seed = state.polls.issue();
        on_ai_status_result(&state, seed, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(row_text(&state, "openrouter"), "Key stored");

        // The slow poll is spawned first and completes last — the whole shape
        // of the defect.
        let slow = state.polls.issue();
        let fresh = state.polls.issue();
        on_ai_status_result(&state, fresh, Ok(Vec::new()));
        assert_eq!(
            row_text(&state, "openrouter"),
            "No key set",
            "sanity: the newer poll must be applied normally"
        );

        on_ai_status_result(&state, slow, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(
            row_text(&state, "openrouter"),
            "No key set",
            "an out-of-order completion must not regress the row to stale data"
        );
    }

    /// A stale poll's `Err` must not replace a live, newer list with the
    /// "Unavailable" placeholder either — the guard covers every arm, not just
    /// the success one.
    #[gtk::test]
    fn a_stale_error_cannot_overwrite_a_newer_success() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        let slow = state.polls.issue();
        let fresh = state.polls.issue();
        on_ai_status_result(&state, fresh, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(row_text(&state, "openrouter"), "Key stored");

        on_ai_status_result(&state, slow, down());
        assert_eq!(
            row_text(&state, "openrouter"),
            "Key stored",
            "a stale failure must not blank out a newer, live result"
        );
    }

    // ── The rows follow the shell, both directions (#1015 review, MEDIUM 1) ──

    /// #1003 in one sequence, over the *rows* rather than the log: opened
    /// before the shell, the shell starts, then dies, then comes back. Mirrors
    /// `main.rs`'s `the_banner_reveals_and_hides_in_both_directions`.
    ///
    /// This is the property the issue is actually about, and before this test
    /// no assertion anywhere in the crate read the string `"Unavailable"` or
    /// called `is_sensitive()` on a clear button — `apply_ai_status`'s `Err`
    /// arm ran (via the transitions-logging test below) but nothing checked
    /// what it did to the rows.
    ///
    /// Falsified by replacing `apply_ai_status`'s `Err` arm with `Err(_) => {}`
    /// — a mutation the #1015 review found left the rest of the suite green.
    #[gtk::test]
    fn the_ai_keys_rows_follow_the_shell_in_both_directions() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        applied(&state, down()); // 1. before the shell
        assert_eq!(row_text(&state, "openrouter"), "Unavailable");
        assert!(!row_sensitive(&state, "openrouter"));

        applied(&state, Ok(vec!["openrouter".to_owned()])); // 2. shell starts
        assert_eq!(row_text(&state, "openrouter"), "Key stored");
        assert!(row_sensitive(&state, "openrouter"));

        applied(&state, down()); // 3. shell dies
        assert_eq!(row_text(&state, "openrouter"), "Unavailable");
        assert!(!row_sensitive(&state, "openrouter"));

        applied(&state, Ok(Vec::new())); // 4. and returns
        assert_eq!(row_text(&state, "openrouter"), "No key set");
        assert!(!row_sensitive(&state, "openrouter"));
    }

    /// A read issued **before** a Clear click must not resurrect the
    /// pre-clear status if it lands *after* Clear's own (newer-generation)
    /// result — the same ordering property that protects Apply/Clear against
    /// [`on_shell_reachable_change`]'s transition-triggered reads, now that
    /// there is no periodic tick to frame it around (adapted from the #1015
    /// review's supplied `a_tick_issued_before_a_clear_cannot_resurrect_the_
    /// old_status`, which pinned the identical property against the timer
    /// this PR removed).
    #[gtk::test]
    fn an_earlier_read_cannot_resurrect_a_status_a_later_clear_already_overwrote() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        // An earlier read is issued (e.g. a shell-reachable transition) but
        // its result hasn't landed yet.
        let earlier = state.polls.issue();

        // The user clicks Clear; its own refresh is issued — and completes —
        // after the earlier read.
        let clear_refresh = state.polls.issue();
        on_ai_status_result(&state, clear_refresh, Ok(Vec::new()));
        assert_eq!(row_text(&state, "openrouter"), "No key set");

        // The earlier, slower read finally lands, carrying the pre-clear
        // answer.
        on_ai_status_result(&state, earlier, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(
            row_text(&state, "openrouter"),
            "No key set",
            "an earlier-issued, later-landing read must not resurrect what Clear already \
             overwrote"
        );
    }

    // ── The shell-reachable transition itself (#1015 review, HIGH 1) ─────────

    /// `reachable = false` must not cost a `Control` round trip: the rows go
    /// straight to "Unavailable" from the probe's own verdict, synchronously.
    ///
    /// Falsified by making `on_shell_reachable_change`'s `false` arm a no-op:
    /// the row stays on its last-good value instead of following the shell
    /// down.
    #[gtk::test]
    fn shell_becoming_unreachable_marks_the_rows_unavailable_without_a_call() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);
        applied(&state, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(row_text(&state, "openrouter"), "Key stored");

        on_shell_reachable_change(&state, false);

        assert_eq!(row_text(&state, "openrouter"), "Unavailable");
        assert!(!row_sensitive(&state, "openrouter"));
    }

    /// `reachable = true` must spawn a genuine `ListAiKeys` read — observed
    /// through the one synchronous side effect a spawn has, issuing a fresh
    /// generation, since nothing here can complete the round trip without a
    /// real `Control` endpoint.
    ///
    /// Falsified by making `on_shell_reachable_change`'s `true` arm a no-op:
    /// `issued` does not advance and the assertion fails.
    #[gtk::test]
    fn shell_becoming_reachable_issues_a_fresh_read() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);
        let before = state.polls.issued.get();

        on_shell_reachable_change(&state, true);

        assert!(
            state.polls.issued.get() > before,
            "a reachable transition must issue a fresh ListAiKeys read"
        );
    }

    /// The exact race `on_shell_reachable_change`'s `false` arm claims to
    /// defend against in its own doc (#1015 review, second pass, LOW 1): a
    /// read spawned on the up edge that only lands *after* the shell went
    /// down again must not resurrect the stale success over the synthetic
    /// "Unavailable".
    ///
    /// Falsified by having the `false` arm call `apply_ai_status` directly
    /// instead of issuing a generation through `on_ai_status_result` — the
    /// in-flight read then has no generation to be refused by, and its
    /// eventual `Ok` regresses the row back to "Key stored".
    #[gtk::test]
    fn a_read_in_flight_when_the_shell_went_down_cannot_resurrect_key_stored() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        on_shell_reachable_change(&state, true);
        let in_flight = state.polls.issued.get();

        on_shell_reachable_change(&state, false);
        assert_eq!(row_text(&state, "openrouter"), "Unavailable");

        on_ai_status_result(&state, in_flight, Ok(vec!["openrouter".to_owned()]));
        assert_eq!(
            row_text(&state, "openrouter"),
            "Unavailable",
            "a read issued before the shell went down must not resurrect its answer"
        );
        assert!(!row_sensitive(&state, "openrouter"));
    }

    /// A steady shell must cost exactly one `ListAiKeys` read no matter how
    /// many probe ticks land — the property the removed 2 s timer violated by
    /// construction, and the composition (`ShellProbeUi` → this tab's
    /// callback → an issued read) `build_page`'s own doc calls out as
    /// otherwise untested (#1015 review, second pass, LOW 2/HIGH 1 follow-up).
    ///
    /// Falsified by deleting `ShellProbeUi::apply`'s `!= Some(reachable)`
    /// comparison: the read count becomes one *per tick* instead of one per
    /// outage.
    #[gtk::test]
    fn a_steady_shell_costs_exactly_one_ai_keys_read_across_many_probe_ticks() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);
        let banner = adw::Banner::new("");
        let label = gtk::Label::new(None);
        let ui = crate::ShellProbeUi::new(&banner, &label);
        {
            let state = state.clone();
            ui.set_reachable_listener(move |reachable| {
                on_shell_reachable_change(&state, reachable);
            });
        }

        let before = state.polls.issued.get();
        for _ in 0..8 {
            ui.apply(&up_probe());
        }
        assert_eq!(
            state.polls.issued.get() - before,
            1,
            "a steady, reachable shell must cost exactly one ListAiKeys read across every tick"
        );

        // …and the down edge is still delivered, once.
        ui.apply(&down_probe());
        ui.apply(&down_probe());
        assert_eq!(
            state.polls.issued.get() - before,
            2,
            "the down edge fires exactly once"
        );
        assert_eq!(row_text(&state, "openrouter"), "Unavailable");
    }

    fn up_probe() -> crate::ShellProbe {
        crate::ShellProbe {
            connection: Ok(("pong".to_owned(), "0.1.0".to_owned())),
            revision: Some("34e3d96".to_owned()),
        }
    }

    fn down_probe() -> crate::ShellProbe {
        crate::ShellProbe {
            connection: Err(hytte_bus::BusError::Permanent {
                reason: "The name mov.vibec0re.trollshell.Control was not provided by any \
                         .service files"
                    .to_owned(),
                dbus_name: Some("org.freedesktop.DBus.Error.ServiceUnknown".to_owned()),
            }),
            revision: None,
        }
    }

    /// A `tracing` writer that collects every emitted line in memory, so a
    /// test can count log lines rather than infer them from state. Mirrors
    /// `main.rs`'s `CapturedLog`.
    #[derive(Clone)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("the capture buffer is never held across a panic")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn captured_logs(body: impl FnOnce()) -> Vec<String> {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(CapturedLog(buffer.clone()))
            .with_max_level(tracing::Level::INFO)
            .finish();
        {
            let _guard = tracing::subscriber::set_default(subscriber);
            body();
        }
        let bytes = buffer.lock().expect("no panic while capturing").clone();
        String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The tab logs on **transitions**, not on every applied read — the same
    /// #780 property #989 pinned for the shell probe, and load-bearing here
    /// precisely because [`on_shell_reachable_change`] can now apply a read
    /// on every shell restart in a session, not just once.
    ///
    /// Falsified by removing `apply_ai_status`'s `log_transition` guard: four
    /// failing reads then emit four lines and the first assertion fails.
    #[gtk::test]
    fn ai_keys_status_logs_transitions_not_every_change() {
        adw::init().expect("libadwaita init");
        let state = test_state(&["openrouter"]);

        let lines = captured_logs(|| {
            for _ in 0..4 {
                applied(&state, down());
            }
        });
        assert_eq!(
            lines.len(),
            1,
            "a shell that stays down must log once, not once per read: {lines:#?}"
        );
        assert!(
            lines[0].contains("ListAiKeys failed"),
            "the one line must be the failure: {:?}",
            lines[0]
        );

        // …and recovering is a transition too, so the guard is not "never
        // log again".
        let lines = captured_logs(|| {
            for _ in 0..2 {
                applied(&state, Ok(Vec::new()));
            }
        });
        assert_eq!(
            lines.len(),
            1,
            "recovering must log exactly once, not once per subsequent success: {lines:#?}"
        );
        assert!(
            lines[0].contains("ListAiKeys recovered"),
            "the one line must be the recovery: {:?}",
            lines[0]
        );
    }
}
