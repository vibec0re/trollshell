//! `hytte-plugin-clock-demo` — the reference out-of-process widget plugin for
//! trollshell's "frontend B" plugin architecture (issue #35; on the #266 wire
//! protocol and the #272 host transport).
//!
//! It is the **end-to-end proof** that a plugin can live outside the shell,
//! link **no GTK** (only [`hytte_plugin_proto`] + `tokio`), and drive a real
//! widget over a Unix socket. It renders a clock into the shell's `SidebarTop`
//! slot and, when its button is clicked, asks the host to open the power menu —
//! exercising the render path, the state-subscription path, and the
//! event→effect round-trip in one demo.
//!
//! # Shape — The Elm Architecture (reducer + view)
//!
//! The plugin is autonomous: it holds **all** its own state ([`Local`]); the
//! host is a stateless render target. The core is two pure functions:
//!
//! - [`reduce`] folds one inbound [`HostMsg`] into `Local` and decides the next
//!   [`Step`] (re-render, reply to a ping, exit, or ignore).
//! - [`view`] projects `Local` into a declarative [`Node`] tree.
//!
//! Both are unit-tested (see the `tests` module) — that reducer test is the
//! demo's main correctness signal, since the live host isn't reachable here.
//!
//! # Lifecycle
//!
//! `main` dials `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with a bounded
//! exponential backoff (so a host that isn't up yet, or a host restart, never
//! turns into a hard crash-loop), sends [`PluginMsg::Register`] as the first
//! frame, then loops [`read_frame`] → [`reduce`] → [`write_frame`] until the
//! host disconnects (socket EOF). systemd's `Restart=on-failure` is the outer
//! supervisor for genuine process failures.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use hytte_plugin_proto::{
    Capability, Dir, Effect, EventKind, HostMsg, LogLevel, Manifest, Mount, Node, Page, PluginMsg,
    ProtoError, StateKey, read_frame, write_frame,
};
use tokio::net::UnixStream;

/// Stable plugin id — the host's mount-slot ownership key and audit-log subject.
const PLUGIN_ID: &str = "clock-demo";
/// Node ids. `CLOCK_BTN` is the click event target (a `Button` requires an id).
const ROOT_ID: &str = "clock-demo-root";
const TIME_ID: &str = "clock-demo-time";
const CLOCK_BTN: &str = "clock-demo-btn";

/// Reconnect backoff bounds: start small, cap so we never hammer the socket.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

// ── The model ────────────────────────────────────────────────────────────────

/// The plugin's entire state. Lives here — the host never stores or round-trips
/// it; on a crash we lose only this and re-derive from the next snapshot.
#[derive(Debug, Default, PartialEq, Eq)]
struct Local {
    /// Latest ISO-8601 local timestamp from the host's clock subscription.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState`).
    unix: i64,
}

/// The reducer's decision for one inbound [`HostMsg`].
#[derive(Debug)]
enum Step {
    /// Re-render, bundling these effects on the frame (usually empty; a button
    /// click carries the one-shot `OpenPage`).
    Render(Vec<Effect>),
    /// Reply to a host [`HostMsg::Ping`] with this `seq` (no re-render).
    Pong(u64),
    /// Exit cleanly (host [`HostMsg::Shutdown`]).
    Shutdown,
    /// Nothing to do.
    Ignore,
}

/// Fold one host message into `state` and decide what to do next. Pure and
/// panic-free over any host-sent bytes — the reducer is the testable heart of
/// the plugin.
fn reduce(state: &mut Local, msg: HostMsg) -> Step {
    match msg {
        // Subscribed-state snapshot: update our clock and re-render. `clock` is
        // optional on the wire (a startup snapshot may arrive before the host's
        // clock pump has published), so tolerate `None`.
        HostMsg::StateSnapshot { snapshot } => {
            if let Some(clock) = snapshot.clock {
                state.iso = clock.iso;
                state.unix = clock.unix;
                Step::Render(Vec::new())
            } else {
                Step::Ignore
            }
        }
        // User interaction on our tree. Our only interactive node is the button;
        // a click asks the host to open the power menu (bundled as a one-shot
        // effect on the next render, so a clock tick never re-fires it).
        HostMsg::Event { node, kind } => {
            if node == CLOCK_BTN && matches!(kind, EventKind::Click) {
                Step::Render(vec![Effect::OpenPage(Page::PowerMenu)])
            } else {
                Step::Ignore
            }
        }
        // Liveness: the v1 host never sends Ping, but a correct plugin answers
        // one if it ever does.
        HostMsg::Ping { seq } => Step::Pong(seq),
        // The host is going away: exit cleanly.
        HostMsg::Shutdown => Step::Shutdown,
        // No RunCommand effect is issued, so no EffectResult is expected.
        HostMsg::EffectResult { .. } => Step::Ignore,
    }
}

/// Project `Local` into the declarative widget tree the host reconciles into
/// GTK. A vertical `Box` holding the formatted time (`ts-clock`, the host's
/// monospace/tabular clock class) above a `Button` that opens the power menu.
fn view(state: &Local) -> Node {
    Node::Box {
        id: Some(ROOT_ID.to_owned()),
        dir: Dir::Vertical,
        spacing: 4,
        scroll: false,
        classes: Vec::new(),
        children: vec![
            Node::Label {
                id: Some(TIME_ID.to_owned()),
                text: state.iso.clone(),
                classes: vec!["ts-clock".to_owned()],
            },
            Node::Button {
                id: CLOCK_BTN.to_owned(),
                classes: Vec::new(),
                child: Box::new(Node::Label {
                    id: None,
                    text: "Power menu".to_owned(),
                    classes: Vec::new(),
                }),
            },
        ],
    }
}

/// The plugin's self-description. Subscribes to `Clock`, mounts `SidebarTop`,
/// requests the `OpenPage` capability. `Manifest::new` stamps `proto =
/// PROTO_VERSION`, which the host exact-matches at the handshake.
fn manifest() -> Manifest {
    let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop);
    m.subscribes = vec![StateKey::Clock];
    m.capabilities = vec![Capability::OpenPage];
    m
}

// ── Transport ────────────────────────────────────────────────────────────────

/// The host socket under `$XDG_RUNTIME_DIR` (same construction as the host), or
/// `None` if that env var is unset — then there is nothing to dial.
fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut path = PathBuf::from(base);
    path.push("trollshell");
    path.push("plugin.sock");
    Some(path)
}

/// Drive one connected session: handshake, initial render, then the reducer
/// loop until the peer disconnects (socket EOF surfaces as [`ProtoError::Io`]).
async fn run_session(stream: UnixStream) -> Result<(), ProtoError> {
    let (mut rd, mut wr) = stream.into_split();

    // Handshake: `Register` MUST be the first frame (else the host drops us).
    write_frame(
        &mut wr,
        &PluginMsg::Register {
            manifest: manifest(),
        },
    )
    .await?;

    // A greeting through the host log, tagged by our plugin id — demonstrates
    // the `Log` frame path.
    write_frame(
        &mut wr,
        &PluginMsg::Log {
            level: LogLevel::Info,
            msg: "clock-demo connected".to_owned(),
        },
    )
    .await?;

    // Seed with a placeholder and render once so the slot mounts immediately,
    // even before the first state snapshot lands.
    let mut state = Local {
        iso: "—".to_owned(),
        unix: 0,
    };
    write_frame(
        &mut wr,
        &PluginMsg::Render {
            tree: view(&state),
            effects: Vec::new(),
        },
    )
    .await?;

    // Full-duplex reducer loop until the host disconnects.
    loop {
        let msg = read_frame::<HostMsg, _>(&mut rd).await?;
        match reduce(&mut state, msg) {
            Step::Render(effects) => {
                write_frame(
                    &mut wr,
                    &PluginMsg::Render {
                        tree: view(&state),
                        effects,
                    },
                )
                .await?;
            }
            Step::Pong(seq) => write_frame(&mut wr, &PluginMsg::Pong { seq }).await?,
            Step::Shutdown => return Ok(()),
            Step::Ignore => {}
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Some(path) = socket_path() else {
        eprintln!("[clock-demo] XDG_RUNTIME_DIR unset; no host socket to dial");
        std::process::exit(1);
    };

    // Dial with bounded exponential backoff: a host that isn't up yet (both
    // start under the same session target) or a host restart is a transient
    // condition we ride out here rather than exiting into systemd's start-limit.
    let mut backoff = BACKOFF_BASE;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                let started = Instant::now();
                if let Err(e) = run_session(stream).await {
                    eprintln!("[clock-demo] session ended: {e}");
                }
                // Reset backoff only after a stable session, so a flapping host
                // (accept-then-drop) can't defeat the backoff.
                if started.elapsed() >= BACKOFF_CAP {
                    backoff = BACKOFF_BASE;
                }
            }
            Err(e) => {
                eprintln!(
                    "[clock-demo] connect {}: {e}; retrying in {backoff:?}",
                    path.display()
                );
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = backoff.saturating_mul(2).min(BACKOFF_CAP);
    }
}

#[cfg(test)]
mod tests {
    use super::{CLOCK_BTN, Local, Step, manifest, reduce, view};
    use hytte_plugin_proto::{
        ClockState, Dir, Effect, EventKind, HostMsg, Node, Page, PluginMsg, StateSnapshot, decode,
        encode,
    };

    fn clock_snapshot(iso: &str, unix: i64) -> HostMsg {
        HostMsg::StateSnapshot {
            snapshot: StateSnapshot {
                clock: Some(ClockState {
                    iso: iso.to_owned(),
                    unix,
                }),
            },
        }
    }

    /// The core signal: a `StateSnapshot{clock}` updates the plugin's state and
    /// `view` renders the exact expected widget tree the host will reconcile.
    #[test]
    fn state_snapshot_updates_state_and_renders_expected_tree() {
        let mut state = Local::default();
        let step = reduce(
            &mut state,
            clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740),
        );
        assert!(matches!(step, Step::Render(ref fx) if fx.is_empty()));
        assert_eq!(state.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(state.unix, 1_752_241_740);

        let expected = Node::Box {
            id: Some("clock-demo-root".to_owned()),
            dir: Dir::Vertical,
            spacing: 4,
            scroll: false,
            classes: vec![],
            children: vec![
                Node::Label {
                    id: Some("clock-demo-time".to_owned()),
                    text: "2026-07-11T15:49:00+02:00".to_owned(),
                    classes: vec!["ts-clock".to_owned()],
                },
                Node::Button {
                    id: "clock-demo-btn".to_owned(),
                    classes: vec![],
                    child: Box::new(Node::Label {
                        id: None,
                        text: "Power menu".to_owned(),
                        classes: vec![],
                    }),
                },
            ],
        };
        assert_eq!(view(&state), expected);
    }

    /// A snapshot whose `clock` is `None` (startup window) leaves state untouched.
    #[test]
    fn snapshot_without_clock_is_ignored() {
        let mut state = Local::default();
        let step = reduce(
            &mut state,
            HostMsg::StateSnapshot {
                snapshot: StateSnapshot::default(),
            },
        );
        assert!(matches!(step, Step::Ignore));
        assert_eq!(state, Local::default());
    }

    /// Clicking the clock button emits exactly one `OpenPage(PowerMenu)` effect.
    #[test]
    fn button_click_emits_open_power_menu_effect() {
        let mut state = Local::default();
        let step = reduce(
            &mut state,
            HostMsg::Event {
                node: CLOCK_BTN.to_owned(),
                kind: EventKind::Click,
            },
        );
        match step {
            Step::Render(effects) => {
                assert_eq!(effects, vec![Effect::OpenPage(Page::PowerMenu)]);
            }
            other => panic!("expected Render with OpenPage, got {other:?}"),
        }
    }

    /// A click on a node we don't own is ignored (no spurious effect).
    #[test]
    fn click_on_unknown_node_is_ignored() {
        let mut state = Local::default();
        let step = reduce(
            &mut state,
            HostMsg::Event {
                node: "not-ours".to_owned(),
                kind: EventKind::Click,
            },
        );
        assert!(matches!(step, Step::Ignore));
    }

    /// A Ping is answered with a Pong echoing its seq.
    #[test]
    fn ping_is_answered_with_pong_seq() {
        let mut state = Local::default();
        assert!(matches!(
            reduce(&mut state, HostMsg::Ping { seq: 42 }),
            Step::Pong(42)
        ));
    }

    /// Shutdown requests a clean exit.
    #[test]
    fn shutdown_requests_exit() {
        let mut state = Local::default();
        assert!(matches!(
            reduce(&mut state, HostMsg::Shutdown),
            Step::Shutdown
        ));
    }

    /// The frames the plugin sends (Register manifest + Render tree) are valid
    /// on the wire — they round-trip through the proto codec.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut state = Local::default();
        reduce(&mut state, clock_snapshot("2026-07-11T15:49:00+02:00", 1));
        let render = PluginMsg::Render {
            tree: view(&state),
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
