//! `BlueZ` `Agent1` implementation for the Bluetooth pairing dialog.
//!
//! Implements `org.bluez.Agent1` so `BlueZ` can ask us to confirm pairings.
//! Without a registered agent `BlueZ` rejects most pair attempts that need any
//! user interaction (e.g. SSP numeric comparison). MVP scope:
//!   * `RequestConfirmation`: user confirms the displayed code matches.
//!   * `RequestAuthorization`: bare yes/no.
//!   * `AuthorizeService`: "Allow X to use Y?", yes/no. bluetoothd asks only
//!     for a device that is not `Trusted` ([`PairAgent::authorize_service_for`]).
//!   * PIN / Passkey entry methods: the banner takes the PIN or passkey.
//!   * Cancel: aborts the pending prompt.
//!
//! Every method answers only bluetoothd: the current owner of `org.bluez`
//! ([`PairAgent::admit`]). Anyone else gets `org.bluez.Error.Rejected`.
//!
//! The agent is registered under `bus::own_name` on the SYSTEM bus. `BlueZ`
//! records the system-bus unique name when we call `RegisterAgent`, then issues
//! `Agent1` callbacks on that same connection. This mirrors the polkit pattern:
//! agent + anchor name both on the system bus.

use super::shared_state;
use super::trust::{self, DeviceBus, Pairings};
use super::types::{AgentReply, PairPrompt, PromptKind};
use crate::retry;
use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
use hytte_bus::{BusKind, SignalItem};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::oneshot;
use zbus::zvariant::OwnedObjectPath;

pub(super) const AGENT_PATH: &str = "/com/trollshell/BluetoothAgent";
pub(super) const AGENT_ANCHOR_NAME: &str = "mov.vibec0re.trollshell.bluez-agent";

/// bluetoothd's well-known name. The agent answers only its current owner.
pub(super) const BLUEZ_BUS_NAME: &str = "org.bluez";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.bluez.Error")]
enum AgentError {
    #[zbus(error)]
    ZBus(zbus::Error),
    Rejected(String),
    Canceled(String),
}

/// The `org.bluez.Agent1` object bluetoothd calls back on.
///
/// It raises each request's prompt on `banner` and waits for the user's
/// answer in `pending`: the service's own handles, which the UI and the
/// watcher's cleanup reach through [`shared_state`]. A test hands in its own.
#[derive(Clone)]
pub(super) struct PairAgent {
    /// Admits a call only from bluetoothd: the current owner of `org.bluez`,
    /// looked up on the call's own connection when it arrives.
    caller: hytte_bus::CallerCheck,
    pending: Arc<ReplySlot>,
    banner: Mutable<Option<PairPrompt>>,
    /// Where an accepted pairing's device is marked `Trusted` once it has
    /// paired ([`trust::accepted`]): the system bus in production, a
    /// recorder in the tests.
    devices: Arc<dyn DeviceBus>,
    /// The devices being paired, shared with `pair_device`. Their
    /// `AuthorizeService` requests are let through without a prompt.
    pairings: Pairings,
}

impl PairAgent {
    /// An agent answering only bluetoothd, raising its prompts on `banner`,
    /// waiting for answers in `pending`, tracking pairings in `pairings`, and
    /// trusting a paired device through `devices`.
    pub(super) fn new(
        pending: Arc<ReplySlot>,
        banner: Mutable<Option<PairPrompt>>,
        devices: Arc<dyn DeviceBus>,
        pairings: Pairings,
    ) -> Self {
        Self {
            caller: hytte_bus::caller_check(BLUEZ_BUS_NAME),
            pending,
            banner,
            devices,
            pairings,
        }
    }

    /// Answer a `method` call only if it came from bluetoothd.
    ///
    /// The agent is exported on our system connection, so any peer the bus
    /// policy lets through can call it, not only bluetoothd. Unchecked, such
    /// a peer could raise a pairing prompt for a device the user never chose,
    /// or `Cancel` a real one. Every method checks first and touches nothing
    /// (no banner, no reply slot) when the check refuses. `BlueZ`'s own policy
    /// file (`src/bluetooth.conf`) lets only root send to `org.bluez.Agent1`,
    /// so this stops a root process other than bluetoothd.
    ///
    /// The owner is looked up per call ([`hytte_bus::CallerCheck`]), on
    /// `conn`, the connection the call arrived on: a restarted bluetoothd
    /// comes back under a new unique name, and a cached owner would refuse
    /// it.
    async fn admit(
        &self,
        conn: &zbus::Connection,
        header: &zbus::message::Header<'_>,
        method: &str,
    ) -> Result<(), AgentError> {
        self.caller.check(conn, header).await.map_err(|refused| {
            tracing::warn!(
                method,
                %refused,
                "bluetooth agent: refused a call that did not come from bluetoothd",
            );
            AgentError::Rejected(format!(
                "only {} may call this agent: {refused}",
                self.caller.name()
            ))
        })
    }

    /// Raise `prompt` and wait for the user's answer ([`await_reply_in`]).
    async fn ask(&self, prompt: PairPrompt) -> AgentReply {
        await_reply_in(&self.pending, &self.banner, prompt).await
    }

    /// Pass a pairing prompt's `result` through, first putting `device` in
    /// progress if the user accepted ([`trust::accepted`]).
    ///
    /// It goes in before bluetoothd gets the reply: bluetoothd sends the
    /// service requests it queued during the pairing as soon as the pairing's
    /// services resolve, and those must find the device in progress. Nothing
    /// is set in bluetoothd until the device has paired. A rejected prompt
    /// never enters the set. An accepted one while the shell's own `Pair` of
    /// the device runs leaves that `Pair`'s entry as it is.
    fn in_progress_if_accepted<T>(
        &self,
        device: &OwnedObjectPath,
        result: Result<T, AgentError>,
    ) -> Result<T, AgentError> {
        if result.is_ok() {
            trust::accepted(&self.devices, &self.pairings, device.as_str());
        }
        result
    }

    /// `RequestPinCode` once the caller is admitted.
    async fn pin_code_for(&self, device: &OwnedObjectPath) -> Result<String, AgentError> {
        let reply = self
            .ask(new_prompt(device, None, PromptKind::EnterPinCode))
            .await;
        let result = match reply {
            AgentReply::Pin(s) => Ok(s),
            _ => Err(AgentError::Rejected("user did not provide PIN".into())),
        };
        self.in_progress_if_accepted(device, result)
    }

    /// `RequestPasskey` once the caller is admitted.
    async fn passkey_for(&self, device: &OwnedObjectPath) -> Result<u32, AgentError> {
        let reply = self
            .ask(new_prompt(device, None, PromptKind::EnterPasskey))
            .await;
        let result = match reply {
            AgentReply::Passkey(p) => Ok(p),
            _ => Err(AgentError::Rejected("user did not provide passkey".into())),
        };
        self.in_progress_if_accepted(device, result)
    }

    /// `RequestConfirmation` once the caller is admitted.
    async fn confirmation_for(
        &self,
        device: &OwnedObjectPath,
        passkey: u32,
    ) -> Result<(), AgentError> {
        let reply = self
            .ask(new_prompt(
                device,
                Some(passkey),
                PromptKind::ConfirmPasskey,
            ))
            .await;
        let result = allow_or_reject(&reply, "user rejected pairing");
        self.in_progress_if_accepted(device, result)
    }

    /// `RequestAuthorization` once the caller is admitted.
    async fn authorization_for(&self, device: &OwnedObjectPath) -> Result<(), AgentError> {
        let reply = self
            .ask(new_prompt(device, None, PromptKind::Authorize))
            .await;
        let result = allow_or_reject(&reply, "user rejected pairing");
        self.in_progress_if_accepted(device, result)
    }

    /// `AuthorizeService` once the caller is admitted: ask the user whether
    /// `device` may use the service `uuid` names, and answer bluetoothd.
    ///
    /// bluetoothd calls this only for a device that is **not** `Trusted`. Its
    /// authorization queue (`process_auth_queue` in `src/adapter.c`, `BlueZ`
    /// 5.87) accepts a trusted device's request itself and never reaches the
    /// agent. A device the user pairs, from either side, is made trusted once
    /// it has paired, and while it is being paired its requests are let
    /// through here without a prompt ([`trust`]). So what asks is a device
    /// someone deliberately left untrusted (or one never paired) wanting
    /// audio, input or networking. It used to be accepted without asking, on
    /// the reasoning that the requests come from trusted devices, which is
    /// exactly the case bluetoothd never sends.
    ///
    /// Allow answers `Ok`. Deny, a `Cancel` (including bluetoothd's own after
    /// its 60 s `REQUEST_TIMEOUT`, `src/agent.c`) and a second request while
    /// one is waiting are all `org.bluez.Error.Rejected`. Nothing is saved:
    /// the device is not made `Trusted`, so it asks again next time.
    ///
    /// Before it asks, it reads the device's `Trusted` from bluetoothd, and a
    /// device trusted by then is allowed without a prompt (#1611, LOW-2).
    /// bluetoothd decided to ask when it queued the request, and a pairing
    /// that has just succeeded can have trusted the device and left the set
    /// since: this check runs only after the caller check's round trip to
    /// the bus. A read that fails asks, as before.
    async fn authorize_service_for(
        &self,
        device: &OwnedObjectPath,
        uuid: &str,
    ) -> Result<(), AgentError> {
        if self.pairings.contains(device.as_str()) {
            tracing::info!(
                device = %device.as_str(),
                uuid,
                "bluetooth agent: a device being paired asks to use a service; allowed, \
                 since the user started or accepted its pairing"
            );
            return Ok(());
        }
        // This read does not start a bluetoothd that was stopped. The caller
        // check (`admit`) has just asked the bus and found that this request
        // comes from the current owner of `org.bluez`, and that bluetoothd is
        // waiting on our answer, so it is running: the bus delivers the read
        // to it and activates nothing. Only a bluetoothd that exits in the
        // moment between that check and this read would be started again,
        // the same window every call this module makes after a bluetoothd
        // reply has.
        match self.devices.trusted(device.as_str()).await {
            Ok(true) => {
                tracing::info!(
                    device = %device.as_str(),
                    uuid,
                    "bluetooth agent: a device trusted since bluetoothd asked wants to use a \
                     service; allowed"
                );
                return Ok(());
            }
            Ok(false) => {}
            Err(e) => {
                tracing::debug!(
                    device = %device.as_str(),
                    error = %e,
                    "bluetooth agent: could not read the device's Trusted; asking the user"
                );
            }
        }
        let prompt = PairPrompt {
            service: Some(service_label(uuid)),
            ..new_prompt(device, None, PromptKind::AuthorizeService)
        };
        tracing::info!(
            device = %device.as_str(),
            uuid,
            "bluetooth agent: an untrusted device asks to use a service; asking the user"
        );
        let reply = self.ask(prompt).await;
        allow_or_reject(&reply, "user did not allow the service")
    }
}

/// The Bluetooth Base UUID's fixed tail. Every 16-bit assigned number `xxxx`
/// stands for `0000xxxx-0000-1000-8000-00805f9b34fb` (Bluetooth Core
/// Specification, Vol 3, Part B, §2.5.1).
const BASE_UUID_TAIL: &str = "-0000-1000-8000-00805f9b34fb";

/// The 16-bit assigned number inside a 128-bit UUID string, if it is one
/// built on the Bluetooth Base UUID. Case-insensitive.
fn short_uuid(uuid: &str) -> Option<u16> {
    let lower = uuid.to_ascii_lowercase();
    let short = lower.strip_suffix(BASE_UUID_TAIL)?.strip_prefix("0000")?;
    if short.len() != 4 || !short.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u16::from_str_radix(short, 16).ok()
}

/// A readable name for the services an `AuthorizeService` prompt is most
/// likely to name, or `None` for anything else.
///
/// The numbers are the Bluetooth SIG's assigned numbers ("Service Class and
/// Profile UUIDs", plus the GATT "Human Interface Device" service for HID
/// over GATT), checked against `BlueZ` 5.87's `lib/bluetooth/uuid.h` and its
/// name table in `src/shared/util.c`. bluetoothd authorizes A2DP under
/// `ADVANCED_AUDIO_UUID` (`profiles/audio/a2dp.c`), AVRCP under
/// `AVRCP_REMOTE_UUID` (`profiles/audio/avctp.c`), PAN under `BNEP_SVC_UUID`
/// (`profiles/network/server.c`) and classic HID under `HID_UUID`
/// (`profiles/input/server.c`). HFP and HSP come from external profiles
/// (`PipeWire`, `oFono`), which bluetoothd authorizes under the profile's own
/// UUID (`src/profile.c`).
fn service_name(uuid: &str) -> Option<&'static str> {
    Some(match short_uuid(uuid)? {
        0x110a => "audio streaming (A2DP source)",
        0x110b => "audio streaming (A2DP sink)",
        0x110d => "audio streaming (A2DP)",
        0x110c | 0x110e | 0x110f => "media controls (AVRCP)",
        0x1108 | 0x1112 | 0x1131 => "headset audio (HSP)",
        0x111e | 0x111f => "hands-free calling (HFP)",
        0x1124 | 0x1812 => "keyboard and mouse input (HID)",
        0x000f | 0x1115 | 0x1116 | 0x1117 => "network sharing (PAN)",
        _ => return None,
    })
}

/// What the prompt calls the service `uuid` names: its readable name, or
/// the UUID itself when it is not one we name.
fn service_label(uuid: &str) -> String {
    service_name(uuid).map_or_else(|| uuid.to_owned(), str::to_owned)
}

/// Map a yes/no prompt's answer to the reply bluetoothd expects: `Confirm`
/// is `Ok`, anything else (a rejection, a cancel, a timeout, no prompt slot)
/// is `org.bluez.Error.Rejected` with `why`.
fn allow_or_reject(reply: &AgentReply, why: &str) -> Result<(), AgentError> {
    if matches!(reply, AgentReply::Confirm) {
        Ok(())
    } else {
        Err(AgentError::Rejected(why.into()))
    }
}

// Display-only handlers receive owned values they only acknowledge.
#[allow(clippy::needless_pass_by_value)]
#[zbus::interface(name = "org.bluez.Agent1")]
impl PairAgent {
    async fn release(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "Release").await?;
        tracing::debug!("agent released");
        Ok(())
    }

    async fn request_pin_code(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String, AgentError> {
        self.admit(conn, &header, "RequestPinCode").await?;
        self.pin_code_for(&device).await
    }

    async fn display_pin_code(
        &self,
        device: OwnedObjectPath,
        pincode: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "DisplayPinCode").await?;
        // Display-only acknowledgement — there is no return value to gate.
        // The user enters the PIN on the remote device. Nothing to do.
        let _ = (device, pincode);
        Ok(())
    }

    async fn request_passkey(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<u32, AgentError> {
        self.admit(conn, &header, "RequestPasskey").await?;
        self.passkey_for(&device).await
    }

    async fn display_passkey(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        entered: u16,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "DisplayPasskey").await?;
        // Same as DisplayPinCode — no input from us.
        let _ = (device, passkey, entered);
        Ok(())
    }

    async fn request_confirmation(
        &self,
        device: OwnedObjectPath,
        passkey: u32,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "RequestConfirmation").await?;
        self.confirmation_for(&device, passkey).await
    }

    async fn request_authorization(
        &self,
        device: OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "RequestAuthorization").await?;
        self.authorization_for(&device).await
    }

    async fn authorize_service(
        &self,
        device: OwnedObjectPath,
        uuid: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "AuthorizeService").await?;
        self.authorize_service_for(&device, &uuid).await
    }

    /// Reject whichever request is waiting. Its banner is taken down by that
    /// request itself, when it wakes ([`await_reply_in`]'s [`OwnBanner`]).
    /// This method does not take it down.
    ///
    /// It used to clear the banner here too, unconditionally. Between taking
    /// the sender and clearing the banner, the next request could already
    /// have raised its own banner, and that clear wiped it. The newer request
    /// would then wait out bluetoothd's minute with nothing on screen.
    ///
    /// Only bluetoothd may cancel: anyone else is refused and the waiting
    /// request keeps waiting.
    async fn cancel(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), AgentError> {
        self.admit(conn, &header, "Cancel").await?;
        tracing::debug!("agent cancel — aborting pending prompt");
        cancel_in(&self.pending).await;
        Ok(())
    }
}

/// The one agent request waiting on the user, keyed by the prompt it raised.
///
/// The key is what keeps an answer on the request it was given for (#1551).
/// The slot used to hold the bare sender, and an answer went to whatever
/// request held the slot when it landed. After a `Cancel` took request A's
/// sender, request B could install itself before a click on A's banner was
/// applied, and that click then answered B: a pairing confirmed for a code
/// the user never saw.
pub(crate) struct Waiting {
    /// [`PairPrompt::id`] of the prompt this request raised.
    id: u64,
    /// The sender this request's [`await_reply_in`] is waiting on.
    tx: oneshot::Sender<AgentReply>,
}

/// The reply slot: [`Waiting`] while a request waits on the user, `None`
/// otherwise.
pub(crate) type ReplySlot = AsyncMutex<Option<Waiting>>;

/// Deliver the user's `reply` to the request that raised prompt `id`, and to
/// no other.
///
/// A reply for a prompt that is no longer waiting (answered, cancelled, timed
/// out, or replaced by a newer request) is dropped with a debug line. It
/// decides nothing: the request in the slot, if any, keeps waiting on its own
/// prompt (the #1510 rule the agents plugin follows for consent cards).
pub(super) async fn answer_in(pending: &ReplySlot, id: u64, reply: AgentReply) {
    let mut slot = pending.lock().await;
    match slot.take() {
        Some(waiting) if waiting.id == id => {
            let _ = waiting.tx.send(reply);
        }
        other => {
            tracing::debug!(
                id,
                waiting = ?other.as_ref().map(|w| w.id),
                "bluetooth: a reply for a prompt that is no longer waiting; dropped"
            );
            *slot = other;
        }
    }
}

/// Reject whichever request is waiting, whatever its prompt. bluetoothd's
/// `Cancel` names no request (it has one outstanding at a time), and the
/// watcher's cleanup after bluetoothd went away rejects whatever the dead
/// daemon left waiting.
pub(super) async fn cancel_in(pending: &ReplySlot) {
    if let Some(waiting) = pending.lock().await.take() {
        let _ = waiting.tx.send(AgentReply::Reject);
    }
}

/// Source of [`PairPrompt::id`]. Starts at 1 and counts up for the life of
/// the process, so no two requests share an id.
static NEXT_PROMPT_ID: AtomicU64 = AtomicU64::new(1);

/// A fresh prompt for one agent request about `device`: its own id, plus the
/// device's label as the watcher currently knows it.
fn new_prompt(device: &OwnedObjectPath, passkey: Option<u32>, kind: PromptKind) -> PairPrompt {
    let device_path = device.as_str().to_string();
    PairPrompt {
        id: NEXT_PROMPT_ID.fetch_add(1, Ordering::Relaxed),
        alias: lookup_alias(&device_path),
        address: lookup_address(&device_path),
        device_path,
        passkey,
        kind,
        service: None,
    }
}

/// The device's Bluetooth address (`AA:BB:CC:DD:EE:FF`): the snapshot's,
/// else the one bluetoothd spells into the object path's last segment
/// (`…/dev_AA_BB_CC_DD_EE_FF`), else empty.
fn lookup_address(path: &str) -> String {
    shared_state()
        .and_then(|s| {
            let devs = s.devices.lock_ref();
            devs.iter()
                .find(|d| d.path == path && !d.address.is_empty())
                .map(|d| d.address.clone())
        })
        .or_else(|| address_from_path(path))
        .unwrap_or_default()
}

/// The address in a `BlueZ` device path, `…/dev_AA_BB_CC_DD_EE_FF` →
/// `AA:BB:CC:DD:EE:FF`, or `None` if the path does not end in one.
fn address_from_path(path: &str) -> Option<String> {
    let octets = path.rsplit('/').next()?.strip_prefix("dev_")?;
    let parts: Vec<&str> = octets.split('_').collect();
    let well_formed = parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()));
    well_formed.then(|| parts.join(":"))
}

/// Resolve a device path to a user-facing label. Prefers the `BlueZ` Alias,
/// falls through to the MAC address, and ultimately "Unknown device" so a
/// raw D-Bus object path never bleeds into UI copy.
fn lookup_alias(path: &str) -> String {
    shared_state()
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

/// Suspend the calling Agent1 method until the user responds via the UI.
/// Returns `AgentReply::Reject` if the channel is dropped or another pair is
/// already in flight — callers pattern-match on the returned variant to shape
/// their D-Bus return.
///
/// `pending` holds the sender the UI answers through. `banner` holds the
/// prompt the UI shows. This request puts its sender in `pending`, then
/// raises its prompt in `banner`. Once it wakes, it takes down that prompt
/// only if `banner` still shows it ([`OwnBanner`]). A wake can come late:
/// after a `Cancel` or an answer has taken the sender, the next request can
/// install itself and raise its own prompt before this task is scheduled
/// again. Before this check, that late wake cleared the newer prompt, and the
/// newer request waited out bluetoothd's minute with nothing on screen.
///
/// A request whose future is dropped before anyone answers it frees
/// `pending` too, and a sender nobody is waiting on any more never refuses
/// the next request (#1551). Until then the dropped request's sender stayed
/// in `pending`, and every later request saw it, took it for a pairing in
/// flight and answered `Reject`, until something took it out.
///
/// The sender goes into `pending` under this request's prompt id, so only an
/// answer given on this request's own prompt reaches it ([`answer_in`]).
async fn await_reply_in(
    pending: &ReplySlot,
    banner: &Mutable<Option<PairPrompt>>,
    prompt: PairPrompt,
) -> AgentReply {
    let (tx, rx) = oneshot::channel::<AgentReply>();
    {
        let mut guard = pending.lock().await;
        // A closed sender is a request that is gone: its receiver was dropped
        // without anyone answering it. [`OwnBanner`] frees the slot when that
        // happens, but only if it can take the lock right then, so this is
        // the backstop for when it could not.
        if guard
            .as_ref()
            .is_some_and(|waiting| !waiting.tx.is_closed())
        {
            // Another pairing already pending — refuse cleanly so BlueZ
            // doesn't pile up coincident prompts.
            return AgentReply::Reject;
        }
        *guard = Some(Waiting { id: prompt.id, tx });
    }

    // From here on the banner and the reply slot are this request's to clean
    // up, on every exit, including this future being dropped mid-await.
    let mut own = OwnBanner {
        banner: banner.clone(),
        pending,
        id: prompt.id,
        rx,
    };
    banner.set(Some(prompt));
    (&mut own.rx).await.unwrap_or(AgentReply::Reject)
}

/// One request's claim on the banner and on the reply slot, cleaned up when
/// the request ends, however it ends.
///
/// On drop it clears the banner only if the banner still shows request `id`,
/// so a newer request's prompt stays up. Then it frees `pending` if the
/// sender there can no longer reach anyone. The guard closes its own receiver
/// first, so if `pending` still holds this request's sender, that sender is
/// now closed. A newer request's sender is open, because its receiver is
/// waiting, so it is left alone. The only other closed sender `pending` could
/// hold is one left behind by an earlier dropped request, and freeing that is
/// right too. Once this request was answered, its sender had already been
/// taken out of `pending`, so there is nothing of its own left to clear.
///
/// `pending` is an async mutex and `drop` cannot wait for it, so the slot is
/// freed only if the lock is free right now. When it is not, the sender stays
/// until whoever holds the lock takes it, or until the next request finds it
/// closed and takes its place ([`await_reply_in`]).
struct OwnBanner<'a> {
    banner: Mutable<Option<PairPrompt>>,
    pending: &'a ReplySlot,
    id: u64,
    rx: oneshot::Receiver<AgentReply>,
}

impl Drop for OwnBanner<'_> {
    fn drop(&mut self) {
        clear_banner_if(&self.banner, self.id);
        self.rx.close();
        if let Ok(mut slot) = self.pending.try_lock()
            && slot.as_ref().is_some_and(|waiting| waiting.tx.is_closed())
        {
            *slot = None;
        }
    }
}

/// Clear `banner` if it shows request `id`; otherwise leave it alone.
///
/// The check and the clear happen under one `lock_mut`, so a newer prompt
/// cannot land between them. The lock only notifies subscribers when it is
/// written through, so the not-ours branch causes no UI rebuild. That matters:
/// a rebuild would throw away a PIN the user is halfway through typing.
fn clear_banner_if(banner: &Mutable<Option<PairPrompt>>, id: u64) {
    let mut current = banner.lock_mut();
    if current.as_ref().is_some_and(|p| p.id == id) {
        *current = None;
    }
}

/// Start the pairing-agent registration loop. Uses `bus::own_name` on the
/// SYSTEM bus (`BlueZ` is on system bus; it records the unique name of the
/// connection that called `RegisterAgent`). Mounting `PairAgent` at
/// `AGENT_PATH` via `.at_path()` ensures the object is visible before
/// `RequestName` succeeds, so `BlueZ` never races a missing object.
///
/// After the name is owned, we call `RegisterAgent` and
/// `RequestDefaultAgent` via `bus::call`, retrying a failure
/// ([`register_until_accepted`]), and register again on every [`AgentWake`]:
/// bluetoothd gaining a new owner, or a hole in the owner stream.
///
/// The `ownership` handle is passed in (created in `Service::start` and also
/// stored in `BluetoothHandles`) to keep the interface alive for the duration
/// of this task.
pub(super) async fn run_agent(_ownership: hytte_bus::OwnNameSignal) {
    // The receiver is taken before the start-up registration, so an owner
    // change made while that registration runs is not missed.
    let owner_sub = super::devices::name_owner_changed();
    let mut owner_items = owner_sub.items();
    agent_loop(
        &mut owner_items,
        register_agent_once,
        super::devices::bluez_has_owner,
        crate::wifi::nm_registrar::REGISTER_RETRY,
    )
    .await;
}

/// What one item of the bus's `NameOwnerChanged` stream means for the
/// pairing agent's registration with bluetoothd.
#[derive(Debug, PartialEq, Eq)]
enum AgentWake {
    /// Register again. Either `org.bluez` has a new owner, a bluetoothd that
    /// knows no agent, or the stream reported a hole (`Resubscribed` /
    /// `Lagged`). A `Resubscribed` is also how a reconnect of this process's
    /// own system-bus connection shows up, and that reconnect gives it a new
    /// unique name: bluetoothd keys agents by the caller's unique name and
    /// drops one whose owner disconnects (`agent_disconnect`, `src/agent.c`),
    /// so the agent is gone although bluetoothd never changed hands (#1551).
    Register,
    /// `org.bluez` has no owner: bluetoothd exited. Stop trying to register
    /// until it is back, since a call to `org.bluez` would D-Bus-activate a
    /// bluetoothd stopped on purpose.
    Gone,
}

/// Classify one owner-stream item. `None` for any name other than
/// `org.bluez`, and for a body that does not parse.
fn agent_wake(item: &SignalItem) -> Option<AgentWake> {
    match item {
        SignalItem::Event(evt) => {
            let (name, _old, new) = evt
                .body
                .body()
                .deserialize::<(String, String, String)>()
                .ok()?;
            if name != "org.bluez" {
                return None;
            }
            Some(if new.is_empty() {
                AgentWake::Gone
            } else {
                AgentWake::Register
            })
        }
        SignalItem::Resubscribed | SignalItem::Lagged { .. } => Some(AgentWake::Register),
    }
}

/// [`run_agent`]'s body, with the registration, the bus question and the
/// retry ramp injected so a test can drive it under paused time.
///
/// Registers once at start-up, then on every [`AgentWake::Register`]. A wake
/// that lands while a registration is still retrying starts it over;
/// [`AgentWake::Gone`] stops it. Until #1551 only an `org.bluez` owner change
/// woke this, and the stream's markers were dropped (`events()`), so after a
/// reconnect of the shell's own bus connection bluetoothd had no agent until
/// it next restarted, and a pairing that needed a confirmation failed with no
/// prompt.
async fn agent_loop<S, R, RF, H, HF>(
    owner_items: &mut S,
    register: R,
    has_owner: H,
    policy: retry::Policy,
) where
    S: Stream<Item = SignalItem> + Unpin,
    R: Fn() -> RF,
    RF: Future<Output = Result<(), hytte_bus::BusError>>,
    H: Fn() -> HF,
    HF: Future<Output = Result<bool, hytte_bus::BusError>>,
{
    // The start-up registration, unasked, as before: it is what starts an
    // activatable bluetoothd that is not running yet.
    let mut want = true;
    loop {
        if !want {
            let Some(item) = owner_items.next().await else {
                return;
            };
            match agent_wake(&item) {
                Some(AgentWake::Register) => want = true,
                Some(AgentWake::Gone) => {
                    tracing::warn!("org.bluez lost — will re-register agent on restart");
                }
                None => {}
            }
            continue;
        }
        want = false;
        tracing::debug!("bluetooth agent: registering with bluetoothd");
        let mut registering =
            std::pin::pin!(register_until_accepted(&register, &has_owner, policy));
        loop {
            tokio::select! {
                () = &mut registering => break,
                item = owner_items.next() => {
                    let Some(item) = item else { return };
                    match agent_wake(&item) {
                        Some(AgentWake::Register) => {
                            want = true;
                            break;
                        }
                        Some(AgentWake::Gone) => {
                            tracing::warn!("org.bluez lost — will re-register agent on restart");
                            break;
                        }
                        None => {}
                    }
                }
            }
        }
    }
}

/// Register the agent, retrying a failure on `policy` while bluetoothd is
/// running (#1551). Returns once bluetoothd took it, or once it is not
/// running any more: its return is an [`AgentWake::Register`].
///
/// Until #1551 a failed registration was one `warn!` and nothing more, and
/// bluetoothd had no agent until it next changed hands. The ramp is the iwd
/// watcher's and the `NetworkManager` agent's (`REGISTER_RETRY`): unbounded
/// attempts, a bounded delay. A refusal that persists is one `warn!`, then
/// `debug!` ([`retry::FailureLatch`]). Each retry first asks the bus whether
/// bluetoothd is running (`has_owner`), never bluetoothd itself, so a retry
/// never starts one that was stopped.
async fn register_until_accepted<R, RF, H, HF>(register: &R, has_owner: &H, policy: retry::Policy)
where
    R: Fn() -> RF,
    RF: Future<Output = Result<(), hytte_bus::BusError>>,
    H: Fn() -> HF,
    HF: Future<Output = Result<bool, hytte_bus::BusError>>,
{
    let mut latch = retry::FailureLatch::new();
    // 1-based, and counts the attempt that produced the outcome being
    // weighed, per `retry::Policy::step`.
    let mut attempt: u32 = 1;
    loop {
        let outcome = register().await;
        let verdict = policy.step(&outcome, attempt);
        log_registration(&outcome, latch.record(outcome.is_ok()), attempt, verdict);
        match verdict {
            retry::Step::Retry { after } => {
                tokio::time::sleep(after).await;
                if matches!(has_owner().await, Ok(false)) {
                    tracing::debug!(
                        "bluetooth agent: bluetoothd is not running; registering when it returns"
                    );
                    return;
                }
                attempt = attempt.saturating_add(1);
            }
            // `GiveUp` is unreachable under the unbounded ramp.
            retry::Step::Proceed | retry::Step::GiveUp => return,
        }
    }
}

/// What one registration attempt is worth saying, given the ones before it.
/// The iwd watcher's `log_registration` is the same cadence for its agent.
fn log_registration(
    outcome: &Result<(), hytte_bus::BusError>,
    report: retry::Report,
    attempt: u32,
    verdict: retry::Step,
) {
    let error = outcome
        .as_ref()
        .err()
        .map_or_else(String::new, ToString::to_string);
    let retry_in_secs = match verdict {
        retry::Step::Retry { after } => after.as_secs_f64(),
        retry::Step::Proceed | retry::Step::GiveUp => 0.0,
    };
    match report {
        retry::Report::Quiet => tracing::info!("bluetooth pairing agent registered"),
        retry::Report::Recovered => tracing::info!(
            failed_attempts = attempt.saturating_sub(1),
            "bluetooth pairing agent registered after earlier attempts failed"
        ),
        retry::Report::Opened => tracing::warn!(
            attempt,
            retry_in_secs,
            %error,
            "bluetooth agent registration failed — bluetoothd cannot ask this shell to \
             confirm a pairing until it succeeds. Retrying with backoff while bluetoothd \
             runs, and again whenever it changes hands; this line does not repeat while \
             the failure does"
        ),
        retry::Report::Repeating => tracing::debug!(
            attempt,
            retry_in_secs,
            %error,
            "bluetooth agent registration still failing"
        ),
    }
}

/// Register the pairing agent with bluetoothd once: `RegisterAgent`, then
/// `RequestDefaultAgent`.
///
/// bluetoothd's `AlreadyExists` for `RegisterAgent` counts as registered: it
/// keys agents by the caller's unique name and answers that only when this
/// connection already has one (`register_agent`, `src/agent.c`), which is what
/// a re-registration after a `Lagged` hole meets when nothing was lost.
/// `RequestDefaultAgent` still follows, so the agent is the default either way.
async fn register_agent_once() -> Result<(), hytte_bus::BusError> {
    let agent_op = zbus::zvariant::ObjectPath::try_from(AGENT_PATH)
        .map_err(|e| hytte_bus::BusError::Permanent {
            reason: format!("bad agent path: {e}"),
            dbus_name: None,
        })?
        .to_owned();

    // RegisterAgent — capability "DisplayYesNo": we can show a code and
    // accept yes/no, which is what RequestConfirmation needs.
    match hytte_bus::call(BusKind::System, "org.bluez")
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RegisterAgent")
        .args((agent_op.clone(), "DisplayYesNo".to_string()))
        .send::<()>()
        .await
    {
        Ok(()) => {}
        Err(e) if super::is_already_exists(&e) => {
            tracing::debug!("bluetoothd already holds this connection's agent");
        }
        Err(e) => return Err(e),
    }

    // RequestDefaultAgent — make us the system-wide default. Without this
    // BlueZ may use whichever Agent it sees first, including stale ones
    // from a previous trollshell run if any.
    hytte_bus::call(BusKind::System, "org.bluez")
        .at_path("/org/bluez")
        .iface("org.bluez.AgentManager1")
        .method("RequestDefaultAgent")
        .args((agent_op,))
        .send::<()>()
        .await
}

#[cfg(test)]
mod tests {
    use super::{
        AgentReply, PairPrompt, PromptKind, ReplySlot, answer_in, await_reply_in, cancel_in,
        clear_banner_if,
    };
    use futures_signals::signal::{Mutable, Signal};
    use futures_util::FutureExt;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

    type Pending = Arc<ReplySlot>;

    fn prompt(id: u64) -> PairPrompt {
        PairPrompt {
            id,
            device_path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".to_owned(),
            alias: "Keyboard".to_owned(),
            address: "AA:BB:CC:DD:EE:FF".to_owned(),
            passkey: Some(123_456),
            kind: PromptKind::ConfirmPasskey,
            service: None,
        }
    }

    /// The id of the prompt on the banner, if any.
    fn shown(banner: &Mutable<Option<PairPrompt>>) -> Option<u64> {
        banner.lock_ref().as_ref().map(|p| p.id)
    }

    /// The user answers prompt `id`, the way `send_reply` does.
    async fn answer(pending: &Pending, id: u64, reply: AgentReply) {
        answer_in(pending, id, reply).await;
    }

    /// Cancel(A), then Request(B), then A's task finally runs. A's late wake
    /// must leave B's banner up.
    ///
    /// The interleaving is forced, not hoped for. On a current-thread runtime,
    /// A cannot run between the cancel that wakes it and B being polled by
    /// hand, so B has installed itself and raised its banner before A clears
    /// anything.
    ///
    /// **Falsification:** clear the banner unconditionally once `rx` resolves
    /// (the pre-fix `clear_prompt()`) and the "A's late wake" assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_wake_of_a_cancelled_request_leaves_the_newer_banner_up() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let a = tokio::spawn({
            let pending = pending.clone();
            let banner = banner.clone();
            async move { await_reply_in(&pending, &banner, prompt(1)).await }
        });
        while shown(&banner) != Some(1) {
            tokio::task::yield_now().await;
        }

        // Cancel(A). A is now runnable but has not run.
        cancel_in(&pending).await;

        // Request(B) lands first: one poll by hand installs it and raises its
        // banner, with no await point where A could be scheduled.
        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none(), "B waits on the user");
        assert_eq!(shown(&banner), Some(2), "B's banner is up");

        // A finally runs.
        let a_reply = a.await.expect("request A panicked");
        assert!(matches!(a_reply, AgentReply::Reject));
        assert_eq!(
            shown(&banner),
            Some(2),
            "A's late wake must not take down B's banner"
        );

        // B is answered: its own banner goes.
        answer(&pending, 2, AgentReply::Confirm).await;
        assert!(matches!(b.await, AgentReply::Confirm));
        assert_eq!(shown(&banner), None, "B takes its own banner down");
    }

    /// Answering a request takes its banner down: the ordinary path is
    /// unchanged by the id check.
    #[tokio::test(flavor = "current_thread")]
    async fn an_answered_request_takes_its_own_banner_down() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = std::pin::pin!(await_reply_in(&pending, &banner, prompt(7)));
        assert!(a.as_mut().now_or_never().is_none());
        assert_eq!(shown(&banner), Some(7));

        answer(&pending, 7, AgentReply::Confirm).await;
        assert!(matches!(a.await, AgentReply::Confirm));
        assert_eq!(shown(&banner), None);
    }

    /// A request whose future is dropped mid-wait still takes its banner
    /// down, so no prompt is left on screen with nothing behind it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_request_takes_its_own_banner_down() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = Box::pin(await_reply_in(&pending, &banner, prompt(3)));
        assert!(a.as_mut().now_or_never().is_none());
        assert_eq!(shown(&banner), Some(3));

        drop(a);
        assert_eq!(shown(&banner), None);
    }

    /// A request whose future is dropped before anyone answers it frees the
    /// reply slot, so the next request is not refused.
    ///
    /// Before #1551 only the banner was cleaned up on that path. The dropped
    /// request's sender stayed in the slot, and every later request took it
    /// for a pairing in flight and answered `Reject` without raising a prompt.
    ///
    /// **Falsification:** remove the slot clear from `OwnBanner::drop` and the
    /// "frees the slot" assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_request_frees_the_reply_slot() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = Box::pin(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());
        assert!(pending.lock().await.is_some(), "A's sender is in the slot");

        drop(a);
        assert!(pending.lock().await.is_none(), "dropping A frees the slot");

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(
            b.as_mut().now_or_never().is_none(),
            "B waits on the user instead of being refused"
        );
        assert_eq!(shown(&banner), Some(2));
        answer(&pending, 2, AgentReply::Confirm).await;
        assert!(matches!(b.await, AgentReply::Confirm));
    }

    /// The backstop for a drop that could not take the lock: a sender left in
    /// the slot whose request is gone does not refuse the next request.
    ///
    /// **Falsification:** make `await_reply_in` refuse whenever the slot holds
    /// any sender (the pre-#1551 `guard.is_some()`) and B is refused.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dead_sender_left_in_the_slot_does_not_refuse_the_next_request() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = Box::pin(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());
        {
            // Someone holds the lock while A is dropped, so A's guard cannot
            // free the slot.
            let held = pending.try_lock().expect("the slot is not locked");
            drop(a);
            assert!(
                held.as_ref().is_some_and(|waiting| waiting.tx.is_closed()),
                "A's sender is still in the slot, and closed"
            );
        }

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(
            b.as_mut().now_or_never().is_none(),
            "B waits on the user instead of being refused"
        );
        assert_eq!(shown(&banner), Some(2));
        answer(&pending, 2, AgentReply::Confirm).await;
        assert!(matches!(b.await, AgentReply::Confirm));
    }

    /// While a request is waiting on the user, a second one is still refused:
    /// the backstop above only skips a sender nobody is waiting on.
    ///
    /// **Falsification:** let `await_reply_in` take the slot whatever it holds
    /// and B replaces A instead of being refused.
    #[tokio::test(flavor = "current_thread")]
    async fn a_waiting_request_still_refuses_a_second() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = std::pin::pin!(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());

        let b = await_reply_in(&pending, &banner, prompt(2)).now_or_never();
        assert!(
            matches!(b, Some(AgentReply::Reject)),
            "B is refused at once, got {b:?}"
        );
        assert_eq!(shown(&banner), Some(1), "A's banner stays up");

        answer(&pending, 1, AgentReply::Confirm).await;
        assert!(
            matches!(a.await, AgentReply::Confirm),
            "A is still answerable"
        );
    }

    /// A request answered and then dropped before its task ran leaves the
    /// next request's sender in the slot: the drop frees only a sender
    /// nobody is waiting on.
    ///
    /// **Falsification:** make `OwnBanner::drop` clear the slot whatever it
    /// holds and B's sender is gone, so B can never be answered.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_an_answered_request_leaves_the_newer_sender() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = Box::pin(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());
        answer(&pending, 1, AgentReply::Reject).await;

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none());

        drop(a);
        assert!(
            pending.lock().await.is_some(),
            "B's sender must stay in the slot"
        );
        assert_eq!(shown(&banner), Some(2), "B's banner stays up");
        answer(&pending, 2, AgentReply::Confirm).await;
        assert!(matches!(b.await, AgentReply::Confirm));
    }

    /// A Confirm clicked on A's banner never confirms B (#1551).
    ///
    /// bluetoothd cancels A, B installs itself and raises its banner, and
    /// only then is the click the user made on A's banner applied. That click
    /// is for a prompt that is no longer waiting, so it decides nothing: B
    /// keeps waiting on its own prompt, and only an answer for B's id
    /// reaches it.
    ///
    /// **Falsification:** let `answer_in` take whatever request is waiting
    /// (the pre-#1551 `send_reply`) and B is confirmed by A's click, a
    /// pairing for a code the user never saw.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reply_for_a_cancelled_prompt_does_not_answer_the_next_request() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = Box::pin(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());
        // bluetoothd's Cancel for A.
        cancel_in(&pending).await;

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none());
        assert_eq!(shown(&banner), Some(2), "B's banner is up");

        // The click was on A's banner.
        answer(&pending, 1, AgentReply::Confirm).await;
        assert!(
            b.as_mut().now_or_never().is_none(),
            "A's answer must not reach B"
        );
        assert!(
            pending.lock().await.is_some(),
            "B is still waiting on its own prompt"
        );
        assert_eq!(shown(&banner), Some(2), "B's banner stays up");

        answer(&pending, 2, AgentReply::Reject).await;
        assert!(matches!(b.await, AgentReply::Reject));
        assert!(matches!(a.await, AgentReply::Reject), "A was cancelled");
    }

    /// An answer for a prompt nobody is waiting on any more (already
    /// answered, say, by a double click) changes nothing, and the next request
    /// installs as usual.
    #[tokio::test(flavor = "current_thread")]
    async fn a_reply_with_nothing_waiting_is_dropped() {
        let pending: Pending = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);

        let mut a = std::pin::pin!(await_reply_in(&pending, &banner, prompt(1)));
        assert!(a.as_mut().now_or_never().is_none());
        answer(&pending, 1, AgentReply::Confirm).await;
        answer(&pending, 1, AgentReply::Reject).await;
        assert!(
            matches!(a.await, AgentReply::Confirm),
            "the first answer stands"
        );
        assert!(pending.lock().await.is_none(), "nothing is waiting");

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none(), "B is not refused");
        answer(&pending, 2, AgentReply::Confirm).await;
        assert!(matches!(b.await, AgentReply::Confirm));
    }

    /// The check itself. Clearing for an id that is not on the banner leaves
    /// the banner as it is, and notifies no subscriber: a spurious
    /// notification would rebuild the banner and lose a half-typed PIN.
    #[test]
    fn clearing_another_requests_id_leaves_the_banner_alone() {
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(Some(prompt(2)));
        let mut changes = Box::pin(banner.signal_ref(|p| p.as_ref().map(|p| p.id)));
        let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(
            changes.as_mut().poll_change(&mut cx).is_ready(),
            "a fresh subscriber sees the current value"
        );

        clear_banner_if(&banner, 1);
        assert_eq!(shown(&banner), Some(2));
        assert!(
            changes.as_mut().poll_change(&mut cx).is_pending(),
            "another request's id must not notify subscribers"
        );

        clear_banner_if(&banner, 2);
        assert_eq!(shown(&banner), None);
        clear_banner_if(&banner, 2);
        assert_eq!(shown(&banner), None, "clearing an empty banner is a no-op");
    }
}

/// `AuthorizeService` (#1491): what it asks, how the answer maps to the
/// reply bluetoothd gets, and what the prompt calls each service; and what
/// answering a pairing prompt does to the device's trust (#1603 review,
/// MED-1). Hermetic: the agent is driven past its caller check (which the
/// system tests cover), with its own banner, reply slot and a recording
/// device bus.
#[cfg(test)]
mod authorize_service_tests {
    use super::{
        AgentError, AgentReply, PairAgent, PairPrompt, PromptKind, ReplySlot, address_from_path,
        allow_or_reject, answer_in, cancel_in, service_label, service_name,
    };
    use crate::bluetooth::trust::fake::{Call, FakeBus};
    use crate::bluetooth::trust::{PAIRING_WINDOW, Pairings, Shown, pair_then_trust};
    use crate::bluetooth::{BluetoothShared, PairOutcome, pair_from_shell};
    use futures_signals::signal::Mutable;
    use futures_util::FutureExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::Notify;
    use zbus::zvariant::OwnedObjectPath;

    /// What bluetoothd sends for an incoming A2DP stream
    /// (`ADVANCED_AUDIO_UUID`).
    const A2DP: &str = "0000110d-0000-1000-8000-00805f9b34fb";

    /// An agent with its own slots and a recording device bus. Its caller
    /// check is never run here.
    struct Fixture {
        agent: PairAgent,
        pending: Arc<ReplySlot>,
        banner: Mutable<Option<PairPrompt>>,
        devices: Arc<FakeBus>,
        pairings: Pairings,
    }

    fn fixture() -> Fixture {
        let pending: Arc<ReplySlot> = Arc::new(AsyncMutex::new(None));
        let banner = Mutable::new(None);
        let devices = Arc::new(FakeBus::new());
        let pairings = Pairings::default();
        let agent = PairAgent::new(
            pending.clone(),
            banner.clone(),
            devices.clone(),
            pairings.clone(),
        );
        Fixture {
            agent,
            pending,
            banner,
            devices,
            pairings,
        }
    }

    /// [`fixture`] without the device bus.
    fn agent() -> (PairAgent, Arc<ReplySlot>, Mutable<Option<PairPrompt>>) {
        let f = fixture();
        (f.agent, f.pending, f.banner)
    }

    fn device() -> OwnedObjectPath {
        OwnedObjectPath::try_from("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF").expect("object path")
    }

    /// The reply bluetoothd gets for `result`, by D-Bus error name.
    fn reply_name(result: &Result<(), AgentError>) -> Option<String> {
        use zbus::DBusError as _;
        result.as_ref().err().map(|e| e.name().to_string())
    }

    /// `AuthorizeService` asks: the banner shows who wants what, and nothing
    /// is answered until the user does. Allow admits the service.
    ///
    /// **Falsification:** answer `Ok(())` without asking (the old
    /// auto-accept) and the "waits on the user" assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn it_asks_the_user_and_allow_admits_the_service() {
        let (agent, pending, banner) = agent();
        let device = device();
        let mut asking = std::pin::pin!(agent.authorize_service_for(&device, A2DP));
        assert!(
            asking.as_mut().now_or_never().is_none(),
            "AuthorizeService waits on the user"
        );

        let shown = banner.get_cloned().expect("a prompt is raised");
        assert_eq!(shown.kind, PromptKind::AuthorizeService);
        assert_eq!(shown.service.as_deref(), Some("audio streaming (A2DP)"));
        assert_eq!(shown.device_path, device.as_str());
        assert_eq!(
            shown.address, "AA:BB:CC:DD:EE:FF",
            "read off the path when the snapshot has none"
        );
        assert_eq!(shown.passkey, None);

        answer_in(&pending, shown.id, AgentReply::Confirm).await;
        assert!(asking.await.is_ok(), "Allow admits the service");
        assert!(banner.get_cloned().is_none(), "the prompt comes down");
    }

    /// Deny is `org.bluez.Error.Rejected`.
    #[tokio::test(flavor = "current_thread")]
    async fn deny_rejects_the_service() {
        let (agent, pending, banner) = agent();
        let device = device();
        let mut asking = std::pin::pin!(agent.authorize_service_for(&device, A2DP));
        assert!(asking.as_mut().now_or_never().is_none());
        let shown = banner.get_cloned().expect("a prompt is raised");

        answer_in(&pending, shown.id, AgentReply::Reject).await;
        assert_eq!(
            reply_name(&asking.await).as_deref(),
            Some("org.bluez.Error.Rejected")
        );
    }

    /// A `Cancel` — bluetoothd's own once its 60 s are up, or the watcher's
    /// cleanup after bluetoothd went away — is a rejection, not a default
    /// yes.
    #[tokio::test(flavor = "current_thread")]
    async fn a_cancel_or_timeout_rejects_the_service() {
        let (agent, pending, _banner) = agent();
        let device = device();
        let mut asking = std::pin::pin!(agent.authorize_service_for(&device, A2DP));
        assert!(asking.as_mut().now_or_never().is_none());

        cancel_in(&pending).await;
        assert_eq!(
            reply_name(&asking.await).as_deref(),
            Some("org.bluez.Error.Rejected")
        );
    }

    /// Allowing a service saves nothing: the device is not made trusted.
    #[tokio::test(flavor = "current_thread")]
    async fn allowing_a_service_does_not_trust_the_device() {
        let f = fixture();
        let device = device();
        let mut asking = std::pin::pin!(f.agent.authorize_service_for(&device, A2DP));
        assert!(asking.as_mut().now_or_never().is_none());
        let shown = f.banner.get_cloned().expect("a prompt is raised");
        answer_in(&f.pending, shown.id, AgentReply::Confirm).await;
        assert!(asking.await.is_ok());
        assert!(f.devices.calls().is_empty(), "{:?}", f.devices.calls());
    }

    /// Drive one pairing prompt of `kind` on a fresh agent, answer it with
    /// `accept`, and return whether bluetoothd got `Ok`, every device-bus call
    /// made by the time it did, and whether the device was in progress then.
    async fn answer_pairing(kind: PromptKind, accept: bool) -> (bool, Vec<Call>, bool) {
        let f = fixture();
        let device = device();
        let mut asking: std::pin::Pin<Box<dyn std::future::Future<Output = bool> + '_>> = match kind
        {
            PromptKind::ConfirmPasskey => {
                Box::pin(async { f.agent.confirmation_for(&device, 123_456).await.is_ok() })
            }
            PromptKind::Authorize => {
                Box::pin(async { f.agent.authorization_for(&device).await.is_ok() })
            }
            PromptKind::EnterPinCode => {
                Box::pin(async { f.agent.pin_code_for(&device).await.is_ok() })
            }
            PromptKind::EnterPasskey => {
                Box::pin(async { f.agent.passkey_for(&device).await.is_ok() })
            }
            PromptKind::AuthorizeService => unreachable!("not a pairing prompt"),
        };
        assert!(asking.as_mut().now_or_never().is_none(), "{kind:?} waits");
        let shown = f.banner.get_cloned().expect("a prompt is raised");
        assert_eq!(shown.kind, kind);
        let reply = match (accept, kind) {
            (false, _) => AgentReply::Reject,
            (true, PromptKind::EnterPinCode) => AgentReply::Pin("0000".to_owned()),
            (true, PromptKind::EnterPasskey) => AgentReply::Passkey(123_456),
            (true, _) => AgentReply::Confirm,
        };
        answer_in(&f.pending, shown.id, reply).await;
        let ok = asking.await;
        (ok, f.devices.calls(), f.pairings.contains(device.as_str()))
    }

    const PAIRING_PROMPTS: [PromptKind; 4] = [
        PromptKind::ConfirmPasskey,
        PromptKind::Authorize,
        PromptKind::EnterPinCode,
        PromptKind::EnterPasskey,
    ];

    /// Accepting a pairing the device started puts it in progress by the
    /// time bluetoothd has its reply, for every kind of pairing prompt, and
    /// sets nothing in bluetoothd yet: `Trusted` waits for the pairing to
    /// succeed (#1603 review, MED-A).
    ///
    /// **Falsification:** drop the `accepted` call from
    /// `in_progress_if_accepted` and the device is not in progress; trust it
    /// at once (the previous design) and a `SetTrusted(true)` shows up.
    #[tokio::test(flavor = "current_thread")]
    async fn accepting_a_pairing_puts_it_in_progress_and_trusts_nothing_yet() {
        for kind in PAIRING_PROMPTS {
            let (ok, calls, in_progress) = answer_pairing(kind, true).await;
            assert!(ok, "{kind:?} accepted");
            assert!(in_progress, "{kind:?}: in progress by the reply");
            assert!(calls.is_empty(), "{kind:?}: {calls:?}");
        }
    }

    /// A rejected pairing prompt never enters the set and sets nothing.
    ///
    /// **Falsification:** put the device in progress on any answer
    /// (`in_progress_if_accepted` ignoring `result`) and this fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_rejected_pairing_never_enters_the_set() {
        for kind in PAIRING_PROMPTS {
            let (ok, calls, in_progress) = answer_pairing(kind, false).await;
            assert!(!ok, "{kind:?} rejected");
            assert!(!in_progress, "{kind:?}: entered the set");
            assert!(calls.is_empty(), "{kind:?}: {calls:?}");
        }
    }

    /// While a device is being paired, its `AuthorizeService` is allowed at
    /// once, with no prompt and nothing set.
    ///
    /// **Falsification:** drop the in-progress check from
    /// `authorize_service_for` and the request raises a prompt and waits.
    #[tokio::test(flavor = "current_thread")]
    async fn a_service_request_during_a_pairing_is_allowed_without_a_prompt() {
        let f = fixture();
        let device = device();
        f.pairings.begin_pair(device.as_str());
        let answered = f.agent.authorize_service_for(&device, A2DP).now_or_never();
        assert!(
            matches!(answered, Some(Ok(()))),
            "allowed at once, got {answered:?}"
        );
        assert!(f.banner.get_cloned().is_none(), "no prompt");
        assert!(f.devices.calls().is_empty(), "nothing set");
    }

    /// LOW-2 (#1611): the set misses a device whose pairing has just
    /// succeeded, and bluetoothd already has it trusted. No prompt.
    ///
    /// This is the review's sequence: bluetoothd sends `AuthorizeService`,
    /// then applies the shell's `Set Trusted` and answers it, and the shell
    /// takes the device out of the set, all while the request is still in
    /// the caller check. So by the time it reaches the set, the device is out.
    ///
    /// **Falsification:** drop the read of bluetoothd's `Trusted` from
    /// `authorize_service_for` (prompt on every set miss) and the request
    /// raises a prompt and waits.
    #[tokio::test(flavor = "current_thread")]
    async fn a_device_trusted_since_bluetoothd_asked_is_allowed_without_a_prompt() {
        let f = fixture();
        let device = device();
        let outcome = pair_then_trust(&*f.devices, &f.pairings, device.as_str()).await;
        assert!(matches!(outcome, PairOutcome::Paired));
        assert!(
            !f.pairings.contains(device.as_str()),
            "the set misses: the pairing has ended"
        );

        let answered = f.agent.authorize_service_for(&device, A2DP).now_or_never();
        assert!(
            matches!(answered, Some(Ok(()))),
            "allowed at once, got {answered:?}"
        );
        assert!(f.banner.get_cloned().is_none(), "no prompt");
        assert_eq!(f.devices.trusted_reads(), 1, "bluetoothd was asked once");
        assert_eq!(
            f.devices.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect],
            "the request itself set nothing"
        );
    }

    /// A read of bluetoothd's `Trusted` that fails asks the user, as a miss
    /// did before the read existed: an unreadable device is not a trusted
    /// one.
    ///
    /// **Falsification:** take a failed read for "trusted" and the request
    /// is allowed without asking.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_trust_read_still_asks() {
        let f = fixture();
        f.devices.set_live_trusted(None);
        let device = device();
        let mut asking = std::pin::pin!(f.agent.authorize_service_for(&device, A2DP));
        assert!(
            asking.as_mut().now_or_never().is_none(),
            "the request asks the user"
        );
        let shown = f.banner.get_cloned().expect("a prompt is raised");
        assert_eq!(shown.kind, PromptKind::AuthorizeService);
        assert_eq!(f.devices.trusted_reads(), 1);
        answer_in(&f.pending, shown.id, AgentReply::Reject).await;
        assert!(asking.await.is_err());
    }

    /// LOW-1 (#1611): a device the shell's `Pair` puts in the set is one the
    /// agent built beside that state finds there. While the shell pairs it,
    /// bluetoothd's service request for it is allowed with no prompt.
    ///
    /// **Falsification:** give either side its own set, the review's two
    /// mutations, and the request misses the set and raises a prompt:
    /// - Q2: `pair_from_shell` pairs in `Pairings::default()` instead of
    ///   `shared.pairings`;
    /// - Q3: `BluetoothShared::with_agent` builds the agent with
    ///   `Pairings::default()` instead of `shared.pairings.clone()`.
    #[tokio::test(flavor = "current_thread")]
    async fn the_shells_pair_and_the_agent_share_one_pairing_set() {
        let bus = FakeBus::new();
        let gate = Arc::new(Notify::new());
        *bus.pair_gate.lock().unwrap() = Some(gate.clone());
        let bus = Arc::new(bus);
        let (shared, agent) = BluetoothShared::with_agent(bus.clone());
        let shared = Arc::new(shared);
        let device = device();

        let pairing = tokio::spawn({
            let shared = shared.clone();
            let bus = bus.clone();
            let path = device.as_str().to_owned();
            async move { pair_from_shell(&shared, &*bus, &path).await }
        });
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        assert_eq!(bus.calls(), vec![Call::Pair], "the shell's Pair is running");

        let answered = agent.authorize_service_for(&device, A2DP).now_or_never();
        assert!(
            matches!(answered, Some(Ok(()))),
            "a request during the shell's Pair is allowed at once, got {answered:?}"
        );
        assert!(shared.pair_prompt.get_cloned().is_none(), "no prompt");

        gate.notify_one();
        pairing.await.expect("pairing task");
        assert_eq!(
            bus.calls(),
            vec![Call::Pair, Call::SetTrusted(true), Call::Connect]
        );
        assert!(shared.action_error.get_cloned().is_none(), "no toast");
    }

    /// The agent built beside the shared state raises its prompts on that
    /// state's banner, the one the UI shows, and is answered through its reply
    /// slot, the one the UI answers through.
    ///
    /// **Falsification:** build the agent in `with_agent` with a banner or a
    /// reply slot of its own and the prompt never reaches the shared state.
    #[tokio::test(flavor = "current_thread")]
    async fn the_agent_prompts_on_the_shared_banner_and_reply_slot() {
        let (shared, agent) = BluetoothShared::with_agent(Arc::new(FakeBus::new()));
        let device = device();
        let mut asking = std::pin::pin!(agent.authorize_service_for(&device, A2DP));
        assert!(asking.as_mut().now_or_never().is_none());
        let shown = shared
            .pair_prompt
            .get_cloned()
            .expect("the prompt is on the shared banner");
        answer_in(&shared.pending_response, shown.id, AgentReply::Confirm).await;
        let answered = asking.now_or_never();
        assert!(
            matches!(answered, Some(Ok(()))),
            "answered through the shared slot, got {answered:?}"
        );
    }

    /// A pairing the shell starts that fails reaches the toast through the
    /// state it paired in.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_shell_pairing_reaches_the_toast() {
        let bus = FakeBus::new();
        *bus.pair_reply.lock().unwrap() = Some(Err(hytte_bus::BusError::Permanent {
            reason: "AuthenticationFailed".to_owned(),
            dbus_name: Some("org.bluez.Error.AuthenticationFailed".to_owned()),
        }));
        let bus = Arc::new(bus);
        let (shared, _agent) = BluetoothShared::with_agent(bus.clone());
        pair_from_shell(&shared, &*bus, device().as_str()).await;
        let toast = shared.action_error.get_cloned().expect("a toast");
        assert!(toast.message.starts_with("Couldn't pair: "), "{toast:?}");
        assert!(!shared.pairings.contains(device().as_str()));
    }

    /// Once an accepted pairing's window has passed without it completing,
    /// the device's service requests ask again.
    ///
    /// **Falsification:** let the in-progress entry outlive the window and
    /// the request is still allowed without a prompt.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn after_the_pairing_window_a_service_request_asks_again() {
        let f = fixture();
        let device = device();
        f.devices.show(Some(Shown::default()));
        let mut asking = std::pin::pin!(f.agent.confirmation_for(&device, 123_456));
        assert!(asking.as_mut().now_or_never().is_none());
        let shown = f.banner.get_cloned().expect("a prompt is raised");
        answer_in(&f.pending, shown.id, AgentReply::Confirm).await;
        assert!(asking.await.is_ok());
        assert!(f.pairings.contains(device.as_str()));

        tokio::time::sleep(PAIRING_WINDOW + Duration::from_secs(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!f.pairings.contains(device.as_str()), "the window passed");

        let mut asking = std::pin::pin!(f.agent.authorize_service_for(&device, A2DP));
        assert!(
            asking.as_mut().now_or_never().is_none(),
            "the request asks the user again"
        );
        assert_eq!(
            f.banner.get_cloned().map(|p| p.kind),
            Some(PromptKind::AuthorizeService)
        );
        assert!(f.devices.calls().is_empty(), "nothing was trusted");
    }

    /// The mapping itself: only an explicit yes is `Ok`.
    #[test]
    fn only_a_yes_is_allowed() {
        assert!(allow_or_reject(&AgentReply::Confirm, "no").is_ok());
        for reply in [
            AgentReply::Reject,
            AgentReply::Pin("0000".to_owned()),
            AgentReply::Passkey(1),
        ] {
            assert_eq!(
                reply_name(&allow_or_reject(&reply, "no")).as_deref(),
                Some("org.bluez.Error.Rejected"),
                "{reply:?} must reject"
            );
        }
    }

    /// The services a prompt names, by the UUID bluetoothd sends. The
    /// numbers are the SIG's assigned numbers; see `service_name`.
    #[test]
    fn common_services_have_readable_names() {
        let base = |short: &str| format!("0000{short}-0000-1000-8000-00805f9b34fb");
        let cases = [
            ("110a", "audio streaming (A2DP source)"),
            ("110b", "audio streaming (A2DP sink)"),
            ("110d", "audio streaming (A2DP)"),
            ("110c", "media controls (AVRCP)"),
            ("110e", "media controls (AVRCP)"),
            ("110f", "media controls (AVRCP)"),
            ("1108", "headset audio (HSP)"),
            ("1112", "headset audio (HSP)"),
            ("1131", "headset audio (HSP)"),
            ("111e", "hands-free calling (HFP)"),
            ("111f", "hands-free calling (HFP)"),
            ("1124", "keyboard and mouse input (HID)"),
            ("1812", "keyboard and mouse input (HID)"),
            ("000f", "network sharing (PAN)"),
            ("1115", "network sharing (PAN)"),
            ("1116", "network sharing (PAN)"),
            ("1117", "network sharing (PAN)"),
        ];
        for (short, name) in cases {
            assert_eq!(service_name(&base(short)), Some(name), "0x{short}");
        }
        assert_eq!(
            service_name("0000110B-0000-1000-8000-00805F9B34FB"),
            Some("audio streaming (A2DP sink)"),
            "UUIDs compare case-insensitively"
        );
    }

    /// The address comes out of a `BlueZ` device path, and nothing comes out
    /// of a path that does not end in one.
    #[test]
    fn the_address_is_read_off_the_device_path() {
        assert_eq!(
            address_from_path("/org/bluez/hci0/dev_0A_1b_2C_3d_4E_5f").as_deref(),
            Some("0A:1b:2C:3d:4E:5f")
        );
        for path in [
            "/org/bluez/hci0",
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE",
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF/sep1",
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_GG",
            "",
        ] {
            assert_eq!(address_from_path(path), None, "{path:?}");
        }
    }

    /// Anything else is shown as the UUID bluetoothd sent.
    #[test]
    fn any_other_service_is_shown_as_its_uuid() {
        for uuid in [
            // Serial Port: assigned, but not one we name.
            "00001101-0000-1000-8000-00805f9b34fb",
            // A vendor's own 128-bit UUID.
            "d0611e78-bbb4-4591-a5f8-487910ae4366",
            // Not hex where the short number goes.
            "0000+10b-0000-1000-8000-00805f9b34fb",
            "",
        ] {
            assert_eq!(service_name(uuid), None, "{uuid:?}");
            assert_eq!(service_label(uuid), uuid);
        }
        assert_eq!(
            service_label("0000110b-0000-1000-8000-00805f9b34fb"),
            "audio streaming (A2DP sink)"
        );
    }
}

/// The registration loop (#1551): what wakes it, the retry, and when the
/// retry stops. All under paused time, with the registration, the bus
/// question and the owner stream scripted.
#[cfg(test)]
mod registration_tests {
    use super::{AgentWake, agent_loop, agent_wake, register_until_accepted};
    use crate::retry;
    use hytte_bus::{BusError, SignalEvent, SignalItem};
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::time::Duration;

    /// A ramp of the shipped policy's shape with its own numbers, so tuning
    /// `REGISTER_RETRY` cannot redden these tests.
    const TEST_RAMP: retry::Policy = retry::Policy {
        max_attempts: None,
        initial: Duration::from_secs(1),
        max_backoff: Duration::from_secs(4),
    };

    fn refused(name: &str) -> BusError {
        BusError::Permanent {
            reason: name.rsplit('.').next().unwrap_or(name).to_owned(),
            dbus_name: Some(name.to_owned()),
        }
    }

    fn noc_item(name: &str, old: &str, new: &str) -> SignalItem {
        let body = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("signal builder")
        .build(&(name, old, new))
        .expect("build NameOwnerChanged");
        SignalItem::Event(SignalEvent {
            body,
            sender: None,
            timestamp: std::time::SystemTime::now(),
        })
    }

    /// bluetoothd changing hands, bluetoothd going away and a hole in the
    /// stream are told apart; any other name is not the agent's business.
    #[test]
    fn owner_items_are_classified() {
        assert_eq!(
            agent_wake(&noc_item("org.bluez", "", ":1.7")),
            Some(AgentWake::Register)
        );
        assert_eq!(
            agent_wake(&noc_item("org.bluez", ":1.6", ":1.7")),
            Some(AgentWake::Register)
        );
        assert_eq!(
            agent_wake(&noc_item("org.bluez", ":1.7", "")),
            Some(AgentWake::Gone)
        );
        assert_eq!(agent_wake(&noc_item("net.connman.iwd", "", ":1.8")), None);
        assert_eq!(agent_wake(&noc_item(":1.9", ":1.9", "")), None);
        assert_eq!(
            agent_wake(&SignalItem::Resubscribed),
            Some(AgentWake::Register)
        );
        assert_eq!(
            agent_wake(&SignalItem::Lagged { skipped: 3 }),
            Some(AgentWake::Register)
        );
    }

    /// A reconnect of the shell's own bus connection registers the agent
    /// again (#1551): it shows up as a `Resubscribed` marker, not as an
    /// `org.bluez` owner change, and bluetoothd dropped the agent with the
    /// old unique name.
    ///
    /// **Falsification:** ignore the stream's markers (the pre-#1551
    /// `events()`, or a `Resubscribed` arm answering `None`) and only the
    /// start-up registration is made.
    #[tokio::test(start_paused = true)]
    async fn a_reconnect_of_our_own_connection_registers_again() {
        let registrations = Cell::new(0_u32);
        let (tx, mut items) = futures_channel::mpsc::unbounded::<SignalItem>();

        let looping = agent_loop(
            &mut items,
            || {
                registrations.set(registrations.get() + 1);
                async { Ok(()) }
            },
            || async { Ok(true) },
            TEST_RAMP,
        );
        let script = async {
            // The marker comes once the start-up registration is done, as a
            // reconnect does: a wake that lands before the first attempt
            // just starts it over, and one registration covers both.
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(registrations.get(), 1, "the start-up registration");
            tx.unbounded_send(SignalItem::Resubscribed)
                .expect("loop is listening");
            tokio::time::sleep(Duration::from_secs(1)).await;
            assert_eq!(
                registrations.get(),
                2,
                "the reconnect registers the agent again"
            );
        };
        tokio::select! {
            () = looping => panic!("the loop never ends while its stream is open"),
            () = script => {}
        }
    }

    /// A refused registration is tried again on the ramp until bluetoothd
    /// takes it, rather than once (#1551).
    ///
    /// **Falsification:** return after the first attempt whatever its
    /// outcome (the pre-#1551 single `warn!`) and only one attempt is made.
    #[tokio::test(start_paused = true)]
    async fn a_refused_registration_is_retried_until_bluetoothd_takes_it() {
        let answers = RefCell::new(VecDeque::from([
            Err(refused("org.freedesktop.DBus.Error.NoReply")),
            Err(refused("org.bluez.Error.Failed")),
            Ok(()),
        ]));
        let attempts = Cell::new(0_u32);
        let started = tokio::time::Instant::now();

        tokio::time::timeout(
            Duration::from_mins(1),
            register_until_accepted(
                &|| {
                    attempts.set(attempts.get() + 1);
                    let answer = answers.borrow_mut().pop_front().expect("scripted");
                    async move { answer }
                },
                &|| async { Ok(true) },
                TEST_RAMP,
            ),
        )
        .await
        .expect("the registration must end once bluetoothd takes it");

        assert_eq!(attempts.get(), 3, "two refusals, then the one it took");
        assert_eq!(
            started.elapsed(),
            Duration::from_secs(1 + 2),
            "the retries wait on the ramp"
        );
    }

    /// A retry asks the bus first, and stops while bluetoothd is not running,
    /// so it never D-Bus-activates a bluetoothd that was stopped on purpose.
    /// Its return is the wake that registers again.
    ///
    /// **Falsification:** drop the `has_owner` check and the registration
    /// keeps calling `org.bluez` for the whole minute.
    #[tokio::test(start_paused = true)]
    async fn a_retry_waits_for_bluetoothd_to_be_running() {
        let attempts = Cell::new(0_u32);
        tokio::time::timeout(
            Duration::from_mins(1),
            register_until_accepted(
                &|| {
                    attempts.set(attempts.get() + 1);
                    async { Err(refused("org.freedesktop.DBus.Error.ServiceUnknown")) }
                },
                &|| async { Ok(false) },
                TEST_RAMP,
            ),
        )
        .await
        .expect("no owner must end the retry");
        assert_eq!(attempts.get(), 1, "only the attempt before the question");
    }

    /// bluetoothd going away while a registration is retrying stops it, and
    /// its return registers again.
    ///
    /// **Falsification:** let `Gone` leave the retry running and it keeps
    /// calling `org.bluez` after bluetoothd is gone.
    #[tokio::test(start_paused = true)]
    async fn bluetoothd_going_away_stops_the_retry() {
        let attempts = Cell::new(0_u32);
        let (tx, mut items) = futures_channel::mpsc::unbounded::<SignalItem>();

        let looping = agent_loop(
            &mut items,
            || {
                attempts.set(attempts.get() + 1);
                async { Err(refused("org.bluez.Error.Failed")) }
            },
            || async { Ok(true) },
            TEST_RAMP,
        );
        let script = async {
            // Let the start-up registration fail and retry for a while.
            tokio::time::sleep(Duration::from_secs(10)).await;
            let before = attempts.get();
            assert!(before > 1, "the refusal is retried, got {before}");
            tx.unbounded_send(noc_item("org.bluez", ":1.6", ""))
                .expect("loop is listening");
            tokio::time::sleep(Duration::from_secs(30)).await;
            assert_eq!(
                attempts.get(),
                before,
                "no attempt while bluetoothd is gone"
            );
            tx.unbounded_send(noc_item("org.bluez", "", ":1.7"))
                .expect("loop is listening");
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert_eq!(attempts.get(), before + 1, "its return registers again");
        };
        tokio::select! {
            () = looping => panic!("the loop never ends while its stream is open"),
            () = script => {}
        }
    }
}

// ── System tests: the agent's sender check over a real bus ───────────────────
//
// What only a broker can show: that each `Agent1` method reads its sender off
// the call and answers bluetoothd — whoever owns `org.bluez` — and refuses
// everyone else before raising a prompt or touching the reply slot. The agent
// is mounted on an ephemeral `dbus-daemon`, one connection owns `org.bluez`
// the way bluetoothd does, and another owns nothing. The `BusGuard` /
// `ephemeral_bus` harness is `wifi::nm_agent`'s, copied per module as this
// crate does (`devices.rs` and `screensaver.rs` carry the same copy). Run with:
//   cargo test -p hytte-services --features system-tests --lib bluetooth::agent
#[cfg(all(test, feature = "system-tests"))]
mod system_tests {
    use super::{
        AGENT_PATH, AgentReply, BLUEZ_BUS_NAME, PairAgent, PairPrompt, ReplySlot, answer_in,
    };
    use futures_signals::signal::Mutable;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use tokio::sync::Mutex as AsyncMutex;
    use zbus::Connection;
    use zbus::connection::Builder;
    use zbus::zvariant::OwnedObjectPath;

    /// Upper bound on any one round trip. A liveness guard, not a latency
    /// assertion: the same number and the same reasoning as
    /// `wifi::nm_agent`'s budget.
    const DBUS_REPLY_BUDGET: Duration = Duration::from_secs(30);

    const AGENT_IFACE: &str = "org.bluez.Agent1";
    const DEVICE: &str = "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF";
    const REJECTED: &str = "org.bluez.Error.Rejected";

    /// Kills the spawned `dbus-daemon` on drop (see `wifi::nm_agent`).
    struct BusGuard {
        child: Option<Child>,
        _tmp: TempDir,
        address: String,
    }

    impl Drop for BusGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.start_kill();
                tokio::task::block_in_place(|| {
                    let handle = tokio::runtime::Handle::current();
                    let _ = handle.block_on(child.wait());
                });
            }
        }
    }

    /// Spawn a fresh `dbus-daemon` and return a guard that kills it on drop.
    async fn ephemeral_bus() -> BusGuard {
        let tmp = TempDir::new().expect("create tempdir for dbus-daemon");
        let socket: PathBuf = tmp.path().join("bus");
        let address = format!("unix:path={}", socket.display());

        let config = tmp.path().join("session.conf");
        std::fs::write(
            &config,
            format!(
                r#"<?xml version="1.0"?>
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>{address}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#
            ),
        )
        .expect("write dbus-daemon config");

        let mut child = Command::new("dbus-daemon")
            .arg("--config-file")
            .arg(&config)
            .arg("--print-address=1")
            .arg("--nofork")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn dbus-daemon — install package `dbus` if missing");

        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut lines = BufReader::new(stdout).lines();
        let printed = tokio::time::timeout(DBUS_REPLY_BUDGET, lines.next_line())
            .await
            .expect("dbus-daemon address timeout")
            .expect("dbus-daemon read address")
            .expect("dbus-daemon closed stdout");
        assert!(
            printed.contains("unix:path="),
            "unexpected dbus-daemon address: {printed}"
        );

        BusGuard {
            child: Some(child),
            _tmp: tmp,
            address,
        }
    }

    /// The agent under test, mounted on the bus, plus the two slots it raises
    /// prompts on and waits in.
    struct Mounted {
        /// Keeps the agent mounted; never read.
        _server: Connection,
        /// The agent's unique name, which every proxy addresses.
        dest: String,
        pending: Arc<ReplySlot>,
        banner: Mutable<Option<PairPrompt>>,
    }

    /// Mount a `PairAgent`, built exactly as production builds it: its caller
    /// check asks the bus each call arrives on, which here is this test's.
    /// The agent is staged on the connection's builder, so its object server
    /// dispatches before anything can be read off the socket
    /// (`wifi::nm_agent`'s `mount_and_proxy` has the whole argument, #756).
    async fn mount(guard: &BusGuard) -> Mounted {
        let pending: Arc<ReplySlot> = Arc::new(AsyncMutex::new(None));
        let banner: Mutable<Option<PairPrompt>> = Mutable::new(None);
        let agent = PairAgent::new(
            pending.clone(),
            banner.clone(),
            Arc::new(crate::bluetooth::trust::fake::FakeBus::new()),
            crate::bluetooth::trust::Pairings::default(),
        );
        let server = Builder::address(guard.address.as_str())
            .expect("parse ephemeral bus address")
            .serve_at(AGENT_PATH, agent)
            .expect("stage the agent")
            .build()
            .await
            .expect("mount the agent");
        let dest = server
            .unique_name()
            .expect("server has a unique name")
            .to_string();
        Mounted {
            _server: server,
            dest,
            pending,
            banner,
        }
    }

    /// A proxy to the agent from a fresh connection that owns `name`
    /// (`org.bluez` for bluetoothd, `None` for anyone else).
    async fn proxy(
        guard: &BusGuard,
        mounted: &Mounted,
        name: Option<&str>,
    ) -> zbus::Proxy<'static> {
        let mut builder =
            Builder::address(guard.address.as_str()).expect("parse ephemeral bus address");
        if let Some(name) = name {
            builder = builder.name(name).expect("a valid well-known name");
        }
        let client = builder.build().await.expect("connect a client to the bus");
        zbus::Proxy::new(&client, mounted.dest.clone(), AGENT_PATH, AGENT_IFACE)
            .await
            .expect("build Agent1 proxy")
    }

    fn device() -> OwnedObjectPath {
        OwnedObjectPath::try_from(DEVICE).expect("a valid object path")
    }

    /// Wait until the banner shows a prompt, and return it.
    async fn shown(banner: &Mutable<Option<PairPrompt>>) -> PairPrompt {
        let end = tokio::time::Instant::now() + DBUS_REPLY_BUDGET;
        while tokio::time::Instant::now() < end {
            if let Some(prompt) = banner.get_cloned() {
                return prompt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no prompt was raised within {DBUS_REPLY_BUDGET:?}");
    }

    /// Assert the result is a `MethodError` named exactly `expected`.
    fn assert_error_name_is<T: std::fmt::Debug>(result: &Result<T, zbus::Error>, expected: &str) {
        match result {
            Err(zbus::Error::MethodError(name, _detail, _reply)) => {
                assert_eq!(name.as_str(), expected);
            }
            Err(other) => panic!("expected MethodError {expected:?}, got {other:?}"),
            Ok(value) => panic!("expected MethodError {expected:?}, got Ok({value:?})"),
        }
    }

    /// Every method that raises a prompt, called by a peer that is not
    /// bluetoothd, is rejected at once: no banner, nothing in the reply
    /// slot. bluetoothd's own request on the same agent is still asked and
    /// answered.
    ///
    /// **Falsification:** drop the `admit` call from any of
    /// `request_pin_code`, `request_passkey`, `request_confirmation`,
    /// `request_authorization` or `authorize_service`, and the intruder's
    /// request raises a banner and waits on it, so its "rejected at once"
    /// expectation times out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_from_a_peer_that_is_not_bluez_is_rejected_and_raises_no_prompt() {
        const A2DP: &str = "0000110d-0000-1000-8000-00805f9b34fb";

        let guard = ephemeral_bus().await;
        let mounted = mount(&guard).await;
        let intruder = proxy(&guard, &mounted, None).await;
        let bluez = proxy(&guard, &mounted, Some(BLUEZ_BUS_NAME)).await;

        for method in [
            "RequestPinCode",
            "RequestPasskey",
            "RequestConfirmation",
            "RequestAuthorization",
            "AuthorizeService",
        ] {
            let asked = async {
                match method {
                    "RequestConfirmation" => {
                        intruder.call_method(method, &(device(), 123_456_u32)).await
                    }
                    "AuthorizeService" => {
                        intruder
                            .call_method(method, &(device(), A2DP.to_string()))
                            .await
                    }
                    _ => intruder.call_method(method, &(device(),)).await,
                }
            };
            let refused = tokio::time::timeout(DBUS_REPLY_BUDGET, asked)
                .await
                .unwrap_or_else(|_| {
                    panic!("{method} from an intruder waited on a prompt instead of being rejected")
                });
            assert_error_name_is(&refused, REJECTED);
            assert!(
                mounted.banner.get_cloned().is_none(),
                "{method}: no banner raised"
            );
            assert!(
                mounted.pending.lock().await.is_none(),
                "{method}: nothing put in the reply slot"
            );
        }

        // bluetoothd, on the same agent, is asked as before.
        let call = tokio::spawn(async move {
            bluez
                .call::<_, _, ()>("RequestConfirmation", &(device(), 123_456_u32))
                .await
        });
        let prompt = shown(&mounted.banner).await;
        assert_eq!(prompt.passkey, Some(123_456));
        answer_in(&mounted.pending, prompt.id, AgentReply::Confirm).await;
        tokio::time::timeout(DBUS_REPLY_BUDGET, call)
            .await
            .expect("bluetoothd's request did not return in time")
            .expect("request task panicked")
            .expect("bluetoothd's confirmed request returned an error");
    }

    /// A `Cancel` from a peer that is not bluetoothd is rejected and cancels
    /// nothing: bluetoothd's waiting request keeps its banner and its reply
    /// slot, and the user's answer still reaches it.
    ///
    /// **Falsification:** drop the `admit` call from `cancel` and the
    /// intruder's cancel succeeds, rejecting bluetoothd's request, so the
    /// refusal assertion fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_from_a_peer_that_is_not_bluez_leaves_the_request_waiting() {
        let guard = ephemeral_bus().await;
        let mounted = mount(&guard).await;
        let intruder = proxy(&guard, &mounted, None).await;
        let bluez = proxy(&guard, &mounted, Some(BLUEZ_BUS_NAME)).await;

        let call = tokio::spawn(async move {
            bluez
                .call::<_, _, ()>("RequestAuthorization", &(device(),))
                .await
        });
        let prompt = shown(&mounted.banner).await;

        let cancel: Result<(), zbus::Error> =
            tokio::time::timeout(DBUS_REPLY_BUDGET, intruder.call("Cancel", &()))
                .await
                .expect("Cancel did not return in time");
        assert_error_name_is(&cancel, REJECTED);
        assert_eq!(
            mounted.banner.get_cloned().map(|p| p.id),
            Some(prompt.id),
            "a refused cancel leaves bluetoothd's banner up"
        );
        assert!(
            mounted.pending.lock().await.is_some(),
            "a refused cancel leaves bluetoothd's request waiting"
        );
        assert!(!call.is_finished(), "bluetoothd's request is still pending");

        answer_in(&mounted.pending, prompt.id, AgentReply::Confirm).await;
        tokio::time::timeout(DBUS_REPLY_BUDGET, call)
            .await
            .expect("bluetoothd's request did not return in time")
            .expect("request task panicked")
            .expect("bluetoothd's authorized request returned an error");
    }

    /// `AuthorizeService` over the wire: bluetoothd's request raises the
    /// prompt with the service's name and gets `Ok` on Allow and `Rejected`
    /// on Deny; anyone else's is rejected without a prompt.
    #[tokio::test(flavor = "multi_thread")]
    async fn authorize_service_asks_for_bluez_and_refuses_anyone_else() {
        const A2DP: &str = "0000110d-0000-1000-8000-00805f9b34fb";

        let guard = ephemeral_bus().await;
        let mounted = mount(&guard).await;
        let intruder = proxy(&guard, &mounted, None).await;
        let bluez = proxy(&guard, &mounted, Some(BLUEZ_BUS_NAME)).await;

        let refused: Result<(), zbus::Error> = tokio::time::timeout(
            DBUS_REPLY_BUDGET,
            intruder.call("AuthorizeService", &(device(), A2DP.to_string())),
        )
        .await
        .expect("a refused request is rejected at once");
        assert_error_name_is(&refused, REJECTED);
        assert!(mounted.banner.get_cloned().is_none(), "no banner raised");

        for (answer, allowed) in [(AgentReply::Confirm, true), (AgentReply::Reject, false)] {
            let call = {
                let bluez = bluez.clone();
                tokio::spawn(async move {
                    bluez
                        .call::<_, _, ()>("AuthorizeService", &(device(), A2DP.to_string()))
                        .await
                })
            };
            let prompt = shown(&mounted.banner).await;
            assert_eq!(prompt.kind, super::PromptKind::AuthorizeService);
            assert_eq!(prompt.service.as_deref(), Some("audio streaming (A2DP)"));
            answer_in(&mounted.pending, prompt.id, answer).await;
            let reply = tokio::time::timeout(DBUS_REPLY_BUDGET, call)
                .await
                .expect("AuthorizeService did not return in time")
                .expect("request task panicked");
            if allowed {
                reply.expect("Allow admits the service");
            } else {
                assert_error_name_is(&reply, REJECTED);
            }
            // The request takes its prompt down before it replies, so the
            // next round's prompt is its own.
            assert!(
                mounted.banner.get_cloned().is_none(),
                "the prompt comes down with the reply"
            );
        }
    }

    /// The methods that raise no prompt are checked too: bluetoothd gets its
    /// empty reply, anyone else `Rejected`.
    ///
    /// **Falsification:** drop the `admit` call from any of the three and
    /// the intruder's call succeeds.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_methods_without_a_prompt_answer_only_bluez() {
        async fn all_three(proxy: &zbus::Proxy<'_>) -> [Result<(), zbus::Error>; 3] {
            let release = proxy.call("Release", &()).await;
            let pin = proxy
                .call("DisplayPinCode", &(device(), "0000".to_string()))
                .await;
            let passkey = proxy
                .call("DisplayPasskey", &(device(), 123_456_u32, 0_u16))
                .await;
            [release, pin, passkey]
        }

        let guard = ephemeral_bus().await;
        let mounted = mount(&guard).await;
        let intruder = proxy(&guard, &mounted, None).await;
        let bluez = proxy(&guard, &mounted, Some(BLUEZ_BUS_NAME)).await;

        let from_bluez = tokio::time::timeout(DBUS_REPLY_BUDGET, all_three(&bluez))
            .await
            .expect("the calls did not return in time");
        for reply in &from_bluez {
            assert!(reply.is_ok(), "bluetoothd was refused: {reply:?}");
        }

        let from_intruder = tokio::time::timeout(DBUS_REPLY_BUDGET, all_three(&intruder))
            .await
            .expect("the calls did not return in time");
        for reply in &from_intruder {
            assert_error_name_is(reply, REJECTED);
        }
    }
}
