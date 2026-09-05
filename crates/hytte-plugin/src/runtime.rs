//! The transport runtime extracted from the reference plugin (#275): dial +
//! bounded backoff, the `Register` handshake, and the read→update→render
//! session loop. A plugin author never touches this — [`run`] is the whole
//! surface.

use std::future::Future;
use std::time::{Duration, Instant};

use hytte_plugin_proto::{
    HostMsg, LogLevel, PluginMsg, ProtoError, StateKey, VOCAB_UNCONDITIONAL, read_frame,
    socket_path, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{Input, Plugin};

/// Reconnect backoff bounds: start small, cap so we never hammer the socket.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// A session that ends in **error** faster than this is an *immediate* failure —
/// long enough to complete `Register` + the seed render and be dropped, short
/// enough that a healthy session never looks immediate. Used to detect the #437
/// wire-vocab-skew crash-loop (session dies right after the first render, the SDK
/// redials, the identical tree re-renders, the older host drops it again).
const IMMEDIATE_FAILURE: Duration = Duration::from_secs(2);

/// Consecutive immediate failures before the runtime escalates its log from the
/// ordinary per-session line to a warning naming the likely cause (a plugin↔host
/// wire-vocabulary skew), so an otherwise near-silent 5 s crash-loop leaves a
/// trace a human can act on.
const SKEW_WARN_AFTER: u32 = 3;

/// Minimum interval between full `view()` recomputation + dedup + `write_frame`
/// passes in the session loop (~33 ms ≈ 30 Hz), the SDK-wide view-rate cap
/// (#560). `update()` still runs on **every** event so a plugin's model/
/// ballistics never lag; only the render step is coalesced. A high-frequency
/// plugin (e.g. the audio widget, whose 20 Hz frame tick and the host's ~23 Hz
/// spectrum push are independent events → ~43 view passes/s) is capped to one
/// render per interval instead. A plugin that emits below this rate is
/// unaffected: its events land ≥ this interval apart, so each renders
/// immediately (the leading edge). A burst that stops still renders its final
/// state — a suppressed view arms a deadline that flushes the trailing frame at
/// the interval boundary (see [`session`]).
const VIEW_MIN_INTERVAL: Duration = Duration::from_millis(33);

/// Bounded exponential reconnect backoff. [`delay`](Backoff::delay) yields the
/// current wait and doubles the next (capped); only a session that *lived*
/// past the cap resets it (via [`note_session`](Backoff::note_session)), so a
/// flapping host (accept-then-drop) can't defeat the backoff.
struct Backoff {
    next: Duration,
}

impl Backoff {
    fn new() -> Self {
        Self { next: BACKOFF_BASE }
    }

    /// The wait to sleep now; doubles the next one, capped at [`BACKOFF_CAP`].
    fn delay(&mut self) -> Duration {
        let d = self.next;
        self.next = d.saturating_mul(2).min(BACKOFF_CAP);
        d
    }

    /// Record a completed session: only one that lived at least the cap
    /// counts as stable and resets the backoff.
    fn note_session(&mut self, lived: Duration) {
        if lived >= BACKOFF_CAP {
            self.next = BACKOFF_BASE;
        }
    }
}

/// Tracks consecutive *immediate* session failures so the runtime can escalate
/// its log once a plugin is stuck in a silent redial crash-loop — the #437
/// wire-vocab-skew signature. Pure/stateful so it is unit-testable off the async
/// loop.
struct Redial {
    immediate_failures: u32,
}

impl Redial {
    fn new() -> Self {
        Self {
            immediate_failures: 0,
        }
    }

    /// Record a completed session and return whether the runtime should escalate
    /// its log to a skew warning. A clean end (`ended_ok`, i.e. host `Shutdown`)
    /// or any session that outlived [`IMMEDIATE_FAILURE`] resets the streak;
    /// otherwise the streak grows, and once it reaches [`SKEW_WARN_AFTER`] this
    /// returns `true` — the cue to warn about a likely host/plugin vocabulary
    /// skew instead of the ordinary per-session line.
    fn note(&mut self, lived: Duration, ended_ok: bool) -> bool {
        if ended_ok || lived >= IMMEDIATE_FAILURE {
            self.immediate_failures = 0;
            false
        } else {
            self.immediate_failures = self.immediate_failures.saturating_add(1);
            self.immediate_failures >= SKEW_WARN_AFTER
        }
    }
}

/// One session-loop iteration's work: fold a plugin-facing [`Input`] through
/// `update`, or perform a runtime-internal [`Rerender`](Step::Rerender) that
/// refreshes the view without an `update` — used when the host installs a new
/// accent (#376), which changes what the `preem` kit paints but is not a TEA
/// message, and when the host advertises its wire vocabulary (#882/#884), which
/// changes whether [`display`](crate::display) widgets rasterise or emit state.
enum Step<M> {
    Update(Input<M>),
    Rerender,
    /// The view-rate cap's deferred-render deadline fired (#560): recompute the
    /// view and send the coalesced trailing frame, with no `update` — like
    /// [`Rerender`](Step::Rerender), but triggered by the cap boundary rather
    /// than a host accent install.
    Flush,
}

/// Drive one connected session: handshake, seed render, then the
/// read→update→render loop. `Ok(())` means the host sent `Shutdown`; any
/// transport failure (EOF = the host went away) surfaces as `Err`. Either way
/// the caller redials — see the crate docs on why `Shutdown` does not exit.
///
/// Generic over the I/O halves (not `UnixStream`) so the whole loop is
/// hermetically testable over `tokio::io::duplex`.
// One cohesive session lifecycle (handshake → seed render → the select loop over
// every host frame → dedup); the length is the host-frame vocabulary, not
// branching complexity — splitting it would scatter the loop for no gain.
#[allow(clippy::too_many_lines)]
async fn session<P, R, W>(rd: R, mut wr: W) -> Result<(), ProtoError>
where
    P: Plugin,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
{
    // The negotiated generation lives in a thread-local (`display::NEGOTIATED`)
    // read by `view()` on whatever thread this future is polled on, so the
    // seed, the `Hello` and every render have to share one thread. `run` builds
    // a current-thread runtime and `block_on`s this, and only the frame reader
    // is spawned, so today they do. If they ever didn't, the feature would
    // disable itself in silence — a read on the wrong worker sees the `Cell`'s
    // default `0` and degrades to `Raster`, fail-safe and total, with every
    // test still green (#898 review R7).
    //
    // That is already impossible, but only *incidentally*: this future holds a
    // `MsgStream` — `Pin<Box<dyn Stream>>` with no `+ Send` — across its awaits,
    // so `tokio::spawn` has never accepted it. The `Rc` states the requirement
    // deliberately instead, so that adding `+ Send` to `MsgStream` for some
    // unrelated reason cannot quietly re-open the hole. Dropped explicitly at
    // the end so it is genuinely live across every await rather than something
    // the generator layout may elide; verified by probe — `tokio::spawn` of
    // this future names `Rc<()>` as the offending type.
    let thread_bound = std::rc::Rc::new(());

    // Handshake: `Register` MUST be the first frame (else the host drops us),
    // then a greeting through the host log (exercises the `Log` frame path).
    let mut manifest = P::manifest();
    // Auto-opt-in to the desktop-accent push (#376): the SDK knows how to
    // consume `HostMsg::Accent` (it feeds the `preem` kit's default tint), so it
    // declares the subscription on every plugin's behalf — accent tracking is
    // out-of-the-box, transparent to the plugin author. The host gates the push
    // on this key (#305), so a *pre-#376* SDK that never adds it simply never
    // receives the variant it couldn't decode.
    if !manifest.subscribes.contains(&StateKey::Accent) {
        manifest.subscribes.push(StateKey::Accent);
    }
    let plugin_id = manifest.id.clone();
    // Kept for the vocabulary negotiation (#884): `Manifest::negotiated_vocab`
    // is the proto's own arithmetic over `vocab_max` and the host's offer, and
    // both ends must compute the same number from the same two inputs — so the
    // negotiation reads the manifest that was actually registered rather than
    // re-deriving the ceiling by hand. One small clone per session.
    let negotiation = manifest.clone();
    // Seed the negotiated generation at the *unconditional* ceiling — what this
    // plugin may emit with no advertisement at all. A host that never sends
    // `Hello` leaves it here, which is below `PREEM_VOCAB`, so every
    // `display` widget CPU-rasterises exactly as it does today. Re-seeding on
    // every (re)connect is what makes a reconnect to an older shell degrade
    // instead of carrying the previous session's advertisement forward.
    //
    // Floored at `VOCAB_UNCONDITIONAL` rather than trusted from the manifest
    // (#898 review N2): `Manifest`'s fields are `pub`, so a plugin can hand-set
    // `vocab` above `PREEM_VOCAB`, and seeding from that would put the *seed*
    // render on the state arm — a `Node::Preem` at a host that has advertised
    // nothing, which is the #437 decode-fail crash loop this gate exists to
    // prevent. An older host's `check_vocab` refuses such a plugin anyway, so
    // this only closes the window against a current one.
    crate::display::set_negotiated(negotiation.vocab.min(VOCAB_UNCONDITIONAL));
    write_frame(&mut wr, &PluginMsg::Register { manifest }).await?;
    write_frame(
        &mut wr,
        &PluginMsg::Log {
            level: LogLevel::Info,
            msg: format!("{plugin_id} connected"),
        },
    )
    .await?;

    // The per-session command lane (#280): the runtime owns the channel, so
    // its lifecycle is exactly this session. `init` gets the sender (the model
    // sends on it from `update`), `sources` gets the receiver (its I/O task
    // drains it). Both ends die when this session's model and sources drop, so
    // a queued command never crosses a reconnect. Command-less plugins set
    // `Cmd = Infallible` and ignore both ends.
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<P::Cmd>();

    // Seed render: the fresh model's view goes out immediately, so the slot
    // mounts before the first state snapshot lands.
    let mut model = P::init(cmd_tx);
    let mut last_view = model.view();
    write_frame(
        &mut wr,
        &PluginMsg::Render {
            tree: last_view.tree.clone(),
            panel: last_view.panel.clone(),
            effects: Vec::new(),
        },
    )
    .await?;

    // View-rate cap state (#560). `next_send_allowed` is the earliest instant a
    // coalesced view send may go out; seeded to *now* (not now + interval) so the
    // first real event after this seed render still renders immediately — the
    // leading edge, matching the pre-#560 behavior for a plugin that emits below
    // the cap. `pending` marks a view change that was suppressed within the
    // interval and is waiting for the flush deadline to deliver the trailing
    // frame. Effects (one-shot, #277) always bypass the cap.
    let mut next_send_allowed = tokio::time::Instant::now();
    let mut pending = false;

    // `read_frame` is cancel-safe only at frame boundaries, so it must not
    // race in a `select!` arm (the losing future would drop mid-frame and
    // desync the stream). A reader task owns the read half and forwards whole
    // frames through a channel — channel recv *is* cancel-safe. Mirrors the
    // host's reader/writer task shape.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        let mut rd = rd;
        loop {
            let frame = read_frame::<HostMsg, _>(&mut rd).await;
            let end = frame.is_err();
            if tx.send(frame).is_err() || end {
                return;
            }
        }
    });

    // The plugin's own message sources, normalized to one never-yielding
    // stream when it has none. `src_done` keeps a finished stream from being
    // polled again (a `Stream` may panic after its final `None`). This is also
    // where the command receiver lands: a plugin's I/O task drains `cmd_rx`
    // and re-emits results as the app messages this stream carries.
    let mut src = P::sources(cmd_rx).unwrap_or_else(|| Box::pin(tokio_stream::pending()));
    let mut src_done = false;

    let result = loop {
        let step = tokio::select! {
            frame = rx.recv() => match frame {
                Some(Ok(HostMsg::StateSnapshot { snapshot })) => Step::Update(Input::Snapshot(snapshot)),
                Some(Ok(HostMsg::Event { node, kind })) => Step::Update(Input::Event { node, kind }),
                Some(Ok(HostMsg::EffectResult { id, outcome })) => {
                    Step::Update(Input::EffectResult { id, outcome })
                }
                Some(Ok(HostMsg::SlotVisibility { visible })) => Step::Update(Input::SlotVisible(visible)),
                Some(Ok(HostMsg::AudioSpectrum { spectrum })) => {
                    // #405: an audio-reactive frame (peak + bands), delivered only
                    // to a plugin that subscribed the key. Surface it to the model
                    // like any other app-level input.
                    Step::Update(Input::AudioSpectrum(spectrum))
                }
                // #487 phase 1b: the human's answer to a `RequestConsent` this
                // plugin raised (or `Deny` on the host's 60 s timeout).
                Some(Ok(HostMsg::ConsentDecision {
                    request_id,
                    decision,
                })) => Step::Update(Input::ConsentDecision {
                    request_id,
                    decision,
                }),
                // #484: the upcoming-calendar digest, delivered only to a plugin
                // that subscribed the key and holds `Capability::Calendar`.
                Some(Ok(HostMsg::CalendarUpcoming { events })) => {
                    Step::Update(Input::CalendarUpcoming(events))
                }
                // #484: the session lock state (seeded at register, then on change).
                Some(Ok(HostMsg::SessionLocked { locked })) => {
                    Step::Update(Input::SessionLocked(locked))
                }
                // #528: the now-playing digest off the mpris active player.
                Some(Ok(HostMsg::NowPlaying { now_playing })) => {
                    Step::Update(Input::NowPlaying(now_playing))
                }
                // #509: a datasource query forwarded to this provider plugin.
                // Answer it by returning an `Effect::DatasourceResult` echoing
                // `request_id` (the opaque host correlation).
                Some(Ok(HostMsg::DatasourceQuery {
                    request_id,
                    datasource,
                    scope,
                    params,
                })) => Step::Update(Input::DatasourceQuery {
                    request_id,
                    datasource,
                    scope,
                    params,
                }),
                // #509: the answer to a query this requester plugin issued, keyed by
                // its own `request_id`.
                Some(Ok(HostMsg::DatasourceResult {
                    request_id,
                    outcome,
                })) => Step::Update(Input::DatasourceResult {
                    request_id,
                    outcome,
                }),
                Some(Ok(HostMsg::Accent { color })) => {
                    // Theme plumbing (#376): the host resolved `@accent_color`
                    // and handed it over. Feed it to the `preem` kit as the
                    // default widget tint (an explicit plugin palette still
                    // wins), then fall through to re-render so the new default
                    // shows. Never surfaced to the TEA model.
                    crate::preem::set_accent(color);
                    Step::Rerender
                }
                Some(Ok(HostMsg::Hello { vocab })) => {
                    // #882/#884: the host's vocabulary advertisement, sent
                    // because `Manifest::new` declares a `vocab_max`. Runtime
                    // plumbing like `Accent` — never surfaced to the TEA model.
                    //
                    // Record what the two ends agreed on (the proto computes it;
                    // see `negotiation` above), then fall through to re-render:
                    // the seed frame already went out under the pre-`Hello`
                    // floor, so without this the plugin would keep shipping
                    // `Pixels` until its next `update` happened to fire. From
                    // here on `display`'s widgets emit `Node::Preem` and stop
                    // rasterising — and stop ticking their own animation, which
                    // the shell now owns.
                    crate::display::raise_negotiated(negotiation.negotiated_vocab(vocab));
                    Step::Rerender
                }
                Some(Ok(HostMsg::Ping { seq })) => {
                    // Liveness is runtime plumbing: answer, don't surface.
                    if let Err(e) = write_frame(&mut wr, &PluginMsg::Pong { seq }).await {
                        break Err(e);
                    }
                    continue;
                }
                Some(Ok(HostMsg::Shutdown)) => break Ok(()),
                Some(Err(e)) => break Err(e),
                // Reader gone without a final error: treat as EOF.
                None => break Err(ProtoError::Io(std::io::ErrorKind::UnexpectedEof.into())),
            },
            msg = src.next(), if !src_done => {
                if let Some(m) = msg {
                    Step::Update(Input::App(m))
                } else {
                    src_done = true;
                    continue;
                }
            },
            // The view-rate cap's trailing-frame deadline (#560): only armed
            // while a view change is `pending` (suppressed within the interval).
            // Firing at `next_send_allowed` guarantees the coalesced final state
            // of a burst is delivered even after the events stop.
            () = tokio::time::sleep_until(next_send_allowed), if pending => Step::Flush,
        };

        // update → view → dedup, behind the view-rate cap (#560). `update()`
        // runs on every event so the model/ballistics never lag; the render step
        // is coalesced. A `Rerender` (accent install, #376) or a `Flush` (the
        // cap deadline) refreshes the view without an `update`.
        let effects = match step {
            Step::Update(input) => model.update(input),
            Step::Rerender | Step::Flush => Vec::new(),
        };
        let view = model.view();
        // Dedup is unchanged: the whole `View` compares at once, so a panel
        // change while the chip tree is unchanged (the common case) still counts.
        let changed = view != last_view;
        let now = tokio::time::Instant::now();
        // Send iff there are effects (one-shot — never coalesced, they ride the
        // render frame), or the view changed AND the cap interval has elapsed
        // since the last send. A change within the interval is deferred: `pending`
        // arms the flush deadline (`next_send_allowed`) that delivers the
        // coalesced trailing frame.
        let send = !effects.is_empty() || (changed && now >= next_send_allowed);
        if send {
            let frame = PluginMsg::Render {
                tree: view.tree.clone(),
                panel: view.panel.clone(),
                effects,
            };
            if let Err(e) = write_frame(&mut wr, &frame).await {
                break Err(e);
            }
            last_view = view;
            next_send_allowed = now + VIEW_MIN_INTERVAL;
        }
        // A view change we couldn't send yet stays `pending` (keeps the flush arm
        // armed); anything else clears it (a send satisfied it, or there is no
        // outstanding change).
        pending = changed && !send;
    };
    // Stop reading; the caller drops the write half, which half-closes the
    // socket and lets the host reap the connection.
    reader.abort();
    // Keeps the `!Send` marker live across every await above — see its comment.
    drop(thread_bound);
    result
}

/// The connect→session→backoff loop, factored from [`run`] so a test can
/// drive it with an in-memory connector and pin the runtime's headline
/// decision: a session ending `Ok` (host `Shutdown`) **redials** — it never
/// terminates the loop (see the crate docs on why exiting would strand a
/// `Restart=on-failure` unit). A failed connect backs off the same way.
async fn reconnect_loop<P, R, W, C, Fut>(plugin_id: &str, mut connect: C) -> !
where
    P: Plugin,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
    C: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<(R, W)>>,
{
    let mut backoff = Backoff::new();
    let mut redial = Redial::new();
    loop {
        match connect().await {
            Ok((rd, wr)) => {
                let started = Instant::now();
                let outcome = session::<P, _, _>(rd, wr).await;
                let lived = started.elapsed();
                // Escalate the log iff we've hit a streak of immediate failures —
                // the #437 crash-loop signature — so it isn't a silent 5 s spin.
                let skew = redial.note(lived, outcome.is_ok());
                match outcome {
                    Ok(()) => eprintln!("[{plugin_id}] host shut down; will reconnect"),
                    Err(e) if skew => eprintln!(
                        "[{plugin_id}] WARNING: session keeps failing immediately ({e}); \
                         the host may be older than this plugin's wire vocabulary \
                         (schema skew, #437) — update the shell",
                    ),
                    Err(e) => eprintln!("[{plugin_id}] session ended: {e}"),
                }
                backoff.note_session(lived);
            }
            Err(e) => {
                eprintln!("[{plugin_id}] connect failed: {e}");
            }
        }
        tokio::time::sleep(backoff.delay()).await;
    }
}

/// Run a [`Plugin`] against the trollshell host socket — forever. Owns the
/// process: builds a current-thread tokio runtime, dials
/// [`socket_path`](hytte_plugin_proto::socket_path) with bounded exponential
/// backoff (a host that isn't up yet — both start under the same session
/// target — or a host restart is a transient we ride out here rather than
/// exiting into systemd's start-limit), and drives one session per
/// connection. Exits the process (status 1) only on unrecoverable setup:
/// `XDG_RUNTIME_DIR` unset (then there is nothing to dial, ever) or the
/// tokio runtime failing to build.
pub fn run<P: Plugin>() -> ! {
    let plugin_id = P::manifest().id;
    let Some(path) = socket_path() else {
        eprintln!("[{plugin_id}] XDG_RUNTIME_DIR unset; no host socket to dial");
        std::process::exit(1);
    };
    eprintln!("[{plugin_id}] dialing {}", path.display());

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("[{plugin_id}] tokio runtime failed to build: {e}");
            std::process::exit(1);
        }
    };

    rt.block_on(reconnect_loop::<P, _, _, _, _>(&plugin_id, move || {
        let path = path.clone();
        async move {
            let stream = UnixStream::connect(&path).await?;
            Ok(stream.into_split())
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        BACKOFF_BASE, BACKOFF_CAP, Backoff, IMMEDIATE_FAILURE, Redial, SKEW_WARN_AFTER,
        reconnect_loop, session,
    };
    use crate::display::{Marquee, StyleName};
    use crate::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
    use hytte_plugin_proto::preem::PREEM_VOCAB;
    use hytte_plugin_proto::{
        AudioSpectrum, Capability, ClockState, ConsentDecision, Effect, EffectOutcome, EventKind,
        HostMsg, Manifest, Mount, Node, Page, PluginMsg, SPECTRUM_BINS, StateKey, StateSnapshot,
        VOCAB, VOCAB_UNCONDITIONAL, read_frame, write_frame,
    };
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite, duplex};

    // ── Test plugins ─────────────────────────────────────────────────────────

    /// Host-driven: shows the latest snapshot's `iso` (or the outcome of an
    /// `EffectResult`); a click on `echo-btn` emits one `OpenPage` effect
    /// while the tree stays unchanged (the view doesn't depend on clicks) —
    /// exactly the effects-force-send case.
    struct Echo {
        iso: String,
    }

    impl Plugin for Echo {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            let mut m = Manifest::new("echo-test", Mount::SidebarTop);
            m.subscribes = vec![
                StateKey::Clock,
                StateKey::CalendarUpcoming,
                StateKey::SessionLocked,
                StateKey::NowPlaying,
            ];
            m.capabilities = vec![
                Capability::OpenPage,
                Capability::Calendar,
                Capability::SessionState,
                Capability::NowPlaying,
            ];
            m
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self {
                iso: "seed".to_owned(),
            }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            match input {
                Input::Snapshot(s) => {
                    if let Some(c) = s.clock {
                        self.iso = c.iso;
                    }
                    Vec::new()
                }
                Input::Event { node, kind } => {
                    if node == "echo-btn" && matches!(kind, EventKind::Click) {
                        vec![Effect::OpenPage(Page::PowerMenu)]
                    } else {
                        Vec::new()
                    }
                }
                Input::EffectResult { id, outcome } => {
                    self.iso = format!("cmd{id} ok={}", outcome.ok);
                    Vec::new()
                }
                Input::ConsentDecision {
                    request_id,
                    decision,
                } => {
                    self.iso = format!("consent{request_id}={decision:?}");
                    Vec::new()
                }
                // #484/#528 domain pushes — reflect each into the view so a test
                // can observe it arriving at `update`.
                Input::CalendarUpcoming(events) => {
                    self.iso = format!(
                        "cal:{}",
                        events
                            .iter()
                            .map(|e| e.title.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    Vec::new()
                }
                Input::SessionLocked(locked) => {
                    self.iso = format!("locked={locked}");
                    Vec::new()
                }
                Input::NowPlaying(np) => {
                    self.iso = format!("np:{}|{}|{}", np.title, np.artist, np.playing);
                    Vec::new()
                }
                // #509: as a provider, answer a forwarded query by echoing the host
                // correlation back in a `DatasourceResult`.
                Input::DatasourceQuery { request_id, .. } => {
                    vec![Effect::DatasourceResult {
                        request_id,
                        outcome: hytte_plugin_proto::DatasourceOutcome::Ready("echo".to_owned()),
                    }]
                }
                // #509: as a requester, reflect the query result into the view.
                Input::DatasourceResult {
                    request_id,
                    outcome,
                } => {
                    self.iso = format!("ds{request_id}={outcome:?}");
                    Vec::new()
                }
                Input::SlotVisible(_) | Input::AudioSpectrum(_) => Vec::new(),
                Input::App(never) => match never {},
            }
        }

        fn view(&self) -> View {
            Node::Label {
                id: Some("echo-lbl".to_owned()),
                text: self.iso.clone(),
                classes: Vec::new(),
            }
            .into()
        }
    }

    /// Reflects the latest slot-visibility push in its view, so a
    /// [`HostMsg::SlotVisibility`] arriving as [`Input::SlotVisible`] is
    /// observable as a re-render — the park-your-pollers signal in miniature.
    struct Watcher {
        visible: bool,
    }

    impl Plugin for Watcher {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("watcher-test", Mount::SidebarTop)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { visible: false }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::SlotVisible(visible) = input {
                self.visible = visible;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: None,
                text: if self.visible { "visible" } else { "hidden" }.to_owned(),
                classes: Vec::new(),
            }
            .into()
        }
    }

    /// Renders one [`crate::display`] widget, so a session test can observe
    /// which arm of the #884 negotiation the SDK picked — and, because a
    /// marquee's scroll is shell-owned in state mode and plugin-owned in raster
    /// mode, whether the plugin is still ticking its own animation.
    ///
    /// Deliberately the *one-code-path* shape a migrated plugin has: `update`
    /// calls `advance` unconditionally and `view` calls `node` unconditionally.
    /// Neither branches on the mode; that is the seam under test.
    struct Scroller {
        marquee: Marquee,
        text: String,
    }

    /// Wide enough that a 64 px window can't hold it, so the strip really
    /// scrolls (a held message ignores the offset — see `MarqueeStrip::window`)
    /// and a plugin-side tick is actually observable as a different buffer.
    const SCROLL_TEXT: &str = "A LONG ENOUGH MESSAGE TO OVERFLOW THE WINDOW AND SCROLL";

    impl Plugin for Scroller {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            let mut m = Manifest::new("scroller-test", Mount::BarRight);
            m.subscribes = vec![StateKey::Clock];
            m
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self {
                marquee: Marquee::new(StyleName::Vfd).window_px(64),
                text: SCROLL_TEXT.to_owned(),
            }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::Snapshot(_) = input {
                // One second of scroll at the configured speed: 20 dots in
                // raster mode, nothing at all once the host speaks preem.
                self.marquee.advance(1.0);
            }
            Vec::new()
        }

        fn view(&self) -> View {
            self.marquee.node("scroll", &self.text).into()
        }
    }

    /// Self-driven: folds messages from a finite `sources()` stream, so the
    /// session must merge app messages and survive the stream ending.
    struct Ticker {
        count: u32,
    }

    impl Plugin for Ticker {
        type Msg = u32;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("ticker-test", Mount::SidebarBottom)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { count: 0 }
        }

        fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<u32>> {
            Some(Box::pin(tokio_stream::iter([1_u32, 2, 3])))
        }

        fn update(&mut self, input: Input<u32>) -> Vec<Effect> {
            if let Input::App(n) = input {
                self.count += n;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: None,
                text: self.count.to_string(),
                classes: Vec::new(),
            }
            .into()
        }
    }

    /// Yields one message, ends — and **panics if polled again after its
    /// final `None`** (which the `Stream` contract permits). This is exactly
    /// the misbehavior the session's `src_done` guard exists to prevent;
    /// without the guard, the select loop would re-poll it and blow up.
    struct Fragile {
        yielded: bool,
        ended: bool,
    }

    impl tokio_stream::Stream for Fragile {
        type Item = u32;

        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<u32>> {
            assert!(
                !self.ended,
                "stream polled after completion — the src_done guard is broken"
            );
            if self.yielded {
                self.ended = true;
                Poll::Ready(None)
            } else {
                self.yielded = true;
                Poll::Ready(Some(5))
            }
        }
    }

    /// A `Ticker` variant whose source is the poll-after-end-intolerant
    /// [`Fragile`] stream.
    struct FragileTicker {
        count: u32,
    }

    impl Plugin for FragileTicker {
        type Msg = u32;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("fragile-test", Mount::SidebarBottom)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { count: 0 }
        }

        fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<u32>> {
            Some(Box::pin(Fragile {
                yielded: false,
                ended: false,
            }))
        }

        fn update(&mut self, input: Input<u32>) -> Vec<Effect> {
            if let Input::App(n) = input {
                self.count += n;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: None,
                text: self.count.to_string(),
                classes: Vec::new(),
            }
            .into()
        }
    }

    /// Constant chip tree, but the `View`'s panel flips on a slot-visibility
    /// toggle — so a panel change with an *unchanged* chip tree still forces a
    /// render frame (#349: the whole `View` is what dedup compares).
    struct Paneled {
        open: bool,
    }

    impl Plugin for Paneled {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("paneled-test", Mount::BarCenter)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { open: false }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::SlotVisible(v) = input {
                self.open = v;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            // Constant chip tree — never changes across the panel flip.
            View::new(Node::Label {
                id: Some("paneled-chip".to_owned()),
                text: "chip".to_owned(),
                classes: Vec::new(),
            })
            .panel(Node::Label {
                id: Some("paneled-panel".to_owned()),
                text: if self.open { "open" } else { "closed" }.to_owned(),
                classes: Vec::new(),
            })
        }
    }

    /// A minimal I/O "task" for [`Commander`]: it *is* the sources stream —
    /// each command drained from the [`CmdReceiver`] is turned into an app
    /// message. Stands in for a real plugin's socket/HTTP task, which likewise
    /// consumes commands and re-emits results as [`Input::App`]s. Hand-rolled
    /// (over `poll_recv`) to keep the SDK's own tests off the `tokio-stream`
    /// wrapper features.
    struct CmdEcho {
        rx: CmdReceiver<u32>,
    }

    impl tokio_stream::Stream for CmdEcho {
        type Item = String;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<String>> {
            self.get_mut()
                .rx
                .poll_recv(cx)
                .map(|opt| opt.map(|n| format!("io:{n}")))
        }
    }

    /// Command-driven: a click dispatches a [`Cmd`](Plugin::Cmd) down the
    /// per-session lane; its own I/O side ([`CmdEcho`]) echoes it back as an
    /// app message that folds into the view. Exercises the whole outbound
    /// path — `update` → `Cmd` → I/O task → `Msg` → `update` → render (#280).
    struct Commander {
        cmd_tx: CmdSender<u32>,
        last: String,
    }

    impl Plugin for Commander {
        type Msg = String;
        type Cmd = u32;

        fn manifest() -> Manifest {
            Manifest::new("commander-test", Mount::SidebarTop)
        }

        fn init(cmds: CmdSender<Self::Cmd>) -> Self {
            Self {
                cmd_tx: cmds,
                last: "seed".to_owned(),
            }
        }

        fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
            Some(Box::pin(CmdEcho { rx: cmds }))
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            match input {
                Input::Event { node, kind } => {
                    if node == "cmd-btn" && matches!(kind, EventKind::Click) {
                        // Fire-and-forget onto the plugin's own I/O side; the
                        // click alone changes neither the view nor the effects.
                        let _ = self.cmd_tx.send(42);
                    }
                }
                Input::App(done) => self.last = done,
                Input::Snapshot(_)
                | Input::EffectResult { .. }
                | Input::SlotVisible(_)
                | Input::AudioSpectrum(_)
                | Input::ConsentDecision { .. }
                | Input::CalendarUpcoming(_)
                | Input::SessionLocked(_)
                | Input::NowPlaying(_)
                | Input::DatasourceQuery { .. }
                | Input::DatasourceResult { .. } => {}
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: Some("cmd-lbl".to_owned()),
                text: self.last.clone(),
                classes: Vec::new(),
            }
            .into()
        }
    }

    /// Reflects the latest audio-spectrum peak in its view, so a
    /// [`HostMsg::AudioSpectrum`] arriving as [`Input::AudioSpectrum`] is
    /// observable as a re-render — the audio-reactive push in miniature (#405).
    struct Meter {
        peak: f32,
    }

    impl Plugin for Meter {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            let mut m = Manifest::new("meter-test", Mount::SidebarTop);
            m.subscribes = vec![StateKey::AudioSpectrum];
            m
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { peak: 0.0 }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::AudioSpectrum(spectrum) = input {
                self.peak = spectrum.peak;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: None,
                text: format!("{:.2}", self.peak),
                classes: Vec::new(),
            }
            .into()
        }
    }

    // ── Host-side helpers ────────────────────────────────────────────────────

    fn snapshot(iso: &str) -> HostMsg {
        HostMsg::StateSnapshot {
            snapshot: StateSnapshot {
                clock: Some(ClockState {
                    iso: iso.to_owned(),
                    unix: 0,
                }),
            },
        }
    }

    async fn next_plugin_frame<R: AsyncRead + Unpin>(rd: &mut R) -> PluginMsg {
        read_frame(rd).await.expect("a plugin frame")
    }

    async fn send<W: AsyncWrite + Unpin>(wr: &mut W, msg: &HostMsg) {
        write_frame(wr, msg).await.expect("host frame written");
    }

    /// Consume the fixed handshake (`Register` → `Log` → seed `Render`),
    /// asserting its shape, and return the seed tree.
    async fn eat_handshake<R: AsyncRead + Unpin>(rd: &mut R, id: &str) -> Node {
        let PluginMsg::Register { manifest } = next_plugin_frame(rd).await else {
            panic!("first frame must be Register");
        };
        assert_eq!(manifest.id, id);
        manifest.check_proto().expect("proto version matches");
        let PluginMsg::Log { .. } = next_plugin_frame(rd).await else {
            panic!("second frame must be the greeting Log");
        };
        let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(rd).await else {
            panic!("third frame must be the seed Render");
        };
        assert!(effects.is_empty(), "seed render carries no effects");
        tree
    }

    // ── Session tests (hermetic: an in-memory duplex "socket") ──────────────

    #[tokio::test]
    async fn handshake_is_register_log_seed_render_and_eof_errors() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, hwr) = tokio::io::split(host_end);

        let host = async move {
            let seed = eat_handshake(&mut hrd, "echo-test").await;
            assert_eq!(
                seed,
                Node::Label {
                    id: Some("echo-lbl".to_owned()),
                    text: "seed".to_owned(),
                    classes: Vec::new(),
                }
            );
            // Host goes away without Shutdown: both halves dropped → EOF.
            drop(hwr);
            drop(hrd);
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_err(), "EOF must surface as a session error");
    }

    #[tokio::test]
    async fn snapshot_rerenders_and_identical_snapshot_is_deduped() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            // The first change after the seed renders immediately (the view-rate
            // cap's leading edge, #560 — the seed doesn't count against it).
            send(&mut hwr, &snapshot("10:00")).await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a changed snapshot must re-render");
            };
            assert!(effects.is_empty());
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "10:00"),
                "render reflects the snapshot"
            );

            // Same snapshot again → same tree → no Render frame. The Ping is
            // the sync barrier: the very next frame must be its Pong.
            send(&mut hwr, &snapshot("10:00")).await;
            send(&mut hwr, &HostMsg::Ping { seq: 7 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 7 }
                ),
                "identical tree must be deduped (Pong, not Render, follows)"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
    }

    /// #560: the view-rate cap coalesces renders. A view change that lands within
    /// `VIEW_MIN_INTERVAL` of the last send is **deferred**, not sent inline —
    /// so a `Ping` arriving right behind it is answered *first* (liveness stays
    /// prompt), and the deferred render then flushes as a trailing frame at the
    /// cap boundary. `start_paused` drives the interval deterministically. This
    /// is the (deliberately) reordered successor to the old "queued snapshot
    /// renders strictly before the Pong" assertion, which the cap retires.
    #[tokio::test(start_paused = true)]
    async fn view_cap_defers_a_within_window_change_and_flushes_the_trailing_frame() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            // Leading edge: the first change after the seed renders at once.
            send(&mut hwr, &snapshot("10:00")).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Render { tree: Node::Label { ref text, .. }, .. } if text == "10:00"
                ),
                "the leading-edge change renders immediately",
            );

            // Now, within the cap interval (paused clock hasn't advanced), a
            // changed snapshot AND a Ping, back to back. The render is deferred,
            // so the Pong comes out first…
            send(&mut hwr, &snapshot("11:00")).await;
            send(&mut hwr, &HostMsg::Ping { seq: 8 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 8 }
                ),
                "a within-window render is deferred, so the following Ping answers first",
            );
            // …and the deferred render flushes as the trailing frame once the
            // interval elapses (auto-advanced under paused time).
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Render { tree: Node::Label { ref text, .. }, .. } if text == "11:00"
                ),
                "the deferred change flushes at the cap boundary (no dropped trailing frame)",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
    }

    #[tokio::test]
    async fn panel_change_alone_forces_a_render() {
        // #349 PR2: dedup now covers the panel independently of the chip tree.
        // `Paneled`'s `view` is constant but its `panel` flips on a visibility
        // toggle, so a panel change with an unchanged chip tree must still emit
        // a `Render` — and an identical (tree, panel) pair must still be deduped.
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            // Seed handshake: the seed Render carries the initial (closed) panel.
            let PluginMsg::Register { .. } = next_plugin_frame(&mut hrd).await else {
                panic!("first frame must be Register");
            };
            let PluginMsg::Log { .. } = next_plugin_frame(&mut hrd).await else {
                panic!("second frame must be the greeting Log");
            };
            let PluginMsg::Render {
                tree,
                panel,
                effects,
            } = next_plugin_frame(&mut hrd).await
            else {
                panic!("third frame must be the seed Render");
            };
            assert!(effects.is_empty(), "seed render carries no effects");
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "chip"),
                "seed chip tree",
            );
            assert!(
                matches!(panel, Some(Node::Label { ref text, .. }) if text == "closed"),
                "seed render carries the initial panel",
            );

            // Flip the panel while the chip tree stays constant → a frame is
            // still forced, and its `panel` reflects the change though `tree`
            // did not.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: true }).await;
            let PluginMsg::Render {
                tree,
                panel,
                effects,
            } = next_plugin_frame(&mut hrd).await
            else {
                panic!("a panel change alone must re-render");
            };
            assert!(effects.is_empty());
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "chip"),
                "the chip tree is unchanged across the panel flip",
            );
            assert!(
                matches!(panel, Some(Node::Label { ref text, .. }) if text == "open"),
                "the render reflects the new panel",
            );

            // The same visibility again → identical (tree, panel) → deduped. The
            // Ping is the sync barrier: the next frame must be its Pong.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: true }).await;
            send(&mut hwr, &HostMsg::Ping { seq: 5 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 5 }
                ),
                "identical (tree, panel) must be deduped (Pong, not Render, follows)",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Paneled, _, _>(prd, pwr), host);
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
    }

    #[tokio::test]
    async fn effects_force_a_send_even_with_unchanged_tree() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let seed = eat_handshake(&mut hrd, "echo-test").await;

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "echo-btn".to_owned(),
                    kind: EventKind::Click,
                },
            )
            .await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a click with effects must produce a Render frame");
            };
            assert_eq!(effects, vec![Effect::OpenPage(Page::PowerMenu)]);
            assert_eq!(tree, seed, "the tree itself is unchanged by the click");

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn event_on_unknown_node_produces_no_frame() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "not-ours".to_owned(),
                    kind: EventKind::Click,
                },
            )
            .await;
            send(&mut hwr, &HostMsg::Ping { seq: 1 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 1 }
                ),
                "no effects + unchanged tree must send nothing"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn sources_feed_app_inputs_and_a_finished_source_keeps_the_session_alive() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "ticker-test").await;

            // The three `iter([1,2,3])` source messages are all immediately
            // ready, so `update()` folds them in one frozen instant: 1, 1+2,
            // 1+2+3. Every one reaches `update` (the count is exact) — but the
            // view-rate cap (#560) coalesces the renders: the first fold "1"
            // renders on the leading edge, then "3" and "6" land within the
            // interval and coalesce, so the trailing frame carries the final
            // "6". Under paused time this is the deterministic ["1", "6"].
            for expected in ["1", "6"] {
                let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                    panic!("the folded source messages must re-render");
                };
                assert!(
                    matches!(tree, Node::Label { ref text, .. } if text == expected),
                    "source folds coalesce to the leading + trailing frame (expected {expected})"
                );
            }

            // The stream is exhausted now; the session must still serve the
            // host side (a terminated source must not wedge or kill the loop).
            send(&mut hwr, &HostMsg::Ping { seq: 9 }).await;
            assert!(matches!(
                next_plugin_frame(&mut hrd).await,
                PluginMsg::Pong { seq: 9 }
            ));

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Ticker, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn finished_source_is_never_polled_again() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "fragile-test").await;

            // The one source message renders; the stream then ends. Every
            // further loop iteration must NOT poll it again ([`Fragile`]
            // panics if it is) — the Ping/Pong exchanges drive extra
            // iterations to prove it.
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("the source message must re-render");
            };
            assert!(matches!(tree, Node::Label { ref text, .. } if text == "5"));

            for seq in [1_u64, 2] {
                send(&mut hwr, &HostMsg::Ping { seq }).await;
                assert!(matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: got } if got == seq
                ));
            }

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<FragileTicker, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn effect_result_surfaces_as_input() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            send(
                &mut hwr,
                &HostMsg::EffectResult {
                    id: 3,
                    outcome: EffectOutcome {
                        ok: true,
                        output: None,
                    },
                },
            )
            .await;
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("an EffectResult must reach update() and re-render");
            };
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "cmd3 ok=true"),
                "the outcome was folded into the model"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::SlotVisibility`] push reaches `update` as
    /// [`Input::SlotVisible`] and re-renders — the mechanism a migrated poller
    /// gates on to park itself while hidden (#288). Latest-wins is exercised
    /// implicitly: each push is folded independently and the tree tracks it.
    #[tokio::test]
    async fn slot_visibility_push_reaches_update_as_input() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let seed = eat_handshake(&mut hrd, "watcher-test").await;
            assert!(
                matches!(seed, Node::Label { ref text, .. } if text == "hidden"),
                "the fresh model starts hidden",
            );

            // "sidebar opened" → the plugin folds SlotVisible(true) and re-renders.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: true }).await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a visibility push must reach update() and re-render");
            };
            assert!(effects.is_empty());
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "visible"),
                "SlotVisibility(true) reached update as Input::SlotVisible(true)",
            );

            // "sidebar closed" flips it back — the park-your-pollers edge.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: false }).await;
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("the close push must re-render");
            };
            assert!(matches!(tree, Node::Label { ref text, .. } if text == "hidden"));

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Watcher, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::AudioSpectrum`] push reaches `update` as
    /// [`Input::AudioSpectrum`] and re-renders — the mechanism a scope/VU tile
    /// consumes to animate to the music (#405).
    #[tokio::test]
    async fn audio_spectrum_push_reaches_update_as_input() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let seed = eat_handshake(&mut hrd, "meter-test").await;
            assert!(
                matches!(seed, Node::Label { ref text, .. } if text == "0.00"),
                "the fresh meter starts at zero",
            );

            send(
                &mut hwr,
                &HostMsg::AudioSpectrum {
                    spectrum: AudioSpectrum {
                        peak: 0.80,
                        bins: [0.5_f32; SPECTRUM_BINS],
                    },
                },
            )
            .await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a spectrum push must reach update() and re-render");
            };
            assert!(effects.is_empty());
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "0.80"),
                "AudioSpectrum reached update as Input::AudioSpectrum",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Meter, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::ConsentDecision`] push reaches `update` as
    /// [`Input::ConsentDecision`] and re-renders — the mechanism a plugin
    /// (infobroker) consumes to complete a parked consent knock (#487 phase 1b).
    #[tokio::test]
    async fn consent_decision_push_reaches_update_as_input() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            send(
                &mut hwr,
                &HostMsg::ConsentDecision {
                    request_id: 3,
                    decision: ConsentDecision::AllowSession,
                },
            )
            .await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a consent decision must reach update() and re-render");
            };
            assert!(effects.is_empty());
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "consent3=AllowSession"),
                "ConsentDecision reached update as Input::ConsentDecision",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::CalendarUpcoming`] push reaches `update` as
    /// [`Input::CalendarUpcoming`] and re-renders — the digest caw's briefing and
    /// the infobroker consume (#484).
    #[tokio::test]
    async fn calendar_upcoming_push_reaches_update_as_input() {
        use hytte_plugin_proto::UpcomingEvent;
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;
            send(
                &mut hwr,
                &HostMsg::CalendarUpcoming {
                    events: vec![
                        UpcomingEvent {
                            start_unix: 1,
                            end_unix: 2,
                            title: "standup".to_owned(),
                            calendar: "Work".to_owned(),
                        },
                        UpcomingEvent {
                            start_unix: 3,
                            end_unix: 4,
                            title: "lunch".to_owned(),
                            calendar: "Personal".to_owned(),
                        },
                    ],
                },
            )
            .await;
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a calendar push must reach update() and re-render");
            };
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "cal:standup,lunch"),
                "CalendarUpcoming reached update as Input::CalendarUpcoming",
            );
            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::SessionLocked`] push reaches `update` as
    /// [`Input::SessionLocked`] and re-renders — the lock/unlock edge caw and the
    /// infobroker key off (#484).
    #[tokio::test]
    async fn session_locked_push_reaches_update_as_input() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;
            send(&mut hwr, &HostMsg::SessionLocked { locked: true }).await;
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a session-locked push must reach update() and re-render");
            };
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "locked=true"),
                "SessionLocked reached update as Input::SessionLocked",
            );
            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// A host [`HostMsg::NowPlaying`] push reaches `update` as
    /// [`Input::NowPlaying`] and re-renders — the track digest the audio widget's
    /// marquee consumes (#528).
    #[tokio::test]
    async fn now_playing_push_reaches_update_as_input() {
        use hytte_plugin_proto::NowPlaying;
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;
            send(
                &mut hwr,
                &HostMsg::NowPlaying {
                    now_playing: NowPlaying {
                        title: "Chrome Rain".to_owned(),
                        artist: "Choom".to_owned(),
                        playing: true,
                        position_us: 83_000_000,
                        length_us: 296_000_000,
                    },
                },
            )
            .await;
            let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a now-playing push must reach update() and re-render");
            };
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "np:Chrome Rain|Choom|true"),
                "NowPlaying reached update as Input::NowPlaying",
            );
            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn a_command_from_update_reaches_the_sources_io_side() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let seed = eat_handshake(&mut hrd, "commander-test").await;
            assert!(matches!(seed, Node::Label { ref text, .. } if text == "seed"));

            // A click emits neither a render nor an effect — it only dispatches
            // command 42 down the lane. The sources I/O side echoes it as the
            // app message "io:42", which folds in and re-renders. So the very
            // next plugin frame is that echo's Render: the round-trip landed.
            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "cmd-btn".to_owned(),
                    kind: EventKind::Click,
                },
            )
            .await;
            let PluginMsg::Render { tree, effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("the echoed command must re-render");
            };
            assert!(
                effects.is_empty(),
                "the command is plugin I/O, not an effect"
            );
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "io:42"),
                "the command round-tripped through the plugin's own I/O side"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(session::<Commander, _, _>(prd, pwr), host);
        assert!(result.is_ok());
    }

    /// The command lane is per-session: [`session`] builds a fresh channel on
    /// every connect and hands its ends to `init`/`sources`. Two back-to-back
    /// sessions must each round-trip — a single global channel would be closed
    /// once the first session's model (holding the sender) dropped, wedging the
    /// second. Also pins that a command never leaks across a reconnect.
    #[tokio::test]
    async fn the_command_lane_is_recreated_each_session() {
        for _ in 0..2 {
            let (plugin_end, host_end) = duplex(64 * 1024);
            let (prd, pwr) = tokio::io::split(plugin_end);
            let (mut hrd, mut hwr) = tokio::io::split(host_end);

            let host = async move {
                eat_handshake(&mut hrd, "commander-test").await;
                send(
                    &mut hwr,
                    &HostMsg::Event {
                        node: "cmd-btn".to_owned(),
                        kind: EventKind::Click,
                    },
                )
                .await;
                let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                    panic!("each session's fresh lane must round-trip");
                };
                assert!(matches!(tree, Node::Label { ref text, .. } if text == "io:42"));
                send(&mut hwr, &HostMsg::Shutdown).await;
            };

            let (result, ()) = tokio::join!(session::<Commander, _, _>(prd, pwr), host);
            assert!(
                result.is_ok(),
                "a fresh per-session command lane round-trips"
            );
        }
    }

    #[tokio::test]
    async fn write_failure_mid_session_ends_the_session_with_an_error() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;

            // Queue a click (forces a Render write: effects on an unchanged
            // tree), then drop the whole host end. The buffered Event is
            // still readable, but the plugin's answering write must fail —
            // and end the session as an error, not a hang or a panic.
            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "echo-btn".to_owned(),
                    kind: EventKind::Click,
                },
            )
            .await;
            drop(hwr);
            drop(hrd);
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr), host);
        assert!(
            result.is_err(),
            "a failed mid-session write surfaces as a session error"
        );
    }

    // ── The reconnect loop ───────────────────────────────────────────────────

    /// Pins the runtime's headline decision (#275): a session that ends `Ok`
    /// (host `Shutdown`) leads to a **redial**, not loop termination — the
    /// second completed handshake is the proof. `start_paused` auto-advances
    /// the backoff sleeps.
    #[tokio::test(start_paused = true)]
    async fn shutdown_leads_to_redial_not_termination() {
        let (p1, h1) = duplex(64 * 1024);
        let (p2, h2) = duplex(64 * 1024);
        // Popped back-to-front: first connect gets p1, the redial gets p2,
        // any further attempt parks forever.
        let mut pending = vec![p2, p1];

        let dial_loop = reconnect_loop::<Echo, _, _, _, _>("echo-test", move || {
            let next = pending.pop();
            async move {
                match next {
                    Some(end) => Ok(tokio::io::split(end)),
                    None => std::future::pending().await,
                }
            }
        });

        let host = async move {
            let (mut hrd1, mut hwr1) = tokio::io::split(h1);
            eat_handshake(&mut hrd1, "echo-test").await;
            send(&mut hwr1, &HostMsg::Shutdown).await;

            // The loop must treat that Ok(()) as "reconnect": the second
            // prepared connection completes a fresh handshake.
            let (mut hrd2, _hwr2) = tokio::io::split(h2);
            eat_handshake(&mut hrd2, "echo-test").await;
        };

        tokio::select! {
            () = host => {}
            () = dial_loop => unreachable!("reconnect_loop never returns"),
        }
    }

    // ── Backoff (pure) ───────────────────────────────────────────────────────

    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = Backoff::new();
        let mut delays = Vec::new();
        for _ in 0..8 {
            delays.push(b.delay());
        }
        assert_eq!(delays[0], BACKOFF_BASE);
        assert_eq!(delays[1], BACKOFF_BASE * 2);
        assert_eq!(delays[6], BACKOFF_CAP);
        assert_eq!(delays[7], BACKOFF_CAP, "stays at the cap");
    }

    #[test]
    fn backoff_resets_only_after_a_stable_session() {
        let mut b = Backoff::new();
        for _ in 0..6 {
            let _ = b.delay();
        }
        // A short-lived (flapping) session must NOT reset the backoff…
        b.note_session(BACKOFF_CAP / 2);
        assert_eq!(b.delay(), BACKOFF_CAP);
        // …while one that lived past the cap does.
        b.note_session(BACKOFF_CAP);
        assert_eq!(b.delay(), BACKOFF_BASE);
    }

    /// #437: a streak of *immediate* session failures (the wire-vocab-skew
    /// crash-loop) escalates to a skew warning at [`SKEW_WARN_AFTER`]; a session
    /// that lives long enough, or a clean host `Shutdown`, resets the streak so a
    /// later transient blip doesn't inherit a stale count.
    #[test]
    fn immediate_failures_escalate_to_a_skew_warning_then_reset() {
        let quick = Duration::from_millis(50);
        let mut r = Redial::new();
        // The first SKEW_WARN_AFTER-1 immediate failures stay quiet…
        for _ in 0..SKEW_WARN_AFTER - 1 {
            assert!(!r.note(quick, false), "below the threshold stays quiet");
        }
        // …the next crosses the threshold, and every further one keeps warning.
        assert!(
            r.note(quick, false),
            "the streak reaches the warn threshold"
        );
        assert!(r.note(quick, false), "a sustained loop keeps warning");

        // A session that outlived IMMEDIATE_FAILURE resets the streak.
        assert!(!r.note(IMMEDIATE_FAILURE, false), "a stable session resets");
        assert!(!r.note(quick, false), "the streak restarts from zero");

        // A clean shutdown (ended_ok) also resets, regardless of how brief.
        let mut r2 = Redial::new();
        for _ in 0..SKEW_WARN_AFTER {
            let _ = r2.note(quick, false);
        }
        assert!(!r2.note(quick, true), "a clean shutdown resets the streak");
        assert!(!r2.note(quick, false), "…so the next failure starts over");
    }

    // ── Vocabulary negotiation, end to end (#884) ───────────────────────────

    /// A node's kind, for assertion messages — `Node::Pixels`'s own `Debug`
    /// would dump the whole RGBA buffer into the failure output.
    fn kind_of(node: &Node) -> &'static str {
        match node {
            Node::Pixels { .. } => "Pixels",
            Node::Preem { .. } => "Preem",
            _ => "some other node kind",
        }
    }

    /// Read the next frame, requiring it to be a `Render`, and return its tree.
    ///
    /// **Bounded**, unlike the plain [`next_plugin_frame`] the older session
    /// tests use, and the falsification round is why: breaking the
    /// emit-vs-rasterise decision so the SDK always emits state makes the frame
    /// `a_hello_below_the_preem_generation_still_rasterises` is waiting for
    /// dedup away entirely, and an unbounded read then **hangs** — a regression
    /// that stalls CI on a timeout instead of naming itself in a failure line.
    /// Five seconds is ~1000× what a duplex round trip takes.
    async fn next_render<R: AsyncRead + Unpin>(rd: &mut R) -> Node {
        let frame = tokio::time::timeout(Duration::from_secs(5), next_plugin_frame(rd))
            .await
            .expect("a Render frame within 5 s — an unsent frame is a bug, not a slow test");
        match frame {
            PluginMsg::Render { tree, .. } => tree,
            PluginMsg::Pong { seq } => panic!("expected a Render frame, got Pong {seq}"),
            PluginMsg::Log { msg, .. } => panic!("expected a Render frame, got Log {msg:?}"),
            PluginMsg::Register { .. } => panic!("expected a Render frame, got a second Register"),
        }
    }

    /// Run one whole `Scroller` session against a host closure.
    async fn scroller_session<F, Fut>(host: F)
    where
        F: FnOnce(
            tokio::io::ReadHalf<tokio::io::DuplexStream>,
            tokio::io::WriteHalf<tokio::io::DuplexStream>,
        ) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (hrd, hwr) = tokio::io::split(host_end);
        let (result, ()) = tokio::join!(session::<Scroller, _, _>(prd, pwr), host(hrd, hwr));
        result.expect("the host shut the session down cleanly");
    }

    /// The `Register` frame carries the #882 negotiation **pair**, and that
    /// pairing is what makes the whole thing reachable: `vocab` stays at the
    /// unconditional generation an old host still accepts, while `vocab_max`
    /// declares what this plugin could speak if asked — which is also the
    /// structural #305 opt-in for the host's `Hello`.
    #[test]
    fn register_declares_the_negotiation_pair() {
        let m = Scroller::manifest();
        assert_eq!(
            m.vocab, VOCAB_UNCONDITIONAL,
            "the unconditional ceiling must NOT be the census counter, or an \
             older host refuses a plugin the negotiation would have made safe",
        );
        assert_eq!(m.vocab_max, Some(VOCAB), "…and the negotiated one is VOCAB");
        assert!(m.negotiates_vocab(), "so the host knows to send Hello");
        m.check_vocab()
            .expect("an old host's handshake still accepts us");

        // Both ends compute the same number from the same two inputs.
        assert_eq!(m.negotiated_vocab(VOCAB_UNCONDITIONAL), VOCAB_UNCONDITIONAL);
        assert_eq!(m.negotiated_vocab(VOCAB), VOCAB);
        assert!(
            m.negotiated_vocab(VOCAB) >= PREEM_VOCAB,
            "a current host reaches the preem generation",
        );
        assert!(
            m.negotiated_vocab(VOCAB_UNCONDITIONAL) < PREEM_VOCAB,
            "an old one does not",
        );
    }

    /// The headline: a host that advertises the preem generation gets typed
    /// state nodes, and the wire then goes **silent** across two heartbeats
    /// that would each have produced a fresh buffer in raster mode.
    ///
    /// Named for what it can actually prove (#898 review N5). It looks like a
    /// guard on `advance` being a no-op, and it is not: defeat all six
    /// `if mode == Raster` guards and this test still passes, because
    /// `MarqueeState` carries only the text — a ticking plugin-side offset
    /// cannot change the emitted node. The guards are covered by
    /// `display::tests::{an_unchanged_marquee_is_quiet_…, settling_animations_are_quiet_…}`,
    /// which is where that mutation goes red.
    #[tokio::test]
    async fn an_advertising_host_gets_state_nodes_and_the_wire_stays_quiet() {
        scroller_session(|mut hrd, mut hwr| async move {
            let seed = eat_handshake(&mut hrd, "scroller-test").await;
            assert!(
                matches!(seed, Node::Pixels { .. }),
                "the seed render goes out before Hello can arrive, so it must \
                 rasterise — got {}",
                kind_of(&seed),
            );

            // The advertisement. The runtime re-renders on it, so the switch
            // lands inside the same session rather than waiting for an update.
            send(&mut hwr, &HostMsg::Hello { vocab: VOCAB }).await;
            let switched = next_render(&mut hrd).await;
            assert!(
                matches!(switched, Node::Preem { .. }),
                "Hello must switch the session to state nodes — got {}",
                kind_of(&switched),
            );

            // Two heartbeats. The view's text never changes and `advance` is a
            // no-op now, so the state node is identical and nothing is sent.
            // The Ping is the sync barrier: a Pong arriving before any Render
            // is the wire staying quiet.
            send(&mut hwr, &snapshot("10:00")).await;
            send(&mut hwr, &snapshot("10:01")).await;
            send(&mut hwr, &HostMsg::Ping { seq: 9 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 9 }
                ),
                "a scrolling marquee must send nothing while the shell animates it",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        })
        .await;
    }

    /// The other half of the compat matrix, and the guard that matters most: a
    /// host that never advertises must **never** see a `Node::Preem`. The same
    /// plugin code keeps rasterising and keeps ticking its own scroll, so every
    /// heartbeat is a fresh buffer — exactly today's behaviour.
    #[tokio::test]
    async fn a_host_that_never_says_hello_keeps_getting_pixels() {
        scroller_session(|mut hrd, mut hwr| async move {
            let seed = eat_handshake(&mut hrd, "scroller-test").await;
            assert!(
                matches!(seed, Node::Pixels { .. }),
                "no advertisement, no state nodes — got {}",
                kind_of(&seed),
            );

            send(&mut hwr, &snapshot("10:00")).await;
            let first = next_render(&mut hrd).await;
            assert!(
                matches!(first, Node::Pixels { .. }),
                "still no advertisement — got {}",
                kind_of(&first),
            );
            // Compared with `!=` rather than `assert_ne!` throughout: these are
            // `Node::Pixels`, whose `Debug` would dump the whole RGBA buffer
            // into a failure message.
            assert!(first != seed, "and the plugin's own tick moved the scroll");

            send(&mut hwr, &snapshot("10:01")).await;
            let second = next_render(&mut hrd).await;
            assert!(
                matches!(second, Node::Pixels { .. }),
                "…for every frame of the session — got {}",
                kind_of(&second),
            );
            assert!(second != first, "the plugin keeps owning the animation");

            send(&mut hwr, &HostMsg::Shutdown).await;
        })
        .await;
    }

    /// A host that negotiates but whose own vocabulary predates the preem
    /// widgets (`Hello { vocab: 1 }`) is still a raster host. The gate is the
    /// **negotiated generation**, not the mere presence of an advertisement.
    #[tokio::test]
    async fn a_hello_below_the_preem_generation_still_rasterises() {
        scroller_session(|mut hrd, mut hwr| async move {
            eat_handshake(&mut hrd, "scroller-test").await;

            // The Rerender this triggers changes nothing (still Pixels, same
            // offset), so it is deduped — a heartbeat forces the next frame.
            send(
                &mut hwr,
                &HostMsg::Hello {
                    vocab: VOCAB_UNCONDITIONAL,
                },
            )
            .await;
            send(&mut hwr, &snapshot("10:00")).await;
            let node = next_render(&mut hrd).await;
            assert!(
                matches!(node, Node::Pixels { .. }),
                "generation {VOCAB_UNCONDITIONAL} is below PREEM_VOCAB \
                 {PREEM_VOCAB} — got {}",
                kind_of(&node),
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        })
        .await;
    }

    /// The generation is re-seeded on every (re)connect, so a reconnect to a
    /// host that does *not* advertise degrades back to pixels instead of
    /// inheriting the previous session's advertisement. Both sessions run on
    /// this one thread, which is precisely the state that could leak.
    #[tokio::test]
    async fn a_reconnect_to_a_silent_host_degrades_back_to_pixels() {
        scroller_session(|mut hrd, mut hwr| async move {
            eat_handshake(&mut hrd, "scroller-test").await;
            send(&mut hwr, &HostMsg::Hello { vocab: VOCAB }).await;
            let switched = next_render(&mut hrd).await;
            assert!(matches!(switched, Node::Preem { .. }), "session 1 upgraded");
            send(&mut hwr, &HostMsg::Shutdown).await;
        })
        .await;

        scroller_session(|mut hrd, mut hwr| async move {
            let seed = eat_handshake(&mut hrd, "scroller-test").await;
            assert!(
                matches!(seed, Node::Pixels { .. }),
                "a new session must start from the unconditional floor — got {}",
                kind_of(&seed),
            );
            send(&mut hwr, &HostMsg::Shutdown).await;
        })
        .await;
    }
}
