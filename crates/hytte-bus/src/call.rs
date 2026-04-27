//! Primitive #3 — one-shot D-Bus method call.
//!
//! See spec section 3.3.

use crate::connection::SharedConnection;
use crate::error::BusError;
use futures_signals::signal::SignalExt;
use futures_util::StreamExt;
use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;
use zbus::zvariant::Type;

/// Retry behavior on transient bus failure (the bus was mid-reconnect when
/// the call landed).
#[derive(Clone, Copy, Debug)]
pub enum RetryPolicy {
    /// Never retry; surface `BusError::Transient` immediately.
    Never,
    /// Retry once after the supervisor re-establishes the connection.
    /// This is the default — the only legitimate transient on a local bus.
    Once,
    /// Exponential backoff up to N attempts.
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
            (Err(_), RetryPolicy::Backoff { max_attempts }) => {
                let mut attempts = 1u32;
                let mut delay = Duration::from_millis(250);
                loop {
                    if attempts >= max_attempts {
                        // Last attempt: return whatever we get.
                        return self.do_call::<R>().await;
                    }
                    tokio::time::sleep(delay).await;
                    match self.do_call::<R>().await {
                        Ok(r) => return Ok(r),
                        Err(e) if !e.is_transient() => return Err(e),
                        Err(_) => {
                            attempts += 1;
                            delay = (delay * 2).min(Duration::from_secs(30));
                        }
                    }
                }
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
    /// Override which bus this builder targets. The default is determined by
    /// the constructor: [`call`](crate::call) uses the session bus.
    ///
    /// Overriding here replaces the `SharedConnection` with the corresponding
    /// global singleton.
    #[must_use]
    pub fn bus(self, kind: crate::BusKind) -> CallBuilder<A> {
        CallBuilder {
            shared: match kind {
                crate::BusKind::Session => crate::connection::session().clone(),
                crate::BusKind::System => crate::connection::system().clone(),
            },
            destination: self.destination,
            path: self.path,
            iface: self.iface,
            method: self.method,
            args: self.args,
            timeout: self.timeout,
            retry: self.retry,
        }
    }

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
