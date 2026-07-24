//! GTK-side effect broker (#277 / #349 PR2 / #436 / #487).
//!
//! Drained on the GTK main thread from the non-lossy effect channel, this maps
//! one wire [`Effect`] onto a real host command. Capability enforcement happens
//! **upstream** in the connection reader ([`super::session::enforce_capabilities`]),
//! so an effect arriving here is always one the plugin was granted.

use hytte::services::notifications;
use hytte_plugin_proto::{Effect, HostMsg, Page};
use tokio::sync::mpsc;

/// Map one wire [`Effect`] onto a real host command. Handles [`Effect::OpenPage`]
/// (→ the modal drawer), [`Effect::RaiseOsd`] (→ the transient OSD nudge, #236),
/// and [`Effect::Notify`] (→ a local notification toast, #406); anything else is
/// logged and skipped. Capability enforcement happens **upstream** of here, per
/// connection: [`super::session::enforce_capabilities`] drops any effect whose
/// [`Capability`](hytte_plugin_proto::Capability) the plugin didn't declare before
/// it ever reaches this broker (#436), so an effect arriving here is always one the
/// plugin was granted. A persisted audit-log and the `RunCommand` round-trip remain
/// deferred.
///
/// `outbound` is the producing connection's host→plugin channel, used only by the
/// **two-way** [`Effect::RequestConsent`] (#487) to route the human's decision
/// back as a [`HostMsg::ConsentDecision`]; the one-way effects ignore it.
pub(super) fn broker_effect(plugin_id: &str, effect: &Effect, outbound: &mpsc::Sender<HostMsg>) {
    match effect {
        Effect::OpenPage(page) => {
            // #499 (deferred #440 hunk): open the drawer on niri's focused output,
            // not an arbitrary one. `preferred = None` let `open_on_focused` pick
            // any mounted drawer; passing the focused connector routes it to the
            // screen the user is on (the consent overlay wants the same routing).
            let focused = super::focused_output();
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
        other => {
            tracing::warn!(plugin = %plugin_id, ?other, "plugin effect unsupported in v1; skipped");
        }
    }
}

/// Map a wire [`Page`] onto the host's `modal::Page`. The two enums mirror each
/// other 1:1 (restored in #508: the host's `Stats` page was briefly split into
/// per-resource flyouts by #307, which made this a lossy approximation onto
/// the CPU flyout; the combined page restores the exact match); written
/// exhaustively so a page added to either side breaks the build here rather
/// than silently mis-routing.
pub(super) fn map_page(page: Page) -> crate::modal::Page {
    use crate::modal::Page as M;
    match page {
        Page::Media => M::Media,
        Page::Network => M::Network,
        Page::Vpn => M::Vpn,
        Page::Connections => M::Connections,
        Page::Bluetooth => M::Bluetooth,
        Page::Stats => M::Stats,
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
