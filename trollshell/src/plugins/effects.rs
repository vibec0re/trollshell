//! GTK-side effect broker (#277 / #349 PR2 / #436 / #487).
//!
//! Drained on the GTK main thread from the non-lossy effect channel, this maps
//! one wire [`Effect`] onto a real host command. Capability enforcement happens
//! **upstream** in the connection reader ([`super::session::enforce_capabilities`]),
//! so an effect arriving here is always one the plugin was granted.

use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use hytte::services::{mpris, niri, notifications, pipewire, systemd};
use hytte_plugin_proto::{
    AudioAction, Effect, EffectOutcome, HostMsg, MediaAction, NiriAction, Page,
};
use tokio::sync::mpsc;

use super::datasource::DatasourceRouter;

/// Map one wire [`Effect`] onto a real host command. Handles [`Effect::OpenPage`]
/// (→ the modal drawer), [`Effect::Niri`] (→ niri's IPC actions), [`Effect::Media`]
/// (→ MPRIS transport on the active player), [`Effect::Audio`] (→ the default
/// sink's volume/mute), [`Effect::RaiseOsd`] (→ the transient OSD nudge, #236),
/// [`Effect::Notify`] (→ a local notification toast, #406), [`Effect::RunCommand`]
/// (→ a spawned `argv`, its outcome routed back as [`HostMsg::EffectResult`], #510 —
/// or, with `detached: true`, a program handed to the systemd user manager so it
/// outlives the shell, #953), [`Effect::RequestConsent`] (→ the interactive consent
/// overlay, #487) and the two datasource legs (#509).
///
/// The match is **exhaustive over the effect vocabulary** — there is no catch-all
/// (#648). The three compositor/media/audio variants were declared, cap-gated and
/// audited as `allowed` while this broker quietly dropped them into a
/// `warn!("unsupported in v1")`, which the plugin author never sees; the way that
/// class of gap stops recurring is for a new [`Effect`] variant to be a compile
/// error here, exactly as it already is in [`effect_kind`] and
/// [`effect_capability`](super::session::effect_capability).
///
/// Capability enforcement happens
/// **upstream** of here, per connection: [`super::session::enforce_capabilities`]
/// drops any effect whose [`Capability`](hytte_plugin_proto::Capability) the plugin
/// didn't declare before it ever reaches this broker (#436), so an effect arriving
/// here is always one the plugin was granted. Every effect reaching the broker is
/// appended to the persisted audit log ([`record_audit`], #510); the two drop sites
/// in [`super::session`] record the dropped ones.
///
/// `outbound` is the producing connection's host→plugin channel, used by the
/// **two-way** effects to route a reply back to this plugin: the human's decision
/// for [`Effect::RequestConsent`] as a [`HostMsg::ConsentDecision`] (#487), and the
/// command outcome for [`Effect::RunCommand`] as a [`HostMsg::EffectResult`] (#510).
/// The one-way effects ignore it.
///
/// `datasource` is the host's cross-connection [`DatasourceRouter`] (#509): unlike
/// the two-way effects above (whose reply routes back to the *same* connection via
/// `outbound`), a datasource query routes to a **different** connection (the
/// provider) and its result back again, so the broker hands those two effects to
/// the router rather than answering on `outbound`.
///
/// Pulled out of [`broker_effect`] itself only to keep that function under
/// clippy's line cap; the logic and its rationale live here.
///
/// A detached launch's audit line has to name the unit it started (#953 M1) —
/// that is what makes the line reconcilable against `systemctl --user
/// list-units 'trollshell-launch-*'`. The name isn't final until the host
/// allocates its uniquifier (M2), so it is allocated *here*, before the audit
/// record, and the finished string is handed to the launcher unchanged: the
/// line and the unit systemd actually starts are then the same string by
/// construction, not by two matching `format!` calls.
///
/// `is_valid_plugin_id` is checked here too (#964 item 2), not only later
/// inside `start_detached` — so a plugin id that can't be a unit-name segment
/// never gets a phantom `unit=`/`slice=` logged for a unit that is never
/// actually asked of systemd. Without this, `start_detached`'s own guard
/// still takes the direct-spawn fallback correctly, but the audit line would
/// already have claimed a unit that doesn't exist — the same shape of defect
/// M1 was filed for, just on the rejected-id path (see the second-pass review
/// on #960).
///
/// For a rejected id this returns `None`. [`broker_effect`]'s `RunCommand`
/// match arm still has to hand [`launch_detached`] *some* `String` (its
/// signature isn't `Option`), so it recomputes one via the same call on the
/// `None` arm — but since the review on this PR (#964 M-2), that recomputed
/// name is deliberately named nowhere an operator would read it: not in the
/// audit line (this function already prevented that) and not in the
/// `tracing::info!` line either. `start_detached`'s own `is_valid_plugin_id`
/// guard means it is never asked of systemd regardless.
fn detached_launch_unit_for_audit(plugin_id: &str, effect: &Effect) -> Option<String> {
    match effect {
        Effect::RunCommand { id, detached, .. }
            if *detached && systemd::is_valid_plugin_id(plugin_id) =>
        {
            Some(allocate_launch_unit(plugin_id, *id))
        }
        _ => None,
    }
}

pub(super) fn broker_effect(
    plugin_id: &str,
    effect: &Effect,
    outbound: &mpsc::Sender<HostMsg>,
    datasource: &DatasourceRouter,
) {
    // #953 M1 / #964 item 2: allocated *here*, before the audit record, and
    // handed to the launcher unchanged — see `detached_launch_unit_for_audit`'s
    // doc for why, and for the rejected-id case.
    let mut detached_unit = detached_launch_unit_for_audit(plugin_id, effect);
    // Every effect reaching the broker cleared capability enforcement + the rate
    // cap upstream (`session`), so it is an `Allowed` decision in the persisted
    // audit log (#510); the dropped ones are recorded at their `session` drop sites.
    record_audit(
        plugin_id,
        effect,
        AuditDecision::Allowed,
        detached_unit.as_deref(),
    );
    match effect {
        Effect::OpenPage(page) => {
            // #499/#517: open the drawer on niri's focused output, not an
            // arbitrary one. `preferred = None` let `open_on_focused` pick any
            // mounted drawer; passing the focused connector routes it to the screen
            // the user is on (the consent overlay wants the same routing). Sourced
            // from the shared `components::focused_output` cache (#496/#440).
            let focused = crate::components::focused_output::current();
            match resolve_open_page(*page) {
                PageAction::OpenBuiltin(target) => {
                    tracing::info!(plugin = %plugin_id, ?target, "plugin effect: OpenPage");
                    crate::modal::open_on_focused(focused.as_deref(), target);
                }
                PageAction::OpenPluginSelf => {
                    tracing::info!(plugin = %plugin_id, "plugin effect: OpenPage(PluginSelf)");
                    crate::modal::open_plugin_on_focused(focused.as_deref(), plugin_id);
                }
            }
        }
        Effect::Niri(action) => {
            // #648: the compositor leg, onto niri's existing fire-and-forget IPC
            // commands — the `Effect` → `do_thing` mapping the frontend-B spec
            // sprinted at. Reaching here means the plugin holds
            // `Capability::Niri`. Both actions address niri's own object ids; a
            // plugin that guesses one wrong gets niri's own no-op, so the host
            // does not second-guess the id (it has no cheaper truth than niri).
            tracing::info!(plugin = %plugin_id, ?action, "plugin effect: Niri");
            match *action {
                NiriAction::FocusWorkspace { id } => niri::focus_workspace(id),
                NiriAction::FocusWindow { id } => niri::focus_window(id),
            }
        }
        Effect::Media(action) => {
            // #648: the transport leg. The wire action carries no player — the
            // vocabulary is deliberately player-agnostic — so the host resolves
            // the target, and resolves it the same way the bar chip does.
            // Reaching here means the plugin holds `Capability::Media`.
            broker_media(plugin_id, *action);
        }
        Effect::Audio(action) => {
            // #648: the audio leg, onto the *default sink* (the same target the
            // volume chip drives), never a plugin-named device: the wire action
            // names no sink, and picking one for the plugin would be host policy
            // invented out of nothing. Reaching here means the plugin holds
            // `Capability::Audio`.
            broker_audio(plugin_id, *action);
        }
        Effect::RaiseOsd { title, body, icon } => {
            tracing::info!(plugin = %plugin_id, title = %title, "plugin effect: RaiseOsd");
            crate::overlays::osd::nudge(title, body, icon.as_deref());
        }
        Effect::Notify { summary, body } => {
            // trollshell owns `org.freedesktop.Notifications`, so a plugin toast
            // is injected through the shell's own local-post path (#227) rather
            // than a D-Bus round-trip — same rendering as an external `Notify`
            // (history, DND gating, rate-limiting). Attributed to the plugin id
            // as the app name. `Normal` urgency: a plugin alert is informational,
            // not error-scope, so DND may hold it (see `post_local`'s docs).
            tracing::info!(plugin = %plugin_id, summary = %summary, "plugin effect: Notify");
            notifications::post_local(plugin_id, summary, body, notifications::Urgency::Normal);
        }
        Effect::RequestConsent {
            request_id,
            agent,
            datasource,
            scope,
            detail,
        } => {
            // #487 phase 1b: raise the interactive consent overlay on the focused
            // output and route the human's decision back to THIS plugin as
            // `HostMsg::ConsentDecision`. Reaching here means the plugin holds
            // `Capability::Consent` (`enforce_capabilities` drops the effect
            // otherwise), so the `ConsentDecision` reply only ever goes to a
            // connection that can decode it — the #305 opt-in gate, enforced
            // upstream.
            tracing::info!(plugin = %plugin_id, %agent, %datasource, "plugin effect: RequestConsent");
            crate::overlays::consent::request(
                *request_id,
                agent,
                datasource,
                scope,
                detail,
                outbound.clone(),
            );
        }
        Effect::RunCommand { id, argv, detached } => {
            // #510: spawn the granted `argv` on the tokio runtime and route the
            // outcome back to THIS plugin as `HostMsg::EffectResult` keyed by
            // `id`. Reaching here means the plugin holds `Capability::RunCommand`
            // (`enforce_capabilities` drops it otherwise) — the highest-trust cap
            // in the vocabulary, so the host runs exactly what the manifest allows.
            //
            // #953 splits it in two. The attached mode is #510's: run to
            // completion under `RUN_COMMAND_TIMEOUT`, `kill_on_drop`, exit status
            // back. The detached mode bypasses *both* — no `output()`, no
            // timeout, no kill — and hands the program to the systemd user
            // manager so it outlives a `trollshell.service` restart.
            if *detached {
                dispatch_detached_run_command(
                    plugin_id,
                    *id,
                    argv,
                    outbound,
                    detached_unit.take(),
                );
            } else {
                tracing::info!(plugin = %plugin_id, id = *id, argc = argv.len(), "plugin effect: RunCommand");
                run_command(plugin_id, *id, argv.clone(), outbound.clone());
            }
        }
        Effect::DatasourceQuery {
            request_id,
            provider,
            scope,
            params,
        } => {
            // #509: host-routed to the providing connection. Reaching here means the
            // plugin holds `Capability::DatasourceQuery` (`enforce_capabilities`
            // drops it otherwise). The router validates a provider is connected +
            // serves the scope, parks the requester keyed by a host correlation, and
            // forwards the query; a missing provider / denied scope / 10 s timeout
            // synthesizes a `Failed` result back to `outbound`, so the requester
            // never hangs.
            tracing::info!(plugin = %plugin_id, %provider, %scope, request_id = *request_id, "plugin effect: DatasourceQuery");
            datasource.route_query(
                plugin_id.to_owned(),
                *request_id,
                provider.clone(),
                scope.clone(),
                params.clone(),
                outbound.clone(),
            );
        }
        Effect::DatasourceResult {
            request_id,
            outcome,
        } => {
            // #509: a provider's answer, keyed by the opaque host correlation the
            // host forwarded (echoed verbatim here, NOT the requester's token).
            // Reaching here means the plugin holds `Capability::DatasourceProvider`.
            // The router maps the correlation back to the parked requester and its
            // original `request_id`; an unknown/expired correlation is dropped, and
            // (#553) so is one echoed by any plugin other than the provider the query
            // was routed to — `plugin_id` is that identity check.
            tracing::info!(plugin = %plugin_id, request_id = *request_id, "plugin effect: DatasourceResult");
            datasource.deliver_result(*request_id, plugin_id.to_owned(), outcome.clone());
        }
    }
}

/// The `Some`/`None` split on [`detached_launch_unit_for_audit`]'s result, for
/// a detached [`Effect::RunCommand`] (#953). Pulled out of [`broker_effect`]
/// itself only to keep that function under clippy's line cap; the logic and
/// its rationale live here (mirrors why [`detached_launch_unit_for_audit`]
/// itself is a separate function).
///
/// `detached_unit` is `Some` exactly when the id is unit-name safe — the
/// audit line and the `--unit=` are guaranteed to be one string on that arm.
/// For a rejected id it is `None`, and the `None` arm below DOES fire (#964
/// M-2 review: it did not before that fix, and a comment here used to claim
/// it couldn't) — [`launch_detached`]'s signature takes an owned `String`,
/// not an `Option`, so *something* has to be recomputed to hand it. That
/// recompute is written rather than an `expect`/panic because a panic here
/// runs on the GTK main thread and would take the whole shell down; the worst
/// it can do, now, is name a unit to nobody at all — not the audit line
/// (already true before the M-2 fix) and, as of that fix, not this tracing
/// line either. `start_detached`'s own `is_valid_plugin_id` guard means it is
/// never asked of systemd regardless.
fn dispatch_detached_run_command(
    plugin_id: &str,
    id: u64,
    argv: &[String],
    outbound: &mpsc::Sender<HostMsg>,
    detached_unit: Option<String>,
) {
    if let Some(unit) = detached_unit {
        #[cfg(test)]
        tests::TRACING_UNIT.with(|cell| *cell.borrow_mut() = Some(unit.clone()));
        tracing::info!(plugin = %plugin_id, id, argc = argv.len(), unit = %unit, "plugin effect: RunCommand (detached launch)");
        launch_detached(plugin_id, id, unit, argv.to_vec(), outbound.clone());
    } else {
        #[cfg(test)]
        tests::TRACING_UNIT.with(|cell| *cell.borrow_mut() = None);
        tracing::info!(
            plugin = %plugin_id, id, argc = argv.len(),
            "plugin effect: RunCommand (detached launch; id is not unit-name \
             safe, direct-spawn fallback)",
        );
        launch_detached(
            plugin_id,
            id,
            allocate_launch_unit(plugin_id, id),
            argv.to_vec(),
            outbound.clone(),
        );
    }
}

// ── Media / audio legs (#648) ────────────────────────────────────────────────

/// Send one wire [`MediaAction`] to the MPRIS player the shell currently treats
/// as active (#648).
///
/// The target is [`mpris::active_bus_name`] — a live manual pin if the user made
/// one, else the Playing > Paused > first heuristic — so a plugin's transport
/// action and a click on the bar chip's own buttons always drive the same
/// player. With **no** player tracked there is nothing to address: the action is
/// skipped with a `warn`, never silently. It is a fire-and-forget effect (no
/// `EffectResult` leg in the vocabulary), so the host log is the only signal
/// there is — which is precisely why it has to be a loud one.
fn broker_media(plugin_id: &str, action: MediaAction) {
    let Some(bus) = mpris::active_bus_name() else {
        tracing::warn!(
            plugin = %plugin_id, ?action,
            "plugin effect: Media with no active player; skipped",
        );
        return;
    };
    tracing::info!(plugin = %plugin_id, ?action, player = %bus, "plugin effect: Media");
    match action {
        MediaAction::PlayPause => mpris::play_pause(&bus),
        MediaAction::Next => mpris::next(&bus),
        MediaAction::Previous => mpris::previous(&bus),
    }
}

/// Apply one wire [`AudioAction`] to the default sink (#648). `SetVolume` is
/// bounds-checked through [`clamp_volume`] before it reaches the audio service;
/// `ToggleMute` needs no argument validation.
fn broker_audio(plugin_id: &str, action: AudioAction) {
    match action {
        AudioAction::SetVolume(requested) => {
            let Some(linear) = clamp_volume(requested) else {
                tracing::warn!(
                    plugin = %plugin_id, requested,
                    "plugin effect: Audio SetVolume with a non-finite level; skipped",
                );
                return;
            };
            if !(MIN_VOLUME..=MAX_VOLUME).contains(&requested) {
                tracing::warn!(
                    plugin = %plugin_id, requested, applied = linear,
                    "plugin effect: Audio SetVolume outside the wire-documented range; clamped",
                );
            }
            tracing::info!(plugin = %plugin_id, linear, "plugin effect: Audio SetVolume");
            pipewire::set_volume(linear);
        }
        AudioAction::ToggleMute => {
            tracing::info!(plugin = %plugin_id, "plugin effect: Audio ToggleMute");
            pipewire::toggle_mute();
        }
    }
}

/// The wire-documented bounds of [`AudioAction::SetVolume`] ("`0.0..=1.0`").
const MIN_VOLUME: f64 = 0.0;
const MAX_VOLUME: f64 = 1.0;

/// Bounds-check a plugin-requested linear volume (#648). Pure, so the host's
/// policy on a hostile or buggy level is unit-testable without an audio daemon.
///
/// The host is the chokepoint between an arbitrary same-user process and the
/// default sink, and the `f64` off the wire can be anything. The audio service
/// writes it through as a per-channel **linear gain** in the SPA pod without
/// re-checking it, so this is where it gets checked: a level outside the
/// documented `0.0..=1.0` is **clamped** (the plugin asked for "as loud as
/// possible" and gets exactly that, not a 5× blast), and a non-finite one is
/// **rejected** — `NaN`/`inf` has no defensible clamp and nothing sane to send.
fn clamp_volume(requested: f64) -> Option<f64> {
    requested
        .is_finite()
        .then(|| requested.clamp(MIN_VOLUME, MAX_VOLUME))
}

/// Map a wire [`Page`] onto the host's `modal::Page`, reading the runtime Stats
/// layout (#508) for the one page that isn't 1:1. Thin wrapper over the pure
/// [`map_page_for_layout`] so the layout-independent arms stay unit-testable
/// without touching the env.
pub(super) fn map_page(page: Page) -> crate::modal::Page {
    map_page_for_layout(page, crate::panels::stats::stats_layout())
}

/// Pure core of [`map_page`]: map a wire [`Page`] onto the host's `modal::Page`
/// for a given [`crate::panels::stats::StatsLayout`]. The two enums mirror each
/// other 1:1 except `Stats`: the wire protocol only ever had a single `Stats`
/// page, so in the `split` layout (#508, which resurrects #307's five
/// per-resource pages) it lands on the CPU flyout (`StatsCpu`, the primary
/// stats page), the same approximation #307 made; in `combined`/`multicolumn`
/// it's an exact `Stats` match. Written exhaustively so a page added to either
/// side breaks the build here rather than silently mis-routing.
pub(super) fn map_page_for_layout(
    page: Page,
    layout: crate::panels::stats::StatsLayout,
) -> crate::modal::Page {
    use crate::modal::Page as M;
    use crate::panels::stats::StatsLayout;
    match page {
        Page::Media => M::Media,
        Page::Network => M::Network,
        Page::Vpn => M::Vpn,
        Page::Connections => M::Connections,
        Page::Bluetooth => M::Bluetooth,
        Page::Stats => match layout {
            StatsLayout::Split => M::StatsCpu,
            StatsLayout::Combined | StatsLayout::Multicolumn => M::Stats,
        },
        Page::Audio => M::Audio,
        Page::Power => M::Power,
        Page::PowerMenu => M::PowerMenu,
        Page::Notifications => M::Notifications,
        Page::Appearance => M::Appearance,
        Page::Displays => M::Displays,
        Page::Clipboard => M::Clipboard,
        Page::Calendar => M::Calendar,
        Page::Settings => M::Settings,
        // `PluginSelf` (#349 PR2) has no built-in `modal::Page`: it is
        // intercepted by `resolve_open_page` in the broker and routed to the
        // requesting plugin's own panel, so it never reaches `map_page`. The
        // arm documents the split and keeps the match exhaustive over wire
        // `Page` (a page added to either side still breaks the build here).
        Page::PluginSelf => unreachable!(
            "PluginSelf is intercepted by resolve_open_page and never mapped to a modal::Page",
        ),
    }
}

/// The host action a wire [`Effect::OpenPage`] resolves to (#349 PR2). Split out
/// as a **pure** function so the [`Page::PluginSelf`] interception — which has no
/// `modal::Page` counterpart — is unit-testable without GTK, the way [`map_page`]
/// is. The broker ([`broker_effect`]) calls this, then dispatches: a built-in
/// page opens by `modal::Page`; `PluginSelf` opens the requesting plugin's own
/// panel (keyed by the effect's plugin id, which the broker already carries).
pub(super) enum PageAction {
    OpenBuiltin(crate::modal::Page),
    OpenPluginSelf,
}

pub(super) fn resolve_open_page(page: Page) -> PageAction {
    match page {
        Page::PluginSelf => PageAction::OpenPluginSelf,
        other => PageAction::OpenBuiltin(map_page(other)),
    }
}

// ── RunCommand round-trip (#510) ─────────────────────────────────────────────

/// How long a plugin-spawned command may run before it is killed and reported
/// as a failed outcome (#510). Bounds a hung child so the plugin never waits
/// forever on its [`HostMsg::EffectResult`]; matches the hooks runner's bound.
const RUN_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on captured stdout returned to a plugin (bytes, #510). The proto lets the
/// host truncate [`EffectOutcome::output`]; keep a single reply frame small.
const RUN_COMMAND_MAX_OUTPUT: usize = 4096;

/// Spawn a plugin-requested `argv` on the tokio runtime and route the
/// [`EffectOutcome`] back to the originating plugin as [`HostMsg::EffectResult`]
/// keyed by `id` (#510). Capability-gated upstream
/// ([`RunCommand`](hytte_plugin_proto::Capability::RunCommand)); this runs only
/// for a granted plugin. The broker itself stays on the GTK main thread, so the
/// actual `spawn` + wait is offloaded to the runtime. Spawn/exec failures are
/// loud (a warn) and still return an `ok: false` outcome — the same "no silent
/// swallow" hygiene as the recorder's spawn path (#523) — so a plugin awaiting a
/// reply never hangs.
fn run_command(plugin_id: &str, id: u64, argv: Vec<String>, outbound: mpsc::Sender<HostMsg>) {
    let plugin_id = plugin_id.to_owned();
    hytte::reactive::runtime::handle().spawn(async move {
        let outcome = execute_command(&plugin_id, id, &argv).await;
        // A one-shot reply we want *delivered* (unlike latest-wins state pushes):
        // `send().await` waits for outbound capacity, and only fails once the
        // connection's writer is gone — at which point the plugin is already
        // leaving, so dropping the reply is correct.
        if outbound
            .send(HostMsg::EffectResult { id, outcome })
            .await
            .is_err()
        {
            tracing::debug!(plugin = %plugin_id, id, "plugin gone before RunCommand result; dropped");
        }
    });
}

/// Run one `argv` to completion (bounded by [`RUN_COMMAND_TIMEOUT`]) and map it
/// onto an [`EffectOutcome`]. stdin is `/dev/null`; stdout/stderr are captured.
pub(super) async fn execute_command(plugin_id: &str, id: u64, argv: &[String]) -> EffectOutcome {
    let Some((program, tail)) = argv.split_first() else {
        tracing::warn!(plugin = %plugin_id, id, "RunCommand with empty argv; nothing to spawn");
        return EffectOutcome {
            ok: false,
            output: None,
        };
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(tail)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(RUN_COMMAND_TIMEOUT, cmd.output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                tracing::info!(plugin = %plugin_id, id, program = %program, "plugin RunCommand finished");
            } else {
                tracing::warn!(
                    plugin = %plugin_id, id, status = ?output.status,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "plugin RunCommand exited non-zero",
                );
            }
            command_outcome(output.status.success(), &output.stdout)
        }
        Ok(Err(e)) => {
            tracing::warn!(plugin = %plugin_id, id, program = %program, error = %e, "plugin RunCommand failed to spawn");
            EffectOutcome {
                ok: false,
                output: None,
            }
        }
        Err(_) => {
            tracing::warn!(
                plugin = %plugin_id, id, program = %program,
                timeout_s = RUN_COMMAND_TIMEOUT.as_secs(),
                "plugin RunCommand timed out; killed",
            );
            EffectOutcome {
                ok: false,
                output: None,
            }
        }
    }
}

/// Map a finished command's success flag + captured stdout onto the wire
/// [`EffectOutcome`] (#510). Pure (no process handle) so it is unit-testable:
/// trailing newlines are trimmed, empty stdout collapses to `None`, and output
/// past [`RUN_COMMAND_MAX_OUTPUT`] bytes is truncated on a char boundary.
fn command_outcome(success: bool, stdout: &[u8]) -> EffectOutcome {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim_end_matches(['\n', '\r']);
    let output = if trimmed.is_empty() {
        None
    } else {
        Some(truncate_on_char_boundary(trimmed, RUN_COMMAND_MAX_OUTPUT))
    };
    EffectOutcome {
        ok: success,
        output,
    }
}

// ── Detached launch (#953) ───────────────────────────────────────────────────
//
// `Effect::RunCommand { detached: true }` is the "launch a terminal that keeps
// running" mode (#947). It shares `Capability::RunCommand` with the attached
// mode above but shares *none* of its machinery, because every piece of that
// machinery is a reason the program would die:
//
// - `execute_command` awaits `cmd.output()`, so the effect task is alive for as
//   long as the program is;
// - `RUN_COMMAND_TIMEOUT` kills anything still running after 10 s;
// - `kill_on_drop(true)` kills it if the task is ever dropped;
// - and the child is a child of *this* process, so it sits in
//   `trollshell.service`'s cgroup and `KillMode=control-group` takes it down on
//   every shell restart.
//
// The detached path therefore does not "re-parent an otherwise identical
// spawn": it never calls `output()`/`wait()`, has no program timeout, sets no
// `kill_on_drop`, and asks the **systemd user manager** to own the process.
//
// ## Why a transient *service*, not `--scope`
//
// `systemd-run --user --scope` looks like the lighter option, but with `--scope`
// systemd-run "runs the command by systemd-run itself as parent process"
// (systemd-run(1)) — it stays in the foreground for the program's whole life.
// Awaiting its exit status would be awaiting the program, which is exactly the
// thing #953 says to bypass, and not awaiting it would mean the host learns
// nothing about whether the launch worked.
//
// Measured on this machine (systemd 260.2), which is what settles it rather than
// the man page:
//
//     systemd-run --user --quiet --collect --scope --unit=x.scope -- sleep 3
//         exit 0 after 3030 ms   (it waited out the program)
//     systemd-run --user --quiet --collect       --unit=x.service -- sleep 3
//         exit 0 after    7 ms   (it returned once the manager took the job)
//
// So a transient *service* has neither problem: its exit status is a launch
// verdict and nothing else, and the program's parent is `systemd --user` — a
// different process tree *and* a different cgroup from `trollshell.service`.
// It is also the exact groove `plugin_launcher.rs` already runs plugins in, and
// it gives the program a name in `systemctl --user`, which is what #953 asked
// for.

/// Bounds the `systemd-run` **launch call** — the short D-Bus round-trip that
/// asks the user manager to start the transient unit — and *nothing else*
/// (#953). It is emphatically not [`RUN_COMMAND_TIMEOUT`]'s sibling: the
/// launched program is never timed out, because outliving the shell is the
/// point.
///
/// Killing a wedged `systemd-run` is safe for the *unit* — the manager owns
/// anything it already took — but that safety does not extend to the *verdict*
/// (#953 L4). Past this deadline the host cannot tell whether the start job was
/// taken, so it does not guess and does not retry: retrying would be the one
/// action that can produce two copies of the program. The failure is reported as
/// an explicitly UNKNOWN outcome naming the unit
/// ([`LaunchFailure::Unknown`]), which the operator can settle with one
/// `systemctl --user status`. The measured round-trip is ~7 ms, so this is a
/// wedge guard, not a working bound.
///
/// Its only reader is [`start_detached`] — see the `allow(dead_code)` there
/// for why a plain `cargo test -p trollshell` (no `system-tests`) makes that
/// reader itself unreachable, and this constant along with it.
#[cfg_attr(all(test, not(feature = "system-tests")), allow(dead_code))]
const LAUNCH_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// The systemd slice every detached launch is placed in (#953, L6). Bounds the
/// units a plugin can accumulate to **one subtree** the user can see and clear:
/// `systemctl --user stop trollshell-launch.slice` stops every launched program
/// at once. systemd derives the parent from the dashes, so this nests as
/// `trollshell.slice/trollshell-launch.slice` and is created on demand.
///
/// A slice binds no lifetime, so the deliberate no-`PartOf=` decision stands —
/// this is a *grouping*, not a dependency. Measured: `--collect` still garbage-
/// collects a unit inside it (a `-- false` launch leaves `LoadState=not-found`
/// and the name is immediately reusable).
///
/// # The slice itself, once empty (#964 item 5)
///
/// Measured (systemd 260.2): once every unit inside it has been collected,
/// `trollshell-launch.slice` stays `loaded active` rather than stopping on its
/// own — a slice with no member processes just sits there; it does not go
/// `failed` and needs no `reset-failed`.
///
/// **Decision: leave it.** An idle slice is a cgroup with zero member
/// processes — the cost is one row in `systemctl --user list-units` and
/// whatever bytes the kernel keeps for an empty cgroup, not a leaked process,
/// fd, or timer. Actively `systemctl --user stop`-ing it once the last unit
/// collects would need the broker to track "is this the last unit in the
/// slice", which is exactly the bookkeeping the manager already owns — and
/// stopping it would undercut the one thing the slice is *for*: a single
/// stable name (`systemctl --user stop trollshell-launch.slice`) that always
/// reaches every currently-running detached launch, launched before or after
/// this moment. If a concrete cost ever turns up, this comment is where to
/// revisit the call.
pub(super) const LAUNCH_SLICE: &str = "trollshell-launch.slice";

/// Why a detached launch took the direct-spawn fallback instead of the systemd
/// user manager (#953, L3). Reported to the plugin, so "no systemd-run" is never
/// claimed for a launch that never consulted systemd-run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FallbackReason {
    /// `systemd-run` could not be executed at all — not on `$PATH`.
    NoSystemdRun,
    /// `systemd-run` ran but could not reach a user manager (no
    /// `$DBUS_SESSION_BUS_ADDRESS`/`$XDG_RUNTIME_DIR`, no user session). It
    /// therefore started nothing, so spawning directly starts the program
    /// exactly once.
    NoUserManager,
    /// The plugin's manifest id is not unit-name safe, so no `--unit=` could be
    /// built for it. systemd-run was never consulted.
    UnsafePluginId,
}

impl FallbackReason {
    /// The phrase shown to the plugin in [`EffectOutcome::output`].
    fn as_str(self) -> &'static str {
        match self {
            FallbackReason::NoSystemdRun => "no systemd-run",
            FallbackReason::NoUserManager => "no systemd user manager",
            FallbackReason::UnsafePluginId => "plugin id is not unit-name safe",
        }
    }
}

/// What a detached launch actually started (#953), so the host can say which of
/// the two paths ran rather than reporting a bare boolean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum LaunchReport {
    /// The normal path: `systemd-run --user` started this transient unit, owned
    /// by the systemd user manager. Survives a `trollshell.service` restart.
    Unit(String),
    /// The fallback path: the program was spawned directly into **its own
    /// process group**, for the stated reason. It still isn't awaited, killed or
    /// timed out, but it is a child of this process, so it only survives what an
    /// orphaned child survives.
    Process { pid: u32, reason: FallbackReason },
}

/// Host-allocated uniquifier for detached unit names (#953, M2). Monotonic for
/// the life of this process; see [`allocate_launch_unit`] for why the pid is in
/// the name too.
static LAUNCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The transient unit name for one detached launch (#953):
/// `trollshell-launch-<plugin>-<id>-<pid>-<seq>.service`. Pure — the caller
/// supplies both uniquifiers, so the exact name is unit-testable.
///
/// `plugin_id` is guarded by [`systemd::is_valid_plugin_id`] before the name
/// reaches `--unit=` (see [`start_detached`]) and the rest are integers, so a
/// used name is always legal, and bounded well under systemd's 255: 18 + 64
/// (the id cap) + 3 separators + 20 + 7 + 20 + 8 ≈ 140.
///
/// # Why the name is not just `<plugin>-<id>` (M2)
///
/// `id` is the **plugin's** correlation token for [`HostMsg::EffectResult`], and
/// a plugin's counter restarts when the plugin does — a stop/start from the
/// control-center, or a crash plus an SDK redial. The host allocates no id of
/// its own there ([`super::session`] takes `manifest.id` from a fresh
/// connection as-is), so a terminal launched as `id: 1`, a plugin restart, and a
/// second launch as `id: 1` used to collide with the *still-running* first unit
/// — systemd answers `Unit … was already loaded or has a fragment file` and the
/// plugin got `ok: false` for a launch that was only unlucky in its naming.
///
/// So the host contributes the uniqueness the plugin cannot:
///
/// - `seq` — a process-monotonic counter, which settles every collision inside
///   one shell process (the plugin-restart case above);
/// - `pid` — this shell's pid, which settles the case `seq` cannot: detached
///   units *outlive a shell restart* by design, so a fresh shell's `seq` starts
///   at 0 again while last shell's `…-0.service` may still be running.
///
/// The residual is pid reuse while a launched program is still alive, which
/// needs the pid space to wrap; it is not eliminated, and if it ever happens the
/// host reports systemd's refusal honestly rather than starting a second copy.
/// The plugin id and effect id stay in the name so a unit is still traceable
/// back to who asked for it and to its `EffectResult`.
pub(super) fn launch_unit_name(plugin_id: &str, id: u64, pid: u32, seq: u64) -> String {
    format!("trollshell-launch-{plugin_id}-{id}-{pid}-{seq}.service")
}

/// [`launch_unit_name`] with the host's uniquifiers filled in — a fresh, unique
/// name on every call (#953, M2). Not pure (it bumps [`LAUNCH_SEQ`]), which is
/// why the formatting half is kept separate and testable.
pub(super) fn allocate_launch_unit(plugin_id: &str, id: u64) -> String {
    let seq = LAUNCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    launch_unit_name(plugin_id, id, std::process::id(), seq)
}

/// The full `systemd-run` argv for one detached launch (#953), sans the
/// `systemd-run` program itself. Pure, so the exact invocation is pinned by a
/// unit test rather than only observable on a live session.
///
/// - `--user`: the session manager, so the unit lands in the user's own tree.
/// - `--quiet`: no "Running as unit …" chatter on stderr.
/// - `--collect`: release the unit even when the program ends failed, so a name
///   is never wedged waiting for a `reset-failed` (same reason
///   `plugin_launcher::systemd_run_args` passes it).
/// - `--slice=`: [`LAUNCH_SLICE`], so every surviving unit lands in one subtree
///   the user can stop with a single command (#953 L6).
/// - `--unit=`: the caller's [`allocate_launch_unit`] name — #953 wants the
///   program to *show up in `systemctl --user` with a name*.
/// - `--description=`: a human line in `systemctl --user status`.
/// - `--setenv=`: the [`FORWARDED_ENV`] variables the *shell* holds, see below.
/// - `--`: terminates option parsing before the plugin-supplied argv, so an
///   `argv[0]` of `--now` can't be read as a `systemd-run` flag.
///
/// Deliberately **not** passed: no `Restart=` (a launched terminal that exits
/// has finished, it is not a supervised service — unlike a plugin), and no
/// `PartOf=` (the program is the user's, launched on their behalf; binding its
/// lifetime to a target would be host policy invented out of nothing, and the
/// user manager already stops everything at logout).
///
/// # Environment (#953 L5)
///
/// Production relies on the **user manager's** environment: `etc/niri/session.kdl`
/// runs `systemctl --user import-environment WAYLAND_DISPLAY XDG_CURRENT_DESKTOP`
/// at session start, so a transient unit finds the display exactly the way a
/// launcher-started app does. That import is the load-bearing mechanism and
/// nothing here replaces it.
///
/// It is not, however, sufficient. It does not carry `NIRI_SOCKET` (so a
/// launched program cannot drive niri IPC) or `DISPLAY` (so an X11-only app
/// finds no display), and in a hand-started dev loop — `cargo run -p trollshell`
/// inside a nested compositor — the unit inherits the *outer* session's
/// variables rather than the shell's, so a launch lands on the wrong compositor.
/// So `env` (built by [`forwarded_env`] from **this shell's own** environment,
/// and empty for any variable the shell doesn't have) is passed explicitly.
/// `--setenv=` overrides the manager's value for those names only; every other
/// variable still comes from the manager.
pub(super) fn launch_argv(
    plugin_id: &str,
    id: u64,
    unit: &str,
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut out = vec![
        "--user".to_owned(),
        "--quiet".to_owned(),
        "--collect".to_owned(),
        format!("--slice={LAUNCH_SLICE}"),
        format!("--unit={unit}"),
        format!("--description=trollshell plugin launch: {plugin_id} #{id}"),
    ];
    for (k, v) in env {
        out.push(format!("--setenv={k}={v}"));
    }
    out.push("--".to_owned());
    out.extend(argv.iter().cloned());
    out
}

/// The environment variables a detached launch forwards from the shell's own
/// process when it has them (#953 L5) — see [`launch_argv`] for why the user
/// manager's import is not enough on its own.
pub(super) const FORWARDED_ENV: [&str; 4] = [
    "WAYLAND_DISPLAY",
    "NIRI_SOCKET",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
];

/// Read [`FORWARDED_ENV`] out of this process's environment, skipping anything
/// unset or empty (so a launch never asserts an empty `DISPLAY=` over the
/// manager's real one) or non-UTF-8 (which `--setenv=` cannot carry). Order
/// follows [`FORWARDED_ENV`] so the argv is deterministic.
///
/// Thin over [`filter_forwarded_env`], which does the actual skip and is the
/// unit-testable half (#964 item 4) — mutating the real process environment in
/// a test needs `std::env::set_var`, which edition 2024 marks `unsafe` (and is
/// process-global besides), so the filter is pulled out pure and fed a fake
/// lookup instead.
fn forwarded_env() -> Vec<(String, String)> {
    filter_forwarded_env(&FORWARDED_ENV, |name| std::env::var(name).ok())
}

/// The empty/unset/non-UTF-8 skip [`forwarded_env`] applies (#953 L5, #964
/// item 4), pulled out as a pure function over an injected `lookup` so it is
/// testable without touching the process environment. `lookup` returns `None`
/// for a name that is unset or not valid UTF-8 (exactly what
/// `std::env::var(name).ok()` already collapses both cases to); `Some("")` is
/// distinct from `None` and is the case this filters out on top.
fn filter_forwarded_env(
    names: &[&str],
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            (!value.is_empty()).then(|| ((*name).to_owned(), value))
        })
        .collect()
}

/// Why a `systemd-run` launch didn't happen (#953). The distinction is
/// load-bearing, and it is exactly "did this start anything?":
///
/// - [`NothingStarted`](LaunchFailure::NothingStarted) — the helper never ran,
///   or ran and could not reach the manager. Nothing was started, so falling
///   back to a direct spawn starts the program **exactly once**.
/// - [`Refused`](LaunchFailure::Refused) — the manager answered and said no (a
///   taken unit name, a bad property). Retrying by another route could start a
///   **second** copy, so a refusal is reported, never worked around.
/// - [`Unknown`](LaunchFailure::Unknown) — the call timed out, so the host
///   cannot tell which of the two happened (#953 L4). Treated like a refusal:
///   never retried, and the message names the unit so the operator can look.
pub(super) enum LaunchFailure {
    NothingStarted(FallbackReason, String),
    Refused(String),
    Unknown(String),
}

/// Classify a `systemd-run` run that exited non-zero (#953 H1). Pure — takes
/// only what the process reported — so the classification is unit-testable
/// without a host that has no user manager.
///
/// # Why the predicate is on stderr and not the exit status
///
/// Measured on this machine (systemd 260.2): a bus-connect failure and a
/// unit-name collision **both exit `1`**, so the status carries no information
/// to discriminate on and the text is the only signal there is.
///
/// ```text
/// $ env -u XDG_RUNTIME_DIR -u DBUS_SESSION_BUS_ADDRESS systemd-run --user … -- sleep 5
/// Failed to connect to user scope bus via local transport: $DBUS_SESSION_BUS_ADDRESS
/// and $XDG_RUNTIME_DIR not defined (…)                             NOBUS_EXIT=1
/// $ systemd-run --user --unit=<taken>.service … -- sleep 10
/// Failed to start transient service unit: Unit <taken>.service was already
/// loaded or has a fragment file.                                 COLLIDE_EXIT=1
/// ```
///
/// The match is the **family**, `"Failed to connect to"` + `"bus"`, not that one
/// sentence: systemd has worded this several ways across versions (`Failed to
/// connect to bus: No medium found`, `… Connection refused`, and 260's `Failed
/// to connect to user scope bus via local transport: …`), and matching only the
/// literal 260 wording is how the fallback became unreachable in the first
/// place. A bus-connect failure provably started nothing — the helper never got
/// as far as `StartTransientUnit` — so it is `NothingStarted`, and the
/// documented "no systemd user manager" fallback is finally reachable on the
/// shape that actually occurs (a dev box or container with `systemd-run` on
/// `$PATH` and no user session).
pub(super) fn classify_systemd_run_failure(status: &str, stderr: &str) -> LaunchFailure {
    let stderr = stderr.trim();
    if stderr.contains("Failed to connect to") && stderr.contains("bus") {
        return LaunchFailure::NothingStarted(
            FallbackReason::NoUserManager,
            format!("systemd-run --user could not reach a user manager: {stderr}"),
        );
    }
    LaunchFailure::Refused(format!("systemd-run --user failed ({status}): {stderr}"))
}

/// Ask the systemd user manager to start `argv` as the transient unit `unit`
/// (#953). Returns as soon as the manager has taken the start job — the
/// program's own lifetime is never observed here.
///
/// The program to run and the launch-call timeout are parameters (#964 item
/// 3), not the hardcoded `"systemd-run"` / [`LAUNCH_CALL_TIMEOUT`] pair the
/// only production caller ([`start_detached`], via [`start_detached_with`])
/// fixes them at — a test can instead pass a fast stub program and a
/// millisecond-scale timeout to pin the never-retry guarantee on
/// [`LaunchFailure::Unknown`] without a slow real timeout or a real absent
/// user manager.
async fn systemd_run_launch_with(
    plugin_id: &str,
    id: u64,
    unit: &str,
    argv: &[String],
    program: &str,
    timeout: Duration,
) -> Result<(), LaunchFailure> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(launch_argv(plugin_id, id, unit, &forwarded_env(), argv))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Safe *here*, unlike on the launched program: this only bounds the
        // short-lived `systemd-run` helper. The transient unit it has already
        // asked for is the manager's, and is untouched by the helper dying.
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return Err(LaunchFailure::NothingStarted(
                FallbackReason::NoSystemdRun,
                format!("systemd-run could not be run: {e}"),
            ));
        }
        // #953 L4: by this point the manager may already have taken the start
        // job, so the host genuinely does not know whether the program is
        // running. Reporting it as a plain failure would invite the plugin to
        // retry with a fresh id and end up with two copies, so it is reported as
        // an *unknown* outcome naming the unit — the operator can settle it with
        // one `systemctl --user status`, and the host does not guess **or
        // retry** (#964 item 3 pins the no-retry half at the [`start_detached`]
        // call site, one level up, since retrying is a decision the *dispatch*
        // makes, not this helper).
        Err(_) => {
            return Err(LaunchFailure::Unknown(format!(
                "systemd-run --user did not answer within {}s; the launch outcome is \
                 UNKNOWN and was not retried (a retry could start a second copy) — \
                 check `systemctl --user status {unit}`",
                timeout.as_secs(),
            )));
        }
    };
    if output.status.success() {
        return Ok(());
    }
    Err(classify_systemd_run_failure(
        &output.status.to_string(),
        &String::from_utf8_lossy(&output.stderr),
    ))
}

/// Spawn `argv` directly, detached, when the systemd user manager could not
/// start it (#953) — `systemd-run` missing from `$PATH`, no user manager
/// reachable, or a plugin id that cannot be a unit-name segment. Every caller
/// passes the [`FallbackReason`] it observed, and it reaches the plugin: the
/// three cases are materially different and only one of them is "no
/// systemd-run".
///
/// [`process_group(0)`](tokio::process::Command::process_group) puts the child
/// in a **new process group**, so a signal delivered to the shell's group (a
/// `Ctrl-C` in a `cargo run` terminal, say) doesn't reach it. That is the whole
/// detachment `unsafe`-free code can buy: `setsid`/double-fork would need
/// `pre_exec`, and `unsafe_code = "forbid"` is workspace policy. Without a user
/// manager there is no cgroup to escape either, so this is not a lesser version
/// of the systemd path so much as the best available answer where that path
/// doesn't exist — [`LaunchReport::Process`] says so to the plugin.
///
/// The handle is **dropped, never awaited**: no `output()`, no `wait()`, no
/// timeout, and `kill_on_drop` left at its default `false` so dropping it
/// cannot kill the program. Tokio's orphan reaper still collects the exit
/// status on `SIGCHLD`, so nothing ever waits on the child and no zombie
/// accumulates either.
fn spawn_detached(argv: &[String], reason: FallbackReason) -> Result<LaunchReport, String> {
    let Some((program, tail)) = argv.split_first() else {
        return Err("empty argv; nothing to launch".to_owned());
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(tail)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    let child = cmd
        .spawn()
        .map_err(|e| format!("spawning {program}: {e}"))?;
    let pid = child.id().ok_or_else(|| {
        format!("spawning {program}: the child was reaped before its pid was read")
    })?;
    drop(child);
    Ok(LaunchReport::Process { pid, reason })
}

/// Start one detached launch into the transient unit `unit` and report what it
/// produced (#953). Tries the systemd user manager first, and falls back to
/// [`spawn_detached`] exactly when **nothing was started** — `systemd-run`
/// missing, or a `systemd-run` that could not reach the manager (#953 H1). A
/// refusal from a manager that *answered*, and a call whose outcome is unknown,
/// are both returned as-is: retrying by another route risks a second copy of the
/// program (see [`LaunchFailure`]).
///
/// Its only production caller is [`launch_detached`], and that call site is
/// itself `#[cfg(not(test))]` (#964 M-3 review — kept out of test builds so
/// `broker_effect`'s own hermetic tests can't reach the real systemd user
/// manager). Its only test caller is the `system-tests`-gated integration
/// suite in `plugins::tests`, which calls this directly. So a plain
/// `cargo test -p trollshell` (`system-tests` off) compiles neither caller —
/// genuinely unreachable in *that one* build, not dead in any other.
#[cfg_attr(all(test, not(feature = "system-tests")), allow(dead_code))]
pub(super) async fn start_detached(
    plugin_id: &str,
    id: u64,
    unit: &str,
    argv: &[String],
) -> Result<LaunchReport, String> {
    start_detached_with(
        plugin_id,
        id,
        unit,
        argv,
        "systemd-run",
        LAUNCH_CALL_TIMEOUT,
    )
    .await
}

/// [`start_detached`] with the `systemd-run` program and its launch-call
/// timeout as parameters (#964 item 3) — the same seam as
/// [`systemd_run_launch_with`], threaded one level up so a test can exercise
/// the *dispatch* decision (never retrying a [`LaunchFailure::Unknown`]), not
/// only the bare classifier. Production reaches this only through
/// [`start_detached`], fixed at `"systemd-run"` / [`LAUNCH_CALL_TIMEOUT`].
async fn start_detached_with(
    plugin_id: &str,
    id: u64,
    unit: &str,
    argv: &[String],
    program: &str,
    timeout: Duration,
) -> Result<LaunchReport, String> {
    if argv.is_empty() {
        return Err("empty argv; nothing to launch".to_owned());
    }
    // The plugin id is spliced into a unit name, so it has to clear the same
    // charset guard the declarative launcher applies to ids read from
    // `plugins.json` (#419). A manifest id is plugin-supplied, so an id that
    // can't be a unit-name segment takes the direct-spawn path rather than
    // smuggling a crafted `--unit=` argument past `systemd-run`.
    if !systemd::is_valid_plugin_id(plugin_id) {
        tracing::warn!(
            plugin = %plugin_id, id,
            "detached launch from a plugin id that is not unit-name safe; spawning directly",
        );
        return spawn_detached(argv, FallbackReason::UnsafePluginId);
    }
    match systemd_run_launch_with(plugin_id, id, unit, argv, program, timeout).await {
        Ok(()) => Ok(LaunchReport::Unit(unit.to_owned())),
        Err(LaunchFailure::NothingStarted(reason, msg)) => {
            tracing::warn!(
                plugin = %plugin_id, id, detail = %msg,
                "systemd user manager unavailable; spawning the detached program directly",
            );
            spawn_detached(argv, reason)
        }
        // #964 item 3: never retried. `Unknown` in particular means the manager
        // may already have taken the start job — retrying here is exactly the
        // action that could produce a second copy of the program, so both
        // failure shapes are surfaced to the plugin as-is.
        Err(LaunchFailure::Refused(msg) | LaunchFailure::Unknown(msg)) => Err(msg),
    }
}

/// Map a detached launch's report onto the wire [`EffectOutcome`] (#953). Pure
/// (no process handle), so the reply a plugin sees is unit-testable.
///
/// `ok` is a **launch** verdict, not an exit status — the host never learns the
/// program's exit status, and the proto documents that split. `output` names
/// what was started so the human (and the plugin's own log line) can find it:
/// the unit for `systemctl --user status`, or the pid on the fallback path.
pub(super) fn launch_outcome(report: &Result<LaunchReport, String>) -> EffectOutcome {
    let (ok, text) = match report {
        Ok(LaunchReport::Unit(unit)) => (true, format!("launched unit {unit}")),
        // #953 L3: name the reason the fallback was taken. Claiming "no
        // systemd-run" for a rejected plugin id would be a false diagnosis —
        // systemd-run was there and simply never consulted.
        Ok(LaunchReport::Process { pid, reason }) => (
            true,
            format!(
                "launched pid {pid} ({}; detached process group)",
                reason.as_str(),
            ),
        ),
        Err(e) => (false, format!("launch failed: {e}")),
    };
    EffectOutcome {
        ok,
        output: Some(truncate_on_char_boundary(&text, RUN_COMMAND_MAX_OUTPUT)),
    }
}

/// Launch a plugin-requested `argv` **independently of the shell** and route the
/// launch verdict back as [`HostMsg::EffectResult`] keyed by `id` (#953).
/// Capability-gated upstream like [`run_command`]; the broker stays on the GTK
/// main thread, so the launch is offloaded to the runtime. The spawned task
/// finishes as soon as the launch verdict is known — it does not live as long as
/// the launched program, which is the whole difference from [`run_command`].
///
/// Under `#[cfg(test)]` the real launch below never runs (#964 M-3 review) —
/// only the capture write does. `broker_effect`'s own hermetic tests
/// (`detached_launch_audit_unit_matches_the_dispatched_unit`,
/// `rejected_plugin_id_records_no_phantom_unit`) need nothing past that
/// capture, and letting the real path run made the *default* `cargo test`
/// start a real transient systemd unit (and, without one, a real detached
/// process) as a side effect of every run — the opposite of "hermetic". No
/// test anywhere in `plugins::tests`, gated or not, drives a detached
/// `RunCommand` through `broker_effect`/`launch_detached`; every test that
/// exercises the launch itself calls [`start_detached`]/`start_detached_with`
/// directly instead, so nothing depends on the spawn below actually firing in
/// a test binary.
///
/// `unit` has to be owned (`String`, not `&str`): the `#[cfg(not(test))]`
/// block moves it into a `'static` future handed to
/// `hytte::reactive::runtime::handle().spawn`. In a `#[cfg(test)]` build that
/// block is compiled out and `unit` is only ever `.clone()`d for the capture
/// below, so clippy's `needless_pass_by_value` fires *only* in that
/// compilation — silenced narrowly rather than changing the signature the
/// real (non-test) caller needs.
#[cfg_attr(test, allow(clippy::needless_pass_by_value))]
fn launch_detached(
    plugin_id: &str,
    id: u64,
    unit: String,
    argv: Vec<String>,
    outbound: mpsc::Sender<HostMsg>,
) {
    // #964 item 1: captured synchronously, before the launch itself is handed
    // to the background runtime — this is the exact `unit` string
    // `broker_effect` decided on, so a test can compare it against the `unit=`
    // its `record_audit` call logged for the same effect without waiting on
    // (or faking) the async launch that follows.
    #[cfg(test)]
    tests::LAST_DETACHED_DISPATCH_UNIT.with(|cell| *cell.borrow_mut() = Some(unit.clone()));

    #[cfg(not(test))]
    {
        let plugin_id = plugin_id.to_owned();
        hytte::reactive::runtime::handle().spawn(async move {
            let report = start_detached(&plugin_id, id, &unit, &argv).await;
            match &report {
                Ok(LaunchReport::Unit(unit)) => {
                    tracing::info!(plugin = %plugin_id, id, unit = %unit, slice = LAUNCH_SLICE, "plugin detached launch started as a transient user unit");
                }
                Ok(LaunchReport::Process { pid, reason }) => {
                    tracing::info!(plugin = %plugin_id, id, pid, reason = reason.as_str(), "plugin detached launch spawned directly");
                }
                Err(e) => {
                    tracing::warn!(plugin = %plugin_id, id, error = %e, "plugin detached launch failed");
                }
            }
            let outcome = launch_outcome(&report);
            if outbound
                .send(HostMsg::EffectResult { id, outcome })
                .await
                .is_err()
            {
                tracing::debug!(plugin = %plugin_id, id, "plugin gone before detached launch result; dropped");
            }
        });
    }
    // In test builds `plugin_id`/`id`/`argv`/`outbound` are otherwise unused
    // past the capture above — see the doc on this function for why the real
    // launch is deliberately skipped rather than gated some other way.
    #[cfg(test)]
    let _ = (plugin_id, id, argv, outbound);
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 code point.
fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

// ── Persisted effect audit log (#510) ────────────────────────────────────────
//
// Every brokered effect — and every one dropped by capability enforcement (#436)
// or the rate cap (#435) — is appended to a bounded, rotating log file under XDG
// state (`$XDG_STATE_HOME/trollshell/effects-audit.log`), so the host's
// allow/deny decisions are reviewable after the fact rather than only visible in
// live `tracing` output. Writes are handed to a single background writer over an
// unbounded channel, so neither the GTK broker thread nor the tokio reader
// threads block on file IO.

/// Total-bytes cap per audit file before rotation (#510). Two files are kept —
/// the live `effects-audit.log` and one rotated `effects-audit.log.1` — so the
/// on-disk footprint is bounded to ~2× this at the effect vocabulary's low,
/// rate-capped write volume.
#[cfg(not(test))]
const MAX_AUDIT_BYTES: u64 = 256 * 1024;

/// The host's allow/deny decision on one effect, recorded in the audit log
/// (#510). `Allowed` effects reach the broker; the two `Dropped*` decisions are
/// recorded upstream at their drop sites in [`super::session`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuditDecision {
    /// Cleared capability enforcement + the rate cap; brokered.
    Allowed,
    /// Dropped: the plugin never declared the required capability (#436).
    DroppedUngranted,
    /// Dropped: the plugin exceeded its effect rate cap (#435).
    DroppedRateCap,
}

impl AuditDecision {
    fn as_str(self) -> &'static str {
        match self {
            AuditDecision::Allowed => "allowed",
            AuditDecision::DroppedUngranted => "dropped(ungranted-capability)",
            AuditDecision::DroppedRateCap => "dropped(rate-cap)",
        }
    }
}

/// The short, stable audit name for an effect kind (#510). Exhaustive over the
/// effect vocabulary so a new variant is a compile error here, mirroring
/// [`effect_capability`](super::session::effect_capability).
///
/// [`Effect::RunCommand`]'s two spawn modes get **two names** (#953). They share
/// one capability, so [`AuditDecision`] can't tell them apart — but they differ
/// in exactly the way an audit reader cares about: a `RunCommand(detached)` line
/// is a program the shell handed to the user manager and will *not* clean up,
/// so it is the one an after-the-fact review has to reconcile against
/// `systemctl --user list-units 'trollshell-launch-*'`.
fn effect_kind(effect: &Effect) -> &'static str {
    match effect {
        Effect::OpenPage(_) => "OpenPage",
        Effect::Niri(_) => "Niri",
        Effect::Media(_) => "Media",
        Effect::Audio(_) => "Audio",
        Effect::RunCommand { detached: true, .. } => "RunCommand(detached)",
        Effect::RunCommand { .. } => "RunCommand",
        Effect::RaiseOsd { .. } => "RaiseOsd",
        Effect::Notify { .. } => "Notify",
        Effect::RequestConsent { .. } => "RequestConsent",
        Effect::DatasourceQuery { .. } => "DatasourceQuery",
        Effect::DatasourceResult { .. } => "DatasourceResult",
    }
}

/// Format one audit line (#510): `<rfc3339> plugin=<id> effect=<kind>
/// decision=<decision>`, plus — for the effects that carry one — the
/// correlation `id=`, and for a detached launch the `unit=` and `slice=` it
/// started (#953 M1). Pure (timestamp injected) so the format is unit-testable.
///
/// The audit name `RunCommand(detached)` exists so a reviewer can reconcile the
/// log against `systemctl --user list-units 'trollshell-launch-*'` — which the
/// bare four-field line could not support, because ten launches from one plugin
/// produced ten byte-identical lines and the unit name is keyed on exactly the
/// fields the line omitted. `unit=` closes that; `slice=` names the one command
/// that clears whatever is left (`systemctl --user stop trollshell-launch.slice`).
///
/// Every interpolated value is either host-built or run through
/// [`sanitize_field`] — the plugin id and the unit name both embed
/// plugin-supplied text, so neither can inject whitespace or a newline and forge
/// a record.
pub(super) fn format_audit_line(
    ts: &str,
    plugin_id: &str,
    kind: &str,
    decision: AuditDecision,
    effect_id: Option<u64>,
    unit: Option<&str>,
) -> String {
    let id = sanitize_field(plugin_id);
    let d = decision.as_str();
    let mut line = format!("{ts} plugin={id} effect={kind} decision={d}");
    if let Some(effect_id) = effect_id {
        let _ = write!(line, " id={effect_id}");
    }
    if let Some(unit) = unit {
        let _ = write!(line, " unit={} slice={LAUNCH_SLICE}", sanitize_field(unit));
    }
    line
}

/// Replace control/whitespace characters with `_` so a value can't inject a
/// newline (and thus a forged log record) into the audit file.
fn sanitize_field(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Append one effect decision to the persisted audit log (#510). Non-blocking:
/// formats the line and hands it to the background writer over an unbounded
/// channel (which serializes the file IO + rotation off both the GTK broker
/// thread and the tokio reader threads). A no-op if the audit path can't be
/// resolved (no `$HOME`/`$XDG_STATE_HOME`).
/// `unit` is the transient unit a detached [`Effect::RunCommand`] is about to
/// start (#953 M1) — `None` for every other effect, and for the drop sites,
/// which never reach a launch.
pub(super) fn record_audit(
    plugin_id: &str,
    effect: &Effect,
    decision: AuditDecision,
    unit: Option<&str>,
) {
    // #964 item 1: in `#[cfg(test)]` builds `audit_sink()` is hard-wired to
    // `None` (see its doc — tests touch no filesystem), which meant the audit
    // line was never even *formatted* under test, let alone inspectable. The
    // line is now built unconditionally and stashed for the test module
    // before the real (production-only) sink decides whether to persist it,
    // so `broker_effect`'s callers can assert on the exact record it produced
    // — in particular, that a detached launch's `unit=` is the same string
    // the launcher was handed (see `tests::LAST_DETACHED_DISPATCH_UNIT`).
    let line = format_audit_line(
        &now_rfc3339(),
        plugin_id,
        effect_kind(effect),
        decision,
        audit_effect_id(effect),
        unit,
    );
    #[cfg(test)]
    tests::LAST_AUDIT_LINE.with(|cell| *cell.borrow_mut() = Some(line.clone()));
    if let Some(tx) = audit_sink() {
        let _ = tx.send(line);
    }
}

/// The plugin's own correlation token for the effects that carry one (#953 M1),
/// so an audit line can be tied to the `EffectResult` that answered it — and,
/// for a detached launch, to the unit named on the same line. `None` for the
/// fire-and-forget effects, which have nothing to correlate.
fn audit_effect_id(effect: &Effect) -> Option<u64> {
    match effect {
        Effect::RunCommand { id, .. } => Some(*id),
        _ => None,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The process-wide audit writer sink, initialized once on first use: resolves
/// the log path and spawns the background writer task. `None` if the path can't
/// be resolved (audit then no-ops).
#[cfg(not(test))]
fn audit_sink() -> Option<&'static mpsc::UnboundedSender<String>> {
    static SINK: std::sync::OnceLock<Option<mpsc::UnboundedSender<String>>> =
        std::sync::OnceLock::new();
    SINK.get_or_init(spawn_audit_writer).as_ref()
}

/// Hermetic in unit tests: never resolves a real path or spawns a writer, so the
/// per-connection tests that exercise `enforce_capabilities` / `throttle_effects`
/// touch no filesystem. The audit machinery itself is covered directly below
/// ([`AuditLog`] rotation, [`format_audit_line`]).
#[cfg(test)]
fn audit_sink() -> Option<&'static mpsc::UnboundedSender<String>> {
    None
}

#[cfg(not(test))]
fn spawn_audit_writer() -> Option<mpsc::UnboundedSender<String>> {
    let path = audit_log_path()?;
    let path_str = path.display().to_string();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let log = AuditLog {
        path,
        max_bytes: MAX_AUDIT_BYTES,
    };
    hytte::reactive::runtime::handle().spawn(async move {
        while let Some(line) = rx.recv().await {
            if let Err(e) = log.append(&line) {
                tracing::debug!(error = %e, "effect audit write failed");
            }
        }
    });
    tracing::info!(path = %path_str, "effect audit log active");
    Some(tx)
}

/// `$XDG_STATE_HOME/trollshell/effects-audit.log`, falling back to
/// `$HOME/.local/state/…` per the XDG base-dir spec. `None` if neither is set.
#[cfg(not(test))]
fn audit_log_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("trollshell").join("effects-audit.log"))
}

/// Bounded, rotating append-only audit file (#510). On each append, if adding the
/// line would push the live file past `max_bytes`, the live file is rotated to
/// `<path>.1` (replacing any previous rotation) and a fresh file is started —
/// bounding the on-disk footprint to ~2× `max_bytes`.
struct AuditLog {
    path: PathBuf,
    max_bytes: u64,
}

impl AuditLog {
    /// Append `line` (a newline is added), rotating first if needed. Returns the
    /// underlying IO error on failure (the caller logs it).
    fn append(&self, line: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let incoming = u64::try_from(line.len())
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        self.rotate_if_needed(incoming)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    fn rotate_if_needed(&self, incoming: u64) -> std::io::Result<()> {
        let current = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        if current > 0 && current.saturating_add(incoming) > self.max_bytes {
            std::fs::rename(&self.path, self.rotated_path())?;
        }
        Ok(())
    }

    fn rotated_path(&self) -> PathBuf {
        let mut p = self.path.clone().into_os_string();
        p.push(".1");
        PathBuf::from(p)
    }
}

#[cfg(test)]
mod tests {
    use super::DatasourceRouter;
    use super::{
        AuditDecision, AuditLog, EffectOutcome, FORWARDED_ENV, LaunchReport, MAX_VOLUME,
        MIN_VOLUME, RUN_COMMAND_MAX_OUTPUT, broker_effect, clamp_volume, command_outcome,
        effect_kind, filter_forwarded_env, format_audit_line, launch_outcome, start_detached_with,
        truncate_on_char_boundary,
    };
    use hytte_plugin_proto::{AudioAction, Effect, HostMsg, MediaAction, NiriAction, Page};
    use std::cell::RefCell;
    use std::time::Duration;
    use tokio::sync::mpsc;

    // #964 item 1 / M-2 review: test-only capture of what `record_audit`,
    // `launch_detached` and the `tracing::info!` call in `broker_effect`'s
    // `RunCommand` arm were each told, so a test can assert they all agree on
    // the unit name (or all agree there isn't one) without a real audit file,
    // a real log subscriber, or a real (or even completed) launch — see
    // `record_audit`'s, `launch_detached`'s and `broker_effect`'s own call
    // sites for the writes. `pub(super)` so the parent module's
    // `#[cfg(test)]` call sites can reach them as `tests::…`.
    thread_local! {
        pub(super) static LAST_AUDIT_LINE: RefCell<Option<String>> = const { RefCell::new(None) };
        pub(super) static LAST_DETACHED_DISPATCH_UNIT: RefCell<Option<String>> =
            const { RefCell::new(None) };
        /// What `broker_effect`'s `tracing::info!` call for a detached
        /// `RunCommand` was told to name as the unit (#964 M-2 review) —
        /// `Some(unit)` on the accepted-id arm, `None` on the rejected-id arm
        /// (which now omits the `unit = …` field entirely rather than log one
        /// nobody will find in `systemctl --user`). Not a real subscriber hook;
        /// mirrors `LAST_AUDIT_LINE`'s technique of writing at the call site.
        pub(super) static TRACING_UNIT: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    /// Clear all three #964 capture cells. Every test that reads them starts by
    /// calling this, so a prior test that ran on the same worker thread
    /// (thread-locals are per OS thread, and `cargo test` reuses threads across
    /// tests) can never leave a stale value behind.
    fn reset_captures() {
        LAST_AUDIT_LINE.with(|cell| *cell.borrow_mut() = None);
        LAST_DETACHED_DISPATCH_UNIT.with(|cell| *cell.borrow_mut() = None);
        TRACING_UNIT.with(|cell| *cell.borrow_mut() = None);
    }

    #[test]
    fn command_outcome_maps_success_and_stdout() {
        // Trailing newline trimmed; success flag preserved.
        assert_eq!(
            command_outcome(true, b"hello\n"),
            EffectOutcome {
                ok: true,
                output: Some("hello".to_owned()),
            },
        );
        // Empty stdout collapses to None, whatever the exit status.
        assert_eq!(
            command_outcome(false, b""),
            EffectOutcome {
                ok: false,
                output: None,
            },
        );
        assert_eq!(
            command_outcome(true, b"\n\n"),
            EffectOutcome {
                ok: true,
                output: None,
            },
        );
        // Non-zero exit with output: ok=false but the stdout still comes back.
        assert_eq!(
            command_outcome(false, b"partial"),
            EffectOutcome {
                ok: false,
                output: Some("partial".to_owned()),
            },
        );
    }

    #[test]
    fn command_outcome_truncates_long_output() {
        let big = vec![b'x'; RUN_COMMAND_MAX_OUTPUT * 2];
        let out = command_outcome(true, &big).output.expect("output present");
        assert_eq!(out.len(), RUN_COMMAND_MAX_OUTPUT);
    }

    #[test]
    fn truncate_on_char_boundary_never_splits_utf8() {
        // "é" is 2 bytes; a max landing mid-char must back up to a boundary.
        let s = "aééé";
        let t = truncate_on_char_boundary(s, 2);
        assert!(s.starts_with(&t));
        assert_eq!(t, "a"); // byte 2 is mid-é, so back up to byte 1
        // A max at/over the length returns the whole string.
        assert_eq!(truncate_on_char_boundary("abc", 10), "abc");
    }

    #[test]
    fn format_audit_line_shape_and_sanitizes_id() {
        assert_eq!(
            format_audit_line(
                "2026-07-24T00:00:00Z",
                "timer",
                "RunCommand",
                AuditDecision::Allowed,
                None,
                None,
            ),
            "2026-07-24T00:00:00Z plugin=timer effect=RunCommand decision=allowed",
        );
        // A hostile id with whitespace/newline can't forge a second record.
        let line = format_audit_line(
            "T",
            "bad\nid here",
            "Notify",
            AuditDecision::DroppedRateCap,
            None,
            None,
        );
        assert!(
            !line.contains('\n'),
            "sanitized id must not inject a newline"
        );
        assert_eq!(
            line,
            "T plugin=bad_id_here effect=Notify decision=dropped(rate-cap)"
        );
    }

    /// #648: the host clamps a plugin-requested level into the wire-documented
    /// `0.0..=1.0` and refuses a non-finite one outright, so an arbitrary `f64`
    /// off the socket can never reach the audio graph as-is.
    #[test]
    fn clamp_volume_bounds_the_level_and_rejects_non_finite() {
        // In range: applied verbatim.
        assert_eq!(clamp_volume(0.42), Some(0.42));
        assert_eq!(clamp_volume(MIN_VOLUME), Some(MIN_VOLUME));
        assert_eq!(clamp_volume(MAX_VOLUME), Some(MAX_VOLUME));
        // Out of range: clamped to the nearest bound, not dropped — the plugin
        // asked to go as loud/quiet as possible and gets exactly that.
        assert_eq!(clamp_volume(5.0), Some(MAX_VOLUME));
        assert_eq!(clamp_volume(-2.0), Some(MIN_VOLUME));
        // Non-finite: no defensible clamp, so refused.
        assert_eq!(clamp_volume(f64::NAN), None);
        assert_eq!(clamp_volume(f64::INFINITY), None);
        assert_eq!(clamp_volume(f64::NEG_INFINITY), None);
    }

    #[test]
    fn effect_kind_names_the_variants() {
        assert_eq!(effect_kind(&Effect::OpenPage(Page::Media)), "OpenPage");
        assert_eq!(
            effect_kind(&Effect::Niri(NiriAction::FocusWindow { id: 1 })),
            "Niri"
        );
        assert_eq!(effect_kind(&Effect::Media(MediaAction::PlayPause)), "Media");
        assert_eq!(
            effect_kind(&Effect::Audio(AudioAction::ToggleMute)),
            "Audio"
        );
        assert_eq!(
            effect_kind(&Effect::RunCommand {
                id: 1,
                argv: vec!["true".to_owned()],
                detached: false,
            }),
            "RunCommand",
        );
        // #953: the detached mode is a distinct audit name — same capability,
        // materially different consequence (a program the shell won't reap).
        assert_eq!(
            effect_kind(&Effect::RunCommand {
                id: 1,
                argv: vec!["foot".to_owned()],
                detached: true,
            }),
            "RunCommand(detached)",
        );
        assert_eq!(
            effect_kind(&Effect::Notify {
                summary: String::new(),
                body: String::new(),
            }),
            "Notify",
        );
    }

    #[test]
    fn audit_log_rotates_and_bounds_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("effects-audit.log");
        let log = AuditLog {
            path: path.clone(),
            max_bytes: 200,
        };
        // Each line ~30 bytes; 100 of them far exceed the 200-byte cap, forcing
        // rotation while keeping the live file bounded.
        for i in 0..100 {
            log.append(&format!("2026-07-24T00:00:00Z line number {i}"))
                .expect("append");
        }
        let live = std::fs::metadata(&path).expect("live file").len();
        assert!(
            live <= 260,
            "live file should stay near the cap, was {live}"
        );
        let rotated = log.rotated_path();
        assert!(rotated.exists(), "a rotated .1 file should exist");
        // The most recent line is in the live file, not lost to rotation.
        let last = std::fs::read_to_string(&path).expect("read live");
        assert!(
            last.contains("line number 99"),
            "live file keeps the newest line"
        );
    }

    #[test]
    fn audit_log_creates_missing_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir
            .path()
            .join("state")
            .join("trollshell")
            .join("effects-audit.log");
        let log = AuditLog {
            path: path.clone(),
            max_bytes: 1024,
        };
        log.append("first").expect("append creates parents");
        let contents = std::fs::read_to_string(&path).expect("read");
        assert_eq!(contents, "first\n");
    }

    // ── #964: four detached-launch mechanisms PR #960's review found unpinned ──

    /// #964 item 1 (M1's residue): the unit `broker_effect` allocates for a
    /// detached launch is handed to BOTH `record_audit` (as its
    /// `unit=`/`slice=`) and the launcher — nothing before this test enforced
    /// they stay the same string. Captures both synchronously (no need to
    /// await, or fake, the background launch itself: `launch_detached` stashes
    /// its `unit` parameter before it ever calls `.spawn`) and compares.
    ///
    /// Falsifies the exact M1 defect the second-pass review reproduced by
    /// hand: passing `record_audit` a `None`, or a freshly-reallocated name,
    /// instead of the string `launch_detached` actually receives.
    #[test]
    fn detached_launch_audit_unit_matches_the_dispatched_unit() {
        reset_captures();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let router = DatasourceRouter::default();

        broker_effect(
            "caw",
            &Effect::RunCommand {
                id: 42,
                argv: vec!["true".to_owned()],
                detached: true,
            },
            &tx,
            &router,
        );

        let audit_line = LAST_AUDIT_LINE
            .with(|cell| cell.borrow().clone())
            .expect("a detached RunCommand must record an audit line");
        let dispatched_unit = LAST_DETACHED_DISPATCH_UNIT
            .with(|cell| cell.borrow().clone())
            .expect("a detached RunCommand must reach launch_detached");
        let audit_unit = audit_line
            .split("unit=")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("audit line carries no unit=: {audit_line}"));

        assert_eq!(
            audit_unit, dispatched_unit,
            "the audit unit= must be the exact string the launcher was handed \
             (audit line: {audit_line:?})",
        );
    }

    /// #964 item 1, second hop (M-1 review): the unit `start_detached` is
    /// handed is the unit that reaches `--unit=` in the argv `systemd-run` is
    /// actually invoked with. `detached_launch_audit_unit_matches_the_dispatched_unit`
    /// above pins `record_audit` ⇄ `launch_detached`'s *parameter*; the M1
    /// defect reintroduced one hop lower, inside the argv builder call itself,
    /// left that test green (and green on a CI sandbox with no user manager,
    /// where the gated system-tests suite's behavioural check also can't see
    /// it) — this closes that hop by reading the argv a stub actually received
    /// back off disk, using the same injectable-program seam item 3 already
    /// added, so it stays hermetic.
    #[tokio::test]
    async fn the_dispatched_unit_reaches_the_systemd_run_argv() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let argv_file = dir.path().join("argv");
        let stub = dir.path().join("stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n",
                argv_file.display()
            ),
        )
        .expect("write stub");
        let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod stub");

        let unit = "trollshell-launch-ts964-argv-7.service";
        let report = start_detached_with(
            "caw",
            7,
            unit,
            &["true".to_owned()],
            stub.to_str().expect("tempdir path is UTF-8"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(report, Ok(LaunchReport::Unit(unit.to_owned())));

        let argv = std::fs::read_to_string(&argv_file).expect("the stub recorded its argv");
        assert!(
            argv.lines().any(|a| a == format!("--unit={unit}")),
            "the unit handed to start_detached must be the one systemd-run is \
             asked for: {argv}",
        );
    }

    /// #964 item 2: `is_valid_plugin_id` is now checked before
    /// `allocate_launch_unit` runs for the audit record (see `broker_effect`),
    /// so a plugin id that can't be a unit-name segment never logs a phantom
    /// `unit=`/`slice=` for a unit `start_detached` never actually asks
    /// systemd to create — it takes the direct-spawn fallback instead
    /// (`FallbackReason::UnsafePluginId`).
    #[test]
    fn rejected_plugin_id_records_no_phantom_unit() {
        reset_captures();
        let (tx, _rx) = mpsc::channel::<HostMsg>(4);
        let router = DatasourceRouter::default();

        broker_effect(
            "my plugin",
            &Effect::RunCommand {
                id: 1,
                argv: vec!["true".to_owned()],
                detached: true,
            },
            &tx,
            &router,
        );

        let audit_line = LAST_AUDIT_LINE
            .with(|cell| cell.borrow().clone())
            .expect("even a rejected id's effect is still audited");
        assert!(
            !audit_line.contains("unit="),
            "a rejected plugin id must not log a unit for a unit that was \
             never created: {audit_line}",
        );
        assert!(
            !audit_line.contains("slice="),
            "no unit means no slice either: {audit_line}",
        );

        // #964 M-2 review: the audit line was already clean before this fix —
        // what wasn't was the shell's own default-level `tracing::info!` line
        // for the same effect, which named the very unit the audit line
        // withheld (and did so unsanitized, with the id's raw space still in
        // it — not even a legal unit name). Assert that surface too.
        let tracing_unit = TRACING_UNIT.with(|cell| cell.borrow().clone());
        assert!(
            tracing_unit.is_none(),
            "a rejected plugin id must not name a unit in the tracing line \
             either — an operator reading the shell's own log would see a unit \
             `systemctl --user list-units` never has: {tracing_unit:?}",
        );
    }

    /// #964 item 3 / #953 L4: a `systemd-run` call that times out must be
    /// reported as `LaunchFailure::Unknown` and never retried — a retry is
    /// exactly the action that could start a second copy of the program,
    /// since the manager may already have taken the first start job. Drives
    /// `start_detached_with` against a stub that sleeps well past a
    /// millisecond-scale test timeout (never a slow real one) and counts its
    /// own invocations, so a reintroduced retry shows up as a second recorded
    /// invocation rather than as a timing flake.
    #[tokio::test]
    async fn detached_launch_timeout_never_retries() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("invocations");
        let stub = dir.path().join("stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho invoked >> '{}'\nsleep 3\n",
                counter.display()
            ),
        )
        .expect("write stub");
        let mut perms = std::fs::metadata(&stub).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).expect("chmod stub");

        let unit = "trollshell-launch-ts964-timeout-1.service";
        let report = start_detached_with(
            "caw",
            1,
            unit,
            &["true".to_owned()],
            stub.to_str().expect("tempdir path is UTF-8"),
            Duration::from_millis(300),
        )
        .await;

        let outcome = launch_outcome(&report);
        assert!(
            !outcome.ok,
            "a call that timed out must never be reported as a successful launch",
        );
        let text = outcome
            .output
            .expect("a failed outcome still carries a message");
        assert!(
            text.contains("UNKNOWN"),
            "must name the outcome UNKNOWN, not a plain failure: {text}",
        );
        assert!(text.contains(unit), "must name the unit: {text}");

        // #964 L-1 review: `.expect`, not `.unwrap_or_default()` — a missing
        // file (the stub killed before its own `echo` landed) and a file
        // proving a retry are different failures, and collapsing the first
        // into "0 invocations" would print the retry-hazard message for what
        // is actually a scheduling artifact of this test, not of the code
        // under test. Measured headroom: 25 consecutive runs under concurrent
        // `cargo build` load, 25/25 green at 338-377ms (the stub sleeps 3s and
        // this test's own timeout is 300ms, so ~40ms is real slack for
        // `/bin/sh` + one `echo`) — comfortably not the flaky case this guards.
        let invocations = std::fs::read_to_string(&counter)
            .expect("the stub must have recorded its invocation before the timeout fired");
        assert_eq!(
            invocations.lines().count(),
            1,
            "a timed-out systemd-run call must be invoked exactly once — a \
             retry could start a second copy of the program: {invocations:?}",
        );
    }

    /// #964 item 4 / #953 L5: an empty (but *set*) env value must never reach
    /// the `--setenv=` argv — an empty `DISPLAY=` would override the user
    /// manager's real one with nothing, which is worse than not forwarding it
    /// at all. `filter_forwarded_env` is `forwarded_env`'s pure half
    /// (production goes through `std::env::var`, which edition 2024 makes
    /// awkward to fake without `unsafe`), so the empty-value skip is
    /// unit-testable via an injected lookup instead of mutating the real
    /// process environment.
    ///
    /// Fed `FORWARDED_ENV` itself (#964 L-2 review), not a hand-written name
    /// list — a prior version used its own three-name array, which had
    /// drifted from `FORWARDED_ENV`'s actual four in both order and
    /// membership, so neither the production list nor `forwarded_env`'s
    /// documented "order follows `FORWARDED_ENV`" claim was actually pinned.
    /// The full expected vector below pins both the skip *and* the order in
    /// one `assert_eq!`; there is deliberately no follow-on call into
    /// `launch_argv` — once `env` is asserted to equal
    /// `[("DISPLAY", ":0"), ("XDG_RUNTIME_DIR", "/run/user/1000")]`, it cannot
    /// contain a `WAYLAND_DISPLAY` entry for `launch_argv` to embed, so
    /// re-deriving an argv from it and asserting the same fact again is a
    /// tautology, not more coverage — `launch_argv`'s own embedding of an
    /// `env` list into `--setenv=` argv entries is pinned directly by
    /// `detached_launch_wraps_the_argv_in_a_systemd_run_service_unit` and
    /// `detached_launch_forwards_only_the_env_the_shell_has` (`tests.rs`).
    #[test]
    fn filter_forwarded_env_skips_empty_and_unset_values_and_preserves_order() {
        let env = filter_forwarded_env(&FORWARDED_ENV, |name| match name {
            "WAYLAND_DISPLAY" => Some(String::new()), // set, but empty — skipped
            "NIRI_SOCKET" => None,                     // unset — skipped
            "DISPLAY" => Some(":0".to_owned()),
            "XDG_RUNTIME_DIR" => Some("/run/user/1000".to_owned()),
            other => panic!("FORWARDED_ENV grew a name this fixture doesn't cover: {other}"),
        });
        assert_eq!(
            env,
            vec![
                ("DISPLAY".to_owned(), ":0".to_owned()),
                ("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned()),
            ],
            "must skip the empty/unset names and keep the rest in FORWARDED_ENV's order",
        );
    }

    /// #964 L-3 review (answering the question, not fixing anything):
    /// `filter_forwarded_env` skips only `is_empty()`, so a *whitespace-only*
    /// value (`DISPLAY=" "`, say) is kept and reaches `--setenv=DISPLAY= `.
    /// This matches `forwarded_env`'s documented contract ("skipping anything
    /// unset or empty … or non-UTF-8") and #953 L5's original wording — both
    /// say "empty", not "blank" — so this is deliberate, not an oversight:
    /// trimming would mean guessing what a padded value was supposed to mean
    /// to whatever reads `WAYLAND_DISPLAY`/`DISPLAY`, which is host policy
    /// invented out of nothing for a shape nothing here produces (the shell's
    /// own `std::env::var` never returns padded values for these names).
    #[test]
    fn filter_forwarded_env_forwards_a_whitespace_only_value() {
        let env = filter_forwarded_env(&["DISPLAY"], |_| Some(" ".to_owned()));
        assert_eq!(
            env,
            vec![("DISPLAY".to_owned(), " ".to_owned())],
            "a whitespace-only value is deliberately NOT treated as empty",
        );
    }
}
