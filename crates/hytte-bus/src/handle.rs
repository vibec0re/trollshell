//! Push-based teardown tracking shared by the long-lived-handle primitives
//! (`property`, `signals`, `proxy`, `export`).
//!
//! Each of those primitives hands the caller a cheaply-cloneable handle that
//! backs a background task. The task must exit the instant the *last* handle
//! clone is dropped — and it must do so **without polling** (dozens of live
//! subscriptions polling at 100 ms each add up to hundreds of timer wakeups per
//! second on an otherwise-idle shell, plus a teardown-latency floor).
//!
//! [`HandleTracker`] provides exactly that: a live-handle count plus a
//! [`Notify`]. The count is decremented with `Release` **before** the last-drop
//! wake, and the task loads it with `Acquire` **after** being woken, so a woken
//! task is guaranteed to observe the final zero count — there is no
//! notify-before-decrement race (which a handle's `Drop` firing a bare `Notify`
//! would have, since `Drop` runs before the `Arc` strong-count decrement).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

/// Tracks how many public handle clones are alive and wakes the background task
/// the moment the last one is dropped. Held by an `Arc` shared between the
/// handle(s) and the task; cloning the handle calls [`inc`](Self::inc) and
/// dropping it calls [`dec`](Self::dec).
pub(crate) struct HandleTracker {
    live: AtomicUsize,
    notify: Notify,
}

impl HandleTracker {
    /// Create a tracker with one live handle already accounted for.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            live: AtomicUsize::new(1),
            notify: Notify::new(),
        })
    }

    /// Account for a cloned handle.
    pub(crate) fn inc(&self) {
        self.live.fetch_add(1, Ordering::Relaxed);
    }

    /// Account for a dropped handle, waking the task on the last drop.
    ///
    /// The decrement is `Release` and the wake is published after it, so a task
    /// woken by [`dropped`](Self::dropped) and reading [`all_dropped`] with
    /// `Acquire` always sees the final count — no teardown race.
    ///
    /// [`all_dropped`]: Self::all_dropped
    pub(crate) fn dec(&self) {
        if self.live.fetch_sub(1, Ordering::Release) == 1 {
            self.notify.notify_one();
        }
    }

    /// Whether every handle clone has been dropped.
    pub(crate) fn all_dropped(&self) -> bool {
        self.live.load(Ordering::Acquire) == 0
    }

    /// Resolve when a handle is dropped (or one already has). Callers re-check
    /// [`all_dropped`](Self::all_dropped) after this returns — an intermediate
    /// (non-last) clone drop does not wake here, but the last one always does.
    pub(crate) async fn dropped(&self) {
        self.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::HandleTracker;

    #[test]
    fn last_drop_flips_all_dropped() {
        let t = HandleTracker::new();
        assert!(!t.all_dropped(), "one live handle: not all dropped");
        t.inc(); // now 2 live
        assert!(!t.all_dropped());
        t.dec(); // back to 1
        assert!(
            !t.all_dropped(),
            "intermediate drop must not report all-dropped"
        );
        t.dec(); // 0
        assert!(t.all_dropped(), "final drop must report all-dropped");
    }

    #[tokio::test]
    async fn dropped_resolves_on_last_drop() {
        let t = HandleTracker::new();
        let waiter = t.clone();
        let task = tokio::spawn(async move {
            waiter.dropped().await;
            waiter.all_dropped()
        });
        // Give the task a moment to park on `dropped()`.
        tokio::task::yield_now().await;
        t.dec(); // last handle drops → wakes the task
        let all_dropped = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("dropped() must resolve after the last dec()")
            .expect("task join");
        assert!(all_dropped, "woken task must observe the final zero count");
    }
}
