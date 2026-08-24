//! Primitive #1 — own a well-known D-Bus name and serve interfaces under it.
//!
//! See spec section 3.1.

use crate::BusError;
use crate::connection::SharedConnection;
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

/// First delay of the acquisition-error ramp (see [`acquire_backoff`]).
const ACQUIRE_BACKOFF_BASE: Duration = Duration::from_millis(250);

/// Ceiling of that ramp. Deliberately seconds, not minutes: this path has no
/// event to wake on — a broken bus emits no `NameOwnerChanged` — so the cap is
/// also the worst-case time to notice the bus came back. Thirty seconds is
/// three orders of magnitude cheaper than the flat 250 ms it replaces while
/// still recovering well inside a human's attention span.
const ACQUIRE_BACKOFF_CAP: Duration = Duration::from_secs(30);

/// A failure streak shorter than this is still consistent with "the bus
/// blipped", which `connection.rs` already warns about; from here on it is
/// consistent only with "this is not working", which nothing else reports.
/// At the [`acquire_backoff`] ramp this is ~4 s of consecutive failures.
const STREAK_IS_NO_LONGER_A_BLIP: u32 = 5;

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

/// Delay before the `attempt`-th consecutive *acquisition error* retry
/// (1-based): the connection or subscription could not be built, the
/// `DBusProxy` could not be constructed, or `RequestName` itself errored.
///
/// 250 ms doubling, capped at [`ACQUIRE_BACKOFF_CAP`]: 250 ms, 500 ms, 1 s,
/// 2 s, 4 s, 8 s, 16 s, then 30 s for as long as it keeps failing.
///
/// The two constants deliberately match `connection.rs`'s private `Backoff`
/// (250 ms → 30 s), which supervises reconnects one layer down: two retry
/// ladders stacked on the same connection should not disagree about how long a
/// bus outage is worth waiting out. They are *not* shared code — that one is
/// indexed by duration and has no attempt counter, so it cannot drive
/// [`logs_at`] — and unifying them is a job for the shared-backoff-helper
/// cleanup in #646 rather than for this fix.
///
/// This is deliberately **not** the contention path. Losing a race for a name
/// somebody else holds is bounded by `permanent_after` and then by an
/// event-driven wait on `NameOwnerChanged` (see
/// [`wait_for_release_or_cooldown`]); it needs no timer ramp because the broker
/// tells us when the situation changes. An *error* has no such event — a bus
/// that is down publishes nothing — so it is the one place left in this file
/// where the retry rate is set by us alone, and before this it was a flat
/// 250 ms forever, which is the 4 Hz spin of #653 surviving in the arm nobody
/// looked at.
fn acquire_backoff(attempt: u32) -> Duration {
    // `min(16)` keeps the shift in range; 250 ms << 16 is ~4.5 h, already far
    // past the cap, so clamping there cannot change the answer.
    let doublings = attempt.saturating_sub(1).min(16);
    ACQUIRE_BACKOFF_BASE
        .saturating_mul(1u32 << doublings)
        .min(ACQUIRE_BACKOFF_CAP)
}

/// Whether the `attempt`-th consecutive failure earns a log line (1-based).
///
/// True at every doubling — 1, 2, 4, 8, 16, … — and nowhere else. Pairing a
/// geometric log cadence with the geometric delay of [`acquire_backoff`] is
/// what gives the *logging* a ceiling as well as the retry: a bus that is down
/// for a day costs ~11 lines rather than one per attempt. Without this the
/// non-transient arm emitted a `warn!` on every retry forever, which is the
/// uncapped-retry-logging #646 objects to, and the transient arm emitted
/// nothing at all, which is the silence #653 objects to.
fn logs_at(attempt: u32) -> bool {
    attempt.is_power_of_two()
}

/// How to react to one failure in a streak: how long to wait, and whether to
/// say anything about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RetryStep {
    /// 1-based position in the current streak.
    attempt: u32,
    /// How long to wait before the next attempt.
    delay: Duration,
    /// Whether this failure earns a log line (see [`logs_at`]).
    log: bool,
}

impl RetryStep {
    /// Whether a streak this long has stopped being explicable as a bus blip.
    ///
    /// Below the line it gets `debug!` — `connection.rs` already warns about
    /// the disconnect itself and a single failure here is only its echo. At or
    /// above it, the name is not owned and is not about to be, which is the
    /// same "this feature is inert" report [`log_give_up`] makes, so it takes
    /// the same level for the same reason (see that function's doc: the
    /// deployed shell's filter drops everything below `error!`).
    fn is_serious(self) -> bool {
        self.attempt >= STREAK_IS_NO_LONGER_A_BLIP
    }
}

/// A run of consecutive failures on the **acquisition** path.
///
/// Lives in `run_ownership` and therefore spans reconnects: a bus that refuses
/// to connect must not reset its own ramp just because the outer loop went
/// round again. It is cleared the moment `RequestName` produces a *reply* of
/// any kind — including "somebody else has it", which proves the broker is
/// answering and hands the situation over to the contention accounting. That
/// reset is what keeps the ramp from turning a busy-loop into a stall: a blip
/// costs at most one extra 250 ms, never the 30 s cap.
#[derive(Debug, Default)]
struct FailureStreak {
    attempts: u32,
}

impl FailureStreak {
    /// Record one failure and say how to back off from it.
    fn record(&mut self) -> RetryStep {
        self.attempts = self.attempts.saturating_add(1);
        RetryStep {
            attempt: self.attempts,
            delay: acquire_backoff(self.attempts),
            log: logs_at(self.attempts),
        }
    }

    /// Forget the streak — something worked.
    fn reset(&mut self) {
        self.attempts = 0;
    }
}

/// Emit the log line for one acquisition failure, at the level its streak has
/// earned (see [`RetryStep::is_serious`]). Shared by all four error arms so they
/// cannot drift on level, cadence, or wording.
fn log_acquire_failure(name: &str, stage: &str, error: &dyn std::fmt::Display, backoff: RetryStep) {
    if !backoff.log {
        return;
    }
    if backoff.is_serious() {
        tracing::error!(
            %name,
            %stage,
            %error,
            attempt = backoff.attempt,
            retry_in = ?backoff.delay,
            "cannot acquire D-Bus name; whatever this name backs stays inert until it succeeds"
        );
    } else {
        tracing::debug!(
            %name,
            %stage,
            %error,
            attempt = backoff.attempt,
            retry_in = ?backoff.delay,
            "cannot acquire D-Bus name yet; retrying"
        );
    }
}

/// Why [`run_inner_loop`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopExit {
    /// Drop this connection, build a fresh one, and try again.
    Reconnect,
    /// Stop the ownership task entirely — retrying cannot ever help.
    Fatal,
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
    // Consecutive *errors* on the way to a `RequestName` reply. Distinct from
    // `consecutive_losses_to`, which counts the times we got an answer and the
    // answer was "no". See `FailureStreak`.
    let mut acquire_failures = FailureStreak::default();

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
            // Transient and permanent used to differ here (a silent 250 ms vs
            // a `warn!` every 5 s). Neither had a ceiling, and the *shape* of
            // the failure matters far less than how long it has been going on,
            // which is what `FailureStreak` now decides — so both arms take the
            // same ramp and differ only in the `stage` they report.
            Err(e) => {
                let stage = if e.is_transient() {
                    "connect"
                } else {
                    "subscribe to NameOwnerChanged"
                };
                let backoff = acquire_failures.record();
                log_acquire_failure(&name, stage, &e, backoff);
                writer.set(OwnState::Acquiring);
                tokio::time::sleep(backoff.delay).await;
                continue;
            }
        };

        let unique = conn.unique_name().map(|u| u.as_str().to_string());

        // ── Inner retry loop: reuse the same connection + subscription ────────
        let exit = run_inner_loop(InnerCtx {
            conn: &conn,
            stream: &mut stream,
            name: &name,
            unique: unique.as_deref(),
            permanent_after,
            cooldown,
            writer: &writer,
            consecutive_losses_to: &mut consecutive_losses_to,
            acquire_failures: &mut acquire_failures,
        })
        .await;
        if exit == LoopExit::Fatal {
            return;
        }
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
    /// Consecutive failures to reach a `RequestName` reply at all, carried in
    /// from `run_ownership` so the ramp survives a reconnect.
    acquire_failures: &'a mut FailureStreak,
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
/// **Level: `error!`, and that is the whole point of #653's first half.** The
/// deployed shell calls `tracing_subscriber::fmt::init()` with no `RUST_LOG` in
/// `etc/systemd/user/trollshell.service`, and `fmt::init()` builds
/// `EnvFilter::from_default_env()`, whose default directive is
/// `LevelFilter::ERROR`. A `warn!` here — which is what #668 shipped — is
/// therefore *dropped before it reaches the journal* on every real install.
/// #653 says a contested name "produces no user-visible signal at all"; the
/// signal existed, at a level the binary does not print. `error!` is also the
/// honest level: past `permanent_after` this is not a warning about something
/// that might resolve, it is a report that a whole subsystem (notifications,
/// the tray, `DisplayConfig`, the screensaver, the `Control` endpoint) is dead
/// until a human removes the other owner.
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
    tracing::error!(
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
/// At the **same level as [`log_give_up`]**, deliberately, so anyone whose
/// filter shows the give-up sees both halves of the story instead of just
/// "notifications are inert" with no line ever following up. That invariant is
/// what moved this to `error!` alongside it: leaving it at `warn!` while the
/// give-up went to `error!` would have made the default `RUST_LOG`-less filter
/// show every death and no recovery — strictly worse than either level chosen
/// consistently. This fires once per release actually observed, not on a timer.
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
    tracing::error!(
        %name,
        holder_at_giveup = %holder,
        "D-Bus name held by another connection was released — re-requesting now, woken by NameOwnerChanged rather than by the retry cooldown, so whatever this name backs comes back within milliseconds instead of minutes if the re-request wins"
    );
}

/// Whether taking the name right now *closes* a give-up incident — i.e. the
/// tally is currently latched at or past `permanent_after`, so a
/// [`log_give_up`] line is already in the journal claiming this name's
/// subsystem is inert.
///
/// Split out as a pure function because it is the whole condition for the
/// recovery line and the only part of it worth testing without a broker.
fn closes_a_give_up(tally: Option<&(String, u32)>, permanent_after: u32) -> bool {
    tally.is_some_and(|(_, consecutive)| *consecutive >= permanent_after)
}

/// Log that a name we had given up on is *actually* ours again.
///
/// #720 removed a "RECOVERED" line from [`log_release_wake`] because it fired
/// when the release was merely *observed*, before the `RequestName` it triggers
/// had been sent, let alone won — it could and did claim a recovery that a
/// third contender then stole. That was the right removal, but it left the
/// incident permanently open: the journal said the subsystem was dead and never
/// said otherwise. This line makes the claim at the only moment it is true —
/// after `RequestName` returned `PrimaryOwner`/`AlreadyOwner` — and only when
/// there is an incident to close ([`closes_a_give_up`]), so a first acquisition
/// at startup and an ordinary sub-threshold retry stay silent.
///
/// Same level as [`log_give_up`], for the same reason [`log_release_wake`] is.
/// Its cadence is bounded the same way too: one line per *real* ownership
/// transition of this one name, of which the broker emits exactly one per
/// change.
fn log_recovered(name: &str, holder: &str, consecutive: u32) {
    tracing::error!(
        %name,
        holder_at_giveup = %holder,
        consecutive,
        "D-Bus name reacquired — whatever this name backs is live again; the earlier report that it was inert is now closed"
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
/// **That bound is peer-imposed, and #668's was not.** #668 shipped a ceiling
/// we set ourselves: one attempt per `cooldown` — twelve an hour, and twelve
/// log lines with them — whatever the rest of the bus did. Waking on the signal
/// trades it for a bound the *contender* sets. A peer that flaps the name
/// (takes it, releases it, repeats) costs one `RequestName` and two or three
/// lines per flap ([`log_release_wake`] on the observed release, then
/// [`log_recovered`] or [`log_give_up`] on the outcome, plus [`log_give_up`]
/// again when it takes the name back), and nothing in this file bounds the flap
/// rate. Note that [`logs_at`]'s geometric cadence does **not** cover this: that
/// ramp belongs to the acquisition-*error* streak, which a contended name resets
/// on every `RequestName` reply.
///
/// The trade is still worth it, and the new bound is a real one — a *static*
/// squatter, which is every case this primitive was written for, still costs
/// exactly one attempt and one line per cooldown, and no peer can push the rate
/// above the bus's own count of genuine ownership changes. But the guarantee
/// changed shape, from "we promise twelve an hour" to "we promise one per real
/// transition", and that is worth writing down (#688 item 2). Recorded rather
/// than rate-limited on purpose: a token bucket over the recovery line would
/// suppress exactly the transitions worth reading, and the only workload that
/// can trip it is a peer already misbehaving loudly enough to be the thing you
/// opened the journal to find.
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

/// Sleep for `dur` while keeping `stream` drained, discarding whatever arrives.
///
/// An unpolled `MessageStream` is not merely stale, it is back-pressure. zbus
/// 5.14 builds each match-rule subscription as a bounded `async_broadcast`
/// channel (`Connection::add_match` → `broadcast(max_queued)`, defaulting to
/// `DEFAULT_MAX_QUEUED` = 64) and the socket reader publishes into it with
/// `broadcast_direct(...).await`, with `set_overflow` never called anywhere in
/// the crate. A subscription whose queue fills therefore **blocks the socket
/// reader** — the single task feeding every other stream and every method reply
/// on that connection — rather than dropping anything. It is the same mechanism
/// [`next_attempt_after`] cites as the reason a `NameOwnerChanged` cannot be
/// silently lost: nothing is lost precisely because everything stalls instead.
/// Both readings were verified against the pinned zbus 5.14.0 source.
///
/// This is held as a flat invariant — *no wait in this file leaves a live
/// subscription unpolled* — rather than argued from a motivating scenario,
/// because the scenarios are all weak. The `arg0` filter
/// ([`build_name_owner_changed_rule`]) keeps everything but this one name's
/// ownership changes out of the queue, so filling 64 slots inside a single
/// window is remote to begin with; and the [`OwnState::Denied`] window that
/// prompted the fix, though it is the longest in the file, is if anything the
/// *quietest* one — a name the broker's policy refuses us (the bluez and iwd
/// agent names under `enableRecommendedServices = false`) is typically a name
/// nobody owns, so there are no ownership changes of it to emit. The invariant
/// earns its keep without any of that: it costs one `select!` arm, it does not
/// need re-deriving every time the traffic estimate changes, and what it
/// forecloses is a stall of the *system* connection every other service shares
/// (#688 item 4).
///
/// Discarding is correct **only because every caller reconnects immediately
/// afterwards**, dropping this stream and building a fresh subscription, so
/// nothing discarded here was going to be read.
///
/// [`on_name_taken`]'s contention retry keeps its bare
/// `sleep(RETRY_AFTER_LOSS * consecutive)` — 250 ms, then 500 ms — and the
/// reason is simply that length: three orders of magnitude short of the traffic
/// it would take to fill 64 slots, so the hazard cannot arise there. It is
/// **not** that a release buffered during that sleep is load-bearing. It cannot
/// be: that retry always sends another `RequestName` before any wait, and
/// reaching [`wait_for_release_or_cooldown`] at all requires that request to
/// have come back `Exists` — the name taken again — so a release buffered
/// during the sleep is stale by the time the wait sees it, and
/// [`is_release_of`] has no recency check with which to notice. It would return
/// a spurious `Wake::Released` and short-circuit the cooldown. In the other
/// outcome, where the re-request wins, [`watch_for_loss`] steps over that same
/// message (its `old_owner` is not ours) and it is dropped there. The window
/// `run_ownership`'s subscription comment actually protects is the one with
/// *no* sleep in it: the method-call gap from a `RequestName` reply of `Exists`,
/// through [`current_holder_of`], to the start of the wait.
async fn sleep_draining<S>(stream: &mut S, dur: Duration)
where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let timer = tokio::time::sleep(dur);
    tokio::pin!(timer);
    loop {
        tokio::select! {
            () = &mut timer => return,
            // A message is read and dropped on purpose — see above. `None`
            // means the stream ended, so nothing more can fill its queue:
            // serve out the rest of the wait rather than spinning on a
            // finished stream (`next()` returns `None` immediately, forever) —
            // the same guard `wait_for_release_or_cooldown` needs, for the
            // same reason.
            msg = stream.next() => if msg.is_none() {
                timer.as_mut().await;
                return;
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

    // Close the incident before announcing `Owned`: if a `log_give_up` line is
    // outstanding for this name, the journal currently says this subsystem is
    // dead, and it is not. See `log_recovered` for why this is here rather than
    // on the release-observed path #720 took it off.
    if closes_a_give_up(tally.as_ref(), permanent_after)
        && let Some((holder, consecutive)) = tally.as_ref()
    {
        log_recovered(name, holder, *consecutive);
    }

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
/// retry would cause, so `AccessDenied` keeps `cooldown` as its delay rather
/// than joining the [`acquire_backoff`] ramp, which would *shorten* it from 5
/// minutes to 30 seconds.
///
/// Its **level** stays `info!` for the same reason: `enableRecommendedServices
/// = false` is a supported configuration under which the bluez and iwd agent
/// names are expected to be denied (CLAUDE.md documents exactly that), so this
/// is a deliberate config outcome, not a fault. Only its *cadence* changes:
/// routed through the streak, it stops repeating once per cooldown forever.
///
/// Both waits here go through [`sleep_draining`] rather than a bare sleep: the
/// `NameOwnerChanged` subscription is live for their whole duration, and the
/// `Denied` one is the longest window in the file. See that function for why an
/// unpolled subscription is a hazard to the shared connection and why
/// discarding is safe on this path specifically.
async fn on_request_name_error<S>(
    e: fdo::Error,
    name: &str,
    cooldown: Duration,
    writer: &Mutable<OwnState>,
    failures: &mut FailureStreak,
    stream: &mut S,
) where
    S: Stream<Item = zbus::Result<zbus::Message>> + Unpin,
{
    let backoff = failures.record();
    if matches!(e, fdo::Error::AccessDenied(_)) {
        if backoff.log {
            tracing::info!(
                %name,
                attempt = backoff.attempt,
                retry_in_secs = cooldown.as_secs(),
                "DBus name ownership refused by policy; service inert (install a /etc/dbus-1/system.d/ rule granting it); will retry"
            );
        }
        writer.set(OwnState::Denied);
        sleep_draining(stream, cooldown).await;
        writer.set(OwnState::Acquiring);
        return;
    }
    let as_zbus = zbus::Error::FDO(Box::new(e));
    log_acquire_failure(name, "RequestName", &as_zbus, backoff);
    writer.set(OwnState::Acquiring);
    sleep_draining(stream, backoff.delay).await;
}

/// Inner retry loop: reuse one connection and one `NameOwnerChanged`
/// subscription across multiple `RequestName` attempts.
///
/// Returns when the connection should be dropped and re-established, or
/// [`LoopExit::Fatal`] when no amount of retrying can help.
async fn run_inner_loop(ctx: InnerCtx<'_>) -> LoopExit {
    let InnerCtx {
        conn,
        stream,
        name,
        unique,
        permanent_after,
        cooldown,
        writer,
        consecutive_losses_to,
        acquire_failures,
    } = ctx;
    loop {
        let dbus = match fdo::DBusProxy::new(conn).await {
            Ok(dbus) => dbus,
            Err(e) => {
                // DBusProxy construction failures are transient; reconnect.
                let backoff = acquire_failures.record();
                log_acquire_failure(name, "build DBusProxy", &e, backoff);
                writer.set(OwnState::Acquiring);
                sleep_draining(&mut *stream, backoff.delay).await;
                return LoopExit::Reconnect;
            }
        };

        let well_known = match name
            .try_into()
            .map_err(|e: zbus::names::Error| zbus::Error::Failure(e.to_string()))
        {
            Ok(w) => w,
            Err(e) => {
                // Not retryable, ever: `name` is fixed for the life of this
                // task and this is pure string validation, so the next thousand
                // attempts fail identically. It used to sleep a minute and
                // return, which had the outer loop rebuild a connection and
                // re-log this same `error!` every 60 s for the process
                // lifetime — an uncapped log storm over a caller bug. Say it
                // once, loudly, and stop.
                tracing::error!(
                    error = %e,
                    %name,
                    "invalid well-known D-Bus name; giving up on owning it — whatever this name backs will stay inert for the life of the process"
                );
                return LoopExit::Fatal;
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
            // Any reply at all — including "somebody else has it" — proves the
            // broker is answering, so the acquisition-error streak is over and
            // the situation belongs to the contention accounting from here.
            // Resetting here rather than on `PrimaryOwner` is what stops a
            // contended name from also climbing the error ramp and inheriting a
            // 30 s floor it does not need.
            Ok(r) => {
                acquire_failures.reset();
                r
            }
            Err(e) => {
                on_request_name_error(e, name, cooldown, writer, acquire_failures, &mut *stream)
                    .await;
                // Return to reconnect with a fresh connection + subscription
                // and re-attempt RequestName.
                return LoopExit::Reconnect;
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
                    return LoopExit::Reconnect;
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
                    return LoopExit::Reconnect;
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
        ACQUIRE_BACKOFF_BASE, ACQUIRE_BACKOFF_CAP, Contention, FailureStreak, NextAttempt,
        OwnState, Wake, acquire_backoff, attributed_holder, closes_a_give_up, is_release_of,
        logs_at, next_attempt_after, on_request_name_error, record_loss, sleep_draining,
        wait_for_release_or_cooldown,
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
    /// sized as a trap instead: a regression that falls through to the timer
    /// returns `Wake::Cooldown` and fails its assertion in 30 s rather than
    /// hanging.
    const LONG_COOLDOWN: Duration = Duration::from_secs(30);

    /// The wait under test in the [`sleep_draining`] tests. Long enough that a
    /// whole [`trickle`] sequence lands inside it with an order of magnitude to
    /// spare, short enough not to be felt in the suite.
    const DRAIN_WINDOW: Duration = Duration::from_millis(300);

    /// Spacing between [`trickle`]'s messages: three of them land at 20/40/60 ms
    /// inside [`DRAIN_WINDOW`].
    const TRICKLE_GAP: Duration = Duration::from_millis(20);

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

    /// A boxed message stream, so [`trickle`] can name a concrete `Unpin` type.
    type BoxedMsgStream =
        std::pin::Pin<Box<dyn futures_util::Stream<Item = zbus::Result<zbus::Message>>>>;

    /// Like [`feed`], but the messages arrive *over time* — one every `gap`,
    /// then the stream ends.
    ///
    /// [`feed`]'s `stream::iter` cannot tell a real drain from a
    /// drain-once-then-`sleep(dur)`: it is ready on the first poll and finished
    /// on the next, so both implementations empty it before the timer is ever
    /// awaited, and both leave it exhausted. Only a stream that produces
    /// something *after* the first poll separates the two, which is why the
    /// [`sleep_draining`] tests use this and not `feed` (#688 review).
    fn trickle(msgs: Vec<zbus::Message>, gap: Duration) -> BoxedMsgStream {
        Box::pin(async_stream::stream! {
            for msg in msgs {
                tokio::time::sleep(gap).await;
                yield Ok(msg);
            }
        })
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
            let mut failures = FailureStreak::default();
            let mut subscription = futures_util::stream::pending::<zbus::Result<zbus::Message>>();
            on_request_name_error(
                fdo::Error::AccessDenied("policy refuses ownership".into()),
                "mov.vibec0re.test.denied",
                // The cooldown separates the two transitions enough that the
                // signal stream observes `Denied` before `Acquiring` (no
                // latest-value coalescing).
                Duration::from_millis(150),
                &w,
                &mut failures,
                &mut subscription,
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
    ///
    /// The returned [`Wake`] is the whole assertion. `Released` is reachable
    /// only from the stream arm of the `select!`, so it *is* the proof that the
    /// 30 s timer never won; the elapsed-time check this test carried until
    /// #688 could not fail for any reason the `Wake` does not already catch,
    /// and — being an *upper* bound, unlike the two that survive elsewhere in
    /// this module — was the one shape of timing assertion a loaded machine can
    /// turn red while the property still holds.
    #[tokio::test]
    async fn a_release_of_the_wanted_name_ends_the_wait_at_once() {
        let mut stream = feed(vec![name_owner_changed(NAME, ":1.6", "")]);
        assert_eq!(
            wait_for_release_or_cooldown(&mut stream, NAME, LONG_COOLDOWN).await,
            Wake::Released
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

    // ── #688 item 4: no unpolled subscription on the error path ─────────────

    /// A draining wait must keep consuming for the *whole* wait, and still
    /// serve its full duration. An implementation that stopped reading would
    /// let the subscription's 64-slot broadcast queue fill and block zbus's
    /// socket reader — see [`sleep_draining`] — and one that returned when the
    /// stream ended would turn the wait into a spin.
    ///
    /// The stream [`trickle`]s on purpose. A backlog that is all present up
    /// front is drained by `drain-once-then-sleep(dur)` just as completely as
    /// by the real thing; messages that arrive at 20/40/60 ms into a 300 ms
    /// wait are not, and leave that implementation holding the last two.
    #[tokio::test]
    async fn a_draining_wait_consumes_the_backlog_and_still_serves_its_time() {
        let mut stream = trickle(
            vec![
                name_owner_changed(NAME, ":1.6", ""),
                name_owner_changed(OTHER, ":1.5", ""),
                name_owner_changed(NAME, "", ":1.7"),
            ],
            TRICKLE_GAP,
        );
        let started = std::time::Instant::now();
        sleep_draining(&mut stream, DRAIN_WINDOW).await;
        assert!(
            started.elapsed() >= DRAIN_WINDOW,
            "the wait must serve its full duration, not end when the stream does"
        );
        assert!(
            stream.next().await.is_none(),
            "every message that arrived during the wait must have been consumed"
        );
    }

    /// The `Denied` wait is the longest window in this file — a whole
    /// `cooldown` — so it is the one that must not leave the live
    /// `NameOwnerChanged` subscription unpolled. Same [`trickle`]d stream as
    /// above, for the same reason: a drain-once implementation has to fail
    /// here too, not just on the helper in isolation.
    #[tokio::test]
    async fn the_denied_wait_keeps_the_subscription_drained() {
        let writer = Mutable::new(OwnState::Acquiring);
        let mut failures = FailureStreak::default();
        let mut stream = trickle(
            vec![
                name_owner_changed(NAME, ":1.6", ""),
                name_owner_changed(OTHER, ":1.5", ""),
            ],
            TRICKLE_GAP,
        );
        on_request_name_error(
            fdo::Error::AccessDenied("policy refuses ownership".into()),
            NAME,
            DRAIN_WINDOW,
            &writer,
            &mut failures,
            &mut stream,
        )
        .await;
        assert_eq!(
            writer.get_cloned(),
            OwnState::Acquiring,
            "the Denied wait must still end in Acquiring so the outer loop retries"
        );
        assert!(
            stream.next().await.is_none(),
            "the Denied wait must consume what arrives, not leave it filling the broadcast queue"
        );
    }

    // ── #653: the acquisition-error ramp ────────────────────────────────────

    /// The backoff progression, pinned exactly. 250 ms doubling to a 30 s
    /// ceiling: the flat `Duration::from_millis(250)` this replaces would fail
    /// every assertion from the second onwards.
    #[test]
    fn acquire_backoff_doubles_then_caps() {
        let expected_ms = [250u64, 500, 1_000, 2_000, 4_000, 8_000, 16_000];
        for (i, ms) in expected_ms.iter().enumerate() {
            let attempt = u32::try_from(i).expect("index fits u32") + 1;
            assert_eq!(
                acquire_backoff(attempt),
                Duration::from_millis(*ms),
                "attempt {attempt} must double the previous delay"
            );
        }
        // Attempt 8 would be 32 s, past the ceiling, and everything after it
        // pins there rather than growing without bound.
        for attempt in [8u32, 9, 100, 10_000, u32::MAX] {
            assert_eq!(
                acquire_backoff(attempt),
                ACQUIRE_BACKOFF_CAP,
                "attempt {attempt} must be clamped to the ceiling"
            );
        }
    }

    /// The ceiling is seconds, not minutes: this path has no `NameOwnerChanged`
    /// to wake it, so the cap is also the worst case for noticing the bus came
    /// back. A regression that "fixed" the busy-loop by reaching for the
    /// 5-minute give-up cooldown would trade #653's spin for the stall the
    /// issue explicitly does not want.
    #[test]
    fn the_ceiling_recovers_within_seconds_not_minutes() {
        assert!(
            ACQUIRE_BACKOFF_CAP <= Duration::from_mins(1),
            "a name that becomes acquirable must be retried within a minute"
        );
        assert!(
            ACQUIRE_BACKOFF_BASE < ACQUIRE_BACKOFF_CAP,
            "the ramp must actually ramp"
        );
    }

    /// The log cadence is geometric, so the *logging* has a ceiling too — not
    /// just the retry. Before #653 the non-transient arm logged on every
    /// attempt forever and the transient arm logged nothing at all.
    #[test]
    fn logs_only_at_each_doubling() {
        let logged: Vec<u32> = (1..=1024u32).filter(|a| logs_at(*a)).collect();
        assert_eq!(logged, vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024]);
    }

    /// The whole point of the ramp, stated as the budget it buys: an hour of
    /// unbroken failure must cost a bounded number of `RequestName` attempts
    /// and a handful of log lines.
    ///
    /// The flat 250 ms retry it replaces would spend 14 400 attempts in the
    /// same hour, so this fails loudly if `acquire_backoff` is flattened back.
    #[test]
    fn an_hour_of_failure_is_bounded_in_attempts_and_lines() {
        let mut streak = FailureStreak::default();
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0u32;
        let mut lines = 0u32;
        while elapsed < Duration::from_hours(1) {
            let backoff = streak.record();
            attempts += 1;
            if backoff.log {
                lines += 1;
            }
            elapsed += backoff.delay;
        }
        assert!(
            attempts <= 130,
            "an hour of failure must not cost more than ~2 attempts a minute, got {attempts}"
        );
        assert!(
            lines <= 8,
            "an hour of failure must not cost more than a handful of log lines, got {lines}"
        );
        // And it must not have gone so quiet that it is no longer retrying.
        assert!(
            attempts >= 100,
            "the 30 s cap implies ~120 attempts an hour"
        );
    }

    /// The other half of the trade: anything that works clears the streak, so
    /// a blip costs one 250 ms wait and never leaves a 30 s delay armed for the
    /// *next* unrelated failure.
    #[test]
    fn a_success_resets_the_ramp_to_the_base_delay() {
        let mut streak = FailureStreak::default();
        for _ in 0..12 {
            let _ = streak.record();
        }
        assert_eq!(
            streak.record().delay,
            ACQUIRE_BACKOFF_CAP,
            "a long streak must be at the ceiling before the reset means anything"
        );

        streak.reset();

        let after = streak.record();
        assert_eq!(after.attempt, 1);
        assert_eq!(
            after.delay, ACQUIRE_BACKOFF_BASE,
            "a success must put the next failure back at the base delay"
        );
        assert!(
            after.log,
            "the first failure of a fresh streak is always worth a line"
        );
    }

    /// The first few failures stay at `debug!` (a reconnect blip is already
    /// reported by `connection.rs`); a streak that outlives that explanation
    /// escalates to `warn!` and stays there.
    #[test]
    fn a_streak_escalates_from_blip_to_serious() {
        let mut streak = FailureStreak::default();
        let mut first_serious = None;
        for _ in 0..64 {
            let backoff = streak.record();
            if backoff.is_serious() && first_serious.is_none() {
                first_serious = Some(backoff.attempt);
            }
        }
        let first = first_serious.expect("a 64-failure streak must become serious");
        assert!(
            (2..=8).contains(&first),
            "escalation must happen within seconds, not attempts later; got {first}"
        );
    }

    // ── #653: closing the give-up incident ──────────────────────────────────

    /// The recovery line fires only when there is an incident to close. A first
    /// acquisition and an ordinary sub-threshold retry must stay silent, or the
    /// line becomes noise on every reconnect.
    #[test]
    fn only_a_latched_tally_closes_a_give_up() {
        assert!(
            !closes_a_give_up(None, 3),
            "a first acquisition has no incident to close"
        );
        assert!(
            !closes_a_give_up(Some(&(":1.7".to_owned(), 1)), 3),
            "a sub-threshold retry never logged a give-up"
        );
        assert!(
            !closes_a_give_up(Some(&(":1.7".to_owned(), 2)), 3),
            "still below the threshold"
        );
        assert!(
            closes_a_give_up(Some(&(":1.7".to_owned(), 3)), 3),
            "the tally that produced a give-up must produce its close"
        );
        assert!(
            closes_a_give_up(Some(&(":1.7".to_owned(), 9)), 3),
            "a re-latched tally is still an open incident"
        );
    }

    /// Every `record_loss` verdict that gives up must also read as an open
    /// incident, and every verdict that retries must not. Ties the two pure
    /// functions together so a change to one cannot silently desync the
    /// give-up line from the line that closes it.
    #[test]
    fn give_up_and_its_close_agree_on_every_verdict() {
        let mut tally = None;
        for _ in 0..8 {
            let gave_up = matches!(
                record_loss(&mut tally, Some(":1.7"), 3),
                Contention::GiveUp { .. }
            );
            assert_eq!(
                gave_up,
                closes_a_give_up(tally.as_ref(), 3),
                "a give-up must be exactly the state a recovery would close"
            );
        }
    }

    // ── #653: state transitions on the error path ───────────────────────────

    /// A `RequestName` error that is *not* `AccessDenied` must not latch the
    /// `Denied` state — that state means "policy refuses", and mislabelling a
    /// transport failure as one would tell a consumer to go install a D-Bus
    /// policy rule that is already fine.
    #[tokio::test]
    async fn a_non_policy_request_name_error_does_not_latch_denied() {
        let writer = Mutable::new(OwnState::Owned);
        let mut streak = FailureStreak::default();
        let mut subscription = futures_util::stream::pending::<zbus::Result<zbus::Message>>();
        on_request_name_error(
            fdo::Error::Disconnected("broker went away".into()),
            NAME,
            // Long enough that a wrongly-taken `AccessDenied` branch would
            // blow the test's own timeout rather than pass by accident.
            LONG_COOLDOWN,
            &writer,
            &mut streak,
            &mut subscription,
        )
        .await;
        assert_eq!(
            writer.get_cloned(),
            OwnState::Acquiring,
            "a transport error must leave us acquiring, not denied"
        );
    }

    /// Consecutive `RequestName` errors advance the ramp, so a broker that
    /// keeps erroring is asked less and less often instead of at a flat rate
    /// forever.
    #[tokio::test]
    async fn repeated_request_name_errors_advance_the_ramp() {
        let writer = Mutable::new(OwnState::Acquiring);
        let mut streak = FailureStreak::default();
        let mut subscription = futures_util::stream::pending::<zbus::Result<zbus::Message>>();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            on_request_name_error(
                fdo::Error::Disconnected("broker went away".into()),
                NAME,
                LONG_COOLDOWN,
                &writer,
                &mut streak,
                &mut subscription,
            )
            .await;
        }
        // 250 ms + 500 ms + 1 s: a flat 250 ms ramp would finish in ~750 ms.
        assert!(
            started.elapsed() >= Duration::from_millis(1_700),
            "three consecutive errors must have backed off, not retried flat"
        );
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
