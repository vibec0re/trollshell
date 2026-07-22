//! `BlueZ` `Agent1` implementation for the Bluetooth pairing dialog.
//!
//! Implements `org.bluez.Agent1` so `BlueZ` can ask us to confirm pairings.
//! Without a registered agent `BlueZ` rejects most pair attempts that need any
//! user interaction (e.g. SSP numeric comparison). MVP scope:
//!   * `RequestConfirmation`: user confirms the displayed code matches.
//!   * `RequestAuthorization`: bare yes/no.
//!   * `AuthorizeService`: auto-accept (typical for trusted devices reconnecting).
//!   * PIN / Passkey entry methods: return Rejected (no text-input UI yet).
//!   * Cancel: aborts the pending prompt.
//!
//! The agent is registered under `bus::own_name` on the SYSTEM bus. `BlueZ`
//! records the system-bus unique name when we call `RegisterAgent`, then issues
//! `Agent1` callbacks on that same connection. This mirrors the polkit pattern:
//! agent + anchor name both on the system bus.

use super::SHARED;
use super::types::{AgentReply, PairPrompt, PromptKind};
use futures_util::StreamExt;
use hytte_bus::BusKind;
use tokio::sync::oneshot;
use zbus::zvariant::OwnedObjectPath;

pub(super) const AGENT_PATH: &str = "/com/trollshell/BluetoothAgent";
pub(super) const AGENT_ANCHOR_NAME: &str = "mov.vibec0re.trollshell.bluez-agent";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

#[derive(Clone)]
pub(super) struct PairAgent;

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await; display/authorize-service handlers also
// receive owned values they only acknowledge. Allowing at the impl-block
// keeps the noise out of each method.
#[allow(clippy::unused_async, clippy::needless_pass_by_value)]
#[zbus::interface(name = "org.bluez.Agent1")]
impl PairAgent {
    async fn release(&self) {
        tracing::debug!("agent released");
    }

    async fn request_pin_code(&self, device: OwnedObjectPath) -> Result<String, AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::EnterPinCode,
        })
        .await;
        match reply {
            AgentReply::Pin(s) => Ok(s),
            _ => Err(AgentError::Rejected("user did not provide PIN".into())),
        }
    }

    async fn display_pin_code(&self, device: OwnedObjectPath, pincode: String) {
        // Display-only acknowledgement — there is no return value to gate.
        // The user enters the PIN on the remote device. Nothing to do.
        let _ = (device, pincode);
    }

    async fn request_passkey(&self, device: OwnedObjectPath) -> Result<u32, AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::EnterPasskey,
        })
        .await;
        match reply {
            AgentReply::Passkey(p) => Ok(p),
            _ => Err(AgentError::Rejected("user did not provide passkey".into())),
        }
    }

    async fn display_passkey(&self, device: OwnedObjectPath, passkey: u32, entered: u16) {
        // Same as DisplayPinCode — no input from us.
        let _ = (device, passkey, entered);
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: Some(passkey),
            kind: PromptKind::ConfirmPasskey,
        })
        .await;
        match reply {
            AgentReply::Confirm => Ok(()),
            _ => Err(AgentError::Rejected("user rejected pairing".into())),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        let path = device.as_str().to_string();
        let alias = lookup_alias(&path);
        let reply = await_reply(PairPrompt {
            device_path: path,
            alias,
            passkey: None,
            kind: PromptKind::Authorize,
        })
        .await;
        match reply {
            AgentReply::Confirm => Ok(()),
            _ => Err(AgentError::Rejected("user rejected pairing".into())),
        }
    }

    async fn authorize_service(&self, device: OwnedObjectPath, uuid: String) {
        // Auto-accept service authorization. BlueZ asks per-service for
        // unknown protocols; for trusted/already-paired devices this is
        // generally fine and matches blueman-applet's default policy.
        let _ = (device, uuid);
    }

    async fn cancel(&self) {
        tracing::debug!("agent cancel — aborting pending prompt");
        if let Some(tx) = take_pending().await {
            let _ = tx.send(AgentReply::Reject);
        }
        clear_prompt();
    }
}

/// Resolve a device path to a user-facing label. Prefers the `BlueZ` Alias,
/// falls through to the MAC address, and ultimately "Unknown device" so a
/// raw D-Bus object path never bleeds into UI copy.
fn lookup_alias(path: &str) -> String {
    SHARED
        .get()
        .and_then(|s| {
            let devs = s.devices.lock_ref();
            devs.iter().find(|d| d.path == path).map(|d| {
                if !d.alias.is_empty() {
                    d.alias.clone()
                } else if !d.address.is_empty() {
                    d.address.clone()
                } else {
                    "Unknown device".to_string()
                }
            })
        })
        .unwrap_or_else(|| "Unknown device".to_string())
}

fn pending_response_arc()
-> Option<std::sync::Arc<tokio::sync::Mutex<Option<oneshot::Sender<AgentReply>>>>> {
    SHARED.get().map(|s| s.pending_response.clone())
}

async fn take_pending() -> Option<oneshot::Sender<AgentReply>> {
    let arc = pending_response_arc()?;
    arc.lock().await.take()
}

fn set_prompt(p: Option<PairPrompt>) {
    if let Some(s) = SHARED.get() {
        s.pair_prompt.set(p);
    }
}

fn clear_prompt() {
    set_prompt(None);
}

/// Suspend the calling Agent1 method until the user responds via the UI.
/// Returns `AgentReply::Reject` if no prompt slot is available, the
/// channel is dropped, or another pair is already in flight — callers
/// pattern-match on the returned variant to shape their D-Bus return.
async fn await_reply(prompt: PairPrompt) -> AgentReply {
    let Some(pending) = pending_response_arc() else {
        return AgentReply::Reject;
    };

    let (tx, rx) = oneshot::channel::<AgentReply>();
    {
        let mut guard = pending.lock().await;
        if guard.is_some() {
            // Another pairing already pending — refuse cleanly so BlueZ
            // doesn't pile up coincident prompts.
            return AgentReply::Reject;
        }
        *guard = Some(tx);
    }

    set_prompt(Some(prompt));
    let reply = rx.await.unwrap_or(AgentReply::Reject);
    clear_prompt();
    reply
}

/// Start the pairing-agent registration loop. Uses `bus::own_name` on the
/// SYSTEM bus (`BlueZ` is on system bus; it records the unique name of the
/// connection that called `RegisterAgent`). Mounting `PairAgent` at
/// `AGENT_PATH` via `.at_path()` ensures the object is visible before
/// `RequestName` succeeds, so `BlueZ` never races a missing object.
///
/// After the name is owned, we call `RegisterAgent` and
/// `RequestDefaultAgent` once via `bus::call`. On bluetoothd restart the
/// `NameOwnerChanged` stream for `org.bluez` wakes us to re-register.
///
/// The `ownership` handle is passed in (created in `Service::start` and also
/// stored in `BluetoothHandles`) to keep the interface alive for the duration
/// of this task.
pub(super) async fn run_agent(_ownership: hytte_bus::OwnNameSignal) {
    // Watch for org.bluez owner changes. When bluetoothd restarts (loses
    // its name), our registration is gone and we must re-register.
    // We re-register once the owner comes back.
    let bluez_gone_sub = hytte_bus::signals("org.freedesktop.DBus")
        .bus(BusKind::System)
        .at_path("/org/freedesktop/DBus")
        .iface("org.freedesktop.DBus")
        .signal("NameOwnerChanged")
        .start();

    // Initial registration attempt.
    try_register_agent().await;

    // Re-register whenever BlueZ (org.bluez) gains a new owner, which
    // indicates bluetoothd has restarted.
    let mut noc_events = bluez_gone_sub.events();
    while let Some(evt) = noc_events.next().await {
        let Ok((name, _old_owner, new_owner)) =
            evt.body.body().deserialize::<(String, String, String)>()
        else {
            continue;
        };
        if name != "org.bluez" {
            continue;
        }
        if new_owner.is_empty() {
            // bluetoothd died — our registration is gone. Wait for it to
            // come back (the next NameOwnerChanged with a non-empty
            // new_owner will trigger re-registration).
            tracing::warn!("org.bluez lost — will re-register agent on restart");
            continue;
        }
        // bluetoothd restarted: re-register.
        tracing::info!("org.bluez new owner — re-registering pairing agent");
        try_register_agent().await;
    }
}

/// Attempt to register the pairing agent with `BlueZ` once. Logs any failure
/// and returns (caller decides whether to retry or wait for `NameOwnerChanged`).
async fn try_register_agent() {
    let agent_op = match zbus::zvariant::ObjectPath::try_from(AGENT_PATH) {
        Ok(p) => p.to_owned(),
        Err(e) => {
            tracing::error!(error = %e, "bluetooth agent: bad agent path");
            return;
        }
    };

    // RegisterAgent — capability "DisplayYesNo": we can show a code and
    // accept yes/no, which is what RequestConfirmation needs.
    if let Err(e) = hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RegisterAgent")
        .args((agent_op.clone(), "DisplayYesNo".to_string()))
        .send::<()>()
        .await
    {
        tracing::warn!(error = %e, "bluetooth agent: RegisterAgent failed");
        return;
    }

    // RequestDefaultAgent — make us the system-wide default. Without this
    // BlueZ may use whichever Agent it sees first, including stale ones
    // from a previous trollshell run if any.
    if let Err(e) = hytte_bus::call("org.bluez")
        .bus(BusKind::System)
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RequestDefaultAgent")
        .args((agent_op,))
        .send::<()>()
        .await
    {
        tracing::warn!(error = %e, "bluetooth agent: RequestDefaultAgent failed");
        return;
    }

    tracing::info!("bluetooth pairing agent registered");
}
