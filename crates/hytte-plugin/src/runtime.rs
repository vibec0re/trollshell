//! The transport runtime extracted from the reference plugin (#275): dial +
//! bounded backoff, the `Register` handshake, and the read→update→render
//! session loop. A plugin author never touches this — [`run`] is the whole
//! surface.

use std::future::Future;
use std::time::{Duration, Instant};

use hytte_plugin_proto::{
    Capability, Effect, HostMsg, LogLevel, Mount, PluginMsg, ProtoError, StateKey,
    VOCAB_UNCONDITIONAL, read_frame, socket_path, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, watch};
use tokio_stream::StreamExt;

use crate::{Input, Plugin, View};

/// The grace [`Plugin::shutdown`] gets before the process exits regardless
/// (#1079) — via [`run_shutdown_hook`] wrapping the call in
/// [`tokio::time::timeout`]. Long enough for a well-behaved flush (one small
/// file write, per the SDK docs' worked example — the infobroker's grant
/// store); short enough that a plugin stuck in its own hook doesn't sit on
/// top of systemd's `TimeoutStopSec` — **only for a hook that actually
/// `.await`s** (#1092 review M4). A `timeout` can only reclaim control at an
/// `.await` point, so a hook that blocks the OS thread instead
/// (`std::fs::write`, `std::thread::sleep`, …) is not preemptable on `run`'s
/// current-thread runtime: measured, `std::thread::sleep(8s)` in a hook held
/// the real `SIGTERM` path for 8059 ms against this 2 s bound, while the same
/// 8 s as an `.await`ed sleep was cut at ~2067 ms as documented. Blocking
/// work belongs behind [`tokio::task::spawn_blocking`], `.await`ed — see the
/// crate docs' *Process shutdown* section and `hytte-plugin-infobroker`'s
/// `GrantStore::drain` for the shape. `TimeoutStopSec` is the *outer* bound
/// regardless (see `run`'s doc) — a unit still not gone by then is
/// `SIGKILL`ed, past anything this runtime controls.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// [`watch::Receiver::changed`], but a sender that is simply dropped without
/// ever sending (no signal ever fired, and none can now — `run`'s protocol
/// only ever sends `true`, at most once) resolves to `None` instead of
/// `Err`, rather than looking like a fired signal. A `select!` branch built
/// on this can therefore never mistake "the listener task is gone" for "it
/// asked us to shut down" — the bug a first cut of this had, where every
/// test's `never_shuts_down()` receiver's sender is dropped immediately and
/// every `session`/`reconnect_loop` call read that as an instant shutdown.
/// Given the protocol, an `Ok` here already means the flag is `true`; there
/// is nothing left to check.
async fn shutdown_fired(shutdown: &mut watch::Receiver<bool>) -> Option<()> {
    shutdown.changed().await.ok()
}

/// The launch-time mount override (#1159, epic #1158): a wire
/// [`Mount`](hytte_plugin_proto::Mount) name that replaces whatever the plugin's
/// own [`manifest`](Plugin::manifest) asked for.
///
/// An **environment variable** rather than an argv flag, settled on #866: two
/// bundled plugins parse their own argv with `clap` for a CLI hat
/// (`hytte-infobroker`, `hytte-plugin-niri-layouts`, #1116) and would reject an
/// SDK-owned `--mount`, while an env var rides the launcher's existing
/// `env` → `--setenv=K=V` path (`trollshell/src/plugin_launcher.rs`) with no
/// launcher change at all — which is exactly what "nix renders the placement onto
/// the launch" already means for every other knob.
///
/// Read once, in [`run`], before the first dial; applied to the manifest inside
/// every [`session`] so a reconnect carries it too. The plugin's own
/// `manifest()` is never called differently and never sees the override.
const MOUNT_ENV: &str = "HYTTE_PLUGIN_MOUNT";

/// A [`MOUNT_ENV`] value that is not a wire mount name.
///
/// A refusal, never a fallback: silently keeping the manifest's mount would put
/// the card on the *other* sidebar from the one the deployment asked for, with
/// nothing on screen to say so — the single worst outcome available, since the
/// plugin looks healthy. So this is fatal at startup, and its message names every
/// spelling that would have worked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountOverrideError {
    /// The value exactly as the environment carried it (untrimmed, and
    /// lossy-converted if it was not UTF-8), so the message shows the typo rather
    /// than a cleaned-up version of it.
    value: String,
}

impl std::fmt::Display for MountOverrideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The spellings come from `Mount::ALL`, not a list written out here: a
        // tenth mount must not be able to exist without this message naming it.
        let names = Mount::ALL.map(Mount::wire_name).join(", ");
        write!(
            f,
            "{MOUNT_ENV}={:?} is not a mount name; expected one of: {names}",
            self.value,
        )
    }
}

impl std::error::Error for MountOverrideError {}

/// Parse one raw [`MOUNT_ENV`] value. The whole decision, with no environment in
/// it, so the nine accepted spellings and the refusal are testable without
/// mutating process state (`unsafe` is forbidden workspace-wide, so
/// `std::env::set_var` is not available to a test anyway).
///
/// `None` in → `Ok(None)`: the variable is unset and the manifest wins.
/// Surrounding whitespace is trimmed before the lookup — a trimmed name still
/// resolves to exactly one mount, so this cannot misplace anything — but the
/// match is otherwise **exact**, case included, because the value has to be a
/// name the wire itself can carry (`Mount::from_wire_name`). An empty or
/// whitespace-only value is therefore a refusal, not "unset": the launcher can
/// render `--setenv=HYTTE_PLUGIN_MOUNT=` from an empty Nix string, and treating
/// that as "no override" would hide a misconfiguration.
fn mount_override(raw: Option<&str>) -> Result<Option<Mount>, MountOverrideError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    Mount::from_wire_name(raw.trim())
        .map(Some)
        .ok_or_else(|| MountOverrideError {
            value: raw.to_owned(),
        })
}

/// [`mount_override`] against the real environment — the only place this SDK
/// reads one. A non-UTF-8 value is refused like any other unparseable one rather
/// than ignored.
fn mount_override_from_env() -> Result<Option<Mount>, MountOverrideError> {
    match std::env::var(MOUNT_ENV) {
        Ok(raw) => mount_override(Some(&raw)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(raw)) => Err(MountOverrideError {
            value: raw.to_string_lossy().into_owned(),
        }),
    }
}

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

/// Sanitise every float a view carries, before it is compared **or** encoded
/// (#904).
///
/// The SDK half of the wire's float seam. [`Node`](hytte_plugin_proto::Node)
/// derives `PartialEq` and [`session`]'s dedup is `view != last_view`, so a
/// single `NaN` in a `Progress`/`Slider` float makes a view unequal to an
/// identical copy of itself: the compare stays true forever and the plugin
/// emits one `Render` per inbound event for a picture that never changes,
/// bounded above only by [`VIEW_MIN_INTERVAL`]'s ~30 fps cap. `NaN != NaN` is
/// the whole mechanism.
///
/// Running it here — once per view, **before** the compare — closes both ends
/// at once: the compare becomes reflexive, *and* every frame that reaches the
/// wire carries finite floats, so a host in any language is handed values its
/// drawing code can actually use. The host still re-sanitises what it receives
/// (`trollshell`'s `wire_map`), because an SDK-built plugin is not the only
/// thing that can dial the socket; applying it twice is harmless precisely
/// because [`Node::clamp_in_place`](hytte_plugin_proto::Node::clamp_in_place)
/// is a **fixpoint** — the second pass moves nothing, so the host's own gates
/// cannot fire a frame late.
///
/// The per-field mapping — what each `NaN` / `±inf` / out-of-range value
/// becomes, and the GTK line every row was derived from — is contract, and
/// lives on `Node::clamp_in_place`.
fn sanitise_view(view: &mut View) {
    view.tree.clamp_in_place();
    if let Some(panel) = view.panel.as_mut() {
        panel.clamp_in_place();
    }
}

/// Drop any effect whose required [`Capability`] this manifest didn't declare,
/// before it ever reaches the wire (#1058). The host already enforces this at
/// `session::enforce_capabilities` — this is the SDK-side mirror, using the
/// same [`Effect::required_capability`] mapping so the two cannot drift — and
/// it closes a gap that used to cost more than a dropped effect: on a **current**
/// host an undeclared emit is silently dropped anyway, but on a host older than
/// the capability's own generation (`Shader` #893, `OpenUri` #1045, …)
/// `Register` decodes fine and the first render frame carrying the effect does
/// not, which is the #437 crash-loop. Catching it here means the mistake is
/// named on every host, not just an old one.
///
/// Not a `debug_assert!`: that would kill a release plugin outright over a
/// typo in a manifest. A warn-and-drop matches the host's own posture — the
/// plugin's own log is the signal, and a stray click silently does nothing
/// rather than the process dying.
///
/// Returns the survivors plus one diagnostic message per **newly** ungranted
/// effect kind this session (review HIGH-1, #1058 fix round): this fn is pure
/// and does no I/O, so it cannot itself put the message anywhere an author
/// would see it. The caller is responsible for surfacing every returned
/// message — both a [`PluginMsg::Log`] frame to the host (which routes it
/// through its own `tracing::warn!`, reaching every host including one older
/// than the dropped effect's own capability generation) and an `eprintln!`
/// to the plugin's own stderr, matching every other author-facing diagnostic
/// this runtime emits (`run`'s connect/redial/session-end lines). A bare
/// `tracing::warn!` here would reach nobody: no plugin binary installs a
/// `tracing` subscriber, and dropping the effect **before** the wire means
/// the host-side diagnostics that used to fire for this exact mistake
/// (`session::enforce_capabilities`'s warn, its audit record, the violation
/// counter the control-center Plugins tab shows) go silent too — the
/// `PluginMsg::Log` frame is what keeps at least one of those alive.
///
/// `warned` tracks which effect *kinds* (by [`std::mem::Discriminant`], not
/// value — two `OpenUri` effects with different URIs are the same kind) have
/// already produced a message this session, so a plugin looping on a bad
/// click is named once, not once per frame.
fn drop_ungranted_effects(
    effects: Vec<Effect>,
    granted: &[Capability],
    warned: &mut std::collections::HashSet<std::mem::Discriminant<Effect>>,
) -> (Vec<Effect>, Vec<String>) {
    let mut messages = Vec::new();
    let kept = effects
        .into_iter()
        .filter(|effect| {
            let Some(cap) = effect.required_capability() else {
                return true;
            };
            if granted.contains(&cap) {
                return true;
            }
            if warned.insert(std::mem::discriminant(effect)) {
                messages.push(format!(
                    "effect {effect:?} requires capability {cap:?}, which this manifest didn't declare; dropped"
                ));
            }
            false
        })
        .collect();
    (kept, messages)
}

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
/// read→update→render loop. `Ok(())` means the host sent `Shutdown` **or**
/// `shutdown` fired (#1079) — either way the caller checks `*shutdown.borrow()`
/// to tell them apart, since only the second means "exit, don't redial". Any
/// transport failure (EOF = the host went away) surfaces as `Err`, same
/// caveat. See the crate docs on why a host `Shutdown` alone does not exit.
///
/// `shutdown` is [`run`]'s process-wide notice (a real `SIGTERM`/`SIGINT` in
/// production; a test fires it directly — see "the fake-host socketpair
/// tests" below). Whenever this function returns with the flag set, it has
/// already run [`Plugin::shutdown`] under [`SHUTDOWN_GRACE`] — regardless of
/// which branch ended the loop, so a host `Shutdown` frame racing the signal
/// on the same poll can never skip the hook (see `run_shutdown_hook`'s call
/// site below).
///
/// Generic over the I/O halves (not `UnixStream`) so the whole loop is
/// hermetically testable over `tokio::io::duplex`.
///
/// `mount_override` is the launch-time placement [`run`] resolved out of
/// [`MOUNT_ENV`] (#1159), applied to the manifest here rather than in `run` so a
/// **reconnect** carries it too — the manifest is rebuilt from
/// [`P::manifest`](Plugin::manifest) on every session. It arrives as a value
/// rather than being re-read from the environment for two reasons: the refusal
/// has to be fatal at *startup*, before the first dial, where whoever started the
/// unit is watching; and a test can then drive all nine placements in one process
/// without touching process state (`unsafe` is forbidden workspace-wide, so
/// `std::env::set_var` is not available to one anyway).
// One cohesive session lifecycle (handshake → seed render → the select loop over
// every host frame → dedup); the length is the host-frame vocabulary, not
// branching complexity — splitting it would scatter the loop for no gain.
#[allow(clippy::too_many_lines)]
async fn session<P, R, W>(
    rd: R,
    mut wr: W,
    mut shutdown: watch::Receiver<bool>,
    mount_override: Option<Mount>,
) -> Result<(), ProtoError>
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
    // The launch-time placement (#1159) replaces whatever the plugin asked for,
    // here rather than in `run`, so a *reconnect* carries it too — the manifest is
    // rebuilt from `P::manifest()` on every session. Applied before the `Register`
    // frame below, which is the only place the mount is ever read.
    if let Some(mount) = mount_override {
        manifest.mount = mount;
    }
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
    // Capped at `VOCAB_UNCONDITIONAL` rather than trusted from the manifest
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
    // #904: the seed view is sanitised like every later one — it is both the
    // first frame on the wire and the baseline every later dedup compares
    // against, so a `NaN` here would poison both at once.
    sanitise_view(&mut last_view);
    write_frame(
        &mut wr,
        &PluginMsg::Render {
            tree: last_view.tree.clone(),
            // #1073: the wire frame boxes `panel`; the SDK's own `View.panel`
            // stays a bare `Option<Node>` (a plugin author never sees the box).
            panel: last_view.panel.clone().map(Box::new),
            // #1050: the seed frame carries the per-screen verdict too. A plugin
            // whose first view is already "nothing to show on DP-2" must not
            // flash a chip there for the interval until its next render.
            hidden_on: last_view.hidden_on.clone(),
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

    // #1058: which effect *kinds* have already warned this session about a
    // missing capability, so a plugin looping on a bad click logs once, not
    // once per frame. `negotiation.capabilities` is the manifest's own grant
    // set — cloned before `Register` moved the original — so this reads the
    // exact list the host will enforce against.
    let mut capability_warned: std::collections::HashSet<std::mem::Discriminant<Effect>> =
        std::collections::HashSet::new();

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

    let result = 'session: loop {
        let step = tokio::select! {
            // #1079: checked first (`biased`) so a process shutdown notice
            // always wins a tie against a simultaneously-ready host frame —
            // deterministically, not by the macro's default random pick —
            // rather than possibly rendering one more update before the loop
            // notices. Whichever branch actually ends the loop, the shutdown
            // hook still runs exactly once below: `*shutdown.borrow()` is
            // checked unconditionally after the loop, not only here.
            //
            // #1092 review L2: what this actually discards is a host frame
            // the reader task already decoded into `rx` but this loop had not
            // yet applied — e.g. an infobroker Allow/Revoke click still
            // sitting in the channel. A click already *applied* (queued on a
            // writer lane) is unaffected and the shutdown hook still sees it;
            // one still in flight on the wire when the signal lands is not
            // recovered by this mechanism.
            biased;
            Some(()) = shutdown_fired(&mut shutdown) => break 'session Ok(()),
            frame = rx.recv() => match frame {
                Some(Ok(HostMsg::StateSnapshot { snapshot })) => Step::Update(Input::Snapshot(snapshot)),
                // `output` (#1050, the monitor whose copy of the mirrored card
                // produced this) is carried through verbatim — including its
                // `None`, which means *not attributable* and must not be turned
                // into a guess here. See the doc on `Input::Event::output`.
                Some(Ok(HostMsg::Event { node, kind, output })) => {
                    Step::Update(Input::Event { node, kind, output })
                }
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
                    // #1168 item 6: every other way this loop can stop taking
                    // input says so on stderr (the reader's EOF, `Shutdown`,
                    // the signal path); a `sources()` stream ending was the
                    // one that did not, and it is the one an author is least
                    // likely to have meant. Every `sources()` in the tree is
                    // an endless stream — a `tick_stream`, a channel the
                    // plugin's own I/O task owns — so `None` here almost
                    // always means that task died (a panic, a dropped sender)
                    // and the plugin is now deaf to its own I/O for the rest
                    // of the session while still rendering host events, which
                    // looks like "the chip froze" and nothing else. Once per
                    // session: `src_done` keeps the stream from being polled
                    // again, so this cannot repeat.
                    //
                    // `eprintln!` rather than `tracing::warn!` for the reason
                    // documented on `drop_ungranted_effects`: most plugin
                    // binaries install no subscriber (`agents` and the
                    // claude bridge do, the rest do not), so a `tracing` line
                    // here would reach only some of them; stderr reaches every
                    // one, and it is what systemd routes to the journal. There is no "source index" to name: a
                    // plugin returns exactly one stream (it merges its own).
                    eprintln!(
                        "[{plugin_id}] sources() stream ended; no further app messages this session"
                    );
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
        let mut view = model.view();
        // #904: sanitise before the compare, never after it. A non-finite float
        // defeats the compare *itself* (`NaN != NaN`), so clamping afterwards
        // would fix the frame's contents but not the render storm sending it.
        sanitise_view(&mut view);
        // Dedup is unchanged: the whole `View` compares at once, so a panel
        // change while the chip tree is unchanged (the common case) still counts.
        let changed = view != last_view;
        let now = tokio::time::Instant::now();
        // Send iff there are effects (one-shot — never coalesced, they ride the
        // render frame), or the view changed AND the cap interval has elapsed
        // since the last send. A change within the interval is deferred: `pending`
        // arms the flush deadline (`next_send_allowed`) that delivers the
        // coalesced trailing frame. Decided on the effects `update` actually
        // returned, BEFORE the #1058 capability guard strips any — a step whose
        // only output was an ungranted effect must still put a frame on the
        // wire (review LOW-1): the host's own `runtime_render` (rendering flag,
        // `last_seen`, the control-center's violation count) only refreshes
        // when a frame arrives, and an all-dropped batch is not a reason to
        // withhold one — it is exactly the "effects turned out empty" case
        // `model.update` returning `Vec::new()` already takes this same path
        // for.
        let send = !effects.is_empty() || (changed && now >= next_send_allowed);
        if send {
            // #1058: drop anything the manifest didn't declare the capability
            // for, right before framing — see `drop_ungranted_effects`. Every
            // returned message must be surfaced twice: a `PluginMsg::Log` so
            // the *host* names the mistake (review HIGH-1 — a bare
            // `tracing::warn!` here reaches no plugin process, since none
            // installs a subscriber, and dropping the effect before the wire
            // silences the host-side warn/audit/violation-count this exact
            // mistake used to produce), and `eprintln!` for the plugin's own
            // stderr, matching every other author-facing line this runtime
            // emits.
            let (effects, warnings) =
                drop_ungranted_effects(effects, &negotiation.capabilities, &mut capability_warned);
            for msg in warnings {
                eprintln!("[{plugin_id}] {msg}");
                if let Err(e) = write_frame(
                    &mut wr,
                    &PluginMsg::Log {
                        level: LogLevel::Warn,
                        msg,
                    },
                )
                .await
                {
                    break 'session Err(e);
                }
            }
            let frame = PluginMsg::Render {
                tree: view.tree.clone(),
                panel: view.panel.clone().map(Box::new),
                // #1050. `changed` above is a whole-`View` compare, so a frame
                // whose *only* difference is the hidden-on set still sends —
                // which is the entire point: a plugin that goes from "shown on
                // both screens" to "hidden on DP-2" typically renders the very
                // same tree, and dedup on `(tree, panel)` alone would swallow it.
                hidden_on: view.hidden_on.clone(),
                effects,
            };
            if let Err(e) = write_frame(&mut wr, &frame).await {
                break 'session Err(e);
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
    // #1079: unconditional, not just on the `shutdown.changed()` branch above —
    // a host `Shutdown` or a transport error can end the loop on the very same
    // poll the flag flipped (`biased` only orders the *tie*, it can't stop the
    // other branch from having already been mid-flight), and either way a
    // requested shutdown must still run the hook exactly once. `borrow()` reads
    // the current value regardless of whether *this* receiver ever observed a
    // `changed()` — see `run_shutdown_hook`'s doc for the grace itself.
    if *shutdown.borrow() {
        run_shutdown_hook(&plugin_id, &mut model).await;
    }
    // Keeps the `!Send` marker live across every await above — see its comment.
    drop(thread_bound);
    result
}

/// Run `model.shutdown()` under [`SHUTDOWN_GRACE`], eprintln-ing rather than
/// panicking if it overruns — the process exits either way (#1079: a bounded
/// chance, not a blocking one). Factored out so [`session`] and the test
/// harness drive the exact same bounded path.
async fn run_shutdown_hook<P: Plugin>(plugin_id: &str, model: &mut P) {
    let started = Instant::now();
    match tokio::time::timeout(SHUTDOWN_GRACE, model.shutdown()).await {
        Ok(()) => eprintln!(
            "[{plugin_id}] shutting down: hook ran in {:.1?}, exiting",
            started.elapsed()
        ),
        Err(_) => eprintln!(
            "[{plugin_id}] shutting down: hook did not finish within {SHUTDOWN_GRACE:?}; \
             exiting anyway"
        ),
    }
}

/// The connect→session→backoff loop, factored from [`run`] so a test can
/// drive it with an in-memory connector and pin the runtime's headline
/// decision: a session ending `Ok` (host `Shutdown`) **redials** — it never
/// terminates the loop on its own (see the crate docs on why exiting would
/// strand a `Restart=on-failure` unit). A failed connect backs off the same
/// way. The one thing that *does* end the loop is `shutdown` firing (#1079):
/// checked (`biased`) against both the connect attempt and the backoff sleep,
/// so a signal arriving with no session up yet returns immediately — no
/// connect, no session, so [`Plugin::sources`] is never called — and one
/// arriving during a live session is handled by [`session`] itself (which
/// already ran the shutdown hook by the time it returns); either way this
/// function returns instead of looping again.
async fn reconnect_loop<P, R, W, C, Fut>(
    plugin_id: &str,
    mut shutdown: watch::Receiver<bool>,
    mut connect: C,
    mount_override: Option<Mount>,
) where
    P: Plugin,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
    C: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<(R, W)>>,
{
    let mut backoff = Backoff::new();
    let mut redial = Redial::new();
    loop {
        let connected = tokio::select! {
            biased;
            Some(()) = shutdown_fired(&mut shutdown) => {
                eprintln!("[{plugin_id}] shutdown requested; exiting before a session started");
                return;
            }
            result = connect() => result,
        };
        match connected {
            Ok((rd, wr)) => {
                let started = Instant::now();
                let outcome = session::<P, _, _>(rd, wr, shutdown.clone(), mount_override).await;
                let lived = started.elapsed();
                // Escalate the log iff we've hit a streak of immediate failures —
                // the #437 crash-loop signature — so it isn't a silent 5 s spin.
                let skew = redial.note(lived, outcome.is_ok());
                backoff.note_session(lived);
                // `session` already ran the shutdown hook if this is why it
                // ended (see its doc). Checked *before* logging `outcome`
                // below, not after (#1092 review L1): a host `Shutdown` and
                // the process notice both surface as this same `Ok(())`, and
                // this flag is the only thing that tells them apart —
                // logging `outcome` first said "host shut down; will
                // reconnect" on the exact path that neither the host shut
                // down nor will reconnect.
                if *shutdown.borrow() {
                    eprintln!("[{plugin_id}] shutting down; not reconnecting");
                    return;
                }
                match outcome {
                    Ok(()) => eprintln!("[{plugin_id}] host shut down; will reconnect"),
                    Err(e) if skew => eprintln!(
                        "[{plugin_id}] WARNING: session keeps failing immediately ({e}); \
                         the host may be older than this plugin's wire vocabulary \
                         (schema skew, #437) — update the shell",
                    ),
                    Err(e) => eprintln!("[{plugin_id}] session ended: {e}"),
                }
            }
            Err(e) => {
                eprintln!("[{plugin_id}] connect failed: {e}");
            }
        }
        tokio::select! {
            biased;
            Some(()) = shutdown_fired(&mut shutdown) => {
                eprintln!("[{plugin_id}] shutdown requested during backoff; exiting");
                return;
            }
            () = tokio::time::sleep(backoff.delay()) => {}
        }
    }
}

/// Run a [`Plugin`] against the trollshell host socket — forever. Owns the
/// process: builds a current-thread tokio runtime, dials
/// [`socket_path`](hytte_plugin_proto::socket_path) with bounded exponential
/// backoff (a host that isn't up yet — both start under the same session
/// target — or a host restart is a transient we ride out here rather than
/// exiting into systemd's start-limit), and drives one session per
/// connection. Exits the process (status 1) only on unrecoverable setup:
/// `XDG_RUNTIME_DIR` unset (then there is nothing to dial, ever), the
/// tokio runtime failing to build, or a [`MOUNT_ENV`] value that is not a wire
/// mount name.
///
/// **Placement is a launch argument** (#1159, epic #1158). Before the first dial,
/// `run` reads [`MOUNT_ENV`] (`HYTTE_PLUGIN_MOUNT`); a value naming one of the
/// nine wire [`Mount`](hytte_plugin_proto::Mount)s replaces
/// [`Plugin::manifest`]'s own `mount` in every `Register` this process sends,
/// including after a reconnect. An unknown or empty value is a **startup
/// failure** whose message names all nine spellings — never a silent fallback to
/// the manifest, which would put the card on the wrong sidebar and look healthy
/// doing it. The plugin's own code is not consulted and never sees the override:
/// `manifest()` is called exactly as before.
///
/// Also installs the `SIGTERM`/`SIGINT` listener for the shutdown lifecycle
/// (#1079, crate docs' "Process shutdown" section): on either signal a
/// process-wide flag flips, the live session (if any) finishes its in-flight
/// frame and runs [`Plugin::shutdown`] under [`SHUTDOWN_GRACE`] — which
/// bounds an `.await`ing hook only, not a thread-blocking one; see that
/// constant's doc — and this function exits the process with status 0
/// instead of reconnecting. Systemd's own `TimeoutStopSec` on the transient
/// unit (`trollshell/src/plugin_launcher.rs`) is the *outer* bound on all of
/// this — a plugin still not gone by then is `SIGKILL`ed. The listener is
/// only live once its spawned task is first polled (inside `block_on`
/// below), so a signal in the sub-millisecond window between process start
/// and that first poll gets the platform's default disposition (terminate)
/// rather than this graceful path — measured at under 1 ms in practice (the
/// `fork`/`exec` gap, not anything this function does), and not otherwise
/// fixable short of blocking `SIGTERM` before the runtime exists.
pub fn run<P: Plugin>() -> ! {
    let manifest = P::manifest();
    let plugin_id = manifest.id.clone();
    // The launch-time placement (#1159), resolved once and before the first dial:
    // an unparseable value is a startup failure, not something a plugin limps on
    // with its manifest's mount (see `MountOverrideError`). `eprintln!` rather
    // than `tracing::error!` for the reason the crate docs give — no plugin
    // process installs a `tracing` subscriber, so a `tracing` line here would
    // reach nobody, while the journal captures stderr for the transient unit.
    let mount_override = match mount_override_from_env() {
        Ok(mount) => mount,
        Err(e) => {
            eprintln!("[{plugin_id}] {e}");
            std::process::exit(1);
        }
    };
    // One line, only when the override actually changes something — a deployment
    // that pins a plugin to the mount it already asked for is not worth a line,
    // and a silent move to the other sidebar is exactly what a reader of this
    // journal would otherwise have to guess at.
    if let Some(mount) = mount_override.filter(|m| *m != manifest.mount) {
        eprintln!(
            "[{plugin_id}] {MOUNT_ENV}={} overrides the manifest's {}",
            mount.wire_name(),
            manifest.mount.wire_name(),
        );
    }
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

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Scheduled now, actually polled once `block_on` below starts driving
    // this runtime — a plain `Runtime::spawn` needs no "inside `block_on`"
    // context of its own.
    rt.spawn(async move {
        // A signal type that fails to install (a platform/sandbox oddity) just
        // never fires; the other one, if it installed, still can. Both
        // failing means this task quietly does nothing — no signal handling,
        // same as pre-#1079.
        let term = signal(SignalKind::terminate());
        let int = signal(SignalKind::interrupt());
        match (term, int) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::select! {
                    _ = term.recv() => {}
                    _ = int.recv() => {}
                }
            }
            (Ok(mut term), Err(_)) => {
                term.recv().await;
            }
            (Err(_), Ok(mut int)) => {
                int.recv().await;
            }
            (Err(_), Err(_)) => return,
        }
        // A closed receiver (the runtime is already tearing down some other
        // way) makes this a no-op, which is fine — there is nothing left to
        // notify.
        let _ = shutdown_tx.send(true);
    });

    rt.block_on(reconnect_loop::<P, _, _, _, _>(
        &plugin_id,
        shutdown_rx,
        move || {
            let path = path.clone();
            async move {
                let stream = UnixStream::connect(&path).await?;
                Ok(stream.into_split())
            }
        },
        mount_override,
    ));
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::{
        BACKOFF_BASE, BACKOFF_CAP, Backoff, IMMEDIATE_FAILURE, MOUNT_ENV, MountOverrideError,
        Redial, SHUTDOWN_GRACE, SKEW_WARN_AFTER, mount_override, reconnect_loop,
    };
    use crate::display::{Marquee, StyleName};
    use crate::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
    use hytte_plugin_proto::preem::PREEM_VOCAB;
    use hytte_plugin_proto::{
        AudioSpectrum, Capability, ClockState, ConsentDecision, Effect, EffectOutcome, EventKind,
        HostMsg, LogLevel, Manifest, Mount, Node, Page, PluginMsg, ProtoError, SPECTRUM_BINS,
        StateKey, StateSnapshot, VOCAB, VOCAB_UNCONDITIONAL, read_frame, write_frame,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::io::{AsyncRead, AsyncWrite, duplex};
    use tokio::sync::watch;

    /// A shutdown notice that never fires — the test stand-in for a plugin
    /// process's whole life with no `SIGTERM`/`SIGINT`, for every test that
    /// isn't itself about the #1079 shutdown lifecycle.
    fn never_shuts_down() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    /// [`super::session`] with **no** launch-time mount override — what every
    /// test here wants except the #1159 placement ones, which call
    /// `super::session` directly with a `Some`.
    ///
    /// A shim in the test module rather than a second entry point in the shipped
    /// lib: the override is not optional in production (it is threaded from
    /// [`run`] on every path), so a `None`-defaulting wrapper there would be dead
    /// code, while here it keeps thirty-odd call sites that have nothing to do
    /// with placement reading exactly as they did.
    async fn session<P, R, W>(
        rd: R,
        wr: W,
        shutdown: watch::Receiver<bool>,
    ) -> Result<(), ProtoError>
    where
        P: Plugin,
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Unpin,
    {
        super::session::<P, R, W>(rd, wr, shutdown, None).await
    }

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
                Input::Event { node, kind, .. } => {
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
                tooltip: None,
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
                tooltip: None,
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
                tooltip: None,
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
                tooltip: None,
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
                tooltip: None,
            })
            .panel(Node::Label {
                id: Some("paneled-panel".to_owned()),
                text: if self.open { "open" } else { "closed" }.to_owned(),
                classes: Vec::new(),
                tooltip: None,
            })
        }
    }

    /// Constant chip tree **and** constant panel, but the `View`'s `hidden_on`
    /// flips on a slot-visibility toggle (#1050). The per-screen verdict is the
    /// one thing a plugin routinely changes *without* changing what it draws —
    /// #1019's chip renders the identical three icons whether or not the active
    /// workspace on some output has two windows — so if dedup did not cover it,
    /// the frame that hides the chip would be the frame that gets swallowed.
    struct HiddenOn {
        hide: bool,
    }

    impl Plugin for HiddenOn {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("hidden-on-test", Mount::BarCenter)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { hide: false }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::SlotVisible(v) = input {
                self.hide = v;
            }
            Vec::new()
        }

        fn view(&self) -> View {
            let v = View::new(Node::Label {
                id: Some("hidden-on-chip".to_owned()),
                text: "chip".to_owned(),
                classes: Vec::new(),
                tooltip: None,
            });
            if self.hide { v.hidden_on(["DP-2"]) } else { v }
        }
    }

    /// Renders whichever screen the last click came from (#1050) — the fixture
    /// for "the wire's `output` reaches `update`".
    ///
    /// It has to *draw* the value rather than merely store it, because a
    /// `Render` frame is the only thing the socketpair harness can observe: a
    /// runtime that decoded `output` and then dropped it on the way into
    /// `Input::Event` — which is exactly what #1068 shipped — is invisible to
    /// every other test in this file, since none of them projects it.
    struct Attributed {
        last: String,
    }

    /// What [`Attributed`] draws for an event that named no screen. Spelled
    /// out so the assertion distinguishes "carried `None`" from "carried
    /// nothing at all"/"never got the event".
    const NO_OUTPUT: &str = "<none>";

    impl Plugin for Attributed {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("attributed-test", Mount::BarRight)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self {
                last: "seed".to_owned(),
            }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::Event { output, .. } = input {
                self.last = output.unwrap_or_else(|| NO_OUTPUT.to_owned());
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: Some("attributed-lbl".to_owned()),
                text: self.last.clone(),
                classes: Vec::new(),
                tooltip: None,
            }
            .into()
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
                Input::Event { node, kind, .. } => {
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
                tooltip: None,
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
                tooltip: None,
            }
            .into()
        }
    }

    /// #1058: emits `Effect::open_uri` on **every** click, and bumps its own
    /// view text on every click too — so a click always changes `tree`, and a
    /// dropped effect can never masquerade as "no frame sent" (the capability
    /// guard and the render-dedup/effects-force-a-send rule stay independently
    /// observable). `GRANTED` selects whether the manifest declares
    /// [`Capability::OpenUri`] at all; everything else about the two
    /// instantiations is identical.
    struct Linker<const GRANTED: bool> {
        clicks: u64,
    }

    impl<const GRANTED: bool> Plugin for Linker<GRANTED> {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            let mut m = Manifest::new("linker-test", Mount::SidebarTop);
            if GRANTED {
                m.capabilities = vec![Capability::OpenUri];
            }
            m
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self { clicks: 0 }
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            // `, ..` (#1058 review MEDIUM-3): #1083 adds `output` to
            // `Input::Event` and marks it `#[non_exhaustive]`, editing every
            // *existing* `Input::Event { node, kind }` arm in the tree to
            // match — but it cannot edit this one, since it doesn't exist on
            // whichever branch it was authored from. Legal today (the variant
            // isn't `#[non_exhaustive]` yet) and forward-compatible with that
            // PR, whichever of the two merges second.
            if let Input::Event { node, kind, .. } = input
                && node == "linker-btn"
                && matches!(kind, EventKind::Click)
            {
                self.clicks += 1;
                return vec![Effect::open_uri(self.clicks, "https://example.invalid/")];
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: Some("linker-lbl".to_owned()),
                text: format!("clicks={}", self.clicks),
                classes: Vec::new(),
                tooltip: None,
            }
            .into()
        }
    }

    /// #1058 review LOW-1: emits an ungranted effect on click but its view
    /// **never changes** — deliberately unlike `Linker`, whose view text
    /// bumps on every click and so would still force a send even if the
    /// capability guard's placement regressed. Isolates "does an all-dropped
    /// effects step still put a frame on the wire" from "did the view
    /// change", which `Linker`'s own tests cannot.
    struct SilentLinker;

    impl Plugin for SilentLinker {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("silent-linker-test", Mount::SidebarTop) // no capabilities
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self
        }

        fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
            if let Input::Event { node, kind, .. } = input
                && node == "silent-btn"
                && matches!(kind, EventKind::Click)
            {
                return vec![Effect::open_uri(1, "https://example.invalid/")];
            }
            Vec::new()
        }

        fn view(&self) -> View {
            Node::Label {
                id: Some("silent-lbl".to_owned()),
                text: "constant".to_owned(),
                classes: Vec::new(),
                tooltip: None,
            }
            .into()
        }
    }

    // ── Host-side helpers ────────────────────────────────────────────────────

    /// #904: a plugin whose view is **constant** but poisoned — a `NaN`
    /// `fraction` in the chip's `Progress`, and an inverted range with a `NaN`
    /// value and a zero step in the panel's `Slider`. Constant, so every
    /// re-render produces a view that *should* dedup; poisoned, so it only
    /// does once the SDK sanitises before the compare, since `NaN != NaN`
    /// defeats the derived `PartialEq` otherwise.
    struct Poisoned;

    impl Plugin for Poisoned {
        type Msg = std::convert::Infallible;
        type Cmd = std::convert::Infallible;

        fn manifest() -> Manifest {
            Manifest::new("poisoned-test", Mount::SidebarTop)
        }

        fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
            Self
        }

        fn update(&mut self, _input: Input<Self::Msg>) -> Vec<Effect> {
            Vec::new()
        }

        fn view(&self) -> View {
            View::new(Node::Progress {
                id: Some("bar".to_owned()),
                fraction: f64::NAN,
                classes: Vec::new(),
            })
            .panel(Node::Slider {
                id: "sld".to_owned(),
                min: 10.0,
                max: 5.0,
                value: f64::NAN,
                step: 0.0,
                enabled: true,
                classes: Vec::new(),
            })
        }
    }

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
                    tooltip: None,
                }
            );
            // Host goes away without Shutdown: both halves dropped → EOF.
            drop(hwr);
            drop(hrd);
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
    }

    /// #904, the SDK half. A **constant** view carrying non-finite floats is
    /// sanitised before the dedup compare, so the plugin emits exactly one
    /// `Render` (the seed) however many events arrive — and that one frame puts
    /// only finite floats on the wire.
    ///
    /// Both halves are load-bearing. Without the sanitise at the dedup site the
    /// `NaN` fraction keeps `view != last_view` true forever, so each of the
    /// three snapshots below emits a `Render` and the `Pong` barrier is never
    /// the next frame; without it at the seed site the first assertions fail
    /// instead, because the poison rides the wire.
    #[tokio::test]
    async fn a_poisoned_view_is_sanitised_before_the_dedup_and_renders_once() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let PluginMsg::Register { manifest } = next_plugin_frame(&mut hrd).await else {
                panic!("first frame must be Register");
            };
            assert_eq!(manifest.id, "poisoned-test");
            let PluginMsg::Log { .. } = next_plugin_frame(&mut hrd).await else {
                panic!("second frame must be the greeting Log");
            };
            let PluginMsg::Render { tree, panel, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("third frame must be the seed Render");
            };

            // The wire carries the mapping's documented images, not the poison
            // the plugin authored — chip tree and panel tree alike.
            let Node::Progress { fraction, .. } = tree else {
                panic!("the seed chip must be a Progress")
            };
            assert_eq!(
                fraction.to_bits(),
                0.0_f64.to_bits(),
                "a NaN fraction reached the wire"
            );
            let Some(Node::Slider {
                min,
                max,
                value,
                step,
                ..
            }) = panel.as_deref()
            else {
                panic!("the seed panel must be a Slider")
            };
            assert_eq!(
                min.to_bits(),
                0.0_f64.to_bits(),
                "a degenerate min reached the wire"
            );
            assert_eq!(
                max.to_bits(),
                1.0_f64.to_bits(),
                "a degenerate max reached the wire"
            );
            assert_eq!(
                value.to_bits(),
                0.0_f64.to_bits(),
                "a NaN value reached the wire"
            );
            assert_eq!(
                step.to_bits(),
                0.01_f64.to_bits(),
                "a zero step reached the wire"
            );

            // Three inbound events, none of which changes the model. Each drives
            // a full update → view → dedup pass, and a sanitised view compares
            // equal to the last one, so none may produce a Render. The Ping is
            // the sync barrier: the very next frame must be its Pong.
            send(&mut hwr, &snapshot("10:00")).await;
            send(&mut hwr, &snapshot("10:01")).await;
            send(&mut hwr, &snapshot("10:02")).await;
            send(&mut hwr, &HostMsg::Ping { seq: 9 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 9 }
                ),
                "a poisoned-but-unchanged view must dedup (Pong, not Render, follows)"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<Poisoned, _, _>(prd, pwr, never_shuts_down()),
            host
        );
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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
                ..
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
                matches!(panel.as_deref(), Some(Node::Label { text, .. }) if text == "closed"),
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
                ..
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
                matches!(panel.as_deref(), Some(Node::Label { text, .. }) if text == "open"),
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

        let (result, ()) =
            tokio::join!(session::<Paneled, _, _>(prd, pwr, never_shuts_down()), host);
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
    }

    #[tokio::test]
    async fn hidden_on_change_alone_forces_a_render() {
        // #1050: the per-screen verdict is part of the `View`, so a change to it
        // alone must still emit a `Render` — and an identical view must still be
        // deduped. Exactly the #349 argument for `panel`, and worth its own test
        // because this is the field a real plugin changes *most* often without
        // changing its tree.
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            let PluginMsg::Register { .. } = next_plugin_frame(&mut hrd).await else {
                panic!("first frame must be Register");
            };
            let PluginMsg::Log { .. } = next_plugin_frame(&mut hrd).await else {
                panic!("second frame must be the greeting Log");
            };
            let PluginMsg::Render { hidden_on, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("third frame must be the seed Render");
            };
            assert!(
                hidden_on.is_empty(),
                "the seed view is hidden nowhere, and an empty set stays off the wire",
            );

            // Flip the verdict while the chip tree stays byte-identical.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: true }).await;
            // Bounded, for [`next_render`]'s reason: the exact bug this test
            // exists to catch — a `View` change that never reaches the wire —
            // makes the awaited frame *not arrive at all*, and an unbounded read
            // then hangs the suite rather than naming itself. Measured: a
            // no-op `View::hidden_on` builder hung `cargo test` past 10 min
            // before this bound; with it, the same mutation fails in 5 s.
            let PluginMsg::Render {
                tree, hidden_on, ..
            } = tokio::time::timeout(Duration::from_secs(5), next_plugin_frame(&mut hrd))
                .await
                .expect("a hidden_on change alone must re-render (within 5 s)")
            else {
                panic!("a hidden_on change alone must produce a Render frame");
            };
            assert!(
                matches!(tree, Node::Label { ref text, .. } if text == "chip"),
                "the chip tree is unchanged across the flip — the change is the verdict",
            );
            assert_eq!(
                hidden_on,
                vec!["DP-2".to_owned()],
                "the render carries the new per-screen verdict",
            );

            // The same visibility again → identical view → deduped. The Ping is
            // the sync barrier: the next frame must be its Pong.
            send(&mut hwr, &HostMsg::SlotVisibility { visible: true }).await;
            send(&mut hwr, &HostMsg::Ping { seq: 5 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 5 }
                ),
                "an identical view (hidden_on included) must be deduped",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<HiddenOn, _, _>(prd, pwr, never_shuts_down()),
            host
        );
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
                    output: None,
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
        assert!(result.is_ok());
    }

    /// The label text of a `Render` frame's tree — the one thing
    /// [`Attributed`] projects.
    fn rendered_text(frame: PluginMsg) -> String {
        let PluginMsg::Render { tree, .. } = frame else {
            panic!("expected a Render frame, got {frame:?}");
        };
        match tree {
            Node::Label { text, .. } => text,
            other => panic!("expected a Label, got {other:?}"),
        }
    }

    /// #1050: the wire's `Event.output` reaches `Input::Event.output` verbatim —
    /// `Some(connector)` as itself, `None` as `None`.
    ///
    /// Both halves matter. The `Some` half is the feature; the `None` half is
    /// the promise that the runtime does not invent a screen for an event the
    /// host could not attribute (the drawer panel), which is what
    /// `Input::Event::output` documents and what a plugin's fallback path is
    /// keyed on.
    #[tokio::test]
    async fn the_events_output_reaches_update_verbatim() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "attributed-test").await;

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "attributed-lbl".to_owned(),
                    kind: EventKind::Click,
                    output: Some("DP-2".to_owned()),
                },
            )
            .await;
            assert_eq!(
                rendered_text(next_plugin_frame(&mut hrd).await),
                "DP-2",
                "the connector the host stamped must arrive at `update` unchanged"
            );

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "attributed-lbl".to_owned(),
                    kind: EventKind::Click,
                    output: None,
                },
            )
            .await;
            assert_eq!(
                rendered_text(next_plugin_frame(&mut hrd).await),
                NO_OUTPUT,
                "an unattributable event stays `None` — the runtime must not \
                 substitute a screen of its own"
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<Attributed, _, _>(prd, pwr, never_shuts_down()),
            host
        );
        assert!(result.is_ok(), "Shutdown ends the session cleanly");
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
                    output: None,
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) =
            tokio::join!(session::<Ticker, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(
            session::<FragileTicker, _, _>(prd, pwr, never_shuts_down()),
            host
        );
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) =
            tokio::join!(session::<Watcher, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Meter, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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
                    output: None,
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

        let (result, ()) = tokio::join!(
            session::<Commander, _, _>(prd, pwr, never_shuts_down()),
            host
        );
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
                        output: None,
                    },
                )
                .await;
                let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                    panic!("each session's fresh lane must round-trip");
                };
                assert!(matches!(tree, Node::Label { ref text, .. } if text == "io:42"));
                send(&mut hwr, &HostMsg::Shutdown).await;
            };

            let (result, ()) = tokio::join!(
                session::<Commander, _, _>(prd, pwr, never_shuts_down()),
                host
            );
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
                    output: None,
                },
            )
            .await;
            drop(hwr);
            drop(hrd);
        };

        let (result, ()) = tokio::join!(session::<Echo, _, _>(prd, pwr, never_shuts_down()), host);
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

        let dial_loop = reconnect_loop::<Echo, _, _, _, _>(
            "echo-test",
            never_shuts_down(),
            move || {
                let next = pending.pop();
                async move {
                    match next {
                        Some(end) => Ok(tokio::io::split(end)),
                        None => std::future::pending().await,
                    }
                }
            },
            None,
        );

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
            () = dial_loop => unreachable!("reconnect_loop never returns with a live shutdown notice that never fires"),
        }
    }

    /// PR #1092 review M1: `reconnect_loop`'s post-session exit gate
    /// (`if *shutdown.borrow() { return; }`, right after a live session ends)
    /// is what turns "the hook ran" into "the process exits instead of
    /// redialing" — the *only* path a real `systemctl stop` on a live session
    /// takes. `session` itself returns `Ok(())` whether a host `Shutdown` or
    /// the process notice caused it, so this is the one place that tells them
    /// apart at the `reconnect_loop` level and no other test here reaches it:
    /// every other shutdown test here drives `session` directly, and
    /// `shutdown_before_a_session_starts_skips_sources_and_exits` covers only
    /// the *pre-session* gate.
    ///
    /// The host never sends `HostMsg::Shutdown` here — only the process-level
    /// notice fires, once the session is up — so a connector call count of 1
    /// after `reconnect_loop` returns proves it exited rather than attempting
    /// a second connect (a redial).
    #[tokio::test]
    async fn shutdown_during_a_live_session_ends_reconnect_loop_without_a_redial() {
        let (p1, h1) = duplex(64 * 1024);
        let mut first = Some(p1);
        let connect_calls = Arc::new(AtomicUsize::new(0));
        let calls = connect_calls.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let dial_loop = reconnect_loop::<Echo, _, _, _, _>(
            "echo-test",
            shutdown_rx,
            move || {
                calls.fetch_add(1, Ordering::SeqCst);
                let next = first.take();
                async move {
                    match next {
                        Some(end) => Ok(tokio::io::split(end)),
                        // A second connect attempt (a redial) parks here forever;
                        // the outer timeout below turns that into a failure
                        // instead of a hang.
                        None => std::future::pending().await,
                    }
                }
            },
            None,
        );

        let host = async move {
            let (mut hrd, _hwr) = tokio::io::split(h1);
            eat_handshake(&mut hrd, "echo-test").await;
            shutdown_tx.send(true).expect("receiver still alive");
            // No `HostMsg::Shutdown` — the process-level notice alone must
            // end both the session and the outer loop.
        };

        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(dial_loop, host)
        })
        .await
        .expect(
            "reconnect_loop must return once a live session ends via shutdown, not redial/hang",
        );

        assert_eq!(
            connect_calls.load(Ordering::SeqCst),
            1,
            "the process must exit rather than attempt a second connect (redial)"
        );
    }

    // ── Shutdown (#1079) ─────────────────────────────────────────────────────

    /// (a) `shutdown` runs exactly once, and only *after* a frame that
    /// arrived **before** the signal has already been processed and
    /// rendered — never a second time for a host message sent **after** the
    /// notice. `Logged`'s `update` and `shutdown` both push into the same
    /// log, so the recorded order pins both properties at once: the `biased`
    /// shutdown-first arm must win the race against the snapshot sent right
    /// behind the notice, or `"update"` would appear twice.
    ///
    /// #1092 review L4: precisely, this pins "a frame that arrived before
    /// the signal still gets processed" — the host `.await`s the `10:00`
    /// `Render` before firing `shutdown_tx`, so there is no race on that
    /// half. It does *not* exercise "a frame already mid-processing when the
    /// signal lands is not aborted": that holds by construction (the
    /// `update`/`view`/send step has no `select!` inside it to be preempted
    /// by), not by anything this test observes.
    ///
    /// Falsification (PR body): skipping the in-flight-frame wait — e.g.
    /// checking `shutdown` before processing the already-selected step —
    /// reds this by reordering the log (`shutdown` before the first
    /// `update`, or `update` appearing twice).
    #[tokio::test]
    async fn shutdown_runs_the_hook_exactly_once_after_the_in_flight_frame() {
        static LOG: Mutex<Vec<&str>> = Mutex::new(Vec::new());

        struct Logged;

        impl Plugin for Logged {
            type Msg = std::convert::Infallible;
            type Cmd = std::convert::Infallible;

            fn manifest() -> Manifest {
                Manifest::new("shutdown-order-test", Mount::SidebarTop)
            }

            fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
                Self
            }

            fn update(&mut self, _input: Input<Self::Msg>) -> Vec<Effect> {
                LOG.lock().unwrap().push("update");
                Vec::new()
            }

            fn view(&self) -> View {
                Node::Label {
                    id: Some("logged-lbl".to_owned()),
                    text: LOG.lock().unwrap().len().to_string(),
                    classes: Vec::new(),
                    tooltip: None,
                }
                .into()
            }

            async fn shutdown(&mut self) {
                LOG.lock().unwrap().push("shutdown");
            }
        }

        // Falsification note (PR body): without the `biased` shutdown-first
        // arm, the race between the "11:00" frame and the shutdown notice is
        // *probabilistic* — a single trial can pass by luck (measured: 1 red
        // in 5 with `biased` removed). Looping many independent sessions is
        // what makes this a reliable falsifier: the fixed tree passes every
        // trial deterministically (`biased` always breaks the tie the same
        // way), while the mutation reds within a handful of trials.
        for _ in 0..20 {
            LOG.lock().unwrap().clear();

            let (plugin_end, host_end) = duplex(64 * 1024);
            let (prd, pwr) = tokio::io::split(plugin_end);
            let (mut hrd, mut hwr) = tokio::io::split(host_end);
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            let host = async move {
                eat_handshake(&mut hrd, "shutdown-order-test").await;
                send(&mut hwr, &snapshot("10:00")).await;
                assert!(
                    matches!(next_plugin_frame(&mut hrd).await, PluginMsg::Render { .. }),
                    "the update from before the signal must still render normally"
                );
                shutdown_tx.send(true).expect("receiver still alive");
                // Sent right after the notice: the biased shutdown-first arm
                // must win this race every time, so this must never reach
                // `update`.
                send(&mut hwr, &snapshot("11:00")).await;
            };

            let (result, ()) = tokio::join!(session::<Logged, _, _>(prd, pwr, shutdown_rx), host);
            assert!(result.is_ok(), "a shutdown notice ends the session cleanly");

            let log = LOG.lock().unwrap();
            assert_eq!(
                log.as_slice(),
                &["update", "shutdown"],
                "shutdown must run exactly once, after the one update the signal didn't race out"
            );
        }
    }

    /// (b) A `shutdown` hook that overruns [`SHUTDOWN_GRACE`] is cut off — the
    /// session still ends rather than hanging on a stuck plugin.
    /// `start_paused` auto-advances `Hanger::shutdown`'s sleep past the grace
    /// without this test taking real minutes.
    ///
    /// Falsification (PR body): removing the grace (`run_shutdown_hook`
    /// awaiting `model.shutdown()` directly instead of through
    /// `tokio::time::timeout`) makes `session` hang forever here — this
    /// test's own outer `tokio::time::timeout` is what turns that into a
    /// fast, named failure instead of a 75-minute CI hang.
    #[tokio::test(start_paused = true)]
    async fn a_hook_that_overruns_the_grace_is_cut_off() {
        struct Hanger;

        impl Plugin for Hanger {
            type Msg = std::convert::Infallible;
            type Cmd = std::convert::Infallible;

            fn manifest() -> Manifest {
                Manifest::new("hanger-test", Mount::SidebarTop)
            }

            fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
                Self
            }

            fn update(&mut self, _input: Input<Self::Msg>) -> Vec<Effect> {
                Vec::new()
            }

            fn view(&self) -> View {
                Node::Label {
                    id: Some("hanger-lbl".to_owned()),
                    text: "x".to_owned(),
                    classes: Vec::new(),
                    tooltip: None,
                }
                .into()
            }

            async fn shutdown(&mut self) {
                // Far past SHUTDOWN_GRACE; under `start_paused` this advances
                // virtually, so the test itself stays fast.
                tokio::time::sleep(SHUTDOWN_GRACE * 100).await;
            }
        }

        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, _hwr) = tokio::io::split(host_end);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let host = async move {
            eat_handshake(&mut hrd, "hanger-test").await;
            shutdown_tx.send(true).expect("receiver still alive");
        };

        let (result, ()) = tokio::time::timeout(Duration::from_mins(1), async {
            tokio::join!(session::<Hanger, _, _>(prd, pwr, shutdown_rx), host)
        })
        .await
        .expect("session must return once the grace elapses, not hang on the stuck shutdown hook");
        assert!(result.is_ok(), "the session still ends cleanly");
    }

    /// (c) A plugin that never overrides `shutdown` (the default no-op) exits
    /// promptly — the grace exists for a hook that does real work, not as a
    /// mandatory delay every plugin pays.
    #[tokio::test]
    async fn a_default_shutdown_hook_exits_promptly() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, _hwr) = tokio::io::split(host_end);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let host = async move {
            eat_handshake(&mut hrd, "echo-test").await;
            shutdown_tx.send(true).expect("receiver still alive");
        };

        let (result, ()) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(session::<Echo, _, _>(prd, pwr, shutdown_rx), host)
        })
        .await
        .expect("a plugin with no shutdown override must not delay the exit at all");
        assert!(result.is_ok());
    }

    /// (d) A shutdown notice that fires before any session ever connects (the
    /// dial/backoff phase) makes `reconnect_loop` exit without ever calling
    /// `connect` to completion or, therefore, [`Plugin::sources`] — there is
    /// no model in that case, so there is nothing to flush it from.
    #[tokio::test]
    async fn shutdown_before_a_session_starts_skips_sources_and_exits() {
        static SOURCES_CALLED: AtomicBool = AtomicBool::new(false);

        struct NeverConnects;

        impl Plugin for NeverConnects {
            type Msg = std::convert::Infallible;
            type Cmd = std::convert::Infallible;

            fn manifest() -> Manifest {
                Manifest::new("never-connects-test", Mount::SidebarTop)
            }

            fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
                Self
            }

            fn sources(_cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
                SOURCES_CALLED.store(true, Ordering::SeqCst);
                None
            }

            fn update(&mut self, _input: Input<Self::Msg>) -> Vec<Effect> {
                Vec::new()
            }

            fn view(&self) -> View {
                Node::Label {
                    id: Some("never-lbl".to_owned()),
                    text: "x".to_owned(),
                    classes: Vec::new(),
                    tooltip: None,
                }
                .into()
            }
        }

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        shutdown_tx.send(true).expect("receiver still alive");

        tokio::time::timeout(
            Duration::from_secs(5),
            reconnect_loop::<NeverConnects, _, _, _, _>(
                "never-connects-test",
                shutdown_rx,
                || {
                    std::future::pending::<
                        std::io::Result<(
                            tokio::io::ReadHalf<tokio::io::DuplexStream>,
                            tokio::io::WriteHalf<tokio::io::DuplexStream>,
                        )>,
                    >()
                },
                None,
            ),
        )
        .await
        .expect(
            "reconnect_loop must exit promptly on a pre-session shutdown notice, not hang dialing",
        );

        assert!(
            !SOURCES_CALLED.load(Ordering::SeqCst),
            "no session ever started, so sources() must never be called"
        );
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
        let (result, ()) = tokio::join!(
            session::<Scroller, _, _>(prd, pwr, never_shuts_down()),
            host(hrd, hwr)
        );
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

    // ── #1058: SDK-side capability guard ─────────────────────────────────────
    //
    // `drop_ungranted_effects` mirrors the host's own `session::
    // enforce_capabilities` (proto's `Effect::required_capability` is the one
    // mapping both call), so an author's own log names an undeclared effect
    // before an old host's decode failure does. `Linker<GRANTED>` bumps its
    // view text on *every* click, so a dropped effect never masquerades as "no
    // frame sent" — the guard and the render-dedup/effects-force-a-send rule
    // stay independently observable.
    //
    // The mismatch is asserted on the wire itself — a `PluginMsg::Log{Warn}`
    // frame — rather than by installing a `tracing` subscriber (review
    // HIGH-1/MEDIUM-4, #1058 fix round): the guard no longer calls
    // `tracing::warn!` at all (nothing in this SDK does; no plugin process
    // installs a subscriber, so it reached nobody), and a hand-rolled
    // `Counting` subscriber here would have been the fifth copy of a shape
    // #1044 is retiring elsewhere. Reading the actual frame is also a
    // strictly stronger assertion: it pins what the host receives, not what a
    // test-only observer sees.

    /// A manifest that never declares [`Capability::OpenUri`], but whose
    /// `update` emits `Effect::open_uri` on every click, gets that effect
    /// dropped before it reaches the wire — but the frame still goes out
    /// (the click also changes the view text), and the mismatch is named
    /// **once for the whole session**, not once per frame: the first click's
    /// `Log{Warn}` frame precedes its `Render`; the second click produces
    /// only a `Render` (nothing else queued ahead of the `Pong` that follows).
    ///
    /// **Falsified** by deleting the [`super::drop_ungranted_effects`] call
    /// from the session loop (both effects reappear and no `Log` frame ever
    /// arrives), by deleting just the `Log`-frame `write_frame` call (the
    /// first assertion turns red — a `Render` arrives where a `Log` was
    /// expected), or by moving the `warned.insert(..)` check so it re-warns
    /// every frame (a second `Log` frame arrives where the `Pong` was
    /// expected).
    #[tokio::test]
    async fn an_undeclared_effect_is_dropped_and_warns_once_per_session() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "linker-test").await;

            let click = HostMsg::Event {
                node: "linker-btn".to_owned(),
                kind: EventKind::Click,
                output: None,
            };

            // First click: the drop is visible on the wire ahead of the
            // Render it also produces.
            send(&mut hwr, &click).await;
            let PluginMsg::Log { level, msg } = next_plugin_frame(&mut hrd).await else {
                panic!("the first undeclared effect must warn via a Log frame");
            };
            assert_eq!(level, LogLevel::Warn);
            assert!(
                msg.contains("OpenUri") && msg.contains("capability"),
                "the message should name the effect and the missing capability: {msg}",
            );
            let PluginMsg::Render { effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a click that changes the view must still produce a Render frame");
            };
            assert!(
                effects.is_empty(),
                "the undeclared OpenUri effect must be dropped"
            );

            // Second click, same effect kind: the drop repeats, the warning
            // does not.
            send(&mut hwr, &click).await;
            let PluginMsg::Render { effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a click that changes the view must still produce a Render frame");
            };
            assert!(
                effects.is_empty(),
                "the undeclared OpenUri effect must be dropped again",
            );

            // And nothing is queued behind it either: a liveness round-trip
            // proves no extra Log frame is waiting.
            send(&mut hwr, &HostMsg::Ping { seq: 7 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Pong { seq: 7 }
                ),
                "no second Log frame may be queued after the second click",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<Linker<false>, _, _>(prd, pwr, never_shuts_down()),
            host
        );
        assert!(result.is_ok());
    }

    /// The mirror case: a manifest that DOES declare [`Capability::OpenUri`]
    /// gets the effect through untouched, with no `Log` frame at all.
    #[tokio::test]
    async fn a_declared_effect_is_framed() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "linker-test").await;

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "linker-btn".to_owned(),
                    kind: EventKind::Click,
                    output: None,
                },
            )
            .await;
            let PluginMsg::Render { effects, .. } = next_plugin_frame(&mut hrd).await else {
                panic!("a click must produce a Render frame");
            };
            assert_eq!(
                effects,
                vec![Effect::open_uri(1, "https://example.invalid/")],
                "a declared capability lets its effect through unchanged",
            );

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<Linker<true>, _, _>(prd, pwr, never_shuts_down()),
            host
        );
        assert!(result.is_ok());
    }

    /// #1058 review LOW-1: a step whose only output was an ungranted effect
    /// must still put a frame on the wire — the host's own `runtime_render`
    /// (the control-center's "rendering"/`last_seen` freshness signal) only
    /// refreshes when a frame arrives, and an all-dropped batch is not a
    /// reason to withhold one. `SilentLinker`'s view never changes, so
    /// `changed` alone can never explain a send here — only the pre-guard
    /// effects presence can.
    ///
    /// **Falsified** by deciding `send` on the post-guard (filtered) effects
    /// instead of the effects `update` actually returned: with
    /// `SilentLinker`'s view held constant, `send` then evaluates to `false`
    /// for this click, so **neither** the `Log` nor the `Render` frame is
    /// ever put on the wire — both reads below time out and this test reds
    /// in seconds.
    ///
    /// Both reads are bounded (#1058 fix-round verification, LOW-1 finding):
    /// a plain, unbounded `next_plugin_frame` doesn't fail under that
    /// mutation, it **hangs** — the host future awaits a frame that is never
    /// sent, so `tokio::join!` never completes, and in CI that's a job
    /// burned to its own workflow timeout (#1011's shape) rather than a red
    /// test. Bounding matches [`next_render`]'s own reasoning and the
    /// `hidden_on`-flip test a few sessions up in this file.
    #[tokio::test]
    async fn an_all_dropped_step_still_sends_a_frame() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "silent-linker-test").await;

            send(
                &mut hwr,
                &HostMsg::Event {
                    node: "silent-btn".to_owned(),
                    kind: EventKind::Click,
                    output: None,
                },
            )
            .await;
            // The click's effect is ungranted, so a `Log{Warn}` frame
            // precedes the `Render` — not what this test is pinning; skip it.
            let PluginMsg::Log { .. } =
                tokio::time::timeout(Duration::from_secs(5), next_plugin_frame(&mut hrd))
                    .await
                    .expect("the ungranted effect must warn via a Log frame (within 5 s)")
            else {
                panic!("the ungranted effect must still warn via a Log frame");
            };
            let PluginMsg::Render { effects, .. } =
                tokio::time::timeout(Duration::from_secs(5), next_plugin_frame(&mut hrd))
                    .await
                    .expect(
                        "an all-dropped-effects step must still produce a Render frame \
                         (within 5 s), even though the view itself never changes",
                    )
            else {
                panic!(
                    "an all-dropped-effects step must still produce a Render frame, \
                     even though the view itself never changes"
                );
            };
            assert!(effects.is_empty(), "the ungranted effect is still dropped");

            send(&mut hwr, &HostMsg::Shutdown).await;
        };

        let (result, ()) = tokio::join!(
            session::<SilentLinker, _, _>(prd, pwr, never_shuts_down()),
            host
        );
        assert!(result.is_ok());
    }

    // ── Launch-time mount override (#1159, epic #1158) ──────────────────────

    /// Drive the handshake half of a session with `override_mount` in force and
    /// return the mount that actually went out in `Register`.
    ///
    /// Reads only the first frame, then drops both host halves: the plugin's
    /// following `Log`/`Render` writes fail, the session returns `Err`, and that
    /// is fine — the claim under test is entirely about the frame already read.
    async fn registered_mount(override_mount: Option<Mount>) -> Mount {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, hwr) = tokio::io::split(host_end);

        let host = async move {
            let PluginMsg::Register { manifest } = next_plugin_frame(&mut hrd).await else {
                panic!("first frame must be Register");
            };
            drop(hwr);
            drop(hrd);
            manifest.mount
        };

        let (_ended, mount) = tokio::join!(
            super::session::<Echo, _, _>(prd, pwr, never_shuts_down(), override_mount),
            host
        );
        mount
    }

    /// No override → the plugin's own manifest wins, unchanged. The baseline
    /// every deployed plugin is on today, and the one case where a regression
    /// would move every existing card at once.
    #[tokio::test]
    async fn without_an_override_the_manifest_mount_registers() {
        assert_eq!(
            Echo::manifest().mount,
            Mount::SidebarTop,
            "precondition: the test plugin asks for SidebarTop",
        );
        assert_eq!(
            registered_mount(None).await,
            Mount::SidebarTop,
            "an unset HYTTE_PLUGIN_MOUNT leaves the manifest's mount alone",
        );
    }

    /// Each of the nine mounts, set as the override, is the mount that goes out
    /// in `Register` — including `SidebarTop`, which is the one the manifest
    /// already asked for (so "the override is applied" is not confused with "the
    /// manifest happened to agree").
    ///
    /// This is the whole feature: `plugins.<id>.mount` in nix (#1161) becomes
    /// `HYTTE_PLUGIN_MOUNT` on the launch, and the only thing that has to be true
    /// is that the frame the host reads says so.
    ///
    /// **Falsified** by deleting the `if let Some(mount) = mount_override` arm in
    /// `session` (eight of the nine rows red), or by applying it *after* the
    /// `Register` write (all nine).
    #[tokio::test]
    async fn every_mount_override_reaches_the_register_frame() {
        for mount in Mount::ALL {
            assert_eq!(
                registered_mount(Some(mount)).await,
                mount,
                "HYTTE_PLUGIN_MOUNT={} must be the mount in Register",
                mount.wire_name(),
            );
        }
    }

    /// An override survives a **reconnect**: the manifest is rebuilt from
    /// `P::manifest()` once per session, so an override applied only on the first
    /// pass would silently move the card back to the left sidebar the first time
    /// the shell restarted.
    ///
    /// Two prepared connections, the host sending `Shutdown` on the first; both
    /// handshakes must name the override. This is why the override lives on
    /// `session` (which runs per connection) rather than being applied once in
    /// `run`.
    ///
    /// **Falsified** by moving the override onto `run`'s own `P::manifest()` copy
    /// instead of threading it into `session` — the second handshake then reports
    /// `SidebarTop`.
    #[tokio::test]
    async fn a_mount_override_survives_a_reconnect() {
        let (p1, h1) = duplex(64 * 1024);
        let (p2, h2) = duplex(64 * 1024);
        let mut pending = vec![p2, p1];

        let dial_loop = reconnect_loop::<Echo, _, _, _, _>(
            "echo-test",
            never_shuts_down(),
            move || {
                let next = pending.pop();
                async move {
                    match next {
                        Some(end) => Ok(tokio::io::split(end)),
                        None => std::future::pending().await,
                    }
                }
            },
            Some(Mount::SidebarRightBottom),
        );

        let host = async move {
            let (mut hrd1, mut hwr1) = tokio::io::split(h1);
            let PluginMsg::Register { manifest } = next_plugin_frame(&mut hrd1).await else {
                panic!("first frame must be Register");
            };
            assert_eq!(
                manifest.mount,
                Mount::SidebarRightBottom,
                "the first session registers on the overridden mount",
            );
            send(&mut hwr1, &HostMsg::Shutdown).await;

            let (mut hrd2, _hwr2) = tokio::io::split(h2);
            let PluginMsg::Register { manifest } = next_plugin_frame(&mut hrd2).await else {
                panic!("the redial's first frame must be Register");
            };
            assert_eq!(
                manifest.mount,
                Mount::SidebarRightBottom,
                "…and so does the session after the reconnect",
            );
        };

        tokio::select! {
            () = host => {}
            () = dial_loop => unreachable!("reconnect_loop never returns with a shutdown notice that never fires"),
        }
    }

    /// The parser, with no environment in it: unset is "no override", and every
    /// wire name resolves to its own mount.
    #[test]
    fn the_mount_override_parser_accepts_exactly_the_nine_wire_names() {
        assert_eq!(
            mount_override(None),
            Ok(None),
            "an unset variable is not an error — the manifest simply wins",
        );
        for mount in Mount::ALL {
            assert_eq!(
                mount_override(Some(mount.wire_name())),
                Ok(Some(mount)),
                "{} parses as itself",
                mount.wire_name(),
            );
        }
        // Surrounding whitespace is tolerated because a trimmed name still names
        // exactly one mount (so this can never misplace a card), and a stray space
        // in a Nix string is otherwise a launch failure with a baffling message.
        assert_eq!(
            mount_override(Some("  SidebarRightLead\t")),
            Ok(Some(Mount::SidebarRightLead)),
            "surrounding whitespace is trimmed before the lookup",
        );
    }

    /// An unparseable value is **refused**, and the refusal names all nine
    /// spellings — the one thing that makes this recoverable for whoever set it.
    /// A silent fallback to the manifest is the outcome this test exists to
    /// forbid: it would put the card on the other sidebar and leave the plugin
    /// looking perfectly healthy.
    ///
    /// The empty and whitespace-only cases are here on purpose: `--setenv=K=` from
    /// an empty Nix string is a plausible mistake, and reading it as "unset" would
    /// swallow it.
    ///
    /// **Falsified** by having `mount_override` return `Ok(None)` on a bad value
    /// (every row reds), by a case-insensitive lookup (the `sidebarrightlead` row),
    /// or by writing the nine names out in `Display` instead of reading
    /// `Mount::ALL` (the message stops listing a tenth mount the day one lands —
    /// which is why the loop below reads `ALL` too).
    #[test]
    fn an_unparseable_mount_override_is_refused_and_names_every_spelling() {
        for bad in [
            "",
            "   ",
            "SidebarRight",
            "sidebarrightlead",
            "SidebarRightMiddle",
            "BarTop",
            "right",
        ] {
            let err = mount_override(Some(bad))
                .expect_err("an unparseable override must be refused, never ignored");
            assert_eq!(
                err.value, bad,
                "the message quotes the value verbatim, untrimmed",
            );
            let msg = err.to_string();
            assert!(
                msg.contains(MOUNT_ENV),
                "the message names the variable: {msg}",
            );
            for mount in Mount::ALL {
                assert!(
                    msg.contains(mount.wire_name()),
                    "the message must name {} as a valid spelling: {msg}",
                    mount.wire_name(),
                );
            }
        }
    }

    /// A non-UTF-8 value is refused like any other unparseable one rather than
    /// quietly ignored — the `VarError::NotUnicode` arm of
    /// `mount_override_from_env`, reached here through `MountOverrideError`
    /// directly because constructing the process environment from a test would
    /// need `unsafe`.
    #[test]
    fn a_non_utf8_mount_override_still_names_the_spellings() {
        let err = MountOverrideError {
            value: String::from_utf8_lossy(b"Sidebar\xffRight").into_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains(MOUNT_ENV), "{msg}");
        for mount in Mount::ALL {
            assert!(msg.contains(mount.wire_name()), "{msg}");
        }
    }
}
