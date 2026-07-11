//! The transport runtime extracted from the reference plugin (#275): dial +
//! bounded backoff, the `Register` handshake, and the read→update→render
//! session loop. A plugin author never touches this — [`run`] is the whole
//! surface.

use std::time::{Duration, Instant};

use hytte_plugin_proto::{
    HostMsg, LogLevel, PluginMsg, ProtoError, read_frame, socket_path, write_frame,
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

/// Drive one connected session: handshake, seed render, then the
/// read→update→render loop. `Ok(())` means the host sent `Shutdown`; any
/// transport failure (EOF = the host went away) surfaces as `Err`. Either way
/// the caller redials — see the crate docs on why `Shutdown` does not exit.
///
/// Generic over the I/O halves (not `UnixStream`) so the whole loop is
/// hermetically testable over `tokio::io::duplex`.
async fn session<P, R, W>(rd: R, mut wr: W) -> Result<(), ProtoError>
where
    P: Plugin,
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
{
    // Handshake: `Register` MUST be the first frame (else the host drops us),
    // then a greeting through the host log (exercises the `Log` frame path).
    let manifest = P::manifest();
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

    // Seed render: the fresh model's view goes out immediately, so the slot
    // mounts before the first state snapshot lands.
    let mut model = P::init();
    let mut last_tree = model.view();
    write_frame(
        &mut wr,
        &PluginMsg::Render {
            tree: last_tree.clone(),
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
    // polled again (a `Stream` may panic after its final `None`).
    let mut src = P::sources().unwrap_or_else(|| Box::pin(tokio_stream::pending()));
    let mut src_done = false;

    let result = loop {
        let input = tokio::select! {
            frame = rx.recv() => match frame {
                Some(Ok(HostMsg::StateSnapshot { snapshot })) => Input::Snapshot(snapshot),
                Some(Ok(HostMsg::Event { node, kind })) => Input::Event { node, kind },
                Some(Ok(HostMsg::EffectResult { id, outcome })) => {
                    Input::EffectResult { id, outcome }
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
                    Input::App(m)
                } else {
                    src_done = true;
                    continue;
                }
            },
        };

        // update → view → dedup: send iff the tree changed or there are
        // effects to deliver (effects ride the render frame, so a non-empty
        // batch forces a send even for an identical tree).
        let effects = model.update(input);
        let tree = model.view();
        if !effects.is_empty() || tree != last_tree {
            let frame = PluginMsg::Render {
                tree: tree.clone(),
                effects,
            };
            if let Err(e) = write_frame(&mut wr, &frame).await {
                break Err(e);
            }
            last_tree = tree;
        }
    };
    // Stop reading; the caller drops the write half, which half-closes the
    // socket and lets the host reap the connection.
    reader.abort();
    result
}

/// Run a [`Plugin`] against the trollshell host socket — forever. Owns the
/// process: builds a current-thread tokio runtime, dials
/// [`socket_path`](hytte_plugin_proto::socket_path) with bounded exponential
/// backoff (a host that isn't up yet — both start under the same session
/// target — or a host restart is a transient we ride out here rather than
/// exiting into systemd's start-limit), and drives one session per
/// connection. Exits the process (status 1) only if `XDG_RUNTIME_DIR` is
/// unset — then there is nothing to dial, ever.
pub fn run<P: Plugin>() -> ! {
    let plugin_id = P::manifest().id;
    let Some(path) = socket_path() else {
        eprintln!("[{plugin_id}] XDG_RUNTIME_DIR unset; no host socket to dial");
        std::process::exit(1);
    };

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

    rt.block_on(async {
        let mut backoff = Backoff::new();
        loop {
            match UnixStream::connect(&path).await {
                Ok(stream) => {
                    let started = Instant::now();
                    let (rd, wr) = stream.into_split();
                    match session::<P, _, _>(rd, wr).await {
                        Ok(()) => eprintln!("[{plugin_id}] host shut down; will reconnect"),
                        Err(e) => eprintln!("[{plugin_id}] session ended: {e}"),
                    }
                    backoff.note_session(started.elapsed());
                }
                Err(e) => {
                    eprintln!("[{plugin_id}] connect {}: {e}", path.display());
                }
            }
            tokio::time::sleep(backoff.delay()).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{BACKOFF_BASE, BACKOFF_CAP, Backoff, session};
    use crate::{Input, MsgStream, Plugin};
    use hytte_plugin_proto::{
        Capability, ClockState, Effect, EventKind, HostMsg, Manifest, Mount, Node, Page, PluginMsg,
        StateKey, StateSnapshot, read_frame, write_frame,
    };
    use tokio::io::{AsyncRead, AsyncWrite, duplex};

    // ── Test plugins ─────────────────────────────────────────────────────────

    /// Host-driven: shows the latest snapshot's `iso`; a click on `echo-btn`
    /// emits one `OpenPage` effect while the tree stays unchanged (the view
    /// doesn't depend on clicks) — exactly the effects-force-send case.
    struct Echo {
        iso: String,
    }

    impl Plugin for Echo {
        type Msg = std::convert::Infallible;

        fn manifest() -> Manifest {
            let mut m = Manifest::new("echo-test", Mount::SidebarTop);
            m.subscribes = vec![StateKey::Clock];
            m.capabilities = vec![Capability::OpenPage];
            m
        }

        fn init() -> Self {
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
                Input::EffectResult { .. } => Vec::new(),
                Input::App(never) => match never {},
            }
        }

        fn view(&self) -> Node {
            Node::Label {
                id: Some("echo-lbl".to_owned()),
                text: self.iso.clone(),
                classes: Vec::new(),
            }
        }
    }

    /// Self-driven: folds messages from a finite `sources()` stream, so the
    /// session must merge app messages and survive the stream ending.
    struct Ticker {
        count: u32,
    }

    impl Plugin for Ticker {
        type Msg = u32;

        fn manifest() -> Manifest {
            Manifest::new("ticker-test", Mount::SidebarBottom)
        }

        fn init() -> Self {
            Self { count: 0 }
        }

        fn sources() -> Option<MsgStream<u32>> {
            Some(Box::pin(tokio_stream::iter([1_u32, 2, 3])))
        }

        fn update(&mut self, input: Input<u32>) -> Vec<Effect> {
            if let Input::App(n) = input {
                self.count += n;
            }
            Vec::new()
        }

        fn view(&self) -> Node {
            Node::Label {
                id: None,
                text: self.count.to_string(),
                classes: Vec::new(),
            }
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
        let PluginMsg::Render { tree, effects } = next_plugin_frame(rd).await else {
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
            let PluginMsg::Render { tree, effects } = next_plugin_frame(&mut hrd).await else {
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
            let PluginMsg::Render { tree, effects } = next_plugin_frame(&mut hrd).await else {
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
