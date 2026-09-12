//! Primitive #3 — one-shot D-Bus method call.
//!
//! See spec section 3.3.

use crate::connection::SharedConnection;
use crate::error::BusError;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use serde::{Serialize, de::DeserializeOwned};
use std::time::Duration;
use zbus::zvariant::Type;

/// A leased file descriptor returned by a D-Bus method whose reply carries a
/// `UNIX_FD` (`h`), wrapping an owned [`std::os::fd::OwnedFd`].
///
/// The fd stays open for exactly as long as this guard is alive; dropping it
/// closes the fd. For resources whose lifetime *is* an open fd — most notably a
/// logind inhibitor from `org.freedesktop.login1.Manager.Inhibit` — that drop
/// **releases** the resource. Store the lease wherever the hold should persist
/// (e.g. a service's `Handles`) and drop it to release.
///
/// Obtained from [`CallBuilder::call_fd`]. The fd is `dup`'d out of the reply
/// message, so it is fully independent of the D-Bus call machinery and remains
/// valid after that call resolves.
#[derive(Debug)]
#[must_use = "dropping the FdLease closes the fd, releasing the underlying \
              resource (e.g. a logind inhibitor)"]
pub struct FdLease {
    fd: std::os::fd::OwnedFd,
}

impl FdLease {
    /// Consume the lease and hand back the raw [`std::os::fd::OwnedFd`].
    ///
    /// The fd remains open; the caller takes over responsibility for keeping it
    /// alive (and, eventually, closing it). The lease's `drop`-releases-the-fd
    /// contract no longer applies once the fd has been extracted.
    #[must_use]
    pub fn into_inner(self) -> std::os::fd::OwnedFd {
        self.fd
    }
}

impl std::os::fd::AsFd for FdLease {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        std::os::fd::AsFd::as_fd(&self.fd)
    }
}

impl std::os::fd::AsRawFd for FdLease {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.fd)
    }
}

/// Retry behavior on transient bus failure (the bus was mid-reconnect when
/// the call landed).
#[derive(Clone, Copy, Debug)]
pub enum RetryPolicy {
    /// Never retry; surface `BusError::Transient` immediately.
    Never,
    /// Retry once after the supervisor re-establishes the connection.
    /// This is the default — the only legitimate transient on a local bus.
    Once,
    /// Exponential backoff, on the crate's shared 250 ms → 30 s ramp
    /// (`backoff::delay_for`).
    ///
    /// `max_attempts` is the number of **calls issued**, counting the first —
    /// so `max_attempts: 1` never retries and `max_attempts: 3` makes at most
    /// three calls with two sleeps between them. Until #1173 it issued
    /// `max_attempts + 1`, because the loop started its counter after the first
    /// call had already been made and then made one more on the iteration that
    /// hit the bound.
    Backoff { max_attempts: u32 },
}

/// Builder for a one-shot D-Bus method call.
pub struct CallBuilder<A> {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    method: String,
    args: A,
    timeout: Duration,
    retry: RetryPolicy,
}

/// Owned version of `CallBuilder` used internally for `fire_and_forget` spawning.
/// Holds `SharedConnection` by value (cheap — it's an Arc clone) so it can be
/// sent across a `'static` future boundary.
struct OwnedCall<A> {
    shared: SharedConnection,
    destination: String,
    path: String,
    iface: String,
    method: String,
    args: A,
    timeout: Duration,
    retry: RetryPolicy,
}

impl<A> OwnedCall<A>
where
    A: Serialize + Type + Clone,
{
    async fn do_call<R>(&self) -> Result<R, BusError>
    where
        R: DeserializeOwned + Type,
    {
        self.shared
            .with_conn(|conn| {
                let dest = self.destination.clone();
                let path = self.path.clone();
                let iface = self.iface.clone();
                let method = self.method.clone();
                let args = self.args.clone();
                let timeout = self.timeout;
                async move {
                    let proxy =
                        zbus::Proxy::new(&conn, dest.as_str(), path.as_str(), iface.as_str())
                            .await?;
                    let fut = proxy.call::<_, _, R>(method.as_str(), &args);
                    tokio::time::timeout(timeout, fut)
                        .await
                        .map_err(|_| zbus::Error::Failure("call timeout".into()))?
                }
            })
            .await
    }

    async fn execute<R>(self) -> Result<R, BusError>
    where
        R: DeserializeOwned + Type + 'static,
        A: Send + Sync + 'static,
    {
        let attempt_one = self.do_call::<R>().await;
        match (attempt_one, self.retry) {
            (Ok(r), _) => Ok(r),
            (Err(e), RetryPolicy::Never) => Err(e),
            (Err(e), _) if !e.is_transient() => Err(e),
            (Err(_), RetryPolicy::Once) => {
                // Wait for the supervisor to re-establish the connection (epoch
                // advances) before retrying. This avoids an immediate second
                // attempt that would also fail if the supervisor hasn't reconnected.
                wait_for_reconnect(&self.shared, self.timeout).await;
                self.do_call::<R>().await
            }
            (Err(e), RetryPolicy::Backoff { max_attempts }) => {
                retry_with_backoff(e, max_attempts, || self.do_call::<R>()).await
            }
        }
    }
}

/// Entry point for constructing a one-shot D-Bus method call.
///
/// # Example
/// ```ignore
/// let names: Vec<String> = call_with(&shared, "org.freedesktop.DBus")
///     .at_path("/org/freedesktop/DBus")
///     .iface("org.freedesktop.DBus")
///     .method("ListNames")
///     .args(())
///     .send()
///     .await?;
/// ```
#[doc(hidden)]
#[must_use]
pub fn call_with(shared: &SharedConnection, destination: impl Into<String>) -> CallBuilder<()> {
    CallBuilder {
        shared: shared.clone(),
        destination: destination.into(),
        path: String::new(),
        iface: String::new(),
        method: String::new(),
        args: (),
        timeout: Duration::from_secs(25),
        retry: RetryPolicy::Once,
    }
}

impl<A> CallBuilder<A> {
    /// Set the object path.
    #[must_use]
    pub fn at_path(mut self, p: impl Into<String>) -> Self {
        self.path = p.into();
        self
    }

    /// Set the D-Bus interface name.
    #[must_use]
    pub fn iface(mut self, i: impl Into<String>) -> Self {
        self.iface = i.into();
        self
    }

    /// Set the method name.
    #[must_use]
    pub fn method(mut self, m: impl Into<String>) -> Self {
        self.method = m.into();
        self
    }

    /// Override the call timeout (default: 25 s).
    #[must_use]
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Override the retry policy (default: `RetryPolicy::Once`).
    #[must_use]
    pub fn retry(mut self, r: RetryPolicy) -> Self {
        self.retry = r;
        self
    }

    /// Set the call arguments, changing the type parameter.
    #[must_use]
    pub fn args<NewA>(self, args: NewA) -> CallBuilder<NewA>
    where
        NewA: Serialize + Type,
    {
        CallBuilder {
            shared: self.shared,
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            method: self.method,
            args,
            timeout: self.timeout,
            retry: self.retry,
        }
    }

    /// Convert this builder into an owned call.
    fn into_owned(self) -> OwnedCall<A> {
        OwnedCall {
            shared: self.shared,
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            method: self.method,
            args: self.args,
            timeout: self.timeout,
            retry: self.retry,
        }
    }
}

impl<A> CallBuilder<A>
where
    A: Serialize + Type + Send + Sync + Clone + 'static,
{
    /// Execute the call. Returns the deserialized reply, or a `BusError`.
    pub async fn send<R>(self) -> Result<R, BusError>
    where
        R: DeserializeOwned + Type + 'static,
    {
        self.into_owned().execute::<R>().await
    }

    /// Execute a method whose reply carries a single `UNIX_FD` (`h`) and take
    /// **ownership** of that fd, returning it as an [`FdLease`].
    ///
    /// The canonical use is a logind inhibitor:
    /// `org.freedesktop.login1.Manager.Inhibit(what, who, why, mode)` returns an
    /// fd whose open-ness *is* the lock — the inhibition lasts exactly as long
    /// as the returned [`FdLease`] is alive. Hold the lease (e.g. in a service's
    /// `Handles`) for as long as the inhibition should last; drop it to release.
    ///
    /// The fd is `dup`'d out of the reply message during deserialization, so it
    /// is fully independent of the D-Bus call machinery and stays valid after
    /// this future resolves (the reply message — and the fd it carried — is
    /// dropped while our independent dup lives on in the [`FdLease`]).
    ///
    /// `login1` lives on the **system** bus, so construct the call with
    /// `call(BusKind::System, …)`. Retry/timeout behaviour is identical to
    /// [`send`](Self::send).
    ///
    /// # Errors
    /// Returns a [`BusError`] if the call fails: a transient bus error (subject
    /// to the configured [`RetryPolicy`]), a timeout, or a D-Bus error reply.
    pub async fn call_fd(self) -> Result<FdLease, BusError> {
        let fd: zbus::zvariant::OwnedFd = self.into_owned().execute().await?;
        Ok(FdLease { fd: fd.into() })
    }

    /// Spawn the call onto the runtime and ignore the reply. Errors are
    /// logged via `tracing::warn!` with destination and method context.
    /// For calls that return a non-unit reply but where the caller does not
    /// need the result, use `runtime::handle().spawn(self.send::<R>())`
    /// directly.
    pub fn fire_and_forget(self) {
        // Clone SharedConnection (cheap — it's an Arc) so the future is 'static.
        let dest_log = self.destination.clone();
        let method_log = self.method.clone();
        let owned = self.into_owned();
        hytte_reactive::runtime::handle().spawn(async move {
            if let Err(e) = owned.execute::<()>().await {
                tracing::warn!(
                    destination = %dest_log,
                    method = %method_log,
                    error = %e,
                    "fire_and_forget call failed",
                );
            }
        });
    }
}

/// Keep calling `attempt` until it succeeds, fails permanently, or the call
/// budget is spent, sleeping the crate's shared ramp in between.
///
/// `first` is the outcome of call #1, which the caller has already made, and
/// `max_attempts` counts calls — so this issues at most `max_attempts - 1`
/// more. The loop this replaces started its counter at 1 *after* the first call
/// and then made one further call on the very iteration that hit the bound, so
/// `Backoff { max_attempts: n }` issued n + 1 calls and `max_attempts: 1` —
/// which can only mean "do not retry" — bought a retry anyway (#1173).
///
/// Taking the attempt as a closure is what makes that countable: `mod tests`
/// drives this exact loop with a counting closure under paused time, so the
/// assertion is about the shipped control flow rather than a restatement of it.
///
/// The delays come from [`crate::backoff::delay_for`] rather than a private
/// doubling cursor. The ramp is numerically identical to the one it replaces
/// (250 ms doubling to a 30 s ceiling); what it buys is that there is one of
/// them.
async fn retry_with_backoff<R, F, Fut>(
    first: BusError,
    max_attempts: u32,
    mut attempt: F,
) -> Result<R, BusError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<R, BusError>>,
{
    let mut last = first;
    let mut calls = 1u32;
    while calls < max_attempts {
        tokio::time::sleep(crate::backoff::delay_for(calls)).await;
        calls += 1;
        match attempt().await {
            Ok(v) => return Ok(v),
            Err(e) if !e.is_transient() => return Err(e),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Wait until the connection epoch advances (indicating the supervisor has
/// successfully reconnected), or until `deadline` expires. Used by the
/// `RetryPolicy::Once` path to avoid an immediate retry that would also fail.
async fn wait_for_reconnect(shared: &SharedConnection, timeout: Duration) {
    let current_epoch = shared.epoch();
    let mut epoch_stream = shared.epoch_signal().to_stream();
    // Give the supervisor a reasonable time to reconnect, bounded by the
    // caller's overall timeout (so fire_and_forget paths don't wait forever).
    let deadline = timeout.min(Duration::from_secs(5));
    let _ = tokio::time::timeout(deadline, async {
        while let Some(epoch) = epoch_stream.next().await {
            if epoch > current_epoch {
                return;
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::{BusError, retry_with_backoff};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// The only failure class `Backoff` retries.
    fn transient() -> BusError {
        BusError::Transient {
            source: zbus::Error::FDO(Box::new(zbus::fdo::Error::Disconnected(
                "bus mid-reconnect".to_owned(),
            ))),
        }
    }

    fn permanent() -> BusError {
        BusError::Permanent {
            reason: "no such method".to_owned(),
            dbus_name: Some("org.freedesktop.DBus.Error.UnknownMethod".to_owned()),
        }
    }

    /// Run the shipped retry loop against a bus that never comes back and
    /// report how many calls were issued in total — the first one the caller
    /// already made, plus every retry the loop drove.
    async fn calls_issued(max_attempts: u32) -> u32 {
        let count = AtomicU32::new(1);
        let outcome: Result<(), BusError> = retry_with_backoff(transient(), max_attempts, || {
            count.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Err(transient()))
        })
        .await;
        assert!(outcome.is_err(), "a bus that never answers cannot succeed");
        count.load(Ordering::Relaxed)
    }

    /// `max_attempts` is a count of calls, not of retries-after-the-first.
    ///
    /// The pre-#1173 loop returns `n + 1` for every one of these, so `1` — the
    /// only spelling of "do not retry" the variant has — issued two calls.
    #[tokio::test(start_paused = true)]
    async fn max_attempts_is_the_number_of_calls_issued() {
        for n in 1..=6u32 {
            assert_eq!(
                calls_issued(n).await,
                n,
                "Backoff {{ max_attempts: {n} }} must issue exactly {n} calls"
            );
        }
    }

    /// Zero is not a spelling anyone should use, but it must not mean
    /// "unbounded": the loop has to terminate having made only the call the
    /// caller already made.
    #[tokio::test(start_paused = true)]
    async fn a_zero_budget_retries_nothing() {
        assert_eq!(calls_issued(0).await, 1);
    }

    /// The waits between calls walk the crate's shared ramp — 250 ms, 500 ms,
    /// 1 s — rather than a private cursor that can drift from it. Paused time
    /// makes this exact.
    #[tokio::test(start_paused = true)]
    async fn the_waits_walk_the_shared_ramp() {
        let start = tokio::time::Instant::now();
        assert_eq!(calls_issued(4).await, 4);
        assert_eq!(
            start.elapsed(),
            Duration::from_millis(250 + 500 + 1000),
            "four calls means three waits, at backoff::delay_for's first three steps"
        );
    }

    /// A success stops the loop where it happened, and nothing is called after
    /// it.
    #[tokio::test(start_paused = true)]
    async fn a_success_ends_the_run() {
        let count = AtomicU32::new(1);
        let outcome: Result<u32, BusError> = retry_with_backoff(transient(), 10, || {
            let n = count.fetch_add(1, Ordering::Relaxed) + 1;
            std::future::ready(if n == 3 { Ok(n) } else { Err(transient()) })
        })
        .await;
        assert_eq!(outcome.ok(), Some(3));
        assert_eq!(
            count.load(Ordering::Relaxed),
            3,
            "no call after the success"
        );
    }

    /// A permanent failure ends the run too: retrying an `UnknownMethod` is
    /// just latency. The budget is irrelevant once the answer cannot change.
    #[tokio::test(start_paused = true)]
    async fn a_permanent_failure_ends_the_run() {
        let count = AtomicU32::new(1);
        let outcome: Result<(), BusError> = retry_with_backoff(transient(), 10, || {
            count.fetch_add(1, Ordering::Relaxed);
            std::future::ready(Err(permanent()))
        })
        .await;
        assert!(outcome.is_err_and(|e| !e.is_transient()));
        assert_eq!(
            count.load(Ordering::Relaxed),
            2,
            "the first permanent answer must be the last call"
        );
    }
}
