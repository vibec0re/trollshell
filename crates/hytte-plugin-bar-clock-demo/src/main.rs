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
//!
//! # …and since #884, the **bar-side** showcase for the preem display seam
//!
//! The chip's `HH:MM` is a [`hytte_plugin::display::SevenSeg`] readout rather
//! than a `Node::Label`, which makes this demo the bar-mounted half of the #884
//! acceptance pair (`hytte-plugin-preem-demo` is the sidebar-card half). Same
//! one code path, two hosts: against a shell that advertises the preem
//! vocabulary in `HostMsg::Hello` the chip goes out as a typed `Node::Preem`
//! the shell draws; against one that doesn't it CPU-rasterises to the
//! `Node::Pixels` a hand-written `preem::seven_seg(…).into_node(…)` would have
//! produced — byte for byte, which the tests below pin.
//!
//! Nothing in `view` branches on which host is on the other end, and this chip
//! has no `advance` to call at all: a seven-segment readout is pure, so its
//! whole state is the string handed to `node`. That is the cheapest possible
//! migration shape, and it is the one most of the remaining bundled plugins
//! have.
//!
//! The drawer panel stays plain GTK labels: an RFC3339 timestamp and a raw unix
//! count are text, not a retro readout, and the contrast is the point — the
//! seam is opt-in per widget, not a mode the plugin enters.

use hytte_plugin::display::{SevenSeg, StyleName};
use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-region ownership key and audit-log subject.
const PLUGIN_ID: &str = "bar-clock-demo";
/// Node ids for stable reconciliation. The chip is a clickable button (#349 PR2)
/// wrapping the time readout; the panel carries its own label ids.
///
/// [`TIME_ID`] keys the host reconciler onto the *same* preem renderer instance
/// across renders (#882's `preem_id` rule), which is what lets the shell own the
/// widget's continuity in state mode and swap the texture in place in raster
/// mode.
const ROOT_ID: &str = "bar-clock-demo-root";
const TIME_ID: &str = "bar-clock-demo-time";
/// The clickable chip button — its `Click` opens the plugin's own panel.
const BTN_ID: &str = "bar-clock-demo-btn";
/// Panel label ids: the full ISO timestamp and the unix seconds.
const PANEL_ISO_ID: &str = "bar-clock-demo-panel-iso";
const PANEL_UNIX_ID: &str = "bar-clock-demo-panel-unix";

/// The all-dash face the readout wears before the first snapshot lands, and
/// whenever the host's timestamp doesn't project to an `HH:MM` — see
/// [`clock_face`].
const NO_CLOCK: &str = "--:--";

/// The plugin's entire state. Lives here — the host never stores or round-trips
/// it; it is rebuilt on every (re)connect and re-derived from the next snapshot.
#[derive(Debug, PartialEq, Eq)]
struct BarClock {
    /// Latest ISO-8601 local timestamp from the host's clock subscription.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState`).
    unix: i64,
    /// The chip's seven-segment readout (#884). Config only — a seven-segment
    /// strip is pure, so this carries no animation state and needs no
    /// `advance`; the text is handed to `node` at render time.
    seg: SevenSeg,
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

/// [`short_time`] narrowed to what the seven-segment chip can actually draw:
/// anything that isn't a literal `HH:MM` falls back to [`NO_CLOCK`].
///
/// The narrowing is new with #884 and it is the one thing the widget swap
/// genuinely changed. As a `Node::Label` a malformed timestamp was merely ugly;
/// a seven-segment strip lays its whole message out on one line at 40 px a
/// character, so a passthrough of the raw ISO string would be a ~1000 px chip
/// in the bar. It also covers the pre-snapshot seed (`"—"`, which has no glyph
/// on a seven-segment drum at all) with the all-dash face a real readout shows
/// when it has no reading.
fn clock_face(iso: &str) -> String {
    let face = short_time(iso);
    let b = face.as_bytes();
    let is_hhmm = b.len() == 5
        && b[2] == b':'
        && b[..2].iter().all(u8::is_ascii_digit)
        && b[3..].iter().all(u8::is_ascii_digit);
    if is_hhmm { face } else { NO_CLOCK.to_owned() }
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
            // VFD: the same skin the timer's bar readout wears, so the two
            // seven-segment chips in the bar match.
            seg: SevenSeg::new(StyleName::Vfd),
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
            Input::Event { .. }
            | Input::EffectResult { .. }
            | Input::SlotVisible(_)
            | Input::AudioSpectrum(_)
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => Vec::new(),
            // `Msg = Infallible`: there are no app messages to receive.
            Input::App(never) => match never {},
        }
    }

    /// Project the model into the rendered [`View`] (#349). The **chip** —
    /// wrapped by the host in a `.ts-plugin-chip` pill — is a horizontal `Box`
    /// holding a [`Node::Button`] (the click target that opens the panel) whose
    /// child is the compact `HH:MM` seven-segment readout. The drawer **panel**
    /// is a vertical `Box` showing the full projected `ClockState` — the RFC3339
    /// timestamp and the raw unix seconds — a second, independent tree distinct
    /// from the compact chip. Its root carries **no** `.card`/`.ts-plugin-*`
    /// class: the drawer supplies the card chrome, so the panel owns only its
    /// inner content.
    ///
    /// The one `node` call is the whole #884 seam: it lands as a typed
    /// `Node::Preem` or a rasterised `Node::Pixels` depending on what the host
    /// advertised, with no branch here. The readout carries no CSS class — the
    /// `ts-clock` the label wore was a monospace/tabular *font* rule, and this
    /// chip is a pixel surface with its own font baked in.
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
                child: Box::new(self.seg.node(TIME_ID, &clock_face(&self.iso))),
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
    use super::{BarClock, NO_CLOCK, TIME_ID, clock_face, short_time};
    use hytte_plugin::display::{RenderMode, StyleName, testing::with_render_mode};
    use hytte_plugin::preem::{DisplayStyle, seven_seg};
    use hytte_plugin::proto::preem::PreemWidget;
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

    /// The chip's face is `HH:MM` when the host's timestamp projects to one and
    /// the all-dash placeholder otherwise — including the pre-snapshot seed,
    /// which has no seven-segment glyph at all (#884).
    #[test]
    fn the_chip_face_falls_back_to_dashes() {
        assert_eq!(clock_face("2026-07-11T15:49:00+02:00"), "15:49");
        assert_eq!(clock_face("2026-07-11T00:00:00Z"), "00:00");
        // The `init` seed — one codepoint with no seven-segment glyph, so
        // without the fallback the chip is a single dark cell.
        assert_eq!(clock_face("—"), NO_CLOCK);
        // …and the shapes `short_time` passes through raw, which at 40 px a
        // character would be a chip several hundred pixels wide.
        assert_eq!(clock_face("no-time-here"), NO_CLOCK);
        assert_eq!(clock_face("2026-07-11T15"), NO_CLOCK);
        // Right length, wrong shape.
        assert_eq!(clock_face("2026-07-11Txx:xx:00Z"), NO_CLOCK);
        assert_eq!(clock_face("2026-07-11T15-49:00Z"), NO_CLOCK);
        // A non-ASCII digit is not a digit: `b[..2]` slices bytes, so this also
        // pins that the check can't be fooled into indexing a wide codepoint.
        assert_eq!(clock_face("2026-07-11T１5:49:00Z"), NO_CLOCK);
    }

    /// The core signal against **today's** shell (#884): a snapshot updates the
    /// model, and `view` renders the exact compact bar chip the host will
    /// reconcile — a rasterised seven-segment readout **byte-identical** to the
    /// `preem::seven_seg(…).into_node(…)` a plugin author writes by hand.
    ///
    /// That equality is the migration's compat promise: this chip must reach an
    /// un-advertising shell as the same pixels it would have had if #884 had
    /// never happened, and only comparing the buffers proves it.
    #[test]
    fn against_an_old_shell_the_chip_is_a_rasterised_seven_seg() {
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
                child: Box::new(
                    seven_seg("15:49", DisplayStyle::Vfd).into_node(Some(TIME_ID), vec![]),
                ),
            }],
        };
        // `==` rather than `assert_eq!`: the operands carry a `Node::Pixels`,
        // whose own `Debug` would dump the whole RGBA buffer into the failure
        // output.
        let tree = with_render_mode(RenderMode::Raster, || model.view().tree);
        assert!(
            tree == expected,
            "the raster chip must match the kit by hand"
        );
    }

    /// …and against a shell that advertises the preem vocabulary, the *same*
    /// `view` ships the typed state node instead — same id, same reading, no
    /// pixels anywhere in the tree (#884).
    #[test]
    fn against_a_preem_shell_the_same_chip_is_a_state_node() {
        let mut model = fresh();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));

        let tree = with_render_mode(RenderMode::State, || model.view().tree);
        let Node::Box { children, .. } = &tree else {
            panic!("the chip root is a Box")
        };
        let [Node::Button { child, .. }] = children.as_slice() else {
            panic!("the chip holds exactly the click target")
        };
        match child.as_ref() {
            Node::Preem { id, widget, .. } => {
                assert_eq!(id.as_deref(), Some(TIME_ID), "the reconciler's key");
                match widget.as_ref() {
                    PreemWidget::SevenSeg { config, state } => {
                        assert_eq!(state.text, "15:49", "the plugin's own reading");
                        assert_eq!(
                            config.style.style,
                            StyleName::Vfd,
                            "the skin travels as a name, never as colors",
                        );
                    }
                    other => panic!("expected a seven-seg widget, got {other:?}"),
                }
            }
            Node::Pixels { .. } => panic!("a preem-speaking host must not get pixels"),
            other => panic!("expected Node::Preem, got {other:?}"),
        }
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
        // The panel is plain GTK either way — the seam is per widget, not a
        // mode the plugin enters — so pin it in *both* render modes.
        for mode in [RenderMode::Raster, RenderMode::State] {
            let panel = with_render_mode(mode, || model.view().panel);
            assert_eq!(panel, Some(expected_panel.clone()), "{mode:?}");
        }
    }

    /// A snapshot whose `clock` is `None` (startup window) changes nothing — the
    /// runtime's tree dedup then sends no frame for it.
    #[test]
    fn snapshot_without_clock_changes_nothing() {
        let mut model = fresh();
        // `==` rather than `assert_eq!`: in raster mode the view carries a
        // `Node::Pixels`, whose `Debug` would dump the whole buffer.
        for mode in [RenderMode::Raster, RenderMode::State] {
            let before = with_render_mode(mode, || model.view());
            let effects = model.update(Input::Snapshot(StateSnapshot::default()));
            assert!(effects.is_empty());
            assert!(
                with_render_mode(mode, || model.view()) == before,
                "{mode:?}",
            );
        }
    }

    /// The frames built from this plugin's data (Register manifest + Render tree)
    /// are valid on the wire — they round-trip through the proto codec. The
    /// manifest carries `Mount::BarCenter`, the mount this PR makes render.
    ///
    /// Both render modes (#884): the typed state node has to survive the codec
    /// exactly as the rasterised buffer already did, since that frame is the
    /// only thing the shell ever sees.
    #[test]
    fn register_and_render_frames_round_trip_in_both_modes() {
        let reg = PluginMsg::Register {
            manifest: BarClock::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);
        assert!(
            reg_manifest_negotiates(&reg),
            "…and it declares the negotiation, which is what makes the host \
             send the Hello that unlocks the state arm",
        );

        let mut model = fresh();
        let _ = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1));
        for mode in [RenderMode::Raster, RenderMode::State] {
            // A panel-bearing render (the chip button + the drawer panel, #349)
            // is what the runtime sends after this plugin's first snapshot.
            let view = with_render_mode(mode, || model.view());
            let render = PluginMsg::Render {
                tree: view.tree,
                panel: view.panel,
                effects: vec![],
            };
            let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
            assert!(render == back, "{mode:?}");
        }
    }

    /// Whether a `Register` frame's manifest opted into the #882 vocabulary
    /// negotiation — `Manifest::new` stamps it, and the host sends `Hello` iff
    /// it is set.
    fn reg_manifest_negotiates(reg: &PluginMsg) -> bool {
        if let PluginMsg::Register { manifest } = reg {
            manifest.negotiates_vocab()
        } else {
            false
        }
    }
}
