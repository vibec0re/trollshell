//! Primitive #1 — own a well-known D-Bus name and serve interfaces under it.
//!
//! See spec section 3.1.

use crate::BusError;
use crate::connection::SharedConnection;
use crate::error::is_transient_zbus_error;
use futures_signals::signal::Mutable;
use futures_util::{Stream, StreamExt};
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
    /// Gave up after N consecutive losses to the same owner. The supervisor
    /// re-attempts the moment `NameOwnerChanged` reports the name released,
    /// and every 5 minutes regardless as a backstop; consumers should render
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
    /// name latches [`OwnState::PermanentlyTaken`] and the retry stops being
    /// timer-driven: one attempt per observed release of the name, plus one
    /// per cooldown as a backstop.
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
    /// This is the **backstop**, not the normal recovery path: a give-up waits
    /// on `NameOwnerChanged` and re-attempts as soon as the holder releases the
    /// name, so the cooldown only ever fires when no release was observed (or
    /// one was missed). See `wait_for_release_or_cooldown`.
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
        // Subscribing here — strictly BEFORE the first `RequestName` below —
        // is also what closes the observe-then-subscribe window of #429 for
        // this primitive (that issue cites this ordering as the reference the
        // property path failed to follow). It matters twice over now that a
        // give-up waits on this stream for the name to be released: a holder
        // that exits between the `RequestName` that told us the name was taken
        // and the start of that wait has already had its `NameOwnerChanged`
        // buffered into the stream, so the wait returns on it immediately
        // instead of falling through to the 5-minute backstop. Nothing between
        // here and `run_inner_loop` may re-subscribe, or that window reopens.
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
/// **Cadence.** This fires at most once per `cooldown` — one line every 5
/// minutes by default — for as long as the name stays taken *and nothing
/// happens to it*, because `record_loss` deliberately keeps the tally latched
/// so each wake makes exactly one attempt. The event-driven wake (#669) adds
/// one further line per *observed release that we then still fail to win*,
/// which is a real ownership transition on the bus rather than a timer tick, so
/// it cannot become a flood on its own — and every observed release, win or
/// not, already got its own line from [`log_release_wake`] the instant it was
/// seen, so the overwhelmingly common case (release then win) costs exactly
/// that one line and no second give-up line. That is the middle ground between
/// the two positions this repo has
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
        "D-Bus name is held by another connection that refuses to be replaced; whatever this name backs is inert until that owner exits. Watching for that owner to release it, and re-checking periodically regardless"
    );
}

/// Log the event-driven wake from a give-up: `NameOwnerChanged` reported the
/// held name released, so we are re-requesting it without waiting out the
/// cooldown.
///
/// At `warn!`, deliberately — the same level as [`log_give_up`], so anyone
/// filtering at `warn` sees both halves of the story instead of just
/// "notifications are inert" with no line ever following up. This fires once
/// per release actually observed, not on a timer.
///
/// The message deliberately does NOT say the name was recovered: this line
/// fires the instant the release is *observed*, before the `RequestName` it
/// triggers has even been sent, let alone won. If a third contender takes the
/// now-free name first, we did not get it back, and a line claiming
/// otherwise would be lying to the journal. `holder` is likewise not
/// necessarily who just released it — it is the peer the tally was
/// attributed to at give-up time, which can predate intervening ownership
/// changes we were unable to attribute (see [`record_loss`]); it names who we
/// were waiting out, not a confirmed actor.
fn log_release_wake(name: &str, holder: &str) {
    tracing::warn!(
        %name,
        holder_at_giveup = %holder,
        "D-Bus name held by another connection was released — re-requesting now, woken by NameOwnerChanged rather than by the retry cooldown, so whatever this name backs comes back within milliseconds instead of minutes if the re-request wins"
    );
}

/// Why [`wait_for_release_or_cooldown`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    /// `NameOwnerChanged` reported the name unowned — it is free right now.
    Released,
    /// The cooldown expired with no such signal. The backstop, not the
    /// expected path.
    Cooldown,
}

/// What the inner loop should do for the next `RequestName` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NextAttempt {
    /// Ask again on the connection and subscription we already have.
    SameConnection,
    /// Return, so the outer loop reconnects with a fresh connection and a
    /// fresh `NameOwnerChanged` subscription, and re-attempts there.
    Reconnect,
}

/// Map a wake to the next move. Split out from the `select!` so the decision is
/// a pure function with its rationale attached, and testable without a broker.
///
/// The asymmetry is the point:
///
/// * A **signal** wake reuses the connection. The name is free *now* — every
///   millisecond spent tearing down and rebuilding the subscription is a
///   millisecond another contender can take it in, and the subscription is
///   demonstrably live because it just delivered. Re-subscribing would also
///   re-run the `RemoveMatch`/`AddMatch` churn `run_ownership` documents as
///   racy, for nothing.
/// * A **cooldown** wake reconnects, exactly as before #669. The timer firing
///   with no signal is also the one observation consistent with a *wedged*
///   subscription — the match rule itself failing to (re)register after a
///   broker hiccup, say. (Not a signal silently dropped from a full queue:
///   zbus 5.14 never calls `set_overflow`, and the socket reader publishes
///   with `broadcast_direct(...).await`, so a full queue on the shared
///   connection stalls the whole connection rather than discarding from it —
///   that mechanism cannot drop a `NameOwnerChanged` out from under us.)
///   Whichever way a subscription actually wedges, the backstop rebuilds the
///   thing that might have failed. That is what makes it a real backstop
///   rather than a slower copy of the fast path.
fn next_attempt_after(wake: Wake) -> NextAttempt {
    match wake {
        Wake::Released => NextAttempt::SameConnection,
        Wake::Cooldown => NextAttempt::Reconnect,
    }
}

/// Whether `msg` is a `NameOwnerChanged` saying `name` has become unowned.
///
/// `NameOwnerChanged` carries `(name, old_owner, new_owner)`; an empty
/// `new_owner` means the name was released rather than handed to someone else.
/// An ownership *transfer* is deliberately not a wake: the name is not free, so
/// re-requesting it would just earn another `Exists` — the tally has already
/// latched by the time we get here, and the new holder gets its own budget on
/// the next real attempt.
///
/// The name check is a second line of defence. The broker already filters by
/// `arg0` (see [`build_name_owner_changed_rule`]), which is what keeps six
/// concurrently-owned names from waking each other's waiters; this re-check
/// costs one string compare and means a mis-built rule degrades into a missed
/// wake rather than a wrong one.
fn is_release_of(msg: &zbus::Message, name: &str) -> bool {
    let Ok((sig_name, _old_owner, new_owner)) =
        msg.body().deserialize::<(String, String, String)>()
    else {
        return false;
    };
    sig_name == name && new_owner.is_empty()
}

/// Wait for `name` to be released, or for `cooldown` to elapse — whichever
/// happens first.
///
/// This is the whole of #669: a give-up used to `sleep(cooldown)`, so when the
/// squatter exited we noticed up to five minutes later. D-Bus already pushes
/// that event, so the timer becomes the backstop and the signal becomes the
/// fast path, and "recover instantly" stops being in tension with "do not poll
/// at 4 Hz" (#653) — the attempt rate is now bounded by *real ownership
/// transitions of this one name*, of which the broker emits exactly one per
/// change, rather than by how short a sleep we dared to use.
///
/// The timer is not optional. A signal can be missed — the match rule can
/// fail to register on a broker restart, say — and a missed signal must
/// degrade to "recovers in five minutes", never to "stranded forever".
///
/// Generic over the stream so the select and the filtering are exercised
/// hermetically, with hand-built messages and no `dbus-daemon`.
async fn wait_for_release_or_cooldown<S>(stream: &mut S, name: &str, cooldown: Duration) -> Wake
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let timer = tokio::time::sleep(cooldown);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            () = &mut timer => return Wake::Cooldown,
            msg = stream.next() => match msg {
                Some(Ok(msg)) if is_release_of(&msg, name) => return Wake::Released,
                // A signal for another name, an ownership transfer, or a body
                // we could not decode: keep waiting on the same timer.
                Some(_) => {}
                // The stream ended, so no signal can ever arrive on it. Serve
                // out the rest of the cooldown instead of spinning on a dead
                // stream (`next()` on a finished stream returns `None`
                // immediately, forever) — the caller then reconnects, which is
                // what a dead stream needs anyway.
                None => {
                    timer.as_mut().await;
                    return Wake::Cooldown;
                }
            },
        }
    }
}

/// Arguments for [`give_up_and_wait`], kept in a struct to match the local
/// convention (see [`InnerCtx`]) rather than a long positional list.
struct GiveUpCtx<'a, S> {
    stream: &'a mut S,
    name: &'a str,
    /// The holder the tally is attributed to — reported in both log lines and
    /// carried in [`OwnState::PermanentlyTaken`].
    holder: String,
    consecutive: u32,
    cooldown: Duration,
    writer: &'a Mutable<OwnState>,
}

/// Latch [`OwnState::PermanentlyTaken`] and wait for the situation to change,
/// then say how to make the next attempt.
///
/// Shared by both routes into a give-up — losing a name we held, and being
/// refused one we asked for — so the two cannot drift on the thing that matters
/// here: which wake happened and what it is worth logging.
async fn give_up_and_wait<S>(ctx: GiveUpCtx<'_, S>) -> NextAttempt
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let GiveUpCtx {
        stream,
        name,
        holder,
        consecutive,
        cooldown,
        writer,
    } = ctx;

    log_give_up(name, &holder, consecutive, cooldown);
    writer.set(OwnState::PermanentlyTaken {
        current_owner: holder.clone(),
    });

    let wake = wait_for_release_or_cooldown(stream, name, cooldown).await;
    if wake == Wake::Released {
        log_release_wake(name, &holder);
    }
    // The tally is deliberately NOT cleared here (#668): a re-acquisition does
    // not earn a fresh `permanent_after` budget against the same peer. Staying
    // latched is cheap now that the latch is event-driven — it costs one
    // attempt per release, not one per 250 ms.
    writer.set(OwnState::Acquiring);
    next_attempt_after(wake)
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

/// Arguments for [`on_name_held`], kept in a struct to match the local
/// convention (see [`InnerCtx`]) rather than a long positional list.
struct HeldCtx<'a, S> {
    stream: &'a mut S,
    name: &'a str,
    /// Our own unique name, so a `NameOwnerChanged` can be told apart from the
    /// buffered ones that predate our acquisition.
    unique: Option<&'a str>,
    permanent_after: u32,
    cooldown: Duration,
    writer: &'a Mutable<OwnState>,
    tally: &'a mut Option<(String, u32)>,
}

/// Hold the name until we are displaced, then decide how to get it back.
///
/// Extracted from `run_inner_loop`'s `PrimaryOwner`/`AlreadyOwner` arm so it
/// sits alongside [`on_name_taken`] — the two arms of the same decision (we
/// have the name and lost it; we asked and were refused) now read the same way
/// and share [`give_up_and_wait`].
///
/// Returns how the caller should make the next attempt.
async fn on_name_held<S>(ctx: HeldCtx<'_, S>) -> NextAttempt
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let HeldCtx {
        stream,
        name,
        unique,
        permanent_after,
        cooldown,
        writer,
        tally,
    } = ctx;

    writer.set(OwnState::Owned);

    // Drain any buffered NameOwnerChanged signals that arrived before we set
    // Owned, then block until we are displaced.
    let new_owner = watch_for_loss(stream, name, unique).await;

    writer.set(OwnState::Lost {
        transient: new_owner.is_none(),
        prev_owner: new_owner.clone(),
    });

    let Some(holder) = new_owner else {
        // Transient loss (bus blip / stream ended) — nobody took the name from
        // us, so the tally resets and we reconnect. Logged at debug because
        // `connection.rs` already warns about the disconnect itself; this is
        // its consequence.
        tracing::debug!(%name, "D-Bus name dropped with the connection; reconnecting");
        *tally = None;
        writer.set(OwnState::Acquiring);
        return NextAttempt::Reconnect;
    };

    match record_loss(tally, Some(&holder), permanent_after) {
        Contention::Retry { consecutive } => {
            tracing::warn!(
                %name,
                %holder,
                consecutive,
                permanent_after,
                "lost D-Bus name to another connection; re-requesting it"
            );
            // Retry RequestName on the same connection + subscription.
            writer.set(OwnState::Acquiring);
            NextAttempt::SameConnection
        }
        Contention::GiveUp { consecutive } => {
            give_up_and_wait(GiveUpCtx {
                stream,
                name,
                holder,
                consecutive,
                cooldown,
                writer,
            })
            .await
        }
    }
}

/// Arguments for [`on_name_taken`], kept in a struct to match the local
/// convention (see [`InnerCtx`]) rather than a long positional list.
struct TakenCtx<'a, S> {
    dbus: &'a fdo::DBusProxy<'a>,
    /// The live `NameOwnerChanged` subscription, subscribed before the
    /// `RequestName` that got us here — handed down so a give-up can wait on
    /// the holder releasing the name instead of sleeping out the cooldown.
    stream: &'a mut S,
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
/// Returns how the caller should make the next attempt.
async fn on_name_taken<S>(ctx: TakenCtx<'_, S>) -> NextAttempt
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let TakenCtx {
        dbus,
        stream,
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
            NextAttempt::SameConnection
        }
        Contention::GiveUp { consecutive } => {
            give_up_and_wait(GiveUpCtx {
                stream,
                name,
                holder,
                consecutive,
                cooldown,
                writer,
            })
            .await
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
                let next = on_name_held(HeldCtx {
                    stream: &mut *stream,
                    name,
                    unique,
                    permanent_after,
                    cooldown,
                    writer,
                    tally: &mut *consecutive_losses_to,
                })
                .await;
                if next == NextAttempt::Reconnect {
                    return;
                }
            }
            fdo::RequestNameReply::Exists | fdo::RequestNameReply::InQueue => {
                let next = on_name_taken(TakenCtx {
                    dbus: &dbus,
                    stream: &mut *stream,
                    name,
                    permanent_after,
                    cooldown,
                    writer,
                    tally: &mut *consecutive_losses_to,
                })
                .await;
                if next == NextAttempt::Reconnect {
                    return;
                }
            }
        }
    }
}

/// Build the `NameOwnerChanged` match rule for the named service (arg0 filter).
///
/// The `arg0` filter is what makes this subscription *this name's*. Six
/// well-known names are owned concurrently in the shell (`notifications`, the
/// tray, `DisplayConfig`, the screensaver, bluetooth, `Control`), each with its
/// own instance of this task; without the filter, every release on the bus
/// would wake all six waiters (and the broker would ship us every ownership
/// change on the session bus to boot).
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
async fn watch_for_loss<S>(stream: &mut S, name: &str, unique: Option<&str>) -> Option<String>
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
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
    use super::{
        Contention, NextAttempt, OwnState, Wake, attributed_holder, is_release_of,
        next_attempt_after, on_request_name_error, record_loss, wait_for_release_or_cooldown,
    };
    use futures_signals::signal::{Mutable, SignalExt as _};
    use futures_util::StreamExt as _;
    use std::time::Duration;
    use zbus::fdo;

    /// The name under test. Two distinct names are used throughout so the
    /// filtering assertions cannot pass by accident.
    const NAME: &str = "mov.vibec0re.test.wanted";
    const OTHER: &str = "mov.vibec0re.test.someone-else";

    /// Cooldown for the tests that mean to *reach* the backstop. Short enough
    /// that serving it out costs nothing, long enough that the timer arm cannot
    /// win a race it should lose.
    const SHORT_COOLDOWN: Duration = Duration::from_millis(60);

    /// Cooldown for the tests that must NOT reach the backstop. Not virtual
    /// time — `tokio`'s `test-util` feature is not enabled here — so this is
    /// sized as a trap instead: the streams are in-memory and resolve in
    /// microseconds, so a regression that falls through to the timer is caught
    /// by [`FAST_PATH_BUDGET`] with three orders of magnitude of margin, and
    /// fails in 30 s rather than hanging.
    const LONG_COOLDOWN: Duration = Duration::from_secs(30);

    /// How long an in-memory stream is allowed to take to deliver a wake before
    /// we call it "waited on the cooldown".
    const FAST_PATH_BUDGET: Duration = Duration::from_secs(5);

    /// A hand-built `NameOwnerChanged` — body `(name, old_owner, new_owner)`,
    /// with an empty `new_owner` meaning "released". No broker involved.
    fn name_owner_changed(name: &str, old_owner: &str, new_owner: &str) -> zbus::Message {
        zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("NameOwnerChanged signal builder")
        .build(&(name, old_owner, new_owner))
        .expect("build NameOwnerChanged")
    }

    fn feed(
        msgs: Vec<zbus::Message>,
    ) -> impl futures_util::Stream<Item = zbus::Result<zbus::Message>> + Unpin {
        futures_util::stream::iter(msgs.into_iter().map(Ok))
    }

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

    /// The fast path (#669): a `NameOwnerChanged` saying the name we want is
    /// now unowned ends the wait at once, without the cooldown elapsing.
    #[tokio::test]
    async fn a_release_of_the_wanted_name_ends_the_wait_at_once() {
        let started = std::time::Instant::now();
        let mut stream = feed(vec![name_owner_changed(NAME, ":1.6", "")]);
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, LONG_COOLDOWN).await,
            Wake::Released
        );
        assert!(
            started.elapsed() < FAST_PATH_BUDGET,
            "the release must be acted on immediately, not after the cooldown"
        );
    }

    /// Requirement 3 of #669: all six owned names share this path, so the wait
    /// must key on *its* name. A release of somebody else's name — and every
    /// other kind of `NameOwnerChanged` traffic — must be stepped over, not
    /// treated as our wake.
    #[tokio::test]
    async fn only_a_release_of_our_own_name_wakes_us() {
        let mut stream = feed(vec![
            // Someone else's name being acquired…
            name_owner_changed(OTHER, "", ":1.5"),
            // …and someone else's name being *released*: the exact shape of
            // our wake, on the wrong name.
            name_owner_changed(OTHER, ":1.5", ""),
            // Our name changing hands is not our name becoming free.
            name_owner_changed(NAME, ":1.6", ":1.7"),
            // Finally, ours.
            name_owner_changed(NAME, ":1.7", ""),
        ]);
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, LONG_COOLDOWN).await,
            Wake::Released
        );
    }

    /// An ownership *transfer* is not a release: the name is not free, so
    /// waking on it would only earn another `Exists`. With nothing else to
    /// come, the wait falls through to the backstop.
    #[tokio::test]
    async fn an_ownership_transfer_is_not_a_release() {
        let mut stream = feed(vec![name_owner_changed(NAME, ":1.6", ":1.7")]);
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, SHORT_COOLDOWN).await,
            Wake::Cooldown
        );
    }

    /// The backstop, on the arm that matters most: a subscription that never
    /// delivers (a missed signal, a dropped match rule) must strand us for one
    /// cooldown, not forever. Asserts the full cooldown was served — a wait
    /// that returned `Cooldown` early would be a busy-poll wearing the right
    /// label.
    #[tokio::test]
    async fn a_silent_subscription_falls_through_to_the_cooldown_backstop() {
        let started = std::time::Instant::now();
        let mut stream = futures_util::stream::pending::<zbus::Result<zbus::Message>>();
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, SHORT_COOLDOWN).await,
            Wake::Cooldown
        );
        assert!(started.elapsed() >= SHORT_COOLDOWN);
    }

    /// A stream that has *ended* (the connection went away) still serves out
    /// the cooldown rather than returning instantly — `next()` on a finished
    /// stream yields `None` forever, so an early return here would spin the
    /// retry loop as fast as the CPU allows.
    #[tokio::test]
    async fn an_ended_stream_still_serves_out_the_cooldown() {
        let started = std::time::Instant::now();
        let mut stream = feed(Vec::new());
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, SHORT_COOLDOWN).await,
            Wake::Cooldown
        );
        assert!(started.elapsed() >= SHORT_COOLDOWN);
    }

    /// A `NameOwnerChanged` whose body will not decode as `(s, s, s)` is
    /// ignored rather than trusted or panicked on.
    #[test]
    fn an_undecodable_body_is_not_a_release() {
        let msg = zbus::Message::signal(
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .expect("signal builder")
        .build(&42u32)
        .expect("build malformed NameOwnerChanged");
        assert!(!is_release_of(&msg, NAME));
    }

    /// The select-arm decision, pinned: the two wakes take deliberately
    /// different next moves — the signal path reuses the live subscription it
    /// was just woken by, the backstop rebuilds the subscription that may be
    /// the reason it had to fire.
    #[test]
    fn the_wake_decides_whether_the_subscription_is_reused() {
        assert_eq!(
            next_attempt_after(Wake::Released),
            NextAttempt::SameConnection,
            "a release must be acted on without a resubscribe round-trip"
        );
        assert_eq!(
            next_attempt_after(Wake::Cooldown),
            NextAttempt::Reconnect,
            "the backstop must rebuild the connection and subscription"
        );
    }
}
