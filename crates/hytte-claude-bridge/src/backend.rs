//! One conversation abstraction, two native implementations.
//!
//! This is @kaesaecracker's cut from the #584 thread, and it is the reason the
//! bridge is not "two backends that happen to share a URL": both arms answer
//! the same [`Turn`] — a title, a transcript, and the newest message — and each
//! does the *native* thing with it rather than emulating the other.
//!
//! - [`Subscription`] rides a **persisted** `hive-claude` session addressed by
//!   title. Resume-then-create, and a resumed session receives **only the
//!   delta**, so the session's prompt prefix stays byte-stable turn over turn
//!   and Claude Code's prompt cache can actually hit. This is the default.
//! - [`Reprompt`] keeps the conversation in the **bridge's own record** and
//!   re-prompts a fresh one-off session with the whole thing each turn. No
//!   claude session state touches disk, at the cost of re-sending context.
//!   Selected with `CLAUDE_BRIDGE_MODE=reprompt`.
//!
//! [`Reprompt`] is where the thread's *other* cut landed too (#730): its
//! [`Engine`] chooses between the one-off `claude` subprocess and a real
//! `/v1/messages` API client ([`crate::messages`]), because the record-plus-
//! re-prompt shape is exactly the stateless one that path wants. Selected with
//! `CLAUDE_BRIDGE_MODE=api`. The record, the bound, and the head-pinning are
//! shared by both engines — only "who answers a composed message list" differs.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hive_claude::{Attach, Cancel, Claude, Config, Error as ClaudeError, Sink};
use serde_json::Value;
use tokio::time::Instant;

use crate::http::Failure;
use crate::messages::Client as MessagesClient;
use crate::session::{AttachKind, OVERFLOW_STATUS, prompt_for, render_transcript};
use crate::wire::Message;

/// Longest error text handed back to the client. `Error::Exit` carries up to
/// twenty stderr lines, which would swamp the pet's one-line log.
pub const MAX_ERROR_CHARS: usize = 300;

/// Longest conversation the [`Reprompt`] record keeps. Past this the oldest
/// turns are dropped, but the head (normally the system/persona message) is
/// pinned so the conversation doesn't lose its instructions.
///
/// It is also the **cost** bound on the API engine (#730): a stateless
/// re-prompt re-sends the whole record every turn, so this is what stops a
/// long-lived pet conversation growing its per-tick bill without limit.
const MAX_RECORD_MESSAGES: usize = 64;

/// How many conversations [`Reprompt`] keeps records for.
const MAX_RECORDS: usize = 64;

/// How long a `claude` child killed with `SIGKILL` at the request deadline gets
/// to be **reaped** before [`run_until`] stops waiting for it.
///
/// The kill itself cannot be ignored and lands at once; what this bounds is the
/// driver noticing. `hive-claude`'s `RunningClaude::wait` returns only once the
/// child has been `wait()`ed **and** both of its output pipes have hit EOF, so a
/// grandchild that inherited stdout/stderr (a hook the owner's settings run,
/// say) could hold the turn open after the child itself is dead. Without one
/// the reap is a few milliseconds — 2.4 ms past the deadline when
/// `a_silently_hung_child_is_killed_and_reaped_at_the_deadline` was first run —
/// so half a second is generous, and still leaves the 504 well inside the
/// client's 10s timeout at the default 8s budget: `main`'s
/// `the_default_budget_is_under_the_clients_global_timeout` pins that sum.
///
/// `pub(crate)` because `bridge.rs` sizes its last-resort backstop from it.
pub(crate) const REAP_GRACE: Duration = Duration::from_millis(500);

/// One turn to answer.
#[derive(Debug, Clone, Copy)]
pub struct Turn<'a> {
    /// The derived session title — the conversation's identity (see
    /// [`crate::session`]).
    pub title: &'a str,
    /// The full transcript the client sent, newest message last.
    pub transcript: &'a [Message],
    /// The newest message, i.e. the delta the session has not seen.
    pub delta: &'a Message,
    /// The client's `max_tokens`, carried down rather than dropped at the HTTP
    /// layer.
    ///
    /// The Claude Code CLI exposes no such flag, so both `claude` arms ignore
    /// it — that is the "at best approximated" the crate docs promise. The
    /// Messages API **requires** it, so [`crate::messages`] is the one arm that
    /// can honour it, and does.
    pub max_tokens: Option<u32>,
    /// When the bridge's per-request budget runs out — **one** deadline for the
    /// whole request, a session rotation's retry included.
    ///
    /// Carried down rather than enforced by a timeout wrapped around the
    /// backend, because that wrapper is exactly what used to orphan `claude`:
    /// dropping the future drops the `hive-claude` turn with no `kill_on_drop`
    /// on its child, and with it the title lock and the request permit that
    /// were meant to bound live children. Every wait in a backend is bounded by
    /// this instead, and the one that owns a child ([`run_until`]) kills and
    /// reaps it before giving the deadline back as a 504.
    ///
    /// A turn dropped anyway — by `bridge.rs`'s backstop, a panic, or a runtime
    /// shutdown — no longer orphans the child: [`run_until`]'s drop guard
    /// `SIGKILL`s it before the lock and the permit go (#1550). What a drop
    /// still gives up is the *reap*, which falls to tokio's orphan reaper — so
    /// a deadline remains the one way a turn ends with its child provably
    /// gone. (A client hanging up mid-turn drops nothing: `serve_connection`
    /// awaits the answer without watching the socket, so that turn runs on to
    /// its deadline.)
    pub deadline: Instant,
}

/// The abstraction both backends implement.
pub trait Conversation {
    /// Answer one turn, or explain why not in terms the HTTP layer can send.
    fn respond(&self, turn: Turn<'_>) -> impl Future<Output = Result<String, Failure>> + Send;
}

/// The subscription path: a persisted, title-addressed `hive-claude` session.
#[derive(Debug)]
pub struct Subscription {
    /// Boxed because `hive-claude` 0.1.1 added a `Config::env` field (#1054),
    /// which pushed `Backend::Subscription(Subscription)` over
    /// `clippy::large_enum_variant`'s threshold against `Backend::Reprompt`'s
    /// far smaller variant — the same class of growth `Engine::Cli` below was
    /// already boxed against for `hive-claude` 0.1.0. One `Subscription`
    /// exists per process, so the indirection costs nothing that matters.
    config: Box<Config>,
    /// Serialises same-title turns (#693).
    ///
    /// Claude Code does **not** serialise two concurrent `--resume`s of one
    /// session — measured, not assumed. Both runs exit 0 with plausible text,
    /// and the session's JSONL is left with one uuid parenting two divergent
    /// children: the transcript becomes a tree, and every later resume
    /// resolves against an ambiguous state. Nothing errors, which is what
    /// makes it worth a lock rather than a retry.
    turns: TitleLocks,
}

impl Subscription {
    /// A subscription backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config: Box::new(config),
            turns: TitleLocks::default(),
        }
    }
}

impl Conversation for Subscription {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        // The child runs inside `resume_then_create`'s title lock, and
        // `run_until` returns only once a deadline-killed child is reaped — so
        // the next turn on this title cannot `--resume` the session while this
        // one's child still has it open (#693's fork, by the back door).
        resume_then_create(turn, &self.turns, |attach, prompt| async move {
            let sink = TextSink::default();
            let ended = run_until(&self.config, &attach, &prompt, &sink, turn.deadline)
                .await
                .map_err(RunError::Failed)?;
            match ended {
                Ok(()) => sink.into_text().map_err(RunError::Failed),
                Err(ClaudeError::SessionNotFound) => Err(RunError::NoSession),
                Err(e) => Err(RunError::Failed(map_error(&e))),
            }
        })
        .await
    }
}

/// Why one attach attempt produced no answer.
///
/// Only [`RunError::NoSession`] is a *routing* outcome; everything else is
/// already the status the client should see. It exists so the resume-then-
/// create sequence can be written once over a seam — spawning a real `claude`
/// to test a lock would be neither hermetic nor free.
#[derive(Debug)]
enum RunError {
    /// The title resolved to no session; the caller may create it.
    NoSession,
    /// Anything else, already mapped for the client.
    Failed(Failure),
}

/// Answer one turn under `turn.title`: resume its session, and create one if
/// there is none — with **both** steps under that title's lock.
///
/// The lock spanning the whole sequence rather than just the resume is the
/// half of #693 that is easy to miss. Two concurrent *first* turns would each
/// miss the resume and each `Attach::Create`, leaving two session files under
/// one title — and `hive-claude` picks between those by readdir order, which
/// it warns about but cannot fix (`store.rs`: "multiple session files share
/// this title … not deterministic"; `Attach::Create`: "Nothing enforces
/// uniqueness"). Guarding only the resume would close the transcript fork and
/// open a rarer, nastier duplicate-session bug in its place.
///
/// `run` is the seam: in production it is `Claude::run` plus a [`TextSink`].
async fn resume_then_create<F, Fut>(
    turn: Turn<'_>,
    locks: &TitleLocks,
    run: F,
) -> Result<String, Failure>
where
    F: Fn(Attach, String) -> Fut,
    Fut: Future<Output = Result<String, RunError>>,
{
    // Bounded by the request's deadline, not merely by the holder: a turn that
    // spent its whole budget queued behind another one on this title answers
    // its 504 on time rather than once the holder lets go.
    let Ok(_serialised) = tokio::time::timeout_at(turn.deadline, locks.acquire(turn.title)).await
    else {
        return Err(expired());
    };

    // Resume first — always. The session may exist from an earlier turn, an
    // earlier bridge process, or an identical earlier request; in every one
    // of those cases it already holds the prefix, so it must receive ONLY
    // the newest message.
    let resume_prompt = prompt_for(AttachKind::Resume, turn.transcript, turn.delta);
    match run(Attach::Resume(turn.title.to_owned()), resume_prompt).await {
        Ok(text) => return Ok(text),
        Err(RunError::NoSession) => {
            tracing::debug!(title = turn.title, "no such session; creating");
        }
        Err(RunError::Failed(failure)) => return Err(failure),
    }

    // Only a session that does not exist yet gets the whole transcript —
    // there is nothing in it to duplicate.
    let create_prompt = prompt_for(AttachKind::Create, turn.transcript, turn.delta);
    run(Attach::Create(turn.title.to_owned()), create_prompt)
        .await
        .map_err(|e| match e {
            RunError::NoSession => map_error(&ClaudeError::SessionNotFound),
            RunError::Failed(failure) => failure,
        })
}

/// Run one `claude` turn to completion — or, if `deadline` passes first, kill
/// the child and **reap it before returning**.
///
/// `Ok` is whatever the driver reported for a turn that ended on its own; `Err`
/// is the deadline, already the 504 the client should see.
///
/// # Why the bridge kills its own child
///
/// Before this, the only thing between a stalled `claude` and the outer
/// `tokio::time::timeout` was `hive-claude`'s idle watchdog, set a second under
/// the budget — and that watchdog could never fire in time. It re-checks
/// silence on a fixed 5 s probe (`IDLE_PROBE`, `driver.rs`), so at the default
/// 8 s budget its 7 s window was checked at t=5 (not yet idle long enough) and
/// next at t=10, two seconds after the outer timeout had dropped the whole
/// turn — the watchdog included, since it lives in the same `tokio::join!` as
/// the stream pumps. The driver sets no `kill_on_drop`, so the child simply
/// kept running, unreaped; the request released its permit (so `bridge.rs`'s
/// `PERMITS` stopped bounding live children) and its title lock (so the next
/// tick `--resume`d the session the orphan still had open). A child that was
/// still *streaming* was never covered by the watchdog at all.
///
/// So the deadline is enforced here, around the one future that owns the child,
/// using the seam `hive-claude` provides for exactly this: [`Claude::spawn`]
/// plus [`RunningClaude::cancel_handle`](hive_claude::RunningClaude::cancel_handle).
/// When the deadline wins, the child gets `SIGKILL` (what the driver's own
/// watchdog sends, via `Child::kill`) and the **same** `wait` future is then
/// driven on — not dropped — because it is what calls `waitpid` and drains the
/// pipes. Only once it returns does this function, and with it the title lock
/// and the request permit above it, let go.
///
/// The deadline is not the only way a turn can end early, though: this future
/// can itself be dropped before the child is dealt with. So a [`KillOnDrop`]
/// guard is armed the moment the spawn succeeds and disarmed on both paths
/// above — the turn's own outcome, and the deadline's kill — which leaves an
/// *abandoned* turn as the one thing that fires it (#1550).
///
/// # The edges, stated plainly
///
/// - **A dropped turn.** This future is dropped before the turn ends — today
///   by `bridge.rs`'s backstop, a panic, or the runtime shutting down, or
///   tomorrow by a timeout someone wraps around a backend again. (Not by a
///   client hanging up mid-turn: `serve_connection` awaits the answer without
///   watching the socket, so that turn runs on to its deadline and the path
///   above kills it.) The guard then sends the child `SIGKILL` and logs one
///   warning.
///
///   When this future is dropped while *suspended* — the backstop, a
///   shutdown, a panic somewhere above it — the guard drops before the
///   driver's `Child`: it is declared *after* the pinned `turn` future, and
///   locals drop in reverse order of declaration, so the signal goes out while
///   `turn` still owns the `Child`. The other order would hand that `Child` to
///   tokio's orphan reaper first, which could reap a child that had just
///   exited on its own, freeing its pid, before the guard's signal went out.
///   Dropping `turn` after the signal hands over a `Child` that is dying or
///   already a zombie, and the orphan reaper collects it — the same hand-off
///   as the grandchild case.
///
///   A panic raised *inside* `turn`'s own poll (the sink, the driver's pumps)
///   is the exception. Its unwind leaves `RunningClaude::wait` first, which
///   drops the `Child` — and tokio's drop reaps it if it has already exited —
///   before the unwind reaches this frame and the guard, so the signal can
///   find a freed pid (measured under `strace`: `wait4`, then `kill` =
///   `ESRCH`; #1623 review). The bridge cannot reorder that, because the
///   `Child` lives inside `hive-claude`'s `wait`; it is one of the raw-pid
///   exceptions below.
///
///   What a drop cannot do is *wait* for the reap: the title lock and the
///   request permit above belong to outer futures, which drop after this one,
///   so they let go with the `SIGKILL` sent but the child not yet reaped —
///   where the deadline path holds them until it is.
/// - **A pipe-holding grandchild.** `wait` also needs both output pipes at EOF,
///   which a grandchild that inherited them can withhold after the child is
///   dead. That wait is bounded by [`REAP_GRACE`] and then abandoned with a
///   warning: the child itself is already dead, and dropping the driver's
///   `Child` hands its zombie to tokio's orphan reaper. The grandchild is not
///   reachable from here — the driver builds its `Command` privately, so there
///   is no process group to signal.
/// - **A stall while writing the prompt.** If the child never reads stdin and
///   the prompt overflows the pipe buffer, the kill surfaces as an `EPIPE` from
///   the driver's stdin write, which returns *before* it reaps; the child is
///   dead either way and tokio's orphan reaper collects it.
/// - **The raw pid.** `Cancel` signals a pid with no generation check
///   (hive-claude#15). It is only ever sent while `wait` is still pending — by
///   the deadline path, or by the guard, which is disarmed the moment `turn`
///   returns because a turn that ended on its own may have reaped its child —
///   so the child is normally unreaped and its pid cannot have been recycled.
///   Three exceptions, all of one class, where the child was reaped first: the
///   grandchild case above; a `wait` whose `join!` has reaped the child but is
///   still pending while its pumps drain buffered output; and a panic raised
///   inside `turn` (see "A dropped turn"). In each, the signal can only reach
///   another process if the pid space wrapped in between. The bridge cannot
///   close them, since the `Child` lives inside `hive-claude`'s `wait`; what
///   would is upstream — `kill_on_drop(true)` on the child (tokio then kills
///   before it reaps) or a pidfd-based `Cancel` (hive-claude#15).
///
/// A deadline that has already passed spawns nothing: a turn that spent its
/// budget queued behind another one must not start a child just to kill it.
async fn run_until(
    config: &Config,
    attach: &Attach,
    prompt: &str,
    sink: &impl Sink,
    deadline: Instant,
) -> Result<hive_claude::Result<()>, Failure> {
    if Instant::now() >= deadline {
        return Err(expired());
    }
    let running = match Claude::spawn(config, attach) {
        Ok(running) => running,
        Err(e) => return Ok(Err(e)),
    };
    let cancel = running.cancel_handle();
    let mut turn = std::pin::pin!(running.wait(prompt, sink));
    // Declared after `turn` on purpose, so that dropping this future while it
    // is suspended drops the guard first and signals the child while `turn`
    // still owns it. (A panic inside `turn`'s own poll unwinds through `turn`
    // first and cannot be reordered from here; see "A dropped turn" above.)
    // Nothing awaits between the spawn and here, so there is no earlier point
    // at which this future could be abandoned with a child and no guard.
    let abandoned = KillOnDrop::arm(cancel.clone());
    // `timeout_at` polls the turn before it checks the clock, so a turn that
    // finished on the same tick the deadline fell is still taken as an answer.
    if let Ok(ended) = tokio::time::timeout_at(deadline, &mut turn).await {
        abandoned.disarm();
        return Ok(ended);
    }
    cancel.cancel(true);
    // Killed here and reaped below, by this path's own wait: a drop during
    // that wait has nothing left to kill, and must not report the deadline's
    // kill as an abandoned turn.
    abandoned.disarm();
    if tokio::time::timeout(REAP_GRACE, &mut turn).await.is_ok() {
        tracing::warn!("claude did not finish within the request budget; killed and reaped it");
    } else {
        tracing::warn!(
            grace_ms = REAP_GRACE.as_millis(),
            "claude did not finish within the request budget and was killed, but its output \
             pipes are still open (a grandchild holding them?); leaving the reap to tokio",
        );
    }
    Err(expired())
}

/// `SIGKILL`s a turn's `claude` child if the turn is **abandoned**: its
/// [`run_until`] future dropped before the child was dealt with (#1550).
///
/// `hive-claude` sets no `kill_on_drop` on the child it spawns, so before this
/// a dropped turn left `claude` running — still spending the owner's
/// subscription — and nothing said so unless the drop was `bridge.rs`'s own
/// backstop. [`run_until`] arms one right after a successful spawn and
/// disarms it on each path that deals with the child itself, so [`Drop`] fires
/// for a dropped or panicking turn and for nothing else.
///
/// Signalling only, never waiting: a `Drop` cannot await the reap, which is
/// left to tokio's orphan reaper once the driver's `Child` is dropped after it.
#[derive(Debug)]
struct KillOnDrop {
    /// `None` once disarmed.
    cancel: Option<Cancel>,
}

impl KillOnDrop {
    /// Kill the child behind `cancel` on drop, unless disarmed first.
    fn arm(cancel: Cancel) -> Self {
        Self {
            cancel: Some(cancel),
        }
    }

    /// The child is already dealt with — it ended its own turn, or the
    /// deadline killed it and is reaping it — so dropping this sends nothing.
    fn disarm(mut self) {
        self.cancel = None;
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel(true);
            tracing::warn!(
                "claude's turn was dropped before it finished (the request backstop, a panic, or \
                 shutdown); killed it and left the reap to tokio",
            );
        }
    }
}

/// The status a request gets when its budget ran out while it was still
/// waiting or working — one wording wherever along the turn that happened.
pub(crate) fn expired() -> Failure {
    Failure::new(504, "no answer within the bridge's budget")
}

/// One async mutex per conversation title, minted on demand and retired when
/// the last turn holding it or queued on it lets go — by releasing it, or by
/// giving up the wait (#1550).
///
/// **Per title, not global.** The title *is* the conversation's identity (see
/// [`crate::session`]), so a global lock would park the pet's turn behind
/// caw's for no reason — a latency regression on the common case, in service
/// of a hazard that only exists when two turns share a title.
///
/// The outer `std` mutex is only ever held for one map lookup, never across an
/// await; the inner `tokio` one is held across a whole turn, which is exactly
/// why it has to be `tokio`'s.
#[derive(Debug, Default)]
struct TitleLocks {
    by_title: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl TitleLocks {
    /// Wait for exclusive use of `title`.
    ///
    /// Dropping the returned future while it still waits — the request's
    /// deadline, or whatever drops the turn around it (the backstop, a panic,
    /// a shutdown) — gives up the place in the queue
    /// (and any lock already handed to it) and retires the title's entry if
    /// no other turn holds or waits on it.
    async fn acquire(&self, title: &str) -> TitleGuard<'_> {
        let mutex = {
            let mut by_title = self.by_title.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(by_title.entry(title.to_owned()).or_default())
        };
        // The guard exists from the moment this turn holds a clone of the
        // mutex, not from the moment it holds the lock: a waiter dropped while
        // queued has to run the same cleanup a holder does on release, or the
        // entry outlives it (#1550). The holder's release cannot do it on the
        // waiter's behalf — it rightly keeps an entry a waiter still holds a
        // clone of, and that waiter may then never come back for it.
        //
        // Declared *before* the wait on purpose. The pending `lock_owned`
        // owns the waiter's clone, and a future dropped mid-`.await` drops its
        // locals innermost scope first, so the clone is gone by the time this
        // guard's `Drop` counts what is left.
        let mut guard = TitleGuard {
            locks: self,
            title: title.to_owned(),
            held: None,
        };
        // Owned rather than borrowed: the guard outlives the local `Arc`, and
        // an owned guard keeps its mutex alive by itself.
        guard.held = Some(mutex.lock_owned().await);
        guard
    }

    /// How many titles hold a lock right now. Only the invariant that this
    /// falls back to zero is interesting, which makes it a test's business.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.by_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Exclusive use of one title, released on drop — or, while
/// [`TitleLocks::acquire`] is still waiting for it, a claim on the title's map
/// entry that is given up on drop the same way.
#[derive(Debug)]
struct TitleGuard<'a> {
    locks: &'a TitleLocks,
    title: String,
    /// `None` while `acquire` is still waiting for the lock — the one state
    /// in which this guard can be dropped without ever having held it — and
    /// an `Option` at all so [`Drop`] can release the mutex *before* deciding
    /// whether its map entry is still wanted.
    held: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for TitleGuard<'_> {
    fn drop(&mut self) {
        // Release first: an owned guard holds a strong reference of its own, so
        // the count below could never fall to one while this still held it.
        // A guard dropped while still waiting has nothing to release here: its
        // strong reference lived in the pending `lock_owned`, which is dropped
        // before this guard (see `acquire`).
        drop(self.held.take());
        let mut by_title = self
            .locks
            .by_title
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The cleanup race, and why a strong count is the right question: a
        // task that has already claimed this title holds a reference of its own
        // — it clones the `Arc` under this same `std` mutex, *before* it starts
        // waiting — so a count of one proves the map is the sole owner and
        // nobody can be parked on it. Retiring an entry out from under a waiter
        // would be worse than leaking it: the waiter would hold a mutex the map
        // no longer names, and the next arrival would mint a second one and
        // fail to exclude it.
        if by_title
            .get(&self.title)
            .is_some_and(|mutex| Arc::strong_count(mutex) == 1)
        {
            by_title.remove(&self.title);
        }
    }
}

/// The re-prompting path: the bridge holds the conversation, the thing that
/// answers holds nothing.
///
/// Which thing answers is [`Engine`]. Everything *around* it — the bounded,
/// head-pinned record and the compose/remember cycle — is shared, because that
/// is the part the stateless shape is made of; swapping a subprocess for an
/// HTTP call changes nothing about the conversation's identity or its bound.
#[derive(Debug)]
pub struct Reprompt {
    engine: Engine,
    records: Mutex<Records>,
}

/// Who answers one composed message list.
#[derive(Debug)]
enum Engine {
    /// A one-off `claude --print` session per turn — no `--resume`, no
    /// `--name`, so claude persists no titled session for the bridge to depend
    /// on. Rides the subscription; holds no key.
    ///
    /// Boxed because `hive-claude` 0.1.0's `Config` grew past the point where
    /// `clippy::large_enum_variant` tolerates inlining it (288 bytes against
    /// [`Engine::Api`]'s 72). One `Engine` exists per process, so the
    /// indirection costs nothing that matters.
    Cli(Box<Config>),
    /// The Anthropic Messages API, billed to an API key (#730). No subprocess,
    /// no `claude` CLI, no `hive-claude` session state at all.
    Api(MessagesClient),
}

impl Reprompt {
    /// A re-prompting backend running `claude` per `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            engine: Engine::Cli(Box::new(config)),
            records: Mutex::new(Records::default()),
        }
    }

    /// A re-prompting backend answering through the Messages API instead of a
    /// `claude` subprocess (#730).
    #[must_use]
    pub fn with_api(client: MessagesClient) -> Self {
        Self {
            engine: Engine::Api(client),
            records: Mutex::new(Records::default()),
        }
    }
}

impl Conversation for Reprompt {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        let messages = {
            let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.compose(turn)
        };
        // The guard above is dropped before this await on purpose: a `Mutex`
        // held across an await point would make this future non-`Send`, which
        // the `Conversation` trait requires.
        let reply = match &self.engine {
            Engine::Cli(config) => {
                let sink = TextSink::default();
                run_until(
                    config,
                    &Attach::OneOff,
                    &render_transcript(&messages),
                    &sink,
                    turn.deadline,
                )
                .await?
                .map_err(|e| map_error(&e))?;
                sink.into_text()?
            }
            // No child to reap here: `spawn_blocking` cannot be cancelled, so
            // the client carries its own budget, under this deadline (see
            // `messages::Client::new`), and this only stops *waiting* for it.
            Engine::Api(client) => {
                tokio::time::timeout_at(turn.deadline, client.respond(&messages, turn.max_tokens))
                    .await
                    .map_err(|_elapsed| expired())??
            }
        };
        {
            let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
            records.remember(turn.title, messages, &reply);
        }
        Ok(reply)
    }
}

/// The bridge-side conversation store backing [`Reprompt`].
#[derive(Debug, Default)]
struct Records {
    by_title: HashMap<String, Vec<Message>>,
    order: VecDeque<String>,
}

impl Records {
    /// The message list to re-prompt with.
    ///
    /// Prefers the bridge's own record when it is at least as complete as the
    /// client's prefix — a client that drops assistant turns from its history
    /// (as a stateless prompt-stuffing client does) still gets continuity.
    /// Otherwise the client's transcript wins, because it knows something the
    /// record does not.
    fn compose(&self, turn: Turn<'_>) -> Vec<Message> {
        // Saturating: the HTTP layer rejects an empty `messages`, but a
        // panicking underflow in a daemon is not a defence worth relying on.
        let prefix_len = turn.transcript.len().saturating_sub(1);
        match self.by_title.get(turn.title) {
            Some(record) if record.len() >= prefix_len => {
                let mut composed = record.clone();
                composed.push(turn.delta.clone());
                composed
            }
            _ => turn.transcript.to_vec(),
        }
    }

    /// Record what was sent plus the reply it produced.
    fn remember(&mut self, title: &str, mut messages: Vec<Message>, reply: &str) {
        messages.push(Message::assistant(reply));
        // Drop from just after the head so the system/persona message stays.
        while messages.len() > MAX_RECORD_MESSAGES {
            messages.remove(1);
        }
        if self.by_title.insert(title.to_owned(), messages).is_none() {
            self.order.push_back(title.to_owned());
        }
        while self.order.len() > MAX_RECORDS {
            if let Some(evicted) = self.order.pop_front() {
                self.by_title.remove(&evicted);
            }
        }
    }
}

/// The runtime-selected backend. An enum rather than a `dyn` object because the
/// trait returns `impl Future` (not dyn-compatible) and there will only ever be
/// the two arms the design names.
#[derive(Debug)]
pub enum Backend {
    Subscription(Subscription),
    Reprompt(Reprompt),
}

impl Backend {
    /// Whether this backend's conversations accumulate in **claude's** own
    /// on-disk session state — the thing that grows without bound and that only
    /// a fresh title can retire (#667).
    ///
    /// [`Reprompt`]'s conversation lives in the bridge's own record, which is
    /// already bounded and head-pinned ([`MAX_RECORD_MESSAGES`]), so an
    /// overflow there means the *client's* transcript does not fit — which no
    /// rotation can fix, and which would spin the generation counter once per
    /// request if it were allowed to try.
    #[must_use]
    pub fn is_persisted(&self) -> bool {
        matches!(self, Self::Subscription(_))
    }
}

impl Conversation for Backend {
    async fn respond(&self, turn: Turn<'_>) -> Result<String, Failure> {
        match self {
            Self::Subscription(b) => b.respond(turn).await,
            Self::Reprompt(b) => b.respond(turn).await,
        }
    }
}

/// Collects a turn's answer out of the `stream-json` stream.
///
/// `hive_claude::Claude::run` reports only *how* a turn ended; the text itself
/// arrives through the [`Sink`]. The terminal `result` event carries claude's
/// final answer and is preferred; the concatenated `assistant` text blocks are
/// the fallback for a stream that ends without one.
#[derive(Debug, Default)]
struct TextSink {
    assistant: Mutex<String>,
    result: Mutex<Option<String>>,
}

impl TextSink {
    /// The collected answer, or a 502 if the stream carried no text at all.
    fn into_text(self) -> Result<String, Failure> {
        let result = self
            .result
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let assistant = self
            .assistant
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        let text = result.filter(|s| !s.trim().is_empty()).unwrap_or(assistant);
        if text.trim().is_empty() {
            return Err(Failure::new(502, "claude produced no text"));
        }
        Ok(text)
    }
}

impl Sink for TextSink {
    fn on_event(&self, event: &Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let Some(blocks) = event.pointer("/message/content").and_then(Value::as_array)
                else {
                    return;
                };
                let mut out = self
                    .assistant
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = block.get("text").and_then(Value::as_str)
                    {
                        out.push_str(text);
                    }
                }
            }
            Some("result") => {
                // `is_error` results carry claude's failure text, not an answer;
                // the driver's sentinels already turned those into an `Error`.
                if event.get("is_error").and_then(Value::as_bool) == Some(true) {
                    return;
                }
                if let Some(text) = event.get("result").and_then(Value::as_str) {
                    *self.result.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(text.to_owned());
                }
            }
            _ => {}
        }
    }

    fn on_stderr_line(&self, line: &str) {
        tracing::debug!(target: "hytte_claude_bridge::claude", "{line}");
    }
}

/// Map a driver error onto the status the client should see.
///
/// The typed sentinels are exactly why this crate depends on `hive-claude`
/// rather than sniffing stderr: `RateLimited` vs. `Spawn` vs. `AuthFailed` are
/// three very different operator actions, and classifying them by substring is
/// the kind of code nobody should own twice.
fn map_error(e: &ClaudeError) -> Failure {
    let (status, message) = match e {
        ClaudeError::RateLimited => (
            429,
            "claude: rate-limited or a usage/credit cap was reached".to_owned(),
        ),
        // The one failure a session rotation can fix (#667) — `bridge.rs`
        // triggers on exactly this status, so the constant is shared rather
        // than the number written twice.
        ClaudeError::PromptTooLong => (
            OVERFLOW_STATUS,
            "claude: the conversation exceeds the model's context window".to_owned(),
        ),
        ClaudeError::AuthFailed => (
            502,
            "claude is not authenticated — run `claude` and /login. \
             (The bridge holds no credentials of its own; it is keyless by design.)"
                .to_owned(),
        ),
        ClaudeError::IdleTimeout => (
            504,
            "claude produced no output within the bridge's budget".to_owned(),
        ),
        ClaudeError::SessionNotFound => (
            502,
            "claude could not resolve the session title even after creating it".to_owned(),
        ),
        // `program` is a `PathBuf` since #757's `hive-claude` upgrade: it may be
        // an absolute path rather than a `PATH`-resolved name, so it is
        // `.display()`ed rather than formatted directly.
        ClaudeError::Spawn { program, source } => (
            502,
            format!(
                "could not spawn `{}`: {source} (is the Claude Code CLI on PATH?)",
                program.display()
            ),
        ),
        other => (502, format!("claude: {other}")),
    };
    Failure::new(status, truncate(&message, MAX_ERROR_CHARS))
}

/// Clamp `text` to `max` chars (never mid-char).
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let clamped: String = text.chars().take(max).collect();
    format!("{clamped}…")
}

/// A stand-in `claude` that **hangs silently**, for the tests that pin
/// [`run_until`]'s kill-and-reap — here and in `bridge.rs`, which drives it
/// through a whole request.
///
/// It is the stall the bug needed: it reads its prompt to EOF (as `claude`
/// does before it calls out), then `exec`s a `sleep` that never writes a byte —
/// `exec`, so the pid the bridge signals *is* the sleeping process rather than
/// a shell whose `sleep` child would survive the kill holding the pipes open
/// (the trap `hive-claude`'s own `sleepy_program` test documents).
///
/// Each run leaves two traces in its directory, both keyed by its pid: a
/// `<pid>.pid` marker once it is up, and a `<pid>.saw` listing every *earlier*
/// stand-in still in `/proc` at the moment it started. `/proc/<pid>` outlives
/// the process until it is **reaped** — a zombie still has one — so "absent from
/// `/proc`" is the reap, not merely the kill, and `.saw` is how a test asks
/// whether a child started while another was still unreaped.
#[cfg(test)]
pub(crate) mod stand_in {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// Parameter expansion rather than `dirname`/`basename`, so the script
    /// needs nothing but `cat` and `sleep` beside the shell. The pid lives in
    /// the marker's *name*, so creating it is the whole write, and it is
    /// created last: once a test sees `<pid>.pid`, that run's `.saw` is final.
    const SCRIPT: &str = r#"#!/bin/sh
dir="${0%/*}"
cat > /dev/null
for f in "$dir"/*.pid; do
  [ -e "$f" ] || continue
  pid="${f##*/}"
  pid="${pid%.pid}"
  [ -d "/proc/$pid" ] && echo "$pid" >> "$dir/$$.saw"
done
: > "$dir/$$.pid"
exec sleep 30
"#;

    /// Write the stand-in into `dir` and return its path.
    pub(crate) fn hung_claude(dir: &Path) -> PathBuf {
        let program = dir.join("hung-claude");
        install(&program, SCRIPT);
        program
    }

    /// The opposite stand-in: reads its prompt, answers with one successful
    /// `result` event and exits 0 — a turn that ends on its own, for the tests
    /// that need [`super::run_until`]'s ordinary path. It leaves no `.pid`
    /// marker, so it is invisible to [`started`] and can share a directory
    /// with [`hung_claude`] runs.
    pub(crate) fn answering_claude(dir: &Path) -> PathBuf {
        let program = dir.join("answering-claude");
        install(
            &program,
            "#!/bin/sh\ncat > /dev/null\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"answered\"}'\n",
        );
        program
    }

    /// Install `script` at `path` as an executable — written by a `sh` child,
    /// **never by this process**.
    ///
    /// Not `std::fs::write` plus a chmod, because of `ETXTBSY`. Every test
    /// thread in this binary that forks (each stand-in spawn, `plugin.rs`'s
    /// re-exec of the test binary) hands its child a copy of *every* fd this
    /// process has open at that instant — `O_CLOEXEC` closes them only at the
    /// child's `exec` — and `execve` refuses a file that anyone holds open for
    /// writing. A script written in-process can therefore fail to start with
    /// "Text file busy" if a sibling test forked during the write; measured
    /// here, once in eight runs of this suite as soon as three tests spawned
    /// stand-ins. With the write in a child process, this process never holds
    /// a writable fd on the script for anyone to inherit, and once that child
    /// is reaped nobody does.
    pub(crate) fn install(path: &Path, script: &str) {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let mut writer = Command::new("/bin/sh")
            .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn the script writer");
        writer
            .stdin
            .take()
            .expect("piped")
            .write_all(script.as_bytes())
            .expect("hand the script over");
        assert!(
            writer.wait().expect("reap the script writer").success(),
            "could not install {}",
            path.display(),
        );
    }

    /// The pids of every stand-in that has started in `dir`, ascending.
    pub(crate) fn started(dir: &Path) -> Vec<u32> {
        let mut pids: Vec<u32> = std::fs::read_dir(dir)
            .expect("read the stand-in dir")
            .filter_map(|entry| {
                let name = entry.ok()?.file_name();
                name.to_str()?.strip_suffix(".pid")?.parse().ok()
            })
            .collect();
        pids.sort_unstable();
        pids
    }

    /// Wait (bounded) until `n` stand-ins have started in `dir`, and return
    /// their pids.
    pub(crate) async fn wait_for(dir: &Path, n: usize) -> Vec<u32> {
        for _ in 0..1000 {
            let pids = started(dir);
            if pids.len() >= n {
                return pids;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{n} stand-in(s) never started in {}", dir.display());
    }

    /// The earlier stand-ins `pid` found still in `/proc` when it started.
    pub(crate) fn saw(dir: &Path, pid: u32) -> Vec<u32> {
        std::fs::read_to_string(dir.join(format!("{pid}.saw")))
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().expect("one pid per line"))
            .collect()
    }

    /// Whether `pid` is still in `/proc` — running, **or dead but unreaped**.
    pub(crate) fn present(pid: u32) -> bool {
        Path::new("/proc").join(pid.to_string()).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::{Records, RunError, TextSink, TitleLocks, Turn, map_error, truncate};
    use crate::http::Failure;
    use crate::wire::Message;
    use hive_claude::{Attach, Error as ClaudeError, Sink as _};
    use serde_json::json;
    use std::collections::HashSet;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use tokio::sync::Barrier;

    /// How long one fake turn stays inside the seam, in scheduler yields. On a
    /// current-thread runtime a yield hands control to the other half of the
    /// race at exactly the points a real subprocess await would, which is what
    /// makes the races below deterministic rather than hopeful.
    const YIELDS: usize = 8;

    fn msg(role: &str, content: &str) -> Message {
        Message {
            role: role.to_owned(),
            content: content.to_owned(),
        }
    }

    /// A turn over `transcript` whose delta is its newest message.
    fn turn<'a>(title: &'a str, transcript: &'a [Message]) -> Turn<'a> {
        Turn {
            title,
            transcript,
            delta: transcript.last().expect("non-empty"),
            max_tokens: None,
            // Far enough out that no test below that is not *about* the
            // deadline can reach it.
            deadline: tokio::time::Instant::now() + std::time::Duration::from_hours(1),
        }
    }

    /// The terminal `result` event is the answer.
    #[test]
    fn the_result_event_supplies_the_answer() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "system", "subtype": "init"}));
        sink.on_event(&json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": "partial"}]}
        }));
        sink.on_event(&json!({"type": "result", "is_error": false, "result": "final answer"}));
        assert_eq!(sink.into_text().expect("text"), "final answer");
    }

    /// Without a `result` event the concatenated assistant text blocks stand in.
    #[test]
    fn assistant_blocks_are_the_fallback() {
        let sink = TextSink::default();
        sink.on_event(&json!({
            "type": "assistant",
            "message": {"content": [
                {"type": "text", "text": "one "},
                {"type": "thinking", "thinking": "IGNORED"},
                {"type": "text", "text": "two"},
            ]}
        }));
        assert_eq!(sink.into_text().expect("text"), "one two");
    }

    /// An `is_error` result carries claude's failure text, not an answer.
    #[test]
    fn an_error_result_is_not_treated_as_an_answer() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "result", "is_error": true, "result": "Prompt is too long"}));
        assert!(sink.into_text().is_err());
    }

    /// A stream with no text at all is a 502, not an empty 200 — an empty
    /// answer would reach the pet as a blank speech bubble.
    #[test]
    fn a_textless_stream_is_a_bad_gateway() {
        let sink = TextSink::default();
        sink.on_event(&json!({"type": "system", "subtype": "init"}));
        assert_eq!(sink.into_text().expect_err("no text").status, 502);
    }

    /// The rotation trigger, pinned end to end: the driver's typed overflow
    /// sentinel is the thing — and, of the statuses reachable from a turn, the
    /// only thing — that `bridge.rs` retires a session for (#667). If a second
    /// `ClaudeError` ever maps to this status it would start rotating sessions
    /// that are not full, so the exclusivity is asserted too.
    #[test]
    fn only_the_overflow_sentinel_triggers_a_rotation() {
        assert_eq!(
            map_error(&ClaudeError::PromptTooLong).status,
            crate::session::OVERFLOW_STATUS
        );
        for other in [
            ClaudeError::RateLimited,
            ClaudeError::AuthFailed,
            ClaudeError::IdleTimeout,
            ClaudeError::SessionNotFound,
        ] {
            assert_ne!(
                map_error(&other).status,
                crate::session::OVERFLOW_STATUS,
                "{other} would trigger a rotation"
            );
        }
    }

    /// Only the persisted-session backend accumulates state a rotation can
    /// retire. **Both** reprompt engines are non-persisted — the API one holds
    /// no claude session state at all, so a rotation there would spin the
    /// generation counter once per request and fix nothing (#667).
    #[test]
    fn only_the_subscription_backend_rotates() {
        use super::{Backend, Reprompt, Subscription};
        use crate::messages::{Client as MessagesClient, Thinking};
        use hive_claude::Config;
        use std::time::Duration;
        assert!(Backend::Subscription(Subscription::new(Config::default())).is_persisted());
        assert!(!Backend::Reprompt(Reprompt::new(Config::default())).is_persisted());
        let api = Reprompt::with_api(MessagesClient::new(
            "sk-ant-test".to_owned(),
            "claude-opus-5".to_owned(),
            Thinking::default(),
            Duration::from_secs(7),
        ));
        assert!(!Backend::Reprompt(api).is_persisted());
    }

    /// Each typed sentinel maps to a distinct, actionable status.
    #[test]
    fn sentinels_map_to_distinct_statuses() {
        assert_eq!(map_error(&ClaudeError::RateLimited).status, 429);
        assert_eq!(map_error(&ClaudeError::PromptTooLong).status, 413);
        assert_eq!(map_error(&ClaudeError::AuthFailed).status, 502);
        assert_eq!(map_error(&ClaudeError::IdleTimeout).status, 504);
        assert_eq!(map_error(&ClaudeError::SessionNotFound).status, 502);
        assert!(
            map_error(&ClaudeError::AuthFailed)
                .message
                .contains("/login")
        );
    }

    /// A long `Error::Exit` stderr tail must not swamp the caller's log line.
    #[test]
    fn error_text_is_clamped() {
        let long = "x".repeat(5_000);
        assert!(truncate(&long, 300).chars().count() <= 301);
        assert_eq!(truncate("short", 300), "short");
        // Multi-byte input must not be split mid-char.
        assert_eq!(truncate("äöü", 2), "äö…");
    }

    /// With no record, the reprompt path sends the client's transcript verbatim.
    #[test]
    fn reprompt_falls_back_to_the_client_transcript() {
        let records = Records::default();
        let transcript = vec![msg("system", "persona"), msg("user", "hello")];
        assert_eq!(records.compose(turn("t", &transcript)), transcript);
    }

    /// With a record, the reprompt path replays it plus the delta — including
    /// the assistant turns a stateless client dropped.
    #[test]
    fn reprompt_replays_its_own_record_plus_the_delta() {
        let mut records = Records::default();
        let turn1 = vec![msg("system", "persona"), msg("user", "hello")];
        records.remember("t", turn1.clone(), "hi there");

        // The client forgot the assistant turn and just appended a new question.
        let transcript = vec![msg("system", "persona"), msg("user", "again")];
        let composed = records.compose(turn("t", &transcript));
        assert_eq!(
            composed,
            vec![
                msg("system", "persona"),
                msg("user", "hello"),
                msg("assistant", "hi there"),
                msg("user", "again"),
            ]
        );
    }

    /// A record is bounded and keeps its head (the persona) when it trims.
    #[test]
    fn records_are_bounded_and_pin_the_head() {
        let mut records = Records::default();
        let mut convo = vec![msg("system", "PERSONA")];
        for n in 0..200 {
            convo.push(msg("user", &format!("turn {n}")));
            records.remember("t", convo.clone(), "ok");
            convo = records.by_title["t"].clone();
        }
        let record = &records.by_title["t"];
        assert!(record.len() <= super::MAX_RECORD_MESSAGES);
        assert_eq!(record[0], msg("system", "PERSONA"));
    }

    /// The record map itself is bounded.
    #[test]
    fn the_record_map_is_bounded() {
        let mut records = Records::default();
        for n in 0..200 {
            records.remember(&format!("t{n}"), vec![msg("user", "hi")], "ok");
        }
        assert!(records.by_title.len() <= super::MAX_RECORDS);
    }

    // ---- #693: same-title turns must not run concurrently ------------------

    /// A stand-in for `Claude::run`: it records what it was asked to attach to,
    /// keeps its own idea of which sessions exist, and — the point — reports
    /// how many turns were ever inside it at once.
    #[derive(Debug, Default)]
    struct Fake {
        /// `("resume" | "create", title)`, in the order the fake saw them.
        calls: Mutex<Vec<(&'static str, String)>>,
        /// Sessions this fake has created; a resume misses until one exists.
        sessions: Mutex<HashSet<String>>,
        inside: AtomicUsize,
        /// The most turns ever inside the fake at one time. `1` is the whole
        /// claim of #693.
        peak: AtomicUsize,
    }

    impl Fake {
        /// One fake turn, wide enough for the other racer to get inside it.
        ///
        /// The state is read and written *after* the yields on purpose: that is
        /// what lets two unguarded first turns both observe "no such session"
        /// before either creates one.
        async fn run(&self, attach: Attach, _prompt: String) -> Result<String, RunError> {
            let inside = self.inside.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(inside, Ordering::SeqCst);
            // A create replays the whole transcript where a resume sends only
            // the delta, so it really is the slower of the two. Modelled,
            // because it is what makes the resume-then-create window
            // observable at all: a lock covering only the resume lets the
            // loser's resume run *while* the winner is still inside its
            // create, and miss. With both arms equally fast the deterministic
            // interleaving below happens to hide that, which is a property of
            // the test rather than of the code.
            let stay = match &attach {
                Attach::Create(_) => YIELDS * 3,
                _ => YIELDS,
            };
            for _ in 0..stay {
                tokio::task::yield_now().await;
            }
            let outcome = match &attach {
                Attach::Resume(title) => {
                    self.record("resume", title);
                    if self.sessions.lock().expect("sessions").contains(title) {
                        Ok(format!("resumed {title}"))
                    } else {
                        Err(RunError::NoSession)
                    }
                }
                Attach::Create(title) => {
                    self.record("create", title);
                    self.sessions
                        .lock()
                        .expect("sessions")
                        .insert(title.to_owned());
                    Ok(format!("created {title}"))
                }
                other => panic!("the subscription path never attaches as {other:?}"),
            };
            self.inside.fetch_sub(1, Ordering::SeqCst);
            outcome
        }

        fn record(&self, kind: &'static str, title: &str) {
            self.calls
                .lock()
                .expect("calls")
                .push((kind, title.to_owned()));
        }

        /// How many times the fake was asked to create `title`.
        fn creates(&self, title: &str) -> usize {
            self.calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(kind, seen)| *kind == "create" && seen == title)
                .count()
        }
    }

    /// Two concurrent turns on one title must not overlap.
    ///
    /// This is #693's measured failure: unguarded, both enter `Claude::run`,
    /// and claude forks the session's transcript into a tree — silently, with
    /// both callers getting a plausible answer. Both turns here take the
    /// **resume** arm, the shape that was actually reproduced.
    #[tokio::test]
    async fn concurrent_turns_on_one_title_never_overlap() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        fake.sessions
            .lock()
            .expect("sessions")
            .insert("t".to_owned());
        let transcript = vec![msg("system", "persona"), msg("user", "hi")];

        let (one, two) = tokio::join!(
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        assert_eq!(one.expect("answered"), "resumed t");
        assert_eq!(two.expect("answered"), "resumed t");
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            1,
            "two turns were inside claude at once — the session forks here"
        );
        assert_eq!(locks.len(), 0, "the title's lock outlived its holders");
    }

    /// The half that is easy to miss: two concurrent **first** turns.
    ///
    /// Both miss the resume, so a lock covering only the resume would let both
    /// create — two session files under one title, which `--resume` then picks
    /// between by readdir order. Exactly one create, and the loser resumes the
    /// winner's session rather than minting a rival.
    #[tokio::test]
    async fn concurrent_first_turns_create_exactly_one_session() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        let transcript = vec![msg("system", "persona"), msg("user", "hi")];

        let (one, two) = tokio::join!(
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        one.expect("answered");
        two.expect("answered");
        assert_eq!(
            fake.creates("t"),
            1,
            "one title, two sessions on disk — `--resume` would pick by readdir order"
        );
        assert_eq!(
            *fake.calls.lock().expect("calls"),
            vec![
                ("resume", "t".to_owned()),
                ("create", "t".to_owned()),
                ("resume", "t".to_owned()),
            ],
            "the second turn should have joined the first's session"
        );
        assert_eq!(fake.sessions.lock().expect("sessions").len(), 1);
    }

    /// Per title, not global. A global lock would pass both tests above and
    /// still be wrong: it parks the pet behind caw on every request whose title
    /// differs, which is the common case.
    #[tokio::test]
    async fn turns_on_different_titles_still_run_concurrently() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        let transcript = vec![msg("user", "hi")];

        let (pet, caw) = tokio::join!(
            super::resume_then_create(turn("pet", &transcript), &locks, |a, p| fake.run(a, p)),
            super::resume_then_create(turn("caw", &transcript), &locks, |a, p| fake.run(a, p)),
        );

        pet.expect("answered");
        caw.expect("answered");
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            2,
            "unrelated conversations were serialised against each other"
        );
    }

    /// The map-entry cleanup race: a release that another task is already
    /// waiting on must **not** retire the entry.
    ///
    /// If it did, the waiter would hold a mutex the map no longer names, and
    /// the next arrival would mint a second one and not exclude it — the exact
    /// overlap the lock exists to prevent. The waiter therefore has to still
    /// find its own entry in the map once it wakes.
    #[tokio::test]
    async fn releasing_a_contended_title_keeps_its_entry() {
        let locks = TitleLocks::default();
        let seen_by_waiter = Mutex::new(None);

        let holder = async {
            let guard = locks.acquire("t").await;
            // Let the waiter reach the wait queue before releasing.
            for _ in 0..YIELDS {
                tokio::task::yield_now().await;
            }
            drop(guard);
        };
        let waiter = async {
            let guard = locks.acquire("t").await;
            *seen_by_waiter.lock().expect("seen") = Some(locks.len());
            drop(guard);
        };
        tokio::join!(holder, waiter);

        assert_eq!(
            *seen_by_waiter.lock().expect("seen"),
            Some(1),
            "the waiter's entry was retired out from under it"
        );
        assert_eq!(locks.len(), 0, "the entry outlived its last holder");
    }

    /// The map is a cache of live contention, not a log of every title the
    /// process has ever answered under.
    #[tokio::test]
    async fn the_lock_map_does_not_accumulate_titles() {
        let locks = TitleLocks::default();
        for n in 0..200 {
            let guard = locks.acquire(&format!("t{n}")).await;
            assert_eq!(locks.len(), 1);
            drop(guard);
        }
        assert_eq!(locks.len(), 0);
    }

    // ---- #1550: a waiter dropped while queued retires its title's entry -----

    /// Poll `fut` once, by hand, with a waker that does nothing.
    ///
    /// The tests below are about *when* a queued waiter is next polled, or
    /// that it never is: a release hands the lock to the head of the queue and
    /// wakes it, and the window #1550 is about lies between that wake and the
    /// poll that would take the lock. A scheduler decides that on its own; this
    /// leaves it to the test, so the window is hit every run rather than on a
    /// lucky interleaving.
    fn poll_once<F: Future + ?Sized>(fut: Pin<&mut F>) -> Poll<F::Output> {
        fut.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// **The leak, closed.** A turn queued behind another on its title is
    /// dropped *after* the holder let go but *before* it was polled to take the
    /// lock — a runtime shutdown dropping the task, say (not a client hanging
    /// up, which drops nothing: the turn runs on). The holder's release
    /// rightly kept the entry, because the waiter still held a clone of its
    /// mutex; before #1550 nothing ran the cleanup after that, so the entry
    /// stayed until the title's next turn. One per cancelled title, without
    /// bound over a session with many distinct titles.
    #[tokio::test]
    async fn a_waiter_dropped_after_the_release_retires_the_entry() {
        let locks = TitleLocks::default();
        let holder = locks.acquire("t").await;
        let mut waiter = Box::pin(locks.acquire("t"));
        assert!(
            poll_once(waiter.as_mut()).is_pending(),
            "the held title let a second turn in"
        );

        drop(holder);
        assert_eq!(locks.len(), 1, "the release retired an entry a waiter held");
        // The lock is now the waiter's, handed over but not yet taken.
        drop(waiter);

        assert_eq!(
            locks.len(),
            0,
            "a waiter dropped while queued left its title's entry behind"
        );
        // And it gave back the lock it had been handed: the title is free.
        let mut next = Box::pin(locks.acquire("t"));
        let Poll::Ready(next) = poll_once(next.as_mut()) else {
            panic!("the dropped waiter kept the lock it was handed");
        };
        assert_eq!(locks.len(), 1);
        drop(next);
        assert_eq!(locks.len(), 0);
    }

    /// The other half: a dropped waiter must **not** retire an entry that
    /// another waiter still needs. If it did, the remaining waiter would be
    /// queued on a mutex the map no longer names, and a newcomer would mint a
    /// second one and not be excluded from it — the overlap the lock exists to
    /// prevent, which is why "no other holder or waiter" is the condition and
    /// not merely "this one gave up".
    #[tokio::test]
    async fn a_dropped_waiter_keeps_the_entry_another_waiter_needs() {
        let locks = TitleLocks::default();
        let holder = locks.acquire("t").await;
        let mut dropped = Box::pin(locks.acquire("t"));
        let mut kept = Box::pin(locks.acquire("t"));
        assert!(poll_once(dropped.as_mut()).is_pending());
        assert!(poll_once(kept.as_mut()).is_pending());

        // The release hands the lock to the head of the queue, which then
        // gives up without taking it.
        drop(holder);
        drop(dropped);

        assert_eq!(
            locks.len(),
            1,
            "the dropped waiter retired the entry another waiter is queued on"
        );
        let mut newcomer = Box::pin(locks.acquire("t"));
        assert!(
            poll_once(newcomer.as_mut()).is_pending(),
            "a newcomer was not excluded from the waiter still queued"
        );
        let Poll::Ready(kept) = poll_once(kept.as_mut()) else {
            panic!("the lock the dropped waiter was handed never reached the next in line");
        };
        // The newcomer gives up too, this time while the title is held.
        drop(newcomer);
        assert_eq!(locks.len(), 1, "the entry went while its title was held");
        drop(kept);
        assert_eq!(locks.len(), 0);
    }

    /// The path that does not give up is unchanged: an uncontended title is
    /// taken on the first poll, a waiter polled after the release takes the
    /// lock, and dropping the *finished* acquire — which handed its guard out
    /// — retires nothing while that guard is held.
    #[tokio::test]
    async fn a_waiter_polled_after_the_release_takes_the_lock() {
        let locks = TitleLocks::default();
        let mut first = Box::pin(locks.acquire("t"));
        let Poll::Ready(holder) = poll_once(first.as_mut()) else {
            panic!("an uncontended title had to wait");
        };
        let mut waiter = Box::pin(locks.acquire("t"));
        assert!(poll_once(waiter.as_mut()).is_pending());

        drop(holder);
        let Poll::Ready(guard) = poll_once(waiter.as_mut()) else {
            panic!("the waiter was not handed the released lock");
        };
        drop(waiter);
        drop(first);

        assert_eq!(locks.len(), 1, "a finished acquire retired a held entry");
        drop(guard);
        assert_eq!(locks.len(), 0);
    }

    /// The tests above pin the window on one thread. Here the release and the
    /// waiters' cancellation race for real: each round, one task holds the
    /// title while three others queue on it, and then the holder lets go on
    /// one worker while the waiters drop their queued acquires on others,
    /// never polling them again. However that interleaves — the release
    /// before, between or after the drops, and the lock handed to a waiter
    /// that is already gone — the last of them out must retire the entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropped_waiters_leave_no_entry_on_a_multi_threaded_runtime() {
        const ROUNDS: usize = 256;
        const WAITERS: usize = 3;

        let locks = Arc::new(TitleLocks::default());
        for round in 0..ROUNDS {
            let title = Arc::new(format!("t{round}"));
            // Everyone arrives once the holder has the lock, and again once
            // every waiter is queued; the second crossing is the race.
            let held = Arc::new(Barrier::new(1 + WAITERS));
            let queued = Arc::new(Barrier::new(1 + WAITERS));
            let mut tasks = tokio::task::JoinSet::new();
            {
                let (locks, title) = (Arc::clone(&locks), Arc::clone(&title));
                let (held, queued) = (Arc::clone(&held), Arc::clone(&queued));
                tasks.spawn(async move {
                    let guard = locks.acquire(&title).await;
                    held.wait().await;
                    queued.wait().await;
                    drop(guard);
                });
            }
            for _ in 0..WAITERS {
                let (locks, title) = (Arc::clone(&locks), Arc::clone(&title));
                let (held, queued) = (Arc::clone(&held), Arc::clone(&queued));
                tasks.spawn(async move {
                    held.wait().await;
                    let mut waiting = Box::pin(locks.acquire(&title));
                    assert!(poll_once(waiting.as_mut()).is_pending());
                    queued.wait().await;
                    drop(waiting);
                });
            }
            while let Some(joined) = tasks.join_next().await {
                joined.expect("no panic");
            }
            assert_eq!(locks.len(), 0, "round {round} left its title's entry");
        }
    }

    // ---- the request deadline kills and reaps the child --------------------

    /// What [`super::KillOnDrop`] says when it fires — a fragment of its
    /// `warn!`, so a reworded line turns the positive assertions below red
    /// rather than leaving the zero-counts silently vacuous.
    const ABANDONED: &str = "claude's turn was dropped before it finished";

    /// The `WARN` lines this thread logs while it lives — the workspace's one
    /// capture harness, `hytte_config::test_support::capture`, plus a count.
    ///
    /// A *scoped* default, so a sibling test on another thread cannot write
    /// into it; `#[tokio::test]`'s current-thread runtime is what keeps every
    /// poll — and every drop — of the turn under test on this thread.
    ///
    /// Not a hand-rolled `set_default`, which is what this was first and what
    /// failed 21 of 40 runs beside bridge.rs's hung-turn tests (#1623 review).
    /// A fresh `Dispatch` re-derives the interest of every callsite registered
    /// *before* it, but while it is the only live dispatcher, a callsite first
    /// registered *after* it by a thread with no subscriber is cached
    /// `Interest::never()` for the whole process (`tracing-core`'s
    /// `Rebuilder::JustOne`, #1022) — and bridge.rs's tests reach the deadline
    /// path's `warn!` from exactly such threads. `capture` installs the
    /// process-wide `AlwaysInterested` global default first, which makes that
    /// thread impossible; its module doc has the measurements.
    struct Warnings {
        captured: hytte_config::test_support::Captured,
        _scope: tracing::dispatcher::DefaultGuard,
    }

    impl Warnings {
        fn capture() -> Self {
            let (captured, scope) = hytte_config::test_support::capture();
            Self {
                captured,
                _scope: scope,
            }
        }

        /// How many captured `WARN` messages contain `needle`.
        fn count(&self, needle: &str) -> usize {
            self.captured
                .warnings()
                .iter()
                .filter(|message| message.contains(needle))
                .count()
        }
    }

    /// **The orphan, closed.** A `claude` that stalls without a byte of output
    /// is killed with `SIGKILL` at the deadline and reaped before `run_until`
    /// hands the 504 back — so before the title lock and the request permit
    /// above it can be released.
    ///
    /// Before the fix this was `hive-claude`'s idle watchdog's job, and at the
    /// default budget it could not do it (5 s probes against a 7 s window, an 8
    /// s outer timeout): the turn was dropped and the stand-in here would have
    /// slept on, unreaped, for its full 30 s.
    ///
    /// The claim is the `/proc` assertion: the child is gone — reaped, not
    /// merely killed — when `run_until` returns. The wall clock is checked only
    /// coarsely, and on purpose. Its lower bound is exact (tokio timers never
    /// fire early, so the turn really was held to its deadline), but its upper
    /// bound is a generous `STALL_SLACK` rather than [`super::REAP_GRACE`]:
    /// on a loaded CI runner, scheduler lateness lands in that number too, and
    /// a bound tight enough to tell a reap from a grace expiry would fail on a
    /// slow machine rather than on a regression. What it still catches is the
    /// deadline not being enforced at all — the stand-in sleeps for 30 s.
    ///
    /// It also pins that the deadline's kill is the deadline's own, reported
    /// in its own words: the abandoned-turn guard (#1550) is disarmed on this
    /// path, so it neither signals a pid the deadline has just reaped nor
    /// reports this turn as dropped.
    #[tokio::test]
    async fn a_silently_hung_child_is_killed_and_reaped_at_the_deadline() {
        use super::{run_until, stand_in};
        use hive_claude::Config;
        use std::time::Duration;
        use tokio::time::Instant;

        /// How late past the deadline the answer may land before this test
        /// calls it a hang — scheduler lateness on a loaded runner included.
        const STALL_SLACK: Duration = Duration::from_secs(5);

        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            program: Some(stand_in::hung_claude(dir.path())),
            ..Config::default()
        };
        let warnings = Warnings::capture();
        // Long enough that a loaded machine still gets the stand-in to its
        // marker before the kill; the claim is about what happens after it.
        let budget = Duration::from_secs(2);
        let started = Instant::now();
        let outcome = run_until(
            &config,
            &Attach::OneOff,
            "hi",
            &TextSink::default(),
            started + budget,
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            outcome.expect_err("the deadline, not an answer").status,
            504
        );
        assert!(
            (budget..budget + STALL_SLACK).contains(&elapsed),
            "answered after {elapsed:?}: not at the {budget:?} deadline",
        );
        let [pid] = stand_in::started(dir.path())[..] else {
            panic!("expected exactly one stand-in run");
        };
        assert!(
            !stand_in::present(pid),
            "the child ({pid}) is still in /proc after its 504 — running, or dead but unreaped",
        );
        assert_eq!(
            warnings.count("did not finish within the request budget"),
            1,
            "the deadline's kill went unreported (or this capture hears nothing)",
        );
        assert_eq!(
            warnings.count(ABANDONED),
            0,
            "the deadline's own kill was also reported as an abandoned turn",
        );
    }

    /// **The orphan, closed for a dropped turn too (#1550).** A turn whose
    /// future is dropped mid-flight — `bridge.rs`'s backstop, a panic unwinding
    /// through it, the runtime shutting down — takes its `claude` with it,
    /// rather than leaving the child running on the owner's subscription.
    ///
    /// Before the guard the stand-in here slept on for its full 30 s, unreaped:
    /// `hive-claude` sets no `kill_on_drop`, so dropping the driver's `Child`
    /// only handed tokio a process that was still running. Now the guard's
    /// `SIGKILL` lands first, and that same hand-off becomes a zombie for
    /// tokio's orphan reaper — so "out of `/proc`" (killed **and** reaped) is
    /// what this waits for, bounded well under those 30 s. The wait is a poll
    /// rather than a single check because nothing in a drop can await the
    /// reap: it happens on the next `SIGCHLD` the runtime sees.
    #[tokio::test]
    async fn a_turn_dropped_mid_flight_kills_its_child() {
        use super::{run_until, stand_in};
        use hive_claude::Config;
        use std::time::Duration;
        use tokio::time::Instant;

        /// How long the kill and the orphan reap together may take before
        /// this test calls the child orphaned — generous for a loaded runner,
        /// and still far under the stand-in's 30 s sleep.
        const REAP_SLACK: Duration = Duration::from_secs(5);

        let dir = tempfile::tempdir().expect("tempdir");
        let config = Config {
            program: Some(stand_in::hung_claude(dir.path())),
            ..Config::default()
        };
        let sink = TextSink::default();
        let warnings = Warnings::capture();

        // A deadline nobody reaches: the only way this turn ends is a drop.
        let far = Instant::now() + Duration::from_hours(1);
        let mut turn = Box::pin(run_until(&config, &Attach::OneOff, "hi", &sink, far));
        let pid = tokio::select! {
            outcome = &mut turn => panic!("the hung turn ended on its own: {outcome:?}"),
            pids = stand_in::wait_for(dir.path(), 1) => pids[0],
        };
        assert!(stand_in::present(pid), "the child ({pid}) is not up yet");
        drop(turn);

        let reaped_by = Instant::now() + REAP_SLACK;
        while stand_in::present(pid) {
            assert!(
                Instant::now() < reaped_by,
                "the child ({pid}) is still in /proc {REAP_SLACK:?} after its turn was dropped \
                 — orphaned and running, or killed but never reaped",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            warnings.count(ABANDONED),
            1,
            "the abandoned turn's kill was not reported exactly once",
        );
    }

    /// The guard's positive control: a turn that ends on its own was never
    /// abandoned, so the guard is disarmed and sends nothing — a `SIGKILL`
    /// there would go to a pid the driver may already have reaped, i.e. to
    /// whatever the kernel hands that number next, and the warning would cry
    /// wolf on every answered turn.
    ///
    /// The zero is only worth something if this capture would have heard the
    /// line, so the same capture then watches a turn that really is abandoned
    /// and must count exactly one.
    #[tokio::test]
    async fn a_turn_that_ends_on_its_own_trips_no_guard() {
        use super::{run_until, stand_in};
        use hive_claude::Config;
        use std::time::Duration;
        use tokio::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let far = Instant::now() + Duration::from_hours(1);
        let warnings = Warnings::capture();

        let answering = Config {
            program: Some(stand_in::answering_claude(dir.path())),
            ..Config::default()
        };
        let sink = TextSink::default();
        let outcome = run_until(&answering, &Attach::OneOff, "hi", &sink, far).await;
        assert!(matches!(outcome, Ok(Ok(()))), "{outcome:?}");
        assert_eq!(sink.into_text().expect("the stand-in's answer"), "answered");
        assert_eq!(
            warnings.count(ABANDONED),
            0,
            "a turn that answered was reported as abandoned",
        );

        let hung = Config {
            program: Some(stand_in::hung_claude(dir.path())),
            ..Config::default()
        };
        let sink = TextSink::default();
        let mut turn = Box::pin(run_until(&hung, &Attach::OneOff, "hi", &sink, far));
        tokio::select! {
            outcome = &mut turn => panic!("the hung turn ended on its own: {outcome:?}"),
            _ = stand_in::wait_for(dir.path(), 1) => {}
        }
        drop(turn);
        assert_eq!(
            warnings.count(ABANDONED),
            1,
            "this capture does not hear the guard, so the zero above proves nothing",
        );
    }

    /// The deadline path disarms the guard **at its kill**, not once the reap
    /// grace is over — so a turn dropped *during* that grace sends nothing more.
    ///
    /// That window is real: with a grandchild holding the pipes, the driver's
    /// `child.wait()` reaps the killed child within milliseconds while `turn`
    /// stays pending for up to [`super::REAP_GRACE`], and a guard still armed
    /// there would `SIGKILL` a freed pid. "Disarm once, at the end" is a
    /// natural refactor that the suite did not see (#1623 review, M1).
    ///
    /// Deterministic rather than timed: the turn is not polled while its
    /// deadline passes, then polled exactly once — which takes the deadline
    /// branch (kill, disarm) and parks in the reap grace, held open by the
    /// stand-in's 3 s grandchild — and then dropped.
    #[tokio::test]
    async fn a_drop_during_the_deadlines_reap_grace_sends_nothing() {
        use super::{run_until, stand_in};
        use hive_claude::Config;
        use std::time::Duration;
        use tokio::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let program = dir.path().join("forking-claude");
        // `sleep 3 &` inherits stdout/stderr, so the driver's `wait` is still
        // pending after the deadline's kill, whatever the load.
        stand_in::install(
            &program,
            "#!/bin/sh\ndir=\"${0%/*}\"\ncat > /dev/null\nsleep 3 &\n: > \"$dir/$$.pid\"\nexec sleep 30\n",
        );
        let config = Config {
            program: Some(program),
            ..Config::default()
        };
        let sink = TextSink::default();
        let warnings = Warnings::capture();
        let deadline = Instant::now() + Duration::from_millis(1500);
        let mut turn = Box::pin(run_until(&config, &Attach::OneOff, "hi", &sink, deadline));
        tokio::select! {
            biased;
            outcome = &mut turn => panic!("the turn ended before its deadline: {outcome:?}"),
            _ = stand_in::wait_for(dir.path(), 1) => {}
        }
        tokio::time::sleep_until(deadline).await;
        let polled = std::future::poll_fn(|cx| Poll::Ready(turn.as_mut().poll(cx))).await;
        assert!(
            polled.is_pending(),
            "the grandchild holds the pipes, so the reap grace is still running: {polled:?}",
        );
        drop(turn);
        assert_eq!(
            warnings.count(ABANDONED),
            0,
            "a drop inside the deadline's reap grace was reported as abandoned",
        );
    }

    /// The guard's kill is `SIGKILL`, which a child cannot ignore. The hung
    /// stand-in dies of `SIGINT` too, so the drop test above cannot tell; real
    /// `claude` may well install a `SIGINT` handler (#1623 review, M2). This
    /// stand-in ignores `SIGINT` and `SIGTERM` — `SIG_IGN` survives the
    /// `exec` — so only an uncatchable signal takes it out of `/proc`.
    #[tokio::test]
    async fn the_guards_kill_cannot_be_ignored() {
        use super::{run_until, stand_in};
        use hive_claude::Config;
        use std::time::Duration;
        use tokio::time::Instant;

        /// As in `a_turn_dropped_mid_flight_kills_its_child`.
        const REAP_SLACK: Duration = Duration::from_secs(5);

        let dir = tempfile::tempdir().expect("tempdir");
        let program = dir.path().join("stubborn-claude");
        stand_in::install(
            &program,
            "#!/bin/sh\ndir=\"${0%/*}\"\ncat > /dev/null\ntrap '' INT TERM\n: > \"$dir/$$.pid\"\nexec sleep 30\n",
        );
        let config = Config {
            program: Some(program),
            ..Config::default()
        };
        let sink = TextSink::default();
        let far = Instant::now() + Duration::from_hours(1);
        let mut turn = Box::pin(run_until(&config, &Attach::OneOff, "hi", &sink, far));
        let pid = tokio::select! {
            outcome = &mut turn => panic!("the stubborn turn ended on its own: {outcome:?}"),
            pids = stand_in::wait_for(dir.path(), 1) => pids[0],
        };
        drop(turn);

        let reaped_by = Instant::now() + REAP_SLACK;
        while stand_in::present(pid) {
            assert!(
                Instant::now() < reaped_by,
                "the SIGINT-ignoring child ({pid}) survived the guard for {REAP_SLACK:?}",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A turn whose deadline has already passed — it spent the budget queued
    /// behind another turn on its title — must not start a child just to kill
    /// it. Proved with a program that does not exist: had a spawn been
    /// attempted, the outcome would be the driver's `Error::Spawn`, not the 504.
    #[tokio::test]
    async fn a_spent_deadline_spawns_nothing() {
        use super::run_until;
        use hive_claude::Config;

        let config = Config {
            program: Some("/nonexistent/hytte-bridge-test/claude".into()),
            ..Config::default()
        };
        let outcome = run_until(
            &config,
            &Attach::OneOff,
            "hi",
            &TextSink::default(),
            tokio::time::Instant::now(),
        )
        .await;
        assert_eq!(outcome.expect_err("refused before spawning").status, 504);
    }

    /// Queuing for the title lock is bounded by the deadline too: a turn that
    /// spent its whole budget behind another one on its title answers its 504
    /// at the deadline and never runs, rather than whenever the holder lets
    /// go. On a paused clock, so the second it waits costs no real time.
    #[tokio::test(start_paused = true)]
    async fn a_turn_queued_on_its_title_past_the_deadline_never_runs() {
        let locks = TitleLocks::default();
        let held = locks.acquire("t").await;
        let transcript = vec![msg("user", "hi")];
        let queued = Turn {
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            ..turn("t", &transcript)
        };
        let attempts = AtomicUsize::new(0);

        let out = super::resume_then_create(queued, &locks, |_attach, _prompt| {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(String::new()))
        })
        .await;

        assert_eq!(out.expect_err("the deadline").status, 504);
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            0,
            "it ran past its deadline"
        );
        drop(held);
        assert_eq!(locks.len(), 0, "the abandoned wait left its title's entry");
    }

    /// A turn that ends on its own is the driver's answer, untouched by the
    /// deadline machinery — here the spawn failure itself, which must still
    /// reach `map_error` as the typed `Error::Spawn` it is.
    #[tokio::test]
    async fn a_turn_that_ends_before_the_deadline_is_the_drivers_own_outcome() {
        use super::run_until;
        use hive_claude::Config;

        let config = Config {
            program: Some("/nonexistent/hytte-bridge-test/claude".into()),
            ..Config::default()
        };
        let outcome = run_until(
            &config,
            &Attach::OneOff,
            "hi",
            &TextSink::default(),
            tokio::time::Instant::now() + std::time::Duration::from_mins(1),
        )
        .await;
        assert!(
            matches!(outcome, Ok(Err(ClaudeError::Spawn { .. }))),
            "{outcome:?}"
        );
    }

    /// A resume that lands never creates — the delta rule depends on it.
    #[tokio::test]
    async fn a_resumed_session_is_not_recreated() {
        let locks = TitleLocks::default();
        let fake = Fake::default();
        fake.sessions
            .lock()
            .expect("sessions")
            .insert("t".to_owned());
        let transcript = vec![msg("user", "hi")];

        let reply =
            super::resume_then_create(turn("t", &transcript), &locks, |a, p| fake.run(a, p))
                .await
                .expect("answered");

        assert_eq!(reply, "resumed t");
        assert_eq!(fake.creates("t"), 0);
    }

    /// A failure that is not `SessionNotFound` is reported, not retried as a
    /// create: retrying a rate limit or an auth failure that way would mint a
    /// rival session per failed turn.
    #[tokio::test]
    async fn a_non_session_failure_is_not_retried_as_a_create() {
        let locks = TitleLocks::default();
        let attempts = AtomicUsize::new(0);
        let transcript = vec![msg("user", "hi")];

        let out = super::resume_then_create(turn("t", &transcript), &locks, |_attach, _prompt| {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Err(RunError::Failed(Failure::new(429, "rate-limited"))))
        })
        .await;

        assert_eq!(out.expect_err("failed").status, 429);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    /// The tests above pin the interleaving deterministically on a
    /// current-thread runtime. This one hands it to a real scheduler instead:
    /// 256 rounds of 4 racers on 4 worker threads — 1024 turns whose ordering
    /// nobody controls — and both invariants have to hold in every round.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_race_stays_closed_on_a_multi_threaded_runtime() {
        const ROUNDS: usize = 256;
        const RACERS: usize = 4;

        let locks = Arc::new(TitleLocks::default());
        let fake = Arc::new(Fake::default());
        for round in 0..ROUNDS {
            let title = format!("t{round}");
            let mut racers = tokio::task::JoinSet::new();
            for _ in 0..RACERS {
                let locks = Arc::clone(&locks);
                let fake = Arc::clone(&fake);
                let title = title.clone();
                racers.spawn(async move {
                    let transcript = vec![msg("user", "hi")];
                    super::resume_then_create(turn(&title, &transcript), &locks, |a, p| {
                        fake.run(a, p)
                    })
                    .await
                });
            }
            while let Some(joined) = racers.join_next().await {
                joined.expect("no panic").expect("answered");
            }
            assert_eq!(fake.creates(&title), 1, "round {round} created twice");
        }
        assert_eq!(
            fake.peak.load(Ordering::SeqCst),
            1,
            "turns on one title overlapped"
        );
        assert_eq!(locks.len(), 0);
    }
}
