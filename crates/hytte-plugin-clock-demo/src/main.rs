//! `hytte-plugin-clock-demo` — the reference out-of-process widget plugin for
//! trollshell's "frontend B" plugin architecture (issue #35; on the #266 wire
//! protocol, the #272 host transport, and the #275 `hytte-plugin` runtime).
//!
//! It is the **end-to-end proof** that a plugin can live outside the shell,
//! link **no GTK** (only [`hytte_plugin`] — not even tokio directly), and
//! drive a real widget over a Unix socket. It renders a clock into the
//! shell's `SidebarTop` slot and, when its button is clicked, asks the host
//! to open the power menu — exercising the render path, the
//! state-subscription path, and the event→effect round-trip in one demo.
//!
//! # Shape — The Elm Architecture, and nothing else
//!
//! Everything below is the pure TEA core: a model ([`ClockDemo`]) plus
//! [`update`](Plugin::update) / [`view`](Plugin::view). All transport —
//! dialing `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with bounded backoff,
//! the `Register` handshake, liveness, render dedup, reconnection — lives in
//! the [`hytte_plugin`] runtime behind the one-line `main`. systemd's
//! `Restart=on-failure` is the outer supervisor for genuine process failures.
//!
//! Both `update` and `view` are unit-tested (see the `tests` module) — that
//! is the demo's main correctness signal, since the live host isn't reachable
//! here; the session loop itself is covered by `hytte-plugin`'s own tests.

use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-slot ownership key and audit-log subject.
const PLUGIN_ID: &str = "clock-demo";
/// Node ids. `CLOCK_BTN` is the click event target (a `Button` requires an id).
const ROOT_ID: &str = "clock-demo-root";
const TIME_ID: &str = "clock-demo-time";
const CLOCK_BTN: &str = "clock-demo-btn";

/// The plugin's entire state. Lives here — the host never stores or
/// round-trips it; it is rebuilt on every (re)connect and re-derived from the
/// next snapshot.
#[derive(Debug, Default, PartialEq, Eq)]
struct ClockDemo {
    /// Latest ISO-8601 local timestamp from the host's clock subscription.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState`).
    unix: i64,
}

impl Plugin for ClockDemo {
    /// Purely host-driven: no timers, no fetches, no self-generated messages.
    type Msg = std::convert::Infallible;

    /// Purely display: it issues no I/O of its own, so it has no commands and
    /// ignores the command lane entirely (see `hytte_plugin`'s *Commands*
    /// docs). `Infallible` = "no command can ever be constructed".
    type Cmd = std::convert::Infallible;

    /// Subscribes to `Clock`, mounts `SidebarTop`, requests the `OpenPage`
    /// capability. `Manifest::new` stamps `proto = PROTO_VERSION`, which the
    /// host exact-matches at the handshake.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarTop);
        m.subscribes = vec![StateKey::Clock];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    /// Placeholder time until the first snapshot lands (the runtime renders
    /// this seed immediately, so the slot mounts right away). The command
    /// sender goes unused — this plugin only reads state and asks the host to
    /// open a page.
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            iso: "—".to_owned(),
            unix: 0,
        }
    }

    /// Fold one input into the model. Pure and panic-free over any host-sent
    /// value — this is the testable heart of the plugin. Re-rendering is the
    /// runtime's problem (identical trees are deduped), so a snapshot without
    /// a clock simply changes nothing.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // Subscribed-state snapshot: take the clock. `clock` is optional
            // on the wire (a startup snapshot may arrive before the host's
            // clock pump has published), so tolerate `None`.
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.iso = clock.iso;
                    self.unix = clock.unix;
                }
                Vec::new()
            }
            // Our only interactive node is the button; a click asks the host
            // to open the power menu. The effect rides exactly one render
            // frame, so a clock tick never re-fires it.
            Input::Event { node, kind } => {
                if node == CLOCK_BTN && matches!(kind, EventKind::Click) {
                    vec![Effect::OpenPage(Page::PowerMenu)]
                } else {
                    Vec::new()
                }
            }
            // Two no-ops: no RunCommand is issued (so no EffectResult is
            // expected), and the clock demo has no pollers to park, so it just
            // ignores the sidebar-visibility push (#288) — neither touches the view.
            Input::EffectResult { .. } | Input::SlotVisible(_) | Input::AudioSpectrum(_) => {
                Vec::new()
            }
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
    }

    /// Project the model into the declarative widget tree the host reconciles
    /// into GTK. A vertical `Box` holding the formatted time (`ts-clock`, the
    /// host's monospace/tabular clock class) above a `Button` that opens the
    /// power menu.
    fn view(&self) -> View {
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 4,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Label {
                    id: Some(TIME_ID.to_owned()),
                    text: self.iso.clone(),
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
        .into()
    }
}

fn main() {
    hytte_plugin::run::<ClockDemo>()
}

#[cfg(test)]
mod tests {
    use super::{CLOCK_BTN, ClockDemo};
    use hytte_plugin::proto::{
        ClockState, Dir, Effect, EventKind, Node, Page, PluginMsg, StateSnapshot, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    fn clock_snapshot(iso: &str, unix: i64) -> Input<std::convert::Infallible> {
        Input::Snapshot(StateSnapshot {
            clock: Some(ClockState {
                iso: iso.to_owned(),
                unix,
            }),
        })
    }

    /// A fresh model with a throwaway command sender — the demo issues no
    /// commands, so the lane goes unused (`cmd_channel` lets the test build a
    /// sender without a direct tokio dependency).
    fn fresh() -> ClockDemo {
        ClockDemo::init(hytte_plugin::cmd_channel().0)
    }

    /// The core signal: a snapshot with a clock updates the model and `view`
    /// renders the exact expected widget tree the host will reconcile.
    #[test]
    fn snapshot_updates_model_and_renders_expected_tree() {
        let mut model = fresh();
        let effects = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));
        assert!(effects.is_empty());
        assert_eq!(model.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(model.unix, 1_752_241_740);

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
        assert_eq!(model.view().tree, expected);
    }

    /// A snapshot whose `clock` is `None` (startup window) changes nothing —
    /// the runtime's tree dedup then sends no frame for it.
    #[test]
    fn snapshot_without_clock_changes_nothing() {
        let mut model = fresh();
        let before = model.view();
        let effects = model.update(Input::Snapshot(StateSnapshot::default()));
        assert!(effects.is_empty());
        assert_eq!(model.view(), before);
    }

    /// Clicking the clock button emits exactly one `OpenPage(PowerMenu)` effect.
    #[test]
    fn button_click_emits_open_power_menu_effect() {
        let mut model = fresh();
        let effects = model.update(Input::Event {
            node: CLOCK_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert_eq!(effects, vec![Effect::OpenPage(Page::PowerMenu)]);
    }

    /// A click on a node we don't own is ignored (no spurious effect).
    #[test]
    fn click_on_unknown_node_is_ignored() {
        let mut model = fresh();
        let effects = model.update(Input::Event {
            node: "not-ours".to_owned(),
            kind: EventKind::Click,
        });
        assert!(effects.is_empty());
    }

    /// The frames built from this plugin's data (Register manifest + Render
    /// tree) are valid on the wire — they round-trip through the proto codec.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: ClockDemo::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut model = fresh();
        let _ = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1));
        let view = model.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel,
            effects: vec![Effect::OpenPage(Page::PowerMenu)],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
