//! The transport runtime extracted from the reference plugin (#275): dial +
//! bounded backoff, the `Register` handshake, and the read→update→render
//! session loop. A plugin author never touches this — [`run`] is the whole
//! surface.

use std::future::Future;
use std::time::{Duration, Instant};

use hytte_plugin_proto::{
    HostMsg, LogLevel, PluginMsg, ProtoError, StateKey, read_frame, socket_path, write_frame,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::{Input, Plugin};

/// Reconnect backoff bounds: start small, cap so we never hammer the socket.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

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

/// One session-loop iteration's work: fold a plugin-facing [`Input`] through
/// `update`, or perform a runtime-internal [`Rerender`](Step::Rerender) that
/// refreshes the view without an `update` — used when the host installs a new
/// accent (#376), which changes what the `preem` kit paints but is not a TEA
/// message.
enum Step<M> {
    Update(Input<M>),
    Rerender,
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
                Some(Ok(HostMsg::Accent { color })) => {
                    // Theme plumbing (#376): the host resolved `@accent_color`
                    // and handed it over. Feed it to the `preem` kit as the
                    // default widget tint (an explicit plugin palette still
                    // wins), then fall through to re-render so the new default
                    // shows. Never surfaced to the TEA model.
                    crate::preem::set_accent(color);
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
        };

        // update → view → dedup: send iff the `View` (tree or panel) changed,
        // or there are effects to deliver (effects ride the render frame, so a
        // non-empty batch forces a send even for an identical view). The whole
        // `View` compares at once, so a panel change while the chip tree is
        // unchanged (the common case) still pushes a frame. A `Rerender` (e.g.
        // an accent install, #376) refreshes the view without an `update`, so
        // the accent-tinted frame repaints.
        let effects = match step {
            Step::Update(input) => model.update(input),
            Step::Rerender => Vec::new(),
        };
        let view = model.view();
        if !effects.is_empty() || view != last_view {
            let frame = PluginMsg::Render {
                tree: view.tree.clone(),
                panel: view.panel.clone(),
                effects,
            };
            if let Err(e) = write_frame(&mut wr, &frame).await {
                break Err(e);
            }
            last_view = view;
        }
    };
    // Stop reading; the caller drops the write half, which half-closes the
    // socket and lets the host reap the connection.
    reader.abort();
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
    loop {
        match connect().await {
            Ok((rd, wr)) => {
                let started = Instant::now();
                match session::<P, _, _>(rd, wr).await {
                    Ok(()) => eprintln!("[{plugin_id}] host shut down; will reconnect"),
                    Err(e) => eprintln!("[{plugin_id}] session ended: {e}"),
                }
                backoff.note_session(started.elapsed());
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
    use super::{BACKOFF_BASE, BACKOFF_CAP, Backoff, reconnect_loop, session};
    use crate::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
    use hytte_plugin_proto::{
        AudioSpectrum, Capability, ClockState, ConsentDecision, Effect, EffectOutcome, EventKind,
        HostMsg, Manifest, Mount, Node, Page, PluginMsg, SPECTRUM_BINS, StateKey, StateSnapshot,
        read_frame, write_frame,
    };
    use std::pin::Pin;
    use std::task::{Context, Poll};
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
                | Input::NowPlaying(_) => {}
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

            // A burst with a *pending* render: a changed snapshot and a Ping
            // queued back-to-back must come out strictly FIFO — Render, then
            // Pong.
            send(&mut hwr, &snapshot("11:00")).await;
            send(&mut hwr, &HostMsg::Ping { seq: 8 }).await;
            assert!(
                matches!(
                    next_plugin_frame(&mut hrd).await,
                    PluginMsg::Render { tree: Node::Label { ref text, .. }, .. } if text == "11:00"
                ),
                "the queued snapshot renders before the Pong"
            );
            assert!(matches!(
                next_plugin_frame(&mut hrd).await,
                PluginMsg::Pong { seq: 8 }
            ));

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

    #[tokio::test]
    async fn sources_feed_app_inputs_and_a_finished_source_keeps_the_session_alive() {
        let (plugin_end, host_end) = duplex(64 * 1024);
        let (prd, pwr) = tokio::io::split(plugin_end);
        let (mut hrd, mut hwr) = tokio::io::split(host_end);

        let host = async move {
            eat_handshake(&mut hrd, "ticker-test").await;

            // The three source messages fold in order: 1, 1+2, 1+2+3.
            for expected in ["1", "3", "6"] {
                let PluginMsg::Render { tree, .. } = next_plugin_frame(&mut hrd).await else {
                    panic!("each source message must re-render");
                };
                assert!(
                    matches!(tree, Node::Label { ref text, .. } if text == expected),
                    "source messages fold in order (expected {expected})"
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
}
