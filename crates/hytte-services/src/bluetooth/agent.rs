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
use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use hytte_bus::BusKind;
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
        if let Some(tx) = take_pending().await {
            let _ = tx.send(AgentReply::Reject);
        }
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

fn pending_response_arc()
-> Option<std::sync::Arc<tokio::sync::Mutex<Option<oneshot::Sender<AgentReply>>>>> {
    shared_state().map(|s| s.pending_response.clone())
}

async fn take_pending() -> Option<oneshot::Sender<AgentReply>> {
    let arc = pending_response_arc()?;
    arc.lock().await.take()
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
async fn await_reply_in(
    pending: &AsyncMutex<Option<oneshot::Sender<AgentReply>>>,
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
        if guard.as_ref().is_some_and(|waiting| !waiting.is_closed()) {
            // Another pairing already pending — refuse cleanly so BlueZ
            // doesn't pile up coincident prompts.
            return AgentReply::Reject;
        }
        *guard = Some(tx);
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
    pending: &'a AsyncMutex<Option<oneshot::Sender<AgentReply>>>,
    id: u64,
    rx: oneshot::Receiver<AgentReply>,
}

impl Drop for OwnBanner<'_> {
    fn drop(&mut self) {
        clear_banner_if(&self.banner, self.id);
        self.rx.close();
        if let Ok(mut slot) = self.pending.try_lock()
            && slot.as_ref().is_some_and(oneshot::Sender::is_closed)
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
    let bluez_gone_sub = hytte_bus::signals(BusKind::System, "org.freedesktop.DBus")
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
    if let Err(e) = hytte_bus::call(BusKind::System, "org.bluez")
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
    if let Err(e) = hytte_bus::call(BusKind::System, "org.bluez")
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

#[cfg(test)]
mod tests {
    use super::{AgentReply, PairPrompt, PromptKind, await_reply_in, clear_banner_if};
    use futures_signals::signal::{Mutable, Signal};
    use futures_util::FutureExt;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    use tokio::sync::oneshot;

    type Pending = Arc<AsyncMutex<Option<oneshot::Sender<AgentReply>>>>;

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

    /// Answer whichever request is waiting, the way `send_reply` and
    /// `PairAgent::cancel` do: take its sender and send on it.
    async fn answer(pending: &Pending, reply: AgentReply) {
        pending
            .lock()
            .await
            .take()
            .expect("a request is waiting")
            .send(reply)
            .expect("the waiting request is still listening");
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
        answer(&pending, AgentReply::Reject).await;

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
        answer(&pending, AgentReply::Confirm).await;
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

        answer(&pending, AgentReply::Confirm).await;
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
        assert!(
            pending.lock().await.is_none(),
            "dropping A frees the slot"
        );

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(
            b.as_mut().now_or_never().is_none(),
            "B waits on the user instead of being refused"
        );
        assert_eq!(shown(&banner), Some(2));
        answer(&pending, AgentReply::Confirm).await;
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
                held.as_ref().is_some_and(oneshot::Sender::is_closed),
                "A's sender is still in the slot, and closed"
            );
        }

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(
            b.as_mut().now_or_never().is_none(),
            "B waits on the user instead of being refused"
        );
        assert_eq!(shown(&banner), Some(2));
        answer(&pending, AgentReply::Confirm).await;
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

        answer(&pending, AgentReply::Confirm).await;
        assert!(matches!(a.await, AgentReply::Confirm), "A is still answerable");
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
        answer(&pending, AgentReply::Reject).await;

        let mut b = std::pin::pin!(await_reply_in(&pending, &banner, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none());

        drop(a);
        assert!(
            pending.lock().await.is_some(),
            "B's sender must stay in the slot"
        );
        assert_eq!(shown(&banner), Some(2), "B's banner stays up");
        answer(&pending, AgentReply::Confirm).await;
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
