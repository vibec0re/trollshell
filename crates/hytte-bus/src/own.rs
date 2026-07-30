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

/// Stand-in holder identity reported when a name is taken but we never managed
/// to identify by whom — `GetNameOwner` races the holder disappearing, so it can
/// legitimately fail right after a `RequestName` said the name was taken.
///
/// Only surfaces when *no* observation in the current episode was attributable;
/// a single successful lookup keeps its answer for the rest of the episode (see
/// `record_loss`). Cannot collide with a real holder: unique names always start
/// with `:`.
pub const UNKNOWN_HOLDER: &str = "<unknown>";

/// How long to wait before re-requesting a name we just lost (or failed to
/// take) while still below the `permanent_after` threshold.
const RETRY_AFTER_LOSS: Duration = Duration::from_millis(250);

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
        /// The unique name of the connection that currently holds the name,
        /// or [`UNKNOWN_HOLDER`] when the broker could not tell us (see
        /// [`UNKNOWN_HOLDER`] for when that happens).
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

    /// Override the consecutive-losses threshold (default 3) after which the
    /// name latches [`OwnState::PermanentlyTaken`] and the retry drops to one
    /// attempt per cooldown.
    ///
    /// Counts both ways of not having the name: being displaced after owning
    /// it, and `RequestName` coming back "already taken" because the current
    /// owner refused replacement.
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
    // Track consecutive losses to the same owner: (unique_name, count). See
    // `record_loss` for exactly when it survives and when it resets — notably
    // it does NOT reset on a successful re-acquisition, because losing the same
    // name to the same peer three times running is a replacement war whether or
    // not we won it back in between.
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

/// What to do about one observation of the name being in someone else's hands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contention {
    /// Still below `permanent_after`: retry on the short interval.
    Retry {
        /// 1-based count of consecutive losses to this holder.
        consecutive: u32,
    },
    /// At or past `permanent_after`: latch [`OwnState::PermanentlyTaken`] and
    /// back off to the long cooldown. `consecutive` keeps climbing past the
    /// threshold, so `consecutive == permanent_after` identifies the *first*
    /// give-up and anything larger is a re-latch on a later cooldown cycle.
    GiveUp {
        /// 1-based count of consecutive losses to this holder.
        consecutive: u32,
    },
}

/// Fold one loss of the name into the consecutive-loss tally and decide what to
/// do about it. `holder` is `None` when we could not establish who has it.
///
/// The tally is keyed by holder so alternating contenders never accumulate into
/// a give-up: only *consecutive* losses to the *same* unique name count, and a
/// loss to a different holder restarts at 1.
///
/// An **unattributable** loss (`holder == None`) continues whatever tally is
/// already in flight and leaves its key alone, rather than starting a new one.
/// This is load-bearing, not defensive coding: `GetNameOwner` routinely loses
/// the race with a holder that releases the name right after refusing us, so a
/// contended name produces an alternating `Some(peer)` / `None` sequence. Were
/// `None` treated as a distinct holder, every other observation would reset the
/// count to 1 and `permanent_after` could never trip — reinstating exactly the
/// unbounded retry this accounting exists to stop. (Observed: without this rule
/// the tally oscillated `:1.1`→1, `<unknown>`→1, forever.)
///
/// Two further deliberate non-resets:
///
/// * It survives a successful re-acquisition. Three losses in a row to one peer
///   is a replacement war worth latching even though we held the name briefly
///   between each; that is precisely what `permanent_after` exists to stop.
/// * It survives a give-up. Once latched, each cooldown wake makes a *single*
///   attempt and re-latches, instead of resetting to zero and firing a fresh
///   burst of `permanent_after` attempts every cycle. That is what keeps a
///   permanently-camped name at one `RequestName` per cooldown.
///
/// Only a transient loss (a bus blip, where nobody took the name from us at
/// all) clears it, at the call site.
fn record_loss(
    tally: &mut Option<(String, u32)>,
    holder: Option<&str>,
    permanent_after: u32,
) -> Contention {
    let consecutive = match (tally.as_ref(), holder) {
        // Another loss to the peer we are already counting against.
        (Some((who, count)), Some(new)) if who == new => count.saturating_add(1),
        // Unattributable, but a tally is already running: continue it.
        (Some((_, count)), None) => count.saturating_add(1),
        // First loss, or a different peer has taken over: start again at 1.
        _ => 1,
    };
    // The key only moves when we positively identified a holder, so an
    // unattributable observation cannot erase who we are counting against.
    let key = match (holder, tally.take()) {
        (Some(new), _) => new.to_string(),
        (None, Some((who, _))) => who,
        (None, None) => UNKNOWN_HOLDER.to_string(),
    };
    *tally = Some((key, consecutive));
    if consecutive >= permanent_after {
        Contention::GiveUp { consecutive }
    } else {
        Contention::Retry { consecutive }
    }
}

/// The holder the tally is currently attributed to — the last peer we
/// positively identified, which may predate an unattributable observation.
/// [`UNKNOWN_HOLDER`] if we never identified one.
fn attributed_holder(tally: Option<&(String, u32)>) -> &str {
    tally.map_or(UNKNOWN_HOLDER, |(who, _)| who.as_str())
}

/// Log the give-up transition into [`OwnState::PermanentlyTaken`].
///
/// **Cadence.** This fires once per `cooldown` — one line every 5 minutes by
/// default — for as long as the name stays taken, because `record_loss`
/// deliberately keeps the tally latched so each cooldown wake makes exactly one
/// attempt. That is the middle ground between the two positions this repo has
/// argued: #634 kept a *self-healing* condition loud on every attempt precisely
/// because its backoff capped the rate, while #646 objects to flat, uncapped
/// retry logging with no ceiling. A contested well-known name is a *static*
/// condition — nothing changes until the other owner exits — so it must not
/// repeat at the retry rate, but it must still be findable in a journal opened
/// long after the shell started. Rate-capping the line to the cooldown gives
/// both, and the live [`OwnState`] signal carries the condition continuously
/// for any consumer that wants it without costing a log line at all.
fn log_give_up(name: &str, holder: &str, consecutive: u32, cooldown: Duration) {
    tracing::warn!(
        %name,
        %holder,
        consecutive,
        retry_in_secs = cooldown.as_secs(),
        "D-Bus name is held by another connection that refuses to be replaced; whatever this name backs is inert until that owner exits. Still re-checking periodically"
    );
}

/// Ask the broker who currently holds `name`, for the log line and for the
/// tally key.
///
/// Best-effort, and it fails often enough to matter: the holder can release the
/// name between the `RequestName` reply that told us it was taken and this
/// call, in which case the broker answers `NameHasNoOwner`. `None` therefore
/// means "we could not attribute this loss", which [`record_loss`] handles
/// explicitly rather than treating as a holder in its own right.
async fn current_holder_of(dbus: &fdo::DBusProxy<'_>, name: &str) -> Option<String> {
    let bus_name = zbus::names::BusName::try_from(name).ok()?;
    dbus.get_name_owner(bus_name)
        .await
        .ok()
        .map(|owner| owner.as_str().to_string())
}

/// Arguments for [`on_name_taken`], kept in a struct to match the local
/// convention (see [`InnerCtx`]) rather than a long positional list.
struct TakenCtx<'a> {
    dbus: &'a fdo::DBusProxy<'a>,
    name: &'a str,
    permanent_after: u32,
    cooldown: Duration,
    writer: &'a Mutable<OwnState>,
    tally: &'a mut Option<(String, u32)>,
}

/// Handle a `RequestName` reply saying somebody else already holds the name.
///
/// With `DoNotQueue` set this is always `Exists`, and it means the current
/// owner did **not** pass `AllowReplacement` — so our `ReplaceExisting` was
/// refused and no amount of asking again will change that while they hold it.
/// This is the mako/dunst-owns-`org.freedesktop.Notifications` case, and before
/// this path fed the tally it retried at 4 Hz for the whole process lifetime
/// without ever logging or escalating.
///
/// Returns `true` when the caller should drop the connection and reconnect.
async fn on_name_taken(ctx: TakenCtx<'_>) -> bool {
    let TakenCtx {
        dbus,
        name,
        permanent_after,
        cooldown,
        writer,
        tally,
    } = ctx;

    let holder = current_holder_of(dbus, name).await;
    writer.set(OwnState::Lost {
        transient: false,
        prev_owner: holder.clone(),
    });

    let verdict = record_loss(tally, holder.as_deref(), permanent_after);
    let holder = attributed_holder(tally.as_ref()).to_string();

    match verdict {
        Contention::Retry { consecutive } => {
            tracing::warn!(
                %name,
                %holder,
                consecutive,
                permanent_after,
                "D-Bus name is already owned by a connection that refused replacement; retrying"
            );
            // Grow the gap with each consecutive failure, so a name that is
            // only briefly held is not declared permanently taken after a
            // fraction of a second: at the default threshold of 3 this waits
            // 250 ms and then 500 ms, latching at ~0.75 s rather than ~0.5 s.
            // Deliberately a multiple of one constant rather than a fourth
            // hand-rolled backoff type (#646) — the give-up cooldown, not this
            // ramp, is what actually bounds the retry rate.
            tokio::time::sleep(RETRY_AFTER_LOSS * consecutive).await;
            writer.set(OwnState::Acquiring);
            false
        }
        Contention::GiveUp { consecutive } => {
            log_give_up(name, &holder, consecutive, cooldown);
            writer.set(OwnState::PermanentlyTaken {
                current_owner: holder,
            });
            tokio::time::sleep(cooldown).await;
            writer.set(OwnState::Acquiring);
            true
        }
    }
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

                let Some(holder) = new_owner else {
                    // Transient loss (bus blip / stream ended) — nobody took
                    // the name from us, so the tally resets and we reconnect.
                    // Logged at debug because `connection.rs` already warns
                    // about the disconnect itself; this is its consequence.
                    tracing::debug!(%name, "D-Bus name dropped with the connection; reconnecting");
                    *consecutive_losses_to = None;
                    writer.set(OwnState::Acquiring);
                    return;
                };

                match record_loss(consecutive_losses_to, Some(&holder), permanent_after) {
                    Contention::Retry { consecutive } => {
                        tracing::warn!(
                            %name,
                            %holder,
                            consecutive,
                            permanent_after,
                            "lost D-Bus name to another connection; re-requesting it"
                        );
                        // Retry RequestName on the same connection +
                        // subscription.
                        writer.set(OwnState::Acquiring);
                    }
                    Contention::GiveUp { consecutive } => {
                        log_give_up(name, &holder, consecutive, cooldown);
                        writer.set(OwnState::PermanentlyTaken {
                            current_owner: holder,
                        });
                        tokio::time::sleep(cooldown).await;
                        writer.set(OwnState::Acquiring);
                        // Break to reconnect with a fresh subscription.
                        return;
                    }
                }
            }
            fdo::RequestNameReply::Exists | fdo::RequestNameReply::InQueue => {
                if on_name_taken(TakenCtx {
                    dbus: &dbus,
                    name,
                    permanent_after,
                    cooldown,
                    writer,
                    tally: &mut *consecutive_losses_to,
                })
                .await
                {
                    return;
                }
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
    use super::{Contention, OwnState, attributed_holder, on_request_name_error, record_loss};
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

    /// Consecutive losses to one holder accumulate and trip at
    /// `permanent_after` — the accounting that the `Exists`/`InQueue` branch
    /// never reached before #653, leaving it retrying at 4 Hz forever.
    #[test]
    fn consecutive_losses_to_one_holder_trip_the_threshold() {
        let mut tally = None;
        assert_eq!(
            record_loss(&mut tally, Some(":1.7"), 3),
            Contention::Retry { consecutive: 1 }
        );
        assert_eq!(
            record_loss(&mut tally, Some(":1.7"), 3),
            Contention::Retry { consecutive: 2 }
        );
        assert_eq!(
            record_loss(&mut tally, Some(":1.7"), 3),
            Contention::GiveUp { consecutive: 3 },
            "the third consecutive loss to the same holder must give up"
        );
        assert_eq!(attributed_holder(tally.as_ref()), ":1.7");
    }

    /// A loss to a *different* holder is not a continuation: two contenders
    /// alternating must never accumulate into a give-up.
    #[test]
    fn a_different_holder_restarts_the_tally() {
        let mut tally = None;
        for holder in [":1.7", ":1.8", ":1.7", ":1.8"] {
            assert_eq!(
                record_loss(&mut tally, Some(holder), 3),
                Contention::Retry { consecutive: 1 },
                "alternating holders must each look like a first loss"
            );
        }
    }

    /// An unattributable loss must *continue* the running tally, not restart
    /// it. `GetNameOwner` routinely loses the race with a holder releasing the
    /// name, so a contended name yields an alternating `Some`/`None` sequence;
    /// treating `None` as its own holder pinned the count at 1 forever and left
    /// `permanent_after` unreachable — i.e. the original bug, reintroduced.
    /// This is a regression test for that, caught against a live broker.
    #[test]
    fn an_unknown_holder_continues_rather_than_resets_the_tally() {
        let mut tally = None;
        assert_eq!(
            record_loss(&mut tally, Some(":1.1"), 3),
            Contention::Retry { consecutive: 1 }
        );
        assert_eq!(
            record_loss(&mut tally, None, 3),
            Contention::Retry { consecutive: 2 },
            "an unattributable loss must not reset the count"
        );
        assert_eq!(
            attributed_holder(tally.as_ref()),
            ":1.1",
            "an unattributable loss must not erase who we are counting against"
        );
        assert_eq!(
            record_loss(&mut tally, Some(":1.1"), 3),
            Contention::GiveUp { consecutive: 3 }
        );
    }

    /// With no holder ever identified, the tally still escalates rather than
    /// looping forever — a broker that never answers `GetNameOwner` must not
    /// be able to keep us retrying indefinitely either.
    #[test]
    fn unknown_holders_alone_still_escalate() {
        let mut tally = None;
        assert_eq!(
            record_loss(&mut tally, None, 2),
            Contention::Retry { consecutive: 1 }
        );
        assert_eq!(
            record_loss(&mut tally, None, 2),
            Contention::GiveUp { consecutive: 2 }
        );
        assert_eq!(attributed_holder(tally.as_ref()), super::UNKNOWN_HOLDER);
    }

    /// Once latched, the tally stays latched: every later observation of the
    /// same holder gives up again immediately rather than resetting and firing
    /// a fresh burst of `permanent_after` attempts. This is what caps a
    /// permanently-camped name at one `RequestName` (and one warn) per
    /// cooldown instead of `permanent_after` of each.
    #[test]
    fn a_latched_holder_stays_latched() {
        let mut tally = None;
        assert_eq!(
            record_loss(&mut tally, Some(":1.7"), 2),
            Contention::Retry { consecutive: 1 }
        );
        assert_eq!(
            record_loss(&mut tally, Some(":1.7"), 2),
            Contention::GiveUp { consecutive: 2 }
        );
        for expected in 3..8u32 {
            assert_eq!(
                record_loss(&mut tally, Some(":1.7"), 2),
                Contention::GiveUp {
                    consecutive: expected
                },
                "a re-latch must not reset the tally"
            );
        }
    }

    /// The tally is keyed by holder, so a holder that takes over after a
    /// latch gets its own fresh budget rather than inheriting the previous
    /// one's give-up.
    #[test]
    fn a_new_holder_after_a_latch_gets_a_fresh_budget() {
        let mut tally = None;
        record_loss(&mut tally, Some(":1.7"), 2);
        assert!(matches!(
            record_loss(&mut tally, Some(":1.7"), 2),
            Contention::GiveUp { .. }
        ));
        assert_eq!(
            record_loss(&mut tally, Some(":1.9"), 2),
            Contention::Retry { consecutive: 1 }
        );
    }
}
