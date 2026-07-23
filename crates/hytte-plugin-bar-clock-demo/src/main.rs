//! `hytte-plugin-bar-clock-demo` — the reference out-of-process **bar-chip**
//! plugin for trollshell's "frontend B" plugin architecture (issue #349; on the
//! #266 wire protocol, the #272 host transport, and the #275 `hytte-plugin`
//! runtime).
//!
//! It is the end-to-end proof for the #349 **chip + panel** slices: a plugin
//! that mounts [`Mount::BarCenter`] renders its `view()` chip tree as a bar
//! chip (where v1 dropped it), and its `HH:MM` chip is a clickable
//! [`Node::Button`] that opens the drawer panel carried on the same
//! [`View`](hytte_plugin::View) via [`Effect::OpenPage(Page::PluginSelf)`]. It links
//! **no GTK** (only [`hytte_plugin`]) and drives a real bar widget over the Unix
//! socket — the bar-side twin of `hytte-plugin-clock-demo` (which mounts a
//! sidebar card). A clock belongs in a bar, so it renders a compact `HH:MM` chip
//! driven by the host's `Clock` subscription, and its panel shows the full
//! projected `ClockState` (the RFC3339 timestamp + unix seconds).
//!
//! # Shape — The Elm Architecture, and nothing else
//!
//! Everything below is the pure TEA core: a model ([`BarClock`]) plus
//! [`update`](Plugin::update) / [`view`](Plugin::view). All transport — dialing
//! `$XDG_RUNTIME_DIR/trollshell/plugin.sock` with bounded backoff, the
//! `Register` handshake, liveness, render dedup, reconnection — lives in the
//! [`hytte_plugin`] runtime behind the one-line `main`. systemd's
//! `Restart=on-failure` is the outer supervisor for genuine process failures.
//!
//! `update`, `view`, and the `HH:MM` projection are unit-tested below — that is
//! the demo's main correctness signal, since the live host isn't reachable here;
//! the session loop itself is covered by `hytte-plugin`'s own tests.

use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-region ownership key and audit-log subject.
const PLUGIN_ID: &str = "bar-clock-demo";
/// Node ids for stable reconciliation. The chip is a clickable button (#349 PR2)
/// wrapping the time label; the panel carries its own label ids.
const ROOT_ID: &str = "bar-clock-demo-root";
const TIME_ID: &str = "bar-clock-demo-time";
/// The clickable chip button — its `Click` opens the plugin's own panel.
const BTN_ID: &str = "bar-clock-demo-btn";
/// Panel label ids: the full ISO timestamp and the unix seconds.
const PANEL_ISO_ID: &str = "bar-clock-demo-panel-iso";
const PANEL_UNIX_ID: &str = "bar-clock-demo-panel-unix";

/// The plugin's entire state. Lives here — the host never stores or round-trips
/// it; it is rebuilt on every (re)connect and re-derived from the next snapshot.
#[derive(Debug, Default, PartialEq, Eq)]
struct BarClock {
    /// Latest ISO-8601 local timestamp from the host's clock subscription.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState`).
    unix: i64,
}

/// Project an RFC3339 timestamp (`2026-07-11T15:49:00+02:00`) to the compact
/// `HH:MM` a bar chip shows. Panic-free over any host-sent value: a string
/// without a `T`, or one too short after it, degrades to the raw input rather
/// than slicing out of bounds.
fn short_time(iso: &str) -> String {
    match iso.find('T') {
        // `T` + `HH:MM` is 5 chars; `get` returns `None` (→ fall back) if the
        // string is truncated there, so this never panics on a bad boundary.
        Some(t) => iso.get(t + 1..t + 6).unwrap_or(iso).to_owned(),
        None => iso.to_owned(),
    }
}

impl Plugin for BarClock {
    /// Purely host-driven: no timers, no fetches, no self-generated messages.
    type Msg = std::convert::Infallible;

    /// Purely display: it issues no I/O of its own, so it has no commands and
    /// ignores the command lane entirely. `Infallible` = "no command can ever be
    /// constructed".
    type Cmd = std::convert::Infallible;

    /// Subscribes to `Clock`, mounts [`Mount::BarCenter`] — the center bar group.
    /// Requests [`Capability::OpenPage`] (#349 PR2) so its click can open its own
    /// drawer panel via [`Effect::OpenPage(Page::PluginSelf)`]. `Manifest::new`
    /// stamps `proto = PROTO_VERSION`, exact-matched at the handshake.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarCenter);
        m.subscribes = vec![StateKey::Clock];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    /// Placeholder time until the first snapshot lands (the runtime renders this
    /// seed immediately, so the chip mounts right away). The command sender goes
    /// unused — this plugin only reads state.
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            iso: "—".to_owned(),
            unix: 0,
        }
    }

    /// Fold one input into the model. Pure and panic-free over any host-sent
    /// value. Re-rendering is the runtime's problem (identical trees are
    /// deduped), so a snapshot without a clock simply changes nothing.
    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // Subscribed-state snapshot: take the clock. `clock` is optional on
            // the wire (a startup snapshot may arrive before the host's clock
            // pump has published), so tolerate `None`.
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.iso = clock.iso;
                    self.unix = clock.unix;
                }
                Vec::new()
            }
            // A click on the chip button opens the plugin's own drawer panel
            // (#349 PR2). The host resolves `PluginSelf` to *this* plugin's
            // `panel()` by the effect's plugin id — no page name to know.
            Input::Event {
                node,
                kind: EventKind::Click,
            } if node == BTN_ID => vec![Effect::OpenPage(Page::PluginSelf)],
            // Any other interaction, effect result, or sidebar-visibility push
            // (#288) is a no-op that never touches the view.
            Input::Event { .. } | Input::EffectResult { .. } | Input::SlotVisible(_) => Vec::new(),
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
    }

    /// Project the model into the rendered [`View`] (#349). The **chip** —
    /// wrapped by the host in a `.ts-plugin-chip` pill — is a horizontal `Box`
    /// holding a [`Node::Button`] (the click target that opens the panel) whose
    /// child is the compact `HH:MM` time (`ts-clock`, the host's
    /// monospace/tabular clock class). The drawer **panel** is a vertical `Box`
    /// showing the full projected `ClockState` — the RFC3339 timestamp and the
    /// raw unix seconds — a second, independent tree distinct from the compact
    /// chip. Its root carries **no** `.card`/`.ts-plugin-*` class: the drawer
    /// supplies the card chrome, so the panel owns only its inner content.
    fn view(&self) -> View {
        let chip = Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Button {
                id: BTN_ID.to_owned(),
                classes: Vec::new(),
                child: Box::new(Node::Label {
                    id: Some(TIME_ID.to_owned()),
                    text: short_time(&self.iso),
                    classes: vec!["ts-clock".to_owned()],
                }),
            }],
        };
        View::new(chip).panel(Node::Box {
            id: Some("bar-clock-demo-panel".to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Label {
                    id: Some(PANEL_ISO_ID.to_owned()),
                    text: self.iso.clone(),
                    classes: vec!["title-2".to_owned()],
                },
                Node::Label {
                    id: Some(PANEL_UNIX_ID.to_owned()),
                    text: format!("unix: {}", self.unix),
                    classes: vec!["dim-label".to_owned()],
                },
            ],
        })
    }
}

fn main() {
    hytte_plugin::run::<BarClock>()
}

#[cfg(test)]
mod tests {
    use super::{BarClock, short_time};
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
    /// commands, so the lane goes unused.
    fn fresh() -> BarClock {
        BarClock::init(hytte_plugin::cmd_channel().0)
    }

    /// `short_time` projects RFC3339 → `HH:MM`, and degrades gracefully on any
    /// malformed input rather than panicking.
    #[test]
    fn short_time_extracts_hh_mm() {
        assert_eq!(short_time("2026-07-11T15:49:00+02:00"), "15:49");
        assert_eq!(short_time("2026-07-11T00:00:00Z"), "00:00");
        // No 'T' → raw passthrough (the "—" seed and any odd value survive).
        assert_eq!(short_time("—"), "—");
        assert_eq!(short_time("no-time-here"), "no-time-here");
        // Truncated after 'T' → passthrough, never an out-of-bounds slice.
        assert_eq!(short_time("2026-07-11T15"), "2026-07-11T15");
    }

    /// The core signal: a snapshot with a clock updates the model and `view`
    /// renders the exact compact bar chip the host will reconcile.
    #[test]
    fn snapshot_updates_model_and_renders_expected_chip() {
        let mut model = fresh();
        let effects = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));
        assert!(effects.is_empty());
        assert_eq!(model.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(model.unix, 1_752_241_740);

        let expected = Node::Box {
            id: Some("bar-clock-demo-root".to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: vec![],
            children: vec![Node::Button {
                id: "bar-clock-demo-btn".to_owned(),
                classes: vec![],
                child: Box::new(Node::Label {
                    id: Some("bar-clock-demo-time".to_owned()),
                    text: "15:49".to_owned(),
                    classes: vec!["ts-clock".to_owned()],
                }),
            }],
        };
        assert_eq!(model.view().tree, expected);
    }

    /// #349 PR2: a click on the chip button opens the plugin's own panel — the
    /// `update` returns exactly `[Effect::OpenPage(Page::PluginSelf)]`, and
    /// `panel()` projects the full `ClockState` (a tree distinct from the chip).
    #[test]
    fn click_opens_panel_and_panel_renders_full_clock() {
        let mut model = fresh();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));

        // A click on the chip button yields exactly the PluginSelf open effect.
        let effects = model.update(Input::Event {
            node: "bar-clock-demo-btn".to_owned(),
            kind: EventKind::Click,
        });
        assert_eq!(effects, vec![Effect::OpenPage(Page::PluginSelf)]);

        // A click on some other node (or non-click) opens nothing.
        assert!(
            model
                .update(Input::Event {
                    node: "somewhere-else".to_owned(),
                    kind: EventKind::Click,
                })
                .is_empty(),
            "only the chip button opens the panel",
        );

        // The panel projects the full ISO + unix seconds, distinct from the chip.
        let expected_panel = Node::Box {
            id: Some("bar-clock-demo-panel".to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: vec![],
            children: vec![
                Node::Label {
                    id: Some("bar-clock-demo-panel-iso".to_owned()),
                    text: "2026-07-11T15:49:00+02:00".to_owned(),
                    classes: vec!["title-2".to_owned()],
                },
                Node::Label {
                    id: Some("bar-clock-demo-panel-unix".to_owned()),
                    text: "unix: 1752241740".to_owned(),
                    classes: vec!["dim-label".to_owned()],
                },
            ],
        };
        assert_eq!(model.view().panel, Some(expected_panel));
    }

    /// A snapshot whose `clock` is `None` (startup window) changes nothing — the
    /// runtime's tree dedup then sends no frame for it.
    #[test]
    fn snapshot_without_clock_changes_nothing() {
        let mut model = fresh();
        let before = model.view();
        let effects = model.update(Input::Snapshot(StateSnapshot::default()));
        assert!(effects.is_empty());
        assert_eq!(model.view(), before);
    }

    /// The frames built from this plugin's data (Register manifest + Render tree)
    /// are valid on the wire — they round-trip through the proto codec. The
    /// manifest carries `Mount::BarCenter`, the mount this PR makes render.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: BarClock::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut model = fresh();
        let _ = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1));
        // A panel-bearing render (the chip button + the drawer panel, #349)
        // is what the runtime sends after this plugin's first snapshot.
        let view = model.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel,
            effects: vec![],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
