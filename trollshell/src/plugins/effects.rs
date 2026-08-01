//! GTK-side effect broker (#277 / #349 PR2 / #436 / #487).
//!
//! Drained on the GTK main thread from the non-lossy effect channel, this maps
//! one wire [`Effect`] onto a real host command. Capability enforcement happens
//! **upstream** in the connection reader ([`super::session::enforce_capabilities`]),
//! so an effect arriving here is always one the plugin was granted.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use hytte::services::{mpris, niri, notifications, pipewire};
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
/// (→ a spawned `argv`, its outcome routed back as [`HostMsg::EffectResult`], #510),
/// [`Effect::RequestConsent`] (→ the interactive consent overlay, #487) and the two
/// datasource legs (#509).
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
pub(super) fn broker_effect(
    plugin_id: &str,
    effect: &Effect,
    outbound: &mpsc::Sender<HostMsg>,
    datasource: &DatasourceRouter,
) {
    // Every effect reaching the broker cleared capability enforcement + the rate
    // cap upstream (`session`), so it is an `Allowed` decision in the persisted
    // audit log (#510); the dropped ones are recorded at their `session` drop sites.
    record_audit(plugin_id, effect, AuditDecision::Allowed);
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
        Effect::RunCommand { id, argv } => {
            // #510: spawn the granted `argv` on the tokio runtime and route the
            // outcome back to THIS plugin as `HostMsg::EffectResult` keyed by
            // `id`. Reaching here means the plugin holds `Capability::RunCommand`
            // (`enforce_capabilities` drops it otherwise) — a separately granted,
            // higher-trust cap, so the host runs exactly what the manifest allows.
            tracing::info!(plugin = %plugin_id, id = *id, argc = argv.len(), "plugin effect: RunCommand");
            run_command(plugin_id, *id, argv.clone(), outbound.clone());
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
async fn execute_command(plugin_id: &str, id: u64, argv: &[String]) -> EffectOutcome {
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
fn effect_kind(effect: &Effect) -> &'static str {
    match effect {
        Effect::OpenPage(_) => "OpenPage",
        Effect::Niri(_) => "Niri",
        Effect::Media(_) => "Media",
        Effect::Audio(_) => "Audio",
        Effect::RunCommand { .. } => "RunCommand",
        Effect::RaiseOsd { .. } => "RaiseOsd",
        Effect::Notify { .. } => "Notify",
        Effect::RequestConsent { .. } => "RequestConsent",
        Effect::DatasourceQuery { .. } => "DatasourceQuery",
        Effect::DatasourceResult { .. } => "DatasourceResult",
    }
}

/// Format one audit line (#510): `<rfc3339> plugin=<id> effect=<kind>
/// decision=<decision>`. Pure (timestamp injected) so the format is
/// unit-testable. The plugin id is sanitized of control/whitespace characters so
/// a hostile id can't forge extra log lines.
fn format_audit_line(ts: &str, plugin_id: &str, kind: &str, decision: AuditDecision) -> String {
    let id = sanitize_field(plugin_id);
    let d = decision.as_str();
    format!("{ts} plugin={id} effect={kind} decision={d}")
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
pub(super) fn record_audit(plugin_id: &str, effect: &Effect, decision: AuditDecision) {
    if let Some(tx) = audit_sink() {
        let line = format_audit_line(&now_rfc3339(), plugin_id, effect_kind(effect), decision);
        let _ = tx.send(line);
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
    use super::{
        AuditDecision, AuditLog, EffectOutcome, MAX_VOLUME, MIN_VOLUME, RUN_COMMAND_MAX_OUTPUT,
        clamp_volume, command_outcome, effect_kind, format_audit_line, truncate_on_char_boundary,
    };
    use hytte_plugin_proto::{AudioAction, Effect, MediaAction, NiriAction, Page};

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
                AuditDecision::Allowed
            ),
            "2026-07-24T00:00:00Z plugin=timer effect=RunCommand decision=allowed",
        );
        // A hostile id with whitespace/newline can't forge a second record.
        let line = format_audit_line("T", "bad\nid here", "Notify", AuditDecision::DroppedRateCap);
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
            }),
            "RunCommand",
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
}
