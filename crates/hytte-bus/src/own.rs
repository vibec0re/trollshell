//! Primitive #1 — own a well-known D-Bus name and serve interfaces under it.
//!
//! See spec section 3.1.

use crate::BusError;
use crate::connection::SharedConnection;
use crate::error::is_transient_zbus_error;
use futures_signals::signal::Mutable;
use futures_util::StreamExt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use zbus::fdo;
use zbus::object_server::{Interface, SignalEmitter};
use zbus::{MatchRule, MessageStream};

/// Type-erased async closure: given a `&zbus::Connection`, mount an interface
/// and return a `zbus::Result<()>`.
type MountFn = Arc<
    dyn Fn(zbus::Connection) -> Pin<Box<dyn Future<Output = zbus::Result<()>> + Send>>
        + Send
        + Sync,
>;

/// Lifecycle of an owned name as observed from outside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnState {
    /// Initial state, or after a loss while we retry.
    Acquiring,
    /// We currently hold the name + the interfaces are mounted.
    Owned,
    /// We just lost the name. Supervisor will retry shortly.
    Lost {
        /// True if this is a single transient loss (the bus blipped).
        transient: bool,
        /// Who holds the name now, if known.
        prev_owner: Option<String>,
    },
    /// Gave up after N consecutive losses to the same owner. The
    /// supervisor still retries every 5 minutes; consumers should render
    /// this state distinctly (e.g. a tray indicator).
    PermanentlyTaken {
        /// The unique name of the connection that currently holds the name.
        current_owner: String,
    },
    /// The broker's policy refused this user the right to own the name
    /// (`RequestName` returned `AccessDenied`). Unlike [`PermanentlyTaken`],
    /// no peer holds the name — only a policy change (e.g. installing a
    /// `/etc/dbus-1/system.d/` rule) will let us acquire it. The supervisor
    /// retries on a long interval so a later policy install is picked up
    /// **without** a shell restart. Consumers should render this distinctly
    /// (the service is inert until the policy allows it).
    ///
    /// [`PermanentlyTaken`]: OwnState::PermanentlyTaken
    Denied,
}

/// A cloneable handle to the live ownership-state signal returned by
/// [`OwnNameBuilder::start`].
///
/// Call [`signal_cloned`](OwnNameSignal::signal_cloned) to obtain a
/// [`futures_signals::signal::Signal`] that tracks the current [`OwnState`].
/// Multiple independent subscriptions are supported.
#[derive(Clone)]
pub struct OwnNameSignal {
    inner: Mutable<OwnState>,
    shared: SharedConnection,
}

impl OwnNameSignal {
    /// Returns a fresh [`Signal`](futures_signals::signal::Signal) that
    /// delivers the current state immediately and then on every change.
    pub fn signal_cloned(&self) -> impl futures_signals::signal::Signal<Item = OwnState> {
        self.inner.signal_cloned()
    }

    /// Emit a D-Bus signal on the connection that owns this name, at the given
    /// object path.
    ///
    /// The closure receives a [`SignalEmitter<'static>`] bound to the owned
    /// connection and the supplied path. Call the macro-generated signal helper
    /// (e.g. `MyIface::my_signal(&emitter, args...).await`) from inside the
    /// closure.
    ///
    /// Reconnect-aware: routes through the same [`SharedConnection`] that the
    /// ownership task uses, so the emitter always targets the currently-live
    /// connection. Returns `Err(`[`BusError::Transient`]`)` when the
    /// connection is mid-reconnect and no live connection is cached.
    pub async fn emit<F, Fut>(&self, path: &str, f: F) -> Result<(), BusError>
    where
        F: FnOnce(SignalEmitter<'static>) -> Fut + Send,
        Fut: Future<Output = zbus::Result<()>> + Send,
    {
        let path_owned = path.to_string();
        self.shared
            .with_conn(|conn| async move {
                let emitter = SignalEmitter::new(&conn, path_owned.as_str())
                    .map_err(|e| zbus::Error::Failure(e.to_string()))?
                    .into_owned();
                f(emitter).await
            })
            .await
    }
}

/// Builder for `own_name`. See the spec (section 3.1) for full semantics.
pub struct OwnNameBuilder {
    shared: SharedConnection,
    name: String,
    permanent_after: u32,
    /// How long to wait after entering `PermanentlyTaken` before retrying.
    /// Defaults to 5 minutes; tests may override to a short duration.
    cooldown: Duration,
    /// Interface mounts to apply on each new connection. Each entry is a
    /// `(path, mount_fn)` pair; `mount_fn` is called with the connection
    /// after `RequestName` succeeds and must register the interface on the
    /// connection's object server. On reconnect, it is called again on the
    /// fresh connection.
    mounts: Vec<(String, MountFn)>,
}

impl OwnNameBuilder {
    /// Register a D-Bus interface at `path` on every connection this builder
    /// acquires. The interface is mounted BEFORE `RequestName` so that callers
    /// racing the `NameAcquired` signal always find the object already present.
    ///
    /// `iface` must be `Clone` because the object server takes ownership on
    /// each mount; the clone is used when the connection is re-established
    /// after a loss.
    #[must_use]
    pub fn at_path<I>(mut self, path: impl Into<String>, iface: I) -> Self
    where
        I: Interface + Clone + Send + Sync + 'static,
    {
        let path_str: String = path.into();
        let path_for_vec = path_str.clone();
        let mount: MountFn = Arc::new(move |conn: zbus::Connection| {
            let iface_clone = iface.clone();
            let p = path_str.clone();
            Box::pin(async move {
                // `at()` returns `Ok(true)` on first mount, `Ok(false)` if an
                // interface was already registered at this path. We treat both
                // as success: a re-iteration that finds the iface still mounted
                // from a prior loop is a no-op, not an error.
                let _ = conn.object_server().at(p.as_str(), iface_clone).await?;
                Ok(())
            })
        });
        self.mounts.push((path_for_vec, mount));
        self
    }

    /// Override the consecutive-losses threshold (default 3).
    #[must_use]
    pub fn permanent_after(mut self, n: u32) -> Self {
        self.permanent_after = n;
        self
    }

    /// Override the cooldown after a `PermanentlyTaken` transition before
    /// re-attempting acquisition. Default: 5 minutes.
    ///
    /// Test-only — consumers should not shorten this in production. The 5-minute
    /// cooldown is what prevents PermanentlyTaken from degrading into a tight
    /// retry loop that would re-introduce the FD-storm pattern this primitive
    /// is designed to prevent.
    #[doc(hidden)]
    #[must_use]
    pub fn cooldown_after_permanent(mut self, d: Duration) -> Self {
        self.cooldown = d;
        self
    }

    /// Spawn the ownership task. Returns an [`OwnNameSignal`] handle; call
    /// `.signal_cloned()` on it to subscribe. Multiple independent subscriptions
    /// are supported — each call to `.signal_cloned()` returns a fresh signal
    /// derived from the same underlying state.
    #[must_use]
    pub fn start(self) -> OwnNameSignal {
        let state = Mutable::new(OwnState::Acquiring);
        let writer = state.clone();
        let shared = self.shared;
        let name = self.name;
        let threshold = self.permanent_after;
        let cooldown = self.cooldown;
        let mounts = self.mounts;
        let shared_clone = shared.clone();
        hytte_reactive::runtime::handle().spawn(async move {
            run_ownership(shared, name, threshold, cooldown, writer, mounts).await;
        });
        OwnNameSignal {
            inner: state,
            shared: shared_clone,
        }
    }
}

/// Internal entry point taking a `SharedConnection` directly. Production
/// callers use `own_name(...)` (Task 12 wires the global session/system).
#[doc(hidden)]
#[must_use]
pub fn own_name_with(shared: &SharedConnection, name: impl Into<String>) -> OwnNameBuilder {
    OwnNameBuilder {
        shared: shared.clone(),
        name: name.into(),
        permanent_after: 3,
        cooldown: Duration::from_mins(5),
        mounts: Vec::new(),
    }
}

async fn run_ownership(
    shared: SharedConnection,
    name: String,
    permanent_after: u32,
    cooldown: Duration,
    writer: Mutable<OwnState>,
    mounts: Vec<(String, MountFn)>,
) {
    // Track consecutive losses to the same owner: (unique_name, count).
    // Reset to None each time we successfully (re-)acquire the name.
    let mut consecutive_losses_to: Option<(String, u32)> = None;

    loop {
        // ── Connect and set up the NameOwnerChanged subscription ─────────────
        //
        // We subscribe ONCE per connection (not per RequestName attempt). This
        // avoids a race between an old `RemoveMatch` (queued async on drop) and
        // a new `AddMatch` for the next retry: the D-Bus daemon would decrement
        // the reference count and silently stop delivering signals.
        //
        // Interface mounts are also applied here, on the fresh connection,
        // before RequestName so callers racing NameAcquired find the objects
        // already present.
        let connect_result = shared
            .with_conn(|conn| {
                let name = name.clone();
                let mounts = mounts.clone();
                async move {
                    // Mount registered interfaces before subscribing.
                    for (_path, mount_fn) in &mounts {
                        mount_fn(conn.clone()).await?;
                    }
                    let match_rule = build_name_owner_changed_rule(&name)?;
                    let stream = MessageStream::for_match_rule(match_rule, &conn, None).await?;
                    Ok((conn, stream))
                }
            })
            .await;

        let (conn, mut stream) = match connect_result {
            Ok(v) => v,
            Err(ref e) if e.is_transient() => {
                writer.set(OwnState::Acquiring);
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, name = %name, "failed to subscribe to NameOwnerChanged");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let unique = conn.unique_name().map(|u| u.as_str().to_string());

        // ── Inner retry loop: reuse the same connection + subscription ────────
        run_inner_loop(InnerCtx {
            conn: &conn,
            stream: &mut stream,
            name: &name,
            unique: unique.as_deref(),
            permanent_after,
            cooldown,
            writer: &writer,
            consecutive_losses_to: &mut consecutive_losses_to,
        })
        .await;
    }
}

/// Context passed to `run_inner_loop` to avoid exceeding the 7-argument limit.
struct InnerCtx<'a> {
    conn: &'a zbus::Connection,
    stream: &'a mut MessageStream,
    name: &'a str,
    unique: Option<&'a str>,
    permanent_after: u32,
    cooldown: Duration,
    writer: &'a Mutable<OwnState>,
    consecutive_losses_to: &'a mut Option<(String, u32)>,
}

/// Handle a failed `RequestName`: set the appropriate [`OwnState`] and back
/// off. The caller returns afterward so the outer loop reconnects with a fresh
/// connection + subscription and re-attempts acquisition.
///
/// `AccessDenied` is the notable case: the broker's policy refuses this user
/// the right to own the name. Installing a `/etc/dbus-1/system.d/` rule
/// (possibly while the shell is already running) is what fixes it, so we
/// surface a distinct [`OwnState::Denied`] and retry on the long `cooldown`
/// interval — a later policy install is then picked up without a restart. The
/// long interval keeps this from becoming the warn-storm / FD-storm a tight
/// retry would cause.
async fn on_request_name_error(
    e: fdo::Error,
    name: &str,
    cooldown: Duration,
    writer: &Mutable<OwnState>,
) {
    if matches!(e, fdo::Error::AccessDenied(_)) {
        tracing::info!(
            %name,
            retry_in_secs = cooldown.as_secs(),
            "DBus name ownership refused by policy; service inert (install a /etc/dbus-1/system.d/ rule granting it); will retry"
        );
        writer.set(OwnState::Denied);
        tokio::time::sleep(cooldown).await;
        writer.set(OwnState::Acquiring);
        return;
    }
    let as_zbus = zbus::Error::FDO(Box::new(e));
    if is_transient_zbus_error(&as_zbus) {
        writer.set(OwnState::Acquiring);
        tokio::time::sleep(Duration::from_millis(250)).await;
    } else {
        tracing::warn!(error = %as_zbus, %name, "RequestName failed");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Inner retry loop: reuse one connection and one `NameOwnerChanged`
/// subscription across multiple `RequestName` attempts.
///
/// Returns when the connection should be dropped and re-established.
async fn run_inner_loop(ctx: InnerCtx<'_>) {
    let InnerCtx {
        conn,
        stream,
        name,
        unique,
        permanent_after,
        cooldown,
        writer,
        consecutive_losses_to,
    } = ctx;
    loop {
        let Ok(dbus) = fdo::DBusProxy::new(conn).await else {
            // DBusProxy construction failures are transient; reconnect.
            writer.set(OwnState::Acquiring);
            tokio::time::sleep(Duration::from_millis(250)).await;
            return;
        };

        let well_known = match name
            .try_into()
            .map_err(|e: zbus::names::Error| zbus::Error::Failure(e.to_string()))
        {
            Ok(w) => w,
            Err(e) => {
                tracing::error!(error = %e, %name, "invalid well-known name");
                tokio::time::sleep(Duration::from_mins(1)).await;
                return;
            }
        };

        let reply = match dbus
            .request_name(
                well_known,
                fdo::RequestNameFlags::AllowReplacement
                    | fdo::RequestNameFlags::ReplaceExisting
                    | fdo::RequestNameFlags::DoNotQueue,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                on_request_name_error(e, name, cooldown, writer).await;
                // Return to reconnect with a fresh connection + subscription
                // and re-attempt RequestName.
                return;
            }
        };

        match reply {
            fdo::RequestNameReply::PrimaryOwner | fdo::RequestNameReply::AlreadyOwner => {
                writer.set(OwnState::Owned);

                // Drain any buffered NameOwnerChanged signals that arrived
                // before we set Owned, then block until we are displaced.
                let new_owner = watch_for_loss(stream, name, unique).await;

                writer.set(OwnState::Lost {
                    transient: new_owner.is_none(),
                    prev_owner: new_owner.clone(),
                });

                if let Some(owner) = new_owner {
                    let new_count = match consecutive_losses_to.as_ref() {
                        Some((who, c)) if who == &owner => c + 1,
                        _ => 1,
                    };
                    if new_count >= permanent_after {
                        *consecutive_losses_to = None;
                        writer.set(OwnState::PermanentlyTaken {
                            current_owner: owner,
                        });
                        tokio::time::sleep(cooldown).await;
                        writer.set(OwnState::Acquiring);
                        // Break to reconnect with a fresh subscription.
                        return;
                    }
                    *consecutive_losses_to = Some((owner, new_count));
                } else {
                    // Transient loss (bus blip / stream ended) — reset
                    // counter and reconnect.
                    *consecutive_losses_to = None;
                    writer.set(OwnState::Acquiring);
                    return;
                }

                // Non-permanent loss: retry RequestName on the same
                // connection + subscription.
                writer.set(OwnState::Acquiring);
            }
            fdo::RequestNameReply::Exists | fdo::RequestNameReply::InQueue => {
                // Name held by someone else; retry after brief pause.
                writer.set(OwnState::Lost {
                    transient: false,
                    prev_owner: None,
                });
                tokio::time::sleep(Duration::from_millis(250)).await;
                writer.set(OwnState::Acquiring);
            }
        }
    }
}

/// Build the `NameOwnerChanged` match rule for the named service (arg0 filter).
///
/// Using a raw `MessageStream` (rather than `DBusProxy::receive_name_owner_changed`)
/// avoids the `SignalStream` proxy-ownership filter, which tracks the daemon's
/// unique name via internal `NameOwnerChanged` handling and can spuriously
/// terminate the stream when the reference-counted match rule is removed.
fn build_name_owner_changed_rule(name: &str) -> Result<zbus::OwnedMatchRule, zbus::Error> {
    let rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .path("/org/freedesktop/DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .interface("org.freedesktop.DBus")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .member("NameOwnerChanged")
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .arg(0, name)
        .map_err(|e| zbus::Error::Failure(e.to_string()))?
        .build();
    Ok(rule.into())
}

/// Poll `stream` until a `NameOwnerChanged` signal shows that `name` was taken
/// from `unique` (our unique name). Returns the new owner's unique name, or
/// `None` if the stream ended (bus error / connection dropped).
async fn watch_for_loss(
    stream: &mut MessageStream,
    name: &str,
    unique: Option<&str>,
) -> Option<String> {
    while let Some(msg) = stream.next().await {
        let Ok(msg) = msg else { continue };
        let Ok((sig_name, old_owner, new_owner)) =
            msg.body().deserialize::<(String, String, String)>()
        else {
            continue;
        };
        if sig_name != name {
            continue;
        }
        // Only act when WE were the previous owner. Buffered signals from
        // before our acquisition (e.g. the previous holder releasing) carry
        // a different old_owner and must be skipped.
        if old_owner.as_str() != unique.unwrap_or("") {
            continue;
        }
        return if new_owner.is_empty() {
            None
        } else {
            Some(new_owner)
        };
    }
    None // stream ended — treat as transient
}

#[cfg(test)]
mod tests {
    use super::{OwnState, on_request_name_error};
    use futures_signals::signal::{Mutable, SignalExt as _};
    use futures_util::StreamExt as _;
    use std::time::Duration;
    use zbus::fdo;

    /// A `RequestName` `AccessDenied` must surface the distinct `Denied` state
    /// and then return to `Acquiring` after the cooldown, so the outer loop
    /// retries (a later policy install is picked up without a restart) — rather
    /// than parking dead as the old `std::future::pending()` did.
    #[tokio::test]
    async fn access_denied_sets_denied_then_reacquires() {
        let writer = Mutable::new(OwnState::Acquiring);
        let mut stream = writer.signal_cloned().to_stream();
        // Consume the initial `Acquiring`.
        assert_eq!(stream.next().await, Some(OwnState::Acquiring));

        let w = writer.clone();
        let task = tokio::spawn(async move {
            on_request_name_error(
                fdo::Error::AccessDenied("policy refuses ownership".into()),
                "mov.vibec0re.test.denied",
                // The cooldown separates the two transitions enough that the
                // signal stream observes `Denied` before `Acquiring` (no
                // latest-value coalescing).
                Duration::from_millis(150),
                &w,
            )
            .await;
        });

        // Drive the stream until we see Denied followed by Acquiring, bounded so
        // a regression fails the test instead of hanging it.
        let mut saw_denied = false;
        loop {
            let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("timed out waiting for Denied→Acquiring transition");
            match next {
                Some(OwnState::Denied) => saw_denied = true,
                Some(OwnState::Acquiring) if saw_denied => break,
                Some(other) => panic!("unexpected state before Denied→Acquiring: {other:?}"),
                None => panic!("state stream ended unexpectedly"),
            }
        }
        assert!(
            saw_denied,
            "AccessDenied must surface a distinct Denied state"
        );

        task.await.expect("on_request_name_error task");
    }
}
