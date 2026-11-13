//! The seam over `hytte-bus`'s "subscribed" handshake (#1551).
//!
//! A service that subscribes and then reads a daemon's state has to wait for
//! [`hytte_bus::SignalSubscription::subscribed`] in between, or whatever the
//! daemon emits before the match rule lands is lost. The helpers that order
//! those steps ([`subscribe_then_snapshot`], and `wifi`'s `open_discovery`)
//! take the subscription as a [`Handshake`] and do the waiting **themselves**.
//! A call site cannot leave the wait out, and a test can hand the helper a
//! recording stand-in and read the order back.
//!
//! [`subscribe_then_snapshot`] serves every site where the read comes after
//! one wait: `bluetooth`'s `listen` (its owner subscription, then the
//! snapshot), its `event_loop` (every subscription the fold reads, then one
//! resync), its `wait_for_bluez` (the owner subscription, then
//! `NameHasOwner`), and `wifi`'s station (the station's `PropertiesChanged`,
//! then the station re-read). `open_discovery` has an extra step between the
//! receivers and the wait, so it orders its own.

use hytte_bus::SignalSubscription;

/// Something whose match rule can be waited for.
pub(crate) trait Handshake {
    /// Resolves once the bus has installed this subscription's match rule.
    /// See [`SignalSubscription::subscribed`] for what that does and does not
    /// promise across a reconnect.
    fn subscribed(&self) -> impl Future<Output = ()>;
}

impl Handshake for SignalSubscription {
    fn subscribed(&self) -> impl Future<Output = ()> {
        SignalSubscription::subscribed(self)
    }
}

/// Subscribe (`subscribe`, which takes its receivers too), wait for the bus
/// to have the match rule, then read (`snapshot`), in that order (#1551).
///
/// A helper rather than three lines at each site so the order is pinned
/// (#1573 review): the pre-#1551 regression was the subscribe and the
/// snapshot swapped, which no test could see.
///
/// The wait in the middle is `hytte-bus`'s handshake
/// ([`SignalSubscription::subscribed`]). A subscription installs its match
/// rule from a spawned task, so without the wait the read could be answered
/// before the bus routed anything here. It is made here, on what `subscribe`
/// returned, and not handed in as a step, so a call site cannot leave it out.
/// `snapshot` is not called until the wait is over, so its request is not
/// even built before then.
pub(crate) async fn subscribe_then_snapshot<H, R, T, F>(
    subscribe: impl FnOnce() -> (H, R),
    snapshot: impl FnOnce() -> F,
) -> ((H, R), T)
where
    H: Handshake,
    F: Future<Output = T>,
{
    let (subscription, receiver) = subscribe();
    subscription.subscribed().await;
    let taken = snapshot().await;
    ((subscription, receiver), taken)
}

#[cfg(test)]
mod tests {
    use super::{Handshake, subscribe_then_snapshot};
    use std::cell::RefCell;

    /// A subscription whose handshake is not ready until polled twice, so a
    /// helper that only *creates* the wait without awaiting it to the end is
    /// caught too.
    struct Recording<'a>(&'a RefCell<Vec<&'static str>>);

    impl Handshake for Recording<'_> {
        fn subscribed(&self) -> impl Future<Output = ()> {
            let log = self.0;
            async move {
                tokio::task::yield_now().await;
                log.borrow_mut().push("match rule installed");
            }
        }
    }

    /// The helper subscribes, waits for the bus to have the match rule, and
    /// only then reads (#1573 review, #1551).
    ///
    /// **Falsification:** take the snapshot first (the pre-#1551 order) and
    /// the recorded order flips. Drop the `subscribed().await` and "match
    /// rule installed" never reaches the log; move it below the snapshot and
    /// the order flips. Call `snapshot` before the wait and poll it after,
    /// and "snapshot called" moves up.
    #[tokio::test]
    async fn the_read_waits_for_the_match_rule() {
        let log = RefCell::new(Vec::new());
        let ((_subscription, ()), ()) = subscribe_then_snapshot(
            || {
                log.borrow_mut().push("subscribe");
                (Recording(&log), ())
            },
            || {
                log.borrow_mut().push("snapshot called");
                async { log.borrow_mut().push("snapshot sent") }
            },
        )
        .await;
        assert_eq!(
            log.into_inner(),
            [
                "subscribe",
                "match rule installed",
                "snapshot called",
                "snapshot sent"
            ]
        );
    }
}
