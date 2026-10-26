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

use super::shared_state;
use super::types::{AgentReply, PairPrompt, PromptKind};
use crate::retry;
use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
use hytte_bus::{BusKind, SignalItem};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex as AsyncMutex;
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
        let reply = await_reply(new_prompt(&device, None, PromptKind::EnterPinCode)).await;
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
        let reply = await_reply(new_prompt(&device, None, PromptKind::EnterPasskey)).await;
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
        let reply = await_reply(new_prompt(
            &device,
            Some(passkey),
            PromptKind::ConfirmPasskey,
        ))
        .await;
        match reply {
            AgentReply::Confirm => Ok(()),
            _ => Err(AgentError::Rejected("user rejected pairing".into())),
        }
    }

    async fn request_authorization(&self, device: OwnedObjectPath) -> Result<(), AgentError> {
        let reply = await_reply(new_prompt(&device, None, PromptKind::Authorize)).await;
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

    /// Reject whichever request is waiting. Its banner is taken down by that
    /// request itself, when it wakes ([`await_reply_in`]'s [`OwnBanner`]).
    /// This method does not take it down.
    ///
    /// It used to clear the banner here too, unconditionally. Between taking
    /// the sender and clearing the banner, the next request could already
    /// have raised its own banner, and that clear wiped it. The newer request
    /// would then wait out bluetoothd's minute with nothing on screen.
    async fn cancel(&self) {
        tracing::debug!("agent cancel — aborting pending prompt");
        if let Some(shared) = shared_state() {
            cancel_in(&shared.pending_response).await;
        }
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
        device_path,
        passkey,
        kind,
    }
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
/// Returns `AgentReply::Reject` if no prompt slot is available, the
/// channel is dropped, or another pair is already in flight — callers
/// pattern-match on the returned variant to shape their D-Bus return.
async fn await_reply(prompt: PairPrompt) -> AgentReply {
    let Some(shared) = shared_state() else {
        return AgentReply::Reject;
    };
    await_reply_in(&shared.pending_response, &shared.pair_prompt, prompt).await
}

/// [`await_reply`] over explicit slots, so a test can drive it without the
/// process-global shared state.
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
            passkey: Some(123_456),
            kind: PromptKind::ConfirmPasskey,
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
