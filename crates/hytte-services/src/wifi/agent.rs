//! iwd `net.connman.iwd.Agent` D-Bus interface implementation.

use futures_channel::oneshot;
use futures_signals::signal::Mutable;
use std::sync::atomic::Ordering;

use super::types::PromptRequest;
use super::{NEXT_ID, WaitersMap};
use crate::wifi::client::read_network_metadata;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Placeholder [`PromptRequest::secret_keys`] entry for the iwd backend. iwd's
/// `RequestPassphrase` takes a bare string with no key name, so this exists
/// only to keep the prompt's "one value per key" contract well-formed; the
/// overlay never renders it (a one-key prompt shows a single unlabelled entry).
const IWD_PASSPHRASE_KEY: &str = "passphrase";

// ── Agent ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub(super) struct IwdAgent {
    pub(super) prompts: Mutable<Option<PromptRequest>>,
    pub(super) waiters: WaitersMap,
}

// zbus's `#[interface]` macro requires every method to be `async fn` even
// when the body doesn't await; the EAP stubs also have unused parameters
// since they reject the request without inspecting it. Allowing at the
// impl-block keeps the noise out of each method.
#[allow(clippy::unused_async, unused_variables)]
#[zbus::interface(name = "net.connman.iwd.Agent")]
impl IwdAgent {
    async fn release(&self) {
        tracing::info!("iwd Agent::Release");
    }

    async fn request_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        let path = network.as_str().to_string();
        let (ssid, security) = read_network_metadata(&path).await;

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
        await_passphrase(&self.prompts, &self.waiters, prompt).await
    }

    async fn request_private_key_passphrase(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_name_and_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
    ) -> zbus::fdo::Result<(String, String)> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn request_user_password(
        &self,
        network: zbus::zvariant::OwnedObjectPath,
        username: String,
    ) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::Failed(
            "hytte wifi agent does not support EAP".into(),
        ))
    }

    async fn cancel(&self, reason: String) {
        tracing::info!(%reason, "iwd Agent::Cancel");
        cancel_waiting(&self.prompts, &self.waiters).await;
    }
}

/// Raise `prompt` and wait for the user's answer to it.
///
/// [`IwdAgent::request_passphrase`] after the network's metadata is read, over
/// explicit slots so a test can drive it without a bus. The `bluetooth` agent's
/// `await_reply_in` is the same shape, for the same race (#1491).
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
    prompt: PromptRequest,
) -> zbus::fdo::Result<String> {
    let (tx, rx) = oneshot::channel::<Result<Vec<String>, String>>();
    waiters.lock().await.insert(prompt.id, tx);

    // From here on the prompt is this request's to take down, on every exit,
    // including this future being dropped mid-await.
    let _own = OwnPrompt {
        prompts: prompts.clone(),
        id: prompt.id,
    };
    prompts.set(Some(prompt));

    // Exactly one key was requested, so exactly one value comes back. An
    // empty vector can only mean a malformed submit; treat it as a cancel
    // rather than handing iwd an empty passphrase it would just reject.
    match rx.await {
        Ok(Ok(secrets)) if !secrets.is_empty() => {
            Ok(secrets.into_iter().next().unwrap_or_default())
        }
        _ => Err(zbus::fdo::Error::Failed("agent cancelled".into())),
    }
}

/// Resolve every waiting request with a cancel, and take down the prompt of
/// each one that is still on screen.
///
/// iwd's `Cancel` names no request: it has one outstanding agent request at a
/// time, so every waiter is drained. The prompt slot is cleared only for the
/// ids drained here. It used to be cleared unconditionally, which also took
/// down a prompt this `Cancel` did not cancel. The prompt slot is shared with
/// the `NetworkManager` agent's prompt, and the iwd agent stays exported for
/// the rest of the process once mounted.
///
/// Each woken request clears its own prompt too ([`OwnPrompt`]). Clearing here
/// as well takes the prompt down the moment iwd cancels it, not when its task
/// is next scheduled. Both clears are no-ops unless that prompt is on screen.
async fn cancel_waiting(prompts: &Mutable<Option<PromptRequest>>, waiters: &WaitersMap) {
    let cancelled: Vec<u64> = waiters
        .lock()
        .await
        .drain()
        .map(|(id, tx)| {
            let _ = tx.send(Err("cancelled".into()));
            id
        })
        .collect();
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
    use super::{PromptRequest, await_passphrase, cancel_waiting, clear_prompt_if_shown};
    use crate::wifi::WaitersMap;
    use crate::wifi::types::PromptKind;
    use futures_signals::signal::{Mutable, Signal};
    use futures_util::FutureExt;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;

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
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let a = tokio::spawn({
            let waiters = waiters.clone();
            let prompts = prompts.clone();
            async move { await_passphrase(&prompts, &waiters, prompt(1)).await }
        });
        while shown(&prompts) != Some(1) {
            tokio::task::yield_now().await;
        }

        // Cancel(A). A is now runnable but has not run. Its prompt goes at
        // once: it is one this Cancel cancelled.
        cancel_waiting(&prompts, &waiters).await;
        assert_eq!(shown(&prompts), None, "the cancelled prompt comes down");

        // Request(B) lands first: one poll by hand installs it and raises its
        // prompt, with no await point where A could be scheduled.
        let mut b = std::pin::pin!(await_passphrase(&prompts, &waiters, prompt(2)));
        assert!(b.as_mut().now_or_never().is_none(), "B waits on the user");
        assert_eq!(shown(&prompts), Some(2), "B's prompt is up");

        // A finally runs.
        let a_reply = a.await.expect("request A panicked");
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
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let a = tokio::spawn({
            let waiters = waiters.clone();
            let prompts = prompts.clone();
            async move { await_passphrase(&prompts, &waiters, prompt(1)).await }
        });
        while shown(&prompts) != Some(1) {
            tokio::task::yield_now().await;
        }

        submit(&waiters, 1, "first").await;

        let mut b = std::pin::pin!(await_passphrase(&prompts, &waiters, prompt(2)));
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
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(None);

        let mut a = Box::pin(await_passphrase(&prompts, &waiters, prompt(3)));
        assert!(a.as_mut().now_or_never().is_none());
        assert_eq!(shown(&prompts), Some(3));

        drop(a);
        assert_eq!(shown(&prompts), None);
    }

    /// `Cancel` takes down only the prompts of the requests it cancelled. A
    /// prompt with no iwd waiter behind it (the shared slot can hold the
    /// `NetworkManager` agent's) stays up.
    ///
    /// **Falsification:** make `cancel_waiting` clear the slot unconditionally
    /// (the pre-#1551 `self.prompts.set(None)`) and the first assertion fails.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_leaves_a_prompt_it_did_not_cancel() {
        let waiters = waiters();
        let prompts: Mutable<Option<PromptRequest>> = Mutable::new(Some(prompt(9)));
        let (tx, rx) = futures_channel::oneshot::channel();
        waiters.lock().await.insert(1, tx);

        cancel_waiting(&prompts, &waiters).await;
        assert_eq!(shown(&prompts), Some(9), "prompt 9 was not cancelled");
        assert_eq!(
            rx.await.expect("the waiter was answered"),
            Err("cancelled".to_owned()),
            "the waiting request is cancelled"
        );
        assert!(waiters.lock().await.is_empty(), "every waiter is drained");
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
}
