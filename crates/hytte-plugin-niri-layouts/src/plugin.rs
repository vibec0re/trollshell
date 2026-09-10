//! The widget hat: a three-button bar chip.
//!
//! The chip is a `Row` of three `Button`s, each wrapping a symbolic `Icon` that
//! carries the layout's legend as its tooltip (`Node::Button` has no tooltip
//! field; `Node::Icon` does, and GTK resolves a hover against the deepest widget
//! under the pointer, so the icon's legend is what the button shows).
//!
//! # Why the work happens on the command lane
//!
//! [`Plugin::update`] is synchronous and runs on the session loop, so it must
//! not block on four unix-socket round trips. A click therefore only *queues* a
//! [`Cmd::Apply`]; the worker [`NiriLayouts::sources`] spawns drains that lane
//! and does the blocking niri IPC on `spawn_blocking`, sending a
//! [`Msg::Failed`] back when niri refuses. `update` turns that message into the
//! one [`Effect::Notify`] toast — which is why the plugin declares
//! [`Capability::Notify`] and nothing else. It needs no `Capability::Niri`: the
//! wire's `Effect::Niri` only knows `FocusWorkspace` / `FocusWindow`, and this
//! plugin talks to the compositor in its own process rather than through the
//! shell.
//!
//! The view is a **constant** — it projects no model state — which is fine: the
//! runtime dedups identical trees but force-sends a frame that carries effects,
//! so the failure toast still lands.

use crate::layout::Layout;
use crate::niri::{self, SocketTransport, Transport};
use hytte_plugin::proto::{Capability, Effect, EventKind, Manifest, Mount, Node};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, nodes};

/// The mount-slot key, the audit-log subject, and the stderr log prefix.
pub(crate) const PLUGIN_ID: &str = "niri-layouts";

/// The chip's root node id.
const ROOT_ID: &str = "niri-layouts";

/// Every button id is this plus the layout's own token, so
/// [`layout_for_node`] is a `strip_prefix` and the two can never disagree.
const BUTTON_PREFIX: &str = "niri-layouts-";

/// Inter-button gap, in pixels. Three 16 px glyphs sit too close at `0`.
const BUTTON_SPACING: u16 = 2;

/// One click's worth of work, handed to the worker task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    Apply(Layout),
}

/// What the worker sends back. Only failures: a successful apply is visible on
/// screen, so a toast for it would be noise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Msg {
    Failed(String),
}

pub(crate) struct NiriLayouts {
    cmds: CmdSender<Cmd>,
}

/// The button id for `layout`.
fn button_id(layout: Layout) -> String {
    format!("{BUTTON_PREFIX}{}", layout.id())
}

/// The layout a click on `node` asks for, or `None` for any other node. Pure, so
/// the id → layout mapping is testable without a session.
fn layout_for_node(node: &str) -> Option<Layout> {
    node.strip_prefix(BUTTON_PREFIX).and_then(Layout::from_id)
}

/// One button: an id'd [`Node::Button`] wrapping a tooltipped symbolic icon.
///
/// `flat` is the stock libadwaita token a bar chip's inline buttons wear; a
/// plugin cannot ship CSS of its own, and the host already wraps a bar mount in
/// its own `.ts-plugin-chip`.
fn layout_button(layout: Layout) -> Node {
    Node::Button {
        id: button_id(layout),
        classes: vec!["flat".to_owned()],
        child: Box::new(Node::Icon {
            id: None,
            name: layout.icon().to_owned(),
            classes: Vec::new(),
            tooltip: Some(layout.tooltip().to_owned()),
        }),
    }
}

/// The chip. **This is the whole of #1019 question 2's answer** — three inline
/// glyph buttons. Should it become one chip that opens a panel with the three,
/// this function is the only thing that changes: `update` keys off the button
/// ids, which a panel would reuse verbatim.
fn chip() -> Node {
    nodes::row(Layout::ALL.into_iter().map(layout_button).collect())
        .id(ROOT_ID)
        .spacing(BUTTON_SPACING)
        .tooltip("Column layouts for the focused niri workspace")
        .build()
}

/// Run one queued command against `transport`, reporting only what the human
/// needs to see.
///
/// Split out of the worker task so the click → apply → toast path is testable
/// against a fake niri, with no runtime and no socket.
pub(crate) fn apply_and_report(transport: &mut impl Transport, layout: Layout) -> Option<Msg> {
    match niri::apply(transport, layout) {
        Ok(0) => {
            // The one debug line the no-op case gets. stderr, which systemd
            // routes to the journal for a plugin unit and to the terminal for
            // the CLI hat.
            eprintln!(
                "[{PLUGIN_ID}] {}: no tiled columns on the focused workspace, nothing to do",
                layout.id()
            );
            None
        }
        Ok(columns) => {
            eprintln!(
                "[{PLUGIN_ID}] {}: set {columns} column width(s)",
                layout.id()
            );
            None
        }
        Err(error) => Some(Msg::Failed(error)),
    }
}

/// The toast a [`Msg::Failed`] becomes.
fn failure_toast(error: String) -> Effect {
    Effect::Notify {
        summary: "niri layout failed".to_owned(),
        body: error,
    }
}

impl Plugin for NiriLayouts {
    type Msg = Msg;
    type Cmd = Cmd;

    /// Mounts [`Mount::BarRight`] as a chip. The only capability it wants is
    /// [`Capability::Notify`], for the failure toast; it subscribes no host
    /// state at all (the SDK adds the accent subscription on its behalf).
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarRight);
        m.capabilities = vec![Capability::Notify];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self { cmds }
    }

    fn sources(mut cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = hytte_plugin::tokio::sync::mpsc::unbounded_channel();
        hytte_plugin::tokio::spawn(async move {
            while let Some(Cmd::Apply(layout)) = cmds.recv().await {
                // `Socket::send` is blocking std I/O, so it goes to the blocking
                // pool rather than stalling the SDK's current-thread runtime
                // (which is also servicing the host socket).
                let outcome = hytte_plugin::tokio::task::spawn_blocking(move || {
                    apply_and_report(&mut SocketTransport, layout)
                })
                .await;
                let report = match outcome {
                    Ok(report) => report,
                    Err(e) => Some(Msg::Failed(format!("layout worker died: {e}"))),
                };
                if let Some(msg) = report {
                    // Err only once the session is tearing down.
                    let _ = msg_tx.send(msg);
                }
            }
        });
        Some(Box::pin(
            hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream::new(msg_rx),
        ))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::Event { node, kind } => {
                if matches!(kind, EventKind::Click)
                    && let Some(layout) = layout_for_node(&node)
                {
                    // Err only once the session is tearing down, in which case
                    // the worker is gone and there is nothing to apply to.
                    let _ = self.cmds.send(Cmd::Apply(layout));
                }
                Vec::new()
            }
            Input::App(Msg::Failed(error)) => vec![failure_toast(error)],
            // No host state is subscribed and no host effect is brokered, so
            // every other push is a no-op.
            Input::Snapshot(_)
            | Input::EffectResult { .. }
            | Input::SlotVisible(_)
            | Input::AudioSpectrum(_)
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => Vec::new(),
        }
    }

    fn view(&self) -> View {
        chip().into()
    }
}

#[cfg(test)]
mod tests {
    use super::{Cmd, Msg, NiriLayouts, apply_and_report, button_id, chip, layout_for_node};
    use crate::layout::Layout;
    use crate::niri::fake::Fake;
    use hytte_plugin::proto::{Capability, Effect, EventKind, Mount, Node};
    use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};

    /// The chip's three buttons, in render order, as `(id, icon, tooltip)`.
    fn buttons() -> Vec<(String, String, Option<String>)> {
        let Node::Row { children, .. } = chip() else {
            panic!("the chip is a Row");
        };
        children
            .into_iter()
            .map(|child| {
                let Node::Button { id, child, .. } = child else {
                    panic!("every chip child is a Button");
                };
                let Node::Icon { name, tooltip, .. } = *child else {
                    panic!("every button wraps an Icon");
                };
                (id, name, tooltip)
            })
            .collect()
    }

    fn drain(cmds: &mut CmdReceiver<Cmd>) -> Vec<Cmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmds.try_recv() {
            out.push(cmd);
        }
        out
    }

    #[test]
    fn the_chip_is_three_icon_buttons_in_layout_order() {
        let buttons = buttons();

        assert_eq!(buttons.len(), 3, "three inline glyph buttons (#1019 Q2)");
        for (button, layout) in buttons.iter().zip(Layout::ALL) {
            assert_eq!(button.0, button_id(layout), "id");
            assert_eq!(button.1, layout.icon(), "icon");
            assert_eq!(
                button.2.as_deref(),
                Some(layout.tooltip()),
                "the glyph's only words"
            );
        }
    }

    #[test]
    fn every_button_id_maps_back_to_its_layout_and_nothing_else_does() {
        for layout in Layout::ALL {
            assert_eq!(layout_for_node(&button_id(layout)), Some(layout));
        }
        assert_eq!(layout_for_node("niri-layouts"), None, "the root is inert");
        assert_eq!(layout_for_node("niri-layouts-fibonacci"), None);
        assert_eq!(layout_for_node("equal"), None, "the prefix is required");
    }

    #[test]
    fn the_manifest_mounts_bar_right_and_asks_only_to_notify() {
        let m = NiriLayouts::manifest();

        assert_eq!(m.id, super::PLUGIN_ID);
        assert_eq!(m.mount, Mount::BarRight);
        assert_eq!(
            m.capabilities,
            vec![Capability::Notify],
            "it reaches niri in its own process, so no Capability::Niri"
        );
        assert!(m.subscribes.is_empty(), "no host state drives the chip");
    }

    #[test]
    fn a_click_queues_exactly_that_layout_and_emits_no_effect() {
        let (tx, mut rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        for layout in Layout::ALL {
            let effects = plugin.update(Input::Event {
                node: button_id(layout),
                kind: EventKind::Click,
            });

            assert!(effects.is_empty(), "the work is queued, not effected");
            assert_eq!(drain(&mut rx), vec![Cmd::Apply(layout)]);
        }
    }

    #[test]
    fn a_click_on_an_unknown_node_queues_nothing() {
        let (tx, mut rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        plugin.update(Input::Event {
            node: "niri-layouts".to_owned(),
            kind: EventKind::Click,
        });

        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_non_click_event_on_a_button_queues_nothing() {
        let (tx, mut rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        plugin.update(Input::Event {
            node: button_id(Layout::Golden),
            kind: EventKind::Scroll { dx: 0.0, dy: 1.0 },
        });

        assert!(drain(&mut rx).is_empty(), "only a click applies a layout");
    }

    #[test]
    fn a_worker_failure_becomes_one_notify_toast_carrying_niris_text() {
        let (tx, _rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        let effects = plugin.update(Input::App(Msg::Failed(
            "unknown action SetWindowWidth".to_owned(),
        )));

        assert_eq!(
            effects,
            vec![Effect::Notify {
                summary: "niri layout failed".to_owned(),
                body: "unknown action SetWindowWidth".to_owned(),
            }]
        );
    }

    /// The whole click → apply → toast chain, minus the tokio hop the worker
    /// task adds: a click queues the command, the worker *body* runs it against
    /// a niri that refuses, and the message it returns becomes the toast.
    #[test]
    fn click_to_apply_to_toast_carries_niris_refusal_end_to_end() {
        let (tx, mut rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);
        let mut niri = Fake::two_columns();
        niri.action_error = Some("no such window".to_owned());

        // 1. The click queues the layout…
        plugin.update(Input::Event {
            node: button_id(Layout::Golden),
            kind: EventKind::Click,
        });
        let queued = drain(&mut rx);
        assert_eq!(queued, vec![Cmd::Apply(Layout::Golden)]);

        // 2. …the worker body applies it and reports the failure…
        let Cmd::Apply(layout) = queued[0];
        let report = apply_and_report(&mut niri, layout);
        assert_eq!(report, Some(Msg::Failed("no such window".to_owned())));

        // 3. …and folding that back in yields exactly one toast with niri's text.
        let effects = plugin.update(Input::App(report.expect("reported a failure")));
        assert_eq!(
            effects,
            vec![Effect::Notify {
                summary: "niri layout failed".to_owned(),
                body: "no such window".to_owned(),
            }]
        );
    }

    #[test]
    fn a_successful_apply_reports_nothing_to_the_session() {
        let mut niri = Fake::two_columns();

        assert_eq!(apply_and_report(&mut niri, Layout::Split), None);
        assert_eq!(
            niri.widths(),
            vec![(10, 0.5), (20, 0.5)],
            "it really did apply, it just has nothing to say about it"
        );
    }

    #[test]
    fn an_empty_workspace_reports_nothing_either() {
        let mut niri = Fake::with(Vec::new());

        assert_eq!(
            apply_and_report(&mut niri, Layout::Equal),
            None,
            "a no-op gets a log line, not a toast"
        );
    }
}
