//! Polkit (`org.freedesktop.PolicyKit1`) authentication agent.
//!
//! Without an agent registered for the user's session, GUI-triggered
//! privileged operations — `pkexec`, udisks mounts, `NetworkManager` VPN
//! credential prompts, `systemctl reboot/poweroff` from a non-privileged
//! shell — silently fail.  This module registers a session-scoped
//! `AuthenticationAgent` and surfaces password prompts to the UI via the
//! [`auth_prompts`] signal, mirroring the Bluetooth pairing-agent pattern.
//!
//! Authentication itself is performed by polkit's own setuid helper
//! `/usr/lib/polkit-1/polkit-agent-helper-1`; we feed it the cookie + user
//! response over stdin and report success back to the polkit Authority via
//! `AuthenticationAgentResponse2`.
//!
//! # Public API
//!
//! ```ignore
//! // Register once at startup:
//! .with(polkit::service())
//!
//! // Subscribe in widgets:
//! polkit::auth_prompts() -> impl Signal<Item = Option<AuthPrompt>>
//!
//! // Resolve from the UI (password is wrapped in `Zeroizing<String>`):
//! polkit::respond_to_auth(Some((Zeroizing::new(pw), uid))); // Confirm
//! polkit::respond_to_auth(None);                            // Cancel
//! ```

use anyhow::{Context, Result};
use futures_signals::signal::{Mutable, Signal};
use futures_util::StreamExt;
use hytte_bus::OwnNameSignal;
use hytte_reactive::{registry, runtime, Service};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio::sync::Mutex as AsyncMutex;
use zbus::zvariant::{OwnedValue, Value};
pub use zeroize::Zeroizing;

// ── Public data shapes ────────────────────────────────────────────────────────

/// One identity polkit is willing to authenticate as for the current
/// action.  Most commonly there's exactly one — the user's own uid — but
/// admin-rule actions may include `root` and members of the `wheel` group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthIdentity {
    /// Numeric uid.
    pub uid: u32,
    /// Best-effort human-readable label ("annika" or "root").  Falls back
    /// to `"uid <n>"` when name resolution fails.
    pub pretty_name: String,
}

/// A pending polkit authentication prompt the user must satisfy or cancel.
/// The agent's `BeginAuthentication` method blocks until the UI calls
/// [`respond_to_auth`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthPrompt {
    /// Action being authenticated (e.g. `org.freedesktop.systemd1.manage-units`).
    pub action_id: String,
    /// Polkit-supplied human-readable description of why auth is needed.
    pub message: String,
    /// Freedesktop icon name suggested by the action's policy file (may be empty).
    pub icon: String,
    /// All identities polkit will accept as authenticators.  The UI lets
    /// the user pick if there's more than one; defaulting to the entry
    /// matching the current uid is the right call.
    pub identities: Vec<AuthIdentity>,
    /// True when this prompt arrives mid-flight (e.g. confirm-new-
    /// password follow-up). The dialog updates an existing window
    /// in-place rather than rebuilding.
    pub follow_up: bool,
}

// ── Service handle ────────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct PolkitHandles {
    pub(crate) prompt: Mutable<Option<AuthPrompt>>,
    /// Sender half of the oneshot the agent method is awaiting.  Held under
    /// an async mutex so [`respond_to_auth`] can race-lessly take it.
    /// `None` when no prompt is in flight.
    pub(crate) pending_response: Arc<AsyncMutex<Option<oneshot::Sender<UserReply>>>>,
    /// Keeps the `own_name` watcher task alive for the process lifetime so
    /// the session bus holds `ANCHOR_NAME` and the `AuthAgent` interface
    /// remains reachable at `AGENT_PATH`.
    #[allow(dead_code)]
    ownership: OwnNameSignal,
}

/// User's resolution of a pending prompt.
#[derive(Debug)]
pub(crate) enum UserReply {
    /// Submitted password — consumed by `polkit-agent-helper-1` and dropped
    /// immediately afterwards.  Wrapped in [`Zeroizing`] so the heap buffer
    /// is overwritten on drop rather than just released.
    Submit {
        password: Zeroizing<String>,
        uid: u32,
    },
    /// Cancel / dismiss.
    Cancel,
}


// ── Service marker ────────────────────────────────────────────────────────────

pub struct PolkitService;

const ANCHOR_NAME: &str = "cc.hannig.trollshell.polkit-agent";

impl Service for PolkitService {
    type Handles = PolkitHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let prompt = Mutable::new(None);
        let pending_response = Arc::new(AsyncMutex::new(None));

        let agent = AuthAgent {};

        // Mount the AuthAgent interface on the session bus under a private
        // anchor name.  The well-known name is irrelevant to polkit — it
        // calls back via our unique name (:1.xx) — but owning it keeps the
        // bus layer's connection alive and re-mounts the interface on
        // reconnect.
        let ownership = hytte_bus::own_name(ANCHOR_NAME)
            .at_path(AGENT_PATH, agent)
            .start();

        rt.spawn(async move {
            if let Err(e) = run_agent_lifecycle().await {
                tracing::error!(error = %e, "polkit agent setup failed");
            }
        });

        PolkitHandles {
            prompt,
            pending_response,
            ownership,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Returns the polkit service to register with the hytte runtime.
#[must_use]
pub fn service() -> PolkitService {
    PolkitService
}

/// Signal emitting the active authentication prompt, or `None` when no
/// prompt is pending.  The UI shows a modal while this is `Some` and
/// resolves it via [`respond_to_auth`].
pub fn auth_prompts() -> impl Signal<Item = Option<AuthPrompt>> {
    registry::with(|r| {
        r.get::<PolkitHandles>()
            .expect("polkit::service() not registered")
            .prompt
            .signal_cloned()
    })
}

/// Resolve the pending prompt.
///
/// * `Some((password, uid))` — submit `password` for `uid` to
///   `polkit-agent-helper-1`.  The password is taken by [`Zeroizing`] so
///   the underlying heap buffer is wiped when the wrapper is dropped.
/// * `None` — cancel; the agent's `BeginAuthentication` returns an error
///   and polkit reports the action as not-authorized.
pub fn respond_to_auth(response: Option<(Zeroizing<String>, u32)>) {
    let reply = match response {
        Some((password, uid)) => UserReply::Submit { password, uid },
        None => UserReply::Cancel,
    };
    runtime::handle().spawn(async move {
        let pending = registry::with(|r| {
            r.get::<PolkitHandles>().map(|h| h.pending_response.clone())
        });
        let Some(pending) = pending else { return };
        let mut guard = pending.lock().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(reply);
        }
    });
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn pending_response_arc() -> Option<Arc<AsyncMutex<Option<oneshot::Sender<UserReply>>>>> {
    registry::with(|r| {
        r.get::<PolkitHandles>()
            .map(|h| h.pending_response.clone())
    })
}

fn set_prompt(p: Option<AuthPrompt>) {
    registry::with(|r| {
        if let Some(h) = r.get::<PolkitHandles>() {
            h.prompt.set(p);
        }
    });
}

fn clear_prompt() {
    set_prompt(None);
}

/// Suspend the agent method until the UI replies.  Returns `Cancel` if no
/// prompt slot is available, the channel is dropped, or another auth is
/// already in flight.
async fn await_reply(prompt: AuthPrompt) -> UserReply {
    let Some(pending) = pending_response_arc() else {
        return UserReply::Cancel;
    };

    let (tx, rx) = oneshot::channel::<UserReply>();
    {
        let mut guard = pending.lock().await;
        if guard.is_some() {
            // Polkit normally serialises calls to a single agent, but be
            // defensive — refuse cleanly rather than queueing.
            return UserReply::Cancel;
        }
        *guard = Some(tx);
    }

    set_prompt(Some(prompt));
    let reply = rx.await.unwrap_or(UserReply::Cancel);
    clear_prompt();
    reply
}

/// Service-internal: emit a follow-up `AuthPrompt` and wait for the
/// dialog's reply. `Some(pw)` on submit, `None` on user cancel.
///
/// `await_reply` already handles the `set_prompt(Some(..))` /
/// `clear_prompt()` lifecycle, so this is a thin wrapper that just
/// projects the password out of the `UserReply`.
async fn await_followup_password(prompt: AuthPrompt) -> Option<Zeroizing<String>> {
    match await_reply(prompt).await {
        UserReply::Submit { password, .. } => Some(password),
        UserReply::Cancel => None,
    }
}

/// Resolve a uid to a printable username via NSS.  Falls back to
/// `"uid <n>"` so we never feed the UI an empty string.  This fallback is
/// **only** safe for display: see [`username_for_uid`] for the helper path.
fn pretty_name_for_uid(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map_or_else(|| format!("uid {uid}"), |u| u.name)
}

/// Strict variant: returns the actual passwd entry's `pw_name`, or `Err`
/// when NSS can't resolve one.  Used for `polkit-agent-helper-1`'s
/// argv[1], which calls `getpwnam()` and silently aborts if it returns
/// NULL — feeding `"uid 1000"` there would deadlock the auth round-trip
/// before PAM ever sees the cookie.
fn username_for_uid(uid: u32) -> Result<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .context("nss lookup")?
        .map(|u| u.name)
        .with_context(|| format!("no passwd entry for uid {uid}"))
}

/// Extract the unix-user uid from a polkit identity tuple.  Polkit
/// identities arrive as `(type, details)` where `type` is `"unix-user"`
/// or `"unix-group"` and `details` is `{"uid": u32}` / `{"gid": u32}`.
/// Returns `None` for groups (we can't authenticate as a group directly
/// from a single uid prompt) and for malformed entries.
fn uid_from_identity(kind: &str, details: &HashMap<String, OwnedValue>) -> Option<u32> {
    if kind != "unix-user" {
        return None;
    }
    let val = details.get("uid")?.try_clone().ok()?;
    u32::try_from(val).ok()
}

// ── polkit-agent-helper-1 invocation ──────────────────────────────────────────

/// Canonical install locations for `polkit-agent-helper-1` across distros,
/// probed in priority order.  The first existing path wins; if none of them
/// exist we fall back to entry 0 so the spawn fails predictably with
/// `ENOENT` and the Authority sees a `Failed` reply.
const HELPER_CANDIDATES: &[&str] = &[
    // Arch, Alpine, most musl distros.
    "/usr/lib/polkit-1/polkit-agent-helper-1",
    // Fedora / RHEL / openSUSE.
    "/usr/libexec/polkit-1/polkit-agent-helper-1",
    // Debian / Ubuntu (legacy `policykit-1` directory name).
    "/usr/lib/policykit-1/polkit-agent-helper-1",
    // Source / custom builds.
    "/usr/local/lib/polkit-1/polkit-agent-helper-1",
];

/// Resolve `polkit-agent-helper-1` once per process.
///
/// Returns the first [`HELPER_CANDIDATES`] entry that exists on disk.  If
/// none do, logs a single error and returns the Arch path so the spawn
/// surfaces an actionable `ENOENT` instead of crashing the agent.
fn find_helper() -> &'static Path {
    static CACHED: OnceLock<&'static Path> = OnceLock::new();
    CACHED.get_or_init(|| {
        for candidate in HELPER_CANDIDATES {
            if Path::new(candidate).exists() {
                return Path::new(candidate);
            }
        }
        tracing::error!(
            candidates = ?HELPER_CANDIDATES,
            "polkit-agent-helper-1 not found in any canonical location; \
             falling back to {} — auth will fail until the helper is installed",
            HELPER_CANDIDATES[0]
        );
        Path::new(HELPER_CANDIDATES[0])
    })
}

/// Run the polkit setuid helper to verify the password.
///
/// Stdin protocol (per upstream `polkitagenthelper-pam.c`):
///   * argv[1] = identity username
///   * first stdin line = the cookie
///   * helper writes `PAM_PROMPT_ECHO_OFF <prompt>` / `PAM_PROMPT_ECHO_ON <prompt>` —
///     we reply with the password (or empty for `ECHO_ON`) followed by `\n`.
///   * helper writes `PAM_TEXT_INFO <msg>` / `PAM_ERROR_MSG <msg>` — informational, ignore.
///   * helper writes `SUCCESS` and exits 0 on success, `FAILURE` on failure.
///
/// The password is moved into this fn (wrapped in [`Zeroizing`] so its
/// heap buffer is wiped on drop) and dropped as soon as it has been
/// written into the helper's stdin pipe.
async fn run_helper(
    username: &str,
    cookie: &str,
    password: Zeroizing<String>,
    action_id: &str,
    icon: &str,
    identities: &[AuthIdentity],
) -> Result<bool> {
    let helper = find_helper();
    let mut child = tokio::process::Command::new(helper)
        .arg(username)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {}", helper.display()))?;

    let mut stdin = child.stdin.take().context("helper stdin")?;
    let stdout = child.stdout.take().context("helper stdout")?;
    let mut reader = BufReader::new(stdout).lines();

    // Cookie first.
    stdin
        .write_all(format!("{cookie}\n").as_bytes())
        .await
        .context("write cookie to helper")?;

    // PAM conversation. Wrap the password in an Option so we can take it
    // out and drop it the moment it's written; dropping the Zeroizing
    // wrapper overwrites the heap buffer.
    let mut password_slot: Option<Zeroizing<String>> = Some(password);
    let mut authenticated = false;

    while let Some(line) = reader.next_line().await.context("read helper stdout")? {
        let line = line.trim_end_matches('\r');
        let (tag, rest) = line.split_once(' ').unwrap_or((line, ""));
        match tag {
            "PAM_PROMPT_ECHO_OFF" => {
                // First prompt: serve from password_slot (the password the
                // user already typed in the initial dialog). Subsequent
                // prompts come from a follow-up dialog round-trip — common
                // in password-change flows like passwd's "Retype new
                // password".
                let pw = if let Some(pw) = password_slot.take() {
                    pw
                } else {
                    let prompt = AuthPrompt {
                        action_id: action_id.to_string(),
                        message: rest.to_string(),
                        icon: icon.to_string(),
                        identities: identities.to_vec(),
                        follow_up: true,
                    };
                    if let Some(pw) = await_followup_password(prompt).await {
                        pw
                    } else {
                        // User cancelled the follow-up; tear the helper
                        // down.
                        authenticated = false;
                        break;
                    }
                };
                stdin
                    .write_all(pw.as_bytes())
                    .await
                    .context("write password")?;
                stdin
                    .write_all(b"\n")
                    .await
                    .context("write password newline")?;
                // Drop the secret immediately; Zeroizing's Drop wipes the
                // backing allocation before releasing it.
                drop(pw);
            }
            "PAM_PROMPT_ECHO_ON" => {
                // Helper wants something visible (e.g. account name).  We
                // don't have a way to ask the user mid-flight, so just
                // submit empty + carry on; PAM will fail authentication.
                stdin.write_all(b"\n").await.context("write empty echo")?;
                let _ = rest;
            }
            "PAM_TEXT_INFO" | "PAM_ERROR_MSG" => {
                tracing::debug!(target: "polkit::helper", %tag, msg = rest);
            }
            "SUCCESS" => {
                authenticated = true;
                break;
            }
            "FAILURE" => {
                authenticated = false;
                break;
            }
            other if !other.is_empty() => {
                tracing::debug!(target: "polkit::helper", "unknown line: {line}");
            }
            _ => {}
        }
    }

    drop(stdin);
    drop(password_slot); // belt-and-braces: ensure no Some is left over

    let status = child.wait().await.context("await helper exit")?;
    if !status.success() {
        authenticated = false;
    }
    Ok(authenticated)
}

// ── DBus types ────────────────────────────────────────────────────────────────

/// Errors we throw from the agent interface.  Polkit handles the
/// `org.freedesktop.PolicyKit1.Error.*` namespace; using these names lets
/// polkitd map them onto the correct internal failure modes.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
#[allow(dead_code)]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Failed(String),
    Cancelled(String),
    NotAuthorized(String),
}

/// Polkit identity wire shape: `(s, a{sv})`.
type IdentityTuple = (String, HashMap<String, OwnedValue>);

const AGENT_PATH: &str = "/com/trollshell/PolkitAgent";

#[derive(Clone)]
struct AuthAgent {}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl AuthAgent {
    #[allow(clippy::too_many_arguments)]
    async fn begin_authentication(
        &self,
        action_id: String,
        message: String,
        icon_name: String,
        details: HashMap<String, String>,
        cookie: String,
        identities: Vec<IdentityTuple>,
    ) -> Result<(), AgentError> {
        let _ = details; // policy-specific hints; not used in MVP
        // Translate the wire identities to AuthIdentity, keeping only
        // unix-user entries.  Group identities are valid auth targets but
        // require resolving group membership to a specific uid — out of
        // scope for the MVP; we surface only direct user identities.
        let mut auth_identities: Vec<AuthIdentity> = identities
            .iter()
            .filter_map(|(kind, details)| {
                let uid = uid_from_identity(kind, details)?;
                Some(AuthIdentity {
                    uid,
                    pretty_name: pretty_name_for_uid(uid),
                })
            })
            .collect();

        if auth_identities.is_empty() {
            return Err(AgentError::Failed(
                "no usable identities offered for this action".into(),
            ));
        }

        // Default sort: own uid first if present, then root, then everything else.
        let own_uid = nix::unistd::getuid().as_raw();
        auth_identities.sort_by_key(|id| match id.uid {
            u if u == own_uid => 0,
            0 => 1,
            _ => 2,
        });

        let prompt = AuthPrompt {
            action_id: action_id.clone(),
            message,
            icon: icon_name.clone(),
            identities: auth_identities.clone(),
            follow_up: false,
        };

        // ── Wait for UI ────────────────────────────────────────────────────────

        let reply = await_reply(prompt).await;
        let UserReply::Submit { password, uid } = reply else {
            return Err(AgentError::Cancelled("user cancelled".into()));
        };

        // Helper's argv[1] must be a real passwd-resolvable username, not
        // the UI's "uid N" fallback — see [`username_for_uid`] docs.
        let username = username_for_uid(uid)
            .map_err(|_| AgentError::Failed("user not found in passwd".into()))?;

        // ── Verify with helper ─────────────────────────────────────────────────

        let ok = run_helper(
            &username,
            &cookie,
            password,
            &action_id,
            &icon_name,
            &auth_identities,
        )
        .await
        .map_err(|e| AgentError::Failed(format!("helper: {e}")))?;

        if !ok {
            // Failed (not NotAuthorized) so polkitd lets the user retry —
            // matches gnome-shell / polkit-kde-agent behaviour.
            return Err(AgentError::Failed("authentication failed".into()));
        }

        // ── Report success back to the Authority ───────────────────────────────
        //
        // AuthenticationAgentResponse2(uid: u, cookie: s, identity: (sa{sv}))
        // The identity argument is the one that just authenticated, in the
        // same wire shape polkit gave us.
        let mut details: HashMap<String, OwnedValue> = HashMap::new();
        let uid_v = OwnedValue::try_from(Value::U32(uid))
            .map_err(|e| AgentError::Failed(format!("encode uid: {e}")))?;
        details.insert("uid".into(), uid_v);
        let identity: IdentityTuple = ("unix-user".into(), details);

        hytte_bus::call("org.freedesktop.PolicyKit1")
            .bus(hytte_bus::BusKind::System)
            .at_path("/org/freedesktop/PolicyKit1/Authority")
            .iface("org.freedesktop.PolicyKit1.Authority")
            .method("AuthenticationAgentResponse2")
            .args((uid, cookie.clone(), identity))
            .send::<()>()
            .await
            .map_err(|e| AgentError::Failed(format!("AuthenticationAgentResponse2: {e}")))?;

        Ok(())
    }

    async fn cancel_authentication(&self, cookie: String) -> Result<(), AgentError> {
        tracing::debug!(%cookie, "polkit CancelAuthentication");
        let pending = pending_response_arc();
        if let Some(p) = pending
            && let Some(tx) = p.lock().await.take()
        {
            let _ = tx.send(UserReply::Cancel);
        }
        clear_prompt();
        Ok(())
    }
}

// ── Registration loop ────────────────────────────────────────────────────────

/// `XDG_SESSION_ID` exported by logind for the user's graphical session.
/// Polkit needs this to scope our agent to the right session.
fn current_session_id() -> Result<String> {
    std::env::var("XDG_SESSION_ID").context("XDG_SESSION_ID not set")
}

/// Build the `(s, a{sv})` subject for `RegisterAuthenticationAgent`:
/// `("unix-session", {"session-id": <id>})`.
fn build_subject(session_id: &str) -> Result<(String, HashMap<String, OwnedValue>)> {
    let mut details: HashMap<String, OwnedValue> = HashMap::new();
    let v = OwnedValue::try_from(Value::Str(session_id.into()))
        .context("encode session-id")?;
    details.insert("session-id".into(), v);
    Ok(("unix-session".into(), details))
}

async fn run_agent_lifecycle() -> Result<()> {
    let session_id = current_session_id().context("XDG_SESSION_ID unset")?;
    let subject = build_subject(&session_id).context("build polkit subject")?;

    // Register with polkit Authority on system bus.
    register_with_authority(subject.clone())
        .await
        .context("Authority.RegisterAuthenticationAgent")?;
    tracing::info!(session = %session_id, "polkit authentication agent registered");

    // Watch polkitd's well-known name on the system bus; on re-appearance,
    // re-register our agent (our registration is lost whenever polkitd
    // restarts).
    let polkitd_changes = hytte_bus::signals("org.freedesktop.DBus")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    let mut events = polkitd_changes.events();
    while let Some(event) = events.next().await {
        let Ok(args) = event.body.body().deserialize::<(String, String, String)>() else {
            continue;
        };
        let (name, _old, new_owner) = args;
        if name != "org.freedesktop.PolicyKit1" {
            continue;
        }
        if new_owner.is_empty() {
            tracing::warn!("polkitd disappeared — will re-register on restart");
            continue;
        }
        // polkitd came back: re-register our agent.
        if let Err(e) = register_with_authority(subject.clone()).await {
            tracing::warn!(error = %e, "re-RegisterAuthenticationAgent failed");
        } else {
            tracing::info!("polkit agent re-registered after polkitd restart");
        }
    }
    Ok(())
}

async fn register_with_authority(
    subject: (String, HashMap<String, OwnedValue>),
) -> Result<(), hytte_bus::BusError> {
    hytte_bus::call("org.freedesktop.PolicyKit1")
        .bus(hytte_bus::BusKind::System)
        .at_path("/org/freedesktop/PolicyKit1/Authority")
        .iface("org.freedesktop.PolicyKit1.Authority")
        .method("RegisterAuthenticationAgent")
        .args((subject, "en_US.UTF-8".to_string(), AGENT_PATH.to_string()))
        .send::<()>()
        .await
}
