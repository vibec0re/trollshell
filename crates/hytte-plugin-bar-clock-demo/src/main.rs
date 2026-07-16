//! `hytte-plugin-bar-clock-demo` — the reference out-of-process **bar-chip**
//! plugin for trollshell's "frontend B" plugin architecture (issue #349; on the
//! #266 wire protocol, the #272 host transport, and the #275 `hytte-plugin`
//! runtime).
//!
//! It is the end-to-end proof for the #349 **chip** slice: a plugin that mounts
//! [`Mount::BarCenter`] now renders its `view()` tree as a bar chip, where v1
//! dropped it. It links **no GTK** (only [`hytte_plugin`]) and drives a real bar
//! widget over the Unix socket — the bar-side twin of `hytte-plugin-clock-demo`
//! (which mounts a sidebar card). A clock belongs in a bar, so it renders a
//! compact `HH:MM` chip driven by the host's `Clock` subscription.
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

use hytte_plugin::proto::{Dir, Effect, Manifest, Mount, Node, StateKey};
use hytte_plugin::{CmdSender, Input, Plugin};

/// Stable plugin id — the host's mount-region ownership key and audit-log subject.
const PLUGIN_ID: &str = "bar-clock-demo";
/// Node ids. The chip is pure display (no interactive node), so only the root +
/// time label carry ids for stable reconciliation.
const ROOT_ID: &str = "bar-clock-demo-root";
const TIME_ID: &str = "bar-clock-demo-time";

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
    /// It requests **no** capabilities (a pure status chip: no effects, nothing
    /// to open). `Manifest::new` stamps `proto = PROTO_VERSION`, exact-matched at
    /// the handshake.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarCenter);
        m.subscribes = vec![StateKey::Clock];
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
            // A pure status chip: no interactive nodes, no commands, no pollers
            // to park — so events, effect results, and the sidebar-visibility
            // push (#288) are all no-ops that never touch the view.
            Input::Event { .. } | Input::EffectResult { .. } | Input::SlotVisible(_) => Vec::new(),
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
    }

    /// Project the model into the declarative widget tree the host reconciles
    /// into GTK — and wraps in a `.ts-plugin-chip` pill (#349). A horizontal
    /// `Box` holding the compact `HH:MM` time (`ts-clock`, the host's
    /// monospace/tabular clock class).
    fn view(&self) -> Node {
        Node::Box {
            id: Some(ROOT_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Label {
                id: Some(TIME_ID.to_owned()),
                text: short_time(&self.iso),
                classes: vec!["ts-clock".to_owned()],
            }],
        }
    }
}

fn main() {
    hytte_plugin::run::<BarClock>()
}

#[cfg(test)]
mod tests {
    use super::{BarClock, short_time};
    use hytte_plugin::proto::{ClockState, Dir, Node, PluginMsg, StateSnapshot, decode, encode};
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
            children: vec![Node::Label {
                id: Some("bar-clock-demo-time".to_owned()),
                text: "15:49".to_owned(),
                classes: vec!["ts-clock".to_owned()],
            }],
        };
        assert_eq!(model.view(), expected);
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
        let render = PluginMsg::Render {
            tree: model.view(),
            effects: vec![],
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }
}
