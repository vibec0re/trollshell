//! iwd `net.connman.iwd.Agent` D-Bus interface implementation.
//!
//! iwd calls the agent back on our system connection's unique name, which any
//! peer the bus policy lets through can address too. So every method first
//! checks that its caller is iwd, the current owner of `net.connman.iwd`
//! ([`IwdAgent::admit`], #1605), the way the `NetworkManager` secret agent and
//! the `BlueZ` pairing agent check theirs (#1603).
//!
//! The agent shares the prompt slot and the waiters map with the
//! `NetworkManager` secret agent, and stays exported for the rest of the
//! process once iwd has been the backend. Its `Cancel` therefore reaches only
//! the requests it raised itself, which it keeps in a set of its own
//! ([`IwdAgent::own`]). Before #1605 it drained the whole shared map, so a
//! `Cancel` also failed `NetworkManager`'s pending `GetSecrets`.

use futures_channel::oneshot;
use futures_signals::signal::Mutable;
use futures_util::future::BoxFuture;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex as AsyncMutex;
use zbus::names::ErrorName;

use super::types::PromptRequest;
use super::{NEXT_ID, WaitersMap};
use crate::wifi::client::read_network_metadata;
use crate::wifi_backend::IWD_BUS_NAME;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Placeholder [`PromptRequest::secret_keys`] entry for the iwd backend. iwd's
/// `RequestPassphrase` takes a bare string with no key name, so this exists
/// only to keep the prompt's "one value per key" contract well-formed; the
/// overlay never renders it (a one-key prompt shows a single unlabelled entry).
const IWD_PASSPHRASE_KEY: &str = "passphrase";

/// The error iwd's agent API names for its agent methods:
/// `net.connman.iwd.Agent.Error.Canceled`. iwd 3.12's `doc/agent-api.txt`
/// lists it as the one "Possible Error" of `RequestPassphrase`,
/// `RequestPrivateKeyPassphrase`, `RequestUserNameAndPassword` and
/// `RequestUserPassword`.
const CANCELED: &str = "net.connman.iwd.Agent.Error.Canceled";

// ── Agent error ───────────────────────────────────────────────────────────────

/// What the agent answers with in place of a result.
///
/// A hand-written [`zbus::DBusError`] rather than the derive, because the two
/// variants need names under two different prefixes and the derive takes one.
#[derive(Debug)]
enum IwdAgentError {
    /// The call did not come from iwd: its sender is not the current owner of
    /// `net.connman.iwd`. Sent as [`CANCELED`].
    Canceled(String),
    /// Every other error, as it was before #1605: the user dismissed the
    /// prompt, or an EAP request this agent does not answer
    /// (`org.freedesktop.DBus.Error.Failed` either way). iwd 3.12 reads any
    /// error reply the same way (`passphrase_reply` and
    /// `user_name_passwd_reply` in `src/agent.c` give `AGENT_RESULT_FAILED` for
    /// all of them), so these names were left alone.
    Fdo(zbus::fdo::Error),
}

impl zbus::DBusError for IwdAgentError {
    fn create_reply(
        &self,
        call: &zbus::message::Header<'_>,
    ) -> zbus::Result<zbus::message::Message> {
        match self {
            Self::Canceled(description) => {
                zbus::message::Message::error(call, self.name())?.build(description)
            }
            Self::Fdo(error) => error.create_reply(call),
        }
    }

    fn name(&self) -> ErrorName<'_> {
        match self {
            Self::Canceled(_) => ErrorName::from_static_str_unchecked(CANCELED),
            Self::Fdo(error) => error.name(),
        }
    }

    fn description(&self) -> Option<&str> {
        match self {
            Self::Canceled(description) => Some(description),
            Self::Fdo(error) => error.description(),
        }
    }
}

// ── Agent ─────────────────────────────────────────────────────────────────────

/// The ids of the requests one iwd agent raised that may still be waiting.
///
/// An id is inserted (by [`await_passphrase`]) and drained (by
/// [`cancel_waiting`]) **while holding the waiters lock**, so a `Cancel`
/// never sees a waiter without its id. The request's own removal of its id
/// once it has ended takes no waiters lock: by then its waiter is already
/// out of the map, taken by a submit, a dismissal or a cancel. So a `Cancel`
/// can find an id whose waiter is gone, and [`cancel_waiting`] skips it. Ids
/// come from one counter shared with the `NetworkManager` agent and are
/// never reused, so such an id can never name another request.
type OwnRequests = Arc<AsyncMutex<HashSet<u64>>>;

/// How the agent learns a network's name and security type for its prompt,
/// from iwd's object path for it.
///
/// Production reads iwd's `net.connman.iwd.Network` over the pooled system
/// bus ([`read_iwd_network_info`]). A test mounts the agent on its own bus,
/// where that read would go to the host's real system bus instead, so it
/// swaps this out.
type NetworkInfo = fn(String) -> BoxFuture<'static, (String, String)>;

#[derive(Clone)]
pub(super) struct IwdAgent {
    /// The prompt slot, shared with the `NetworkManager` agent: one prompt is
    /// on screen at a time, whichever agent raised it.
    prompts: Mutable<Option<PromptRequest>>,
    /// Every pending request's waiter, shared with the `NetworkManager` agent:
    /// the map `submit_prompt`/`cancel_prompt` resolve by id.
    waiters: WaitersMap,
    /// Which of `waiters` are this agent's. Its `Cancel` reaches these and
    /// nothing else.
    own: OwnRequests,
    /// Admits a call only from iwd: the current owner of `net.connman.iwd`,
    /// looked up on the call's own connection when it arrives.
    caller: hytte_bus::CallerCheck,
    /// Where `RequestPassphrase` reads the network's name and type from.
    network_info: NetworkInfo,
}

impl IwdAgent {
    /// An agent surfacing its prompts on `prompts` and parking their answers
    /// in `waiters` (the service-wide handles `submit_prompt`/`cancel_prompt`
    /// resolve), answering only iwd.
    pub(super) fn new(prompts: Mutable<Option<PromptRequest>>, waiters: WaitersMap) -> Self {
        Self {
            prompts,
            waiters,
            own: Arc::default(),
            caller: hytte_bus::caller_check(IWD_BUS_NAME),
            network_info: read_iwd_network_info,
        }
    }

    /// Answer a `method` call only if it came from iwd.
    ///
    /// The agent is exported on our system connection, so any peer the bus
    /// policy lets through can call it, not only iwd. Unchecked, such a peer
    /// could raise a passphrase prompt that reads like iwd's and collect what
    /// the user types, or cancel iwd's real request. Every method checks
    /// first and touches nothing (no network read, no prompt, no waiter, no
    /// cancel) when the check refuses. A refusal is iwd's own [`CANCELED`].
    ///
    /// The owner is looked up per call ([`hytte_bus::CallerCheck`]) on `conn`,
    /// the connection the call arrived on: iwd that restarts comes back under
    /// a new unique name, and a cached owner would refuse it.
    async fn admit(
        &self,
        conn: &zbus::Connection,
        header: &zbus::message::Header<'_>,
        method: &str,
    ) -> Result<(), IwdAgentError> {
        self.caller.check(conn, header).await.map_err(|refused| {
            tracing::warn!(
                method,
                %refused,
                "iwd Agent: refused a call that did not come from iwd",
            );
            IwdAgentError::Canceled(format!(
                "only {} may call this agent: {refused}",
                self.caller.name()
            ))
        })
    }
}

/// [`NetworkInfo`] as production reads it: iwd's `Network` properties over
/// the pooled system bus.
fn read_iwd_network_info(path: String) -> BoxFuture<'static, (String, String)> {
    Box::pin(async move { read_network_metadata(&path).await })
}

// `unused_variables` silences the arguments the EAP stubs deliberately ignore:
// they refuse the request without reading it.
#[allow(unused_variables)]
#[zbus::interface(name = "net.connman.iwd.Agent")]
impl IwdAgent {
    /// iwd has unregistered the agent. Nothing to clean up; checked all the
    /// same, so this interface answers no one but iwd.
    async fn release(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), IwdAgentError> {
        self.admit(conn, &header, "Release").await?;
        tracing::info!("iwd Agent::Release");
        Ok(())
    }

    /// iwd needs the passphrase for `network`: raise a prompt and answer with
    /// what the user types.
    ///
    /// A call from anyone but iwd is refused before the network is read or a
    /// prompt raised ([`IwdAgent::admit`]).
    async fn request_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String, IwdAgentError> {
        self.admit(conn, &header, "RequestPassphrase").await?;

        let path = network.as_str().to_string();
        let (ssid, security) = (self.network_info)(path.clone()).await;

        let prompt = PromptRequest {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            network_path: path,
            ssid,
            security,
            kind: super::types::PromptKind::WifiPassphrase,
            // iwd's RequestPassphrase carries no retry/failure signal (unlike
            // NM's REQUEST_NEW flag) — see the field doc on PromptRequest.
            prior_failure: false,
            // iwd has no secret-key vocabulary: RequestPassphrase returns one
            // bare string. One key keeps the overlay on its single-entry,
            // unlabelled path, so this placeholder is never rendered.
            secret_keys: vec![IWD_PASSPHRASE_KEY.to_string()],
        };
        await_passphrase(&self.prompts, &self.waiters, &self.own, prompt)
            .await
            .map_err(IwdAgentError::Fdo)
    }

    /// Not supported (EAP). Checked first, so only iwd hears why.
    async fn request_private_key_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String, IwdAgentError> {
        self.admit(conn, &header, "RequestPrivateKeyPassphrase")
            .await?;
        Err(eap_unsupported())
    }

    /// Not supported (EAP). Checked first, so only iwd hears why.
    async fn request_user_name_and_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(String, String), IwdAgentError> {
        self.admit(conn, &header, "RequestUserNameAndPassword")
            .await?;
        Err(eap_unsupported())
    }

    /// Not supported (EAP). Checked first, so only iwd hears why.
    async fn request_user_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        username: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<String, IwdAgentError> {
        self.admit(conn, &header, "RequestUserPassword").await?;
        Err(eap_unsupported())
    }

    /// iwd gave up on its request before we answered (it timed out, the
    /// network went out of range, iwd is shutting down). Cancel every request
    /// **this agent** raised; the `NetworkManager` agent's stay pending.
    ///
    /// Only iwd may cancel. Anyone else is refused and every request stays
    /// pending ([`IwdAgent::admit`]).
    async fn cancel(
        &self,
        reason: String,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> Result<(), IwdAgentError> {
        self.admit(conn, &header, "Cancel").await?;
        tracing::info!(%reason, "iwd Agent::Cancel");
        cancel_waiting(&self.prompts, &self.waiters, &self.own).await;
        Ok(())
    }
}

/// The answer to an EAP request, which this agent does not support.
fn eap_unsupported() -> IwdAgentError {
    IwdAgentError::Fdo(zbus::fdo::Error::Failed(
        "hytte wifi agent does not support EAP".into(),
    ))
}

/// Raise `prompt` and wait for the user's answer to it.
///
/// [`IwdAgent::request_passphrase`] after the network's metadata is read, over
/// explicit slots so a test can drive it without a bus. The `bluetooth` agent's
/// `await_reply_in` is the same shape, for the same race (#1491).
///
/// The request's id goes into `own` with its waiter, so the agent's `Cancel`
/// can reach it, and leaves `own` when the request ends.
///
/// The request takes down its own prompt when it ends, and only if the prompt
/// slot still shows it ([`OwnPrompt`]). Its task can wake late: after a
/// `Cancel` or a submit has resolved it, iwd can send the next
/// `RequestPassphrase` and that request can raise its prompt before this task
/// is scheduled again. Until #1551 this cleared the slot unconditionally once
/// `rx` resolved, so that late wake took down the newer prompt, and with it
/// the passphrase the user was halfway through typing. iwd then waited out its
/// two minutes on a request with nothing on screen.
async fn await_passphrase(
    prompts: &Mutable<Option<PromptRequest>>,
    waiters: &WaitersMap,
    own: &AsyncMutex<HashSet<u64>>,
    prompt: PromptRequest,
) -> zbus::fdo::Result<String> {
    let id = prompt.id;
    let (tx, rx) = oneshot::channel::<Result<Vec<String>, String>>();
    {
        // The id goes into `own` under the waiters lock, so `cancel_waiting`
        // (which takes the same two locks in the same order) sees both or
        // neither.
        let mut waiters = waiters.lock().await;
        waiters.insert(id, tx);
        own.lock().await.insert(id);
    }

    // From here on the prompt is this request's to take down, on every exit,
    // including this future being dropped mid-await.
    let _shown = OwnPrompt {
        prompts: prompts.clone(),
        id,
    };
    prompts.set(Some(prompt));

    let answer = rx.await;
    // Done, whichever way it ended: no `Cancel` can reach it any more.
    own.lock().await.remove(&id);

    // Exactly one key was requested, so exactly one value comes back. An
    // empty vector can only mean a malformed submit; treat it as a cancel
    // rather than handing iwd an empty passphrase it would just reject.
    match answer {
        Ok(Ok(secrets)) if !secrets.is_empty() => {
            Ok(secrets.into_iter().next().unwrap_or_default())
        }
        _ => Err(zbus::fdo::Error::Failed("agent cancelled".into())),
    }
}

/// Resolve every request **this agent** raised that is still waiting with a
/// cancel, and take down the prompt of each one that is still on screen.
///
/// iwd's `Cancel` names no request: it has one outstanding agent request at a
/// time, so every request in `own` is cancelled. Nothing else in `waiters` is
/// touched. The map is shared with the `NetworkManager` agent, and until #1605
/// this drained all of it, so iwd's `Cancel` (or anyone's, before the caller
/// check) also failed `NetworkManager`'s pending `GetSecrets` with
/// `UserCanceled`.
///
/// The prompt slot is cleared only for the ids cancelled here. It used to be
/// cleared unconditionally (#1551), which also took down a prompt this
/// `Cancel` did not cancel. Each woken request clears its own prompt too
/// ([`OwnPrompt`]). Clearing here as well takes the prompt down the moment iwd
/// cancels it, not when its task is next scheduled. Both clears are no-ops
/// unless that prompt is on screen.
async fn cancel_waiting(
    prompts: &Mutable<Option<PromptRequest>>,
    waiters: &WaitersMap,
    own: &AsyncMutex<HashSet<u64>>,
) {
    let cancelled: Vec<u64> = {
        let mut waiters = waiters.lock().await;
        let ids: Vec<u64> = own.lock().await.drain().collect();
        for id in &ids {
            // Absent when the user's answer or dismissal won the race: that
            // request is completing anyway (see `OwnRequests`).
            if let Some(tx) = waiters.remove(id) {
                let _ = tx.send(Err("cancelled".into()));
            }
        }
        ids
    };
    for id in cancelled {
        clear_prompt_if_shown(prompts, id);
    }
}

/// One request's claim on the prompt slot. On drop it clears the slot only if
/// the slot still shows request `id`, so a newer request's prompt stays up.
struct OwnPrompt {
    prompts: Mutable<Option<PromptRequest>>,
    id: u64,
}

impl Drop for OwnPrompt {
    fn drop(&mut self) {
        clear_prompt_if_shown(&self.prompts, self.id);
    }
}

/// Clear `prompts` if it shows request `id`; otherwise leave it alone.
///
/// One `lock_mut`, so a newer prompt cannot land between the check and the
/// clear. A `MutableLockMut` notifies subscribers only when it is written
/// through, so the not-ours branch causes no overlay rebuild, which would
/// throw away a half-typed passphrase. The `NetworkManager` agent's
/// `clear_prompt_if_shown` is the same rule for the same slot.
fn clear_prompt_if_shown(prompts: &Mutable<Option<PromptRequest>>, id: u64) {
    let mut slot = prompts.lock_mut();
    if slot.as_ref().is_some_and(|shown| shown.id == id) {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CANCELED, IwdAgentError, OwnRequests, PromptRequest, await_passphrase, cancel_waiting,
        clear_prompt_if_shown,
    };
    use crate::wifi::WaitersMap;
    use crate::wifi::types::PromptKind;
    use futures_signals::signal::{Mutable, Signal};
    use futures_util::FutureExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex as AsyncMutex;

    /// How long a test waits on a spawned request that a `Cancel` should have
    /// resolved already. A bound, not a latency claim: it turns a regression
    /// that leaves the request waiting into a failure instead of a hang, which
    /// `checks.workspace-tests` would only report as a job timeout.
    const RESOLVED_BUDGET: Duration = Duration::from_secs(5);

    fn prompt(id: u64) -> PromptRequest {
        PromptRequest {
            id,
            network_path: "/net/connman/iwd/0/4/6e6574_psk".to_owned(),
            ssid: "net".to_owned(),
            security: "psk".to_owned(),
            kind: PromptKind::WifiPassphrase,
            prior_failure: false,
            secret_keys: vec!["passphrase".to_owned()],
        }
    }

    fn waiters() -> WaitersMap {
        Arc::new(AsyncMutex::new(HashMap::new()))
    }

    /// The id of the prompt on screen, if any.
    fn shown(prompts: &Mutable<Option<PromptRequest>>) -> Option<u64> {
        prompts.lock_ref().as_ref().map(|p| p.id)
    }

    /// Answer request `id` the way `wifi::submit_prompt` does: take its waiter
    /// out of the map and send on it.
    async fn submit(waiters: &WaitersMap, id: u64, passphrase: &str) {
        waiters
            .lock()
            .await
            .remove(&id)
            .expect("request is waiting")
            .send(Ok(vec![passphrase.to_owned()]))
            .expect("the waiting request is still listening");
    }

    /// Cancel(A), then Request(B), then A's task finally runs. A's late wake
    /// must leave B's prompt up.
    ///
    /// The interleaving is forced, not hoped for: on a current-thread runtime
    /// A cannot run between the cancel that wakes it and B being polled by
    /// hand, so B has raised its prompt before A clears anything.
    ///
    /// **Falsification:** clear the prompt unconditionally once `rx` resolves
    /// (the pre-#1551 `self.prompts.set(None)`) and the "A's late wake"
    /// assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_wake_of_a_cancelled_request_leaves_the_newer_prompt_up() {
        let waiters = waiters();
        let own = OwnRequests::default();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let a = tokio::spawn({
            let waiters = waiters.clone();
            let own = own.clone();
            let prompts = prompts.clone();
            async move { await_passphrase(&prompts, &waiters, &own, prompt(1)).await }
        });
        while shown(&prompts) != Some(1) {
            tokio::task::yield_now().await;
        }

        // Cancel(A). A is now runnable but has not run. Its prompt goes at
        // once: it is one this Cancel cancelled.
        cancel_waiting(&prompts, &waiters, &own).await;
        assert_eq!(shown(&prompts), None, "the cancelled prompt comes down");

        // Request(B) lands first: one poll by hand installs it and raises its
        // prompt, with no await point where A could be scheduled.
        let mut b = std::pin::pin!(await_passphrase(&prompts, &waiters, &own, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none(), "B waits on the user");
        assert_eq!(shown(&prompts), Some(2), "B's prompt is up");

        // A finally runs. Bounded: if the cancel never resolved A's waiter,
        // A would wait forever.
        let a_reply = tokio::time::timeout(RESOLVED_BUDGET, a)
            .await
            .expect("the cancel resolved request A")
            .expect("request A panicked");
        assert!(a_reply.is_err(), "A was cancelled");
        assert_eq!(
            shown(&prompts),
            Some(2),
            "A's late wake must not take down B's prompt"
        );

        // B is answered: its own prompt goes.
        submit(&waiters, 2, "hunter22").await;
        assert_eq!(b.await.expect("B was answered"), "hunter22");
        assert_eq!(shown(&prompts), None, "B takes its own prompt down");
    }

    /// The same late wake on the answered path: A's passphrase is submitted,
    /// B raises its prompt before A's task runs, and A's wake leaves B's up.
    ///
    /// **Falsification:** as above. The pre-#1551 code cleared the prompt on
    /// this arm too.
    #[tokio::test(flavor = "current_thread")]
    async fn a_late_wake_of_an_answered_request_leaves_the_newer_prompt_up() {
        let waiters = waiters();
        let own = OwnRequests::default();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let a = tokio::spawn({
            let waiters = waiters.clone();
            let own = own.clone();
            let prompts = prompts.clone();
            async move { await_passphrase(&prompts, &waiters, &own, prompt(1)).await }
        });
        while shown(&prompts) != Some(1) {
            tokio::task::yield_now().await;
        }

        submit(&waiters, 1, "first").await;

        let mut b = std::pin::pin!(await_passphrase(&prompts, &waiters, &own, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none(), "B waits on the user");
        assert_eq!(shown(&prompts), Some(2));

        assert_eq!(a.await.expect("request A panicked").as_deref(), Ok("first"));
        assert_eq!(
            shown(&prompts),
            Some(2),
            "A's late wake must not take down B's prompt"
        );
    }

    /// A request whose future is dropped mid-wait still takes its prompt
    /// down, so no prompt is left on screen with nothing behind it.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_request_takes_its_own_prompt_down() {
        let waiters = waiters();
        let own = OwnRequests::default();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let mut a = Box::pin(await_passphrase(&prompts, &waiters, &own, prompt(3)));
        assert!(a.as_mut().now_or_never().is_none());
        assert_eq!(shown(&prompts), Some(3));

        drop(a);
        assert_eq!(shown(&prompts), None);
    }

    /// `Cancel` takes down only the prompts of the requests it cancelled. A
    /// prompt with no iwd request behind it (the shared slot can hold the
    /// `NetworkManager` agent's) stays up.
    ///
    /// **Falsification:** make `cancel_waiting` clear the slot unconditionally
    /// (the pre-#1551 `self.prompts.set(None)`) and the first assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_leaves_a_prompt_it_did_not_cancel() {
        let waiters = waiters();
        let own = OwnRequests::default();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(Some(prompt(9)));
        let (tx, rx) = futures_channel::oneshot::channel();
        waiters.lock().await.insert(1, tx);
        own.lock().await.insert(1);

        cancel_waiting(&prompts, &waiters, &own).await;
        assert_eq!(shown(&prompts), Some(9), "prompt 9 was not cancelled");
        // One poll decides it: the cancel has either answered the waiter by
        // now or never will.
        assert_eq!(
            rx.now_or_never()
                .expect("the cancel resolved the waiter")
                .expect("the waiter was answered"),
            Err("cancelled".to_owned()),
            "the waiting request is cancelled"
        );
        assert!(waiters.lock().await.is_empty(), "its waiter is gone");
        assert!(own.lock().await.is_empty(), "and so is its id");
    }

    /// `Cancel` reaches only the requests this agent raised. A waiter in the
    /// shared map that is not in `own` (a `NetworkManager` `GetSecrets`)
    /// keeps waiting, and its prompt stays on screen.
    ///
    /// **Falsification:** make `cancel_waiting` drain the whole waiters map
    /// (the pre-#1605 `waiters.lock().await.drain()`) and the NM request is
    /// answered with a cancel, so the "still waiting" assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_reaches_only_this_agent_s_requests() {
        let waiters = waiters();
        let own = OwnRequests::default();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        // iwd's request, raised through the agent.
        let mut iwd = Box::pin(await_passphrase(&prompts, &waiters, &own, prompt(1)));
        assert!(iwd.as_mut().now_or_never().is_none());
        // NM's request, waiting in the same map and now on screen.
        let (nm_tx, mut nm_rx) = futures_channel::oneshot::channel();
        waiters.lock().await.insert(2, nm_tx);
        prompts.set(Some(prompt(2)));

        cancel_waiting(&prompts, &waiters, &own).await;

        // One poll decides it, on this current-thread runtime: the cancel has
        // either resolved iwd's request by now or never will.
        assert!(
            iwd.now_or_never()
                .expect("the cancel resolved iwd's request")
                .is_err(),
            "iwd's own request is cancelled"
        );
        assert!(
            waiters.lock().await.contains_key(&2),
            "NM's request must still be waiting"
        );
        assert_eq!(
            nm_rx.try_recv().expect("NM's waiter is still connected"),
            None,
            "NM's request must not be answered"
        );
        assert_eq!(shown(&prompts), Some(2), "NM's prompt stays on screen");
    }

    /// The check itself. Clearing for an id that is not on screen leaves the
    /// slot as it is and notifies no subscriber: a spurious notification would
    /// rebuild the overlay and lose a half-typed passphrase.
    #[test]
    fn clearing_another_requests_id_leaves_the_prompt_alone() {
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(Some(prompt(2)));
        let mut changes = Box::pin(prompts.signal_ref(|p| p.as_ref().map(|p| p.id)));
        let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(
            changes.as_mut().poll_change(&mut cx).is_ready(),
            "a fresh subscriber sees the current value"
        );

        clear_prompt_if_shown(&prompts, 1);
        assert_eq!(shown(&prompts), Some(2));
        assert!(
            changes.as_mut().poll_change(&mut cx).is_pending(),
            "another request's id must not notify subscribers"
        );

        clear_prompt_if_shown(&prompts, 2);
        assert_eq!(shown(&prompts), None);
    }

    /// A refusal carries the name iwd's agent API documents, written out as a
    /// literal rather than built from the constant, so a change to the
    /// constant cannot carry this expectation along with it. Source: iwd
    /// 3.12, `doc/agent-api.txt`, the "Possible Errors" of every agent method
    /// that returns a value. The other replies keep their pre-#1605 name.
    ///
    /// **Falsification:** misspell [`CANCELED`] (say `….Error.Cancelled`) and
    /// the first assertion fails.
    #[test]
    fn a_refusal_carries_the_error_name_iwd_documents() {
        use zbus::DBusError as _;
        assert_eq!(
            IwdAgentError::Canceled(String::new()).name().as_str(),
            "net.connman.iwd.Agent.Error.Canceled"
        );
        assert_eq!(CANCELED, "net.connman.iwd.Agent.Error.Canceled");
        assert_eq!(
            IwdAgentError::Fdo(zbus::fdo::Error::Failed(String::new()))
                .name()
                .as_str(),
            "org.freedesktop.DBus.Error.Failed"
        );
    }
}

// ── System tests: the caller check and Cancel's reach over a real bus ────────
//
// What only a broker can show: that each `net.connman.iwd.Agent` method reads
// its sender off the call and answers iwd (whoever owns `net.connman.iwd`)
// and refuses everyone else before raising a prompt or touching a waiter, and
// that iwd's own `Cancel` cannot reach a `NetworkManager` `GetSecrets` waiting
// beside it. Both agents are mounted on one connection to an ephemeral
// `dbus-daemon`, sharing one prompt slot and one waiters map, the way
// production leaves them once iwd has been the backend. One peer owns
// `net.connman.iwd`, one owns `org.freedesktop.NetworkManager`, and one owns
// nothing. The `BusGuard` / `ephemeral_bus` harness is `wifi::nm_agent`'s,
// copied per module as this crate does. Run with:
//   cargo test -p hytte-services --features system-tests --lib wifi::agent
#[cfg(all(test, feature = "system-tests"))]
mod system_tests {
    use super::{IwdAgent, NetworkInfo, OwnRequests, PromptRequest};
    use crate::wifi::nm_agent::NmAgent;
    use crate::wifi::types::PromptKind;
    use crate::wifi::{AGENT_PATH, WaitersMap};
    use crate::wifi_backend::{IWD_BUS_NAME, NM_BUS_NAME};
    use crate::wifi_nm::NM_AGENT_PATH;
    use futures_signals::signal::Mutable;
    use futures_util::future::BoxFuture;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::process::{Child, Command};
    use tokio::sync::Mutex as AsyncMutex;
    use zbus::Connection;
    use zbus::connection::Builder;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    /// Upper bound on any one round trip. A liveness guard, not a latency
    /// assertion: the same number and the same reasoning as
    /// `wifi::nm_agent`'s budget.
    const DBUS_REPLY_BUDGET: Duration = Duration::from_secs(30);

    const IWD_IFACE: &str = "net.connman.iwd.Agent";
    const NM_IFACE: &str = "org.freedesktop.NetworkManager.SecretAgent";
    const NETWORK: &str = "/net/connman/iwd/0/4/46524954a5426f78_psk";
    const CANCELED: &str = "net.connman.iwd.Agent.Error.Canceled";
    /// What a cancelled `RequestPassphrase` has always answered (unchanged by
    /// #1605).
    const FAILED: &str = "org.freedesktop.DBus.Error.Failed";
    /// What the stand-in for iwd's `Network` properties names every network.
    const SSID: &str = "FRITZ!Box";

    /// NM's `a{sa{sv}}` connection settings: setting name → (key → value).
    type ConnectionDict = HashMap<String, HashMap<String, OwnedValue>>;

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

    /// Stands in for iwd's `Network` properties. The real read goes to the
    /// pooled **system** bus, i.e. the host's, which a test must not touch:
    /// on a machine with iwd installed it could even start it.
    fn named_network(_path: String) -> BoxFuture<'static, (String, String)> {
        Box::pin(async { (SSID.to_owned(), "psk".to_owned()) })
    }

    /// How many times [`counted_network`] has been asked. Read only by the
    /// one test that mounts with it, so it is a static of its own: the system
    /// tests run in parallel in one binary, and a shared counter would count
    /// the other tests' reads too.
    static OUTSIDER_TEST_READS: AtomicUsize = AtomicUsize::new(0);

    /// [`named_network`], counting each read into [`OUTSIDER_TEST_READS`], so
    /// a test can tell whether a call got as far as reading the network.
    fn counted_network(path: String) -> BoxFuture<'static, (String, String)> {
        OUTSIDER_TEST_READS.fetch_add(1, AtomicOrdering::SeqCst);
        named_network(path)
    }

    /// Both agents, mounted on one connection, plus the slots they share and
    /// the iwd agent's own-request set.
    struct Mounted {
        /// Keeps the agents mounted; never read.
        _server: Connection,
        /// The agents' unique name, which every proxy addresses.
        dest: String,
        prompts: Mutable<Option<PromptRequest>>,
        waiters: WaitersMap,
        own: OwnRequests,
    }

    /// Mount an `IwdAgent` and an `NmAgent` over one prompt slot and one
    /// waiters map, each built as production builds it (`IwdAgent::new`,
    /// `NmAgent::new`): their caller checks ask whatever bus a call arrives
    /// on, which here is this test's. Only the iwd agent's network read is
    /// swapped, for `network_info` ([`named_network`] or [`counted_network`]).
    /// The agents are staged on the connection's builder, so its object
    /// server dispatches before anything can be read off the socket
    /// (`wifi::nm_agent`'s `mount_and_proxy` has the whole argument, #756).
    async fn mount(guard: &BusGuard, network_info: NetworkInfo) -> Mounted {
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);
        let waiters: WaitersMap = Arc::new(AsyncMutex::new(HashMap::new()));
        let iwd = IwdAgent {
            network_info,
            ..IwdAgent::new(prompts.clone(), waiters.clone())
        };
        let own = iwd.own.clone();
        let nm = NmAgent::new(prompts.clone(), waiters.clone());
        let server = Builder::address(guard.address.as_str())
            .expect("parse ephemeral bus address")
            .serve_at(AGENT_PATH, iwd)
            .expect("stage the iwd agent")
            .serve_at(NM_AGENT_PATH, nm)
            .expect("stage the NM agent")
            .build()
            .await
            .expect("mount the agents");
        let dest = server
            .unique_name()
            .expect("server has a unique name")
            .to_string();
        Mounted {
            _server: server,
            dest,
            prompts,
            waiters,
            own,
        }
    }

    /// A proxy to one of the mounted agents from a fresh connection that owns
    /// `name` (`net.connman.iwd` for iwd, NM's name for NM, `None` for
    /// anyone else).
    async fn proxy(
        guard: &BusGuard,
        mounted: &Mounted,
        name: Option<&str>,
        path: &'static str,
        iface: &'static str,
    ) -> zbus::Proxy<'static> {
        let mut builder =
            Builder::address(guard.address.as_str()).expect("parse ephemeral bus address");
        if let Some(name) = name {
            builder = builder.name(name).expect("a valid well-known name");
        }
        let client = builder.build().await.expect("connect a client to the bus");
        zbus::Proxy::new(&client, mounted.dest.clone(), path, iface)
            .await
            .expect("build the agent proxy")
    }

    /// A proxy to the iwd agent from a peer owning `name`.
    async fn iwd_agent(
        guard: &BusGuard,
        mounted: &Mounted,
        name: Option<&str>,
    ) -> zbus::Proxy<'static> {
        proxy(guard, mounted, name, AGENT_PATH, IWD_IFACE).await
    }

    fn network() -> OwnedObjectPath {
        OwnedObjectPath::try_from(NETWORK).expect("a valid object path")
    }

    /// The id of the prompt on screen, if any.
    fn shown_id(prompts: &Mutable<Option<PromptRequest>>) -> Option<u64> {
        prompts.lock_ref().as_ref().map(|p| p.id)
    }

    /// Wait until the slot shows a prompt other than `not`, and return it.
    async fn await_prompt_other_than(
        prompts: &Mutable<Option<PromptRequest>>,
        not: Option<u64>,
    ) -> PromptRequest {
        let end = tokio::time::Instant::now() + DBUS_REPLY_BUDGET;
        while tokio::time::Instant::now() < end {
            if let Some(prompt) = prompts.get_cloned()
                && Some(prompt.id) != not
            {
                return prompt;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("no new prompt was raised within {DBUS_REPLY_BUDGET:?}");
    }

    /// Answer request `id` the way `wifi::submit_prompt` does.
    async fn submit(waiters: &WaitersMap, id: u64, secret: &str) {
        waiters
            .lock()
            .await
            .remove(&id)
            .expect("the request is still waiting")
            .send(Ok(vec![secret.to_owned()]))
            .expect("the waiting request is still listening");
    }

    /// The `GetSecrets` arguments of an interactive WPA-PSK request, the way
    /// NM marshals one: the SSID as `ay`, the `802-11-wireless-security`
    /// setting (NM's `WIRELESS_SECURITY_SETTING`), the `psk` hint, and flag
    /// `0x1`, `ALLOW_INTERACTION`.
    fn nm_psk_request() -> (ConnectionDict, OwnedObjectPath, String, Vec<String>, u32) {
        let owned = |v: Value<'static>| v.try_to_owned().expect("to OwnedValue");
        let wireless = HashMap::from([("ssid".to_owned(), owned(Value::from(b"NM-net".to_vec())))]);
        let security = HashMap::from([(
            "key-mgmt".to_owned(),
            owned(Value::from("wpa-psk".to_owned())),
        )]);
        let connection = HashMap::from([
            ("802-11-wireless".to_owned(), wireless),
            ("802-11-wireless-security".to_owned(), security),
        ]);
        (
            connection,
            OwnedObjectPath::try_from("/org/freedesktop/NetworkManager/Settings/1")
                .expect("a valid object path"),
            "802-11-wireless-security".to_owned(),
            vec!["psk".to_owned()],
            0x1,
        )
    }

    /// Assert the reply to `method` is a `MethodError` named exactly
    /// `expected`.
    fn assert_error_name_is<T: std::fmt::Debug>(
        method: &str,
        result: &Result<T, zbus::Error>,
        expected: &str,
    ) {
        match result {
            Err(zbus::Error::MethodError(name, _detail, _reply)) => {
                assert_eq!(name.as_str(), expected, "{method}: wrong error");
            }
            Err(other) => panic!("{method}: expected MethodError {expected:?}, got {other:?}"),
            Ok(value) => panic!("{method}: expected MethodError {expected:?}, got Ok({value:?})"),
        }
    }

    /// Every agent method, called by a peer that is not iwd, is refused at
    /// once with iwd's `Canceled`: no network read, no prompt raised, no
    /// waiter parked. iwd's own request on the same agent is still asked and
    /// answered, and its `Release` still accepted.
    ///
    /// **Falsification:** drop the `admit` call from `request_passphrase`
    /// and the outsider's request raises a prompt and waits on it, so its
    /// "refused at once" expectation times out. Move it below the network
    /// read and the read count is 1, not 0. Drop it from any of the other
    /// five and that method answers with its old name (`Failed`, or
    /// success), not `Canceled`. Check `org.freedesktop.NetworkManager`
    /// instead of `net.connman.iwd` and iwd's own request is refused. Drop
    /// `await_passphrase`'s removal of its id and the answered request's id
    /// stays in `own`.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_from_a_peer_that_is_not_iwd_is_refused_and_raises_no_prompt() {
        let guard = ephemeral_bus().await;
        let mounted = mount(&guard, counted_network).await;
        let outsider = iwd_agent(&guard, &mounted, None).await;
        let iwd = iwd_agent(&guard, &mounted, Some(IWD_BUS_NAME)).await;

        for method in [
            "RequestPassphrase",
            "RequestPrivateKeyPassphrase",
            "RequestUserNameAndPassword",
            "RequestUserPassword",
            "Release",
            "Cancel",
        ] {
            let asked = async {
                match method {
                    "RequestUserPassword" => {
                        outsider
                            .call_method(method, &(network(), "user".to_owned()))
                            .await
                    }
                    "Release" => outsider.call_method(method, &()).await,
                    "Cancel" => {
                        outsider
                            .call_method(method, &("user-canceled".to_owned(),))
                            .await
                    }
                    _ => outsider.call_method(method, &(network(),)).await,
                }
            };
            let refused = tokio::time::timeout(DBUS_REPLY_BUDGET, asked)
                .await
                .unwrap_or_else(|_| {
                    panic!("{method} from an outsider waited on a prompt instead of being refused")
                });
            assert_error_name_is(method, &refused, CANCELED);
            assert!(
                mounted.prompts.get_cloned().is_none(),
                "{method}: no prompt raised"
            );
            assert!(
                mounted.waiters.lock().await.is_empty(),
                "{method}: no waiter parked"
            );
            assert!(
                mounted.own.lock().await.is_empty(),
                "{method}: no request of the agent's own"
            );
        }
        assert_eq!(
            OUTSIDER_TEST_READS.load(AtomicOrdering::SeqCst),
            0,
            "a refused call must not read the network it names"
        );

        // iwd, on the same agent, is asked as before.
        let call = tokio::spawn(async move {
            iwd.call::<_, _, String>("RequestPassphrase", &(network(),))
                .await
        });
        let prompt = await_prompt_other_than(&mounted.prompts, None).await;
        assert_eq!(prompt.ssid, SSID);
        assert_eq!(prompt.network_path, NETWORK);
        assert_eq!(prompt.kind, PromptKind::WifiPassphrase);
        submit(&mounted.waiters, prompt.id, "hunter22").await;
        let passphrase = tokio::time::timeout(DBUS_REPLY_BUDGET, call)
            .await
            .expect("iwd's request did not return in time")
            .expect("request task panicked")
            .expect("iwd's answered request returned an error");
        assert_eq!(passphrase, "hunter22");
        assert_eq!(
            OUTSIDER_TEST_READS.load(AtomicOrdering::SeqCst),
            1,
            "iwd's own request reads its network once"
        );
        // The reply goes out only after `await_passphrase` has returned, so
        // by now the answered request has taken its id back out.
        assert!(
            mounted.own.lock().await.is_empty(),
            "an answered request leaves the agent's own set"
        );

        let iwd = iwd_agent(&guard, &mounted, Some(IWD_BUS_NAME)).await;
        tokio::time::timeout(DBUS_REPLY_BUDGET, iwd.call::<_, _, ()>("Release", &()))
            .await
            .expect("Release did not return in time")
            .expect("iwd's Release was refused");
    }

    /// A `Cancel` from a peer that is not iwd is refused and cancels nothing:
    /// iwd's waiting request keeps its waiter and its prompt, and the user's
    /// answer still reaches iwd.
    ///
    /// **Falsification:** drop the `admit` call from `cancel` and the
    /// outsider's cancel succeeds, failing iwd's request, so the refusal
    /// assertion fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_from_a_peer_that_is_not_iwd_leaves_the_request_pending() {
        let guard = ephemeral_bus().await;
        let mounted = mount(&guard, named_network).await;
        let outsider = iwd_agent(&guard, &mounted, None).await;
        let iwd = iwd_agent(&guard, &mounted, Some(IWD_BUS_NAME)).await;

        let call = tokio::spawn(async move {
            iwd.call::<_, _, String>("RequestPassphrase", &(network(),))
                .await
        });
        let prompt = await_prompt_other_than(&mounted.prompts, None).await;

        let cancel: Result<(), zbus::Error> = tokio::time::timeout(
            DBUS_REPLY_BUDGET,
            outsider.call("Cancel", &("user-canceled".to_owned(),)),
        )
        .await
        .expect("Cancel did not return in time");
        assert_error_name_is("Cancel", &cancel, CANCELED);

        assert!(
            mounted.waiters.lock().await.contains_key(&prompt.id),
            "a refused cancel must leave iwd's request waiting"
        );
        assert_eq!(
            shown_id(&mounted.prompts),
            Some(prompt.id),
            "a refused cancel must leave iwd's prompt on screen"
        );
        assert!(!call.is_finished(), "iwd's request is still pending");

        submit(&mounted.waiters, prompt.id, "hunter22").await;
        let passphrase = tokio::time::timeout(DBUS_REPLY_BUDGET, call)
            .await
            .expect("iwd's request did not return in time")
            .expect("request task panicked")
            .expect("iwd's answered request returned an error");
        assert_eq!(passphrase, "hunter22");
    }

    /// iwd's own `Cancel` cancels iwd's request and nothing else: NM's
    /// `GetSecrets`, waiting beside it in the shared map, keeps its waiter
    /// and its prompt, and still completes with the user's answer.
    ///
    /// **Falsification:** make `cancel_waiting` drain the whole waiters map
    /// (the pre-#1605 behaviour) and NM's request is failed with
    /// `UserCanceled`, so the "NM's request is still waiting" assertion
    /// fails.
    #[tokio::test(flavor = "multi_thread")]
    async fn iwd_s_cancel_leaves_network_manager_s_request_pending() {
        let guard = ephemeral_bus().await;
        let mounted = mount(&guard, named_network).await;
        let iwd = iwd_agent(&guard, &mounted, Some(IWD_BUS_NAME)).await;
        let nm = proxy(&guard, &mounted, Some(NM_BUS_NAME), NM_AGENT_PATH, NM_IFACE).await;

        // iwd asks first; then NM asks, and its prompt replaces iwd's on
        // screen (the slot holds one prompt, as it always has).
        let iwd_call = tokio::spawn({
            let iwd = iwd.clone();
            async move {
                iwd.call::<_, _, String>("RequestPassphrase", &(network(),))
                    .await
            }
        });
        let iwd_prompt = await_prompt_other_than(&mounted.prompts, None).await;
        let nm_call = tokio::spawn(async move {
            nm.call::<_, _, ConnectionDict>("GetSecrets", &nm_psk_request())
                .await
        });
        let nm_prompt = await_prompt_other_than(&mounted.prompts, Some(iwd_prompt.id)).await;
        assert_eq!(nm_prompt.ssid, "NM-net", "NM's prompt is on screen");

        // iwd gives up on its request.
        tokio::time::timeout(
            DBUS_REPLY_BUDGET,
            iwd.call::<_, _, ()>("Cancel", &("timed-out".to_owned(),)),
        )
        .await
        .expect("Cancel did not return in time")
        .expect("iwd's Cancel was refused");

        let iwd_reply = tokio::time::timeout(DBUS_REPLY_BUDGET, iwd_call)
            .await
            .expect("iwd's request did not return in time")
            .expect("request task panicked");
        assert_error_name_is("RequestPassphrase", &iwd_reply, FAILED);

        assert!(
            mounted.waiters.lock().await.contains_key(&nm_prompt.id),
            "NM's request is still waiting"
        );
        assert_eq!(
            shown_id(&mounted.prompts),
            Some(nm_prompt.id),
            "NM's prompt stays on screen"
        );
        assert!(!nm_call.is_finished(), "NM's request is still pending");

        submit(&mounted.waiters, nm_prompt.id, "hunter2").await;
        let secrets = tokio::time::timeout(DBUS_REPLY_BUDGET, nm_call)
            .await
            .expect("NM's GetSecrets did not return in time")
            .expect("GetSecrets task panicked")
            .expect("NM's GetSecrets returned an error");
        let psk = secrets
            .get("802-11-wireless-security")
            .and_then(|setting| setting.get("psk"))
            .and_then(|value| String::try_from(value.try_clone().ok()?).ok());
        assert_eq!(psk.as_deref(), Some("hunter2"));
    }
}
